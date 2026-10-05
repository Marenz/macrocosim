//! gRPC loopback supervisor that mirrors macrocosim's own gRPC
//! service back through `frequenz-microgrid`'s client + logical-
//! meter actors. The metrics panel's charts and chips read from
//! there, so the SPA reads through the same client and logical-meter
//! code a downstream EMS would, served by the same Microgrid service
//! code.
//!
//! `spawn_microgrid_loopback` kicks the supervisor task; the
//! supervisor watches `MicrogridSite` events and rebuilds the
//! `Microgrid` handle when the site's structure or run generation
//! moved (see `SiteShape`), which also resubscribes every forwarder
//! against the new graph.

use std::collections::HashSet;
use std::time::Duration;

use frequenz_microgrid::{
    LogicalMeterConfig, LogicalMeterHandle, Microgrid, MicrogridClientHandle, Sample, metric,
    quantity::{Frequency, Power, ReactivePower},
};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

use crate::sim::{MicrogridSite, events::SiteEvent};

use super::state::{
    HistorySample, MICROGRID_HISTORY_CAP, MicrogridSampleSnapshot, SharedMicrogrid,
};

/// Spawn a tokio task that builds a [`Microgrid`] on `client`, kicks
/// off forwarders for the aggregated streams the metrics panel cares
/// about, and stores the handle in `slot` once the graph is built.
/// The UI's `serve` doesn't wait for the build: the microgrid
/// endpoints return 503 until the slot fills.
///
/// The supervisor reuses `client` for every rebuild and never
/// returns: it holds `site`, which owns the event sender, so its
/// event stream cannot close. After the first successful build the
/// slot's `Microgrid` holds a clone too. A dropped client closes its
/// instructions channel, and frequenz-microgrid's
/// `MicrogridClientActor` busy-spins at 100 % CPU on a closed channel
/// (tracked upstream in frequenz-microgrid-rs).
///
/// `site` is the sink the forwarders publish to via
/// [`MicrogridSite::broadcast_microgrid_sample`]; the existing `/ws/events`
/// stream then carries the samples to the SPA without any extra
/// wiring — they ride the same `SiteEvent` discriminator the
/// per-component samples already use.
pub fn spawn_microgrid_loopback(
    client: MicrogridClientHandle,
    slot: SharedMicrogrid,
    site: MicrogridSite,
) {
    tokio::spawn(async move {
        // Subscribe BEFORE the initial build. The build can take a
        // while (the graph build loops until it succeeds), and a
        // topology change landing in that window must queue an
        // event here — otherwise the loopback would serve the
        // pre-change graph until the NEXT mutation, silently.
        let events = site.subscribe_events();
        run_supervisor(client, slot, site, events).await;
    });
}

/// Build a fresh `Microgrid` and wire up its forwarders. Same code
/// path for the initial boot and every subsequent rebuild, all on the
/// one long-lived `client`. Each call builds a fresh
/// `LogicalMeterHandle` against the current topology and assembles
/// the `Microgrid` via `new_from_handles`. The old `Microgrid`
/// (replaced in `slot`) drops normally; its `LogicalMeterActor` exits
/// cleanly because it handles a closed instructions channel by
/// breaking out.
///
/// Forwarder subscriptions are awaited synchronously **before** the
/// slot swap. The shared `MicrogridClientActor` caches a
/// `broadcast::Sender` per component and its backing tonic stream
/// task exits the moment it sees `receiver_count == 0` between
/// upstream samples (tracked upstream in frequenz-microgrid-rs).
/// Subscribing the new LM first keeps that count ≥ 1 across the
/// handoff, so the stream task survives and samples reach the new
/// forwarders without a multi-second silence.
///
/// Returns false if the logical-meter setup fails outright (which the
/// crate normally retries through).
async fn build_microgrid(
    client: &MicrogridClientHandle,
    slot: &SharedMicrogrid,
    site: &MicrogridSite,
) -> bool {
    // 1 Hz sample cadence matches the existing history sampler;
    // the metrics panel's charts and chips refresh at this rate.
    // LogicalMeterHandle's
    // try_new internally loops on the graph build until it
    // succeeds, so a topology mid-mutation just delays this call
    // rather than returning Err.
    let config = LogicalMeterConfig::new(chrono::TimeDelta::seconds(1));
    let lm = match LogicalMeterHandle::try_new(client.clone(), config).await {
        Ok(lm) => lm,
        Err(e) => {
            log::error!("microgrid loopback: logical-meter setup failed: {e}");
            return false;
        }
    };
    let mut mg = Microgrid::new_from_handles(client.clone(), lm);
    // First cursor reset, BEFORE the new forwarders spawn: a stream that
    // stalled long before this rebuild left an hours-old cursor, and a new
    // forwarder reviving it would otherwise integrate a trapezoid across
    // that whole gap the moment its first sample lands.
    reset_energy_cursors(slot);
    let forwarders = subscribe_power_forwarders(&mut mg, site, slot.clone()).await;
    // Atomic swap. Aborting the old forwarders + dropping the old
    // Microgrid happens AFTER the new LM has subscribed to every
    // component it cares about (above), so the shared client's
    // per-component broadcast Senders never see receiver_count drop
    // to zero between LM generations.
    for h in slot.forwarders.lock().drain(..) {
        h.abort();
    }
    // The energy bookkeeping runs only now, with the old forwarders gone —
    // earlier, their still-arriving samples would re-seed a just-reset
    // cursor or repopulate a just-cleared map. The new forwarders are
    // already live, but their samples carry the new topology: a cleared
    // total loses at most the sub-second span since their subscribe, and a
    // reset cursor just re-seeds on their next sample.
    //
    // A site reset (config hot-reload) started a new run: the caches
    // belong to the previous run and go, like the site's own
    // per-component accumulators and histories. A plain topology
    // mutation (same generation) keeps the energy totals and prunes
    // the caches to the streams the new graph publishes, so the charts
    // reading the survivors carry on instead of restarting from empty.
    prune_for_rebuild(slot, site, &forwarders.streams);
    // Second cursor reset: any cursor present now was re-seeded during the
    // handoff (by an old forwarder up to the abort, or a new one already
    // delivering). Dropping it costs at most one real ~1 s interval and
    // guarantees no stream integrates across the swap gap.
    reset_energy_cursors(slot);
    // The cumulative energy totals survive a same-generation rebuild —
    // re-expose the ones the prune dropped from the cache.
    republish_energy_totals(slot, site);
    *slot.forwarders.lock() = forwarders.handles;
    *slot.microgrid.write() = Some(mg);
    true
}

/// Run the initial build, then rebuild the Microgrid handle on every
/// TopologyChanged that follows a structural change or a site reset —
/// the event also fires for runtime pokes (see `SiteShape`), which
/// keep the graph. The graph crate's ComponentGraph is snapshotted at
/// try_new time, so formulas + subscriptions go stale once the site's
/// structure mutates; rebuilding picks up the new shape. The loop is
/// entered even when the initial build failed: a transient failure
/// (gRPC transport hiccup) heals on the next topology event instead of
/// leaving the microgrid endpoints 503 forever. Lagged-receiver events
/// run the same check (defensive — a missed event might have been a
/// topology change).
async fn run_supervisor(
    client: MicrogridClientHandle,
    slot: SharedMicrogrid,
    site: MicrogridSite,
    mut events: tokio::sync::broadcast::Receiver<SiteEvent>,
) {
    let mut built = None;
    build_if_needed(&client, &slot, &site, &mut built).await;
    loop {
        match events.recv().await {
            Ok(SiteEvent::TopologyChanged { .. }) => {
                debounce_topology_burst(&mut events).await;
                build_if_needed(&client, &slot, &site, &mut built).await;
            }
            Ok(_) => continue,
            Err(RecvError::Lagged(n)) => {
                log::warn!(
                    "microgrid loopback supervisor: lagged {n} events, checking the graph defensively"
                );
                debounce_topology_burst(&mut events).await;
                build_if_needed(&client, &slot, &site, &mut built).await;
            }
            Err(RecvError::Closed) => {
                // Unreachable while this task holds `site`. Park
                // anyway: returning would drop `client`.
                log::error!("microgrid loopback supervisor: site events closed; parking");
                std::future::pending::<()>().await;
            }
        }
    }
}

/// After seeing the first TopologyChanged, swallow any further
/// events that arrive within `DEBOUNCE` so a hot-reload that
/// registers 12 components in rapid succession only triggers one
/// rebuild instead of 12.
async fn debounce_topology_burst(events: &mut tokio::sync::broadcast::Receiver<SiteEvent>) {
    const DEBOUNCE: Duration = Duration::from_millis(300);
    let deadline = tokio::time::Instant::now() + DEBOUNCE;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(_)) => continue, // keep collecting
            Ok(Err(_)) => return,  // broadcast error; supervisor's main loop deals with it
            Err(_) => return,      // deadline; we're done
        }
    }
}

/// Rebuild the `LogicalMeterHandle` so its graph snapshot reflects
/// the new topology — unless the site's shape still matches what
/// `built` mirrors (a runtime poke bumped the version, nothing
/// structural moved), in which case the graph, its forwarders and
/// every cached stream stay as they are. `built` records the shape a
/// successful build mirrors: snapshotted BEFORE the build and recorded
/// only on success, so a mutation landing mid-build, or a failed
/// build, leaves the next event free to rebuild.
///
/// `build_microgrid` does the work — it subscribes the new
/// forwarders first, then atomically aborts the old ones and swaps
/// the slot. The old `Microgrid` stays in the slot until then so the
/// shared client's per-component broadcast Senders keep at least one
/// live receiver across the handoff.
async fn build_if_needed(
    client: &MicrogridClientHandle,
    slot: &SharedMicrogrid,
    site: &MicrogridSite,
    built: &mut Option<SiteShape>,
) {
    let shape = SiteShape::of(site);
    if *built == Some(shape) {
        log::debug!(
            "microgrid loopback: site version moved without a structural change — keeping the graph"
        );
        return;
    }
    log::debug!("microgrid loopback: building the graph handle");
    if build_microgrid(client, slot, site).await {
        log::info!("microgrid loopback: graph built + forwarders running");
        *built = Some(shape);
    }
}

/// One build's forwarders: the tasks to abort on the next rebuild,
/// and the stream names they publish, which is the set of streams
/// `retain_streams` keeps in the caches.
#[derive(Default)]
struct Forwarders {
    handles: Vec<JoinHandle<()>>,
    streams: Vec<&'static str>,
}

impl Forwarders {
    /// Record one task and the streams it publishes (a bounds
    /// forwarder publishes two).
    fn push(&mut self, streams: &[&'static str], handle: JoinHandle<()>) {
        self.handles.push(handle);
        self.streams.extend_from_slice(streams);
    }
}

/// Build subscriptions for the aggregate streams the metrics panel
/// charts — grid, consumer, producer and PV active power, site and
/// per-source reactive power, and the battery pool's power plus its
/// bounds envelope — and spawn one tokio task per surviving
/// subscription to forward samples onto the MicrogridSite event bus.
///
/// Each `formula.subscribe().await` is run on the caller's task so
/// that, when this function returns, the new LM has already
/// subscribed all its required components through the shared
/// client. That keeps `build_microgrid`'s swap step safe: the old
/// `Microgrid` can drop without ever taking the shared client's
/// per-component broadcast receiver count to zero.
///
/// Streams whose underlying category is absent (no PV in the
/// topology, etc.) emit a single `log::info!` and are silently
/// dropped — no forwarder, so nothing is ever published under that
/// stream name and the metrics panel's card and chip for it read
/// "—" until the category appears and a rebuild resubscribes.
async fn subscribe_power_forwarders(
    microgrid: &mut Microgrid,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> Forwarders {
    let mut forwarders = Forwarders::default();
    let lm = microgrid.logical_meter();
    let metered: [(&'static str, _); 4] = [
        ("grid_power", lm.grid::<metric::AcPowerActive>()),
        ("consumer_power", lm.consumer::<metric::AcPowerActive>()),
        ("producer_power", lm.producer::<metric::AcPowerActive>()),
        ("pv_power", lm.pv::<metric::AcPowerActive>(None)),
    ];
    for (stream, formula) in metered {
        if let Some(h) = subscribe_power_forwarder(stream, formula, site, state.clone()).await {
            forwarders.push(&[stream], h);
        }
    }
    // Site Q at the connection point.
    if let Some(h) = subscribe_reactive_forwarder(
        "grid_reactive_power",
        lm.grid::<metric::AcPowerReactive>(),
        site,
        state.clone(),
    )
    .await
    {
        forwarders.push(&["grid_reactive_power"], h);
    }
    // Per-source Q for the metrics panel's Reactive card: the same
    // logical-meter formulas as the power streams, metric
    // AcPowerReactive. energy_stream_for maps none of them — varh
    // accumulation stays out of scope, as with grid Q.
    for (stream, formula) in [
        ("pv_reactive_power", lm.pv::<metric::AcPowerReactive>(None)),
        (
            "battery_reactive_power",
            lm.battery::<metric::AcPowerReactive>(None),
        ),
    ] {
        if let Some(h) = subscribe_reactive_forwarder(stream, formula, site, state.clone()).await {
            forwarders.push(&[stream], h);
        }
    }
    // Site frequency via the grid Frequency formula — a COALESCE over
    // the meters feeding the PCC, so any site shape works (the old
    // main-meter workaround required exactly one grid → one meter).
    // Wired since frequenz-microgrid 0.6.0 added the Frequency
    // sender arm.
    if let Some(h) = subscribe_frequency_forwarder(
        "grid_frequency",
        lm.grid::<metric::AcFrequency>(),
        site,
        state.clone(),
    )
    .await
    {
        forwarders.push(&["grid_frequency"], h);
    }
    // BatteryPool and SteamBoilerPool have the same shape — an
    // aggregate power formula plus a bounds envelope — and both take
    // &mut self for power() / power_bounds() (they cache subscriber
    // refs): build each once and let it go out of scope after both
    // subscriptions resolve.
    match microgrid.battery_pool(None) {
        Ok(mut pool) => {
            spawn_pool_forwarders(
                "battery_pool_power",
                ["battery_pool_bounds_lower", "battery_pool_bounds_upper"],
                pool.power(),
                pool.power_bounds(),
                site,
                state.clone(),
                &mut forwarders,
            )
            .await;
        }
        Err(e) => log::info!("microgrid loopback: battery pool unavailable — skipping: {e}"),
    }
    match microgrid.steam_boiler_pool(None) {
        Ok(mut pool) => {
            spawn_pool_forwarders(
                "steam_boiler_pool_power",
                [
                    "steam_boiler_pool_bounds_lower",
                    "steam_boiler_pool_bounds_upper",
                ],
                pool.power(),
                pool.power_bounds(),
                site,
                state,
                &mut forwarders,
            )
            .await;
        }
        Err(e) => log::info!("microgrid loopback: steam boiler pool unavailable — skipping: {e}"),
    }
    forwarders
}

/// Forward a pool's aggregate power formula as the `power_stream`
/// point stream, and its bounds envelope as the two `bounds_streams`
/// (see `spawn_bounds_forwarder`). The envelope is flattened into
/// two point streams so the existing point-sample infrastructure
/// (cache + sparkline) renders both halves without an
/// envelope-shaped payload variant.
async fn spawn_pool_forwarders(
    power_stream: &'static str,
    bounds_streams: [&'static str; 2],
    power: Result<frequenz_microgrid::Formula<Power>, frequenz_microgrid::Error>,
    bounds: tokio::sync::broadcast::Receiver<Vec<frequenz_microgrid::Bounds<Power>>>,
    site: &MicrogridSite,
    state: SharedMicrogrid,
    forwarders: &mut Forwarders,
) {
    // The two travel together: a pool whose power stream could not
    // be subscribed gets no envelope stream either.
    let Some(h) = subscribe_power_forwarder(power_stream, power, site, state.clone()).await else {
        return;
    };
    forwarders.push(&[power_stream], h);
    forwarders.push(
        &bounds_streams,
        spawn_bounds_forwarder(bounds_streams, bounds, site, state),
    );
}

/// Forward a pool's `Vec<Bounds<Power>>` stream as the two point
/// streams named in `[lower, upper]` (`battery_pool_bounds_lower` +
/// `battery_pool_bounds_upper`, and the `steam_boiler_pool_` pair).
/// The upstream tracker emits a fresh Vec on every telemetry
/// snapshot, so the cadence matches the power forwarders' 1 Hz;
/// sparklines alongside the pool power tile track the same time axis.
///
/// When the Vec is empty (no components in the pool) both halves
/// publish `None`. When it has multiple disjoint regions we keep
/// only the outermost envelope — single-region is by far the
/// common case and a multi-region split is a niche signal that the
/// developer-facing dashboard isn't designed around.
fn spawn_bounds_forwarder(
    [lower_stream, upper_stream]: [&'static str; 2],
    mut rx: tokio::sync::broadcast::Receiver<Vec<frequenz_microgrid::Bounds<Power>>>,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> JoinHandle<()> {
    let site = site.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(envelopes) => {
                    let lower = outer_bound(&envelopes, |b| b.lower(), f32::min);
                    let upper = outer_bound(&envelopes, |b| b.upper(), f32::max);
                    let ts_ms = chrono::Utc::now().timestamp_millis();
                    publish_scalar(lower_stream, "Power", "W", lower, ts_ms, &site, &state);
                    publish_scalar(upper_stream, "Power", "W", upper, ts_ms, &site, &state);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!(
                        "microgrid loopback: {lower_stream}/{upper_stream} lagged {n} samples"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!(
                        "microgrid loopback: {lower_stream}/{upper_stream} closed; forwarder exiting"
                    );
                    return;
                }
            }
        }
    })
}

fn outer_bound(
    envelopes: &[frequenz_microgrid::Bounds<Power>],
    pick: impl Fn(&frequenz_microgrid::Bounds<Power>) -> Option<Power>,
    fold: fn(f32, f32) -> f32,
) -> Option<f32> {
    envelopes
        .iter()
        .filter_map(|b| pick(b).map(|p| p.as_watts()))
        .reduce(fold)
}

/// Subscribe to one Power-valued formula and spawn a forwarder that
/// pushes each `Sample<Power>` onto the MicrogridSite event bus as a
/// `MicrogridSample { stream, quantity: "Power", unit: "W", ... }`
/// event. The `formula.subscribe().await` runs on the caller's task
/// so the LM has actually registered for the component samples by
/// the time we return — see `build_microgrid` for why that ordering
/// matters across rebuilds. Returns `None` (no spawn) if the formula
/// errored at construction (typical for absent categories) or the
/// initial subscribe failed.
async fn subscribe_power_forwarder(
    stream: &'static str,
    formula: Result<frequenz_microgrid::Formula<Power>, frequenz_microgrid::Error>,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> Option<JoinHandle<()>> {
    let formula = match formula {
        Ok(f) => f,
        Err(e) => {
            log::info!("microgrid loopback: skip {stream} ({e})");
            return None;
        }
    };
    let mut rx = match formula.subscribe().await {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("microgrid loopback: subscribe {stream} failed: {e}");
            return None;
        }
    };
    let site = site.clone();
    Some(tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(sample) => publish_power(stream, sample, &site, &state),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("microgrid loopback: {stream} lagged {n} samples");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!("microgrid loopback: {stream} closed; forwarder exiting");
                    return;
                }
            }
        }
    }))
}

fn publish_power(
    stream: &'static str,
    sample: Sample<Power>,
    site: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let value = sample.value().map(|p| p.as_watts());
    let ts_ms = sample.timestamp().timestamp_millis();
    // Integrate this aggregate into a companion cumulative-energy stream
    // (`grid_power` → `grid_energy`, …) before the new power overwrites the
    // cached sample the integral reads from.
    if let Some(energy_stream) = energy_stream_for(stream) {
        accumulate_energy(energy_stream, value, ts_ms, site, state);
    }
    publish_scalar(stream, "Power", "W", value, ts_ms, site, state);
}

/// Subscribe to one ReactivePower-valued formula and spawn a forwarder that
/// pushes each `Sample<ReactivePower>` onto the MicrogridSite event bus as a
/// `MicrogridSample { stream, quantity: "ReactivePower", unit: "var", ... }`
/// event. Kept as a parallel pair with `subscribe_power_forwarder` /
/// `publish_power` rather than a generic-over-quantity helper: that keeps
/// the (far more heavily used) Power path monomorphic and easy to read,
/// at the cost of a little duplication for this one Q stream. The
/// `formula.subscribe().await` runs on the caller's task so the LM has
/// actually registered for the component samples by the time we return —
/// see `build_microgrid` for why that ordering matters across rebuilds.
/// Returns `None` (no spawn) if the formula errored at construction
/// (typical for absent categories) or the initial subscribe failed.
async fn subscribe_reactive_forwarder(
    stream: &'static str,
    formula: Result<frequenz_microgrid::Formula<ReactivePower>, frequenz_microgrid::Error>,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> Option<JoinHandle<()>> {
    let formula = match formula {
        Ok(f) => f,
        Err(e) => {
            log::info!("microgrid loopback: skip {stream} ({e})");
            return None;
        }
    };
    let mut rx = match formula.subscribe().await {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("microgrid loopback: subscribe {stream} failed: {e}");
            return None;
        }
    };
    let site = site.clone();
    Some(tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(sample) => publish_reactive(stream, sample, &site, &state),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("microgrid loopback: {stream} lagged {n} samples");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!("microgrid loopback: {stream} closed; forwarder exiting");
                    return;
                }
            }
        }
    }))
}

fn publish_reactive(
    stream: &'static str,
    sample: Sample<ReactivePower>,
    site: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let value = sample.value().map(|q| q.as_volt_amperes_reactive());
    let ts_ms = sample.timestamp().timestamp_millis();
    // No energy hook on purpose: reactive energy (varh) accumulation is
    // out of scope; energy_stream_for never maps this stream.
    publish_scalar(stream, "ReactivePower", "var", value, ts_ms, site, state);
}

/// Subscribe to the Frequency-valued grid formula and spawn a forwarder that
/// pushes each `Sample<Frequency>` onto the MicrogridSite event bus as a
/// `MicrogridSample { stream, quantity: "Frequency", unit: "Hz", ... }`
/// event. Kept as a third parallel copy alongside `subscribe_power_forwarder`
/// / `subscribe_reactive_forwarder` for the same reason as the reactive one:
/// a generic-over-quantity helper would fight the type-level differences
/// between `Power`, `ReactivePower` and `Frequency` for a single stream's
/// worth of savings. The `formula.subscribe().await` runs on the caller's
/// task so the LM has actually registered for the component samples by the
/// time we return — see `build_microgrid` for why that ordering matters
/// across rebuilds. Returns `None` (no spawn) if the formula errored at
/// construction or the initial subscribe failed.
async fn subscribe_frequency_forwarder(
    stream: &'static str,
    formula: Result<frequenz_microgrid::Formula<Frequency>, frequenz_microgrid::Error>,
    site: &MicrogridSite,
    state: SharedMicrogrid,
) -> Option<JoinHandle<()>> {
    let formula = match formula {
        Ok(f) => f,
        Err(e) => {
            log::info!("microgrid loopback: skip {stream} ({e})");
            return None;
        }
    };
    let mut rx = match formula.subscribe().await {
        Ok(rx) => rx,
        Err(e) => {
            log::warn!("microgrid loopback: subscribe {stream} failed: {e}");
            return None;
        }
    };
    let site = site.clone();
    Some(tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(sample) => publish_frequency(stream, sample, &site, &state),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("microgrid loopback: {stream} lagged {n} samples");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    log::info!("microgrid loopback: {stream} closed; forwarder exiting");
                    return;
                }
            }
        }
    }))
}

fn publish_frequency(
    stream: &'static str,
    sample: Sample<Frequency>,
    site: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let value = sample.value().map(|q| q.as_hertz());
    let ts_ms = sample.timestamp().timestamp_millis();
    // No energy hook: frequency has no cumulative companion stream;
    // energy_stream_for never maps this stream.
    publish_scalar(stream, "Frequency", "Hz", value, ts_ms, site, state);
}

/// The cumulative-energy companion stream for a metered power stream, or
/// `None` for streams we don't integrate (e.g. the bounds envelopes).
fn energy_stream_for(power_stream: &str) -> Option<&'static str> {
    Some(match power_stream {
        "grid_power" => "grid_energy",
        "consumer_power" => "consumer_energy",
        "producer_power" => "producer_energy",
        "pv_power" => "pv_energy",
        "battery_pool_power" => "battery_pool_energy",
        "steam_boiler_pool_power" => "steam_boiler_pool_energy",
        _ => return None,
    })
}

/// The two site counters a rebuild is keyed on. Every successful eval
/// bumps the plain site version so the UI refetches — a
/// `(set-meter-power …)` from the inspector included — but the graph
/// the loopback mirrors only moves with the structural version
/// (components, connections, names) or a site reset (config
/// hot-reload: new run generation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SiteShape {
    structural: u64,
    generation: u64,
}

impl SiteShape {
    fn of(site: &MicrogridSite) -> Self {
        Self {
            structural: site.structural_version(),
            generation: site.run_generation(),
        }
    }
}

/// Drop the cached `latest` sample and history ring of every stream
/// the rebuilt graph no longer publishes — a topology that lost its
/// last PV must not keep serving `pv_power` via
/// `/api/mg/{id}/metrics/history` forever — and keep the rest, so
/// the surviving charts don't restart from empty on every rebuild. An
/// energy total belongs to its power stream: it stays while that
/// stream is live (its forwarder keeps advancing it) and goes with it
/// otherwise (`republish_energy_totals` then re-exposes the retained
/// total as a single point, as before).
fn retain_streams(state: &SharedMicrogrid, live: &[&'static str]) {
    let mut keep: HashSet<&'static str> = live.iter().copied().collect();
    keep.extend(live.iter().filter_map(|s| energy_stream_for(s)));
    state.latest.write().retain(|s, _| keep.contains(s));
    state.history.write().retain(|s, _| keep.contains(s));
}

/// Bring the slot's caches in line with the rebuilt graph. On a new run
/// (the run generation moved: `reset()`, a config hot-reload) every
/// cached stream goes, the energy totals included. On a same-generation
/// rebuild the totals stay and the caches keep the streams in `live`.
fn prune_for_rebuild(state: &SharedMicrogrid, site: &MicrogridSite, live: &[&'static str]) {
    let new_run = clear_energy_on_new_run(state, site);
    retain_streams(state, if new_run { &[] } else { live });
}

/// Clear the aggregate energy totals when `site`'s run generation moved
/// past the one the slot's totals were gathered under: the site was reset
/// (a config hot-reload), so the energy belongs to a previous run — just
/// like the site's own per-component accumulators, which `reset()` clears.
/// Same-generation rebuilds (topology mutations) keep the totals. Returns
/// whether the generation moved.
fn clear_energy_on_new_run(state: &SharedMicrogrid, site: &MicrogridSite) -> bool {
    let generation = site.run_generation();
    let new_run = state
        .energy_generation
        .swap(generation, std::sync::atomic::Ordering::Relaxed)
        != generation;
    if new_run {
        state.energy.write().clear();
    }
    new_run
}

/// Drop every aggregate integrator cursor (`EnergyAccum::reset_cursor`),
/// so the first power sample each stream sees afterwards re-seeds instead
/// of integrating a trapezoid across a dead window using a stale cursor.
/// A rebuild runs it twice: before the new forwarders spawn (a stream
/// that stalled long ago left an old cursor a revived stream would
/// integrate across) and again after the old forwarders are aborted (a
/// still-live forwarder re-seeds cursors during the handoff).
fn reset_energy_cursors(state: &SharedMicrogrid) {
    for e in state.energy.write().values_mut() {
        e.reset_cursor();
    }
}

/// Re-expose the retained energy totals after a rebuild pruned
/// `latest`/`history`, so a read of e.g. `pv_energy` still returns the
/// accumulated value even when that stream's power never fires again (a
/// topology that dropped PV, taking `pv_energy` out of the caches with
/// it) and would otherwise never repopulate the cache. Streams a new forwarder already repopulated are left
/// alone — their cached sample is newer than this wall-clock-stamped
/// republish. (A racing sample between the check and the publish can
/// still be overwritten, but only on a live stream, whose next 1 Hz
/// sample corrects it.)
///
/// A no-op on first boot, when `state.energy` is still empty.
fn republish_energy_totals(state: &SharedMicrogrid, site: &MicrogridSite) {
    let ts_ms = chrono::Utc::now().timestamp_millis();
    // Snapshot (stream, total) under the energy lock, then publish outside
    // it — publish_scalar takes the latest/history locks.
    let totals: Vec<(&'static str, f64)> = state
        .energy
        .read()
        .iter()
        .map(|(stream, e)| (*stream, e.total_wh))
        .collect();
    for (stream, total) in totals {
        if state.latest.read().contains_key(stream) {
            continue;
        }
        publish_scalar(
            stream,
            "Energy",
            "Wh",
            Some(total as f32),
            ts_ms,
            site,
            state,
        );
    }
}

/// Advance an aggregate energy total by the trapezoid between the last power
/// sample and the incoming one, and publish the running total. The total
/// lives in `state.energy` (persistent across rebuilds), not in `latest`
/// (pruned on rebuild) — so a topology mutation mid-run doesn't reset the
/// integral. A `None` power (stream gap) carries the total forward without
/// advancing it. Signed like the power, so the total is net energy (Wh).
fn accumulate_energy(
    energy_stream: &'static str,
    new_value: Option<f32>,
    new_ts_ms: i64,
    site: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let total = {
        let mut acc = state.energy.write();
        let e = acc.entry(energy_stream).or_default();
        // A None power (stream gap) carries the total forward without
        // advancing it; a real sample integrates the trapezoid since the
        // previous one.
        if let Some(nv) = new_value {
            e.advance(nv, new_ts_ms);
        }
        e.total_wh
    };
    publish_scalar(
        energy_stream,
        "Energy",
        "Wh",
        Some(total as f32),
        new_ts_ms,
        site,
        state,
    );
}

/// Push a typed scalar onto both the per-stream `latest` cache and
/// the WS event bus. The `quantity` + `unit` pair travels with the
/// sample so the SPA picks the right autoscale family (Power
/// W→kW→MW, Frequency Hz, etc.) without pattern-matching on the
/// stream name.
fn publish_scalar(
    stream: &'static str,
    quantity: &'static str,
    unit: &'static str,
    value: Option<f32>,
    ts_ms: i64,
    site: &MicrogridSite,
    state: &SharedMicrogrid,
) {
    let snapshot = MicrogridSampleSnapshot {
        quantity,
        unit,
        ts_ms,
        value,
    };
    state.latest.write().insert(stream, snapshot);
    // Append to the rolling history ring so the metrics panel's
    // charts have past data to backfill from on panel open.
    // Drop the oldest entry when the ring is full.
    {
        let mut history = state.history.write();
        let ring = history.entry(stream).or_default();
        if ring.len() == MICROGRID_HISTORY_CAP {
            ring.pop_front();
        }
        ring.push_back(HistorySample { ts_ms, value });
    }
    // The scenario reporter's site metrics ride the same two grid
    // formula streams the panel charts — fed here so the journal
    // needs no main-meter special case. A gap sample (`None`) carries
    // nothing to peak or average, and a timestamp outside chrono's
    // range can't be bucketed, so both are skipped.
    if let Some(v) = value
        && matches!(stream, "grid_power" | "grid_reactive_power")
        && let Some(ts) = chrono::DateTime::from_timestamp_millis(ts_ms)
    {
        if stream == "grid_power" {
            site.record_grid_power_sample(v, ts);
        } else {
            site.record_grid_reactive_sample(v, ts);
        }
    }
    site.broadcast_microgrid_sample(stream, quantity, unit, ts_ms, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::new_microgrid_slot;

    const HOUR_MS: i64 = 3_600_000;

    #[test]
    fn energy_stream_for_maps_aggregates_and_ignores_the_rest() {
        assert_eq!(energy_stream_for("grid_power"), Some("grid_energy"));
        assert_eq!(energy_stream_for("pv_power"), Some("pv_energy"));
        assert_eq!(
            energy_stream_for("battery_pool_power"),
            Some("battery_pool_energy")
        );
        assert_eq!(
            energy_stream_for("steam_boiler_pool_power"),
            Some("steam_boiler_pool_energy")
        );
        // Bounds envelopes and unknown streams don't integrate.
        assert_eq!(energy_stream_for("active_power_lower_bound_w"), None);
        assert_eq!(energy_stream_for("steam_boiler_pool_bounds_lower"), None);
    }

    #[test]
    fn rebuild_keeps_energy_total_and_reseeds_after_the_gap() {
        let state = new_microgrid_slot();
        let site = MicrogridSite::new();
        // Move 1000 Wh on grid_energy: 1000 W held for one hour.
        accumulate_energy("grid_energy", Some(1000.0), 0, &site, &state);
        accumulate_energy("grid_energy", Some(1000.0), HOUR_MS, &site, &state);
        let before = state.energy.read()["grid_energy"].total_wh;
        assert!((before - 1000.0).abs() < 1e-6, "{before}");

        // A rebuild resets the cursors (before its forwarders spawn),
        // prunes the streams the new graph dropped (here: all of them),
        // then republishes the retained totals.
        reset_energy_cursors(&state);
        retain_streams(&state, &[]);
        republish_energy_totals(&state, &site);

        // #2: the retained total is republished into the cleared `latest`,
        // so a read still sees it even though no power sample has fired
        // since the rebuild (e.g. a topology that dropped the stream).
        let republished = state.latest.read()["grid_energy"].value.unwrap();
        assert!((republished as f64 - before).abs() < 1.0, "{republished}");

        // #1: the first sample after the rebuild re-seeds instead of
        // integrating a trapezoid across the (here 10 h) dead window — the
        // total is unchanged until a real interval elapses.
        accumulate_energy(
            "grid_energy",
            Some(1000.0),
            HOUR_MS + 10 * HOUR_MS,
            &site,
            &state,
        );
        let after_seed = state.energy.read()["grid_energy"].total_wh;
        assert!(
            (after_seed - before).abs() < 1e-6,
            "gap bridged: {after_seed}"
        );

        // Integration then resumes cleanly from the new cursor (+1 h).
        accumulate_energy(
            "grid_energy",
            Some(1000.0),
            HOUR_MS + 10 * HOUR_MS + HOUR_MS,
            &site,
            &state,
        );
        let resumed = state.energy.read()["grid_energy"].total_wh;
        assert!((resumed - (before + 1000.0)).abs() < 1e-6, "{resumed}");
    }

    /// A forwarder sample that lands between the `latest` prune and the
    /// republish owns the fresher value — the republish must not
    /// overwrite it with the wall-clock-stamped copy.
    #[test]
    fn republish_skips_streams_a_forwarder_already_repopulated() {
        let state = new_microgrid_slot();
        let site = MicrogridSite::new();
        accumulate_energy("grid_energy", Some(1000.0), 0, &site, &state);
        reset_energy_cursors(&state);
        retain_streams(&state, &[]);
        // A new forwarder fires before the republish runs.
        accumulate_energy("grid_energy", Some(1000.0), HOUR_MS, &site, &state);
        let fresh_ts = state.latest.read()["grid_energy"].ts_ms;
        republish_energy_totals(&state, &site);
        assert_eq!(state.latest.read()["grid_energy"].ts_ms, fresh_ts);
    }

    /// A config reload resets the site and bumps its run generation: the
    /// aggregate totals belong to the previous run and clear, matching
    /// the per-component accumulators `reset()` cleared. A rebuild in the
    /// same generation (a topology mutation) keeps them.
    #[test]
    fn energy_clears_on_a_new_run_generation_only() {
        let state = new_microgrid_slot();
        let site = MicrogridSite::new();
        accumulate_energy("grid_energy", Some(1000.0), 0, &site, &state);
        accumulate_energy("grid_energy", Some(1000.0), HOUR_MS, &site, &state);

        // Same generation: a mutation rebuild keeps the totals.
        clear_energy_on_new_run(&state, &site);
        assert!(!state.energy.read().is_empty());

        // A reset (config reload) starts a new run: the totals clear.
        site.reset();
        clear_energy_on_new_run(&state, &site);
        assert!(state.energy.read().is_empty());
    }

    /// Every successful eval bumps the site version — a meter power
    /// override included — but only a structural edit moves the graph
    /// the loopback mirrors. A runtime poke must not trigger a rebuild,
    /// which tears down and resubscribes every forwarder for nothing.
    #[test]
    fn runtime_pokes_do_not_change_the_site_shape() {
        let site = MicrogridSite::new();
        let built = SiteShape::of(&site);
        // What `Overrides::eval` does after `(set-meter-power …)`.
        site.bump_version();
        assert_eq!(SiteShape::of(&site), built);
    }

    /// A structural edit or a site reset (config hot-reload: new run
    /// generation) still rebuilds.
    #[test]
    fn structural_edits_and_resets_change_the_site_shape() {
        let site = MicrogridSite::new();
        let built = SiteShape::of(&site);
        site.register(crate::sim::Meter::new(
            5,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));
        assert_ne!(SiteShape::of(&site), built, "structural edit");

        let built = SiteShape::of(&site);
        site.reset();
        assert_ne!(SiteShape::of(&site), built, "new run generation");
    }

    /// A rebuild drops the cached streams the new graph no longer
    /// serves (no more PV, say) and keeps the rest — `latest` and the
    /// history rings alike — so the surviving charts don't restart.
    /// An energy total follows its power stream.
    #[test]
    fn rebuild_prunes_only_the_streams_the_new_graph_dropped() {
        let state = new_microgrid_slot();
        let site = MicrogridSite::new();
        publish_scalar("grid_power", "Power", "W", Some(1.0), 0, &site, &state);
        publish_scalar("pv_power", "Power", "W", Some(2.0), 0, &site, &state);
        accumulate_energy("grid_energy", Some(1000.0), 0, &site, &state);
        accumulate_energy("pv_energy", Some(1000.0), 0, &site, &state);
        // The bounds halves have no energy companion; they survive only
        // because the pool forwarder records both names with its task.
        for s in ["battery_pool_bounds_lower", "battery_pool_bounds_upper"] {
            publish_scalar(s, "Power", "W", Some(3.0), 0, &site, &state);
        }

        prune_for_rebuild(
            &state,
            &site,
            &[
                "grid_power",
                "battery_pool_bounds_lower",
                "battery_pool_bounds_upper",
            ],
        );

        let latest = state.latest.read();
        let history = state.history.read();
        for kept in [
            "grid_power",
            "grid_energy",
            "battery_pool_bounds_lower",
            "battery_pool_bounds_upper",
        ] {
            assert!(latest.contains_key(kept), "latest keeps {kept}");
            assert!(history.contains_key(kept), "history keeps {kept}");
        }
        for dropped in ["pv_power", "pv_energy"] {
            assert!(!latest.contains_key(dropped), "latest drops {dropped}");
            assert!(!history.contains_key(dropped), "history drops {dropped}");
        }
    }

    /// A site reset (config hot-reload) starts a new run: every cached
    /// stream goes, surviving names included, like the site's own
    /// per-component histories. Keeping them would splice two runs
    /// into one ring, with a cliff where the cleared energy totals
    /// restart at zero.
    #[test]
    fn a_new_run_drops_every_cached_stream() {
        let state = new_microgrid_slot();
        let site = MicrogridSite::new();
        publish_scalar("grid_power", "Power", "W", Some(1.0), 0, &site, &state);
        accumulate_energy("grid_energy", Some(1000.0), 0, &site, &state);
        prune_for_rebuild(&state, &site, &["grid_power"]);
        assert!(
            state.latest.read().contains_key("grid_power"),
            "same run keeps"
        );

        site.reset();
        prune_for_rebuild(&state, &site, &["grid_power"]);
        assert!(
            state.latest.read().is_empty(),
            "{:?}",
            state.latest.read().keys()
        );
        assert!(
            state.history.read().is_empty(),
            "{:?}",
            state.history.read().keys()
        );
    }

    #[test]
    fn grid_reactive_power_has_no_energy_stream() {
        assert_eq!(energy_stream_for("grid_reactive_power"), None);
    }

    /// Only the two grid streams feed the scenario journal. `pv_power`
    /// carries watts of the same shape and must leave the peak alone,
    /// and a gap sample (`None`) on `grid_power` carries nothing to
    /// record.
    #[test]
    fn publish_scalar_feeds_the_journal_from_grid_streams_only() {
        let state = new_microgrid_slot();
        let site = MicrogridSite::new();
        let now = chrono::Utc::now();
        let ts_ms = now.timestamp_millis();
        site.scenario_start("filter".into(), now);

        publish_scalar("pv_power", "Power", "W", Some(5000.0), ts_ms, &site, &state);
        assert_eq!(site.scenario_report(now).peak_grid_w, 0.0);

        publish_scalar("grid_power", "Power", "W", None, ts_ms, &site, &state);
        assert_eq!(site.scenario_report(now).peak_grid_w, 0.0);

        publish_scalar(
            "grid_power",
            "Power",
            "W",
            Some(1200.0),
            ts_ms,
            &site,
            &state,
        );
        let peak = site.scenario_report(now).peak_grid_w;
        assert!((peak - 1200.0).abs() < 1e-6, "{peak}");

        publish_scalar(
            "grid_reactive_power",
            "ReactivePower",
            "var",
            Some(1600.0),
            ts_ms,
            &site,
            &state,
        );
        let r = site.scenario_report(now);
        assert!(
            (r.peak_grid_var - 1600.0).abs() < 1e-6,
            "{}",
            r.peak_grid_var
        );
        // P=1200, Q=1600 → PF = 1200/2000 = 0.6, paired off the P above.
        let pf = r.site_pf_at_peak_var.expect("pf after a paired PQ sample");
        assert!((pf - 0.6).abs() < 1e-6, "{pf}");

        publish_scalar(
            "pv_reactive_power",
            "ReactivePower",
            "var",
            Some(9000.0),
            ts_ms,
            &site,
            &state,
        );
        let r = site.scenario_report(now);
        assert!(
            (r.peak_grid_var - 1600.0).abs() < 1e-6,
            "sibling Q stream reached the journal: {}",
            r.peak_grid_var
        );
    }
}
