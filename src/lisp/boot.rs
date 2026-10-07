//! `Config` bootstrap and lifecycle: build the interpreter, eval the
//! config file, spawn the long-lived loops (Lisp refresh + scenario
//! auto-advance), and the hot-reload + tags-pass entry points.
//!
//! Everything in this file is an `impl Config { ... }` (or a free
//! helper Config relies on) so that the heavy bootstrap logic
//! doesn't sit in the parent module alongside the trivial getters.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, Watcher};
use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;
use tulisp::{Error, SharedMut, TulispContext, TulispObject};

use crate::sim::MicrogridSite;
use crate::sim::microgrids::SiteRouter;

use super::{Config, Metadata, defuns};

/// Why a [`Config::load_file`] call failed.
///
/// One case is singled out because callers act on it rather than
/// just reporting it: a file whose `(make-microgrid …)` claims an id
/// some OTHER file already loaded. The load endpoint turns that into
/// a "load it under a free id instead?" offer, so it needs the id as
/// a number — not a sentence to grep for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The file declares microgrid `id`, which is already live from
    /// somewhere else. [`Config::load_as`] is the way in.
    Collision { id: u64 },
    /// Anything else: unreadable file, malformed markers, a lisp
    /// error in the file. The string is the formatted diagnostic.
    Other(String),
}

impl LoadError {
    /// Sort a formatted lisp error into a variant. The collision is
    /// raised deep inside the `(make-microgrid …)` defun, so the only
    /// thing that crosses back out is its message — this is the ONE
    /// place that reads it, and everything above gets the id typed.
    fn classify(formatted: String) -> Self {
        match collision_id_in(&formatted) {
            Some(id) => LoadError::Collision { id },
            None => LoadError::Other(formatted),
        }
    }
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Collision { id } => write!(f, "microgrid {id} is already loaded"),
            LoadError::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for LoadError {}

/// Why a [`Config::load_as`] call failed.
///
/// One case is singled out for the same reason as
/// [`LoadError::Collision`]: callers act on it rather than just
/// reporting it. A copy whose generated block registered and whose
/// script section then failed leaves a LIVE microgrid and a file on
/// disk — the caller got what it asked for plus a problem, and must
/// not retry. Only `load_as` can tell that apart from a call that
/// copied nothing, because only it knows whether it got as far as
/// writing and loading the target; the several early refusals
/// ("already registered", "already exists") return before anything
/// is copied, and a caller inspecting the registry afterwards would
/// see the file the PREVIOUS, successful call left and mistake one
/// for the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadAsError {
    /// The copy was written and its microgrid `id` registered, but
    /// the load then failed — `cause` says how. The file is kept.
    CommittedPartial { id: u64, cause: String },
    /// Anything else. Nothing from this call is live: either nothing
    /// was copied, or the copy was cleaned up again.
    Other(String),
}

impl std::fmt::Display for LoadAsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadAsError::CommittedPartial { cause, .. } => {
                write!(f, "{cause} — the copy loaded but its script section failed")
            }
            LoadAsError::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for LoadAsError {}

/// Every refusal that isn't the committed partial arrives as a plain
/// message, so `?` can carry one straight out of `load_as`.
impl From<String> for LoadAsError {
    fn from(msg: String) -> Self {
        LoadAsError::Other(msg)
    }
}

/// The microgrid id named by a "microgrid N is already loaded (…)"
/// message anywhere inside `msg` — the number immediately before the
/// phrase. `None` when the message says something else.
fn collision_id_in(msg: &str) -> Option<u64> {
    const PHRASE: &str = "is already loaded";
    let mut rest = msg;
    while let Some(at) = rest.find(PHRASE) {
        let digits: String = rest[..at]
            .trim_end()
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits.chars().rev().collect::<String>().parse().ok();
        }
        rest = &rest[at + PHRASE.len()..];
    }
    None
}

impl Config {
    /// Build a config from one script file — the single-script
    /// convenience for embedders and tests. The state dir is the
    /// script's directory, which keeps `enterprise.lisp` /
    /// snapshots / managed microgrid files next to the script — the entry-config
    /// contract this constructor has always had. (The binary's CLI
    /// goes through [`Config::new_with`], whose default is the cwd.)
    /// Returns the formatted lisp error on parse / eval failure —
    /// caller decides whether to panic (binary boot) or surface in
    /// the UI (hot reload). On error no background loops have been
    /// spawned and nothing is ticking (the binary spawns each site's
    /// physics only after a successful `new`); sites the failed eval
    /// already registered are dropped with the registry Arcs.
    pub fn new(filename: &str) -> Result<Self, String> {
        let scripts = vec![filename.to_string()];
        let state_dir = script_parent_dir(filename);
        Self::new_inner(scripts, state_dir, false)
    }

    /// Build a config from zero or more boot scripts. With none, the
    /// engine boots bare: empty registry, full DSL live via the
    /// embedded prelude — topologies then arrive at runtime through
    /// `(load …)` / the REPL / the HTTP create + import endpoints.
    ///
    /// `state_dir` anchors everything persistent (`enterprise.lisp`,
    /// `snapshots/`, managed microgrid files) and the
    /// relative-path resolution of `(load …)` / `(file-exists-p …)`.
    /// `None` falls back to the process cwd — one anchor regardless
    /// of where the scripts live, so a `(load …)` typed into the
    /// REPL resolves the same whether the world came from a boot
    /// script or a runtime load.
    pub fn new_with(scripts: &[String], state_dir: Option<PathBuf>) -> Result<Self, String> {
        let state_dir = state_dir.unwrap_or_else(|| PathBuf::from("."));
        Self::new_inner(scripts.to_vec(), state_dir, false)
    }

    /// Build a *headless* `Config` for deterministic, faster-than-real-
    /// time scenario runs. Timers and scenario time run on a hand-
    /// advanced [`ManualClock`](tulisp_async::ManualClock) (returned
    /// alongside), and the background loops (physics, lisp refresh,
    /// scenario auto-advance, the frequency driver) are NOT spawned —
    /// the caller steps the simulation itself via
    /// [`Config::sim_step`]. Used for CI scenario assertions.
    pub fn new_headless(filename: &str) -> Result<(Self, Arc<tulisp_async::ManualClock>), String> {
        let scripts = vec![filename.to_string()];
        let state_dir = script_parent_dir(filename);
        let cfg = Self::new_inner(scripts, state_dir, true)?;
        // A stepped run has no UI and no REPL — nothing can load a
        // topology later, so an empty registry here is a dead run,
        // not a bare boot waiting for input.
        if cfg.microgrids.lock().is_empty() {
            return Err("config loaded but no (make-microgrid …) form ran — \
                 a headless run needs its config to register a microgrid"
                .to_string());
        }
        let clock = cfg
            .sim_clock
            .clone()
            .expect("headless Config always has a sim clock");
        Ok((cfg, clock))
    }

    fn new_inner(scripts: Vec<String>, state_dir: PathBuf, headless: bool) -> Result<Self, String> {
        use std::sync::atomic::AtomicU64;
        let mut ctx = TulispContext::new();
        let enterprise_id_allocator =
            Arc::new(AtomicU64::new(crate::sim::component::FIRST_AUTO_ID));
        let site = MicrogridSite::with_id_allocator(enterprise_id_allocator.clone());
        let metadata = Arc::new(RwLock::new(Metadata::default()));
        let extra_watches = Arc::new(Mutex::new(HashSet::new()));
        let clock = crate::sim::clock::new_clock();
        let scenarios = crate::sim::scenarios::new_registry();
        let microgrids = crate::sim::microgrids::new_registry();
        let dispatches = crate::sim::dispatch::new_store();
        let current_microgrid = crate::sim::microgrids::new_current_microgrid();
        let loading = crate::sim::microgrids::new_loading_slot();
        let router = SiteRouter::new(microgrids.clone(), current_microgrid.clone(), site.clone());
        // Capacity = 1024 to absorb a mass-create burst (e.g. a
        // script POST'ing /api/microgrids a few hundred times
        // back-to-back) without lagging the WS event pump's
        // receiver. Even on Lagged the pump re-snapshots the
        // registry and back-fills forwarders, so capacity tuning is
        // belt-and-suspenders — but a fresh subscriber spinning up
        // mid-burst still benefits from the extra slack.
        let microgrid_registered = Arc::new(broadcast::channel(1024).0);
        let port_pins = crate::sim::microgrids::new_port_pins();
        // Enterprise-wide grid frequency state — one OU process drives
        // every MicrogridSite in the registry so they share the
        // physically-correct same frequency. The driver task is
        // spawned below; bootstrap site + future make-microgrid forms
        // both attach to this slot.
        let grid_frequency = crate::sim::frequency::new_shared();
        site.set_grid_frequency(grid_frequency.clone());
        // The wall-clock background loops (frequency driver, lisp
        // refresh) are spawned only after the config evals
        // successfully, below — a failed `Config::new` must not leave
        // orphan loops ticking. A headless run never spawns them: it
        // drives every tick itself, and the loops would race the
        // stepped driver.

        // tulisp canonicalizes the load path, which requires the
        // directory to exist — create it up front so a fresh
        // `--state-dir` fails with a clear message (every other
        // consumer create_dir_all's lazily anyway).
        std::fs::create_dir_all(&state_dir)
            .map_err(|e| format!("state dir {} cannot be created: {e}", state_dir.display()))?;
        ctx.set_load_path(Some(&state_dir))
            .map_err(|e| format!("set_load_path({}): {e}", state_dir.display()))?;

        // Headless builds run the timer queue + scenario time on a
        // hand-advanced ManualClock so a scenario can be stepped
        // deterministically and faster than real time; live builds use
        // the wall clock and the background loops below.
        let sim_clock = if headless {
            Some(Arc::new(tulisp_async::ManualClock::new()))
        } else {
            None
        };
        let now = match &sim_clock {
            Some(clock) => crate::sim::sim_clock::NowSource::sim(
                crate::sim::sim_clock::headless_base(),
                clock.clone(),
            ),
            None => crate::sim::sim_clock::NowSource::wall(),
        };
        site.set_now_source(now.clone());

        defuns::register_runtime(
            &mut ctx,
            router.clone(),
            metadata.clone(),
            state_dir.clone(),
            microgrids.clone(),
            now.clone(),
        );
        defuns::register_clock(&mut ctx, clock.clone());
        defuns::register_watches(&mut ctx, state_dir.clone(), extra_watches.clone());
        defuns::register_scenarios(&mut ctx, scenarios.clone());
        defuns::register_microgrids(
            &mut ctx,
            microgrids.clone(),
            current_microgrid.clone(),
            enterprise_id_allocator.clone(),
            microgrid_registered.clone(),
            grid_frequency.clone(),
            loading.clone(),
            metadata.clone(),
            port_pins.clone(),
            now.clone(),
        );
        defuns::register_frequency(&mut ctx, grid_frequency.clone());
        crate::lisp::renames::register(&mut ctx);

        // tulisp-async gives the config DSL access to run-with-timer,
        // cancel-timer, sleep-for and friends, used to drive
        // *environment* animation (per-tick voltage / frequency
        // perturbations, scheduled events). Component logic stays in
        // Rust; lisp's only job is wiring + scripting the site
        // around it. Must be called inside a tokio runtime —
        // TokioExecutor::new captures Handle::current().
        //
        // The returned `Handle` is what the dedicated
        // `spawn_lisp_refresh_loop` task ticks at 100 ms cadence
        // to fire pending timer firings. Without it the mailbox
        // would just accumulate.
        let executor = Arc::new(tulisp_async::TokioExecutor::new());
        let timer_handle = match &sim_clock {
            Some(clock) => tulisp_async::register_with_clock(&mut ctx, executor, clock.clone()),
            None => tulisp_async::register(&mut ctx, executor),
        };
        // A timer body's error goes to the log, which the UI shows.
        timer_handle.set_body_error_handler(|_, e| log::error!("run-with-timer: {e}"));

        // Embedded scenario DSL prelude. The vocabulary a config needs —
        // `at` / `check` / `controller` / `drive-*` / `timeline` /
        // `define-controller`, the `make-*` defaults, and the `scenario--run`
        // runner — lives in three lisp files that config.lisp `(load …)`s
        // today; bake them in so a binary shipping no lisp still has them.
        // Loaded after the Rust defuns + tulisp-async (so `every` /
        // `run-with-timer` exist) and before the user config, so its
        // `(define-scenario …)` / `(make-* …)` forms resolve. Order matters:
        // common before scenarios (the runner calls `define-controller`).
        // Additive — a config that still `(load …)`s these files just
        // redefines them on top.
        //
        // TODO: once tulisp supports embedding files (an eval-with-filename
        // path), evaluate each file under its real path so a prelude error
        // cites the real source file rather than the shared `<eval_string>`
        // bucket (todo §D9).
        for src in [
            include_str!("../../sim/common.lisp"),
            include_str!("../../sim/defaults.lisp"),
            include_str!("../../sim/scenarios.lisp"),
        ] {
            if let Err(e) = ctx.eval_string(src) {
                return Err(format!("embedded prelude failed to load: {e}"));
            }
        }

        let ctx = SharedMut::new(ctx);

        // The Config exists BEFORE the boot scripts run: each script
        // is evaluated through `Config::load_file`, which needs a
        // built `self` to set the ambient loading-file slot, record
        // the file for reload replay, and attribute every
        // `(make-microgrid …)` form to the file it came from. The
        // background loops are still spawned only further down, after
        // a fully successful eval — an error path returns here with
        // nothing ticking, exactly as before.
        let cfg = Self {
            // Filled in by `load_file` below, one canonicalized entry
            // per source file, so the same file spelled differently
            // (relative argv vs an absolute runtime `(load …)`) counts
            // once.
            source_files: Arc::new(Mutex::new(Vec::new())),
            state_dir,
            ctx,
            site,
            metadata,
            extra_watches,
            clock,
            scenarios,
            microgrids: microgrids.clone(),
            dispatches,
            router,
            loading,
            current_microgrid,
            enterprise_id_allocator,
            import_lock: Arc::new(tokio::sync::Mutex::new(())),
            create_lock: Arc::new(tokio::sync::Mutex::new(())),
            undo: super::undo::new_undo_histories(),
            microgrid_registered,
            written_hashes: Arc::new(Mutex::new(std::collections::HashMap::new())),
            timer_handle: timer_handle.clone(),
            now,
            sim_clock,
            port_pins,
        };

        // Enterprise-wide state (enterprise id, timezone, socket
        // addresses, the `*-defaults` plists) before any microgrid
        // file: a microgrid loaded next builds its components with
        // these defaults in place.
        cfg.load_enterprise()?;

        for script in &scripts {
            // argv paths are relative to the process cwd, not to the
            // state dir (which `load_file` resolves against), so
            // absolutize here — `load_file` reports a clean read error
            // for a script that doesn't exist.
            let path = PathBuf::from(script);
            let path = path.canonicalize().unwrap_or(path);
            cfg.load_file(&path).map_err(|e| e.to_string())?;
        }
        warn_orphaned_chp_defaults(&mut cfg.ctx.borrow_mut());

        // An empty registry is a legitimate live state: a bare boot
        // (no scripts) serves the UI + REPL and topologies arrive on
        // demand via `(load …)` or the create/import endpoints. Warn
        // when scripts WERE given and registered nothing — that is
        // almost certainly a config mistake, but with the UI up the
        // user can still recover interactively. The headless path
        // hard-errors instead (see `new_headless`).
        if microgrids.lock().is_empty() {
            if scripts.is_empty() {
                log::info!(
                    "bare boot: no microgrids registered — load a script from \
                     the UI or REPL, e.g. (load \"examples/berlin-demo.lisp\")"
                );
            } else {
                log::warn!(
                    "boot scripts registered no microgrids — no gRPC servers \
                     will be up until a (make-microgrid …) form runs"
                );
            }
        }

        // Validate every registered site, not the (always-empty)
        // bootstrap: components live in the per-mg sites created by
        // `(make-microgrid …)`. The UI reads validation per request
        // via /api/mg/{mg}/topology; these calls exist for the boot
        // log.
        for (id, entry) in microgrids.lock().iter() {
            log_topology_validation(&entry.site, &format!("boot (microgrid {id})"));
        }

        // Wall-clock background loops, spawned only now that the
        // config evaluated: an eval error returns above, and loops
        // spawned earlier would keep ticking (and keep the registry
        // Arcs alive) with no handle to stop them.
        if !headless {
            crate::sim::frequency::spawn_driver(grid_frequency.clone());
        }

        // Lisp refresh loop. One tokio task at 100 ms cadence holds
        // the interpreter lock once per pass, walks every registered
        // microgrid's components calling `refresh_inputs` (which
        // re-resolves any lambda-bound `:power-w` / `:sunlight-pct` / …
        // into `DynamicScalar`'s atomic), and drains the
        // tulisp-async timer mailbox so `(every …)` / `(run-with-
        // timer …)` callbacks fire.
        //
        // Decoupling this from the per-site physics tick means:
        //  - Physics ticks are lock-free; a long-running /api/eval
        //    no longer stalls every microgrid's per-second physics.
        //  - The refresh ticks at its own cadence (100 ms by
        //    default), so lambda-bound inputs lag at most one
        //    refresh interval behind their underlying lisp source.
        //    For a 15-min sine curve that's a 0.005% phase shift —
        //    negligible.
        //  - `Config::refresh_once` exposes the same work
        //    synchronously for tests that drive `tick_once` and
        //    expect the lambda result to be visible immediately.
        if !headless {
            Self::spawn_lisp_refresh_loop(microgrids, cfg.ctx.clone(), timer_handle);
        }
        // The scenario runners (todo §J2) replace the old day-stage
        // auto-advance task; until they land, registered scenarios are
        // introspectable but not yet runnable from the registry.

        Ok(cfg)
    }

    /// Evaluate `enterprise.lisp`, creating an empty one first when
    /// the state dir doesn't have it yet. Enterprise-wide state is
    /// not a microgrid file: it registers nothing, so it goes
    /// straight through the interpreter rather than through
    /// `load_file`.
    ///
    /// A file that cannot be read, written or evaluated is a boot
    /// error — every microgrid loaded afterwards would silently get
    /// the built-in defaults instead of the operator's.
    pub(super) fn load_enterprise(&self) -> Result<(), String> {
        let mut ctx = self.ctx.borrow_mut();
        self.load_enterprise_locked(&mut ctx)
    }

    /// [`load_enterprise`](Self::load_enterprise) against an
    /// already-held interpreter guard — `reload` re-evals the
    /// enterprise file inside its own locked section, and
    /// re-borrowing there would deadlock.
    pub(super) fn load_enterprise_locked(&self, ctx: &mut TulispContext) -> Result<(), String> {
        let path = self.enterprise_path();
        if !path.exists() {
            let text = super::microgrid_file::compose(
                "",
                super::microgrid_file::FRESH_ENTERPRISE_SCRIPT_HEADER,
            );
            super::microgrid_file::write_atomic(&path, &text)
                .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            self.record_self_write(&path, &text);
        }
        match ctx.eval_file(&path.to_string_lossy()) {
            Ok(_) => Ok(()),
            Err(e) => {
                let formatted = e.to_string();
                log::error!("Tulisp error in {}:\n{formatted}", path.display());
                Err(formatted)
            }
        }
    }

    /// Load one microgrid file: parse it (managed files carry a
    /// macrocosim-generated block; hand-written ones don't), evaluate
    /// it with the file recorded as the ambient loading file, and
    /// return the ids of the microgrids it newly registered.
    ///
    /// Relative paths resolve against `state_dir` — the same anchor
    /// `(load …)` and `(file-exists-p …)` use — so a path typed into
    /// the REPL means the same thing as one written in a config.
    ///
    /// Loading a file that is already loaded IS the reload path — the
    /// load picker lists `microgrids/`, whose files are typically
    /// live already. It runs the full per-file reload (this file's
    /// timers cancelled, its microgrids' sites reset, then the
    /// re-eval), so a second load can't stack a second copy of every
    /// `every` block. Its `(make-microgrid …)` forms reuse their
    /// entries in place, so live runtimes keep their site handles and
    /// the returned id list is empty — nothing is *new*.
    pub fn load_file(&self, path: &Path) -> Result<Vec<u64>, LoadError> {
        let mut ctx = self.ctx.borrow_mut();
        self.load_file_locked(&mut ctx, path)
    }

    /// [`load_file`](Self::load_file) against an already-held
    /// interpreter guard — for callers that must not re-borrow, like
    /// a `(load …)` routed out of `eval_locked`.
    pub(super) fn load_file_locked(
        &self,
        ctx: &mut TulispContext,
        path: &Path,
    ) -> Result<Vec<u64>, LoadError> {
        let resolved = self.resolve_in_state_dir(path);
        let canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
        // Already visited by the loader? Then this is a re-load, and
        // re-evaluating it on top of itself would double-arm its
        // timers. Go the whole reload way instead.
        if self.source_files.lock().contains(&canonical) {
            return self.reload_visited_locked(ctx, &canonical);
        }
        self.eval_source_file(ctx, &resolved, &canonical)
    }

    /// Resolve `path` the way every loader entry point does: absolute
    /// paths pass through, relative ones join `state_dir` — the same
    /// anchor `(load …)` and `(file-exists-p …)` use, so a path typed
    /// into the REPL means what one written in a config means.
    ///
    /// One place for the rule, because every entry point that takes a
    /// path from a person (load, load-as, the registry lookup behind
    /// the load endpoint, the collision 409's managed-file probe) has
    /// to agree on what a relative path means.
    pub(crate) fn resolve_in_state_dir(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.state_dir.join(path)
        }
    }

    /// Evaluate one source file with itself recorded as the ambient
    /// loading file, record it as visited, and return the ids of the
    /// microgrids it *newly* registered.
    ///
    /// The fresh-load body, with no reload preamble: callers that
    /// need one (a re-load, a whole-world replay) do their cancelling
    /// and resetting first and then come here.
    fn eval_source_file(
        &self,
        ctx: &mut TulispContext,
        resolved: &Path,
        canonical: &Path,
    ) -> Result<Vec<u64>, LoadError> {
        use crate::sim::microgrids::{LoadingFile, with_loading};

        let text = std::fs::read_to_string(resolved)
            .map_err(|e| LoadError::Other(format!("cannot read {}: {e}", resolved.display())))?;
        // A managed file's structure is macrocosim's to rewrite, an
        // unmanaged one's is the author's — and the split also
        // decides how the file is evaluated (see below).
        let parsed = super::microgrid_file::parse(&text)
            .map_err(|e| LoadError::Other(format!("{}: {e}", resolved.display())))?;
        let managed = parsed.generated.is_some();

        let before: HashSet<u64> = self.microgrids.lock().keys().copied().collect();
        // The canonical spelling is what the registry stores in
        // `source`, so every spelling of one file compares equal —
        // that comparison is what distinguishes a reload of this file
        // from a second file claiming its ids.
        let file = LoadingFile {
            path: canonical.to_path_buf(),
            managed,
        };
        let result = with_loading(&self.loading, file, || match &parsed.generated {
            // Managed file: the two sections are evaluated
            // separately, because they run in different scopes. The
            // generated block registers the microgrid (its
            // `:topology` lambda is scoped by `make-microgrid`
            // itself); the script section is promised to run "in this
            // microgrid's scope", which only holds if we put it
            // there — a top-level form otherwise resolves through the
            // router's fallback, i.e. the LOWEST registered id.
            //
            // Cost of the split: tulisp reports positions inside the
            // script section against an eval-string bucket rather
            // than the file. The load path is unaffected — `(load …)`
            // resolves against the state dir globally
            // (`set_load_path`), not against the evaluating file.
            Some(generated) => {
                ctx.eval_string(generated)?;
                if parsed.script.trim().is_empty() {
                    return Ok(TulispObject::nil());
                }
                match self.block_microgrid_id(generated, &before) {
                    Some(id) => {
                        crate::sim::microgrids::with_microgrid(&self.current_microgrid, id, || {
                            ctx.eval_string(&parsed.script)
                        })
                    }
                    // A block that declared no microgrid we can name
                    // (a hand-mangled head) leaves the script where
                    // an unmanaged file's would run.
                    None => ctx.eval_string(&parsed.script),
                }
            }
            // Unmanaged file: one script, one scope, one eval.
            None => ctx.eval_file(&resolved.to_string_lossy()),
        });
        if let Err(e) = result {
            let formatted = e.to_string();
            log::error!("Tulisp error in {}:\n{formatted}", resolved.display());
            // Evaluation STARTED, so the file is part of the world
            // whatever went wrong — whether the failure was in a
            // generated block (whose `make-microgrid` may already have
            // registered before a later form died), in the script
            // section after the block registered, or partway through
            // an unmanaged script that had armed its timers. Record it
            // even though the load reports an error: that routes every
            // retry through the reload path, which cancels this file's
            // timers first instead of double-arming them, and puts the
            // file under the watcher so saving a fix reloads it.
            //
            // A file that never got this far — unreadable, unparseable
            // path — ran no form and is recorded nowhere; those return
            // before this point.
            self.note_source_file(canonical);
            return Err(LoadError::classify(formatted));
        }
        // A file that evaluated is worth watching and worth
        // re-evaluating on its own even if it registered no
        // microgrid — a driver script that only arms `every` blocks
        // is exactly that case.
        self.note_source_file(canonical);

        let new_ids: Vec<u64> = self
            .microgrids
            .lock()
            .iter()
            .filter(|(id, _)| !before.contains(id))
            .map(|(id, entry)| {
                // Tell UI subscribers the site is populated — the
                // topology arrived after the WS pump's snapshot.
                entry.site.bump_version();
                *id
            })
            .collect();
        Ok(new_ids)
    }

    /// Which microgrid a just-evaluated generated `block` belongs to,
    /// given the registry key set from before its eval.
    ///
    /// A generated block holds exactly one `(make-microgrid …)`, so a
    /// first load leaves exactly one new key and the diff names it —
    /// including when the head auto-allocated its id. A same-file
    /// reload reuses its entry in place and moves no key, so the diff
    /// is empty and the head's own `:id` is read instead.
    fn block_microgrid_id(&self, block: &str, before: &HashSet<u64>) -> Option<u64> {
        let new_ids: Vec<u64> = {
            let reg = self.microgrids.lock();
            reg.keys()
                .filter(|id| !before.contains(id))
                .copied()
                .collect()
        };
        match new_ids[..] {
            [id] => Some(id),
            // No new entry (a reload) — or, for a hand-edited block
            // with several heads, no single answer: fall back to what
            // the block itself declares.
            _ => super::microgrid_file::head_id(block),
        }
    }

    /// Copy a managed microgrid file to `microgrids/{new_id}.lisp`
    /// under the state dir with its microgrid id rewritten to
    /// `new_id`, then load the copy. Returns `new_id`.
    ///
    /// This is how one file becomes two live microgrids ("load
    /// another copy of this site"). Unmanaged files are refused:
    /// without a generated block there is no id to rewrite
    /// mechanically, and guessing at hand-written source would be a
    /// silent mangle. Refuses an id that is already registered or
    /// whose target file already exists, so a copy can never clobber
    /// a microgrid that exists.
    ///
    /// The copy also gets its own `:grpc-port` and a fresh `:id` for
    /// every component. A microgrid's port is held by a listening
    /// gRPC server, and component ids are enterprise-unique, so a
    /// copy carrying either of the original's would be refused at
    /// load ("already bound by microgrid N", "component id X is
    /// already registered in microgrid Y") — which is the whole point
    /// of the copy failing for the one case it exists to serve:
    /// duplicating a file that is already loaded.
    pub fn load_as(&self, path: &Path, new_id: u64) -> Result<u64, LoadAsError> {
        let resolved = self.resolve_in_state_dir(path);
        let text = std::fs::read_to_string(&resolved)
            .map_err(|e| format!("cannot read {}: {e}", resolved.display()))?;
        let rewritten = super::microgrid_file::rewrite_id(&text, new_id)
            .map_err(|e| format!("{}: {e}", resolved.display()))?;
        // Id check and port pick under one registry lock, so the port
        // we write is free as of the same instant the id was.
        let reserved = self.reserved_ports();
        let free_port = {
            let reg = self.microgrids.lock();
            if reg.contains_key(&new_id) {
                return Err(LoadAsError::Other(format!(
                    "microgrid {new_id} is already registered"
                )));
            }
            crate::sim::microgrids::next_free_port_in(&reg, &reserved)
        };
        // A head with no `:grpc-port` comes back unchanged — the
        // loader allocates one for it anyway.
        let rewritten = super::microgrid_file::rewrite_grpc_port(&rewritten, free_port)
            .map_err(|e| format!("{}: {e}", resolved.display()))?;
        // Fresh component ids from the enterprise allocator — the
        // same counter `MicrogridSite::next_id` draws from, so
        // nothing the copy declares can collide with a live
        // component, and `reserve_id` keeps the counter above the
        // explicit ids we just wrote.
        let allocator = self.enterprise_id_allocator.clone();
        let rewritten = super::microgrid_file::remap_component_ids(&rewritten, || {
            allocator.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        })
        .map_err(|e| format!("{}: {e}", resolved.display()))?;
        let target = self.microgrids_dir().join(format!("{new_id}.lisp"));
        if target.exists() {
            return Err(LoadAsError::Other(format!(
                "{} already exists; refusing to clobber",
                target.display()
            )));
        }
        super::microgrid_file::write_atomic(&target, &rewritten)
            .map_err(|e| format!("write {}: {e}", target.display()))?;
        self.record_self_write(&target, &rewritten);
        // Past this point the copy exists, so this is the only place
        // that can tell a committed partial from a call that left
        // nothing — we know we just wrote and loaded THIS target, and
        // the registry answer is about our own load rather than some
        // earlier one's leftovers.
        if let Err(e) = self.load_file(&target) {
            let registered = self.microgrids_backed_by(&target);
            return Err(match registered.first() {
                // The generated block DID register; only the script
                // section failed. The file is where that live
                // microgrid persists, undoes and snapshots to, so
                // removing it would strand it — keep both, and hand
                // the caller a live id alongside the complaint.
                Some(&id) => LoadAsError::CommittedPartial {
                    id,
                    cause: e.to_string(),
                },
                // Nothing registered, so the copy is an orphan the
                // next reload would trip over. Drop it — and take it
                // back out of the visited set, which the failed load
                // just put it in, or the watcher would follow a path
                // that no longer exists. Forget before unlink, while
                // the path can still be canonicalized the same way
                // the loader recorded it.
                None => {
                    self.forget_source_file(&target);
                    let _ = std::fs::remove_file(&target);
                    LoadAsError::Other(e.to_string())
                }
            });
        }
        Ok(new_id)
    }

    /// Trigger one refresh + timer-drain pass synchronously. Mirrors
    /// what the background loop does once per 100 ms, but on the
    /// caller's thread — tests reach for this when they need a
    /// `(run-with-timer 0 …)` fire to be visible before the next
    /// `tick_once`, or a lambda-bound `:power-w` value to resolve
    /// before reading `aggregate_power_w`.
    ///
    /// Acquires the interpreter lock, walks every registered
    /// microgrid's components calling `refresh_inputs`, then drains
    /// the timer mailbox once. Tests that drive `tick_once` directly
    /// call this first so lambda-bound `:power-w` / `:sunlight-pct` /
    /// `(run-with-timer 0 …)` values are visible before the synthetic
    /// physics tick.
    pub fn refresh_once(&self) {
        refresh_pass(&self.microgrids, &self.ctx, &self.timer_handle);
    }

    /// Advance a headless simulation by `dt`: move the sim clock
    /// forward, re-resolve lisp-driven inputs + fire now-due timers
    /// ([`refresh_once`](Self::refresh_once)), then tick physics on
    /// every site at the new sim-time. Deterministic and as fast as the
    /// caller loops it — no real-time sleeping. No-op (logged) on a live
    /// `Config` (one built by [`Config::new`] rather than
    /// [`Config::new_headless`]).
    pub fn sim_step(&self, dt: Duration) {
        let Some(clock) = self.sim_clock.as_ref() else {
            log::warn!("Config::sim_step called on a live Config; ignored");
            return;
        };
        clock.advance(dt);
        // Timers + dynamic inputs first (a `timeline` source reads
        // scenario-elapsed, now at the advanced sim-time), then physics.
        self.refresh_once();
        let now = self.now.now();
        let sites: Vec<MicrogridSite> = self
            .microgrids
            .lock()
            .values()
            .map(|e| e.site.clone())
            .collect();
        for site in sites {
            site.tick_once(now, dt);
        }
    }

    /// Step a headless simulation to `until` sim-time in `dt` increments.
    /// Returns the number of steps taken. No-op on a live `Config`.
    pub fn sim_run(&self, until: Duration, dt: Duration) -> u64 {
        if self.sim_clock.is_none() {
            log::warn!("Config::sim_run called on a live Config; ignored");
            return 0;
        }
        let mut steps = 0;
        let mut elapsed = Duration::ZERO;
        while elapsed < until {
            self.sim_step(dt);
            elapsed += dt;
            steps += 1;
        }
        steps
    }

    /// Run a registered scenario on the headless sim clock, stepping
    /// by `dt` — for `until` if given, otherwise for the scenario's
    /// declared `:length`. Returns the number of steps. The stepped
    /// runner (todo §J2): compiles the scenario via the Lisp
    /// `scenario--run` (so its cue / check timers fire on the sim
    /// clock) then drives `sim_run`, deterministically and faster
    /// than real time, then stops the scenario — cancelling its
    /// agent/cue/check timers and restoring every driven knob to its
    /// pre-scenario state, the same as a live run's `(scenario-stop)`
    /// — before returning, so a caller inspecting component state
    /// right after this returns sees the SAME teardown a live run
    /// would have left behind, not the scenario's final driven value.
    /// Pair with `scenario-expect` + `scenario report --assert` for a
    /// CI gate (those read the frozen report, unaffected by the
    /// knob restore). Errors on a live `Config`, an unknown scenario,
    /// or — only when `until` is None, since an explicit run length
    /// answers the question `:length` would have — one without a
    /// `:length`.
    pub fn run_scenario_stepped(
        &self,
        name: &str,
        dt: Duration,
        until: Option<Duration>,
    ) -> Result<u64, String> {
        if self.sim_clock.is_none() {
            return Err("run_scenario_stepped requires a headless Config".to_string());
        }
        let run_for = match until {
            Some(d) => d,
            None => Duration::from_secs_f64(
                self.scenarios
                    .lock()
                    .get(name)
                    .ok_or_else(|| format!("no scenario named {name:?}"))?
                    .length_s
                    .ok_or_else(|| format!("scenario {name:?} has no :length for a stepped run"))?,
            ),
        };
        crate::sim::scenarios::start(&self.ctx, &self.scenarios, name)?;
        let steps = self.sim_run(run_for, dt);
        // `eval_silent`, not `eval`: a scenario stop is a runtime poke
        // (health flips, power pokes, teardown restores), not a
        // structural change — `eval` would snapshot every registered
        // microgrid's structural version and run the persist / bump-
        // version pass for nothing, same as `set-meter-power` &c.
        // never going through `eval` either.
        self.eval_silent("(scenario-stop)")?;
        Ok(steps)
    }

    /// The current microgrid's scenario report (peak / charge / SoC
    /// stats + the `scenario-expect` pass/fail ledger) at sim/wall now,
    /// serialized to JSON — the same shape
    /// `GET /api/mg/{mg}/scenario/report` returns. Public + JSON so
    /// an out-of-crate stepped runner (`macroctl scenario run
    /// --stepped`) can print + assert on it without a server or
    /// exposing the internal report type.
    pub fn scenario_report_json(&self) -> serde_json::Value {
        serde_json::to_value(self.site().scenario_report(self.now.now())).unwrap_or_default()
    }

    /// Spawn the Lisp refresh + timer-drain loop. Runs at 100 ms
    /// cadence on its own tokio task; sole acquirer of the
    /// interpreter lock for refresh purposes (eval still contends
    /// at the same lock, but only when it's not held by us). See
    /// the comment block in `Config::new` for the design rationale.
    fn spawn_lisp_refresh_loop(
        registry: crate::sim::microgrids::SharedMicrogrids,
        ctx: SharedMut<TulispContext>,
        timer_handle: tulisp_async::Handle,
    ) {
        tokio::spawn(async move {
            // First tick at +100 ms so tests that boot a Config +
            // drive `tick_once` synchronously don't race the loop.
            let start = tokio::time::Instant::now() + Duration::from_millis(100);
            let mut tick = tokio::time::interval_at(start, Duration::from_millis(100));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                // A long /api/eval grabbing the same lock will delay
                // this iteration but won't block physics: each site's
                // `spawn_physics` task ticks lock-free against the
                // atomics last published by `refresh_inputs`.
                refresh_pass(&registry, &ctx, &timer_handle);
            }
        });
    }

    /// Build a TAGS table for every file in `roots` and every
    /// file each transitively `(load …)`s. Drives tulisp's
    /// parse-with-etags path: every `(defun NAME …)` form across
    /// the file tree becomes one entry, and every Rust-side
    /// `ctx.defun("name", …)` call from `register_runtime` /
    /// `tulisp_async::register` adds an entry pointing at the
    /// Rust source location — so `M-.` on `(set-meter-power …)`
    /// or `(run-with-timer …)` jumps straight into the Rust
    /// implementation.
    ///
    /// Static, but must run inside a tokio runtime —
    /// `tulisp_async::TokioExecutor::new` captures
    /// `Handle::current()`. The etags binary wraps `main` with
    /// `#[tokio::main]` for that.
    ///
    /// The load path is set from the first root's parent
    /// directory (the canonical config); roots beyond the first
    /// can `(load …)` files relative to it just like config.lisp
    /// would.
    pub fn tags_table(roots: &[&str]) -> Result<String, Error> {
        let mut ctx = TulispContext::new();
        let site = MicrogridSite::new();
        let metadata = Arc::new(RwLock::new(Metadata::default()));
        // Throwaway router for the TAGS pass — no microgrids
        // registered, so SiteRouter::site falls through to the
        // bootstrap site and every defun captures that one.
        let microgrids = crate::sim::microgrids::new_registry();
        let current = crate::sim::microgrids::new_current_microgrid();
        let router = SiteRouter::new(microgrids.clone(), current, site.clone());

        let load_dir: PathBuf = roots
            .first()
            .and_then(|r| Path::new(r).parent())
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        ctx.set_load_path(Some(&load_dir))
            .map_err(|e| Error::os_error(format!("set_load_path({}): {e}", load_dir.display())))?;

        defuns::register_runtime(
            &mut ctx,
            router,
            metadata,
            load_dir,
            microgrids,
            crate::sim::sim_clock::NowSource::wall(),
        );
        // The Handle is unused here — tags_table is a one-shot parse
        // pass, no timers ever fire — but `register` still installs
        // the four builtins so that `(run-with-timer …)` etc. show up
        // in the generated TAGS file.
        let _ = tulisp_async::register(&mut ctx, Arc::new(tulisp_async::TokioExecutor::new()));

        ctx.tags_table(Some(roots))
    }

    /// Rebuild exactly one file's world: cancel the timers that file
    /// armed, reset the site of every microgrid it declares, then
    /// evaluate it again. Every other file's microgrids — their
    /// components, their live edits, their timers — are left running
    /// untouched.
    ///
    /// This is the normal reload: an editor save touches one file, so
    /// only that file's world needs rebuilding. Returns the ids the
    /// re-eval registered as NEW (empty for a plain re-load, whose
    /// `(make-microgrid …)` forms reuse their entries in place).
    ///
    /// Works on a file that declares no microgrid at all — a driver
    /// script that only arms `every` blocks re-arms them, since its
    /// old timers were cancelled first.
    ///
    /// On a failed re-eval the error is returned and this file's
    /// microgrids stay reset-empty (their runtimes keep ticking an
    /// empty site) until the next successful reload of the file;
    /// no OTHER file's world is touched either way.
    pub fn reload_file(&self, path: &Path) -> Result<Vec<u64>, String> {
        let mut ctx = self.ctx.borrow_mut();
        self.reload_file_locked(&mut ctx, path)
    }

    /// [`reload_file`](Self::reload_file) against an already-held
    /// interpreter guard.
    pub(super) fn reload_file_locked(
        &self,
        ctx: &mut TulispContext,
        path: &Path,
    ) -> Result<Vec<u64>, String> {
        let resolved = self.resolve_in_state_dir(path);
        // The canonical spelling is what the registry stores in
        // `source` and what `(current-source-file)` reported to the
        // timers this file armed — both comparisons below need it.
        let canonical = resolved.canonicalize().unwrap_or(resolved);
        self.reload_visited_locked(ctx, &canonical)
            .map_err(|e| e.to_string())
    }

    /// The per-file reload body, on an already-canonicalized path.
    /// Shared with [`load_file_locked`](Self::load_file_locked),
    /// which routes a re-load of an already-visited file here rather
    /// than evaluating it a second time on top of itself.
    ///
    /// Keeps the typed [`LoadError`]: a reload can hit a collision too
    /// (an edited file that moved onto another file's id), and the
    /// load endpoint turns that into its "load it as N instead?"
    /// offer.
    fn reload_visited_locked(
        &self,
        ctx: &mut TulispContext,
        canonical: &Path,
    ) -> Result<Vec<u64>, LoadError> {
        // Cancel just this file's timers. A whole-world reload can
        // cancel everything centrally; a per-file one must not, or it
        // would silently freeze every other file's animation.
        let quoted = super::escape_lisp_string(&canonical.display().to_string());
        if let Err(e) = ctx.eval_string(&format!("(cancel-file-timers \"{quoted}\")")) {
            log::warn!(
                "reload {}: cancel-file-timers failed: {}",
                canonical.display(),
                e
            );
        }
        // Reset the sites this file owns, so a component the edited
        // file no longer declares actually disappears. The entries
        // themselves stay — the per-mg runtimes (physics tick, gRPC
        // server) hold these site handles, and the re-eval'd
        // `(make-microgrid …)` forms reuse them in place.
        for entry in self
            .microgrids
            .lock()
            .values()
            .filter(|e| e.source.as_deref() == Some(canonical))
        {
            entry.site.reset();
        }
        let new_ids = self.eval_source_file(ctx, canonical, canonical)?;
        // Tell UI subscribers this file's microgrids rebuilt.
        // `eval_source_file` bumps only the ids that are new; a plain
        // re-load has none, and its rebuilt sites still need the
        // event.
        for (id, entry) in self
            .microgrids
            .lock()
            .iter()
            .filter(|(_, e)| e.source.as_deref() == Some(canonical))
        {
            log_topology_validation(&entry.site, &format!("reload (microgrid {id})"));
            entry.site.bump_version();
        }
        Ok(new_ids)
    }

    /// Rebuild the WHOLE world: re-evaluate `enterprise.lisp`, cancel
    /// every timer, reset every site and the id allocator, then
    /// re-evaluate every file that backs a registered microgrid, in
    /// the order those files first arrived.
    ///
    /// Only files a live microgrid came from are replayed. A driver
    /// script that registers nothing is not part of this list — it is
    /// re-run when its OWN file changes, via
    /// [`reload_file`](Self::reload_file).
    ///
    /// Returns the formatted lisp error on failure. A failing
    /// `enterprise.lisp` aborts before anything is reset, so the
    /// running world is left untouched. Past that point, files before
    /// the failing one have been rebuilt and the failing and later
    /// files' microgrids stay reset-empty; the next successful reload
    /// converges the whole world again.
    pub fn reload(&self) -> Result<(), String> {
        let mut ctx = self.ctx.borrow_mut();
        self.reload_locked(&mut ctx)
    }

    /// `reload` body against an already-held interpreter guard — for
    /// callers that must hold the lock across surrounding work
    /// (re-borrowing inside would deadlock), and for tests that drive
    /// a reload with the guard in hand.
    pub(super) fn reload_locked(&self, ctx: &mut TulispContext) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let start = std::time::Instant::now();
        // Enterprise-wide state first, exactly as at boot: the
        // `*-defaults` plists and the socket addresses must be back in
        // place before any microgrid file rebuilds its components, or
        // the rebuild would silently use built-in defaults.
        //
        // Before any reset, deliberately: the file only sets variables
        // and metadata, so applying it early is harmless — while a
        // syntax error in it AFTER the resets would leave the whole
        // world wiped with nothing replayed. Failing here leaves the
        // running world exactly as it was.
        self.load_enterprise_locked(ctx)?;
        self.site.reset();
        // Reset the enterprise-wide id allocator too — every site
        // is about to be rebuilt by the re-eval of config.lisp,
        // and we want auto-allocated ids to keep starting at
        // FIRST_AUTO_ID across reloads (the comment on
        // MicrogridSite.next_id justifies why).
        self.enterprise_id_allocator
            .store(crate::sim::component::FIRST_AUTO_ID, Ordering::Relaxed);
        // Keep the registry: the per-mg runtimes (physics tick, history
        // sampler, gRPC server, loopback client) each hold their entry's
        // site handle, and the re-eval'd (make-microgrid …) forms reuse
        // those sites in place — dropping the entries would orphan every
        // runtime on a site the registry no longer hands out. Reset each
        // site up front so a microgrid the new config no longer declares
        // ends up empty (its runtimes keep running against the empty
        // site — they can't be torn down without a restart — but it
        // stops ticking stale components).
        for entry in self.microgrids.lock().values() {
            entry.site.reset();
        }
        // Cancel every live timer centrally before the replay: each
        // replayed file re-registers its own `every` blocks, so this
        // is what keeps timer hygiene ORDER-INDEPENDENT. A per-script
        // `(cancel-timers)` call could not be: replayed after another
        // script, it would cancel that script's just-re-registered
        // timers, permanently freezing the earlier world's animation.
        if let Err(e) = ctx.eval_string("(cancel-timers)") {
            log::warn!("reload: cancel-timers failed: {e}");
        }
        // Replay every file that backs a registered microgrid, in the
        // order those files first arrived. The registry is the list:
        // each live microgrid names the file it came from, so there is
        // no separate replay list that could drift out of sync with
        // the world. A file that vanished since it was loaded is
        // skipped with a warning (its microgrids stay reset-empty)
        // rather than aborting the whole reload.
        let files = self.registered_sources();
        for file in &files {
            if !file.exists() {
                log::warn!(
                    "reload: {} no longer exists; skipping (its microgrids stay empty)",
                    file.display()
                );
                continue;
            }
            // Through the loader, not a bare eval_file: each file must
            // be replayed with itself as the ambient loading file, or
            // its own `(make-microgrid …)` forms would look like a
            // stranger re-claiming ids the file already owns. The
            // fresh-load body, not `load_file_locked`: the resets and
            // the central `(cancel-timers)` above already did — once,
            // for the whole world — what a per-file reload would
            // repeat here for every file.
            self.eval_source_file(ctx, file, file)
                .map_err(|e| e.to_string())?;
        }
        warn_orphaned_chp_defaults(ctx);
        if self.microgrids.lock().is_empty() {
            log::warn!("reload left the registry empty — no loaded file registers a microgrid");
        }
        // Tell UI subscribers the MicrogridSite rebuilt. Catches the
        // "removed the only pending entry" case where remove_pending
        // reloads but has no surviving entries to bump-version
        // through eval_with_affects. Bump every registered microgrid
        // so per-mg UI subscribers all see the rebuild — the router-
        // resolved `cfg.site()` reads from the first registry entry,
        // not the bootstrap site we reset above.
        for (id, entry) in self.microgrids.lock().iter() {
            log_topology_validation(&entry.site, &format!("reload (microgrid {id})"));
            entry.site.bump_version();
        }
        log::info!(
            "Reloaded config in {:.1}ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    pub async fn watch(self) {
        self.watch_with_ready(None).await
    }

    /// [`Config::watch`] with a readiness signal: `ready` fires once
    /// watch setup has resolved — the initial watch set is armed, or
    /// arming failed and hot-reload is off. Either way, a file
    /// rewrite sequenced after the signal is seen by the watcher or
    /// never will be, so tests can rewrite deterministically instead
    /// of sleeping and hoping the inotify hook beat them to it.
    pub async fn watch_with_ready(self, ready: Option<tokio::sync::oneshot::Sender<()>>) {
        // Fires on every exit from the arming phase, the error
        // bail-outs included — a waiting test must not hang on a
        // watcher that decided not to run.
        let mut ready = ready;
        let mut armed_signal = move || {
            if let Some(tx) = ready.take() {
                let _ = tx.send(());
            }
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        // notify can fail at construction (out of inotify slots —
        // `fs.inotify.max_user_watches` exhausted by an IDE running
        // alongside) or at watch-registration time (file vanished).
        // Either case kills hot-reload but the rest of the binary
        // should keep serving; log and bail out of just this task.
        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                futures::executor::block_on(async {
                    let _ = tx.send(res).await;
                });
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                log::error!("watch: notify init failed: {e}; hot-reload disabled");
                armed_signal();
                return;
            }
        };
        // Arm the initial watch set: enterprise.lisp, every file the
        // loader evaluated, and every `(watch-file …)` registration.
        let mut armed: HashSet<PathBuf> = HashSet::new();
        let wanted = self.rearm_watches(&mut watcher, &mut armed);
        if armed.is_empty() {
            if wanted == 0 {
                log::info!("watch: nothing to watch; hot-reload idle");
            } else {
                log::error!("watch: no watch could be registered; hot-reload disabled");
            }
            armed_signal();
            return;
        }
        armed_signal();

        // Debounce window. Editors typically fire several notify
        // events for a single save (write + close-after-write +
        // plugin reformat); we coalesce anything arriving within
        // this window into one reload. 150 ms is comfortably above
        // the inotify event-batch latency on a busy machine and
        // still feels instant to a human editing.
        const DEBOUNCE: Duration = Duration::from_millis(150);
        // How often the watch set is rebuilt when nothing happens.
        // Two things go stale on their own: a file loaded at runtime
        // has no watch yet, and an atomic save (write temp + rename)
        // leaves the inotify watch on the REPLACED inode — the event
        // does arrive, but every later edit lands on the new file the
        // dead watch no longer follows. Both fix themselves within
        // this interval; re-arming a handful of paths is cheap.
        const REARM: Duration = Duration::from_secs(5);

        loop {
            let res = match tokio::time::timeout(REARM, rx.recv()).await {
                Ok(Some(res)) => res,
                // Channel closed — the watcher is gone.
                Ok(None) => return,
                Err(_) => {
                    self.rearm_watches(&mut watcher, &mut armed);
                    continue;
                }
            };
            // Which files this batch touched. Only Modify events
            // count — a create/remove on a watched file is not an
            // edit of live content.
            let mut touched: HashSet<PathBuf> = HashSet::new();
            match res {
                Ok(event) => collect_modified(&event, &mut touched),
                Err(e) => {
                    log::error!("watch error: {:?}", e);
                    return;
                }
            }
            // After the first event, drain any further events that
            // arrive within DEBOUNCE; each additional event restarts
            // the window. Once the window goes quiet, reload what the
            // whole batch touched. Reload errors are logged by the
            // reload path and surfaced on the site event bus so the UI
            // can show a banner; the loop intentionally keeps going so
            // a typo doesn't kill the live-edit feedback path.
            loop {
                match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                    Ok(Some(Ok(event))) => {
                        collect_modified(&event, &mut touched);
                        continue;
                    }
                    Ok(Some(Err(e))) => {
                        log::error!("watch error: {:?}", e);
                        return;
                    }
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            if !touched.is_empty() {
                self.reload_touched(&touched);
            }
            // Re-arm after every batch, reload or not: a reload can
            // change what there is to watch (a file may have
            // registered a new microgrid, or dropped its last one),
            // and the events we ignored may have been the replaced
            // file our watch was following.
            self.rearm_watches(&mut watcher, &mut armed);
        }
    }

    /// Point `watcher` at exactly the files worth watching now:
    /// `enterprise.lisp`, every file the loader evaluated, and every
    /// `(watch-file …)` registration. `armed` is the set currently
    /// watched; it is updated in place. Returns how many paths were
    /// wanted, so a caller can tell "nothing to watch" from "every
    /// watch failed to register".
    ///
    /// Loader-visited files, not just the registry's sources: a
    /// driver script that arms timers and registers no microgrid is
    /// still a file an operator edits and expects to take effect.
    fn rearm_watches(
        &self,
        watcher: &mut RecommendedWatcher,
        armed: &mut HashSet<PathBuf>,
    ) -> usize {
        let mut want: Vec<PathBuf> = vec![self.enterprise_path()];
        want.extend(self.loader_visited_files());
        want.extend(self.extra_watches.lock().iter().cloned());
        let want: HashSet<PathBuf> = want
            .into_iter()
            .map(|p| p.canonicalize().unwrap_or(p))
            .filter(|p| p.exists())
            .collect();
        for gone in armed.difference(&want) {
            let _ = watcher.unwatch(gone);
        }
        // Re-arm every wanted path, the already-armed ones included:
        // an atomic save replaces the file, leaving the old watch on
        // an inode nothing writes to any more.
        let mut next = HashSet::new();
        for path in &want {
            match watcher.watch(path, notify::RecursiveMode::NonRecursive) {
                Ok(()) => {
                    next.insert(path.clone());
                }
                Err(e) => log::warn!("watch {}: {}", path.display(), e),
            }
        }
        *armed = next;
        want.len()
    }

    /// React to one debounced batch of file edits: re-evaluate
    /// enterprise settings, reload each edited file the loader knows
    /// on its own, and fall back to a whole-world reload for anything
    /// else (a `(watch-file …)` registration such as a shared library
    /// of helper defuns, whose edit can affect every microgrid).
    fn reload_touched(&self, touched: &HashSet<PathBuf>) {
        let enterprise = {
            let p = self.enterprise_path();
            p.canonicalize().unwrap_or(p)
        };
        let visited: HashSet<PathBuf> = self.loader_visited_files().into_iter().collect();
        let mut errors: Vec<String> = Vec::new();
        let mut whole_world = false;
        for path in touched {
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            // Macrocosim's own save must not bounce back as a reload
            // of the file it just wrote: the world already IS what the
            // file says, and reloading would throw away live state for
            // nothing. One-shot — see `take_self_write`.
            if let Ok(text) = std::fs::read_to_string(&path)
                && self.take_self_write(&path, &text)
            {
                log::debug!(
                    "watch: {} matches what we last wrote; not reloading",
                    path.display()
                );
                continue;
            }
            if path == enterprise {
                // Enterprise settings only: re-evaluating the file
                // applies the new defaults and socket addresses to the
                // running process. It does NOT reset any microgrid —
                // existing components keep the values they were built
                // with, and new defaults apply to what is built next.
                if let Err(e) = self.load_enterprise() {
                    errors.push(e);
                }
            } else if visited.contains(&path) {
                // A file the loader ran: re-run just that file. Its
                // microgrids rebuild and its timers re-arm; every
                // other file's world keeps running.
                if let Err(e) = self.reload_file(&path) {
                    errors.push(e);
                }
            } else {
                whole_world = true;
            }
        }
        if whole_world && let Err(e) = self.reload() {
            errors.push(e);
        }
        for msg in errors {
            // Fan out to every REGISTRY site: the WS event pump only
            // spawns forwarders for registry entries, so a broadcast
            // on the bootstrap site's bus would reach no browser.
            for entry in self.microgrids.lock().values() {
                entry.site.broadcast_config_error(msg.clone());
            }
        }
    }
}

/// Add the paths of a Modify event to `touched`. Anything else (a
/// create, a remove, an access) is not an edit of live content.
fn collect_modified(event: &notify::Event, touched: &mut HashSet<PathBuf>) {
    if matches!(event.kind, notify::EventKind::Modify(_)) {
        touched.extend(event.paths.iter().cloned());
    }
}

/// `chp-defaults` was folded into `marker-defaults` when CHP became
/// a marker category. An `enterprise.lisp` (or a hand-written script)
/// that still sets it evals fine, but `make-chp` no longer reads it
/// — warn instead of silently dropping the customization.
fn warn_orphaned_chp_defaults(ctx: &mut TulispContext) {
    if let Ok(v) = ctx.eval_string("(and (boundp 'chp-defaults) chp-defaults)")
        && !v.null()
    {
        log::warn!(
            "chp-defaults is set but no longer read: CHP defaults \
             moved into marker-defaults. Put the customization there."
        );
    }
}

/// One refresh + timer-drain pass: snapshot the per-mg sites outside
/// the ctx lock, take the interpreter lock once, refresh every
/// component's lisp-bound inputs, then drain the timer mailbox. The
/// single implementation behind both `Config::refresh_once` and the
/// background refresh loop, so the two can't drift.
fn refresh_pass(
    registry: &crate::sim::microgrids::SharedMicrogrids,
    ctx: &SharedMut<TulispContext>,
    timer_handle: &tulisp_async::Handle,
) {
    let sites: Vec<MicrogridSite> = registry.lock().values().map(|e| e.site.clone()).collect();
    let mut guard = ctx.borrow_mut();
    for site in &sites {
        for c in site.components().iter() {
            c.refresh_inputs(&mut guard);
        }
    }
    if let Err(e) = timer_handle.tick(&mut guard) {
        log::warn!("refresh: a timer was stopped: {e}");
    }
}

/// The script's directory, for the single-script constructors that
/// anchor state next to the script. `Path::parent()` returns
/// `Some("")` for bare filenames like "config.lisp" — tulisp rejects
/// empty paths, so those fall back to the cwd.
fn script_parent_dir(script: &str) -> PathBuf {
    Path::new(script)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Run the component-graph validator on the current `MicrogridSite` and
/// log the outcome. `phase` is one of "boot" / "reload" so the log
/// line tags which path triggered the check.
///
/// Log-only, not fatal. Empty worlds (no components yet) skip
/// because the graph crate requires exactly one
/// `GridConnectionPoint` and rejects empty graphs — test fixtures
/// that wire up `Config` against `""` would otherwise fail.
/// Non-empty worlds that fail validation surface as a `log::warn!`
/// in the simulator log; the pulse bar's graph pill gets its ⚠ from
/// `/api/mg/{mg}/topology`, which validates the requested site per
/// request.
///
/// On success the log line includes a one-line summary so a dev
/// reading the log can confirm macrocosim parsed the topology the
/// same way `frequenz-microgrid` would.
fn log_topology_validation(site: &MicrogridSite, phase: &str) {
    let (nodes, edges) = crate::sim::graph_adapter::snapshot(site);
    let visible_count = nodes.len();
    if visible_count == 0 {
        log::debug!("graph: {phase} skipped (no visible components)");
        return;
    }
    match crate::sim::graph_adapter::build_from(nodes, edges) {
        Ok(graph) => {
            // `graph.components()` yields nodes that survived
            // pass-through elision. With no pass-through categories
            // in macrocosim's model yet this equals visible_count;
            // we log both so the gap is visible the day we add a
            // transformer / breaker / converter.
            let logical_count = graph.components().count();
            log::info!(
                "graph: {phase} validated ({visible_count} visible, {logical_count} after pass-through elision)"
            );
        }
        Err(e) => {
            log::warn!(
                "graph: {phase} validation failed — {visible_count} visible components rejected by frequenz-microgrid-component-graph: {e}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    /// No shipped Lisp file spells a renamed keyword or defun the old
    /// way. A line that handles the old spelling on purpose (reads it
    /// from a plist and warns, defines the old defun, or a docstring
    /// saying the old name still works) is skipped.
    #[test]
    fn shipped_lisp_uses_no_renamed_keyword() {
        let mut lines: Vec<String> = Vec::new();
        for path in [
            "sim/common.lisp",
            "sim/defaults.lisp",
            "sim/scenarios.lisp",
            "examples/berlin-demo.lisp",
            "examples/scenario-driving.lisp",
        ] {
            for line in std::fs::read_to_string(path).unwrap().lines() {
                let handles_old = line.contains("%warn-renamed")
                    || line.contains("common--seconds-arg")
                    || line.starts_with("(defun ")
                    || line.contains("still works")
                    || line.to_lowercase().contains("the old ");
                if !handles_old {
                    lines.push(line.to_string());
                }
            }
        }
        // The text without the lines that read `old` from a plist.
        let without_reads = |old: &str| {
            let read = format!("plist-get plist {}", old.trim());
            lines
                .iter()
                .filter(|l| !l.contains(&read))
                .map(|l| format!("{l}\n"))
                .collect::<String>()
        };
        for r in crate::lisp::renames::RENAMES {
            let needle = format!("{} ", r.old);
            let text = without_reads(r.old);
            // `BatteryPool::power ` is prose, not the `:power` keyword.
            let used = text
                .match_indices(&needle)
                .any(|(i, _)| !text[..i].ends_with(':'));
            assert!(!used, "{} still used in a shipped file", r.old);
        }
        for old in [
            ":milliseconds",
            ":every-ms",
            ":min-every ",
            ":max-every ",
            ":min-duration ",
            ":max-duration ",
            ":for ",
            ":over ",
            ":target ",
            "scenario-end-after ",
            "-lifetime-ms",
            "set-physics-tick-ms",
            "set-sample-lag-ms",
            "(set-boiler-demand ",
            "(drive-boiler ",
        ] {
            assert!(
                !without_reads(old).contains(old),
                "{old} still used in a shipped file"
            );
        }
    }

    use super::super::Config;
    use super::super::test_support::config_with;
    use crate::test_dir::TestDir;

    /// Boot a headless `Config` from `body`, written into a fresh temp
    /// dir with `sim/common.lisp` + `sim/scenarios.lisp` copied
    /// alongside so the config's own `(load …)` forms resolve. `tag`
    /// just names the directory, for readable strace/ls output when a
    /// case is being debugged. Dropping the returned dir removes it.
    /// The `ManualClock` the constructor also returns is an `Arc`
    /// clone of the one `Config` keeps, so dropping it here changes
    /// nothing.
    fn stepped_config(tag: &str, body: &str) -> (Config, TestDir) {
        let dir = TestDir::new(&format!("macrocosim-{tag}-"));
        let sim = dir.join("sim");
        std::fs::create_dir_all(&sim).unwrap();
        for f in ["common.lisp", "scenarios.lisp"] {
            std::fs::copy(format!("sim/{f}"), sim.join(f)).unwrap();
        }
        let path = dir.join("config.lisp");
        std::fs::write(&path, body).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (cfg, _clock) = rt
            .block_on(async { Config::new_headless(path.to_str().unwrap()) })
            .expect("headless config builds");
        std::mem::forget(rt);
        (cfg, dir)
    }

    /// The id in a collision message survives being wrapped in
    /// tulisp's trace formatting — that number is what the load
    /// endpoint offers a free id against.
    #[test]
    fn collision_messages_yield_their_microgrid_id() {
        assert_eq!(
            super::collision_id_in(
                "eval error:\n  microgrid 9 is already loaded (from /tmp/x/config.lisp)\n  at …"
            ),
            Some(9)
        );
        assert_eq!(super::collision_id_in("cannot read /tmp/nope.lisp"), None);
        // No number in front of the phrase: not a collision we can act on.
        assert_eq!(super::collision_id_in("the file is already loaded"), None);
    }

    /// A bare boot (no scripts) is a legitimate live state: empty
    /// registry, DSL live, topologies arrive on demand.
    #[test]
    fn bare_boot_has_empty_registry() {
        let dir = TestDir::new("macrocosim-bare-");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cfg = rt
            .block_on(async { Config::new_with(&[], Some(dir.to_path_buf())) })
            .expect("bare boot must succeed");
        std::mem::forget(rt);
        assert!(cfg.microgrids().lock().is_empty());
        // The DSL is live: a runtime eval can still build a world.
        let out = cfg
            .eval("(make-microgrid :id 7 :grpc-port 8807 :topology (lambda () nil))")
            .expect("runtime make-microgrid");
        assert!(!out.is_empty());
        assert!(cfg.microgrids().lock().contains_key(&7));
    }

    /// Reload replays every file a live microgrid came from, so a
    /// file loaded after boot (a runtime `(load …)` or a created
    /// microgrid's file) brings its microgrid back instead of being
    /// forgotten.
    #[test]
    fn reload_replays_every_registered_source() {
        let (cfg, dir) = config_with("nil");
        let extra = dir.join("extra-mg.lisp");
        std::fs::write(
            &extra,
            "(make-microgrid :id 42 :grpc-port 8842 :topology (lambda () nil))",
        )
        .unwrap();
        cfg.load_file(&extra).expect("load");
        cfg.reload().expect("reload");
        let reg = cfg.microgrids();
        let reg = reg.lock();
        assert!(reg.contains_key(&42), "recorded file must be replayed");
        assert!(reg.len() >= 2, "boot script's microgrid must survive too");
    }

    /// `Config::new` returns Err on lisp eval failure rather than
    /// silently logging — the binary panics with a useful message
    /// and tests get a clear assertion target rather than a
    /// half-built MicrogridSite.
    #[test]
    fn config_new_returns_err_on_bad_lisp() {
        let dir = TestDir::new("macrocosim-cfg-bad-");
        let path = dir.join("config.lisp");
        std::fs::write(&path, "(this-is-not-a-defun-anywhere 42)").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let res = rt.block_on(async { Config::new(path.to_str().unwrap()) });
        std::mem::forget(rt);
        let err = match res {
            Ok(_) => panic!("expected lisp error for undefined fn"),
            Err(e) => e,
        };
        assert!(
            err.contains("this-is-not-a-defun-anywhere"),
            "error should name the offending symbol: {err}",
        );
    }

    /// A headless `Config` runs physics, timers, and scenario time on a
    /// hand-advanced sim clock: `sim_run` drives a 60 s scenario in
    /// near-zero wall time, the timer fires at sim t=30, and the battery
    /// SoC integrates on sim-time (3.6 kW into 1 kWh for 60 s = +6%).
    #[test]
    fn headless_sim_runs_physics_and_timers_on_sim_time() {
        use std::time::Duration;
        let body = "(set-enterprise-id 1)
(make-microgrid :id 9 :grpc-port 18901 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2
        :successors (list (%make-battery-inverter :id 3 :rated-lower-w -5000.0 :rated-upper-w 5000.0
          :successors (list (%make-battery :id 4 :rated-lower-w -5000.0 :rated-upper-w 5000.0
            :capacity-wh 1000.0 :initial-soc-pct 50.0)))))))))
(setq fired 0)
(run-with-timer 30 nil (lambda () (setq fired 1)))
(scenario-start \"sim\")
(set-active-power 3 3600.0 600000)";
        let dir = TestDir::new("macrocosim-headless-");
        let path = dir.join("config.lisp");
        std::fs::write(&path, body).unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (cfg, _clock) = rt
            .block_on(async { Config::new_headless(path.to_str().unwrap()) })
            .expect("headless config builds");
        std::mem::forget(rt);

        let start = std::time::Instant::now();
        let steps = cfg.sim_run(Duration::from_secs(60), Duration::from_secs(1));
        let wall = start.elapsed();

        assert_eq!(steps, 60);
        // 60 sim-seconds elapsed in near-zero wall time.
        let elapsed: f64 = cfg
            .eval_silent("(scenario-elapsed)")
            .unwrap()
            .parse()
            .unwrap();
        assert!((elapsed - 60.0).abs() < 2.0, "scenario-elapsed = {elapsed}");
        assert!(wall < Duration::from_secs(5), "headless wall time {wall:?}");
        // Timer fired at sim t=30.
        assert_eq!(cfg.eval_silent("fired").unwrap(), "1");
        // Physics integrated SoC on sim-time: 50% -> ~56%. Asserted via
        // the scenario-expect check itself (I2), exercising the whole
        // stack.
        assert_eq!(
            cfg.eval_silent("(scenario-expect :component-id 4 :metric 'soc :approx 56.0 :tol 1.0)")
                .unwrap(),
            "t",
            "battery SoC should integrate on sim-time to ~56%",
        );
        // Energy accrues on the physics tick too, so an energy
        // scenario-expect resolves in a headless stepped run (regression:
        // it used to read the never-populated history layer and always
        // fail). Meter 2 imports 3600 W for ~60 s -> ~60 Wh.
        assert_eq!(
            cfg.eval_silent(
                "(scenario-expect :component-id 2 :metric 'energy :min 40.0 :max 70.0)"
            )
            .unwrap(),
            "t",
            "meter energy should integrate on sim-time in a stepped run",
        );
    }

    /// A headless config's microgrid sites read the sim clock, so a
    /// request lifetime there runs on sim time.
    #[test]
    fn headless_sites_read_the_sim_clock() {
        use std::time::Duration;
        let body = "(make-microgrid :id 9 :grpc-port 18911 :topology (lambda () nil))";
        let (cfg, _dir) = stepped_config("headless-clock", body);
        let base = crate::sim::sim_clock::headless_base();
        assert_eq!(cfg.site().now(), base);
        cfg.sim_run(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(cfg.site().now(), base + chrono::Duration::seconds(5));
    }

    /// The shipped demo script boots cleanly and its scenarios
    /// register — a guard against a `define-scenario` schema change
    /// silently breaking the example, since nothing else exercises
    /// the real shipped script end to end.
    #[test]
    fn default_config_boots_and_registers_library_scenarios() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (cfg, _clock) = rt
            .block_on(async { Config::new_headless("examples/berlin-demo.lisp") })
            .expect("shipped berlin-demo.lisp boots headless");
        std::mem::forget(rt);
        let names: Vec<String> = cfg.scenarios().lock().keys().cloned().collect();
        assert!(
            names.contains(&"peak-evening-load".to_string()),
            "expected the starter library to register; got {names:?}"
        );
        assert!(
            names.len() >= 7,
            "expected >=7 starter scenarios, got {names:?}"
        );
    }

    /// The stepped runner drives a registered scenario end-to-end on
    /// the sim clock: `define-scenario` sections (a `timeline` drive +
    /// timed `check`s) compile through `scenario--run`, the checks fire
    /// as sim-time timers, and `run_scenario_stepped` advances the
    /// clock for the declared `:length` — far enough that a check
    /// scheduled at exactly `:length` still fires, before the run's
    /// own stop. Deterministic + faster than real time — the J2 CI
    /// gate.
    #[test]
    fn stepped_runner_runs_a_registered_scenario() {
        use std::time::Duration;
        let body = "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18903 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)))))
(define-scenario :name \"ramp\"
  :schedule 'relative :clock 'stepped :length \"60s\" :seed 7
  :drive (list (drive-meter 2 (timeline (hold 1000.0 :for-s 30)
                                        (ramp :to 5000.0 :over-s 30))))
  :expect (list (check \"10s\" :component-id 2 :metric 'active-power
                       :approx 1000.0 :tol 200.0)
                (check \"59s\" :component-id 2 :metric 'active-power
                       :approx 5000.0 :tol 800.0)
                (check \"60s\" :component-id 2 :metric 'active-power
                       :approx 5000.0 :tol 800.0)))";
        let (cfg, _dir) = stepped_config("stepped", body);

        let start = std::time::Instant::now();
        let steps = cfg
            .run_scenario_stepped("ramp", Duration::from_secs(1), None)
            .expect("scenario runs");
        let wall = start.elapsed();
        assert_eq!(steps, 60);
        assert!(wall < Duration::from_secs(5), "stepped wall time {wall:?}");

        // All three timed checks fired and passed; none failed. The
        // third sits at exactly `:length`, which pins the runner's
        // ordering: `sim_run` advances the clock through t=60 (firing
        // that check) BEFORE the `(scenario-stop)` that would freeze
        // the journal and cancel any timer still pending.
        let report = cfg.site().scenario_report(chrono::Utc::now());
        assert_eq!(report.checks_passed, 3, "report: {report:?}");
        assert_eq!(report.checks_failed, 0, "report: {report:?}");

        // A scenario without :length can't be stepped; an unknown one
        // errors too.
        assert!(
            cfg.run_scenario_stepped("nope", Duration::from_secs(1), None)
                .is_err()
        );
    }

    /// The Q twin of `stepped_runner_runs_a_registered_scenario`: the
    /// `drive-meter-reactive` and `drive-meter-pf` dispatch arms in
    /// `sim/scenarios.lisp` compile through `scenario--run` and really
    /// drive the meters, checked by timed `:metric 'reactive-power`
    /// expectations. Meter 2 follows a VAr timeline; meter 3 holds a
    /// 0.8 lagging power factor against its own 8 kW of P, so its Q
    /// settles at 8000 * tan(acos(0.8)) = 6 kVAr.
    #[test]
    fn stepped_runner_drives_reactive_and_power_factor() {
        use std::time::Duration;
        let body = "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18904 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)
                        (%make-meter :id 3 :power-w 8000.0)))))
(define-scenario :name \"q-ramp\"
  :schedule 'relative :clock 'stepped :length \"60s\" :seed 7
  :drive (list (drive-meter-reactive 2 (timeline (hold 500.0 :for-s 30)
                                                 (ramp :to 2000.0 :over-s 30)))
               (drive-meter-pf 3 0.8))
  :expect (list (check \"10s\" :component-id 2 :metric 'reactive-power
                       :approx 500.0 :tol 100.0)
                (check \"59s\" :component-id 2 :metric 'reactive-power
                       :approx 2000.0 :tol 400.0)
                (check \"10s\" :component-id 3 :metric 'reactive-power
                       :approx 6000.0 :tol 100.0)))";
        let (cfg, _dir) = stepped_config("stepped-q", body);

        let steps = cfg
            .run_scenario_stepped("q-ramp", Duration::from_secs(1), None)
            .expect("scenario runs");
        assert_eq!(steps, 60);

        let report = cfg.site().scenario_report(chrono::Utc::now());
        assert_eq!(report.checks_passed, 3, "report: {report:?}");
        assert_eq!(report.checks_failed, 0, "report: {report:?}");
    }

    /// Pins the stepped-runner half of scenario teardown:
    /// `run_scenario_stepped` now evals `(scenario-stop)` after
    /// `sim_run` completes, so a driven knob is back to its
    /// pre-scenario state (here, meter 2's constructed `:power-w`) by
    /// the time the call returns — not left at the scenario's final
    /// driven value.
    #[test]
    fn stepped_runner_restores_a_driven_knob_after_it_stops_the_scenario() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "stepped-restore",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18905 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 1234.0)))))
(define-scenario :name \"restore-check\"
  :schedule 'relative :clock 'stepped :length \"5s\"
  :drive (list (drive-meter 2 9999.0)))",
        );

        let steps = cfg
            .run_scenario_stepped("restore-check", Duration::from_secs(1), None)
            .expect("scenario runs");
        assert_eq!(steps, 5);

        let m = cfg.site().get(2).unwrap();
        let reading = m
            .meter_drive()
            .unwrap()
            .meter_power_reading()
            .expect("meter still has a source — the constructed :power-w");
        assert_eq!(
            reading.value, 1234.0,
            "run_scenario_stepped must restore the pre-scenario :power-w after stopping"
        );
    }

    /// Pins the timer-cancellation half of scenario teardown: an
    /// `every`-driven agent installed by `scenario--agent` must stop
    /// firing once `(scenario-stop)` runs — `scenario--cancel-timers`
    /// (wired into the `scenario-stop` Rust defun) cancels exactly
    /// the timers `scenario--run` armed. Drives the scenario manually
    /// (rather than through `run_scenario_stepped`, which now stops
    /// it automatically) so the test can step sim-time both before
    /// AND after an explicit stop and compare.
    #[test]
    fn scenario_stop_cancels_the_agent_timer_so_it_stops_firing() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "stepped-cancel",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18906 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)))))
(setq tick-count 0)
(define-scenario :name \"agent-cancel\"
  :schedule 'relative :clock 'stepped :length \"20s\"
  :agents (list (controller 'counter :every \"1s\"
                  (lambda () (setq tick-count (+ tick-count 1))))))",
        );

        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "agent-cancel")
            .expect("scenario starts");
        cfg.sim_run(Duration::from_secs(3), Duration::from_secs(1));
        let mid = cfg.eval("tick-count").expect("read tick-count");
        assert_eq!(mid, "3", "agent should have fired 3 times by 3s");
        let armed_before_stop = cfg
            .eval("(length active-timers)")
            .expect("read active-timers length");
        assert_eq!(
            armed_before_stop, "1",
            "the agent's own repeating timer is the only entry on active-timers pre-stop"
        );

        cfg.eval("(scenario-stop)").expect("scenario stops");
        let armed_after_stop = cfg
            .eval("(length active-timers)")
            .expect("read active-timers length");
        assert_eq!(
            armed_after_stop, "0",
            "scenario-stop must also prune active-timers, not just cancel the handle"
        );
        cfg.sim_run(Duration::from_secs(5), Duration::from_secs(1));
        let after = cfg.eval("tick-count").expect("read tick-count");
        assert_eq!(
            after, mid,
            "agent timer must be cancelled by scenario-stop — tick-count must not advance further"
        );
    }

    /// `every` takes `:interval-s`; the old `:milliseconds` still arms
    /// the same cadence.
    #[test]
    fn every_takes_seconds_and_still_reads_milliseconds() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "every-s",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(make-microgrid :id 9 :grpc-port 18911 :topology (lambda () nil))
(setq n 0)
(setq m 0)
(every :interval-s 0.1 :call (lambda () (setq n (+ n 1))))
(every :milliseconds 100 :call (lambda () (setq m (+ m 1))))",
        );
        cfg.sim_run(Duration::from_secs(1), Duration::from_millis(100));
        assert_eq!(cfg.eval_silent("n").unwrap(), cfg.eval_silent("m").unwrap());
        assert_ne!(cfg.eval_silent("n").unwrap(), "0");
    }

    /// `define-controller` takes `:every-s`; the old `:every-ms` still
    /// sets the same cadence.
    #[test]
    fn define_controller_takes_seconds_and_still_reads_milliseconds() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "ctl-s",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(make-microgrid :id 9 :grpc-port 18912 :topology (lambda () nil))
(setq n 0)
(setq m 0)
(define-controller :every-s 0.2 :on-tick (lambda () (setq n (+ n 1))))
(define-controller :every-ms 200 :on-tick (lambda () (setq m (+ m 1))))",
        );
        cfg.sim_run(Duration::from_secs(1), Duration::from_millis(100));
        assert_eq!(cfg.eval_silent("n").unwrap(), cfg.eval_silent("m").unwrap());
        assert_ne!(cfg.eval_silent("n").unwrap(), "0");
    }

    /// `(scenario-end-after-s 90)` and `(scenario-end-after 1.5)` stop
    /// the scenario at the same moment: still running at 89 s, stopped
    /// by 91 s.
    #[test]
    fn scenario_end_after_s_and_minutes_schedule_the_same_stop() {
        use std::time::Duration;
        for (tag, form) in [
            ("end-s", "(scenario-end-after-s 90)"),
            ("end-min", "(scenario-end-after 1.5)"),
        ] {
            let (cfg, _dir) = stepped_config(
                tag,
                &format!(
                    "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(make-microgrid :id 9 :grpc-port 18913 :topology (lambda () nil))
(scenario-start \"end\")
{form}"
                ),
            );
            cfg.sim_run(Duration::from_secs(89), Duration::from_secs(1));
            assert_eq!(
                cfg.eval_silent("(scenario-running-p)").unwrap(),
                "t",
                "{form}"
            );
            cfg.sim_run(Duration::from_secs(2), Duration::from_secs(1));
            assert_eq!(
                cfg.eval_silent("(scenario-running-p)").unwrap(),
                "nil",
                "{form}"
            );
        }
    }

    /// A `:setup`-armed `random-outage` chain is a real surprise case
    /// for teardown: it re-arms itself on every fire/restore
    /// (`random-outage--schedule` -> `--fire` -> `--restore` ->
    /// `--schedule` ...), so unlike an `every`-driven agent it never
    /// stops re-tracking a NEW handle each cycle. Pins that
    /// `random-outage--track` routing every re-arm through
    /// `scenario--track-timer` actually stops the chain at
    /// `(scenario-stop)`: with `:min-every-s`/`:max-every-s` and
    /// `:min-duration-s`/`:max-duration-s` collapsed to equal bounds, the
    /// cadence is deterministic without even needing the RNG seed
    /// (`random-uniform`'s span is zero), so this asserts health
    /// flips down-then-up DURING the run, then — after an explicit
    /// stop — stays 'ok and no further outage/restored events land
    /// on the scenario journal, well past where another cycle would
    /// have fired had the chain survived.
    #[test]
    fn scenario_stop_cancels_a_setup_armed_random_outage_chain() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "stepped-outage",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18907 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)))))
(define-scenario :name \"outage-setup\"
  :schedule 'relative :clock 'stepped :length \"30s\" :seed 7
  :setup (lambda () (random-outage (list 2)
                                   :min-every-s 2.0 :max-every-s 2.0
                                   :min-duration-s 2.0 :max-duration-s 2.0)))",
        );

        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "outage-setup")
            .expect("scenario starts");
        // Outage fires at t=2 (health -> error), restores at t=4
        // (health -> ok), next outage armed for t=6. Land at t=5, in
        // the gap, and confirm the chain actually ran.
        cfg.sim_run(Duration::from_secs(5), Duration::from_secs(1));
        let health = cfg.site().runtime_of(2).health;
        assert_eq!(
            health,
            crate::sim::runtime::Health::Ok,
            "mid-run: should be back from the first restore by t=5"
        );
        let events_before_stop = cfg
            .site()
            .scenario_events_since(0, 1000)
            .into_iter()
            .filter(|e| e.kind == "outage" || e.kind == "restored")
            .count();
        assert_eq!(
            events_before_stop, 2,
            "one outage + one restored event by t=5"
        );

        cfg.eval("(scenario-stop)").expect("scenario stops");
        let armed_after_stop = cfg
            .eval("(length active-timers)")
            .expect("read active-timers length");
        assert_eq!(
            armed_after_stop, "0",
            "the outage chain's own re-armed timer must be pruned from active-timers too"
        );

        // Step well past where at least one more outage cycle
        // (t=6 down, t=8 up) would have fired had the chain survived.
        cfg.sim_run(Duration::from_secs(15), Duration::from_secs(1));
        let health_after = cfg.site().runtime_of(2).health;
        assert_eq!(
            health_after,
            crate::sim::runtime::Health::Ok,
            "the outage chain must not flip health again after scenario-stop"
        );
        let events_after = cfg
            .site()
            .scenario_events_since(0, 1000)
            .into_iter()
            .filter(|e| e.kind == "outage" || e.kind == "restored")
            .count();
        assert_eq!(
            events_after, events_before_stop,
            "no further outage/restored events after scenario-stop cancelled the chain"
        );
    }

    /// Cancelling a scenario-owned outage chain removes the very
    /// timer that would have ended the outage in flight, so a stop
    /// landing inside an outage window would strand the victim in
    /// `error with nothing left to bring it back —
    /// `random-outage--release-victim` is teardown doing that
    /// restore itself. Here the chain fires at t=2 for four seconds
    /// and the stop lands at t=3, squarely inside it.
    #[test]
    fn stopping_mid_outage_puts_the_victims_health_back() {
        use crate::sim::runtime::Health;
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "stepped-mid-outage",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18914 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)))))
(define-scenario :name \"mid-outage\"
  :schedule 'relative :clock 'stepped :length \"30s\" :seed 7
  :setup (lambda () (random-outage (list 2)
                                   :min-every-s 2.0 :max-every-s 2.0
                                   :min-duration-s 4.0 :max-duration-s 4.0)))",
        );
        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "mid-outage")
            .expect("scenario starts");
        cfg.sim_run(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(
            cfg.site().runtime_of(2).health,
            Health::Error,
            "the outage fired at t=2 and runs to t=6"
        );

        cfg.eval("(scenario-stop)").expect("scenario stops");
        assert_eq!(
            cfg.site().runtime_of(2).health,
            Health::Ok,
            "teardown cancelled the chain's restore timer, so teardown owes the \
             victim its health back — right away, not never"
        );
        // The journal records it, and nothing fires afterward: the
        // chain is gone, so health stays put past both the cancelled
        // restore (t=6) and the next outage that would have been (t=8).
        let kinds: Vec<String> = cfg
            .site()
            .scenario_events_since(0, 1000)
            .into_iter()
            .filter(|e| e.kind == "outage" || e.kind == "restored")
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, vec!["outage", "restored"], "journal: {kinds:?}");
        cfg.sim_run(Duration::from_secs(10), Duration::from_secs(1));
        assert_eq!(cfg.site().runtime_of(2).health, Health::Ok);
        assert_eq!(
            cfg.site()
                .scenario_events_since(0, 1000)
                .into_iter()
                .filter(|e| e.kind == "outage" || e.kind == "restored")
                .count(),
            2,
            "the cancelled chain must not journal anything more"
        );
    }

    /// The victim marker must not outlive the chain that set it.
    /// `random-outage--release-victim` reads it under whatever chain
    /// owns the process NOW, so an id left behind by a chain
    /// cancelled mid-outage — a whole-world reload's
    /// `(cancel-timers)` is the way that happens — would have a later
    /// scenario's stop force `(set-component-health OLD-ID 'ok)` and
    /// journal a "back" event for an outage that scenario never
    /// caused. Both clears are exercised here: `cancel-timers`
    /// blanking the marker, and the next `random-outage` refusing to
    /// inherit one.
    #[test]
    fn a_chain_cancelled_mid_outage_leaves_no_victim_for_a_later_stop() {
        use crate::sim::runtime::Health;
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "stepped-stale-victim",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18915 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)
                        (%make-meter :id 3 :power-w 0.0)))))
(random-outage (list 2) :min-every-s 2.0 :max-every-s 2.0
                        :min-duration-s 60.0 :max-duration-s 60.0)
(define-scenario :name \"later\"
  :schedule 'relative :clock 'stepped :length \"30s\"
  :setup (lambda () (random-outage (list 3)
                                   :min-every-s 600.0 :max-every-s 600.0
                                   :min-duration-s 60.0 :max-duration-s 60.0)))",
        );
        let victim = || cfg.eval("random-outage--current-victim").unwrap();

        // An ambient chain knocks meter 2 down at t=2, for 60 s.
        cfg.sim_run(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(cfg.site().runtime_of(2).health, Health::Error);
        assert_eq!(victim(), "2");

        // Defence 1 — a whole-world reload's central cancel lands
        // mid-outage: the restore timer dies with everything else,
        // and the marker must not survive it.
        cfg.eval("(cancel-timers)").expect("cancel-timers");
        assert_eq!(
            victim(),
            "nil",
            "cancel-timers must clear the in-flight victim along with the chain"
        );

        // Defence 2 — the same hand-off with no reload in it: a fresh
        // ambient chain downs meter 2 again, and then a scenario arms
        // a chain of its OWN (on meter 3, too slow to fire here).
        // Starting that chain is the only thing standing between the
        // stale id and this scenario's ownership.
        cfg.eval(
            "(random-outage (list 2) :min-every-s 1.0 :max-every-s 1.0 \
                             :min-duration-s 60.0 :max-duration-s 60.0)",
        )
        .expect("ambient chain re-armed");
        cfg.sim_run(Duration::from_secs(2), Duration::from_secs(1));
        assert_eq!(victim(), "2", "the ambient chain is mid-outage again");

        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "later").expect("scenario starts");
        assert_eq!(
            victim(),
            "nil",
            "a chain must not inherit the previous chain's in-flight victim"
        );

        cfg.eval("(scenario-stop)").expect("scenario stops");
        assert_eq!(
            cfg.site().runtime_of(2).health,
            Health::Error,
            "meter 2's outage belongs to the ambient chain — this scenario's \
             teardown must not write its health at all"
        );
        assert_eq!(
            cfg.site()
                .scenario_events_since(0, 1000)
                .into_iter()
                .filter(|e| e.kind == "restored")
                .count(),
            0,
            "no restore was owed, so none may be journalled"
        );
    }

    /// The twin of the case above, and the one that decides where the
    /// ownership question is ASKED. An outage chain armed AMBIENTLY —
    /// before any scenario ran — re-arms itself from its own timer
    /// callbacks, so a chain that happens to re-arm while an unrelated
    /// scenario is running must NOT be adopted by it: ownership is
    /// latched once, at `random-outage`, not re-derived per re-arm.
    /// Steps the clock across a re-arm that lands inside the
    /// scenario's window, stops the scenario, and confirms the chain
    /// both completes that cycle and keeps firing new ones.
    #[test]
    fn an_ambient_outage_chain_survives_an_unrelated_scenarios_stop() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "ambient-outage",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18909 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)))))
(random-outage (list 2) :min-every-s 2.0 :max-every-s 2.0
                        :min-duration-s 2.0 :max-duration-s 2.0)
(define-scenario :name \"unrelated\"
  :schedule 'relative :clock 'stepped :length \"30s\")",
        );
        let health = || cfg.site().runtime_of(2).health;
        use crate::sim::runtime::Health;

        // Ambient cadence with no scenario in sight: down at t=2, up
        // at t=4, next outage armed for t=6.
        cfg.sim_run(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(health(), Health::Ok, "mid-gap at t=5");

        // A scenario starts at t=5 and the chain re-arms (t=6 fire,
        // then a t=8 restore) entirely inside its window.
        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "unrelated")
            .expect("scenario starts");
        cfg.sim_run(Duration::from_secs(2), Duration::from_secs(1));
        assert_eq!(health(), Health::Error, "the t=6 outage fired");
        assert_eq!(
            cfg.eval("(length scenario--armed-timers)").unwrap(),
            "0",
            "an ambient chain's re-arm must not be captured by whatever scenario \
             happens to be running when it re-arms"
        );

        cfg.eval("(scenario-stop)").expect("scenario stops");
        assert_eq!(
            health(),
            Health::Error,
            "teardown must not force-restore an AMBIENT chain's victim: that outage \
             was not this scenario's to end, and the chain will end it on schedule"
        );
        // The restore this chain had already armed still fires...
        cfg.sim_run(Duration::from_secs(1), Duration::from_secs(1));
        assert_eq!(
            health(),
            Health::Ok,
            "the pending restore must survive an unrelated scenario's stop — \
             otherwise the component is stuck in the outage forever"
        );
        // ...and the chain keeps arming new cycles (t=10 fire).
        cfg.sim_run(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(
            health(),
            Health::Error,
            "an ambient chain must keep firing after an unrelated scenario stops"
        );
    }

    /// `scenario--track-timer` needs no flag of its own: it records a
    /// handle exactly when `(scenario-running-p)` says a scenario is
    /// in progress. Outside a run it's a pass-through, so an ambient
    /// `every` / outage chain can route through it unconditionally and
    /// still be left alone by every scenario's teardown.
    #[test]
    fn track_timer_records_only_while_a_scenario_runs() {
        let (cfg, _dir) = stepped_config(
            "track-timer",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18910 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)))))",
        );
        assert_eq!(cfg.eval("(scenario-running-p)").unwrap(), "nil");
        cfg.eval("(scenario--track-timer (run-with-timer 9999 nil (lambda () nil)))")
            .unwrap();
        assert_eq!(
            cfg.eval("(length scenario--armed-timers)").unwrap(),
            "0",
            "a timer armed with no scenario running belongs to nobody"
        );

        cfg.eval("(scenario-start \"live\")").unwrap();
        assert_eq!(cfg.eval("(scenario-running-p)").unwrap(), "t");
        cfg.eval("(scenario--track-timer (run-with-timer 9999 nil (lambda () nil)))")
            .unwrap();
        assert_eq!(
            cfg.eval("(length scenario--armed-timers)").unwrap(),
            "1",
            "a timer armed while a scenario runs is that scenario's to cancel"
        );
    }

    /// Starting a scenario while one is still running tears the
    /// running one down first, rather than orphaning it: `scenario-start`
    /// clears the knob baseline Rust-side and `scenario--run` resets
    /// the armed-timer list, so without the stop scenario A's driven
    /// meter could never be restored and A's agent would keep firing
    /// through B's run and beyond. Drives A (a constructed-`:power-w`
    /// meter + a 1 s agent), starts B with no stop in between, and
    /// checks both halves.
    #[test]
    fn starting_a_scenario_tears_down_the_one_still_running() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "overlap",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18911 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 1234.0)))))
(setq a-ticks 0)
(define-scenario :name \"a\"
  :schedule 'relative :clock 'stepped :length \"60s\"
  :drive (list (drive-meter 2 9000.0))
  :agents (list (controller 'a-counter :every \"1s\"
                  (lambda () (setq a-ticks (+ a-ticks 1))))))
(define-scenario :name \"b\"
  :schedule 'relative :clock 'stepped :length \"60s\")",
        );
        let power = || {
            cfg.site()
                .get(2)
                .unwrap()
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value
        };

        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "a").expect("A starts");
        cfg.sim_run(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(cfg.eval("a-ticks").unwrap(), "3", "A's agent is firing");
        assert_eq!(power(), 9000.0, "A is driving the meter");

        // B starts with A still running — no (scenario-stop) of our own.
        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "b").expect("B starts");
        assert_eq!(
            power(),
            1234.0,
            "A's teardown must restore the meter's PRE-A :power-w as B begins"
        );
        let ticks_at_b_start = cfg.eval("a-ticks").unwrap();
        cfg.sim_run(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(
            cfg.eval("a-ticks").unwrap(),
            ticks_at_b_start,
            "A's agent must have been cancelled by A's teardown, not left running into B"
        );

        cfg.eval("(scenario-stop)").expect("B stops");
        assert_eq!(
            power(),
            1234.0,
            "B drove nothing, so stopping it leaves A's restored pre-scenario value"
        );
    }

    /// A cue that hasn't fired yet is the running scenario's own timer
    /// and dies with it: a cue armed for t=10 s, with the scenario
    /// stopped at t=3 s, must never run — stepping the clock well past
    /// its due time leaves its counter untouched.
    #[test]
    fn scenario_stop_cancels_a_cue_that_has_not_fired_yet() {
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "cue-cancel",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(make-microgrid :id 9 :grpc-port 18912 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 0.0)))))
(setq cue-count 0)
(define-scenario :name \"cue-cancel\"
  :schedule 'relative :clock 'stepped :length \"30s\"
  :cues (list (at \"10s\" (lambda () (setq cue-count (+ cue-count 1))))))",
        );
        crate::sim::scenarios::start(&cfg.ctx, &cfg.scenarios, "cue-cancel")
            .expect("scenario starts");
        cfg.sim_run(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(cfg.eval("cue-count").unwrap(), "0", "cue is due at 10s");

        cfg.eval("(scenario-stop)").expect("scenario stops");
        assert_eq!(
            cfg.eval("(length active-timers)").unwrap(),
            "0",
            "the pending cue timer must be pruned from active-timers too"
        );
        cfg.sim_run(Duration::from_secs(15), Duration::from_secs(1));
        assert_eq!(
            cfg.eval("cue-count").unwrap(),
            "0",
            "a cue cancelled at stop must not fire once its due time passes"
        );
    }

    /// A restored knob must read its CURRENT value the instant the
    /// stop returns, not the one its source last resolved to before
    /// the scenario displaced it. The snapshot carries the live
    /// `DynamicScalar` plus a cached number frozen at capture time, so
    /// a scenario cue that moves the variable the source reads leaves
    /// that cache stale — `scenario_stop`'s refresh phase is what
    /// re-resolves it, both for the value the stepped runner's caller
    /// reads back and for the `KnobChanged` the restore broadcasts.
    #[test]
    fn stepped_runner_restores_a_dynamic_source_at_its_current_value() {
        use crate::sim::events::SiteEvent;
        use std::time::Duration;
        let (cfg, _dir) = stepped_config(
            "restore-refresh",
            "(set-enterprise-id 1)
(load \"sim/common.lisp\")
(load \"sim/scenarios.lisp\")
(setq load-w 1000.0)
(make-microgrid :id 9 :grpc-port 18913 :topology
  (lambda ()
    (%make-grid-connection-point :id 1
      :successors (list (%make-meter :id 2 :power-w 'load-w)))))
(define-scenario :name \"refresh\"
  :schedule 'relative :clock 'stepped :length \"5s\"
  :drive (list (drive-meter 2 9999.0))
  :cues (list (at \"2s\" (lambda () (setq load-w 4242.0)))))",
        );
        let mut rx = cfg.site().subscribe_events();
        let steps = cfg
            .run_scenario_stepped("refresh", Duration::from_secs(1), None)
            .expect("scenario runs");
        assert_eq!(steps, 5);

        let reading = cfg
            .site()
            .get(2)
            .unwrap()
            .meter_drive()
            .unwrap()
            .meter_power_reading()
            .unwrap();
        assert_eq!(
            reading.expr.as_deref(),
            Some("load-w"),
            "the dynamic source itself must come back, not a collapsed constant"
        );
        assert_eq!(
            reading.value, 4242.0,
            "the restored source must read the variable's CURRENT value, not the \
             one cached when the scenario snapshotted it"
        );

        let mut last_broadcast = None;
        while let Ok(ev) = rx.try_recv() {
            if let SiteEvent::KnobChanged {
                id: 2,
                knob: "meter-power",
                value,
                ..
            } = ev
            {
                last_broadcast = value;
            }
        }
        assert_eq!(
            last_broadcast,
            Some(4242.0),
            "the restore's KnobChanged must carry the re-resolved value"
        );
    }

    /// `Config::refresh_once` drains tulisp-async's pending-timer
    /// queue. Without that, run-with-timer would just accumulate
    /// PendingTasks (same-ctx model — nothing fires them
    /// asynchronously). A zero-delay one-shot timer plus one
    /// refresh is the tightest expression of the contract.
    #[test]
    fn refresh_once_drains_pending_timers() {
        let (cfg, _dir) = config_with(
            "(setq fired 0)
             (run-with-timer 0 nil (lambda () (setq fired 1)))",
        );
        cfg.refresh_once();
        assert_eq!(cfg.eval_silent("fired").unwrap(), "1");
    }

    /// `reload()` must rebuild each microgrid's topology on the SAME
    /// site the boot-time runtimes hold — minting fresh sites would
    /// leave physics ticking (and gRPC serving) orphaned pre-reload
    /// state while the registry's new sites never tick.
    #[test]
    fn reload_rebuilds_topology_on_the_same_site() {
        let (cfg, _dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
             (lambda () (%make-grid-connection-point :id 1)))",
        );
        // The handle the running physics task / gRPC server holds.
        let live_site = cfg.microgrids().lock().get(&9).unwrap().site.clone();
        assert!(live_site.get(1).is_some());

        cfg.reload().expect("reload succeeds");

        // The re-eval'd topology landed on the SAME site: the
        // pre-reload handle sees the rebuilt component, and the
        // registry still carries exactly one entry for id 9.
        assert!(
            live_site.get(1).is_some(),
            "pre-reload site handle must see the rebuilt topology",
        );
        let reg = cfg.microgrids();
        let r = reg.lock();
        assert_eq!(r.len(), 1);
        assert!(r.get(&9).unwrap().site.get(1).is_some());
    }

    /// Write a managed microgrid file for `id` with one meter
    /// (`meter_id`) in its generated block and `script` as its
    /// hand-written section. Returns the path.
    fn managed_file(
        dir: &std::path::Path,
        id: u64,
        meter_id: u64,
        script: &str,
    ) -> std::path::PathBuf {
        let path = dir.join(format!("microgrids/{id}.lisp"));
        let block = format!(
            "(make-microgrid :id {id} :name \"m{id}\" :grpc-port {}\n  :topology\n  \
             (lambda ()\n    (%make-meter :id {meter_id})))",
            8800 + id as u16,
        );
        let text = super::super::microgrid_file::compose(&block, script);
        super::super::microgrid_file::write_atomic(&path, &text).unwrap();
        path
    }

    /// The script section of a managed file runs in ITS microgrid's
    /// scope, not in whatever scope the router happens to fall back
    /// to. With a lower-id microgrid already registered, the fallback
    /// would resolve to that one and the script's own component would
    /// come back "not found" — failing the load of a file that is
    /// perfectly correct.
    #[test]
    fn managed_script_section_runs_in_its_own_microgrids_scope() {
        let (cfg, dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
             (lambda () (%make-meter :id 1 :power-w 10.0)))",
        );
        // Component 50 exists only in microgrid 20; microgrid 9 is
        // the lower id, so it is what an unscoped script would hit.
        let path = managed_file(&dir, 20, 50, "(set-meter-power 50 1234.0)\n");
        cfg.load_file(&path).expect("the script runs in mg 20");
        cfg.refresh_once();
        let reg = cfg.microgrids();
        let r = reg.lock();
        let twenty = &r[&20].site;
        assert!(
            (twenty.get(50).unwrap().aggregate_power_w(twenty) - 1234.0).abs() < 1e-3,
            "mg 20's own meter carries the driven value",
        );
        let nine = &r[&9].site;
        assert!(
            (nine.get(1).unwrap().aggregate_power_w(nine) - 10.0).abs() < 1e-3,
            "mg 9 is untouched by mg 20's script",
        );
    }

    /// The same scoping holds on the reload path — `reload_file`
    /// re-runs the script section, so its drivers must land back on
    /// the file's own microgrid.
    #[test]
    fn reload_re_runs_the_script_section_in_scope() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let path = managed_file(&dir, 20, 50, "(set-meter-power 50 1234.0)\n");
        cfg.load_file(&path).unwrap();
        managed_file(&dir, 20, 50, "(set-meter-power 50 4321.0)\n");
        cfg.reload_file(&path)
            .expect("reload runs the script in scope");
        cfg.refresh_once();
        let reg = cfg.microgrids();
        let r = reg.lock();
        let twenty = &r[&20].site;
        assert!((twenty.get(50).unwrap().aggregate_power_w(twenty) - 4321.0).abs() < 1e-3);
    }

    /// Loading a file that is ALREADY loaded is a reload, not a
    /// second load: the load picker lists files that are typically
    /// live, and a plain re-eval would arm a second copy of every
    /// `every` block with no visible symptom but doubled animation.
    #[test]
    fn loading_an_already_loaded_file_does_not_double_arm_its_timers() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let other = dir.join("t.lisp");
        std::fs::write(
            &other,
            "(make-microgrid :id 10 :grpc-port 8801 :topology (lambda () nil))\n\
             (every :interval-s 0.001 :call (lambda () nil))",
        )
        .unwrap();
        let count = || -> i64 {
            cfg.eval_silent("(length active-timers)")
                .unwrap()
                .parse()
                .unwrap()
        };
        cfg.load_file(&other).unwrap();
        assert_eq!(count(), 1, "one timer after the first load");
        cfg.load_file(&other).unwrap();
        assert_eq!(count(), 1, "a second load must not stack the timer");
    }

    /// `reload_file` is per file: it rebuilds only the microgrids
    /// that file declares. Everything another file declared — live
    /// edits included — is left exactly as it was.
    #[test]
    fn reload_file_resets_only_that_files_microgrids() {
        let (cfg, dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
                                  (lambda () (%make-grid-connection-point :id 1)))",
        );
        let other = dir.join("other.lisp");
        std::fs::write(
            &other,
            "(make-microgrid :id 10 :grpc-port 8801 :topology \
                            (lambda () (%make-meter :id 50)))",
        )
        .unwrap();
        cfg.load_file(&other).unwrap();
        // Rename mg 9's component, then reload only other.lisp.
        cfg.eval_in_mg(9, "(rename-component 1 \"kept\")").unwrap();
        cfg.reload_file(&other).unwrap();
        let reg = cfg.microgrids();
        let r = reg.lock();
        assert_eq!(
            r[&9].site.display_name(1).as_deref(),
            Some("kept"),
            "mg 9 untouched by mg 10's reload"
        );
        assert!(r[&10].site.get(50).is_some());
    }

    /// Timers are tracked per source file, so reloading a file
    /// cancels the timers that file armed — and only those — before
    /// its forms arm them again. Without the per-file cancel every
    /// reload would leave a second copy of the file's `every` blocks
    /// running.
    #[test]
    fn reload_file_cancels_that_files_timers_only() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let other = dir.join("t.lisp");
        // The file arms a counter timer via `every`.
        std::fs::write(
            &other,
            "(make-microgrid :id 10 :grpc-port 8801 :topology (lambda () nil))\n\
                            (setq n 0)\n\
                            (every :interval-s 0.001 :call (lambda () (setq n (+ n 1))))",
        )
        .unwrap();
        cfg.load_file(&other).unwrap();
        let count = || -> i64 {
            cfg.eval_silent("(length active-timers)")
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(count(), 1, "one timer after the first load");
        cfg.reload_file(&other).unwrap();
        // The reload cancelled t.lisp's old timer before re-arming:
        // still one entry, not two. A second reload stays at one too.
        assert_eq!(count(), 1, "reload must not double-arm the file's timers");
        cfg.reload_file(&other).unwrap();
        assert_eq!(count(), 1);
    }

    /// A driver-only script — no `(make-microgrid …)`, just timers
    /// driving somebody else's world — reloads on its own like any
    /// other loaded file: its old timers are cancelled and its new
    /// ones armed, with no double-arming and no whole-world reset.
    #[test]
    fn reload_file_re_arms_a_driver_only_script() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let driver = dir.join("driver.lisp");
        std::fs::write(
            &driver,
            "(setq driver-mark 1)\n(every :interval-s 0.001 :call (lambda () nil))",
        )
        .unwrap();
        cfg.load_file(&driver).unwrap();
        let count = || -> i64 {
            cfg.eval_silent("(length active-timers)")
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(count(), 1, "the driver armed one timer");
        // It registers no microgrid, so it is not part of the
        // whole-world replay list …
        assert!(
            !cfg.registered_sources().iter().any(|p| p == &driver),
            "a driver script backs no microgrid"
        );
        // … but the loader saw it, so it is watched and reloadable.
        assert!(
            cfg.loader_visited_files()
                .iter()
                .any(|p| p.ends_with("driver.lisp")),
            "a driver script is still a file the loader ran"
        );
        // Edit and reload just this file: the new mark is live and
        // the timer count stays at one.
        std::fs::write(
            &driver,
            "(setq driver-mark 2)\n(every :interval-s 0.001 :call (lambda () nil))",
        )
        .unwrap();
        cfg.reload_file(&driver).unwrap();
        assert_eq!(cfg.eval_silent("driver-mark").unwrap(), "2");
        assert_eq!(count(), 1, "reload must not double-arm the driver's timer");
    }

    /// A managed file whose generated block succeeded and whose
    /// SCRIPT section then failed is a half-loaded file: the
    /// microgrid is registered and the script's timers are armed, so
    /// the loader has to record it even though the load reports an
    /// error. Otherwise the file is unwatched, and a retry misses the
    /// visited check and evaluates on top of itself — double-arming
    /// every `every` block it managed to run.
    #[test]
    fn a_failed_script_section_still_records_the_file_it_half_loaded() {
        use crate::lisp::microgrid_file::{compose, write_atomic};
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let path = dir.join("microgrids/50.lisp");
        write_atomic(
            &path,
            &compose(
                "(make-microgrid :id 50 :name \"s\" :grpc-port 8850\n  :topology\n  \
                 (lambda ()\n    (%make-meter :id 500)))",
                "(every :interval-s 1.0 :call (lambda () nil))\n\
                 (set-meter-power 999999 1.0)\n",
            ),
        )
        .unwrap();
        let count = || -> i64 {
            cfg.eval_silent("(length active-timers)")
                .unwrap()
                .parse()
                .unwrap()
        };

        let err = cfg.load_file(&path).expect_err("the script section fails");
        assert!(err.to_string().contains("999999"), "{err}");
        assert!(
            cfg.microgrids().lock().contains_key(&50),
            "the generated block ran: its microgrid is live"
        );
        let canonical = path.canonicalize().unwrap();
        assert!(
            cfg.loader_visited_files().contains(&canonical),
            "a half-loaded file is watched and reloadable"
        );
        assert_eq!(count(), 1, "the script armed one timer before failing");

        // The retry takes the reload path, which cancels this file's
        // timers before re-evaluating it.
        assert!(cfg.load_file(&path).is_err(), "it still fails");
        assert_eq!(count(), 1, "and it must not double-arm the timer");
    }

    /// The same rule for an UNMANAGED file, which has no generated
    /// block to succeed: a driver script that arms an `every` and
    /// then errors has already changed the world, so it is recorded
    /// too. Otherwise it is unwatched — saving a fix would not reload
    /// it — and a retry misses the visited check and arms the timer a
    /// second time.
    #[test]
    fn a_failed_unmanaged_file_that_armed_a_timer_is_still_recorded() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let driver = dir.join("driver.lisp");
        std::fs::write(
            &driver,
            "(every :interval-s 1.0 :call (lambda () nil))\n\
             (this-defun-does-not-exist 1)\n",
        )
        .unwrap();
        let count = || -> i64 {
            cfg.eval_silent("(length active-timers)")
                .unwrap()
                .parse()
                .unwrap()
        };

        let err = cfg.load_file(&driver).expect_err("the script errors");
        assert!(
            err.to_string().contains("this-defun-does-not-exist"),
            "{err}"
        );
        assert_eq!(count(), 1, "it armed a timer before failing");
        assert!(
            cfg.loader_visited_files()
                .iter()
                .any(|p| p.ends_with("driver.lisp")),
            "a partially-evaluated file is watched and reloadable"
        );

        assert!(cfg.load_file(&driver).is_err(), "it still fails");
        assert_eq!(count(), 1, "and it must not double-arm the timer");
    }

    /// Nothing evaluated, nothing recorded: a file the loader could
    /// not even read never ran a form, so it armed nothing and
    /// registered nothing.
    #[test]
    fn a_file_that_never_evaluated_is_not_recorded() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let missing = dir.join("no-such-file.lisp");
        assert!(cfg.load_file(&missing).is_err());
        assert!(
            !cfg.loader_visited_files()
                .iter()
                .any(|p| p.ends_with("no-such-file.lisp")),
            "a file that never ran is not part of the world"
        );
    }

    /// `load_as` deletes its copy when the load leaves nothing behind
    /// — an orphan file the next reload would trip over. But a load
    /// that registered a microgrid and then failed its script section
    /// OWNS that file: removing it would strand a live microgrid with
    /// no file to persist to, undo against, or snapshot.
    #[test]
    fn load_as_keeps_a_copy_whose_microgrid_registered() {
        use crate::lisp::microgrid_file::{compose, write_atomic};
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let src = dir.join("mg51.lisp");
        write_atomic(
            &src,
            &compose(
                "(make-microgrid :id 51 :name \"s\" :grpc-port 8851\n  :topology\n  \
                 (lambda ()\n    (%make-meter :id 510)))",
                "(set-meter-power 999999 1.0)\n",
            ),
        )
        .unwrap();

        let err = cfg.load_as(&src, 52).expect_err("the script section fails");
        // Typed, not sniffed: only load_as can tell a committed
        // partial from a call that copied nothing, so it says which.
        let crate::lisp::LoadAsError::CommittedPartial { id, cause } = &err else {
            panic!("expected a committed partial, got {err}");
        };
        assert_eq!(*id, 52, "and names the microgrid that DID come up");
        assert!(cause.contains("999999"), "{cause}");
        assert!(
            cfg.microgrids().lock().contains_key(&52),
            "the copy's generated block ran"
        );
        assert!(
            dir.join("microgrids/52.lisp").exists(),
            "so the copy stays on disk"
        );

        // The mirror: a copy whose block itself fails registers
        // nothing, so the file IS an orphan — deleted, and taken back
        // out of the loader's record, which the failed load put it in.
        // A leftover entry would leave the watcher and the reload list
        // naming a path that no longer exists.
        let broken = dir.join("mg53.lisp");
        write_atomic(
            &broken,
            &compose(
                "(make-microgrid :id 53 :name \"b\" :grpc-port 8853\n  :topology\n  \
                 (lambda ()\n    (this-defun-does-not-exist 1)))",
                "",
            ),
        )
        .unwrap();
        let target = dir.join("microgrids/54.lisp");
        let err = cfg.load_as(&broken, 54).expect_err("the block fails");
        assert!(
            matches!(err, crate::lisp::LoadAsError::Other(_)),
            "nothing registered, so nothing is committed: {err}"
        );
        assert!(!target.exists(), "the orphan copy is deleted");
        assert!(
            !cfg.loader_visited_files()
                .iter()
                .any(|p| p.ends_with("54.lisp")),
            "and the loader forgets it: {:?}",
            cfg.loader_visited_files()
        );
    }

    /// `enterprise.lisp` is what makes enterprise-wide state durable:
    /// an eval that touches the enterprise id or a `*-defaults` plist
    /// rewrites it, and every boot re-reads it before any microgrid
    /// file — so a component built in the NEXT process is built with
    /// the operator's defaults, not the built-in ones.
    #[test]
    fn enterprise_state_survives_a_restart() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        cfg.eval("(setq battery-defaults '(:capacity-wh 12345.0))")
            .unwrap();
        cfg.eval("(set-enterprise-id 77)").unwrap();
        cfg.eval("(set-default-augment-lifetime-ms 7000)").unwrap();

        // A second process on the same state dir — no boot scripts,
        // exactly what a bare `macrocosim --state-dir` does.
        let boot = |dir: std::path::PathBuf| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let built = rt.block_on(async { Config::new_with(&[], Some(dir)) });
            std::mem::forget(rt);
            built
        };
        let restarted = boot(dir.to_path_buf()).expect("second boot reads enterprise.lisp");
        assert_eq!(restarted.metadata().enterprise_id, 77);
        assert_eq!(
            restarted.metadata().default_augment_lifetime,
            std::time::Duration::from_millis(7000),
            "the augment-lifetime knob persists across a restart too"
        );
        restarted
            .eval(
                "(make-microgrid :id 6 :grpc-port 8806 :topology \
                 (lambda () (make-battery :id 300)))",
            )
            .unwrap();
        let site = restarted.microgrids().lock().get(&6).unwrap().site.clone();
        let telemetry = site.get(300).unwrap().telemetry(&site);
        assert_eq!(
            telemetry.capacity_wh,
            Some(12_345.0),
            "the persisted default built the component"
        );

        // And a broken enterprise.lisp fails the boot rather than
        // silently falling back to the built-in defaults.
        std::fs::write(
            dir.join("enterprise.lisp"),
            "(setq battery-defaults '(:capacity-wh",
        )
        .unwrap();
        assert!(
            boot(dir.to_path_buf()).is_err(),
            "a broken enterprise file must fail the boot"
        );
    }

    /// A broken `enterprise.lisp` aborts a whole-world reload BEFORE
    /// anything is reset: an undo click must not turn a typo in the
    /// settings file into an empty world.
    #[test]
    fn reload_leaves_the_world_alone_when_enterprise_is_broken() {
        let (cfg, _dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
             (lambda () (%make-grid-connection-point :id 1)))",
        );
        let live_site = cfg.microgrids().lock().get(&9).unwrap().site.clone();
        std::fs::write(cfg.enterprise_path(), "(this-defun-does-not-exist 1)").unwrap();

        let err = cfg
            .reload()
            .expect_err("a broken enterprise file must fail");
        assert!(err.contains("this-defun-does-not-exist"), "got: {err}");
        assert!(
            live_site.get(1).is_some(),
            "the running world must survive a failed reload untouched",
        );
    }

    /// Self-write suppression is one-shot: the first event whose
    /// content matches what macrocosim wrote is skipped, and the
    /// hash is forgotten — so an operator reverting the file to
    /// exactly that content by hand is a real edit again.
    #[test]
    fn self_write_suppression_is_one_shot() {
        let (cfg, dir) = config_with("nil");
        let path = dir.join("managed.lisp");
        cfg.record_self_write(&path, "body");
        assert!(
            cfg.take_self_write(&path, "body"),
            "our own save is recognised"
        );
        assert!(
            !cfg.take_self_write(&path, "body"),
            "the same content a second time is a human edit, not our save"
        );
        // Different content never matches in the first place.
        cfg.record_self_write(&path, "body");
        assert!(!cfg.take_self_write(&path, "edited by hand"));
    }

    /// A save recorded under a relative path (the default state dir
    /// is `.`, so writes go to `./enterprise.lisp`) is still
    /// recognised when the watcher asks with the canonical one.
    #[test]
    fn self_write_matches_a_relative_path() {
        let (cfg, dir) = config_with("nil");
        let canonical = dir.join("managed.lisp");
        std::fs::write(&canonical, "body").unwrap();
        let canonical = canonical.canonicalize().unwrap();
        // The same file spelled relative to the working directory.
        let cwd = std::env::current_dir().unwrap();
        let mut relative = std::path::PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.push(canonical.strip_prefix("/").unwrap());
        cfg.record_self_write(&relative, "body");
        assert!(cfg.take_self_write(&canonical, "body"));
    }
}
