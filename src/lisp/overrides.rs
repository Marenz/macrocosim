//! `Config::eval` and the persistence it triggers.
//!
//! A successful eval that moved a microgrid's structure regenerates
//! that microgrid's managed file from live state
//! (`Config::persist`); one that touched enterprise-wide state — a
//! `*-defaults` plist or a metadata setter — regenerates
//! `enterprise.lisp` (`Config::persist_enterprise`). Runtime pokes
//! (`set-meter-power`, health flips, scenario steps) change no file:
//! the script section is where a hand-written poke belongs.

use std::fs;
use std::path::{Path, PathBuf};

use super::microgrid_file;
use super::{Config, DEFAULT_CATEGORIES};

impl Config {
    /// Evaluate `src` in the interpreter and, on success, save
    /// whatever it changed. `eval_string` returns the final form's
    /// value; we stringify it via `Display` and return the result
    /// formatted via `Display`. Errors are formatted with full trace
    /// context the same way the reload path's logger formats them.
    ///
    /// Synchronous — acquires the interpreter write lock for the
    /// duration of the eval. Callers in async contexts must wrap in
    /// `tokio::task::spawn_blocking` to keep the executor free.
    ///
    /// Errored evals save nothing — a half-applied topology change
    /// shouldn't be written out as the microgrid's new structure.
    /// Either way the MicrogridSite version bumps so UI subscribers
    /// refetch.
    pub fn eval(&self, src: &str) -> Result<String, String> {
        let mut ctx = self.ctx.borrow_mut();
        self.eval_locked(&mut ctx, src)
    }

    /// Per-microgrid scoped eval — `/api/mg/{id}/eval`'s backend.
    /// Scope-set, eval, save, and the version bump all happen under
    /// one interpreter-lock acquisition, so two concurrent scoped
    /// evals can't cross microgrids (the scope pointer is ambient
    /// global state every scoped defun reads).
    pub fn eval_in_mg(&self, mg_id: u64, src: &str) -> Result<String, String> {
        self.scoped(mg_id, |cfg, ctx| cfg.eval_locked(ctx, src))
    }

    /// [`Config::eval_in_mg`] that refuses a microgrid the registry
    /// no longer holds once the interpreter lock is taken: `None`
    /// then, and nothing is evaluated.
    pub fn eval_in_registered_mg(&self, mg_id: u64, src: &str) -> Option<Result<String, String>> {
        self.scoped(mg_id, |cfg, ctx| {
            let registered = cfg.microgrids().lock().contains_key(&mg_id);
            registered.then(|| cfg.eval_locked(ctx, src))
        })
    }

    /// Run `f` with the interpreter locked and `current_microgrid`
    /// flipped to `mg_id` (restored on exit, panic included). The
    /// one sanctioned way to do a scoped operation from Rust: the
    /// pointer only ever flips while the interpreter lock is held,
    /// so no in-flight eval can observe a foreign scope.
    pub fn scoped<R>(
        &self,
        mg_id: u64,
        f: impl FnOnce(&Self, &mut tulisp::TulispContext) -> R,
    ) -> R {
        let mut ctx = self.ctx.borrow_mut();
        crate::sim::microgrids::with_microgrid(&self.current_microgrid, mg_id, || f(self, &mut ctx))
    }

    /// `eval` body against an already-held interpreter guard. The
    /// save pass and the version bump stay inside the locked section
    /// because both resolve the ambient scope pointer — after the
    /// lock is released a concurrent scoped call may flip it.
    ///
    /// An eval consisting only of `(load "file")` forms is a load,
    /// not an edit, so it is routed through [`Config::load_file`]
    /// instead of a bare `eval_string`: that way the microgrids the
    /// file registers are attributed to it, and a reload rebuilds
    /// them from the file.
    ///
    /// A load MIXED with other top-level forms still runs through the
    /// plain eval path (the other forms need it), so its microgrids
    /// are attributed to no file — same as if they had been typed.
    fn eval_locked(&self, ctx: &mut tulisp::TulispContext, src: &str) -> Result<String, String> {
        let (loads, has_other_forms) = top_level_load_paths(src);
        // A pure-load eval IS a load: route it through the loader so
        // the file becomes the ambient source of whatever microgrids
        // it registers — and so a reload rebuilds them from the file,
        // exactly like a boot script's.
        if !loads.is_empty() && !has_other_forms {
            for path in &loads {
                // `load_file_locked` already bumped exactly the sites
                // the load created or rebuilt; the ambient site the
                // other paths bump is not necessarily one of them.
                self.load_file_locked(ctx, path)
                    .map_err(|e| e.to_string())?;
            }
            // `(load …)` is a statement, not an expression; mirror
            // elisp and report success rather than a file's last form.
            return Ok("t".to_string());
        }
        let before = self.structural_versions();
        let result = match ctx.eval_string(src) {
            Ok(v) => Ok(v.to_string()),
            Err(e) => Err(e.format(ctx)),
        };
        if result.is_ok() {
            self.persist_changed(ctx, src, &before);
        }
        // Bump the version on the microgrid the eval actually
        // mutated (the one current_microgrid points at, or — if no
        // scope was set — the router's fallback) so the WS event
        // pump fires TopologyChanged on the right bus. Without this
        // the bootstrap site's version moved, but UI sessions only
        // listen to per-mg buses.
        self.router.site().bump_version();
        result
    }

    /// Read-only eval — same machinery as `eval` but nothing is
    /// saved to any file and the site version does NOT bump. For UI introspection (e.g. "what's the current
    /// value of battery-defaults?") that shouldn't surface as a
    /// persisted edit.
    pub fn eval_silent(&self, src: &str) -> Result<String, String> {
        let mut ctx = self.ctx.borrow_mut();
        match ctx.eval_string(src) {
            Ok(v) => Ok(v.to_string()),
            Err(e) => Err(e.format(&ctx)),
        }
    }

    /// Read-only value of the `*-defaults` variable `var`, printed
    /// with old keywords shown under their new names. Nothing is
    /// saved.
    pub fn defaults_silent(&self, var: &str) -> Result<String, String> {
        let mut ctx = self.ctx.borrow_mut();
        match ctx.eval_string(var) {
            Ok(v) => Ok(renamed_defaults(&mut ctx, v).to_string()),
            Err(e) => Err(e.format(&ctx)),
        }
    }

    /// Every registered microgrid's structural version, snapshotted
    /// before an eval so the save pass can tell which microgrids the
    /// eval actually moved. Pokes (power, health, modes) don't touch
    /// the structural version, so they cost no file write.
    fn structural_versions(&self) -> Vec<(u64, u64)> {
        self.microgrids
            .lock()
            .iter()
            .map(|(id, e)| (*id, e.site.structural_version()))
            .collect()
    }

    /// Save what an eval changed: every microgrid whose structure
    /// moved since `before`, plus `enterprise.lisp` when the source
    /// carries an enterprise-wide edit.
    ///
    /// Suppressed while a file is loading — a load must not rewrite
    /// the file it is reading (the file's own forms are exactly what
    /// moved the structure, so every load would otherwise rewrite
    /// itself, and a reload would rewrite everything).
    fn persist_changed(&self, ctx: &mut tulisp::TulispContext, src: &str, before: &[(u64, u64)]) {
        if self.loading.lock().is_some() {
            return;
        }
        let changed: Vec<(u64, bool, bool)> = {
            let reg = self.microgrids.lock();
            reg.iter()
                .filter(|(id, e)| {
                    let was = before.iter().find(|(bid, _)| bid == *id).map(|(_, v)| *v);
                    // A microgrid registered by this eval has no
                    // `before` entry — that counts as changed.
                    was != Some(e.site.structural_version())
                })
                .map(|(id, e)| (*id, e.managed, e.source.is_some()))
                .collect()
        };
        for (id, managed, has_source) in changed {
            if managed && has_source {
                // `persist` logs + banners its own failures.
                let _ = self.persist(id);
                continue;
            }
            if !has_source {
                log::warn!(
                    "microgrid {id} changed but is not backed by a file; \
                     the edit will not survive a restart"
                );
            }
            // Live state the file on disk doesn't carry: an unmanaged
            // file is the author's to edit (Adopt makes it managed),
            // and a REPL microgrid has no file at all.
            self.set_unsaved(id, true);
        }
        if (contains_defaults_setq(src) || enterprise_setter_in(src))
            && let Err(e) = self.persist_enterprise_locked(ctx)
        {
            let path = self.enterprise_path();
            log::error!("failed to save {}: {e}", path.display());
            self.router.site().broadcast_config_error(format!(
                "enterprise settings applied but could not be saved to {}: {e} — \
                 the edit will not survive a restart",
                path.display()
            ));
        }
    }

    /// Regenerate microgrid `id`'s managed file from its live state:
    /// the generated block is rewritten, the hand-written script
    /// section is copied through byte for byte. A no-op for a
    /// microgrid that is unmanaged or has no file.
    ///
    /// On failure the in-memory edit stands, the microgrid is flagged
    /// *unsaved*, and the UI gets a config-error banner — the write
    /// is retried by the next structural edit.
    pub fn persist(&self, id: u64) -> std::io::Result<()> {
        let Some((def, site, path)) = ({
            let reg = self.microgrids.lock();
            reg.get(&id).filter(|e| e.managed).and_then(|e| {
                let path = e.source.clone()?;
                Some((e.def.clone(), e.site.clone(), path))
            })
        }) else {
            return Ok(());
        };
        let block = microgrid_file::render_block(&def, &site);
        // What the file says right now becomes the undo step: one
        // press of Undo puts exactly these bytes back. Read before
        // the overwrite (afterwards the previous block is gone) but
        // stacked only once the overwrite succeeded — a failed write
        // left the file alone, so there is nothing to go back from.
        let previous = super::undo::read_generated_block(&path);
        match self.write_two_section(&path, &block, microgrid_file::FRESH_SCRIPT_HEADER) {
            Ok(()) => {
                if let Some(previous) = previous {
                    self.push_undo_block(id, previous);
                }
                self.set_unsaved(id, false);
                Ok(())
            }
            Err(e) => {
                log::error!("failed to save microgrid {id} to {}: {e}", path.display());
                site.broadcast_config_error(format!(
                    "microgrid {id} could not be saved to {}: {e} — the edit is \
                     live but will not survive a restart",
                    path.display()
                ));
                self.set_unsaved(id, true);
                Err(e)
            }
        }
    }

    /// Regenerate `enterprise.lisp` from live enterprise-wide state.
    pub fn persist_enterprise(&self) -> std::io::Result<()> {
        let mut ctx = self.ctx.borrow_mut();
        self.persist_enterprise_locked(&mut ctx)
    }

    /// [`persist_enterprise`](Self::persist_enterprise) against an
    /// already-held interpreter guard — reading the `*-defaults`
    /// plists needs the interpreter the caller already holds.
    pub(super) fn persist_enterprise_locked(
        &self,
        ctx: &mut tulisp::TulispContext,
    ) -> std::io::Result<()> {
        let block = self.render_enterprise_block(ctx);
        self.write_two_section(
            &self.enterprise_path(),
            &block,
            microgrid_file::FRESH_ENTERPRISE_SCRIPT_HEADER,
        )
    }

    /// The generated block of `enterprise.lisp`: the enterprise
    /// metadata setters in a fixed order, then every bound
    /// `*-defaults` plist. Fixed order so two saves of the same state
    /// produce the same bytes (the watcher's self-write check and
    /// `git diff` both care).
    fn render_enterprise_block(&self, ctx: &mut tulisp::TulispContext) -> String {
        use crate::lisp::escape_lisp_string as esc;
        use std::fmt::Write as _;

        let md = self.metadata();
        let mut out = String::new();
        writeln!(out, "(set-enterprise-id {})", md.enterprise_id).unwrap();
        writeln!(out, "(set-timezone \"{}\")", esc(self.tz_name())).unwrap();
        writeln!(
            out,
            "(set-default-request-lifetime-s {})",
            crate::lisp::lisp_float(md.default_request_lifetime.as_secs_f64())
        )
        .unwrap();
        writeln!(
            out,
            "(set-default-augment-lifetime-s {})",
            crate::lisp::lisp_float(md.default_augment_lifetime.as_secs_f64())
        )
        .unwrap();
        writeln!(
            out,
            "(set-assets-socket-addr \"{}\")",
            esc(&md.assets_socket_addr)
        )
        .unwrap();
        writeln!(
            out,
            "(set-dispatch-socket-addr \"{}\")",
            esc(&md.dispatch_socket_addr)
        )
        .unwrap();
        for cat in DEFAULT_CATEGORIES {
            let var = format!("{cat}-defaults");
            // The guard keeps an unbound (or makunbound) category
            // from erroring the whole render.
            let Ok(value) = ctx.eval_string(&format!("(and (boundp '{var}) {var})")) else {
                continue;
            };
            if value.null() {
                continue;
            }
            let value = renamed_defaults(ctx, value);
            let text = value.to_string();
            // format_with_width returns the source unchanged on
            // failure; either way the text re-reads as the same
            // value. Continuation lines are indented to sit under
            // the quote.
            let pretty = tulisp_fmt::format_with_width(&text, 72).unwrap_or_else(|_| text.clone());
            let body = pretty.trim_end().replace('\n', "\n       ");
            writeln!(out, "(setq {var}\n      '{body})").unwrap();
        }
        out
    }

    /// Write a two-section file: `block` between the markers, the
    /// existing file's script section (or `fresh_script` when the
    /// file is new) after them. Atomic, and the written bytes are
    /// recorded so the watcher can recognise our own save.
    ///
    /// Refuses a file that lost its markers — composing over it would
    /// bury the whole hand-written text under a generated block.
    fn write_two_section(
        &self,
        path: &Path,
        block: &str,
        fresh_script: &str,
    ) -> std::io::Result<()> {
        let script = match fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => fresh_script.to_string(),
            Ok(text) => {
                let parsed = microgrid_file::parse(&text).map_err(std::io::Error::other)?;
                match parsed.generated {
                    Some(_) => parsed.script,
                    None => {
                        return Err(std::io::Error::other(format!(
                            "{} carries no macrocosim-generated block",
                            path.display()
                        )));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fresh_script.to_string(),
            Err(e) => return Err(e),
        };
        let text = microgrid_file::compose(block, &script);
        microgrid_file::write_atomic(path, &text)?;
        self.record_self_write(path, &text);
        Ok(())
    }

    /// Flag (or clear) "live edits this microgrid's file doesn't
    /// carry" — the UI renders it as an unsaved marker on the
    /// microgrid card.
    fn set_unsaved(&self, id: u64, unsaved: bool) {
        if let Some(entry) = self.microgrids.lock().get_mut(&id) {
            entry.unsaved = unsaved;
        }
    }
}

/// A `*-defaults` value with old keywords renamed. Only a list is
/// renamed; any other value, and a list whose rename fails (the error
/// is logged), comes back unchanged.
fn renamed_defaults(
    ctx: &mut tulisp::TulispContext,
    value: tulisp::TulispObject,
) -> tulisp::TulispObject {
    if !value.consp() {
        return value;
    }
    match super::renames::rename_plist_value(ctx, &value) {
        Ok(renamed) => renamed,
        Err(e) => {
            log::warn!("a defaults list was kept as written: {}", e.format(ctx));
            value
        }
    }
}

/// Top-level `(load "…")` targets in `src`, plus whether any OTHER
/// top-level expression form rides along. Only string-literal
/// arguments count — a computed path `(load (concat …))` can't be
/// resolved statically and simply isn't recorded for reload.
fn top_level_load_paths(src: &str) -> (Vec<PathBuf>, bool) {
    use tulisp_fmt::cst::CstNode;
    let Ok(cst) = tulisp_fmt::parse(src) else {
        return (Vec::new(), false);
    };
    let mut loads = Vec::new();
    let mut other = false;
    for n in &cst.nodes {
        match n {
            CstNode::Comment { .. } | CstNode::LineBreak { .. } => {}
            CstNode::List { children, .. } => {
                let mut atoms = children.iter().filter_map(|c| match c {
                    CstNode::Atom { text, .. } => Some(text.as_str()),
                    _ => None,
                });
                let is_load = atoms.next() == Some("load");
                let arg = atoms.next();
                match (is_load, arg) {
                    (true, Some(s)) if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 => {
                        // Lisp string literal → path. Escapes other
                        // than \\ and \" don't appear in file paths
                        // in practice; unescape just those two.
                        let unescaped = s[1..s.len() - 1]
                            .replace("\\\\", "\\")
                            .replace("\\\"", "\"");
                        loads.push(PathBuf::from(unescaped));
                    }
                    (true, _) => other = true,
                    (false, _) => other = true,
                }
            }
            _ => other = true,
        }
    }
    (loads, other)
}

/// Enterprise-wide setters: a call to one of these changes state
/// that lives in `enterprise.lisp`, not in any microgrid file.
const ENTERPRISE_SETTERS: &[&str] = &[
    "set-enterprise-id",
    "set-timezone",
    "set-default-request-lifetime-s",
    "set-default-augment-lifetime-s",
    "set-default-request-lifetime-ms",
    "set-default-augment-lifetime-ms",
    "set-assets-socket-addr",
    "set-dispatch-socket-addr",
];

/// Does `src` call one of the [`ENTERPRISE_SETTERS`] at top level?
/// None of them touches a microgrid's structure, so the structural
/// check alone would never save them.
fn enterprise_setter_in(src: &str) -> bool {
    use tulisp_fmt::cst::CstNode;
    let Ok(cst) = tulisp_fmt::parse(src) else {
        return false;
    };
    cst.nodes.iter().any(|n| {
        let CstNode::List { children, .. } = n else {
            return false;
        };
        children
            .iter()
            .find_map(|c| match c {
                CstNode::Atom { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .is_some_and(|head| ENTERPRISE_SETTERS.contains(&head))
    })
}

/// Does `src` contain a top-level defaults edit — `(setq <sym>-defaults …)`?
/// The Defaults panel saves through eval; those shape FUTURE `make-*`
/// calls rather than the live graph, so the structural check alone
/// would drop them — they belong in `enterprise.lisp`.
fn contains_defaults_setq(src: &str) -> bool {
    use tulisp_fmt::cst::CstNode;
    let Ok(cst) = tulisp_fmt::parse(src) else {
        return false;
    };
    cst.nodes.iter().any(|n| {
        let CstNode::List { children, .. } = n else {
            return false;
        };
        let mut atoms = children.iter().filter_map(|c| match c {
            CstNode::Atom { text, .. } => Some(text.as_str()),
            _ => None,
        });
        atoms.next() == Some("setq") && atoms.next().is_some_and(|s| s.ends_with("-defaults"))
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::config_with;

    /// A scoped eval in a microgrid the registry no longer holds does
    /// not run.
    #[test]
    fn eval_in_registered_mg_refuses_an_unregistered_id() {
        let (cfg, _dir) = config_with("");
        assert!(
            cfg.eval_in_registered_mg(9999, "(setq http-race-probe 1)")
                .is_none()
        );
        assert_eq!(cfg.eval("(boundp 'http-race-probe)").unwrap(), "nil");
    }

    /// An `enterprise.lisp` with the old millisecond keyword
    /// `:command-delay-ms 200` is saved back under the new names.
    #[test]
    fn old_defaults_are_saved_with_new_names() {
        let (cfg, dir) = config_with("(setq battery-inverter-defaults '(:command-delay-ms 200))");
        cfg.persist_enterprise().unwrap();
        let text = std::fs::read_to_string(dir.join("enterprise.lisp")).unwrap();
        assert!(text.contains(":command-delay-s 0.2"), "{text}");
        assert!(!text.contains("command-delay-ms"), "{text}");
    }

    /// An old kg/h `:demand` symbol in the defaults is saved as a
    /// readable kg/s expression, and a boiler built from either form
    /// reads the same demand.
    #[test]
    fn an_old_demand_symbol_is_saved_as_a_readable_form() {
        let (cfg, dir) = config_with(
            "(setq flow-kg-h 1800.0)
             (setq steam-boiler-defaults '(:demand flow-kg-h))
             (make-steam-boiler :id 44)",
        );
        cfg.persist_enterprise().unwrap();
        let text = std::fs::read_to_string(dir.join("enterprise.lisp")).unwrap();
        assert!(
            text.contains("(:demand-kg-per-s (/ flow-kg-h 3600.0))"),
            "{text}"
        );
        cfg.eval("(setq steam-boiler-defaults '(:demand-kg-per-s (/ flow-kg-h 3600.0)))")
            .unwrap();
        cfg.eval("(make-steam-boiler :id 45)").unwrap();
        cfg.refresh_once();
        let site = cfg.site();
        for id in [44, 45] {
            let b = site.get(id).unwrap();
            let v = b.steam_drive().unwrap().demand_reading().value;
            assert!((v - 1800.0).abs() < 1e-3, "{id}: {v}");
        }
    }

    /// The enterprise block writes the lifetimes in seconds.
    #[test]
    fn the_enterprise_block_writes_lifetimes_in_seconds() {
        let (cfg, dir) = config_with("(set-default-request-lifetime-ms 20000)");
        cfg.persist_enterprise().unwrap();
        let text = std::fs::read_to_string(dir.join("enterprise.lisp")).unwrap();
        assert!(
            text.contains("(set-default-request-lifetime-s 20"),
            "{text}"
        );
        assert!(text.contains("(set-default-augment-lifetime-s "), "{text}");
        assert!(!text.contains("-lifetime-ms"), "{text}");
    }

    /// A `*-defaults` value that is not a list is written back as it
    /// is, and a list that cannot be renamed keeps its old keywords.
    #[test]
    fn defaults_that_cannot_be_renamed_are_kept() {
        let (cfg, dir) = config_with(
            "(setq meter-defaults 7)
             (setq battery-inverter-defaults '(:command-delay-ms \"soon\"))",
        );
        cfg.persist_enterprise().unwrap();
        let text = std::fs::read_to_string(dir.join("enterprise.lisp")).unwrap();
        assert!(text.contains("(setq meter-defaults\n      '7)"), "{text}");
        assert!(text.contains(":command-delay-ms \"soon\""), "{text}");
    }

    /// A `(load "file")` eval records the file for reload replay
    /// instead of journaling the form: the file itself is the
    /// persistent artifact.
    #[test]
    fn load_evals_are_recorded_not_journaled() {
        let (cfg, dir) = config_with("nil");
        let extra = dir.join("extra-mg.lisp");
        std::fs::write(
            &extra,
            "(make-microgrid :id 55 :grpc-port 8855 :topology (lambda () nil))",
        )
        .unwrap();
        cfg.eval("(load \"extra-mg.lisp\")").unwrap();
        assert!(cfg.microgrids().lock().contains_key(&55));
        // No overrides journal picked up the (load …) form.
        let mg_dir = dir.join("microgrids");
        if let Ok(entries) = std::fs::read_dir(&mg_dir) {
            for e in entries.flatten() {
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                assert!(
                    !text.contains("extra-mg"),
                    "(load …) form must not be journaled; found in {}",
                    e.path().display()
                );
            }
        }
        // The recorded file replays on reload.
        cfg.reload().expect("reload");
        assert!(cfg.microgrids().lock().contains_key(&55));
    }

    /// A pure-load eval goes through the loader and its forms run,
    /// but a script that registers no microgrid is not a source
    /// file: reload replays the files the live microgrids came from,
    /// and this one backs none. Its `setq` stands in the interpreter
    /// like a typed one — nothing replays it.
    #[test]
    fn a_load_that_registers_nothing_is_not_a_source() {
        let (cfg, dir) = config_with("nil");
        let script = dir.join("poke.lisp");
        std::fs::write(&script, "(setq some-transient-var 42)").unwrap();
        cfg.eval("(load \"poke.lisp\")").unwrap();
        assert_eq!(cfg.eval_silent("some-transient-var").unwrap(), "42");
        assert!(
            !cfg.registered_sources()
                .iter()
                .any(|p| p.ends_with("poke.lisp")),
            "a file backing no microgrid is not part of the reload list"
        );
    }

    /// A `(make-microgrid …)` typed into the REPL is backed by no
    /// file, so a whole-world reload has nothing to replay it from.
    /// It must therefore drop out of the rebuild quietly: its entry
    /// survives with a reset, empty site (the runtimes that hold the
    /// site handle can't be torn down), and — the part that used to
    /// break — it must not turn up in the replay claiming an id, or
    /// the reload would abort with every site already reset and leave
    /// the world empty. The unbacked microgrid is transient; the
    /// FILE-backed world must survive.
    #[test]
    fn repl_created_microgrid_does_not_break_reload() {
        let (cfg, _dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800
               :topology (lambda () (%make-grid-connection-point :id 1)))",
        );
        cfg.eval(
            "(make-microgrid :id 10 :grpc-port 8899 :topology (lambda () (%make-meter :id 2)))",
        )
        .unwrap();
        // A REPL microgrid backs no file, so it is not in the replay
        // list — which is exactly why the reload must tolerate it.
        assert!(
            cfg.microgrids().lock()[&10].source.is_none(),
            "a REPL microgrid backs no file",
        );
        // The boot script's world comes back intact.
        cfg.reload()
            .expect("reload must survive a REPL-created microgrid");
        let reg = cfg.microgrids();
        let r = reg.lock();
        assert!(
            r.get(&9).unwrap().site.get(1).is_some(),
            "the file-backed microgrid's topology must be rebuilt",
        );
        assert!(
            r.get(&10).is_some_and(|e| e.site.get(2).is_none()),
            "the REPL microgrid keeps its entry but nothing rebuilds it",
        );
    }

    /// A structural eval rewrites the managed file it belongs to,
    /// keeping the hand-written script section; a transient poke
    /// rewrites nothing, and an unmanaged microgrid is never written
    /// to at all.
    #[test]
    fn structural_evals_regenerate_the_managed_file() {
        let (cfg, dir) = config_with(
            "(make-microgrid :id 9 :grpc-port 8800 :topology \
                                  (lambda () (%make-grid-connection-point :id 1)))",
        );
        // config.lisp is unmanaged → a structural eval flags, but writes nothing.
        cfg.eval("(rename-component 1 \"a\")").unwrap();
        assert!(!dir.join("microgrids").exists());

        // A managed microgrid: created like the UI does it.
        let def = crate::sim::microgrids::MicrogridDef {
            id: 20,
            name: "m".into(),
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
        cfg.eval_in_mg(20, "(%make-meter :id 100 :power-w 500.0)")
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("(%make-meter :id 100"), "{text}");
        assert!(text.contains(":power-w 500.0"), "{text}");
        assert!(
            text.contains("Anything below is yours"),
            "script section preserved"
        );
        // A poke does not rewrite the file.
        let before = std::fs::read_to_string(&path).unwrap();
        cfg.eval_in_mg(20, "(set-meter-power 100 4321.0)").unwrap();
        assert_eq!(before, std::fs::read_to_string(&path).unwrap());
    }

    /// Enterprise-wide state — the `*-defaults` plists and the
    /// metadata setters — regenerates `enterprise.lisp`, never a
    /// microgrid file.
    #[test]
    fn defaults_edits_regenerate_enterprise_lisp() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        cfg.eval("(setq battery-defaults '(:capacity-wh 1000.0))")
            .unwrap();
        let text = std::fs::read_to_string(dir.join("enterprise.lisp")).unwrap();
        assert!(text.contains("battery-defaults"), "{text}");
        assert!(text.contains(":capacity-wh 1000.0"), "{text}");
        cfg.eval("(set-enterprise-id 77)").unwrap();
        let text = std::fs::read_to_string(dir.join("enterprise.lisp")).unwrap();
        assert!(text.contains("(set-enterprise-id 77)"), "{text}");
    }

    /// A file macrocosim cannot write is a *live but unsaved* edit,
    /// never a failed eval: the in-memory change stands, the entry is
    /// flagged so the UI can mark the card, and the next structural
    /// edit retries the write. Here the file has lost its markers,
    /// which `write_two_section` refuses rather than burying the
    /// hand-written text under a generated block.
    #[test]
    fn a_refused_write_leaves_the_microgrid_live_and_flagged_unsaved() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let def = crate::sim::microgrids::MicrogridDef {
            id: 27,
            name: "m".into(),
            grpc_port: 8897,
            tso: None,
        };
        let path = dir.join("microgrids/27.lisp");
        let managed_text = crate::lisp::microgrid_file::compose(
            &crate::lisp::microgrid_file::render_empty_block(&def),
            crate::lisp::microgrid_file::FRESH_SCRIPT_HEADER,
        );
        crate::lisp::microgrid_file::write_atomic(&path, &managed_text).unwrap();
        cfg.load_file(&path).unwrap();
        let unsaved = || cfg.microgrids().lock().get(&27).unwrap().unsaved;
        assert!(!unsaved(), "a freshly loaded microgrid is saved");

        // Somebody replaced the file with marker-less text.
        std::fs::write(&path, ";; mine now\n(setq x 1)\n").unwrap();
        cfg.eval_in_mg(27, "(%make-meter :id 270)")
            .expect("the eval itself succeeds");
        {
            let reg = cfg.microgrids();
            let r = reg.lock();
            assert!(r[&27].site.get(270).is_some(), "the edit is live");
        }
        assert!(unsaved(), "and flagged as not on disk");
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("270"),
            "the hand-written text was not buried"
        );

        // Put a managed file back: the next structural edit saves
        // everything, including the edit that could not be written.
        crate::lisp::microgrid_file::write_atomic(&path, &managed_text).unwrap();
        cfg.eval_in_mg(27, "(%make-meter :id 271)").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("(%make-meter :id 270"), "{text}");
        assert!(text.contains("(%make-meter :id 271"), "{text}");
        assert!(!unsaved(), "and the flag clears");
    }

    /// Concurrent per-mg evals must not cross microgrids: the scope
    /// pointer only flips under the interpreter lock, so each eval's
    /// mutations AND the file it regenerates belong to its own
    /// microgrid. Pre-fix, scope-set happened before the lock and the
    /// persistence after release — two racing `/api/mg/{id}/eval`
    /// calls could write into each other's files.
    #[test]
    fn concurrent_scoped_evals_do_not_cross_microgrids() {
        let (cfg, dir) =
            config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))");
        let managed = |id: u64, port: u16, component: u64| {
            let def = crate::sim::microgrids::MicrogridDef {
                id,
                name: format!("m{id}"),
                grpc_port: port,
                tso: None,
            };
            let path = dir.join(format!("microgrids/{id}.lisp"));
            let block = crate::lisp::microgrid_file::render_empty_block(&def).replace(
                "    nil",
                &format!("    (%make-grid-connection-point :id {component})"),
            );
            crate::lisp::microgrid_file::write_atomic(
                &path,
                &crate::lisp::microgrid_file::compose(
                    &block,
                    crate::lisp::microgrid_file::FRESH_SCRIPT_HEADER,
                ),
            )
            .unwrap();
            cfg.load_file(&path).unwrap();
            path
        };
        let nine_path = managed(21, 8891, 1);
        let ten_path = managed(22, 8892, 2);

        std::thread::scope(|s| {
            let a = cfg.clone();
            let b = cfg.clone();
            s.spawn(move || {
                for i in 0..40 {
                    a.eval_in_mg(21, &format!("(rename-component 1 \"a{i}\")"))
                        .unwrap();
                }
            });
            s.spawn(move || {
                for i in 0..40 {
                    b.eval_in_mg(22, &format!("(rename-component 2 \"b{i}\")"))
                        .unwrap();
                }
            });
        });

        let nine = std::fs::read_to_string(&nine_path).unwrap();
        let ten = std::fs::read_to_string(&ten_path).unwrap();
        assert!(
            !nine.contains(":id 2 ") && nine.contains("\"a39\""),
            "mg 21's file must hold only mg 21's state:\n{nine}"
        );
        assert!(
            !ten.contains(":id 1 ") && ten.contains("\"b39\""),
            "mg 22's file must hold only mg 22's state:\n{ten}"
        );
        // The mutations landed on the right sites too.
        let reg = cfg.microgrids();
        let r = reg.lock();
        assert_eq!(r[&21].site.display_name(1).as_deref(), Some("a39"));
        assert_eq!(r[&22].site.display_name(2).as_deref(), Some("b39"));
    }
}
