//! `/api/mg/{mg}/history` and `/api/mg/{mg}/setpoints`: one
//! component's sample history and setpoint event log.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use crate::sim::history::Metric;
use crate::sim::setpoints::SetpointEvent;

use crate::ui::api::{ApiError, Json, Mg, Query};

#[derive(Deserialize)]
pub(in crate::ui) struct HistoryQuery {
    /// Component id to fetch history for. Required.
    id: u64,
    /// Metric name (one of `History::Metric::as_str` strings).
    /// Required.
    metric: String,
    /// Window length in seconds. Optional; defaults to the full
    /// 10-minute capacity of the ring buffer.
    window_s: Option<i64>,
}

#[derive(Serialize)]
pub(in crate::ui) struct HistoryResponse {
    id: u64,
    metric: String,
    /// Typed quantity (`"Power"`, `"ReactivePower"`, `"Frequency"`,
    /// `"Percentage"`) — mirrors the frequenz-microgrid `Sample<Q>`
    /// `Q` parameter so the SPA picks a scale family from this
    /// instead of pattern-matching on the metric name.
    quantity: &'static str,
    /// Base unit the samples are recorded in (`"W"`, `"var"`,
    /// `"Hz"`, `"%"`).
    unit: &'static str,
    /// Pairs of (timestamp_ms_since_epoch, value). The time format is
    /// JS-ready (Date.now() shape) so chart libs can plot directly.
    samples: Vec<(i64, f32)>,
}

pub(in crate::ui) async fn history(
    mg: Mg,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<HistoryResponse>, ApiError> {
    history_body(&mg.site, q)
}

fn history_body(
    site: &crate::sim::MicrogridSite,
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
        .history_window(q.id, metric, since)
        .unwrap_or_default()
        .into_iter()
        .map(|s| (s.ts.timestamp_millis(), s.value))
        .collect();
    Ok(Json(HistoryResponse {
        id: q.id,
        metric: q.metric,
        quantity: metric.quantity(),
        unit: metric.unit(),
        samples,
    }))
}

#[derive(Deserialize)]
pub(in crate::ui) struct SetpointsQuery {
    id: u64,
    /// Window length in seconds. Optional; defaults to the full
    /// 1000-event capacity of the ring (which at typical control-app
    /// rates covers several minutes).
    window_s: Option<i64>,
}

#[derive(Serialize)]
pub(in crate::ui) struct SetpointsResponse {
    id: u64,
    events: Vec<SetpointEvent>,
}

pub(in crate::ui) async fn setpoints(
    mg: Mg,
    Query(q): Query<SetpointsQuery>,
) -> Json<SetpointsResponse> {
    setpoints_body(&mg.site, q)
}

fn setpoints_body(site: &crate::sim::MicrogridSite, q: SetpointsQuery) -> Json<SetpointsResponse> {
    // Same clamp as history_body: keep a hostile window_s from
    // panicking chrono.
    let window = ChronoDuration::seconds(q.window_s.unwrap_or(600).clamp(0, 31_536_000));
    let since = Utc::now() - window;
    let events = site.setpoints_window(q.id, since);
    Json(SetpointsResponse { id: q.id, events })
}
