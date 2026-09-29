//! Defun installers — every `ctx.defun("name", …)` the Lisp
//! interface exposes is registered here.
//!
//! Each topic lives in its own submodule with its tests. The
//! entry points used by `Config::new` are:
//!
//! - `register_runtime` — pure-defun installers (no enterprise-
//!   wide state). Walked by both the boot path and `tags_table`.
//! - `register_clock`, `register_watches`, `register_scenarios`,
//!   `register_microgrids`, `register_frequency` — installers that
//!   need a slice of the enterprise-wide state that Config built
//!   first.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use tulisp::TulispContext;

use crate::sim::microgrids::SharedSiteRouter;

use super::{Metadata, handle, make};

mod bounds;
mod clock;
mod frequency;
mod fs;
mod grid_state;
mod load_drivers;
mod log;
mod metadata;
mod microgrids;
mod queries;
mod reactive;
mod reset;
mod runtime_modes;
mod scenarios;
mod setpoints;
mod time;
mod watches;
mod weather;
mod world_ops;

pub(super) use clock::register as register_clock;
pub(super) use frequency::register as register_frequency;
pub(super) use microgrids::register as register_microgrids;
pub(super) use scenarios::register_registry as register_scenarios;
pub(super) use watches::register as register_watches;

/// Register every Rust function the config DSL needs that doesn't
/// require an enterprise-wide state handle. The remaining
/// installers (clock, scenarios, microgrids, frequency, watches)
/// are dispatched from `Config::new` directly.
pub(super) fn register_runtime(
    ctx: &mut TulispContext,
    router: SharedSiteRouter,
    metadata: Arc<RwLock<Metadata>>,
    load_dir: PathBuf,
    microgrids: crate::sim::microgrids::SharedMicrogrids,
    now: crate::sim::sim_clock::NowSource,
) {
    log::register(ctx);
    handle::register(ctx);
    make::register(ctx, router.clone());
    reset::register(ctx, router.clone());
    grid_state::register(ctx, router.clone());
    metadata::register(ctx, metadata.clone());
    runtime_modes::register(ctx, router.clone());
    load_drivers::register(ctx, router.clone());
    weather::register(ctx, router.clone());
    time::register(ctx);
    reactive::register(ctx, router.clone());
    setpoints::register(ctx, router.clone(), metadata.clone());
    bounds::register(ctx, router.clone(), metadata);
    queries::register(ctx, router.clone());
    world_ops::register(ctx, router.clone());
    scenarios::register_lifecycle(ctx, router, microgrids, now);
    fs::register(ctx, load_dir.clone());
    super::csv_profile::register(ctx, load_dir);
}
