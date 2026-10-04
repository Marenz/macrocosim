//! Per-microgrid snapshot endpoints — list / save / load under
//! `/api/mg/{mg_id}/snapshots`. Each one wraps a `Config` call that
//! does blocking file IO (and, for load, a reload) on the blocking
//! pool.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::lisp::{Config, SnapshotError};

#[derive(Serialize)]
pub(in crate::ui) struct SnapshotsListResp {
    snapshots: Vec<String>,
}

pub(in crate::ui) async fn snapshots_list_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
) -> Result<Json<SnapshotsListResp>, (StatusCode, String)> {
    super::require_mg(&config, mg_id)?;
    Ok(Json(SnapshotsListResp {
        snapshots: config.list_snapshots_for(mg_id),
    }))
}

#[derive(Deserialize)]
pub(in crate::ui) struct SnapshotsSaveBody {
    name: String,
}

pub(in crate::ui) async fn snapshots_save_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    Json(body): Json<SnapshotsSaveBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = super::blocking(move || config.save_snapshot_for(mg_id, &body.name))
        .await?
        .map_err(status_for)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
    })))
}

#[derive(Deserialize)]
pub(in crate::ui) struct SnapshotsLoadBody {
    name: String,
    /// Load the snapshot as a NEW microgrid under this id instead of
    /// restoring it over the original. Omit to restore in place.
    #[serde(default)]
    as_id: Option<u64>,
}

pub(in crate::ui) async fn snapshots_load_for_mg(
    State(config): State<Config>,
    axum::Extension(runtimes): axum::Extension<crate::runtime::MicrogridRuntimes>,
    Path(mg_id): Path<u64>,
    Json(body): Json<SnapshotsLoadBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let as_id = body.as_id;
    let load = move |cfg: &Config| cfg.load_snapshot_for(mg_id, &body.name, body.as_id);
    // Loading `as_id` is a load-as: it claims an id and a port.
    let loaded = if as_id.is_some() {
        super::blocking_under_create_lock(&config, load).await?
    } else {
        super::blocking(move || load(&config)).await?
    }
    .map_err(status_for)?;
    // Loaded as a new microgrid: wait for its runtime and report it.
    if as_id.is_some()
        && let Some(id) = loaded
    {
        return Ok(Json(serde_json::json!({
            "ok": true,
            "id": id,
            "runtime": super::microgrids::start_and_report(&runtimes, id).await,
        })));
    }
    Ok(Json(serde_json::json!({ "ok": true, "id": loaded })))
}

/// Map a snapshot failure onto the status code that describes it:
/// the caller's fault (name, missing snapshot, unmanaged microgrid)
/// versus ours (IO, a failed reload).
fn status_for(e: SnapshotError) -> (StatusCode, String) {
    let status = match e {
        SnapshotError::InvalidName(_) => StatusCode::BAD_REQUEST,
        SnapshotError::NotFound(_) => StatusCode::NOT_FOUND,
        SnapshotError::Unmanaged(_) => StatusCode::CONFLICT,
        SnapshotError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}
