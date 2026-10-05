//! HTTP handler glue. Each topic lives in its own submodule.
//! `register` in `super::router` wires every route through one of
//! the `pub(in crate::ui)` fns exported here.

use crate::lisp::Config;

use super::api::ApiError;
use super::state::{MicrogridLoopbacks, SharedMicrogrid};

pub(in crate::ui) mod assets;
pub(in crate::ui) mod component;
pub(in crate::ui) mod control;
pub(in crate::ui) mod defaults;
pub(in crate::ui) mod dispatches;
pub(in crate::ui) mod ev;
pub(in crate::ui) mod eval;
pub(in crate::ui) mod formula;
pub(in crate::ui) mod history;
pub(in crate::ui) mod microgrid_data;
pub(in crate::ui) mod microgrids;
pub(in crate::ui) mod scenarios;
pub(in crate::ui) mod scripts;
pub(in crate::ui) mod snapshots;
pub(in crate::ui) mod topology;
pub(in crate::ui) mod undo;
pub(in crate::ui) mod weather;

/// Look up the site for `mg_id` in the registry. Per-microgrid
/// handlers call this at the start; a miss returns 404 verbatim
/// so the SPA can highlight a stale microgrid card and reload
/// without retrying every per-mg fetch on the page.
pub(in crate::ui) fn resolve_site(
    config: &Config,
    mg_id: u64,
) -> Result<crate::sim::MicrogridSite, ApiError> {
    config
        .microgrids()
        .lock()
        .get(&mg_id)
        .map(|e| e.site.clone())
        .ok_or_else(|| ApiError::not_registered(mg_id))
}

/// Guard for per-mg handlers that don't need the site itself: 404
/// (same shape as [`resolve_site`]) when `mg_id` isn't registered.
pub(in crate::ui) fn require_mg(config: &Config, mg_id: u64) -> Result<(), ApiError> {
    if config.microgrids().lock().contains_key(&mg_id) {
        Ok(())
    } else {
        Err(ApiError::not_registered(mg_id))
    }
}

/// Run `f` on the blocking pool, mapping a task panic to a 500 —
/// the spawn_blocking boilerplate every interpreter-touching
/// handler otherwise repeats. Callers keep only their domain error
/// mapping.
pub(in crate::ui) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("task panicked: {e}")))
}

/// [`blocking`] under the create lock, for a route that claims a
/// microgrid id and a port and then writes and loads a file. The
/// guard moves into the blocking task, so it is held until the load
/// ends even if the client goes away and the handler is dropped.
pub(in crate::ui) async fn blocking_under_create_lock<T: Send + 'static>(
    config: &Config,
    f: impl FnOnce(&Config) -> T + Send + 'static,
) -> Result<T, ApiError> {
    let guard = config.create_lock().lock_owned().await;
    let config = config.clone();
    blocking(move || {
        let _guard = guard;
        f(&config)
    })
    .await
}

/// Mirror of [`resolve_site`] for the loopback Microgrid client
/// slot a microgrid owns. Used by the per-mg
/// `/microgrid/{status,latest,formulas}` endpoints.
pub(in crate::ui) fn resolve_loopback(
    loopbacks: &MicrogridLoopbacks,
    mg_id: u64,
) -> Result<SharedMicrogrid, ApiError> {
    loopbacks
        .read()
        .get(&mg_id)
        .cloned()
        .ok_or_else(|| ApiError::not_registered(mg_id))
}
