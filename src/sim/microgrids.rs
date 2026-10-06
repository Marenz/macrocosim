//! Enterprise-scoped microgrid registry — what the binary knows
//! about *every* microgrid it hosts. One entry per microgrid; each
//! entry pairs the configured metadata (id, display name, gRPC
//! port, optional TSO label) with a clonable handle on that
//! microgrid's `MicrogridSite`.
//!
//! The registry lives behind a `parking_lot::Mutex` because the
//! mutators (the `(make-microgrid …)` defun, the create-microgrid
//! HTTP endpoint) need write access from any tokio task that holds
//! the interpreter lock; the readers (the UI's Microgrids landing
//! page, the gRPC servers, the loopback supervisors, the scenarios
//! auto-advance loop) are short critical sections.
//!
//! Pure data + small helpers — no async, no I/O. The HTTP routing,
//! gRPC server spawning, and scenario per-microgrid replay sit one
//! layer up.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use crate::sim::MicrogridSite;

/// Canonical id used as the starting point when `make-microgrid` is
/// called without an explicit `:id`. The DSL's auto-allocator
/// (`next_free_id`) picks the lowest free id at or above this; tests
/// and the stock `config.lisp` pin 2200 explicitly.
pub const DEFAULT_MICROGRID_ID: u64 = 2200;
/// Default display name when `make-microgrid` is called without
/// `:name`.
pub const DEFAULT_MICROGRID_NAME: &str = "default";
/// Starting gRPC port for the auto-port allocator
/// (`next_free_port`). Multi-microgrid configs auto-allocate from
/// this base upward.
pub const DEFAULT_GRPC_PORT: u16 = 8800;

#[derive(Clone, Debug, Serialize)]
pub struct MicrogridDef {
    pub id: u64,
    pub name: String,
    pub grpc_port: u16,
    /// Optional informational TSO zone label (e.g. `"TN"`, `"AM"`,
    /// `"HZ"`, `"BW"`). No behavioural effect for v1; a chip on the
    /// Microgrids landing page surfaces it for operators that care.
    pub tso: Option<String>,
}

#[derive(Clone)]
pub struct MicrogridEntry {
    pub def: MicrogridDef,
    pub site: MicrogridSite,
    /// The file this microgrid was loaded from, canonicalized —
    /// `None` for one declared straight from the REPL (`/api/eval`),
    /// which has no file to persist back into. Two different files
    /// declaring the same id is a hard error; the SAME file
    /// re-declaring it is the reload path and reuses the entry in
    /// place. See `(make-microgrid …)` in `lisp::defuns::microgrids`.
    pub source: Option<PathBuf>,
    /// Whether [`source`](Self::source) is a *managed* microgrid file
    /// — one whose structure lives in a macrocosim-generated block
    /// (see [`crate::lisp::microgrid_file`]) that macrocosim may
    /// rewrite. A hand-written script is unmanaged: macrocosim loads
    /// it and never writes to it.
    pub managed: bool,
    /// Live edits have been applied that no file on disk carries: a
    /// save failed, the microgrid's file is unmanaged (macrocosim
    /// never writes to it), or it was declared straight from the
    /// REPL. Cleared by the next successful save.
    pub unsaved: bool,
}

/// Whether a microgrid's gRPC port is held by its runtime (starting,
/// or bound at least once). Installed by `MicrogridRuntimes`; absent
/// means nothing is pinned.
pub type PortPinCheck = Arc<dyn Fn(u64) -> bool + Send + Sync>;
/// The slot `Config` holds for the [`PortPinCheck`], set at most
/// once.
pub type PortPins = Arc<std::sync::OnceLock<PortPinCheck>>;

pub fn new_port_pins() -> PortPins {
    Arc::new(std::sync::OnceLock::new())
}

/// Ask the installed check; `false` when none is installed.
pub fn port_pinned(pins: &PortPins, id: u64) -> bool {
    pins.get().is_some_and(|check| check(id))
}

pub type SharedMicrogrids = Arc<Mutex<BTreeMap<u64, MicrogridEntry>>>;

pub fn new_registry() -> SharedMicrogrids {
    Arc::new(Mutex::new(BTreeMap::new()))
}

/// JSON-serialisable snapshot used by `GET /api/microgrids`. Drops
/// the `MicrogridSite` handle (not serialisable) and folds in a
/// component_count derived from the underlying site so the UI's
/// card grid can render counts without a second round-trip.
#[derive(Clone, Debug, Serialize)]
pub struct MicrogridView {
    pub id: u64,
    pub name: String,
    pub grpc_port: u16,
    pub tso: Option<String>,
    pub component_count: usize,
    /// Mirrors [`MicrogridEntry::managed`].
    pub managed: bool,
    /// Mirrors [`MicrogridEntry::source`], rendered for display
    /// (`PathBuf` has no stable JSON form across platforms).
    pub source: Option<String>,
    /// Mirrors [`MicrogridEntry::unsaved`].
    pub unsaved: bool,
}

impl From<&MicrogridEntry> for MicrogridView {
    fn from(e: &MicrogridEntry) -> Self {
        MicrogridView {
            id: e.def.id,
            name: e.def.name.clone(),
            grpc_port: e.def.grpc_port,
            tso: e.def.tso.clone(),
            component_count: e.site.component_count(),
            managed: e.managed,
            source: e.source.as_ref().map(|p| p.display().to_string()),
            unsaved: e.unsaved,
        }
    }
}

/// Snapshot the registry into a list of `MicrogridView`s, sorted
/// ascending by id. Driver for `GET /api/microgrids` + `macroctl
/// microgrids list`.
pub fn snapshot(registry: &SharedMicrogrids) -> Vec<MicrogridView> {
    registry.lock().values().map(MicrogridView::from).collect()
}

/// Why a microgrid cannot claim a gRPC port.
#[derive(Debug, PartialEq, Eq)]
pub enum PortClash {
    /// Another microgrid holds it.
    Microgrid(u64),
    /// It is the assets or dispatch server's port.
    Reserved,
}

impl std::fmt::Display for PortClash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Microgrid(other) => write!(f, "is already bound by microgrid {other}"),
            Self::Reserved => write!(f, "is reserved for the assets or dispatch server"),
        }
    }
}

impl std::error::Error for PortClash {}

/// Whether microgrid `id` may claim `port`: no other microgrid holds
/// it and it is not in `reserved`.
pub fn check_port(
    entries: &BTreeMap<u64, MicrogridEntry>,
    reserved: &[u16],
    id: u64,
    port: u16,
) -> Result<(), PortClash> {
    if let Some((other, _)) = entries
        .iter()
        .find(|(oid, e)| **oid != id && e.def.grpc_port == port)
    {
        return Err(PortClash::Microgrid(*other));
    }
    if reserved.contains(&port) {
        return Err(PortClash::Reserved);
    }
    Ok(())
}

/// Smallest port not claimed by a registered microgrid or listed in
/// `reserved`, starting at `DEFAULT_GRPC_PORT` and stepping by 10.
pub fn next_free_port(registry: &SharedMicrogrids, reserved: &[u16]) -> u16 {
    next_free_port_in(&registry.lock(), reserved)
}

/// Like [`next_free_port`] but operates on an already-locked map.
/// Lets the create-microgrid handler pick id + port + insert under
/// one critical section so two concurrent creates can't both pick the
/// same port. `reserved` holds the enterprise's assets and dispatch
/// ports.
pub fn next_free_port_in(entries: &BTreeMap<u64, MicrogridEntry>, reserved: &[u16]) -> u16 {
    let mut ports: Vec<u16> = entries
        .values()
        .map(|e| e.def.grpc_port)
        .chain(reserved.iter().copied())
        .collect();
    ports.sort_unstable();
    let mut candidate = DEFAULT_GRPC_PORT;
    for p in ports {
        if p == candidate {
            candidate = candidate.saturating_add(10);
        } else if p > candidate {
            break;
        }
    }
    candidate
}

/// Dynamic "current microgrid" pointer + registry lookup. Every
/// lisp setter / make-* defun captures a `SharedSiteRouter` and
/// calls `.site()` at the *moment of invocation* to find the
/// `MicrogridSite` it should act on:
///
///   1. If `current_microgrid` is set (via /api/mg/{id}/eval, the
///      scenarios per-microgrid replay, or the (make-microgrid)
///      body), and that id resolves to an entry in the registry,
///      return that entry's site.
///   2. Otherwise return the first registry entry's site
///      (BTreeMap min-id ordering), so single-microgrid configs
///      keep working without ever touching `current_microgrid`.
///   3. Otherwise fall back to the bootstrap site supplied at
///      Router construction time — covers the brief window
///      between `Config::new` allocating its initial site and
///      the config eval running its first `(make-microgrid …)`
///      form.
///
/// Holding a `SharedSiteRouter` is cheap (Arc clone); the inner
/// `RwLock` only contends with the rare write path that flips
/// `current_microgrid`.
pub struct SiteRouter {
    registry: SharedMicrogrids,
    current: Arc<RwLock<Option<u64>>>,
    bootstrap: MicrogridSite,
}

pub type SharedSiteRouter = Arc<SiteRouter>;

impl SiteRouter {
    pub fn new(
        registry: SharedMicrogrids,
        current: Arc<RwLock<Option<u64>>>,
        bootstrap: MicrogridSite,
    ) -> SharedSiteRouter {
        Arc::new(Self {
            registry,
            current,
            bootstrap,
        })
    }

    /// Which registered microgrid — if any — already carries the
    /// component id `id`. Component ids are enterprise-unique, so the
    /// `make-*` defuns consult this before accepting an explicit
    /// `:id`: a duplicate across microgrids would make gRPC lookups
    /// and the UI's cross-microgrid views ambiguous.
    ///
    /// The caller's own site is registered too, but it is checked for
    /// the id first (and errors there with a site-local message), so
    /// by the time this runs a hit can only be a *foreign* microgrid.
    pub fn owner_of_component(&self, id: u64) -> Option<u64> {
        self.registry
            .lock()
            .iter()
            .find(|(_, e)| e.site.get(id).is_some())
            .map(|(mg, _)| *mg)
    }

    /// Resolve the active site under the rules above. Cheap clone
    /// of the underlying `MicrogridSite` (`Arc<MicrogridSiteInner>`
    /// inside).
    pub fn site(&self) -> MicrogridSite {
        if let Some(id) = *self.current.read()
            && let Some(entry) = self.registry.lock().get(&id)
        {
            return entry.site.clone();
        }
        if let Some(entry) = self.registry.lock().values().next() {
            return entry.site.clone();
        }
        self.bootstrap.clone()
    }

    /// The bootstrap site supplied at construction time, regardless
    /// of registry state. For callers that must cover components
    /// living outside every registered microgrid (the import
    /// collision scan): legacy single-site configs run on this site,
    /// and `site()` does not return it once a microgrid is
    /// registered.
    pub fn bootstrap_site(&self) -> MicrogridSite {
        self.bootstrap.clone()
    }
}

/// Shared `Arc<RwLock<Option<u64>>>` carrying the active microgrid
/// id. Lifted out so callers (HTTP handlers, scenario tick) can
/// flip it via `with_microgrid`.
pub type CurrentMicrogrid = Arc<RwLock<Option<u64>>>;

pub fn new_current_microgrid() -> CurrentMicrogrid {
    Arc::new(RwLock::new(None))
}

/// Run `f` with `current_microgrid` temporarily set to `id`, then
/// restore the prior value — even when `f` panics (a Drop guard does
/// the restore, so an unwinding eval can't leave the pointer stuck
/// on a temporary scope).
///
/// The pointer is ambient global state that every scoped defun
/// reads at invocation time, so the flip
/// is only race-free while no other interpreter work can run:
/// callers MUST hold the interpreter lock across this call. The
/// sanctioned entry points — `Config::scoped` / `eval_in_mg` and the
/// scenario layer's `funcall_in_mg` — all lock first; `:topology`
/// funcalls inside `make-microgrid` run under the lock the
/// surrounding eval already holds.
pub fn with_microgrid<R>(current: &CurrentMicrogrid, id: u64, f: impl FnOnce() -> R) -> R {
    struct Restore<'a> {
        current: &'a CurrentMicrogrid,
        prior: Option<u64>,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            *self.current.write() = self.prior;
        }
    }
    let prior = current.write().replace(id);
    let _restore = Restore { current, prior };
    f()
}

/// The file currently being loaded, as seen by every
/// `(make-microgrid …)` form that runs during that load. Ambient
/// state in the same sense as [`CurrentMicrogrid`] — the loader sets
/// it, the defun reads it at invocation time — so the same rule
/// applies: only flip it while the interpreter lock is held.
#[derive(Clone)]
pub struct LoadingFile {
    /// Canonicalized path of the file being loaded. Compared against
    /// [`MicrogridEntry::source`] to tell a same-file reload (reuse
    /// in place) from a second file claiming a taken id (an error).
    pub path: PathBuf,
    /// Whether that file is a managed microgrid file — see
    /// [`MicrogridEntry::managed`].
    pub managed: bool,
}

pub type LoadingSlot = Arc<Mutex<Option<LoadingFile>>>;

pub fn new_loading_slot() -> LoadingSlot {
    Arc::new(Mutex::new(None))
}

/// Run `f` with the loading-file slot set to `file`, restoring the
/// prior value on the way out — panics included (a Drop guard does
/// the restore, mirroring [`with_microgrid`]).
///
/// Save-and-restore rather than set-and-clear so a load triggered
/// from inside another load attributes its microgrids to itself and
/// hands the outer file back afterwards. A nested `(load …)` form
/// goes through tulisp's own loader rather than this one, so it
/// simply keeps the outer file ambient — its microgrids belong to
/// the file the operator actually loaded.
pub fn with_loading<R>(slot: &LoadingSlot, file: LoadingFile, f: impl FnOnce() -> R) -> R {
    struct Restore<'a> {
        slot: &'a LoadingSlot,
        prior: Option<LoadingFile>,
    }
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            *self.slot.lock() = self.prior.take();
        }
    }
    let prior = slot.lock().replace(file);
    let _restore = Restore { slot, prior };
    f()
}

/// Smallest microgrid id not currently registered, starting at
/// `DEFAULT_MICROGRID_ID`. Mirrors `next_free_port`'s shape.
pub fn next_free_id(registry: &SharedMicrogrids) -> u64 {
    next_free_id_in(&registry.lock())
}

/// Like [`next_free_id`] but operates on an already-locked map.
/// Pairs with [`next_free_port_in`] so the create-microgrid path
/// can pick both + insert in one critical section.
pub fn next_free_id_in(entries: &BTreeMap<u64, MicrogridEntry>) -> u64 {
    let mut ids: Vec<u64> = entries.keys().copied().collect();
    ids.sort_unstable();
    let mut candidate = DEFAULT_MICROGRID_ID;
    for id in ids {
        if id == candidate {
            candidate = candidate.saturating_add(1);
        } else if id > candidate {
            break;
        }
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::MicrogridSite;

    fn entry(id: u64, port: u16) -> MicrogridEntry {
        MicrogridEntry {
            def: MicrogridDef {
                id,
                name: format!("mg-{id}"),
                grpc_port: port,
                tso: None,
            },
            site: MicrogridSite::new(),
            source: None,
            managed: false,
            unsaved: false,
        }
    }

    #[test]
    fn next_free_port_increments_by_10_skipping_gaps() {
        let reg = new_registry();
        // Empty registry -> default port.
        assert_eq!(next_free_port(&reg, &[]), DEFAULT_GRPC_PORT);
        // Add 8800; next is 8810.
        reg.lock().insert(2200, entry(2200, 8800));
        assert_eq!(next_free_port(&reg, &[]), 8810);
        // Add 8810 too; next is 8820.
        reg.lock().insert(2201, entry(2201, 8810));
        assert_eq!(next_free_port(&reg, &[]), 8820);
        // Add 8830 (gap); next slot is still 8820.
        reg.lock().insert(2202, entry(2202, 8830));
        assert_eq!(next_free_port(&reg, &[]), 8820);
    }

    /// Reserved ports (the assets and dispatch sockets) are never
    /// handed out: the walk steps over them like a taken port.
    #[test]
    fn next_free_port_skips_reserved_ports() {
        let reg = new_registry();
        assert_eq!(next_free_port(&reg, &[DEFAULT_GRPC_PORT]), 8810);
        assert_eq!(next_free_port(&reg, &[8810]), DEFAULT_GRPC_PORT);
        assert_eq!(next_free_port(&reg, &[DEFAULT_GRPC_PORT, 8810]), 8820);
    }

    #[test]
    fn next_free_id_starts_at_default_and_increments() {
        let reg = new_registry();
        assert_eq!(next_free_id(&reg), DEFAULT_MICROGRID_ID);
        reg.lock()
            .insert(DEFAULT_MICROGRID_ID, entry(DEFAULT_MICROGRID_ID, 8800));
        assert_eq!(next_free_id(&reg), DEFAULT_MICROGRID_ID + 1);
    }

    #[test]
    fn snapshot_is_ascending_by_id() {
        let reg = new_registry();
        for id in [2202u64, 2200, 2201] {
            reg.lock()
                .insert(id, entry(id, 8800 + (id as u16 - 2200) * 10));
        }
        let view = snapshot(&reg);
        assert_eq!(
            view.iter().map(|v| v.id).collect::<Vec<_>>(),
            vec![2200, 2201, 2202]
        );
    }
}
