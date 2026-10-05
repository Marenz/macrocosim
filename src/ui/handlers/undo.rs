//! Per-microgrid undo / redo endpoints under `/api/mg/{mg}/`.
//!
//! The history lives on the server (see [`crate::lisp::undo`]): each
//! structural edit stacks the generated block the microgrid's file
//! carried before it, and these endpoints walk that stack. The UI
//! keeps no stacks of its own — a reload of the page, or a second
//! browser tab, sees the same history.

use axum::extract::State;

use crate::lisp::{Config, UndoDepths};
use crate::ui::api::{ApiError, Json, Mg};

/// GET /api/mg/{mg}/undo — how deep each stack is, so the UI can
/// enable or grey out its buttons without trying a step first.
pub(in crate::ui) async fn undo_depths(
    State(config): State<Config>,
    mg: Mg,
) -> Json<serde_json::Value> {
    Json(depths_json(config.undo_depths(mg.id)))
}

/// POST /api/mg/{mg}/undo — step one structural edit back.
pub(in crate::ui) async fn undo(
    State(config): State<Config>,
    mg: Mg,
) -> Result<Json<serde_json::Value>, ApiError> {
    step(config, mg.id, true).await
}

/// POST /api/mg/{mg}/redo — step one structural edit forward.
pub(in crate::ui) async fn redo(
    State(config): State<Config>,
    mg: Mg,
) -> Result<Json<serde_json::Value>, ApiError> {
    step(config, mg.id, false).await
}

/// Shared body: both directions rewrite a file and reload it, which
/// is blocking work behind the interpreter lock.
async fn step(
    config: Config,
    mg_id: u64,
    backwards: bool,
) -> Result<Json<serde_json::Value>, ApiError> {
    let depths = super::blocking(move || {
        if backwards {
            config.undo(mg_id)
        } else {
            config.redo(mg_id)
        }
    })
    .await?
    // "Nothing to undo" is a request that cannot be satisfied in the
    // world's current state, not a server fault; so is an unmanaged
    // microgrid. Everything reaching here is one of those or an IO
    // failure, and 409 keeps the UI from treating it as a crash.
    .map_err(ApiError::conflict)?;
    Ok(Json(depths_json(depths)))
}

fn depths_json(depths: UndoDepths) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "undo_depth": depths.undo,
        "redo_depth": depths.redo,
    })
}
