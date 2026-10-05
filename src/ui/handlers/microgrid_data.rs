//! Per-microgrid dashboard data: latest samples, history rings,
//! status, formulas, plus the singleton `/api/clock` endpoint.

use std::collections::HashMap;

use axum::{Extension, extract::State, http::StatusCode};
use serde::Serialize;

use crate::lisp::Config;

use super::super::state::{
    HistorySample, MicrogridLoopbacks, MicrogridSampleSnapshot, SharedMicrogrid,
};
use crate::ui::api::{Json, Mg};

#[derive(Serialize)]
pub(in crate::ui) struct MicrogridStatusResp {
    /// Loopback handle is up and the component graph built.
    /// Mirrors `Microgrid::try_new`'s success guarantee — if this
    /// is true, every `LogicalMeterHandle::xxx<M>()` is reachable.
    connected: bool,
    /// Round-trip count from `list_electrical_components` —
    /// confirms macrocosim's gRPC server returned what the
    /// graph crate accepted.
    component_count: Option<usize>,
}

/// The loopback slot for `mg`, present once its runtime has started.
fn loopback_of(loopbacks: &MicrogridLoopbacks, mg: &Mg) -> Option<SharedMicrogrid> {
    loopbacks.read().get(&mg.id).cloned()
}

/// Whether the loopback client is connected. 503 with
/// `connected: false` when it is not, or when the microgrid's
/// runtime has not started.
pub(in crate::ui) async fn microgrid_status(
    mg: Mg,
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
) -> (StatusCode, Json<MicrogridStatusResp>) {
    let lm = loopback_of(&loopbacks, &mg)
        .and_then(|slot| slot.microgrid.read().as_ref().map(|m| m.logical_meter()));
    if let Some(lm) = lm {
        let count = lm.graph().components().count();
        (
            StatusCode::OK,
            Json(MicrogridStatusResp {
                connected: true,
                component_count: Some(count),
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(MicrogridStatusResp {
                connected: false,
                component_count: None,
            }),
        )
    }
}

/// Latest cached sample for every active aggregated stream.
/// Returns a `{ stream: snapshot }` map; absent streams (no PV in
/// the topology, no batteries, etc.) simply don't appear in the
/// map, and a microgrid whose runtime has not started answers `{}`.
/// Lets the SPA's Dashboard paint a populated tile on page load
/// instead of holding "loading…" until the next WS tick.
pub(in crate::ui) async fn microgrid_latest(
    mg: Mg,
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
) -> Json<HashMap<&'static str, MicrogridSampleSnapshot>> {
    Json(
        loopback_of(&loopbacks, &mg)
            .map(|slot| slot.latest.read().clone())
            .unwrap_or_default(),
    )
}

/// The history ring of every aggregated stream; `{}` for a
/// microgrid whose runtime has not started.
pub(in crate::ui) async fn microgrid_history(
    mg: Mg,
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
) -> Json<HashMap<&'static str, Vec<HistorySample>>> {
    Json(
        loopback_of(&loopbacks, &mg)
            .map(|slot| {
                slot.history
                    .read()
                    .iter()
                    .map(|(k, ring)| (*k, ring.iter().copied().collect()))
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// Rendered formula strings (e.g. `"#1 + COALESCE(#2, #3, 0.0)"`)
/// per stream, lifted from the graph crate's per-category formula
/// generators. Inspection-only — these are what the dashboard
/// tooltip surfaces so a developer reading "−25 kW" can see which
/// component ids participate and how. Absent categories don't
/// appear in the response.
///
/// 503 `{}` when the loopback Microgrid handle hasn't built its
/// ComponentGraph yet, or the runtime has not started — same
/// lifecycle as `microgrid/status`.
pub(in crate::ui) async fn microgrid_formulas(
    mg: Mg,
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
) -> (StatusCode, Json<HashMap<&'static str, String>>) {
    let lm = loopback_of(&loopbacks, &mg)
        .and_then(|slot| slot.microgrid.read().as_ref().map(|m| m.logical_meter()));
    let Some(lm) = lm else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(HashMap::new()));
    };
    let graph = lm.graph();
    let mut out: HashMap<&'static str, String> = HashMap::new();
    if let Ok(f) = graph.grid_formula() {
        out.insert("grid_power", format!("{f}"));
    }
    if let Ok(f) = graph.battery_formula(None) {
        out.insert("battery_pool_power", format!("{f}"));
    }
    if let Ok(f) = graph.pv_formula(None) {
        out.insert("pv_power", format!("{f}"));
    }
    if let Ok(f) = graph.consumer_formula() {
        out.insert("consumer_power", format!("{f}"));
    }
    if let Ok(f) = graph.producer_formula() {
        out.insert("producer_power", format!("{f}"));
    }
    (StatusCode::OK, Json(out))
}

#[derive(Serialize)]
pub(in crate::ui) struct ClockInfo {
    /// IANA timezone name set via `(set-timezone …)`, default
    /// Europe/Berlin. UI passes this to `Intl.DateTimeFormat` to
    /// format the pulse-bar clock + (future) per-component
    /// timestamps in the configured civil zone.
    tz: &'static str,
}

pub(in crate::ui) async fn clock_info(State(config): State<Config>) -> Json<ClockInfo> {
    Json(ClockInfo {
        tz: config.tz_name(),
    })
}
