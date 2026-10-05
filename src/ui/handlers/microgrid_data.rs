//! Per-microgrid dashboard data: latest samples, history rings,
//! status, formulas, plus the singleton `/api/clock` endpoint.

use std::collections::HashMap;

use axum::{Extension, extract::State, http::StatusCode};
use serde::Serialize;

use crate::lisp::Config;

use super::super::state::{
    HistorySample, MicrogridLoopbacks, MicrogridSampleSnapshot, SharedMicrogrid,
};
use super::resolve_loopback;
use crate::ui::api::{ApiError, Json, Path};

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

pub(in crate::ui) async fn microgrid_status_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<(StatusCode, Json<MicrogridStatusResp>), ApiError> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(microgrid_status_body(&slot))
}

pub(in crate::ui) async fn microgrid_latest_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<Json<HashMap<&'static str, MicrogridSampleSnapshot>>, ApiError> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(Json(slot.latest.read().clone()))
}

pub(in crate::ui) async fn microgrid_history_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<Json<HashMap<&'static str, Vec<HistorySample>>>, ApiError> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(Json(microgrid_history_body(&slot)))
}

pub(in crate::ui) async fn microgrid_history(
    Extension(state): Extension<SharedMicrogrid>,
) -> Json<HashMap<&'static str, Vec<HistorySample>>> {
    Json(microgrid_history_body(&state))
}

fn microgrid_history_body(state: &SharedMicrogrid) -> HashMap<&'static str, Vec<HistorySample>> {
    state
        .history
        .read()
        .iter()
        .map(|(k, ring)| (*k, ring.iter().copied().collect()))
        .collect()
}

pub(in crate::ui) async fn microgrid_formulas_for_mg(
    Extension(loopbacks): Extension<MicrogridLoopbacks>,
    Path(mg_id): Path<u64>,
) -> Result<(StatusCode, Json<HashMap<&'static str, String>>), ApiError> {
    let slot = resolve_loopback(&loopbacks, mg_id)?;
    Ok(microgrid_formulas_body(&slot))
}

fn microgrid_status_body(state: &SharedMicrogrid) -> (StatusCode, Json<MicrogridStatusResp>) {
    let lm = state.microgrid.read().as_ref().map(|mg| mg.logical_meter());
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

pub(in crate::ui) async fn microgrid_status(
    Extension(state): Extension<SharedMicrogrid>,
) -> (StatusCode, Json<MicrogridStatusResp>) {
    microgrid_status_body(&state)
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

/// Latest cached sample for every active aggregated stream.
/// Returns a `{ stream: snapshot }` map; absent streams (no PV in
/// the topology, no batteries, etc.) simply don't appear in the
/// map. Lets the SPA's Dashboard paint a populated tile on page
/// load instead of holding "loading…" until the next WS tick.
pub(in crate::ui) async fn microgrid_latest(
    Extension(state): Extension<SharedMicrogrid>,
) -> Json<HashMap<&'static str, MicrogridSampleSnapshot>> {
    Json(state.latest.read().clone())
}

/// Rendered formula strings (e.g. `"#1 + COALESCE(#2, #3, 0.0)"`)
/// per stream, lifted from the graph crate's per-category formula
/// generators. Inspection-only — these are what the dashboard
/// tooltip surfaces so a developer reading "−25 kW" can see which
/// component ids participate and how. Absent categories don't
/// appear in the response.
///
/// 503 when the loopback Microgrid handle hasn't built its
/// ComponentGraph yet — same lifecycle as `/api/microgrid/status`.
pub(in crate::ui) async fn microgrid_formulas(
    Extension(state): Extension<SharedMicrogrid>,
) -> (StatusCode, Json<HashMap<&'static str, String>>) {
    microgrid_formulas_body(&state)
}

fn microgrid_formulas_body(
    state: &SharedMicrogrid,
) -> (StatusCode, Json<HashMap<&'static str, String>>) {
    let lm = match state.microgrid.read().as_ref() {
        Some(mg) => mg.logical_meter(),
        None => return (StatusCode::SERVICE_UNAVAILABLE, Json(HashMap::new())),
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
