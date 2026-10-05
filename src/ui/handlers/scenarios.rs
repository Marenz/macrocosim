//! Scenarios HTTP surface: list the registry, start (the live runner),
//! plus the per-scenario summary / events / report readers. The old
//! stage-mutation endpoints (next/prev/jump) are gone with the
//! day-stage model.

use axum::{body::Body, extract::State, http::StatusCode, response::Response};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::sim::microgrid_site::{ScenarioReport, ScenarioSummary};
use crate::ui::api::{ApiError, Json, Path, Query};

pub(in crate::ui) async fn scenarios_list(
    State(config): State<Config>,
) -> Json<Vec<crate::sim::scenarios::ScenarioView>> {
    Json(crate::sim::scenarios::snapshot(&config.scenarios()))
}

/// Live runner: compile + start scenario NAME on the wall clock. Its
/// cue / check timers fire on the refresh loop as real time passes.
/// Runs on a blocking thread since `start` holds the interpreter lock.
pub(in crate::ui) async fn scenarios_start(
    State(config): State<Config>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let res = super::blocking(move || {
        crate::sim::scenarios::start(&config.interpreter(), &config.scenarios(), &name)
    })
    .await?;
    res.map_err(ApiError::bad_request)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Stop the running scenario (whichever the journal currently holds) —
/// the registry is single-journal, so stop is name-less. Freezes the
/// report + flushes any CSV sinks via the `scenario-stop` defun.
pub(in crate::ui) async fn scenarios_stop(
    State(config): State<Config>,
) -> Result<StatusCode, ApiError> {
    let res = super::blocking(move || config.eval("(scenario-stop)").map(|_| ())).await?;
    res.map_err(ApiError::bad_request)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Snapshot of the running scenario's lifecycle. Empty (`name:
/// null`, zero counts) before any `(scenario-start)`; freezes
/// `elapsed_s` once `(scenario-stop)` fires.
pub(in crate::ui) async fn scenario_summary(State(config): State<Config>) -> Json<ScenarioSummary> {
    Json(config.legacy_site().scenario_summary(Utc::now()))
}

#[derive(Deserialize)]
pub(in crate::ui) struct ScenarioEventsQuery {
    /// Return events with id strictly greater than this. Default 0
    /// means "everything in the ring".
    since: Option<u64>,
    /// Cap on returned entries. Default 200.
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(in crate::ui) struct ScenarioEventsResponse {
    events: Vec<crate::sim::scenario::ScenarioEvent>,
    /// `next_event_id` lets a polling client advance its `since=`
    /// cursor even when this batch was empty (because no events
    /// have arrived since last poll, but new ones might before the
    /// next).
    next_event_id: u64,
    /// Lowest event id still in the ring. Clients comparing
    /// `earliest_event_id > since` know their cursor was inside
    /// the evicted window and they missed `earliest_event_id - since`
    /// entries.
    earliest_event_id: u64,
}

pub(in crate::ui) async fn scenario_events(
    State(config): State<Config>,
    Query(q): Query<ScenarioEventsQuery>,
) -> Json<ScenarioEventsResponse> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(200).min(1000);
    let events = config.legacy_site().scenario_events_since(since, limit);
    let summary = config.legacy_site().scenario_summary(Utc::now());
    Json(ScenarioEventsResponse {
        events,
        next_event_id: summary.next_event_id,
        earliest_event_id: summary.earliest_event_id,
    })
}

#[derive(Serialize)]
pub(in crate::ui) struct ScenarioCsvList {
    /// Recording directory (absolute or config-relative), or null.
    dir: Option<String>,
    files: Vec<String>,
}

/// The CSV files the active/most-recent recording wrote, for the run
/// view's download links. Empty when nothing has been recorded.
pub(in crate::ui) async fn scenario_csv_list(
    State(config): State<Config>,
) -> Json<ScenarioCsvList> {
    match config.legacy_site().scenario_csv_listing() {
        Some((dir, files)) => Json(ScenarioCsvList {
            dir: Some(dir.to_string_lossy().into_owned()),
            files,
        }),
        None => Json(ScenarioCsvList {
            dir: None,
            files: Vec::new(),
        }),
    }
}

/// Download one recorded CSV by file name. The name must be a bare
/// `*.csv` file with no path separators (rejecting traversal) and must
/// be one the listing actually reports — so this only ever serves
/// files inside the recording directory.
pub(in crate::ui) async fn scenario_csv_file(
    State(config): State<Config>,
    Path(file): Path<String>,
) -> Result<Response, ApiError> {
    if file.contains('/') || file.contains('\\') || file.contains("..") || !file.ends_with(".csv") {
        return Err(ApiError::bad_request("invalid filename"));
    }
    let (dir, files) = config
        .legacy_site()
        .scenario_csv_listing()
        .ok_or_else(|| ApiError::not_found("no recording"))?;
    if !files.contains(&file) {
        return Err(ApiError::not_found("no such file"));
    }
    // Defense in depth: resolve symlinks and confirm the entry is a
    // regular file that stays inside the recording directory, so a
    // symlink planted in that dir can't turn this into an arbitrary
    // file read. The name checks above guard the request string; this
    // guards the entry it resolves to.
    let canon_dir = dir
        .canonicalize()
        .map_err(|e| ApiError::not_found(e.to_string()))?;
    let canon = dir
        .join(&file)
        .canonicalize()
        .map_err(|e| ApiError::not_found(e.to_string()))?;
    if !canon.starts_with(&canon_dir) || !canon.is_file() {
        return Err(ApiError::bad_request("invalid file"));
    }
    let body = std::fs::read(&canon).map_err(|e| ApiError::not_found(e.to_string()))?;
    Response::builder()
        .header("content-type", "text/csv")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{file}\""),
        )
        .body(Body::from(body))
        .map_err(|e| ApiError::internal(e.to_string()))
}

/// Aggregate metrics for the running scenario (peak grid power so
/// far, energy integrals, SoC stats, checks). Independent of
/// `/api/scenario/events` so a dashboard can poll metrics
/// frequently without scanning the whole event log.
pub(in crate::ui) async fn scenario_report(State(config): State<Config>) -> Json<ScenarioReport> {
    Json(config.legacy_site().scenario_report(Utc::now()))
}
