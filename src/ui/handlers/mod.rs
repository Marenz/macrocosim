//! HTTP handler glue. Each topic lives in its own submodule.
//! `register` in `super::router` wires every route through one of
//! the `pub(in crate::ui)` fns exported here.

use crate::lisp::Config;

use super::api::ApiError;

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

/// Run `f` on the blocking pool, mapping a task panic to a 500 —
/// the spawn_blocking boilerplate every interpreter-touching
/// handler otherwise repeats. Callers keep only their domain error
/// mapping.
pub(in crate::ui) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    blocking_with("task", f).await
}

/// [`blocking`] with `label` naming the task in the 500's text:
/// `{label} panicked: …`.
pub(in crate::ui) async fn blocking_with<T: Send + 'static>(
    label: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("{label} panicked: {e}")))
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
