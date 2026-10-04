//! Microgrids as files: list them, create one, import one from a
//! site export, load one from a file on disk (`/api/load`, plus
//! `/api/load-as` for a second copy under a free id), and adopt a
//! hand-written file so macrocosim may rewrite its structure.
//!
//! Create and import both mint a managed file and load it — the file
//! is the microgrid's declaration, so the registry entry always comes
//! from a load. `make-microgrid` announces the new microgrid during
//! the load, and the runtime listener starts it; create, import and
//! load-as then wait for that runtime and report it.

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::runtime::{MicrogridRuntimes, RuntimeView};

/// One `/api/microgrids` entry: the registry's view of the microgrid
/// plus its runtime, `null` when none was started.
#[derive(Serialize)]
pub(in crate::ui) struct MicrogridListEntry {
    #[serde(flatten)]
    view: crate::sim::microgrids::MicrogridView,
    runtime: Option<RuntimeView>,
}

pub(in crate::ui) async fn microgrids_list(
    State(config): State<Config>,
    axum::Extension(runtimes): axum::Extension<MicrogridRuntimes>,
) -> Json<Vec<MicrogridListEntry>> {
    Json(
        crate::sim::microgrids::snapshot(&config.microgrids())
            .into_iter()
            .map(|view| {
                let runtime = runtimes.status(view.id);
                MicrogridListEntry { view, runtime }
            })
            .collect(),
    )
}

/// Start `id`'s runtime and report it. Of this call and the
/// listener's, the one that runs second finds the runtime running and
/// only reads the status, or tries again after a failure. A start
/// failure is reported, not returned: the microgrid exists either
/// way.
pub(in crate::ui) async fn start_and_report(
    runtimes: &MicrogridRuntimes,
    id: u64,
) -> Option<RuntimeView> {
    if let Err(e) = runtimes.start(id).await {
        log::error!("Microgrid #{id}: {e}");
    }
    runtimes.status(id)
}

#[derive(Deserialize)]
pub(in crate::ui) struct CreateMicrogridBody {
    name: String,
    /// Microgrid id to claim. Omit to take the lowest free one.
    #[serde(default)]
    id: Option<u64>,
    /// gRPC port to bind. Omit to take the next free one.
    #[serde(default)]
    grpc_port: Option<u16>,
    #[serde(default)]
    tso: Option<String>,
}

#[derive(Serialize)]
pub(in crate::ui) struct CreateMicrogridResp {
    id: u64,
    name: String,
    grpc_port: u16,
    tso: Option<String>,
    /// Always true — create writes a macrocosim-generated file, so
    /// the new microgrid's structure is macrocosim's to rewrite.
    managed: bool,
    /// The new microgrid's runtime; `null` when none was started.
    runtime: Option<RuntimeView>,
}

/// POST /api/microgrids/create — auto-allocates id + grpc_port,
/// writes and loads a managed microgrid file with an empty topology,
/// then starts its runtime — physics + history + Microgrid gRPC
/// server + loopback client — through [`MicrogridRuntimes`] and
/// reports it. The runtime listener starts it too, on the load's
/// announcement; `start` is serialized and returns early for a
/// running runtime, so neither can double-boot it. A runtime that
/// fails to start is reported in a 200: the microgrid exists either
/// way.
///
/// Empty-name requests are rejected. `(make-microgrid …)` builds the
/// new site on the shared enterprise id allocator, so its
/// auto-allocated component ids stay globally unique.
pub(in crate::ui) async fn microgrids_create(
    State(config): State<Config>,
    axum::Extension(runtimes): axum::Extension<MicrogridRuntimes>,
    Json(body): Json<CreateMicrogridBody>,
) -> Result<Json<CreateMicrogridResp>, (StatusCode, String)> {
    let mut created = create_serialized(
        &config,
        &body.name,
        body.id,
        body.grpc_port,
        body.tso.as_deref(),
    )
    .await?;
    // The load registered the entry, so `start` finds it. Inert
    // runtimes (unit tests) start nothing and report `null`.
    created.runtime = start_and_report(&runtimes, created.id).await;
    Ok(Json(created))
}

/// [`create_core`] under the create lock, on the blocking pool.
///
/// The lock is what makes the id + port validation inside
/// `create_core` mean anything: the check reads the registry, but the
/// insert only happens later, when the freshly written file is
/// loaded, and no lock can span both (the load evaluates lisp). One
/// create at a time closes that window, so two concurrent creates
/// cannot pick the same id and collapse into one microgrid — the
/// second one sees the first in the registry and gets a 409.
///
/// The whole body is blocking work (file writes plus a lisp eval),
/// so it runs on the blocking pool with the lock's guard.
async fn create_serialized(
    config: &Config,
    name: &str,
    id: Option<u64>,
    grpc_port: Option<u16>,
    tso: Option<&str>,
) -> Result<CreateMicrogridResp, (StatusCode, String)> {
    let (name, tso) = (name.to_string(), tso.map(str::to_string));
    super::blocking_under_create_lock(config, move |cfg| {
        create_core(cfg, &name, id, grpc_port, tso.as_deref())
    })
    .await?
}

/// The shared create path: claims id + port, writes the managed
/// microgrid file, and loads it. The registry entry, its source and
/// its managed flag all come from the load — the file is the
/// microgrid's declaration, and nothing else may insert one. Leaves
/// `runtime` unset; the handler starts the runtime and fills it.
///
/// Callers must hold the create lock; [`create_serialized`] is the
/// only way in.
fn create_core(
    config: &Config,
    name: &str,
    want_id: Option<u64>,
    want_port: Option<u16>,
    tso: Option<&str>,
) -> Result<CreateMicrogridResp, (StatusCode, String)> {
    use crate::lisp::microgrid_file as file;
    use crate::sim::microgrids::{MicrogridDef, next_free_id_in, next_free_port_in};
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "name must be non-empty".into()));
    }
    // Id + port from one look at the registry: a requested one has to
    // be free, an omitted one is allocated. Both are still valid when
    // the load below inserts the entry, because the create lock keeps
    // any other create out in between.
    let reserved = config.reserved_ports();
    let registry = config.microgrids();
    let def = {
        let r = registry.lock();
        let id = match want_id {
            Some(id) if r.contains_key(&id) => {
                return Err((
                    StatusCode::CONFLICT,
                    format!("microgrid {id} is already registered"),
                ));
            }
            Some(id) => id,
            None => next_free_id_in(&r),
        };
        let grpc_port = match want_port {
            Some(0) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "grpc_port must be 1..=65535".into(),
                ));
            }
            Some(p) => {
                if let Some((other, _)) = r.iter().find(|(_, e)| e.def.grpc_port == p) {
                    return Err((
                        StatusCode::CONFLICT,
                        format!("gRPC port {p} is already bound by microgrid {other}"),
                    ));
                }
                if reserved.contains(&p) {
                    return Err((
                        StatusCode::CONFLICT,
                        format!("gRPC port {p} is reserved for the assets or dispatch server"),
                    ));
                }
                p
            }
            None => next_free_port_in(&r, &reserved),
        };
        MicrogridDef {
            id,
            name: name.clone(),
            grpc_port,
            tso: tso.map(str::to_string),
        }
    };
    let path = config.microgrids_dir().join(format!("{}.lisp", def.id));
    if path.exists() {
        return Err((
            StatusCode::CONFLICT,
            format!("{} already exists; refusing to clobber", path.display()),
        ));
    }
    let text = file::compose(&file::render_empty_block(&def), file::FRESH_SCRIPT_HEADER);
    file::write_atomic(&path, &text).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;
    config.record_self_write(&path, &text);
    // A file that fails to load leaves no live microgrid, so the copy
    // on disk would be an orphan the next reload trips over — drop it
    // and report the error.
    if let Err(e) = config.load_file(&path) {
        let _ = std::fs::remove_file(&path);
        let status = match e {
            crate::lisp::LoadError::Collision { .. } => StatusCode::CONFLICT,
            crate::lisp::LoadError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        return Err((status, e.to_string()));
    }
    Ok(CreateMicrogridResp {
        id: def.id,
        name: def.name,
        grpc_port: def.grpc_port,
        tso: def.tso,
        managed: true,
        runtime: None,
    })
}

#[derive(Deserialize)]
pub(in crate::ui) struct ImportMicrogridBody {
    name: String,
    /// Microgrid id to claim, the one the import dialog asks for.
    /// Omit to take the lowest free one, as every import did before
    /// the dialog existed — the field is additive.
    #[serde(default)]
    mid: Option<u64>,
    #[serde(default)]
    tso: Option<String>,
    /// The site export's components.json, verbatim.
    components: crate::sim::site_import::ComponentsFile,
    /// The site export's connections.json, verbatim (optional).
    #[serde(default)]
    connections: Option<crate::sim::site_import::ConnectionsFile>,
}

#[derive(Serialize)]
pub(in crate::ui) struct ImportMicrogridResp {
    id: u64,
    name: String,
    grpc_port: u16,
    tso: Option<String>,
    components: usize,
    connections: usize,
    /// The new microgrid's runtime; `null` when none was started.
    runtime: Option<RuntimeView>,
}

/// POST /api/microgrids/import — creates a REAL microgrid from a
/// site export: same allocate + file + boot path as create, then the
/// export's components rendered as one `(progn (make-* …) …
/// (connect …) …)` form evaluated against the new microgrid — the
/// same path a UI edit takes, so the eval regenerates the managed
/// file with the imported topology in it and every later boot loads
/// it back. Capacity, SoC bounds, rated power bounds and the grid's
/// rated fuse current all survive into the simulation.
///
/// Imported component ids are kept verbatim. Component ids are
/// enterprise-unique in macrocosim, so an id that any registered
/// microgrid already carries fails the whole import atomically —
/// nothing is created. The enterprise id allocator jumps past the
/// import's highest id so later auto-assigned ids can't collide.
///
/// The MICROGRID's own id is a separate matter: `mid` claims one
/// (the import dialog asks for it, pre-filled with the lowest free
/// one), and omitting it auto-allocates exactly as before. A `mid`
/// some other microgrid already holds is refused with create's own
/// 409, since claiming an id is create's job either way.
pub(in crate::ui) async fn microgrids_import(
    State(config): State<Config>,
    axum::Extension(runtimes): axum::Extension<MicrogridRuntimes>,
    Json(body): Json<ImportMicrogridBody>,
) -> Result<Json<ImportMicrogridResp>, (StatusCode, String)> {
    let import = crate::sim::site_import::parse(body.components, body.connections)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // One import at a time, from here to the eval that registers
    // the components. The collision check below is authoritative
    // only while no other import can add components between the
    // scan and this import's own eval — the make-* re-check at
    // replay is per site and cannot see a cross-site collision.
    // Parsing stays outside the lock; it touches no shared state.
    let import_lock = config.import_lock();
    let serialized = import_lock.lock().await;
    // Collision check against every registered site, plus the
    // bootstrap site legacy single-site configs run on.
    // `import.components` comes from a dedup-validated
    // parse, so the collected list is already sorted and unique.
    {
        // The TRUE bootstrap site: once a microgrid is registered,
        // legacy_site()/site() return the first registry entry, which
        // would leave components living on the bootstrap site (legacy
        // single-site configs) out of the collision scan.
        let bootstrap = config.bootstrap_site();
        // Snapshot the site handles under the lock, then scan with
        // the lock RELEASED: a big export means thousands of per-id
        // lookups, and holding the registry mutex through them
        // would stall every other registry user (create, listing,
        // the WS pump, the typed control endpoints).
        let sites: Vec<crate::sim::MicrogridSite> = {
            let registry = config.microgrids();
            let r = registry.lock();
            std::iter::once(bootstrap)
                .chain(r.values().map(|e| e.site.clone()))
                .collect()
        };
        let taken: Vec<u64> = import
            .components
            .iter()
            .map(|c| c.id)
            .filter(|id| sites.iter().any(|s| s.get(*id).is_some()))
            .collect();
        if !taken.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "component ids already exist in other microgrids: {taken:?} \
                     (component ids are enterprise-unique)"
                ),
            ));
        }
    }
    // The requested microgrid id (or None for the lowest free one)
    // goes through the create path's own claim: one place decides
    // whether an id is free, under the create lock that makes the
    // decision hold until the load inserts the entry.
    let created =
        create_serialized(&config, &body.name, body.mid, None, body.tso.as_deref()).await?;
    // Move the shared allocator past the imported ids before any
    // component is built, so nothing auto-allocates into that range.
    // Saturating: an export carrying id u64::MAX must not overflow
    // the bump (the allocator then sits at the ceiling, which only
    // affects auto-assigned ids, not the explicit imported ones).
    config.enterprise_id_allocator().fetch_max(
        import.max_id().saturating_add(1),
        std::sync::atomic::Ordering::SeqCst,
    );
    // Populate the (empty) new microgrid through the per-mg eval
    // path. eval_in_mg holds the interpreter lock across scope-set +
    // eval + overrides append, and the progn is one form, so the
    // whole topology lands atomically — and persists for later
    // boots. spawn_blocking because tulisp's lock is std-sync.
    let forms = import.forms();
    let cfg = config.clone();
    let id = created.id;
    let evaled = super::blocking(move || cfg.eval_in_mg(id, &forms)).await?;
    if let Err(e) = evaled {
        // The microgrid is already registered, so this cannot roll
        // back cleanly; the parse + collision checks above make this
        // a should-not-happen. Name the leftover so the user can act.
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "import failed while building components: {e} \
                 (microgrid {id} was created but is incomplete)"
            ),
        ));
    }
    // Starting the runtime binds a port and can be slow; the next
    // import need not wait for it.
    drop(serialized);
    Ok(Json(ImportMicrogridResp {
        id: created.id,
        name: created.name,
        grpc_port: created.grpc_port,
        tso: created.tso,
        components: import.components.len(),
        connections: import.connections.len(),
        runtime: start_and_report(&runtimes, created.id).await,
    }))
}

#[derive(Deserialize)]
pub(in crate::ui) struct LoadBody {
    /// Path of the file to load. Relative paths resolve against the
    /// state dir, the same anchor `(load …)` uses.
    path: String,
}

/// POST /api/load — evaluate a microgrid file and register whatever
/// microgrids it declares.
///
/// `loaded` lists the microgrids the file BACKS once it has run, not
/// just the ones it newly minted: re-loading a live file reuses its
/// entries in place, and reporting `[]` for that would read as "the
/// file did nothing". An genuinely empty list means the file ran and
/// registered nothing at all — legal (a driver-only script) but
/// worth saying out loud, since the load picker's job is to put
/// microgrids on screen.
///
/// The interesting failure is a collision: the file declares an id
/// some other file already loaded. That gets a 409 carrying the id,
/// whether the file is a managed one (only those can be re-idded
/// mechanically), and a free id to offer — so the UI can turn the
/// refusal into a "load it as N instead?" button that posts to
/// `/api/load-as`.
pub(in crate::ui) async fn load_file(
    State(config): State<Config>,
    Json(body): Json<LoadBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cfg = config.clone();
    let path = std::path::PathBuf::from(&body.path);
    let loaded = super::blocking(move || cfg.load_file(&path)).await?;
    match loaded {
        Ok(_) => {
            let ids = config.microgrids_backed_by(std::path::Path::new(&body.path));
            if ids.is_empty() {
                log::warn!(
                    "load {}: the file ran but registered no microgrid",
                    body.path
                );
            }
            Ok(Json(serde_json::json!({ "loaded": ids })))
        }
        Err(crate::lisp::LoadError::Collision { id }) => {
            let suggested = crate::sim::microgrids::next_free_id(&config.microgrids());
            Err(collision_response(&config, &body.path, id, suggested))
        }
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

/// The 409 body for a collision. `managed` decides which offer the
/// UI makes: a managed file can be re-idded by `/api/load-as`, a
/// hand-written one has to be edited by a person.
fn collision_response(
    config: &Config,
    path: &str,
    collision_id: u64,
    suggested_id: u64,
) -> (StatusCode, String) {
    let resolved = config.resolve_in_state_dir(std::path::Path::new(path));
    let managed = std::fs::read_to_string(&resolved)
        .ok()
        .and_then(|text| crate::lisp::microgrid_file::parse(&text).ok())
        .is_some_and(|parsed| parsed.generated.is_some());
    let body = serde_json::json!({
        "error": format!("microgrid {collision_id} is already loaded"),
        "collision_id": collision_id,
        "managed": managed,
        "suggested_id": suggested_id,
    });
    (StatusCode::CONFLICT, body.to_string())
}

#[derive(Deserialize)]
pub(in crate::ui) struct LoadAsBody {
    path: String,
    /// Id the copy is registered under.
    id: u64,
}

/// POST /api/load-as — copy a managed microgrid file under a free id
/// and load the copy, so one file can back two live microgrids. The
/// answer to the collision 409 above.
///
/// Three outcomes, not two. A clean load is `200 {id, runtime}`. A
/// load that left nothing behind (the copy was cleaned up) is the 409
/// the caller can retry under a different id. In between sits the
/// COMMITTED PARTIAL: the copy's generated block registered and its
/// script section then failed, so the microgrid is live and its file
/// is kept. That one is `200 {id, runtime, warning}` — reporting it
/// as a 409 would read as the collision code and invite a retry that
/// then hits "target exists", or mints a second copy of a microgrid
/// that already loaded. Both 200s carry the copy's runtime, `null`
/// when none was started.
pub(in crate::ui) async fn load_file_as(
    State(config): State<Config>,
    axum::Extension(runtimes): axum::Extension<MicrogridRuntimes>,
    Json(body): Json<LoadAsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = std::path::PathBuf::from(&body.path);
    let id = body.id;
    match super::blocking_under_create_lock(&config, move |cfg| cfg.load_as(&path, id)).await? {
        Ok(id) => Ok(Json(serde_json::json!({
            "id": id,
            "runtime": start_and_report(&runtimes, id).await,
        }))),
        // `load_as` typed this for us — it is the only thing that
        // knows whether the copy got as far as registering, so the
        // handler never has to ask the registry and never has to
        // guess which of several refusals it is looking at.
        Err(e) => match &e {
            crate::lisp::LoadAsError::CommittedPartial { id, .. } => Ok(Json(serde_json::json!({
                "id": id,
                "runtime": start_and_report(&runtimes, *id).await,
                "warning": e.to_string(),
            }))),
            crate::lisp::LoadAsError::Other(_) => Err((StatusCode::CONFLICT, e.to_string())),
        },
    }
}

/// POST /api/mg/{mg_id}/adopt — take a hand-written microgrid file
/// over, so macrocosim may rewrite its structure from then on.
///
/// The live structure is written as a generated block at the top of
/// the file and the original `(make-microgrid …)` form is commented
/// out below it, where it stays readable as a record of what the file
/// used to say. Everything else in the file — comments, `every`
/// blocks, defuns — is carried through untouched and keeps running.
///
/// A microgrid with no file at all (declared from the REPL) gets a
/// fresh `microgrids/{id}.lisp` instead.
pub(in crate::ui) async fn adopt_for_mg(
    State(config): State<Config>,
    axum::extract::Path(mg_id): axum::extract::Path<u64>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let warnings = super::blocking(move || adopt(&config, mg_id)).await??;
    Ok(Json(
        serde_json::json!({ "ok": true, "warnings": warnings }),
    ))
}

/// [`adopt_for_mg`]'s body: all blocking (file read + write) work.
/// Returns the warnings the caller should show.
fn adopt(config: &Config, mg_id: u64) -> Result<Vec<String>, (StatusCode, String)> {
    use crate::lisp::microgrid_file as file;

    let registry = config.microgrids();
    let (def, site, source, managed) = {
        let r = registry.lock();
        let e = r.get(&mg_id).ok_or((
            StatusCode::NOT_FOUND,
            format!("microgrid {mg_id} not registered"),
        ))?;
        (e.def.clone(), e.site.clone(), e.source.clone(), e.managed)
    };
    if managed {
        return Err((
            StatusCode::CONFLICT,
            format!("microgrid {mg_id} is already managed"),
        ));
    }
    // Live state the generated block cannot write down — a
    // lambda-bound input, or a value poked in at runtime that was
    // never a constructor argument. Reported, not refused: the
    // structure still round-trips, but these inputs come back at
    // their constructed values and have to be set again from the
    // script section.
    let warnings: Vec<String> = site
        .components()
        .iter()
        .filter(|c| c.has_unrenderable_source())
        .map(|c| {
            format!(
                "component {} ({}) carries an input value the generated block cannot \
                 write down — set it again from the script section",
                c.id(),
                c.make_fn()
            )
        })
        .collect();

    let block = file::render_block(&def, &site);
    let (path, text) = match source {
        Some(path) => {
            // One file, one microgrid: adopting rewrites the whole
            // file from ONE microgrid's live state, so a file that
            // declares several would lose the others.
            let sharers = registry
                .lock()
                .values()
                .filter(|e| e.source.as_deref() == Some(path.as_path()))
                .count();
            if sharers > 1 {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "{} declares {sharers} microgrids; split the file first, one \
                         microgrid per file",
                        path.display()
                    ),
                ));
            }
            let original = std::fs::read_to_string(&path).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("cannot read {}: {e}", path.display()),
                )
            })?;
            let script = comment_out_make_microgrid(&original, mg_id)
                .map_err(|e| (StatusCode::CONFLICT, format!("{}: {e}", path.display())))?;
            (path, file::compose(&block, &script))
        }
        // Nothing on disk backs this microgrid yet — give it the same
        // file the create endpoint would have written.
        None => {
            let path = config.microgrids_dir().join(format!("{mg_id}.lisp"));
            if path.exists() {
                return Err((
                    StatusCode::CONFLICT,
                    format!("{} already exists; refusing to clobber", path.display()),
                ));
            }
            (path, file::compose(&block, file::FRESH_SCRIPT_HEADER))
        }
    };
    file::write_atomic(&path, &text).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
    })?;
    // Our own write: the watcher must not read it back as a human
    // edit and reload the file underneath us.
    config.record_self_write(&path, &text);
    let canonical = path.canonicalize().unwrap_or(path);
    config.note_source_file(&canonical);
    if let Some(entry) = registry.lock().get_mut(&mg_id) {
        entry.source = Some(canonical);
        entry.managed = true;
        // The file now carries exactly what is live.
        entry.unsaved = false;
    }
    Ok(warnings)
}

/// Comment out the top-level `(make-microgrid …)` form for `mg_id` in
/// `text`, leaving every other line as it was. The generated block
/// composed above the result replaces what the form used to do, so
/// leaving it live would declare the microgrid twice.
///
/// Errors when the form cannot be found at top level — a file that
/// builds its microgrid some other way (inside a `let`, from a
/// helper defun) is not something adopt can mechanically supersede.
fn comment_out_make_microgrid(text: &str, mg_id: u64) -> Result<String, String> {
    use tulisp_fmt::cst::CstNode;

    let cst = tulisp_fmt::parse(text).map_err(|e| format!("failed to parse: {e:?}"))?;
    type ParsedId = Option<Result<u64, std::num::ParseIntError>>;
    let forms: Vec<(std::ops::Range<usize>, ParsedId)> = cst
        .nodes
        .iter()
        .filter_map(|n| {
            let CstNode::List { children, .. } = n else {
                return None;
            };
            let mut atoms = children.iter().filter_map(|c| match c {
                CstNode::Atom { text, .. } => Some(text.as_str()),
                _ => None,
            });
            if atoms.next() != Some("make-microgrid") {
                return None;
            }
            // Whatever the form's `:id` says, if it says anything.
            let declared = atoms
                .clone()
                .skip_while(|a| *a != ":id")
                .nth(1)
                .map(|v| v.parse::<u64>());
            Some((n.span(), declared))
        })
        .collect();
    let span = match forms.iter().find(|(_, id)| *id == Some(Ok(mg_id))) {
        Some((span, _)) => span.clone(),
        // One form with no explicit `:id` got an auto-allocated one,
        // and with a single form in the file that is ours.
        None if forms.len() == 1 && forms[0].1.is_none() => forms[0].0.clone(),
        None => {
            return Err(format!(
                "no top-level (make-microgrid …) form for microgrid {mg_id} found"
            ));
        }
    };
    // Start from the beginning of the form's line so a form indented
    // under a comment column still comments out cleanly.
    let start = text[..span.start].rfind('\n').map_or(0, |i| i + 1);
    let commented: String = text[start..span.end]
        .lines()
        .map(|l| format!(";; {l}\n"))
        .collect();
    Ok(format!(
        "{};; superseded by the generated block above:\n{}{}",
        &text[..start],
        commented,
        &text[span.end..]
    ))
}
