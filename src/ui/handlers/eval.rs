//! `/api/eval`, `/api/mg/{mg}/eval` and `/api/format` (tulisp-fmt).

use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::ui::api::{ApiError, Json, Mg, Query, Text};

#[derive(Serialize)]
pub(in crate::ui) struct EvalResponse {
    /// The evaluated form, printed.
    value: String,
}

/// Evaluate a Lisp expression with no microgrid in scope. Runs in
/// `spawn_blocking` because tulisp's `SharedMut` is std-sync-RwLock-
/// backed and grabbing the write lock from the executor thread would
/// stall every other tokio task waiting on that worker. 400 on an
/// evaluation error.
pub(in crate::ui) async fn eval(
    State(config): State<Config>,
    Text(body): Text,
) -> Result<Json<EvalResponse>, ApiError> {
    let value = super::blocking(move || config.eval(&body))
        .await?
        .map_err(ApiError::bad_request)?;
    Ok(Json(EvalResponse { value }))
}

/// Evaluate with the route's microgrid in scope. 404 when it is not
/// registered, including when a reload removes it while the eval
/// waits for the interpreter. The scope-set, eval, overrides append
/// and version bump share one interpreter-lock acquisition, so two
/// concurrent scoped evals can't cross microgrids.
pub(in crate::ui) async fn eval_for_mg(
    State(config): State<Config>,
    mg: Mg,
    Text(body): Text,
) -> Result<Json<EvalResponse>, ApiError> {
    let mg_id = mg.id;
    let value = super::blocking(move || config.eval_in_registered_mg(mg_id, &body))
        .await?
        .ok_or_else(|| ApiError::not_registered(mg_id))?
        .map_err(ApiError::bad_request)?;
    Ok(Json(EvalResponse { value }))
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
