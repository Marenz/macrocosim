//! Text-format primitives for a managed microgrid `.lisp` file: a
//! macrocosim-generated block (structure, rewritten on every save)
//! followed by a hand-written script section (loaded verbatim,
//! never touched by macrocosim). `parse` / `compose` split and
//! rejoin the two; `rewrite_id` / `rewrite_grpc_port` /
//! `remap_component_ids` patch the microgrid id, its gRPC port, and
//! every component id inside the generated block without disturbing
//! anything else in the file (the three together are what turns a
//! file into a second, independent copy of its microgrid).
//!
//! `render_block` / `render_empty_block` render the generated block
//! from live state, the inverse of `parse`. Between them these are
//! the whole read/write surface of the format: the loader
//! (`lisp::boot`) parses a file to decide how to evaluate it and
//! reads its head id back, the persist pass (`lisp::overrides`)
//! re-renders the block on every structural edit, undo / snapshots
//! swap blocks in and out, and the create / import / load-as
//! endpoints (`ui::handlers::microgrids`) compose and re-id whole
//! files.

use std::fs;
use std::io::Write;
use std::path::Path;

use tulisp_fmt::cst::CstNode;

/// Marks the start of the macrocosim-generated block. Used to
/// detect a managed file (the first non-empty line starts with
/// this) — `GENERATED_BEGIN_LINE` is the exact line macrocosim
/// writes, which carries an explanatory suffix beyond this prefix.
pub const GENERATED_BEGIN: &str = ";;; macrocosim:generated";

/// The literal first line of the generated block, as macrocosim
/// writes it.
pub const GENERATED_BEGIN_LINE: &str =
    ";;; macrocosim:generated — rewritten by macrocosim, do not edit";

/// Marks the end of the macrocosim-generated block.
pub const GENERATED_END: &str = ";;; macrocosim:end";

/// The ownership comment written at the top of the script section
/// of a freshly created managed file.
pub const FRESH_SCRIPT_HEADER: &str = "\
;; Anything below is yours. It runs after the structure, in this
;; microgrid's scope, on every load. Drive meters, define scenarios,
;; set setpoints here — do not construct components (the generated
;; block above owns the structure; constructing more here collides
;; on the next load).
";

/// The ownership comment written at the top of the script section
/// of a freshly created `enterprise.lisp`. Same two-section shape as
/// a microgrid file, but the tail runs once at boot for the whole
/// enterprise rather than per microgrid.
pub const FRESH_ENTERPRISE_SCRIPT_HEADER: &str = "\
;; Anything below is yours. It runs once at boot, before any
;; microgrid file — grid-frequency knobs and other enterprise-wide
;; settings live here.
";

/// A microgrid file split into its two sections.
pub struct ParsedFile {
    /// The generated block's contents, markers stripped, when the
    /// file is managed (its first non-empty line is
    /// `GENERATED_BEGIN_LINE`). `None` for an unmanaged file —
    /// hand-written, with no macrocosim markers at all.
    pub generated: Option<String>,
    /// Everything after the generated block (managed file), or the
    /// entire file (unmanaged file), byte-for-byte.
    pub script: String,
}

/// Split `text` into its generated block and script section.
///
/// A file is managed when its first non-empty line starts with
/// `GENERATED_BEGIN`; unmanaged text with no marker anywhere is
/// returned as `generated: None, script: text`. A marker found
/// anywhere other than a well-formed leading block is an error —
/// this only happens to text macrocosim itself never wrote, so
/// failing loudly beats silently mangling it.
pub fn parse(text: &str) -> Result<ParsedFile, String> {
    let first_non_empty = text.lines().find(|l| !l.trim().is_empty());
    let is_managed = first_non_empty.is_some_and(|l| l.starts_with(GENERATED_BEGIN));

    if !is_managed {
        for line in text.lines() {
            if line.starts_with(GENERATED_BEGIN) || line.starts_with(GENERATED_END) {
                return Err("macrocosim marker found but not at the top of the file".to_string());
            }
        }
        return Ok(ParsedFile {
            generated: None,
            script: text.to_string(),
        });
    }

    // Walk line-by-line via byte offsets (not `lines().collect()` +
    // rejoin) so the script tail survives byte-identical, including
    // any trailing-newline quirks of the original file.
    let mut generated_start = None;
    let mut end_line_start = None;
    let mut end_line_end = None;

    for (start, end, next) in line_spans(text) {
        let line = &text[start..end];
        match generated_start {
            None => {
                if line.starts_with(GENERATED_BEGIN) {
                    generated_start = Some(next);
                }
                // else: a blank line before the begin marker — skip.
            }
            Some(_) => {
                if line.starts_with(GENERATED_END) {
                    end_line_start = Some(start);
                    end_line_end = Some(next);
                    break;
                } else if line.starts_with(GENERATED_BEGIN) {
                    return Err(
                        "a second 'macrocosim:generated' marker found inside the block".into(),
                    );
                }
            }
        }
    }

    let Some(end_start) = end_line_start else {
        return Err("missing ';;; macrocosim:end' marker".to_string());
    };

    Ok(ParsedFile {
        generated: Some(text[generated_start.unwrap()..end_start].to_string()),
        script: text[end_line_end.unwrap()..].to_string(),
    })
}

/// Yield `(line_start, line_end_excluding_newline, next_line_start)`
/// byte offsets for every line in `text`, including a final line
/// with no trailing newline. Used instead of `str::lines()` +
/// rejoin so callers can slice the original bytes exactly.
fn line_spans(text: &str) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let len = text.len();
    let mut pos = 0usize;
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        let end = text[pos..].find('\n').map_or(len, |rel| pos + rel);
        let next = if end < len { end + 1 } else { len };
        let span = (pos, end, next);
        if next == len {
            done = true;
        }
        pos = next;
        Some(span)
    })
}

/// Join a generated block and a script section back into one
/// managed file's text. `block` is trimmed of trailing newlines
/// before the end marker is appended, so `compose` never emits a
/// blank line between the last generated form and
/// `GENERATED_END`.
pub fn compose(generated_block: &str, script: &str) -> String {
    let block = generated_block.trim_end_matches('\n');
    format!("{GENERATED_BEGIN_LINE}\n{block}\n{GENERATED_END}\n{script}")
}

/// Write `text` to `path` atomically: write to a `.tmp` sibling,
/// flush, then rename over the target — a crash mid-write leaves
/// the old content in place rather than a truncated file.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("lisp.tmp");
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Rewrite the `:id` argument of the `(make-microgrid …)` form
/// inside `text`'s generated block to `new_id`, leaving everything
/// else — component ids, formatting, the script section — untouched.
///
/// Errors on unmanaged text (nothing to rewrite) or a generated
/// block that doesn't parse as a `(make-microgrid :id … …)` form.
pub fn rewrite_id(text: &str, new_id: u64) -> Result<String, String> {
    rewrite_head_kwarg(text, ":id", &new_id.to_string())?
        .ok_or_else(|| "no :id argument found in the (make-microgrid …) form".to_string())
}

/// Rewrite the `:grpc-port` argument of the `(make-microgrid …)`
/// form inside `text`'s generated block to `new_port`, leaving
/// everything else untouched.
///
/// A head with NO `:grpc-port` is returned unchanged: the loader
/// auto-allocates a free port for it already, so there is nothing to
/// fix. This pairs with [`rewrite_id`] on the copy path — a second
/// live microgrid needs its own port as much as its own id, because
/// the original's is held by a listening gRPC server.
pub fn rewrite_grpc_port(text: &str, new_port: u16) -> Result<String, String> {
    Ok(
        rewrite_head_kwarg(text, ":grpc-port", &new_port.to_string())?
            .unwrap_or_else(|| text.to_string()),
    )
}

/// Mint a fresh id for every component in the generated block of
/// `text` and rewrite every mention of the old ids: the `:id` of
/// each `%make-*` form, and both endpoints of each `(connect a b)`.
///
/// Component ids are enterprise-unique — `make.rs`'s `id_or_next`
/// refuses an explicit id any other microgrid already owns — and a
/// generated block pins an explicit `:id` on every component. So a
/// copy of a file whose original is live and populated needs ids of
/// its own, or it dies on "component id X is already registered in
/// microgrid Y". `next_id` supplies them; [`Config::load_as`] passes
/// the enterprise allocator, the same counter `MicrogridSite::next_id`
/// draws from, so the fresh ids collide with nothing.
///
/// Only forms inside the `:topology` lambda are touched. The
/// `(make-microgrid … :id …)` head keeps whatever id it carries —
/// that one is [`rewrite_id`]'s business. `text` may be a whole
/// managed file (the script section rides through untouched) or a
/// bare generated block.
///
/// Errors when a `(connect a b)` names a number no `%make-*` form in
/// the block declared: that block is already inconsistent, and
/// splicing half of it would turn a loud failure into a silently
/// mis-wired copy.
pub fn remap_component_ids(text: &str, mut next_id: impl FnMut() -> u64) -> Result<String, String> {
    let parsed = parse(text)?;
    let block = parsed.generated.as_deref().unwrap_or(text);
    let remapped = remap_in_block(block, &mut next_id)?;
    Ok(match &parsed.generated {
        Some(_) => compose(&remapped, &parsed.script),
        None => remapped,
    })
}

/// The body of [`remap_component_ids`], on a bare generated block.
fn remap_in_block(block: &str, next_id: &mut impl FnMut() -> u64) -> Result<String, String> {
    use std::collections::HashMap;

    let cst = tulisp_fmt::parse(block).map_err(|e| format!("failed to parse block: {e:?}"))?;
    let head = microgrid_form(&cst)?;
    // A head with no `:topology` lambda declares no components, so
    // there is nothing to remint.
    let Some(CstNode::List {
        children: forms, ..
    }) = kwarg_value(head, ":topology")
    else {
        return Ok(block.to_string());
    };

    // Pass 1: every id mention, with its byte span, in textual order.
    let mut declared: Vec<(std::ops::Range<usize>, u64)> = Vec::new();
    let mut referenced: Vec<(std::ops::Range<usize>, u64)> = Vec::new();
    collect_component_ids(forms, &mut declared, &mut referenced);

    // Pass 2: allocate old → new in declaration order, then splice
    // every mention (declarations and connect endpoints alike).
    let mut fresh: HashMap<u64, u64> = HashMap::new();
    let mut splices: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for (span, old) in declared {
        let new = *fresh.entry(old).or_insert_with(&mut *next_id);
        splices.push((span, new.to_string()));
    }
    for (span, old) in referenced {
        let new = fresh.get(&old).ok_or_else(|| {
            format!("connect references component id {old} not declared in this block")
        })?;
        splices.push((span, new.to_string()));
    }

    // Back-to-front, so an earlier splice's length change can never
    // move a span we have not applied yet.
    splices.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
    let mut out = block.to_string();
    for (span, text) in splices {
        out.replace_range(span, &text);
    }
    Ok(out)
}

/// Every component category, by the suffix its constructors share.
/// `src/lisp/make.rs` is the source of truth — it registers one
/// `%make-<name>` defun per entry here, and `sim/defaults.lisp` the
/// matching `make-<name>` wrapper. Adding a component type means
/// adding it here too, or a copy of a microgrid using it will keep
/// the original's ids for those components. The palette test in
/// `make.rs` reads it as well: every entry needs an Add-component
/// button in `ui-assets/index.html`.
///
/// The list is CLOSED on purpose. A prefix test (`starts_with
/// "make-"`) would also match a user-defined helper — nothing stops
/// a hand-edited `:topology` calling `(make-load-profile :id 3 …)` —
/// and renumbering that `:id`, which means whatever the helper says
/// it means, would corrupt the copy. Worse, counting the helper as a
/// declaration would let a genuinely undeclared `connect` endpoint
/// resolve, turning a loud error into a silently mis-wired copy.
pub(super) const COMPONENT_MAKE_FNS: &[&str] = &[
    "grid-connection-point",
    "meter",
    "battery",
    "battery-inverter",
    "solar-inverter",
    "ev-charger",
    "chp",
    "wind-turbine",
    "steam-boiler",
    "power-transformer",
    "breaker",
];

/// Does a form head build a component? Both spellings count: the
/// `%make-*` primitives macrocosim renders, and the public `make-*`
/// wrappers a person may well have hand-written into `:topology`
/// (it is ordinary lisp — nothing stops them, and they declare an
/// `:id` just the same). Missing the wrapper spelling would leave
/// those components on the original's ids and make a `connect` to
/// one look undeclared.
///
/// `make-microgrid` is not in the set, so the block's own head can
/// never be mistaken for a component even if a hand-mangled block
/// nests one inside its own `:topology`.
fn declares_a_component(head: &str) -> bool {
    let name = head
        .strip_prefix("%make-")
        .or_else(|| head.strip_prefix("make-"));
    name.is_some_and(|name| COMPONENT_MAKE_FNS.contains(&name))
}

/// Walk `forms` — the `:topology` lambda's children — collecting the
/// span and current value of every component id mention: `declared`
/// gets the `:id` of each component-constructing form, `referenced`
/// gets both endpoints of each `(connect a b)`.
///
/// Recursive, so a hand-edited block that wraps its components in a
/// `progn` (or nests them the pre-flat way) is walked too; a
/// macrocosim-rendered block is flat and never needs the descent.
fn collect_component_ids(
    forms: &[CstNode],
    declared: &mut Vec<(std::ops::Range<usize>, u64)>,
    referenced: &mut Vec<(std::ops::Range<usize>, u64)>,
) {
    for node in forms {
        let CstNode::List { children, .. } = node else {
            continue;
        };
        let mut atoms = children.iter().filter_map(|c| match c {
            CstNode::Atom { text, span } => Some((text.as_str(), span)),
            _ => None,
        });
        match atoms.next() {
            Some((name, _)) if declares_a_component(name) => {
                // A form with no `:id`, or a non-numeric one, is left
                // alone: the loader auto-allocates for it anyway.
                if let Some(CstNode::Atom { text, span }) = kwarg_value(children, ":id")
                    && let Ok(old) = text.parse::<u64>()
                {
                    declared.push((span.clone(), old));
                }
            }
            Some(("connect", _)) => {
                // Everything after the head atom. A non-numeric
                // endpoint is a variable reference, not an id we can
                // map — leave it as written.
                for (text, span) in atoms {
                    if let Ok(old) = text.parse::<u64>() {
                        referenced.push((span.clone(), old));
                    }
                }
            }
            _ => {}
        }
        collect_component_ids(children, declared, referenced);
    }
}

/// The microgrid id declared by a generated `block` (the text
/// `parse` hands back in [`ParsedFile::generated`], markers already
/// stripped) — `None` when the block carries no `(make-microgrid …
/// :id N …)` head it can read.
///
/// The loader uses this to know which microgrid's scope the script
/// section belongs to when the block's own eval registered nothing
/// new, which is what a same-file reload looks like (the entry is
/// reused in place, so the registry key set doesn't move).
pub fn head_id(block: &str) -> Option<u64> {
    let span = head_kwarg_span(block, ":id").ok().flatten()?;
    block[span.0..span.1].trim().parse().ok()
}

/// Replace the value of `kwarg` in the `(make-microgrid …)` form of
/// `text`'s generated block with `value`, and return the whole file
/// text. `Ok(None)` means the form carries no such kwarg — the
/// caller decides whether that is an error or a no-op.
///
/// A byte splice over the original text rather than a re-render, so
/// every other kwarg, the component forms, the comments and the
/// hand-written script section come through exactly as they were.
fn rewrite_head_kwarg(text: &str, kwarg: &str, value: &str) -> Result<Option<String>, String> {
    let parsed = parse(text)?;
    let Some(block) = parsed.generated else {
        return Err(format!(
            "cannot rewrite {kwarg} in an unmanaged file — edit it by hand"
        ));
    };
    let Some((start, end)) = head_kwarg_span(&block, kwarg)? else {
        return Ok(None);
    };

    let mut new_block = String::with_capacity(block.len());
    new_block.push_str(&block[..start]);
    new_block.push_str(value);
    new_block.push_str(&block[end..]);

    Ok(Some(compose(&new_block, &parsed.script)))
}

/// Byte range of `kwarg`'s value atom inside a generated `block`, or
/// `None` when the `(make-microgrid …)` head carries no such kwarg.
/// Errors only when the block does not parse, or holds no
/// `(make-microgrid …)` form at all.
fn head_kwarg_span(block: &str, kwarg: &str) -> Result<Option<(usize, usize)>, String> {
    let cst = tulisp_fmt::parse(block).map_err(|e| format!("failed to parse block: {e:?}"))?;
    let form = microgrid_form(&cst)?;
    // Only the head's own children are walked — a nested
    // `(%make-* :id …)` lives inside a child List, so component ids
    // can never be hit here.
    Ok(match kwarg_value(form, kwarg) {
        Some(CstNode::Atom { span, .. }) => Some((span.start, span.end)),
        // Missing, or a value that isn't a plain atom (a list, a
        // quoted form): nothing this splice can rewrite.
        _ => None,
    })
}

/// The children of the `(make-microgrid …)` form in a parsed
/// generated block. A generated block holds exactly one, and every
/// rewrite in this module works off its children.
fn microgrid_form(cst: &tulisp_fmt::cst::Cst) -> Result<&[CstNode], String> {
    cst.nodes
        .iter()
        .find_map(|n| match n {
            CstNode::List { children, .. } => {
                let first_atom = children.iter().find_map(|c| match c {
                    CstNode::Atom { text, .. } => Some(text.as_str()),
                    _ => None,
                });
                (first_atom == Some("make-microgrid")).then_some(children.as_slice())
            }
            _ => None,
        })
        .ok_or_else(|| "no (make-microgrid …) form found in the generated block".to_string())
}

/// The node holding `kwarg`'s value inside a form's `children`: the
/// first real node after the `kwarg` atom, skipping trivia (comments
/// and line breaks can sit between a keyword and its value).
/// `None` when the form carries no such keyword.
fn kwarg_value<'a>(children: &'a [CstNode], kwarg: &str) -> Option<&'a CstNode> {
    let mut seen_kwarg = false;
    for child in children {
        if seen_kwarg {
            match child {
                CstNode::Comment { .. } | CstNode::LineBreak { .. } => continue,
                value => return Some(value),
            }
        }
        if matches!(child, CstNode::Atom { text, .. } if text == kwarg) {
            seen_kwarg = true;
        }
    }
    None
}

/// Render the generated block for a live microgrid: a
/// `(make-microgrid …)` form that reconstructs the definition, every
/// registered component (in `site.components()` order), and every
/// connection — such that re-evaluating the result reproduces the
/// same site (see the round-trip test below).
pub fn render_block(
    def: &crate::sim::microgrids::MicrogridDef,
    site: &crate::sim::MicrogridSite,
) -> String {
    use std::fmt::Write as _;

    let head = render_head(def);

    let mut body = String::new();
    for c in site.components().iter() {
        let id = c.id();
        write!(body, "\n    ({} :id {}", c.make_fn(), id).unwrap();
        if let Some(name) = site.name_override(id) {
            write!(
                body,
                " :name \"{}\"",
                crate::lisp::escape_lisp_string(&name)
            )
            .unwrap();
        }
        for (k, v) in c.constructor_kwargs() {
            write!(body, " {k} {v}").unwrap();
        }
        let mode = site.operational_mode(id);
        if mode != crate::sim::OperationalMode::Unspecified {
            write!(body, " :operational-mode '{mode}").unwrap();
        }
        body.push(')');
    }
    for (a, b) in site.all_connections() {
        write!(body, "\n    (connect {a} {b})").unwrap();
    }

    if body.is_empty() {
        format!("{head}\n  :topology\n  (lambda ()\n    nil))")
    } else {
        format!("{head}\n  :topology\n  (lambda (){body}))")
    }
}

/// Render the generated block for a microgrid with no components
/// yet — used by the create endpoint before any component exists.
pub fn render_empty_block(def: &crate::sim::microgrids::MicrogridDef) -> String {
    format!("{}\n  :topology\n  (lambda ()\n    nil))", render_head(def))
}

/// The `(make-microgrid :id … :name "…" :grpc-port … [:tso "…"]`
/// head shared by [`render_block`] and [`render_empty_block`],
/// without a trailing space or the `:topology` clause.
fn render_head(def: &crate::sim::microgrids::MicrogridDef) -> String {
    use std::fmt::Write as _;
    let mut head = format!(
        "(make-microgrid :id {} :name \"{}\" :grpc-port {}",
        def.id,
        crate::lisp::escape_lisp_string(&def.name),
        def.grpc_port,
    );
    if let Some(tso) = &def.tso {
        write!(head, " :tso \"{}\"", crate::lisp::escape_lisp_string(tso)).unwrap();
    }
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANAGED: &str = "\
;;; macrocosim:generated — rewritten by macrocosim, do not edit
(make-microgrid :id 2201 :name \"a\" :grpc-port 8810
  :topology
  (lambda ()
    (%make-meter :id 5)))
;;; macrocosim:end
;; Anything below is yours.
(setq x 1)
";

    #[test]
    fn parse_splits_managed_file_into_block_and_script() {
        let p = parse(MANAGED).unwrap();
        let block = p.generated.expect("managed");
        assert!(block.contains("(make-microgrid :id 2201"));
        assert!(!block.contains("macrocosim:generated"));
        assert!(!block.contains("macrocosim:end"));
        assert!(p.script.contains("(setq x 1)"));
        assert!(!p.script.contains("make-microgrid"));
    }

    #[test]
    fn parse_treats_markerless_text_as_unmanaged_script() {
        let p = parse("(make-microgrid :id 1 :topology (lambda () nil))\n").unwrap();
        assert!(p.generated.is_none());
        assert!(p.script.contains("make-microgrid"));
    }

    #[test]
    fn parse_rejects_marker_not_at_top_and_missing_end() {
        assert!(parse("(setq x 1)\n;;; macrocosim:generated\nnil\n;;; macrocosim:end\n").is_err());
        assert!(parse(";;; macrocosim:generated\n(make-microgrid :id 1)\n").is_err());
        // A second begin inside the block is an error too.
        assert!(
            parse(";;; macrocosim:generated\n;;; macrocosim:generated\nnil\n;;; macrocosim:end\n")
                .is_err()
        );
    }

    #[test]
    fn compose_then_parse_round_trips() {
        let block = "(make-microgrid :id 7 :grpc-port 8807\n  :topology (lambda () nil))";
        let script = ";; mine\n(setq y 2)\n";
        let text = compose(block, script);
        assert!(text.starts_with(GENERATED_BEGIN));
        let p = parse(&text).unwrap();
        assert_eq!(p.generated.as_deref().map(str::trim), Some(block.trim()));
        assert_eq!(p.script, script);
    }

    #[test]
    fn rewrite_id_replaces_only_the_microgrid_id() {
        let out = rewrite_id(MANAGED, 2299).unwrap();
        assert!(out.contains("(make-microgrid :id 2299"));
        assert!(
            out.contains("(%make-meter :id 5)"),
            "component ids untouched"
        );
        assert!(out.contains("(setq x 1)"), "script untouched");
        // Unmanaged text is refused.
        assert!(rewrite_id("(make-microgrid :id 1)", 2).is_err());
    }

    #[test]
    fn rewrite_grpc_port_replaces_only_the_port() {
        let out = rewrite_grpc_port(MANAGED, 8899).unwrap();
        assert!(out.contains(":grpc-port 8899"), "{out}");
        assert!(
            out.contains("(make-microgrid :id 2201"),
            "the id is untouched"
        );
        assert!(
            out.contains("(%make-meter :id 5)"),
            "component forms untouched"
        );
        assert!(out.contains("(setq x 1)"), "script untouched");
        // Still one well-formed managed file.
        let parsed = parse(&out).unwrap();
        assert!(parsed.generated.is_some());
    }

    #[test]
    fn rewrite_grpc_port_leaves_a_portless_head_alone() {
        // No `:grpc-port` kwarg: the loader auto-allocates a free
        // port, so there is nothing to rewrite and nothing to fail.
        let text = compose(
            "(make-microgrid :id 3 :name \"p\"\n  :topology (lambda () nil))",
            ";; mine\n",
        );
        assert_eq!(rewrite_grpc_port(&text, 8899).unwrap(), text);
        // Unmanaged text is refused, same as rewrite_id.
        assert!(rewrite_grpc_port("(make-microgrid :id 1 :grpc-port 8800)", 8899).is_err());
    }

    #[test]
    fn remap_component_ids_renumbers_makes_and_connects_together() {
        let text = compose(
            "(make-microgrid :id 2201 :name \"a\" :grpc-port 8810\n  :topology\n  (lambda ()\n\
             \x20   (%make-grid-connection-point :id 70)\n\
             \x20   (%make-meter :id 71 :name \"main\")\n\
             \x20   (%make-battery-inverter :id 72)\n\
             \x20   (connect 70 71)\n\
             \x20   (connect 71 72)))",
            ";; mine\n(set-meter-power 71 1000.0)\n",
        );
        let mut next = 900u64;
        let out = remap_component_ids(&text, || {
            next += 1;
            next
        })
        .unwrap();

        // Fresh ids in declaration order, and the connects moved with
        // them — same shape, different numbers.
        assert!(
            out.contains("(%make-grid-connection-point :id 901)"),
            "{out}"
        );
        assert!(
            out.contains("(%make-meter :id 902 :name \"main\")"),
            "{out}"
        );
        assert!(out.contains("(%make-battery-inverter :id 903)"), "{out}");
        assert!(out.contains("(connect 901 902)"), "{out}");
        assert!(out.contains("(connect 902 903)"), "{out}");
        assert!(!out.contains(" 70"), "no old id survives: {out}");
        // The head's own id is not a component id.
        assert!(out.contains("(make-microgrid :id 2201"), "{out}");
        // The script section is the author's, ids and all.
        assert!(out.contains("(set-meter-power 71 1000.0)"), "{out}");
        // Still one well-formed managed file.
        assert!(parse(&out).unwrap().generated.is_some());
    }

    #[test]
    fn remap_component_ids_rejects_a_connect_to_an_undeclared_id() {
        let text = compose(
            "(make-microgrid :id 1 :topology\n  (lambda ()\n    (%make-meter :id 5)\n\
             \x20   (connect 5 6)))",
            "",
        );
        let err = remap_component_ids(&text, || 900).unwrap_err();
        assert!(err.contains("component id 6"), "{err}");
        // An empty topology has nothing to remint and is left alone.
        let empty = compose("(make-microgrid :id 1 :topology (lambda () nil))", "");
        assert_eq!(remap_component_ids(&empty, || 900).unwrap(), empty);
    }

    /// `:topology` is ordinary lisp, so a hand-edited block may build
    /// its components with the public `make-*` wrappers rather than
    /// the `%make-*` primitives macrocosim renders. Those declare
    /// component ids just the same, so the re-mint has to see them —
    /// otherwise they keep the original's ids (an instant collision)
    /// and a `connect` to one is refused as "not declared in this
    /// block", which is both wrong and misleading.
    #[test]
    fn remap_component_ids_sees_the_public_wrapper_spellings() {
        let text = compose(
            "(make-microgrid :id 2201 :name \"a\" :grpc-port 8810\n  :topology\n  (lambda ()\n\
             \x20   (%make-grid-connection-point :id 1)\n\
             \x20   (make-meter :id 71)\n\
             \x20   (connect 1 71)))",
            "",
        );
        let mut next = 900u64;
        let out = remap_component_ids(&text, || {
            next += 1;
            next
        })
        .unwrap();
        assert!(
            out.contains("(%make-grid-connection-point :id 901)"),
            "{out}"
        );
        assert!(out.contains("(make-meter :id 902)"), "{out}");
        assert!(out.contains("(connect 901 902)"), "{out}");
        // The head is `make-microgrid`, which is NOT a component
        // constructor — its id must survive the wrapper match.
        assert!(out.contains("(make-microgrid :id 2201"), "{out}");
    }

    /// The closed set has to stay in step with `make.rs`, which is
    /// the source of truth — a name that drifts out of it silently
    /// stops being reminted. Every entry must name a real pair of
    /// constructors that take an `:id`, which is the whole basis of
    /// the re-mint, so each one is actually built here.
    #[test]
    fn every_component_make_fn_is_a_real_constructor() {
        use super::super::test_support::config_with;
        let (cfg, _dir) = config_with("nil");
        let mut id = 7000u64;
        for name in COMPONENT_MAKE_FNS {
            for spelling in [format!("%make-{name}"), format!("make-{name}")] {
                id += 1;
                cfg.eval_silent(&format!("({spelling} :id {id})"))
                    .unwrap_or_else(|e| {
                        panic!("{spelling} is in COMPONENT_MAKE_FNS but does not build: {e}")
                    });
            }
        }
    }

    /// The constructor set is CLOSED. A user-defined helper whose
    /// name merely starts with `make-` is not a component, and its
    /// `:id` kwarg means whatever the helper says it means — renaming
    /// that number would corrupt the copy, and counting it as
    /// "declared" would let a genuinely undeclared `connect` slip
    /// through as a silently mis-wired edge instead of erroring.
    #[test]
    fn remap_component_ids_ignores_a_lookalike_helper() {
        let text = compose(
            "(make-microgrid :id 2201 :topology\n  (lambda ()\n\
             \x20   (%make-meter :id 5)\n\
             \x20   (make-load-profile :id 3 :shape 'flat)))",
            "",
        );
        let out = remap_component_ids(&text, || 900).unwrap();
        assert!(out.contains("(%make-meter :id 900)"), "{out}");
        assert!(
            out.contains("(make-load-profile :id 3 :shape 'flat)"),
            "a helper's own :id is not a component id: {out}"
        );
        // And it is not "declared", so a connect to it still errors
        // loudly rather than being wired to something arbitrary.
        let connected = compose(
            "(make-microgrid :id 2201 :topology\n  (lambda ()\n\
             \x20   (%make-meter :id 5)\n\
             \x20   (make-load-profile :id 3)\n\
             \x20   (connect 5 3)))",
            "",
        );
        let err = remap_component_ids(&connected, || 900).unwrap_err();
        assert!(err.contains("component id 3"), "{err}");
    }

    #[test]
    fn write_atomic_replaces_content() {
        let dir = std::env::temp_dir().join(format!("mc-mgfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.lisp");
        write_atomic(&path, "one").unwrap();
        write_atomic(&path, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert!(!path.with_extension("lisp.tmp").exists());
    }

    #[test]
    fn render_block_round_trips_through_a_fresh_config() {
        use super::super::test_support::config_with;
        let body = r#"
(make-microgrid :id 2205 :name "rt" :grpc-port 8815 :tso "TN"
  :topology
  (lambda ()
    (%make-grid-connection-point :id 1 :rated-fuse-current 100)
    (%make-meter :id 2 :name "main" :power 1500.0 :interval-s 0.5)
    (%make-meter :id 3 :hidden t)
    (%make-battery-inverter :id 4 :rated-lower -8000.0 :rated-upper 8000.0
                            :reactive-pf-limit 0)
    (%make-battery :id 5 :capacity 50000.0 :initial-soc 20.0)
    (%make-solar-inverter :id 6 :sunlight% 40.0)
    (%make-ev-charger :id 7 :resume-on-recovery t :phases 1 :idle 'full)
    (%make-chp :id 8 :name "chp")
    (%make-meter :id 9 :operational-mode 'inactive)
    (%make-meter :id 10 :power 2000.0 :reactive-power 500.0)
    (%make-meter :id 11 :power 2000.0 :power-factor 0.9 :leading t)
    ;; No `:sunlight%` at all — a weather-FOLLOWING PV inverter,
    ;; whose rendering is the absence of the kwarg. Round-tripping
    ;; it pins that omission: a render that emitted a number here
    ;; (the cache's seed, say) would reload as a driven inverter and
    ;; the `expr` assertion below would find no "weather" marker.
    (%make-solar-inverter :id 12)
    (connect 1 2) (connect 2 4) (connect 4 5)
    (connect 2 6) (connect 2 7) (connect 2 8) (connect 2 3) (connect 2 9)
    (connect 2 10) (connect 2 11) (connect 2 12)))
"#;
        let (cfg, _dir) = config_with(body);
        let (def, site) = {
            let reg = cfg.microgrids();
            let r = reg.lock();
            let e = r.get(&2205).unwrap();
            (e.def.clone(), e.site.clone())
        };
        // Drive the Follow inverter and put it straight back, so what
        // gets rendered below is a slot that has been all the way
        // round: built following, poked to a constant (unrenderable
        // while it holds one — there is no constructed kwarg to write
        // it into), cleared back to Follow, and renderable again.
        let pv = site.get(12).expect("the weather-following inverter");
        pv.sunlight_drive().unwrap().set_sunlight_pct(30.0);
        assert!(
            pv.has_unrenderable_source(),
            "a constant poked over an inverter built with no :sunlight% has nowhere to render"
        );
        pv.sunlight_drive().unwrap().clear_sunlight_source();
        assert!(
            !pv.has_unrenderable_source(),
            "cleared back to Follow, the omitted kwarg IS the rendering"
        );

        let block = render_block(&def, &site);
        // Evaluate the rendered block in a second, fresh Config.
        let (cfg2, _dir2) = config_with(&block);
        let reg2 = cfg2.microgrids();
        let r2 = reg2.lock();
        let e2 = r2
            .get(&2205)
            .expect("rendered block re-registers the microgrid");
        assert_eq!(e2.def.name, "rt");
        assert_eq!(e2.def.grpc_port, 8815);
        assert_eq!(e2.def.tso.as_deref(), Some("TN"));
        // Same components, same constructor forms, same names, same edges.
        let sig = |site: &crate::sim::MicrogridSite| {
            let mut v: Vec<String> = site
                .components()
                .iter()
                .map(|c| {
                    format!(
                        "{} {} {:?} {:?} {:?}",
                        c.id(),
                        c.make_fn(),
                        c.constructor_kwargs(),
                        site.name_override(c.id()),
                        site.operational_mode(c.id())
                    )
                })
                .collect();
            v.sort();
            (v, site.all_connections())
        };
        assert_eq!(sig(&site), sig(&e2.site));
        // `sig` compares the two sites against EACH OTHER, so a
        // constructor that dropped `:resume-on-recovery` would lose it
        // on both loads and still match. Assert the flag itself made it
        // through the render → reload, so the persistence path is
        // pinned rather than just its own symmetry.
        let ev2 = e2.site.get(7).expect("the EV charger survives the reload");
        assert!(
            ev2.constructor_kwargs()
                .contains(&(":resume-on-recovery", "t".to_string())),
            ":resume-on-recovery must survive render → reload, got {:?}",
            ev2.constructor_kwargs(),
        );
        let kw = ev2.constructor_kwargs();
        assert!(
            kw.contains(&(":phases", "1".to_string())),
            ":phases must survive render → reload, got {kw:?}"
        );
        assert!(
            kw.contains(&(":idle", "'full".to_string())),
            ":idle must survive render → reload, got {kw:?}"
        );
        assert!(
            ev2.ev_port().unwrap().ev_info().is_none(),
            "a plugged car is runtime state, never rendered"
        );
        // …and the reloaded PV inverter is really following the sky
        // again, not sitting on a constant the render invented. Same
        // components/kwargs above only proves the kwarg stayed
        // absent; this proves what the absence reconstructs to.
        let pv2 = e2.site.get(12).expect("the inverter survives the reload");
        assert_eq!(
            pv2.sunlight_drive()
                .expect("PV has a sunlight knob")
                .sunlight_reading()
                .expr,
            Some("weather".to_string()),
            "no :sunlight% must reload as a weather-following slot"
        );
        // Rendering the reloaded site is byte-stable.
        assert_eq!(block, render_block(&e2.def, &e2.site));
    }

    /// A managed file (or snapshot, or `enterprise.lisp`) the previous
    /// binary wrote spells its charger with the pack kwargs the
    /// charger used to own. It must still load — the whole microgrid
    /// rides on that one form — and render back out without them.
    #[test]
    fn a_pre_branch_charger_form_still_loads_and_renders() {
        use super::super::test_support::config_with;
        let body = r#"
(make-microgrid :id 2206 :name "old" :grpc-port 8816
  :topology
  (lambda ()
    (%make-meter :id 1)
    (%make-ev-charger :id 2 :rated-lower 0.0 :rated-upper 22000.0
                      :initial-soc 92.0 :soc-lower 0.0 :soc-upper 100.0
                      :soc-protect-margin 10.0 :capacity 30000.0
                      :command-delay-ms 500 :ramp-rate 3000.0
                      :stream-jitter-pct 10.0)
    (connect 1 2)))
"#;
        let (cfg, _dir) = config_with(body);
        let (def, site) = {
            let reg = cfg.microgrids();
            let r = reg.lock();
            let e = r.get(&2206).expect("the old form registers the microgrid");
            (e.def.clone(), e.site.clone())
        };
        let ev = site.get(2).expect("the charger was built");
        assert!(
            ev.ev_port().unwrap().ev_info().is_none(),
            "the pack kwargs buy no car: the charger starts empty"
        );
        // The kwargs that DID survive are still honoured, so the
        // ignore is surgical rather than a swallowed form.
        let kw = ev.constructor_kwargs();
        assert!(
            kw.contains(&(":rated-upper", "22000.0".to_string())),
            "{kw:?}"
        );
        // And what comes back out carries none of the retired names,
        // so one load-and-save cleans the file up.
        let block = render_block(&def, &site);
        for retired in [
            ":capacity",
            ":initial-soc",
            ":soc-lower",
            ":soc-upper",
            ":soc-protect-margin",
        ] {
            assert!(
                !block.contains(retired),
                "{retired} must not be rendered back out:\n{block}"
            );
        }
        let (cfg2, _dir2) = config_with(&block);
        let reg2 = cfg2.microgrids();
        let r2 = reg2.lock();
        assert!(
            r2.get(&2206).is_some(),
            "the rendered block re-registers the microgrid"
        );
    }

    /// `:device-delay-s` survives a render and reload when it is
    /// set, and a file written without it renders without it.
    #[test]
    fn device_delay_s_round_trips_through_a_managed_file() {
        use super::super::test_support::config_with;
        let body = r#"
(make-microgrid :id 2207 :name "dd" :grpc-port 8817
  :topology
  (lambda ()
    (%make-meter :id 1)
    (%make-battery-inverter :id 2 :device-delay-s 0.25)
    (%make-steam-boiler :id 3)
    (connect 1 2) (connect 1 3)))
"#;
        let (cfg, _dir) = config_with(body);
        let (def, site) = {
            let reg = cfg.microgrids();
            let r = reg.lock();
            let e = r.get(&2207).unwrap();
            (e.def.clone(), e.site.clone())
        };
        let block = render_block(&def, &site);
        assert_eq!(block.matches(":device-delay-s").count(), 1, "{block}");
        assert!(block.contains(":device-delay-s 0.25"), "{block}");
        let (cfg2, _dir2) = config_with(&block);
        let reg2 = cfg2.microgrids();
        let r2 = reg2.lock();
        let site2 = r2.get(&2207).unwrap().site.clone();
        let kw = site2.get(2).unwrap().constructor_kwargs();
        assert!(
            kw.contains(&(":device-delay-s", "0.25".to_string())),
            "{kw:?}"
        );
    }
}
