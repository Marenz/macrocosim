//! Server-side script browser backing the Microgrids tab's
//! Load-script dialog: lists directories and `.lisp` files under the
//! state dir so a script can be picked by click instead of typed.
//! Listing is confined to the state-dir subtree; the dialog's
//! free-text path field covers anything outside it.

use std::path::Path;

use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::ui::api::{ApiError, Json, Query};

#[derive(Deserialize)]
pub(in crate::ui) struct ScriptsQuery {
    /// Directory to list, relative to the state dir. Empty / absent
    /// lists the state dir itself.
    #[serde(default)]
    dir: String,
}

#[derive(Serialize)]
pub(in crate::ui) struct ScriptsListing {
    /// The listed directory, state-dir-relative ("" is the root).
    dir: String,
    /// Directory to navigate up to; `null` at the root.
    parent: Option<String>,
    dirs: Vec<String>,
    files: Vec<String>,
}

pub(in crate::ui) async fn scripts_list(
    State(config): State<Config>,
    Query(q): Query<ScriptsQuery>,
) -> Result<Json<ScriptsListing>, ApiError> {
    if q.dir.contains("..") || q.dir.contains('\\') || Path::new(&q.dir).is_absolute() {
        return Err(ApiError::bad_request("invalid dir"));
    }
    let rel = q.dir.trim_matches('/').to_string();
    let root = config
        .state_dir()
        .canonicalize()
        .map_err(|e| ApiError::internal(format!("state dir: {e}")))?;
    let target = if rel.is_empty() {
        root.clone()
    } else {
        root.join(&rel)
    };
    // Defense in depth, same shape as the scenario-CSV download: the
    // string checks above guard the request, canonicalize + prefix
    // guard whatever it resolves to (a symlink planted under the
    // state dir must not open the rest of the filesystem to listing).
    let canon = target
        .canonicalize()
        .map_err(|_| ApiError::not_found("no such directory"))?;
    if !canon.starts_with(&root) {
        return Err(ApiError::bad_request("directory is outside the state dir"));
    }
    let entries =
        std::fs::read_dir(&canon).map_err(|e| ApiError::internal(format!("read dir: {e}")))?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            dirs.push(name);
        } else if name.ends_with(".lisp") {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    let parent = if rel.is_empty() {
        None
    } else {
        Some(match rel.rfind('/') {
            Some(idx) => rel[..idx].to_string(),
            None => String::new(),
        })
    };
    Ok(Json(ScriptsListing {
        dir: rel,
        parent,
        dirs,
        files,
    }))
}
