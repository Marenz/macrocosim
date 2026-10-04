//! `(make-microgrid)`, the `current-microgrid-id` / `microgrid-name`
//! accessors, and the `set-microgrid-name` / `set-microgrid-tso`
//! setters that edit a live microgrid's definition. Each
//! `(make-microgrid …)` form mints a fresh `MicrogridSite`, inserts a
//! registry entry, flips the `CurrentMicrogrid` pointer, and funcalls
//! the `:topology` lambda so nested make-* forms register into the
//! new site.

use std::sync::Arc;

use tokio::sync::broadcast;
use tulisp::{TulispContext, TulispObject};

use crate::sim::MicrogridSite;

tulisp::AsPlist! {
    pub struct MakeMicrogridArgs {
        name: Option<String> {= None},
        id: Option<i64> {= None},
        grpc_port<":grpc-port">: Option<i64> {= None},
        /// Optional TSO zone label (informational; see
        /// `crate::sim::microgrids::MicrogridDef::tso`).
        tso: Option<String> {= None},
        /// Zero-arg lambda whose body builds the microgrid's
        /// topology — typically a single nested
        /// `(make-grid-connection-point …)` call. The lambda
        /// form is required (not a plain expression) because the
        /// body must evaluate *after* make-microgrid has set the
        /// current-microgrid pointer, so the nested make-* calls
        /// register into the new site instead of the previously-
        /// active one. Optional so a config can register an empty
        /// microgrid that the UI fills in component-by-component.
        topology: Option<crate::lisp::value::LispValue> {= None},
    }
}

/// Register `(make-microgrid …)`. Each call creates a fresh
/// `MicrogridSite`, inserts a registry entry for it, sets the
/// `CurrentMicrogrid` pointer, funcalls the `:topology` lambda
/// (whose body's make-* calls then register into the new site
/// via the router's per-call dispatch), and finally restores the
/// previous pointer.
#[expect(
    clippy::too_many_arguments,
    reason = "one installer wires every shared handle"
)]
pub(in crate::lisp) fn register(
    ctx: &mut TulispContext,
    registry: crate::sim::microgrids::SharedMicrogrids,
    current: crate::sim::microgrids::CurrentMicrogrid,
    id_allocator: Arc<std::sync::atomic::AtomicU64>,
    registered_tx: Arc<broadcast::Sender<u64>>,
    grid_frequency: crate::sim::frequency::SharedFrequency,
    loading: crate::sim::microgrids::LoadingSlot,
    metadata: Arc<parking_lot::RwLock<crate::lisp::Metadata>>,
    port_pins: crate::sim::microgrids::PortPins,
    now: crate::sim::sim_clock::NowSource,
) {
    // `(current-source-file)` — the file whose load is in flight, or
    // nil outside a load (a REPL eval). Scripts use it to resolve
    // sibling data files, and to tell "I was loaded" from "I was
    // typed".
    {
        let slot = loading.clone();
        ctx.defun(
            "current-source-file",
            move || -> Result<TulispObject, tulisp::Error> {
                Ok(match slot.lock().as_ref() {
                    Some(f) => TulispObject::from(f.path.display().to_string()),
                    None => TulispObject::nil(),
                })
            },
        );
    }
    // Read-only accessors scripts use to dispatch on the active
    // microgrid. Outside a per-mg
    // context (e.g. boot before any (make-microgrid) form, or a
    // legacy /api/eval call without an mg scope) they fall back
    // to the first registry entry so single-microgrid configs
    // keep returning sensible values.
    {
        let cur = current.clone();
        let reg = registry.clone();
        ctx.defun(
            "current-microgrid-id",
            move || -> Result<i64, tulisp::Error> {
                if let Some(id) = *cur.read() {
                    return Ok(id as i64);
                }
                let r = reg.lock();
                Ok(r.keys().next().copied().unwrap_or(0) as i64)
            },
        );
    }
    {
        let cur = current.clone();
        let reg = registry.clone();
        ctx.defun(
            "microgrid-name",
            move || -> Result<String, tulisp::Error> {
                let id_opt = *cur.read();
                let r = reg.lock();
                let entry = id_opt
                    .and_then(|id| r.get(&id))
                    .or_else(|| r.values().next());
                Ok(entry.map(|e| e.def.name.clone()).unwrap_or_default())
            },
        );
    }
    // Structural edits to the microgrid's own definition — the two
    // `(make-microgrid …)` head arguments a person changes after the
    // fact. (There is deliberately no `:grpc-port` setter: the port
    // is pinned by a listening gRPC server, so moving it needs an
    // unload, which is a later sub-project.)
    //
    // Both bump the site's STRUCTURAL version even though no
    // component moved: that counter is what `Config::eval` diffs to
    // decide a microgrid's managed file needs rewriting, and the head
    // these setters edit lives in that file.
    {
        let reg = registry.clone();
        ctx.defun(
            "set-microgrid-name",
            move |id: i64, name: String| -> Result<bool, tulisp::Error> {
                let mut r = reg.lock();
                let entry = r.get_mut(&(id as u64)).ok_or_else(|| {
                    tulisp::Error::invalid_argument(format!(
                        "set-microgrid-name: no microgrid with id {id}"
                    ))
                })?;
                entry.def.name = name;
                entry.site.bump_structural_version();
                Ok(true)
            },
        );
    }
    {
        let reg = registry.clone();
        ctx.defun(
            "set-microgrid-tso",
            move |id: i64, tso: TulispObject| -> Result<bool, tulisp::Error> {
                // nil clears the label; anything else must be a
                // string (the TSO zone is free-form text).
                let tso = if tso.null() {
                    None
                } else {
                    Some(String::try_from(tso)?)
                };
                let mut r = reg.lock();
                let entry = r.get_mut(&(id as u64)).ok_or_else(|| {
                    tulisp::Error::invalid_argument(format!(
                        "set-microgrid-tso: no microgrid with id {id}"
                    ))
                })?;
                entry.def.tso = tso;
                entry.site.bump_structural_version();
                Ok(true)
            },
        );
    }
    use crate::sim::microgrids::{
        DEFAULT_MICROGRID_NAME, MicrogridDef, MicrogridEntry, check_port, next_free_id_in,
        next_free_port_in, with_microgrid,
    };
    ctx.defun(
        "make-microgrid",
        move |ctx: &mut TulispContext,
              args: tulisp::Plist<MakeMicrogridArgs>|
              -> Result<i64, tulisp::Error> {
            let a = args.into_inner();
            // `as u16` would silently wrap ports > 65535 onto a
            // different port — validate the range up front.
            if let Some(p) = a.grpc_port
                && !(1..=65535).contains(&p)
            {
                return Err(tulisp::Error::invalid_argument(format!(
                    "make-microgrid: :grpc-port {p} outside 1..=65535"
                )));
            }
            let name = a
                .name
                .clone()
                .unwrap_or_else(|| DEFAULT_MICROGRID_NAME.to_string());
            // Which file (if any) is being loaded right now. A
            // microgrid belongs to that file; one declared from the
            // REPL belongs to nothing.
            let loading_file = loading.lock().clone();
            // Re-registering an id that's already in the registry is
            // only legal from the file that owns it — the reload path
            // re-evaluating its own `(make-microgrid …)` form. That
            // REUSES the existing entry's site, reset in place: the
            // running physics + history tasks, the per-port gRPC
            // server, and the loopback client all hold that site
            // handle, and minting a fresh one would orphan every
            // runtime (the old site would keep ticking and serving
            // gRPC while the registry acts on a site that never
            // ticks). Name / tso update live; the gRPC port is kept
            // while its runtime holds it (see `port_pinned`);
            // otherwise a changed :grpc-port is adopted.
            //
            // Any OTHER claimant — a second file, or a REPL form — is
            // a hard error naming the owner, because silently merging
            // two unrelated definitions onto one site is how a
            // "loaded" microgrid ends up being neither file's.
            //
            // One registry lock for the id probe, the port probe, the
            // reuse check, AND the insert — separate acquisitions let
            // a concurrent /api/microgrids/create hand out the same
            // id or port between our probe and our insert.
            //
            // The assets and dispatch ports, read before the registry
            // lock so the two locks never nest.
            let reserved = crate::lisp::reserved_ports_of(&metadata.read());
            let (id, reused) = {
                let mut reg = registry.lock();
                let id = match a.id {
                    Some(v) if v > 0 => v as u64,
                    // Reject negative ids instead of silently
                    // auto-allocating: a typo like :id -2200 would
                    // otherwise boot under an auto id with the wrong
                    // managed file and gRPC port, no diagnostic.
                    // (:id 0 stays the documented auto sentinel.)
                    Some(v) if v < 0 => {
                        return Err(tulisp::Error::invalid_argument(format!(
                            "make-microgrid: :id {v} must not be negative"
                        )));
                    }
                    _ => next_free_id_in(&reg),
                };
                let existing = reg
                    .get(&id)
                    .map(|e| (e.def.grpc_port, e.site.clone(), e.source.clone()));
                let (grpc_port, site, reused) = match existing {
                    Some((bound_port, site, source)) => {
                        // Same file re-declaring its own microgrid?
                        // Both sides carry canonicalized paths, so a
                        // relative and an absolute spelling of one
                        // file compare equal.
                        let same_file = match (&loading_file, &source) {
                            (Some(l), Some(s)) => l.path == *s,
                            _ => false,
                        };
                        if !same_file {
                            let owner = match &source {
                                Some(p) => format!("from {}", p.display()),
                                None => "from the REPL".to_string(),
                            };
                            return Err(tulisp::Error::invalid_argument(format!(
                                "microgrid {id} is already loaded ({owner})"
                            )));
                        }
                        let grpc_port = match a.grpc_port {
                            Some(p) if p as u16 != bound_port => {
                                let p = p as u16;
                                if crate::sim::microgrids::port_pinned(&port_pins, id) {
                                    log::warn!(
                                        "make-microgrid #{id}: :grpc-port {p} ignored — its \
                                         gRPC server holds :{bound_port} (restart to move it)"
                                    );
                                    bound_port
                                } else {
                                    check_port(&reg, &reserved, id, p).map_err(|c| {
                                        tulisp::Error::invalid_argument(format!(
                                            "make-microgrid #{id}: :grpc-port {p} {c}"
                                        ))
                                    })?;
                                    p
                                }
                            }
                            Some(_) | None => bound_port,
                        };
                        site.reset();
                        (grpc_port, site, true)
                    }
                    None => {
                        let grpc_port = match a.grpc_port {
                            Some(p) => {
                                let p = p as u16;
                                // A collision registers cleanly here
                                // but the runtimes then fail the bind
                                // and mark this microgrid failed,
                                // retried only on its next
                                // registration. Reject up front,
                                // under the same lock as the insert.
                                check_port(&reg, &reserved, id, p).map_err(|c| {
                                    tulisp::Error::invalid_argument(format!(
                                        "make-microgrid #{id}: :grpc-port {p} {c}"
                                    ))
                                })?;
                                p
                            }
                            None => next_free_port_in(&reg, &reserved),
                        };
                        // Fresh site per microgrid that shares the
                        // enterprise's id allocator with the bootstrap site
                        // + every other microgrid — component ids stay
                        // globally unique across the registry without
                        // per-site coordination.
                        let site = MicrogridSite::with_id_allocator(id_allocator.clone());
                        // Same grid frequency source as every other site,
                        // so their `frequency_hz` reads all return the same
                        // OU value (one AC grid → one frequency).
                        site.set_grid_frequency(grid_frequency.clone());
                        // Same clock as the rest of the config: wall
                        // time live, the sim clock headless.
                        site.set_now_source(now.clone());
                        (grpc_port, site, false)
                    }
                };
                let def = MicrogridDef {
                    id,
                    name,
                    grpc_port,
                    tso: a.tso.clone(),
                };
                reg.insert(
                    id,
                    MicrogridEntry {
                        def,
                        site: site.clone(),
                        source: loading_file.as_ref().map(|l| l.path.clone()),
                        managed: loading_file.as_ref().is_some_and(|l| l.managed),
                        // A file just (re)declared the microgrid, so
                        // disk and memory agree by construction.
                        unsaved: false,
                    },
                );
                (id, reused)
            };
            // Funcall the :topology lambda (if any) with the
            // current-microgrid pointer flipped to this new entry, so
            // the nested make-* calls register into the new site.
            // This runs BEFORE the registered notification: a failing
            // topology must not leave a zombie microgrid announced to
            // subscribers — a fresh entry is removed again on error
            // (a reused one stays; its reset site is the same state a
            // failed reload leaves).
            if let Some(topology) = a.topology {
                let lambda = topology.into_inner();
                if !lambda.null() {
                    let nil = TulispObject::nil();
                    if let Err(e) = with_microgrid(&current, id, || ctx.funcall(&lambda, &nil)) {
                        if !reused {
                            registry.lock().remove(&id);
                        }
                        return Err(e);
                    }
                }
            }
            // Notify enterprise-wide subscribers (the WS event pump
            // and the runtime listener). A reused registration is
            // announced too, so a failed runtime gets another start;
            // both subscribers ignore an id they already serve.
            let _ = registered_tx.send(id);
            Ok(id as i64)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;

    /// The assets and dispatch servers' ports, read from their
    /// socket addresses.
    fn server_ports(cfg: &crate::lisp::Config) -> [u16; 2] {
        [cfg.assets_socket_addr(), cfg.dispatch_socket_addr()]
            .map(|a| a.parse::<std::net::SocketAddr>().unwrap().port())
    }

    /// An explicit :grpc-port on the assets or dispatch server's port
    /// is refused, as another microgrid's would be.
    #[test]
    fn make_microgrid_refuses_a_reserved_port() {
        let (cfg, _dir) = config_with("nil");
        for port in server_ports(&cfg) {
            let err = cfg
                .eval(&format!(
                    "(make-microgrid :id 77 :grpc-port {port} :topology (lambda () nil))"
                ))
                .expect_err("a reserved port must be refused");
            assert!(err.contains("reserved"), "unexpected error: {err}");
        }
    }

    /// A second FILE claiming an id another file already loaded is a
    /// hard error, and the message names the owning file so the
    /// operator knows which one to edit.
    #[test]
    fn loading_a_second_file_with_a_taken_id_errors() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let other = dir.join("other.lisp");
        std::fs::write(
            &other,
            "(make-microgrid :id 9 :grpc-port 8801 :topology (lambda () nil))",
        )
        .unwrap();
        // Typed, not a sentence to grep: the load endpoint offers
        // "load it as a free id instead?" off the id.
        let err = cfg.load_file(&other).unwrap_err();
        assert_eq!(err, crate::lisp::LoadError::Collision { id: 9 });
    }

    /// Re-loading the SAME file keeps the reuse-in-place semantics the
    /// live runtimes depend on: same site, reset and rebuilt.
    #[test]
    fn reloading_the_same_file_reuses_the_entry_in_place() {
        let (cfg, dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
             (lambda () (%make-grid-connection-point :id 1)))",
        );
        let live = cfg.microgrids().lock().get(&9).unwrap().site.clone();
        let path = dir.join("config.lisp");
        cfg.load_file(&path).expect("same-file reload allowed");
        assert!(live.get(1).is_some(), "same site, rebuilt in place");
    }

    /// The REPL has no loading file, so re-declaring a loaded id from
    /// an eval collides too — and says the id came from a file.
    #[test]
    fn repl_make_microgrid_with_taken_id_errors() {
        let (cfg, _dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let err = cfg
            .eval("(make-microgrid :id 9 :grpc-port 8801 :topology (lambda () nil))")
            .unwrap_err();
        assert!(err.contains("already loaded"), "{err}");
        assert!(
            err.contains("config.lisp"),
            "error names the owning file: {err}"
        );
    }

    /// Component ids are enterprise-unique: a second microgrid reusing
    /// a component id fails its load, leaving nothing registered.
    #[test]
    fn component_id_collisions_across_microgrids_fail_the_load() {
        let (cfg, dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
             (lambda () (%make-meter :id 42)))",
        );
        let other = dir.join("other.lisp");
        std::fs::write(
            &other,
            "(make-microgrid :id 10 :grpc-port 8801 :topology \
             (lambda () (%make-meter :id 42)))",
        )
        .unwrap();
        let err = cfg.load_file(&other).unwrap_err().to_string();
        assert!(err.contains("42"), "{err}");
        assert!(
            !cfg.microgrids().lock().contains_key(&10),
            "nothing registered"
        );
    }

    /// `load_as` copies a managed file under a free id, rewriting the
    /// id inside the generated block; unmanaged files are refused.
    #[test]
    fn load_as_copies_and_rewrites_the_id() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let src = dir.join("managed.lisp");
        std::fs::write(
            &src,
            crate::lisp::microgrid_file::compose(
                "(make-microgrid :id 9 :name \"m\" :grpc-port 8890\n  :topology\n  (lambda ()\n    nil))",
                "",
            ),
        )
        .unwrap();
        let id = cfg.load_as(&src, 11).expect("load as free id");
        assert_eq!(id, 11);
        assert!(cfg.microgrids().lock().contains_key(&11));
        assert!(dir.join("microgrids/11.lisp").exists());
        // Unmanaged files are refused.
        let raw = dir.join("raw.lisp");
        std::fs::write(&raw, "(make-microgrid :id 9 :topology (lambda () nil))").unwrap();
        assert!(cfg.load_as(&raw, 12).is_err());
    }

    /// The case load-as exists for: copying a file that is ALREADY
    /// loaded. Its port is held by the original's gRPC server, so the
    /// copy has to get its own — otherwise every "load as N" from the
    /// UI's collision offer dies on ":grpc-port … is already bound".
    #[test]
    fn load_as_gives_the_copy_its_own_port() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        // A managed file for the LIVE microgrid 9, port and all —
        // what the create endpoint writes and what the load picker
        // hands back.
        let src = dir.join("mg9.lisp");
        std::fs::write(
            &src,
            crate::lisp::microgrid_file::compose(
                "(make-microgrid :id 9 :name \"m\" :grpc-port 8800\n  :topology\n  \
                 (lambda ()\n    (%make-meter :id 77)))",
                "",
            ),
        )
        .unwrap();
        let id = cfg.load_as(&src, 11).expect("copy loads under a free id");
        assert_eq!(id, 11);
        let reg = cfg.microgrids();
        let r = reg.lock();
        let original = r.get(&9).expect("original still registered");
        let copy = r.get(&11).expect("copy registered");
        assert_eq!(original.def.grpc_port, 8800, "the original keeps its port");
        assert_ne!(
            copy.def.grpc_port, original.def.grpc_port,
            "the copy binds a port of its own"
        );
        // The topology comes across, under ids of its own — see
        // `load_as_remints_component_ids_of_a_live_original`.
        let copied: Vec<u64> = copy.site.components().iter().map(|c| c.id()).collect();
        assert_eq!(copied.len(), 1, "the copy carries the topology");
        assert_ne!(copied[0], 77, "under a fresh component id");
    }

    /// The case "load as N" exists for, with a populated microgrid:
    /// component ids are enterprise-unique, so a copy that kept the
    /// original's ids would die on "component id X is already
    /// registered in microgrid Y" the moment the original is live.
    /// The copy therefore gets fresh ids for every component — and
    /// the same shape, because the `connect` calls move with them.
    #[test]
    fn load_as_remints_component_ids_of_a_live_original() {
        let (cfg, dir) = config_with(
            "(make-microgrid :id 9 :name \"m\" :grpc-port 8800 :topology \
             (lambda () (%make-grid-connection-point :id 70) (%make-meter :id 71) \
             (%make-battery-inverter :id 72) (connect 70 71) (connect 71 72)))",
        );
        // The managed file for the LIVE microgrid 9, rendered from
        // its own state — exactly what the load picker hands back.
        let (def, site) = {
            let reg = cfg.microgrids();
            let r = reg.lock();
            let e = r.get(&9).unwrap();
            (e.def.clone(), e.site.clone())
        };
        let src = dir.join("mg9.lisp");
        std::fs::write(
            &src,
            crate::lisp::microgrid_file::compose(
                &crate::lisp::microgrid_file::render_block(&def, &site),
                "",
            ),
        )
        .unwrap();

        let id = cfg
            .load_as(&src, 11)
            .expect("copy loads beside the original");
        assert_eq!(id, 11);
        let reg = cfg.microgrids();
        let r = reg.lock();
        let original = &r.get(&9).expect("original still registered").site;
        let copy = &r.get(&11).expect("copy registered").site;

        let ids = |s: &crate::sim::MicrogridSite| -> Vec<u64> {
            s.components().iter().map(|c| c.id()).collect()
        };
        let (old_ids, new_ids) = (ids(original), ids(copy));
        assert_eq!(old_ids, vec![70, 71, 72], "the original is untouched");
        assert_eq!(new_ids.len(), 3, "same component count");
        assert!(
            new_ids.iter().all(|n| !old_ids.contains(n)),
            "every copied component got a fresh id: {new_ids:?}"
        );
        // Same graph, different numbers: translate the original's
        // edges through the positional id map and compare.
        let map: std::collections::HashMap<u64, u64> = old_ids
            .iter()
            .copied()
            .zip(new_ids.iter().copied())
            .collect();
        let expected: Vec<(u64, u64)> = original
            .all_connections()
            .into_iter()
            .map(|(a, b)| (map[&a], map[&b]))
            .collect();
        assert_eq!(copy.all_connections(), expected, "isomorphic edge set");
    }

    /// `set-microgrid-name` / `set-microgrid-tso` edit the registry
    /// def AND land in the managed file's `(make-microgrid …)` head,
    /// so the new name survives a reload. Neither moves a component,
    /// so both have to bump the structural version themselves — that
    /// counter is the persist trigger.
    #[test]
    fn microgrid_attribute_setters_persist_into_the_managed_file() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let def = crate::sim::microgrids::MicrogridDef {
            id: 20,
            name: "before".into(),
            grpc_port: 8890,
            tso: None,
        };
        let path = dir.join("microgrids/20.lisp");
        crate::lisp::microgrid_file::write_atomic(
            &path,
            &crate::lisp::microgrid_file::compose(
                &crate::lisp::microgrid_file::render_empty_block(&def),
                crate::lisp::microgrid_file::FRESH_SCRIPT_HEADER,
            ),
        )
        .unwrap();
        cfg.load_file(&path).unwrap();

        cfg.eval("(set-microgrid-name 20 \"after\")").unwrap();
        cfg.eval("(set-microgrid-tso 20 \"BW\")").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(":name \"after\""), "{text}");
        assert!(text.contains(":tso \"BW\""), "{text}");
        assert_eq!(cfg.microgrids().lock()[&20].def.name, "after");

        // The file is what a reload rebuilds from, so both edits come
        // back out of it.
        cfg.reload_file(&path).unwrap();
        let entry = cfg.microgrids().lock()[&20].clone();
        assert_eq!(entry.def.name, "after");
        assert_eq!(entry.def.tso.as_deref(), Some("BW"));

        // nil clears the label again, and that clears it in the file.
        cfg.eval("(set-microgrid-tso 20 nil)").unwrap();
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains(":tso"),
            "a cleared TSO leaves no :tso in the head"
        );
        // An unregistered id is an error, not a silent no-op.
        assert!(cfg.eval("(set-microgrid-name 4242 \"x\")").is_err());
    }

    /// A loaded microgrid remembers the file it came from; a REPL one
    /// does not.
    #[test]
    fn entries_record_their_source_file() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        cfg.eval("(make-microgrid :id 10 :grpc-port 8801 :topology (lambda () nil))")
            .unwrap();
        let reg = cfg.microgrids();
        let r = reg.lock();
        let nine = r.get(&9).unwrap();
        assert_eq!(
            nine.source.as_deref(),
            Some(dir.join("config.lisp").canonicalize().unwrap().as_path()),
        );
        assert!(!nine.managed, "a plain script is unmanaged");
        assert!(
            r.get(&10).unwrap().source.is_none(),
            "REPL entries have no source"
        );
    }

    /// `(current-source-file)` names the file being loaded, and is nil
    /// outside a load.
    #[test]
    fn current_source_file_reports_the_loading_file() {
        let (cfg, dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
             (lambda () (setq seen (current-source-file))))",
        );
        let seen = cfg.eval_silent("seen").unwrap();
        assert!(
            seen.contains("config.lisp"),
            "topology should see its own file: {seen}"
        );
        let _ = dir;
        assert_eq!(cfg.eval_silent("(current-source-file)").unwrap(), "nil");
    }

    /// `config_with` auto-wraps a body lacking `(make-microgrid …)`
    /// into a single-entry registration under the fixed default id
    /// 2200 — tests that care about a different id supply their own
    /// `(make-microgrid …)` form instead.
    #[test]
    fn auto_wrapper_registers_single_microgrid_under_the_default_id() {
        let (cfg, _dir) = config_with("nil");
        let reg = cfg.microgrids();
        let r = reg.lock();
        assert_eq!(r.len(), 1);
        let e = r.get(&2200).expect("auto-wrapped under the default id");
        assert_eq!(e.def.name, "default");
        assert_eq!(e.def.grpc_port, 8800);
    }

    /// `(make-microgrid …)` builds a *new* site for the entry and
    /// funcalls the :topology lambda with the current-microgrid
    /// pointer set to the new id. Nested make-* calls register
    /// into that fresh site, not the bootstrap or any prior
    /// microgrid's site.
    #[test]
    fn make_microgrid_registers_entry_and_topology() {
        let (cfg, _dir) = config_with("nil");
        cfg.eval(
            r#"
            (make-microgrid
              :name "south yard"
              :id 7777
              :grpc-port 8810
              :tso "TN"
              :topology
              (lambda ()
                (%make-grid-connection-point :id 1)))
            "#,
        )
        .unwrap();
        let reg = cfg.microgrids();
        let r = reg.lock();
        let e = r.get(&7777).expect("registered");
        assert_eq!(e.def.name, "south yard");
        assert_eq!(e.def.grpc_port, 8810);
        assert_eq!(e.def.tso.as_deref(), Some("TN"));
        // The :topology lambda ran with current-microgrid pinned
        // to the new id, so the grid component lives on the new
        // microgrid's own site — NOT on the bootstrap site.
        assert!(
            e.site.get(1).is_some(),
            "grid-connection-point id=1 should be on the new site",
        );
    }

    /// A reload of its own file announces the microgrid again, so a
    /// runtime that failed gets another start.
    #[test]
    fn a_reload_announces_the_microgrid_again() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 31 :grpc-port 8931 :topology (lambda () nil))");
        let mut rx = cfg.subscribe_microgrid_registered();
        cfg.load_file(&dir.join("config.lisp")).unwrap();
        assert_eq!(rx.try_recv().ok(), Some(31));
    }

    /// With no pin check installed (or one saying "not pinned"), a
    /// reload adopts a changed :grpc-port.
    #[test]
    fn a_reload_adopts_a_new_port_when_not_pinned() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 32 :grpc-port 8932 :topology (lambda () nil))");
        let path = dir.join("config.lisp");
        std::fs::write(
            &path,
            "(make-microgrid :id 32 :grpc-port 8942 :topology (lambda () nil))",
        )
        .unwrap();
        cfg.load_file(&path).unwrap();
        assert_eq!(cfg.microgrids().lock()[&32].def.grpc_port, 8942);
    }

    /// A reload cannot move a microgrid onto another microgrid's port
    /// or a reserved one; the registry keeps the old port.
    #[test]
    fn a_reload_refuses_a_taken_or_reserved_port() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 34 :grpc-port 8934 :topology (lambda () nil))");
        let other = dir.join("other.lisp");
        std::fs::write(
            &other,
            "(make-microgrid :id 35 :grpc-port 8935 :topology (lambda () nil))",
        )
        .unwrap();
        cfg.load_file(&other).unwrap();
        let path = dir.join("config.lisp");
        let reserved = server_ports(&cfg).map(|p| (p, "reserved"));
        for (port, words) in std::iter::once((8935, "already bound")).chain(reserved) {
            std::fs::write(
                &path,
                format!("(make-microgrid :id 34 :grpc-port {port} :topology (lambda () nil))"),
            )
            .unwrap();
            let err = format!("{:?}", cfg.load_file(&path).expect_err("must be refused"));
            assert!(err.contains(words), "unexpected error: {err}");
            assert_eq!(cfg.microgrids().lock()[&34].def.grpc_port, 8934);
        }
    }

    /// When the pin check says pinned, the registry keeps the bound
    /// port.
    #[test]
    fn a_reload_keeps_a_pinned_port() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 33 :grpc-port 8933 :topology (lambda () nil))");
        assert!(
            cfg.port_pins()
                .set(std::sync::Arc::new(|id| id == 33))
                .is_ok()
        );
        let path = dir.join("config.lisp");
        std::fs::write(
            &path,
            "(make-microgrid :id 33 :grpc-port 8943 :topology (lambda () nil))",
        )
        .unwrap();
        cfg.load_file(&path).unwrap();
        assert_eq!(cfg.microgrids().lock()[&33].def.grpc_port, 8933);
    }

    /// Re-running `(make-microgrid …)` for an id that's already
    /// registered must reuse the existing entry's site (reset in
    /// place), not mint a fresh one — the running physics task and
    /// the per-port gRPC server all hold the original handle, and a
    /// fresh site would orphan them (frozen physics, stale gRPC).
    ///
    /// Re-declaring an id is only legal from the file that owns it
    /// (see `loading_a_second_file_with_a_taken_id_errors`), so the
    /// re-registration here goes through a re-load of that file —
    /// which is the reload path's real shape anyway.
    #[test]
    fn make_microgrid_reuses_the_existing_site_on_reregistration() {
        let (cfg, dir) = config_with(
            r#"
            (make-microgrid
              :name "yard" :id 7000 :grpc-port 8810
              :topology (lambda () (%make-grid-connection-point :id 1)))
            "#,
        );
        // The handle a running microgrid runtime would hold.
        let live_site = cfg.microgrids().lock().get(&7000).unwrap().site.clone();
        assert!(live_site.get(1).is_some());

        // Re-register the same id with a different topology.
        let path = dir.join("config.lisp");
        std::fs::write(
            &path,
            r#"
            (make-microgrid
              :name "yard v2" :id 7000 :grpc-port 8810
              :topology (lambda () (%make-grid-connection-point :id 2)))
            "#,
        )
        .unwrap();
        cfg.load_file(&path).unwrap();

        // The pre-rerun handle sees the new topology: same site,
        // reset and rebuilt in place.
        assert!(live_site.get(1).is_none(), "old component is gone");
        assert!(live_site.get(2).is_some(), "new component on the SAME site");
        let entry = cfg.microgrids().lock().get(&7000).cloned().unwrap();
        assert_eq!(entry.def.name, "yard v2");
        assert!(entry.site.get(2).is_some());
    }

    /// Auto-allocated component ids stay globally unique across
    /// microgrids: each `(make-meter)` consumes the next entry on
    /// the enterprise-wide allocator, regardless of which site
    /// receives the component.
    #[test]
    fn auto_ids_are_globally_unique_across_microgrids() {
        let (cfg, _dir) = config_with("nil");
        let ids: String = cfg
            .eval(
                r#"
                (let (a b c)
                  (make-microgrid :name "alpha" :id 3200
                                  :topology (lambda ()
                                              (setq a (component-id (%make-meter)))))
                  (make-microgrid :name "beta"  :id 3201
                                  :topology (lambda ()
                                              (setq b (component-id (%make-meter)))))
                  (make-microgrid :name "gamma" :id 3202
                                  :topology (lambda ()
                                              (setq c (component-id (%make-meter)))))
                  (format "%d/%d/%d" a b c))
                "#,
            )
            .unwrap()
            .trim_matches('"')
            .to_string();
        let parts: Vec<u64> = ids.split('/').map(|s| s.parse().unwrap()).collect();
        assert_eq!(parts.len(), 3);
        // Distinct values, all >= FIRST_AUTO_ID.
        assert_ne!(parts[0], parts[1]);
        assert_ne!(parts[1], parts[2]);
        assert_ne!(parts[0], parts[2]);
        for p in &parts {
            assert!(*p >= crate::sim::component::FIRST_AUTO_ID);
        }
    }

    /// Two microgrids end up with isolated sites — adding a grid
    /// to one doesn't leak into the other.
    #[test]
    fn two_microgrids_have_isolated_sites() {
        let (cfg, _dir) = config_with("nil");
        cfg.eval(
            r#"
            (make-microgrid :name "alpha" :id 1001
                            :topology (lambda ()
                                        (%make-grid-connection-point :id 1)))
            (make-microgrid :name "beta"  :id 1002
                            :topology (lambda ()
                                        (%make-grid-connection-point :id 2)))
            "#,
        )
        .unwrap();
        let reg = cfg.microgrids();
        let r = reg.lock();
        let a = r.get(&1001).unwrap();
        let b = r.get(&1002).unwrap();
        // Each microgrid sees its own grid component.
        assert!(a.site.get(1).is_some(), "alpha owns id=1");
        assert!(b.site.get(2).is_some(), "beta owns id=2");
        // Neither sees the other's.
        assert!(a.site.get(2).is_none(), "alpha doesn't see beta's id=2");
        assert!(b.site.get(1).is_none(), "beta doesn't see alpha's id=1");
    }

    /// When :id / :grpc-port are omitted, make-microgrid hands out
    /// the next free values starting at the registry's known
    /// floors.
    #[test]
    fn make_microgrid_auto_allocates_id_and_port() {
        let (cfg, _dir) = config_with("nil");
        let first: i64 = cfg
            .eval("(make-microgrid :name \"alpha\")")
            .unwrap()
            .parse()
            .unwrap();
        let second: i64 = cfg
            .eval("(make-microgrid :name \"beta\")")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            second > first,
            "auto-allocated ids must be strictly increasing"
        );
        let r = cfg.microgrids();
        let g = r.lock();
        let a = g.get(&(first as u64)).unwrap();
        let b = g.get(&(second as u64)).unwrap();
        assert_ne!(a.def.grpc_port, b.def.grpc_port);
    }
}
