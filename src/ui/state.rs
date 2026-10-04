//! Shared state types for the UI subsystem: the per-microgrid
//! loopback cache (latest + history rings + forwarder handles),
//! the enterprise map of loopback states, the create-microgrid
//! spawner callback, and the embedded-assets handle.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use frequenz_microgrid::{Microgrid, MicrogridClientHandle};
use parking_lot::{Mutex, RwLock};
use rust_embed::Embed;
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::sim::EnergyAccum;

/// Embedded SPA assets. In debug builds rust-embed reads from the
/// `ui-assets/` folder live (so `cargo run` picks up edits without
/// rebuilding); in release builds the files are baked into the
/// binary so distribution stays single-file.
#[derive(Embed)]
#[folder = "ui-assets/"]
pub(super) struct Assets;

/// One forwarded sample, cached so the SPA can paint immediately
/// on page load instead of waiting up to a full second for the
/// next WS tick. Mirrors the `SiteEvent::MicrogridSample` payload
/// minus the `kind` discriminator.
#[derive(Clone, Debug, Serialize)]
pub struct MicrogridSampleSnapshot {
    pub quantity: &'static str,
    pub unit: &'static str,
    pub ts_ms: i64,
    pub value: Option<f32>,
}

/// Shared state for the loopback Microgrid client: the handle slot
/// plus the per-stream latest-sample cache the forwarders write to,
/// plus the live forwarder JoinHandles. `Arc`'d so the constructor
/// task, the per-stream forwarders, and the HTTP handlers all hold
/// cheap clones.
///
/// `microgrid` is `RwLock<Option<…>>` rather than a `OnceCell`
/// because the supervisor task (see `spawn_microgrid_loopback`)
/// drops + rebuilds the handle whenever the topology changes —
/// the graph crate's `ComponentGraph` is snapshotted at try_new
/// time and doesn't refresh on its own, so formulas + subscriptions
/// drift if we kept the boot-time handle. HTTP handlers take a
/// brief read lock + clone the cheap `LogicalMeterHandle` out
/// before doing any async work.
pub struct MicrogridState {
    pub microgrid: RwLock<Option<Microgrid>>,
    /// The microgrid client, built once on the first
    /// `build_microgrid` call via `MicrogridClientHandle::try_new`
    /// and reused for every rebuild — only the `LogicalMeterHandle`
    /// (which embeds the graph snapshot) gets replaced when the
    /// topology changes. A new client per rebuild would close the
    /// previous one's instructions channel, and
    /// `MicrogridClientActor` in frequenz-microgrid 0.4.1
    /// busy-spins at 100 % CPU on a closed channel (tracked
    /// upstream in frequenz-microgrid-rs). Keeping one handle
    /// clone alive forever sidesteps the bug entirely.
    ///
    /// `tokio::sync::OnceCell` rather than `RwLock<Option<_>>`
    /// because the value is set exactly once on the first
    /// successful boot.
    pub(crate) client: tokio::sync::OnceCell<MicrogridClientHandle>,
    /// Latest sample seen per stream name. Forwarders overwrite on
    /// each recv; the `/api/microgrid/latest` endpoint snapshots the
    /// whole map on each call. `parking_lot::RwLock` because writes
    /// are non-async (no await between lock + drop) and contention
    /// is tiny (one writer per stream at 1 Hz). A rebuild prunes it
    /// to the streams the new graph publishes, so absent streams
    /// don't surface stale values while the rest carry on; a new run
    /// (site reset) clears it.
    pub latest: RwLock<HashMap<&'static str, MicrogridSampleSnapshot>>,
    /// Rolling history per stream (timestamp + value), ring-buffered
    /// to 1000 entries — 15 minutes at the 1 Hz forwarder cadence
    /// with a little slack. Feeds `/api/microgrid/history` so the
    /// Dashboard tile sparklines can backfill on page load instead
    /// of starting empty. Pruned and cleared on rebuilds like
    /// `latest`.
    pub history: RwLock<HashMap<&'static str, VecDeque<HistorySample>>>,
    /// Currently-running forwarder tasks. Rebuilds abort these +
    /// spawn fresh ones bound to the new Microgrid handle's
    /// subscriptions. Dropping the old `Microgrid` alone isn't
    /// enough — the formulas captured inside the spawned tasks
    /// hold sender clones of the underlying actor mpsc, so the
    /// actor stays alive and the forwarders keep recv'ing
    /// indefinitely without explicit abort.
    pub forwarders: Mutex<Vec<JoinHandle<()>>>,
    /// Running energy integral per aggregate stream (`grid_energy`,
    /// …). Kept here, *not* in `latest`, precisely because it must
    /// survive rebuilds: a topology mutation prunes `latest` of the
    /// streams the new graph dropped, but the cumulative energy the run
    /// has moved so far must not reset to zero. The latest cache
    /// re-derives its `*_energy` snapshot from this on the next
    /// forwarded sample.
    pub energy: RwLock<HashMap<&'static str, EnergyAccum>>,
    /// The site run generation the `energy` totals belong to (see
    /// `MicrogridSite::run_generation`). A rebuild seeing a different
    /// generation clears the totals, and `latest` and `history` with
    /// them: the site was reset by a config reload, so they all
    /// belong to a previous run.
    pub energy_generation: AtomicU64,
}

pub type SharedMicrogrid = Arc<MicrogridState>;

pub fn new_microgrid_slot() -> SharedMicrogrid {
    Arc::new(MicrogridState {
        microgrid: RwLock::new(None),
        client: tokio::sync::OnceCell::new(),
        latest: RwLock::new(HashMap::new()),
        history: RwLock::new(HashMap::new()),
        forwarders: Mutex::new(Vec::new()),
        energy: RwLock::new(HashMap::new()),
        energy_generation: AtomicU64::new(0),
    })
}

/// One point on a microgrid_sample stream's rolling history ring.
/// Cap = `MICROGRID_HISTORY_CAP` (15 min at 1 Hz with slack);
/// oldest entry drops on insert when full.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct HistorySample {
    pub ts_ms: i64,
    pub value: Option<f32>,
}

pub(super) const MICROGRID_HISTORY_CAP: usize = 1000;

/// Enterprise map from microgrid id to its loopback state. Each
/// `MicrogridServer` registered in `Config::microgrids` gets one
/// entry — the supervisor for each entry pulls samples through
/// the matching microgrid's gRPC server and feeds the entry's
/// per-stream cache.
///
/// `BTreeMap` keeps the entries ordered by id so the UI's
/// Microgrids list and `/api/mg/{id}/microgrid/latest` lookups
/// stay deterministic. Behind an `Arc<RwLock>` so handlers can
/// take a read lock for lookups without blocking new-microgrid
/// inserts coming from the create-microgrid endpoint.
pub type MicrogridLoopbacks = Arc<RwLock<std::collections::BTreeMap<u64, SharedMicrogrid>>>;

pub fn new_microgrid_loopbacks() -> MicrogridLoopbacks {
    Arc::new(RwLock::new(std::collections::BTreeMap::new()))
}

/// Callback the binary's registered-microgrid listener invokes for
/// each microgrid that lands in the registry after boot: spawn the
/// physics tick + history sampler + Microgrid gRPC server + loopback
/// client. The concrete implementation lives in
/// `src/bin/macrocosim.rs`; test fixtures run no listener, so
/// runtime-created entries simply get no runtime there.
///
/// Args: `(id, name, grpc_port, site)`.
pub type MicrogridSpawner = Arc<dyn Fn(u64, &str, u16, crate::sim::MicrogridSite) + Send + Sync>;
