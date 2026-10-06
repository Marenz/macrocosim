//! Shared state types for the UI subsystem: the per-microgrid
//! loopback cache (latest + history rings + forwarder handles), the
//! enterprise map of loopback states, and the embedded-assets handle.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use frequenz_microgrid::Microgrid;
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
    /// Epoch milliseconds; serialized as RFC 3339 `ts`.
    #[serde(
        rename = "ts",
        serialize_with = "crate::timefmt::serialize_millis_as_rfc3339"
    )]
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
    /// Latest sample seen per stream name. Forwarders overwrite on
    /// each recv; `GET /api/mg/{mg}/metrics/latest` snapshots the
    /// whole map on each call. `parking_lot::RwLock` because writes
    /// are non-async (no await between lock + drop) and contention is
    /// tiny (one writer per stream at 1 Hz). A rebuild prunes it to
    /// the streams the new graph publishes, so absent streams don't
    /// surface stale values while the rest carry on; a new run (site
    /// reset) clears it.
    pub latest: RwLock<HashMap<&'static str, MicrogridSampleSnapshot>>,
    /// Rolling history per stream (timestamp + value), ring-buffered
    /// to 1000 entries — 15 minutes at the 1 Hz forwarder cadence
    /// with a little slack. Feeds `/api/mg/{mg}/metrics/history` so
    /// the Dashboard tile sparklines can backfill on page load
    /// instead of starting empty. Pruned and cleared on rebuilds like
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
    /// Epoch milliseconds; serialized as epoch seconds `t_s`.
    #[serde(
        rename = "t_s",
        serialize_with = "crate::timefmt::serialize_millis_as_epoch_s"
    )]
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
/// Microgrids list and `/api/mg/{mg}/metrics/latest` lookups
/// stay deterministic. Behind an `Arc<RwLock>` so handlers can
/// take a read lock for lookups without blocking new-microgrid
/// inserts coming from the create-microgrid endpoint.
pub type MicrogridLoopbacks = Arc<RwLock<std::collections::BTreeMap<u64, SharedMicrogrid>>>;

pub fn new_microgrid_loopbacks() -> MicrogridLoopbacks {
    Arc::new(RwLock::new(std::collections::BTreeMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_sample_carries_an_rfc3339_ts() {
        let snap = MicrogridSampleSnapshot {
            quantity: "Power",
            unit: "W",
            ts_ms: 1_791_288_000_123,
            value: Some(1.0),
        };
        let v = serde_json::to_value(snap).unwrap();
        assert_eq!(v["ts"], "2026-10-06T12:00:00.123Z");
        assert!(v.get("ts_ms").is_none());
    }

    #[test]
    fn history_sample_carries_epoch_seconds_as_t_s() {
        let sample = HistorySample {
            ts_ms: 1_791_288_000_500,
            value: Some(1.0),
        };
        let v = serde_json::to_value(sample).unwrap();
        assert_eq!(v["t_s"], 1_791_288_000.5);
        assert!(v.get("ts_ms").is_none());
    }
}
