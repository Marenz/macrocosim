//! Lisp glue: load the config DSL, register the `make-*` functions
//! against a `MicrogridSite`, and act as the runtime entry point for
//! the gRPC server (which reads the same sites through their
//! gateways).
//!
//! The `Config` struct is intentionally thin — the simulation state
//! lives in `MicrogridSite`, the lisp interpreter is just the configuration
//! frontend. Behaviour is fanned out across child modules:
//!
//! - `boot` — `Config::new`, the long-lived lisp refresh loop, the
//!   tags-table pass, hot-reload + watch.
//! - `overrides` — `eval` and the file regeneration it triggers.
//! - `snapshots` — `save_snapshot_for` / `load_snapshot_for` against
//!   a microgrid's own file.
//! - `undo` — the per-microgrid undo / redo stacks over managed
//!   microgrid files.
//! - `defuns` — every `ctx.defun(...)` installer the config DSL
//!   exposes.

mod boot;
pub mod csv_profile;
mod defuns;
pub mod handle;
pub mod make;
pub mod microgrid_file;
mod overrides;
pub mod runtime_modes;
mod snapshots;
mod undo;
pub mod value;

#[cfg(test)]
mod test_support;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tulisp::{SharedMut, TulispContext};

use crate::sim::MicrogridSite;
use crate::sim::microgrids::{CurrentMicrogrid, SharedSiteRouter};

pub use boot::{LoadAsError, LoadError};
pub use snapshots::SnapshotError;
pub use undo::UndoDepths;

/// The ports of the enterprise's assets and dispatch sockets. An
/// address that does not parse contributes nothing; the binary
/// validates it when it binds.
pub(crate) fn reserved_ports_of(m: &Metadata) -> Vec<u16> {
    [&m.assets_socket_addr, &m.dispatch_socket_addr]
        .into_iter()
        .filter_map(|a| a.parse::<std::net::SocketAddr>().ok())
        .map(|a| a.port())
        .collect()
}

/// Enterprise-level gateway settings the Lisp config can override.
/// Per-microgrid identity (id, name, grpc_port, TSO) lives in the
/// `sim::microgrids` registry — each `(make-microgrid …)` form
/// inserts one entry. Metadata here only carries enterprise-wide
/// knobs: the enterprise id surfaced on every gRPC `MicrogridInfo`,
/// the assets server's bind address, and the default
/// request-lifetime fallback.
#[derive(Debug, Clone)]
pub struct Metadata {
    pub enterprise_id: u64,
    /// Address the PlatformAssets gRPC service binds to.
    /// Independent of any microgrid's `grpc_port` so a sibling
    /// service (assets / reporting / future API surfaces) doesn't
    /// fight a microgrid for its socket. Overridable from lisp
    /// via `(set-assets-socket-addr "[::1]:9900")`.
    pub assets_socket_addr: String,
    /// Address the single (enterprise-wide) `MicrogridDispatchService`
    /// gRPC service binds to. One service fronts every microgrid,
    /// keyed by `microgrid_id` in each request, so it gets its own
    /// socket — distinct from any microgrid's `grpc_port` and from
    /// the assets server.
    /// Overridable from lisp via `(set-dispatch-socket-addr "…")`.
    pub dispatch_socket_addr: String,
    /// Fallback request lifetime when a `SetElectricalComponentPower`
    /// caller doesn't supply `request_lifetime`. Mirrors microsim's
    /// `retain-requests-duration-ms`. Tunable via
    /// `(set-default-request-lifetime-ms N)`. The gRPC handler's
    /// per-request validation in `server::resolve_lifetime` clamps
    /// to `[REQUEST_LIFETIME_MIN_S, REQUEST_LIFETIME_MAX_S]`; this
    /// default isn't clamped (a config that wants short / long
    /// fallbacks is responsible for picking values that align with
    /// its operational expectations).
    pub default_request_lifetime: Duration,
    /// Fallback lifetime for an Augment request that carries none.
    /// The proto documents 5 s for Augment where SetPower gets 60 s,
    /// so the two fallbacks are separate knobs. Tunable via
    /// `(set-default-augment-lifetime-ms N)`. Like
    /// `default_request_lifetime`, this default isn't clamped to the
    /// RPC's `[AUGMENT_LIFETIME_MIN_S, REQUEST_LIFETIME_MAX_S]`
    /// (5 s, 15 min) range; a config that picks a value outside it is
    /// responsible for the consequences.
    pub default_augment_lifetime: Duration,
}

/// Escape `"` and `\` inside a Lisp string literal, and strip
/// control characters (incl. newlines), so a value can never break
/// out of its quotes in a file we later re-eval. Used by every
/// writer that renders text into Lisp source: the microgrid-file
/// and enterprise-file renderers, and the site-import form
/// renderer.
pub(crate) fn escape_lisp_string(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '"' => vec!['\\', '"'],
            c => vec![c],
        })
        .collect()
}

/// Component categories that carry a `<cat>-defaults` plist in
/// `sim/defaults.lisp`. Walked in this order by `/api/defaults` and
/// by the `enterprise.lisp` renderer, so both see the same set and
/// the file's order is stable. A new category needs an entry here
/// AND a `(setq <cat>-defaults '(…))` block in `sim/defaults.lisp`.
pub(crate) const DEFAULT_CATEGORIES: &[&str] = &[
    "grid",
    "meter",
    "battery",
    "battery-inverter",
    "solar-inverter",
    "ev-charger",
    "steam-boiler",
    // One shared plist for all marker categories (chp, wind turbine,
    // power transformer, breaker).
    "marker",
];

/// Hash of a file's text, as stored in
/// [`Config::written_hashes`]. `DefaultHasher` is not stable across
/// Rust releases — fine here, the map only ever lives inside one
/// running process.
fn content_hash(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    content.hash(&mut h);
    h.finish()
}

/// Renders an f64 so tulisp reads it back as a float (always with a
/// decimal point). Whole numbers of any magnitude get the `.1`
/// form — a bare integer token above i64::MAX would not even parse.
pub(crate) fn lisp_float(v: f64) -> String {
    if v == v.trunc() && v.is_finite() {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// [`lisp_float`] for a value that is natively `f32` — every
/// component's config is. Formatting the `f32` directly gives the
/// shortest decimal that round-trips *as an f32*; widening to f64
/// first would print the f64 nearest to it instead, turning a
/// configured `0.35` into `0.3499999940395355` in the generated
/// block. Reading it back as an f32 yields the same bits either way,
/// but only one of the two is a file a person can stand to read.
pub(crate) fn lisp_float32(v: f32) -> String {
    if v == v.trunc() && v.is_finite() {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            enterprise_id: 0,
            assets_socket_addr: "[::1]:9900".to_string(),
            dispatch_socket_addr: "[::1]:8900".to_string(),
            default_request_lifetime: Duration::from_secs(60),
            default_augment_lifetime: Duration::from_secs(5),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    /// Every file the loader has evaluated — boot scripts, runtime
    /// `(load …)`s, created microgrid files — canonicalized and
    /// dedup'd, in first-load order.
    ///
    /// Two jobs, neither of them "the reload list": it is the set of
    /// files worth WATCHING (a driver script that registers no
    /// microgrid still wants hot-reload), and it carries the ORDER
    /// [`Config::registered_sources`] replays in, since the registry
    /// is a BTreeMap and iterating it yields id order. What a
    /// whole-world reload replays is derived from the registry, not
    /// from here.
    pub(crate) source_files: Arc<Mutex<Vec<PathBuf>>>,
    /// Anchor for everything persistent — `enterprise.lisp`,
    /// `snapshots/`, managed microgrid files — and for the
    /// relative-path resolution of `(load …)` / `(file-exists-p …)`.
    pub(crate) state_dir: PathBuf,
    pub(crate) ctx: SharedMut<TulispContext>,
    pub(crate) site: MicrogridSite,
    pub(crate) metadata: Arc<RwLock<Metadata>>,
    /// Additional files the config has registered via `(watch-file …)`.
    /// `Config::watch` adds each to the live notify watcher so edits to
    /// e.g. `sim/defaults.lisp` trigger the same reload as edits to
    /// the entry-point config. Set semantics — duplicate registrations
    /// (from re-runs of the config during reload) are no-ops.
    pub(crate) extra_watches: Arc<Mutex<HashSet<PathBuf>>>,
    /// Configured display timezone. UI's TZ toggle reads the IANA
    /// name from /api/clock and formats timestamps client-side via
    /// `Intl.DateTimeFormat(..., { timeZone })`. Mutated by
    /// `(set-timezone "…")` in config.lisp; default Europe/Berlin
    /// matches the canonical European-intraday demo target.
    pub(crate) clock: crate::sim::clock::SharedClock,
    /// Multi-stage scenario registry — what `(define-scenario …)`
    /// writes to and what the UI's Scenarios mode + /api/scenarios
    /// read from. See `crate::sim::scenarios` for the data model.
    pub(crate) scenarios: crate::sim::scenarios::SharedScenarios,
    /// Enterprise-scoped microgrid registry — what
    /// `(make-microgrid …)` writes to and what the Microgrids UI
    /// mode + /api/microgrids read from. Empty until the config eval
    /// runs at least one `(make-microgrid …)` form; `Config::new`
    /// errors out if nothing landed in here by the end of eval. See
    /// `crate::sim::microgrids` for the data model.
    pub(crate) microgrids: crate::sim::microgrids::SharedMicrogrids,
    /// Enterprise-wide dispatch store — the single
    /// `MicrogridDispatchService` gRPC server writes here (Create /
    /// Update / Delete from the dispatch CLI), and the per-microgrid
    /// Dispatches UI view + `/api/mg/{id}/dispatches` read from it.
    /// Keyed by `microgrid_id` internally; survives a config reload
    /// (it isn't owned by any `MicrogridSite`). See
    /// `crate::sim::dispatch` for the data model.
    pub(crate) dispatches: crate::sim::dispatch::SharedDispatchStore,
    /// Dynamic site lookup the lisp defuns capture. Resolves to
    /// the current microgrid's site at call time, falling back
    /// to the first registry entry and finally to the bootstrap
    /// site allocated in `Config::new`. See
    /// [`crate::sim::microgrids::SiteRouter`].
    pub(crate) router: SharedSiteRouter,
    /// The microgrid file whose load is in flight, or `None` outside
    /// a load. `load_file` sets it for the duration of the eval so
    /// every `(make-microgrid …)` form the file runs knows which file
    /// it belongs to (and so a second file claiming a taken id is
    /// detectable). Ambient state like `current_microgrid` — only
    /// flipped under the interpreter lock. See
    /// [`crate::sim::microgrids::with_loading`].
    pub(crate) loading: crate::sim::microgrids::LoadingSlot,
    /// Active microgrid id, written by /api/mg/{id}/eval and the
    /// scenario per-microgrid replay. `None` defers to the
    /// router's fallback (first registry entry).
    pub(crate) current_microgrid: CurrentMicrogrid,
    /// Process-wide component-id allocator shared by every
    /// `MicrogridSite` registered through `(make-microgrid …)`,
    /// so auto-allocated component ids stay globally unique
    /// across microgrids. The bootstrap site allocated in
    /// `Config::new` uses the same allocator, so single-site
    /// configs see no behavioural change from the legacy
    /// per-site counter — only the multi-microgrid path gains
    /// cross-site uniqueness.
    pub(crate) enterprise_id_allocator: Arc<std::sync::atomic::AtomicU64>,
    /// Serializes `/api/microgrids/import` requests. The import
    /// handler scans every site for id collisions and only then
    /// evals the components into its new site; no registry lock
    /// spans both steps, so two racing imports could each pass the
    /// scan before the other's components exist — silently breaking
    /// the enterprise-unique component-id invariant. One import at
    /// a time keeps the scan authoritative. A tokio mutex because
    /// the handler holds it across `await`s.
    pub(crate) import_lock: Arc<tokio::sync::Mutex<()>>,
    /// Enterprise-wide notification fired when a new microgrid
    /// lands in `microgrids` — both `(make-microgrid …)` and
    /// `POST /api/microgrids` publish on it. The WS event pump
    /// subscribes so it can spawn a forwarder for the new site's
    /// event bus on the fly, instead of only the entries that
    /// existed at WS-connect time.
    pub(crate) microgrid_registered: Arc<broadcast::Sender<u64>>,
    /// tulisp-async timer handle. The Lisp refresh loop ticks it at
    /// 100 ms cadence to fire `(run-with-timer …)` / `(every …)`
    /// callbacks; `Config::refresh_once` ticks it synchronously for
    /// tests that drive ticks deterministically.
    pub(crate) timer_handle: tulisp_async::Handle,
    /// Source of "now" for scenario time + headless physics stepping.
    /// Wall-clock in a normal (live) `Config`; tied to `sim_clock` in a
    /// headless one. See [`crate::sim::sim_clock`].
    pub(crate) now: crate::sim::sim_clock::NowSource,
    /// Hash of the bytes macrocosim itself last wrote to each file
    /// it manages (managed microgrid files, `enterprise.lisp`). The
    /// file watcher compares an event's file content against this
    /// map and ignores a match, so a save macrocosim performed does
    /// not bounce back as a reload of the file it just wrote.
    pub(crate) written_hashes: Arc<Mutex<HashMap<PathBuf, u64>>>,
    /// Per-microgrid undo / redo stacks over managed microgrid
    /// files. A structural edit pushes the block the file carried
    /// before it; `/api/mg/{id}/undo` pops it back. See
    /// [`crate::lisp::undo`].
    pub(crate) undo: undo::SharedUndo,
    /// Serializes the routes that claim a microgrid id and a port,
    /// then write and load a file: `POST /api/microgrids` (import's
    /// create step included), `/api/load-as`, and a snapshot loaded
    /// as a new microgrid. The load cannot run under the registry
    /// lock (it evaluates lisp), so one claim at a time is what keeps
    /// each route's validation authoritative. A plain `(load …)` or
    /// an eval of `(make-microgrid …)` takes its port as it registers
    /// and does not wait here. A tokio mutex, so a route waits for it
    /// without blocking the runtime and its owned guard can move into
    /// the blocking task.
    pub(crate) create_lock: Arc<tokio::sync::Mutex<()>>,
    /// Present only for a headless `Config` (built by
    /// [`Config::new_headless`]): the hand-advanced clock that drives
    /// the timer queue and `now`. `None` for a live `Config`, whose
    /// timers run on the wall clock and whose background loops tick it.
    pub(crate) sim_clock: Option<Arc<tulisp_async::ManualClock>>,
    /// The check `make-microgrid` asks before adopting a changed
    /// `:grpc-port` on a reload. See
    /// [`crate::sim::microgrids::PortPins`].
    pub(crate) port_pins: crate::sim::microgrids::PortPins,
}

impl Config {
    /// Shared scenarios registry — `(define-scenario …)` writes
    /// here, the UI Scenarios mode + /api/scenarios read here, and
    /// the auto-advance task mutates the per-entry runtime state.
    pub fn scenarios(&self) -> crate::sim::scenarios::SharedScenarios {
        self.scenarios.clone()
    }

    /// Shared enterprise microgrid registry — `(make-microgrid …)`
    /// writes here, the UI Microgrids landing page + /api/microgrids
    /// read here. Always carries at least one entry once
    /// `Config::new` has returned — the hard-error in `Config::new`
    /// rejects configs whose registry is empty after eval.
    /// The import-serialization lock — see the field doc.
    pub(crate) fn import_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.import_lock.clone()
    }

    /// The create-serialization lock — see the field doc.
    pub(crate) fn create_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.create_lock.clone()
    }

    pub fn microgrids(&self) -> crate::sim::microgrids::SharedMicrogrids {
        self.microgrids.clone()
    }

    /// Shared enterprise dispatch store — the `MicrogridDispatchService`
    /// gRPC server mutates it and the UI's per-microgrid Dispatches
    /// view reads it. See [`crate::sim::dispatch`].
    pub fn dispatches(&self) -> crate::sim::dispatch::SharedDispatchStore {
        self.dispatches.clone()
    }

    /// Shared process-wide id allocator backing every microgrid
    /// in the registry. The `POST /api/microgrids` endpoint
    /// clones this into a fresh `MicrogridSite::with_id_allocator`
    /// so runtime-created microgrids participate in the same
    /// globally-unique component-id space as boot-time ones.
    pub fn enterprise_id_allocator(&self) -> Arc<std::sync::atomic::AtomicU64> {
        self.enterprise_id_allocator.clone()
    }

    pub fn port_pins(&self) -> crate::sim::microgrids::PortPins {
        self.port_pins.clone()
    }

    /// A headless `Config` is stepped by `sim_step` on its own clock.
    pub fn is_headless(&self) -> bool {
        self.sim_clock.is_some()
    }

    /// Subscribe to `microgrid_registered` notifications. The WS
    /// event pump uses this to dynamically subscribe to new
    /// microgrid event buses post-connect.
    pub fn subscribe_microgrid_registered(&self) -> broadcast::Receiver<u64> {
        self.microgrid_registered.subscribe()
    }

    /// Mutable handle on the active microgrid id. Per-microgrid
    /// HTTP routes (`/api/mg/{id}/eval` and friends) flip this via
    /// `with_microgrid` so the lisp defuns + the override-file
    /// path resolve to the URL's microgrid.
    pub fn current_microgrid_handle(&self) -> CurrentMicrogrid {
        self.current_microgrid.clone()
    }

    /// Configured display-zone clock handle. The live scenario runner
    /// (todo §J2) reads it to resolve absolute (`HH:MM`) cue times in
    /// the configured timezone.
    pub fn clock_handle(&self) -> crate::sim::clock::SharedClock {
        self.clock.clone()
    }

    /// Clone of the tulisp interpreter handle. Exposed so the scenario
    /// runner (todo §J2) can funcall scenario section forms from
    /// outside `lisp::Config::eval`; everything else inside the
    /// crate should reach for `eval` / `eval_silent` instead.
    pub fn interpreter(&self) -> SharedMut<TulispContext> {
        self.ctx.clone()
    }

    pub fn metadata(&self) -> Metadata {
        self.metadata.read().clone()
    }

    pub fn assets_socket_addr(&self) -> String {
        self.metadata.read().assets_socket_addr.clone()
    }

    pub fn dispatch_socket_addr(&self) -> String {
        self.metadata.read().dispatch_socket_addr.clone()
    }

    /// The ports of the enterprise's assets and dispatch sockets; an
    /// address that does not parse contributes nothing.
    pub fn reserved_ports(&self) -> Vec<u16> {
        reserved_ports_of(&self.metadata.read())
    }

    /// Resolve the site the ambient scope points at. Follows
    /// `current_microgrid`, whose contract requires the interpreter
    /// lock — callers outside an eval (HTTP handlers) must use
    /// [`Config::legacy_site`] instead.
    pub fn site(&self) -> MicrogridSite {
        self.router.site()
    }

    /// Deterministic scope-free resolution for the unscoped legacy
    /// HTTP endpoints: the first registered microgrid (min id), else
    /// the bootstrap site — the same default the typed control
    /// endpoints and the Python client's gRPC reads use. Never
    /// consults `current_microgrid`, so it cannot race a concurrent
    /// `/api/mg/{id}/eval` holding the scope flipped.
    pub fn legacy_site(&self) -> MicrogridSite {
        self.router.first_site()
    }

    /// The bootstrap site legacy single-site configs run on — see
    /// [`crate::sim::microgrids::SiteRouter::bootstrap_site`].
    pub fn bootstrap_site(&self) -> MicrogridSite {
        self.router.bootstrap_site()
    }

    /// IANA name of the configured display zone (default
    /// "Europe/Berlin"; redirected by `(set-timezone "…")`). The
    /// UI's TZ toggle reads this from /api/clock + formats
    /// timestamps via Intl.DateTimeFormat without round-tripping
    /// through Rust.
    pub fn tz_name(&self) -> &'static str {
        self.clock.read().tz_name()
    }

    /// The persistence anchor — see the field doc on `state_dir`.
    pub fn state_dir(&self) -> &std::path::Path {
        &self.state_dir
    }

    /// Directory holding the managed per-microgrid `<id>.lisp`
    /// files, under the state dir. The HTTP create endpoint writes
    /// one here per microgrid it mints; restoring one after a restart
    /// is a manual `(load …)` / CLI argument — nothing scans this
    /// directory.
    pub fn microgrids_dir(&self) -> PathBuf {
        self.state_dir.join("microgrids")
    }

    /// The enterprise-wide state file: enterprise id, timezone,
    /// request lifetime, both socket addresses and every `*-defaults`
    /// plist. Evaluated at boot before any microgrid file and
    /// regenerated by [`Config::persist_enterprise`].
    pub fn enterprise_path(&self) -> PathBuf {
        self.state_dir.join("enterprise.lisp")
    }

    /// Remember that macrocosim wrote `content` to `path`, so the
    /// file watcher can tell its own save apart from a human edit.
    /// Keyed by the canonical path, which is what the watcher asks
    /// with.
    pub(crate) fn record_self_write(&self, path: &Path, content: &str) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.written_hashes
            .lock()
            .insert(path, content_hash(content));
    }

    /// Was `content` exactly what macrocosim last wrote to `path`?
    /// If so, FORGET that write and answer true. `path` must be
    /// canonical, the key [`Self::record_self_write`] stores.
    ///
    /// The file watcher asks this before reloading, so a save
    /// macrocosim performed does not bounce back as a reload. The
    /// answer is deliberately one-shot: a remembered hash that
    /// stayed remembered would go on suppressing forever, so an
    /// operator reverting the file to exactly that content by hand
    /// (an editor undo, a `git checkout --`) would be ignored and
    /// the world would stay on whatever it had drifted to. Forgetting
    /// on the first match costs at most one extra reload of content
    /// identical to what is already live — a no-op — and never a
    /// missed edit.
    pub(crate) fn take_self_write(&self, path: &Path, content: &str) -> bool {
        let mut hashes = self.written_hashes.lock();
        if hashes.get(path) == Some(&content_hash(content)) {
            hashes.remove(path);
            return true;
        }
        false
    }

    /// Remember that the loader evaluated `path`, keeping the order
    /// in which files first arrived. Called by the loader once it has
    /// STARTED evaluating a file — on success, and on a failure too,
    /// since a partly-evaluated file may have registered a microgrid
    /// or armed timers and is then part of the world. Whether the
    /// file registered a microgrid doesn't matter either: a driver
    /// script that only arms timers is worth watching and worth
    /// re-evaluating on its own. A file that never evaluated (missing,
    /// unreadable) is not recorded.
    /// See [`Config::source_files`].
    pub(crate) fn note_source_file(&self, path: &Path) {
        let mut files = self.source_files.lock();
        if !files.iter().any(|p| p == path) {
            files.push(path.to_path_buf());
        }
    }

    /// Drop `path` from the loader's record — for a file that is
    /// being deleted, so the watch set and the reload list stop
    /// naming a path that no longer exists.
    pub(crate) fn forget_source_file(&self, path: &Path) {
        // Canonicalized the way `note_source_file`'s caller recorded
        // it, which is why this runs while the file still exists.
        let canonical = path.canonicalize();
        let canonical = canonical.as_deref().unwrap_or(path);
        self.source_files.lock().retain(|p| p != canonical);
    }

    /// Every file the loader has evaluated, in first-load order.
    /// This is the *watch* set (plus `enterprise.lisp` and the
    /// `(watch-file …)` registrations) and the set of paths a single
    /// file edit may be reloaded on its own — not the whole-world
    /// reload list, which is [`Config::registered_sources`].
    pub(crate) fn loader_visited_files(&self) -> Vec<PathBuf> {
        self.source_files.lock().clone()
    }

    /// The ids of the microgrids `path` backs, ascending. Resolved
    /// and canonicalized the way the loader resolves it, so a
    /// state-dir-relative spelling (what the load endpoint receives)
    /// matches the absolute one the registry stores.
    pub fn microgrids_backed_by(&self, path: &Path) -> Vec<u64> {
        let resolved = self.resolve_in_state_dir(path);
        let canonical = resolved.canonicalize().unwrap_or(resolved);
        self.microgrids
            .lock()
            .iter()
            .filter(|(_, e)| e.source.as_deref() == Some(canonical.as_path()))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Every file that backs at least one registered microgrid, each
    /// listed once, in the order the files first arrived.
    ///
    /// This IS the whole-world reload list: the world is the set of
    /// live microgrids, and each microgrid names the file it came
    /// from, so there is no replay list to drift out of sync with the
    /// registry. A file whose microgrids are all gone drops out by
    /// itself; a microgrid typed into the REPL has no source and
    /// contributes nothing to reload. A driver-only script is not
    /// here either — it is re-run when its own file changes, not
    /// when some other file does.
    pub(crate) fn registered_sources(&self) -> Vec<PathBuf> {
        let live: Vec<PathBuf> = self
            .microgrids
            .lock()
            .values()
            .filter_map(|e| e.source.clone())
            .collect();
        let order = self.source_files.lock();
        let mut out: Vec<PathBuf> = order.iter().filter(|p| live.contains(p)).cloned().collect();
        // A source the loader never saw — the create endpoint writes
        // a file and registers its microgrid in one step, without
        // going through `load_file` — has no recorded order; append
        // it after the ones that do, in registry (id) order.
        for path in live {
            if !out.contains(&path) {
                out.push(path);
            }
        }
        out
    }
}
