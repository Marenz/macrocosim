//! `/api/mg/{mg}/component/{id}/history` and
//! `/api/mg/{mg}/component/{id}/setpoints`: one component's sample
//! history and setpoint event log.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use crate::sim::history::Metric;
use crate::sim::setpoints::SetpointEvent;

use crate::ui::api::{ApiError, ComponentPath, Json, Mg, Path, Query};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::ui) struct HistoryQuery {
    /// Metric name (one of `History::Metric::as_str` strings).
    /// Required.
    metric: String,
    /// Window length in seconds. Optional; defaults to the full
    /// 10-minute capacity of the ring buffer.
    window_s: Option<i64>,
}

#[derive(Serialize)]
pub(in crate::ui) struct HistoryResponse {
    component_id: u64,
    metric: String,
    /// Typed quantity (`"Power"`, `"ReactivePower"`, `"Frequency"`,
    /// `"Percentage"`) — mirrors the frequenz-microgrid `Sample<Q>`
    /// `Q` parameter so the SPA picks a scale family from this
    /// instead of pattern-matching on the metric name.
    quantity: &'static str,
    /// Base unit the samples are recorded in (`"W"`, `"VAr"`,
    /// `"Hz"`, `"%"`).
    unit: &'static str,
    /// Pairs of (t_s, value), `t_s` in epoch seconds, the x scale
    /// chart libraries plot directly.
    samples: Vec<(f64, f32)>,
}

pub(in crate::ui) async fn history(
    mg: Mg,
    Path(ComponentPath { id }): Path<ComponentPath>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
    history_body(&mg.site, id, q)
}

fn history_body(
    site: &crate::sim::MicrogridSite,
    id: u64,
    q: HistoryQuery,
) -> Result<Json<HistoryResponse>, ApiError> {
    let metric: Metric = q
        .metric
        .parse()
        .map_err(|_| ApiError::bad_request(format!("unknown metric '{}'", q.metric)))?;
    // Clamp: chrono panics on |seconds| near i64::MAX, and a huge
    // finite window would panic in the subtraction below. One year
    // is far past any real query.
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600).clamp(0, 31_536_000));
    let since: DateTime<Utc> = Utc::now() - window;
    let samples = site
        .history_window(id, metric, since)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.ts.timestamp_millis() as f64 / 1000.0, s.value))
        .collect();
    Ok(Json(HistoryResponse {
        component_id: id,
        metric: q.metric,
        quantity: metric.quantity(),
        unit: metric.unit(),
        samples,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::ui) struct SetpointsQuery {
    /// Window length in seconds. Optional; defaults to the full
    /// 1000-event capacity of the ring (which at typical control-app
    /// rates covers several minutes).
    window_s: Option<i64>,
}

#[derive(Serialize)]
pub(in crate::ui) struct SetpointsResponse {
    component_id: u64,
    events: Vec<SetpointEventView>,
}

/// A logged setpoint event plus the unit its `value` is in.
#[derive(Serialize)]
struct SetpointEventView {
    #[serde(flatten)]
    event: SetpointEvent,
    unit: &'static str,
}

pub(in crate::ui) async fn setpoints(
    mg: Mg,
    Path(ComponentPath { id }): Path<ComponentPath>,
    Query(q): Query<SetpointsQuery>,
) -> Json<SetpointsResponse> {
    setpoints_body(&mg.site, id, q)
}

fn setpoints_body(
    site: &crate::sim::MicrogridSite,
    id: u64,
    q: SetpointsQuery,
) -> Json<SetpointsResponse> {
    // Same clamp as history_body: keep a hostile window_s from
    // panicking chrono.
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600).clamp(0, 31_536_000));
    let since = Utc::now() - window;
    let events = site
        .setpoints_window(id, since)
        .into_iter()
        .map(|event| SetpointEventView {
            unit: event.kind.unit(),
            event,
        })
        .collect();
    Json(SetpointsResponse {
        component_id: id,
        events,
    })
}
