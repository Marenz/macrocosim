//! `/api/eval` + per-mg variant + `/api/format` (tulisp-fmt).

use axum::{extract::State, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::ui::api::{ApiError, Json, Path, Query, Text};

#[derive(Serialize)]
pub(in crate::ui) struct EvalResponse {
    /// Whether the expression evaluated without an error. False ==
    /// `error` populated, `value` null. True == `value` holds the
    /// Display formatted result.
    ok: bool,
    value: Option<String>,
    error: Option<String>,
}

/// Evaluate a Lisp expression on the running interpreter. Wrapped in
/// `spawn_blocking` because tulisp's `SharedMut` is std-sync-RwLock-
/// backed and grabbing the write lock from the executor thread would
/// stall every other tokio task waiting on that worker.
///
/// Always returns 200 — application-layer success/failure rides in
/// the JSON body. Reserves HTTP 4xx/5xx for transport-level problems
/// (bad UTF-8, the spawn_blocking task panicking, etc.).
pub(in crate::ui) async fn eval(
    State(config): State<Config>,
    Text(body): Text,
) -> impl IntoResponse {
    eval_response(tokio::task::spawn_blocking(move || config.eval(&body)).await)
}

pub(in crate::ui) async fn eval_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    Text(body): Text,
) -> impl IntoResponse {
    if !config.microgrids().lock().contains_key(&mg_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(format!("microgrid {mg_id} not registered")),
            }),
        );
    }
    // eval_in_mg holds the interpreter lock across scope-set + eval +
    // overrides append + version bump, so two concurrent per-mg evals
    // can't cross microgrids.
    let result = tokio::task::spawn_blocking(move || config.eval_in_mg(mg_id, &body)).await;
    eval_response(result)
}

fn eval_response(
    result: Result<Result<String, String>, tokio::task::JoinError>,
) -> (StatusCode, Json<EvalResponse>) {
    match result {
        Ok(Ok(value)) => (
            StatusCode::OK,
            Json(EvalResponse {
                ok: true,
                value: Some(value),
                error: None,
            }),
        ),
        Ok(Err(error)) => (
            StatusCode::OK,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(error),
            }),
        ),
        Err(join_err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(EvalResponse {
                ok: false,
                value: None,
                error: Some(format!("eval task panicked: {join_err}")),
            }),
        ),
    }
}

#[derive(Deserialize)]
pub(in crate::ui) struct FormatQuery {
    /// Column budget for the formatter. Optional; defaults to 80.
    /// Clamped to a sane range so a stray client can't make
    /// `tulisp-fmt` chew through pathological inputs.
    width: Option<usize>,
}

/// Pretty-print a Lisp source string via `tulisp-fmt`. The body is
/// the raw source; the response is the formatted source as
/// text/plain. Returns 400 with the formatter's error message on
/// parse failure so the REPL can keep the user's input untouched
/// and surface the diagnostic.
pub(in crate::ui) async fn format(
    Query(q): Query<FormatQuery>,
    Text(body): Text,
) -> Result<String, ApiError> {
    let width = q.width.unwrap_or(80).clamp(20, 200);
    // spawn_blocking like every other CPU-bound handler: a large,
    // deeply nested body would otherwise stall a tokio worker.
    tokio::task::spawn_blocking(move || {
        tulisp_fmt::format_with_width(&body, width)
            .map_err(|e| ApiError::bad_request(e.to_string()))
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?
}
