//! `/api/defaults` — read every `*-defaults` plist out of the
//! running interpreter, pretty-printed for the side-panel editor.

use axum::extract::State;
use serde::Serialize;

// `DEFAULT_CATEGORIES` names the `*-defaults` alists this endpoint
// walks out of the running interpreter. Shared with the
// `enterprise.lisp` renderer, so both see the same set. An unbound
// category is skipped — `eval_silent` on an unbound symbol fails and
// the entry is dropped.
use crate::lisp::{Config, DEFAULT_CATEGORIES};
use crate::ui::api::{ApiError, Json};

/// One per `*-defaults` alist defined in `sim/defaults.lisp`. The
/// `var_name` is the actual Lisp variable; `value` is its current
/// printed form (a stringified alist), readable / editable as raw
/// Lisp by the UI.
#[derive(Serialize)]
pub(in crate::ui) struct DefaultsEntry {
    category: &'static str,
    var_name: String,
    value: String,
}

#[derive(Serialize)]
pub(in crate::ui) struct DefaultsResponse {
    entries: Vec<DefaultsEntry>,
}

pub(in crate::ui) async fn defaults(
    State(config): State<Config>,
) -> Result<Json<DefaultsResponse>, ApiError> {
    // Read each *-defaults variable via eval_silent so reading the
    // current state doesn't itself look like an edit. spawn_blocking
    // because eval acquires the std-RwLock-backed ctx.
    let entries = super::blocking(move || {
        let mut out = Vec::new();
        for cat in DEFAULT_CATEGORIES {
            let var = format!("{cat}-defaults");
            // Variables that aren't bound just get skipped.
            if let Ok(value) = config.defaults_silent(&var) {
                // One `:key value` pair per line. tulisp-fmt's
                // width-based breaking splits keys from their values
                // at side-panel widths, which reads terribly in the
                // textarea; the raw Display form is the fallback for
                // anything that isn't a flat plist.
                let formatted = format_plist_pairs(&value).unwrap_or(value);
                out.push(DefaultsEntry {
                    category: cat,
                    var_name: var,
                    value: formatted,
                });
            }
        }
        out
    })
    // A panicked interpreter walk is a 500 (via `blocking`), not an
    // empty-but-200 list the UI cannot tell apart from "no defaults
    // defined".
    .await?;
    Ok(Json(DefaultsResponse { entries }))
}

/// Render a flat plist as one `:key value` pair per line:
/// `(:a 1 :b 2)` → `(:a 1\n :b 2)`. Returns `None` for anything that
/// isn't a flat even-length plist with keyword keys — the caller
/// falls back to the raw form. Nested lists and strings stay intact
/// as single value tokens.
fn format_plist_pairs(src: &str) -> Option<String> {
    let inner = src.trim().strip_prefix('(')?.strip_suffix(')')?;
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let (mut depth, mut in_string, mut escaped) = (0u32, false, false);
    for c in inner.chars() {
        if in_string {
            current.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                current.push(c);
            }
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.checked_sub(1)?;
                current.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    if in_string || depth != 0 || tokens.is_empty() || !tokens.len().is_multiple_of(2) {
        return None;
    }
    if !tokens.iter().step_by(2).all(|t| t.starts_with(':')) {
        return None;
    }
    let pairs: Vec<String> = tokens
        .chunks(2)
        .map(|pair| format!("{} {}", pair[0], pair[1]))
        .collect();
    Some(format!("({})", pairs.join("\n ")))
}

#[cfg(test)]
mod tests {
    use super::format_plist_pairs;

    #[test]
    fn flat_plist_renders_one_pair_per_line() {
        assert_eq!(
            format_plist_pairs("(:a 1 :b 2.0 :c ok)").as_deref(),
            Some("(:a 1\n :b 2.0\n :c ok)")
        );
    }

    #[test]
    fn multiline_input_renormalizes() {
        assert_eq!(
            format_plist_pairs("(:a 1\n                :b\n                2)").as_deref(),
            Some("(:a 1\n :b 2)")
        );
    }

    #[test]
    fn nested_and_string_values_stay_single_tokens() {
        assert_eq!(
            format_plist_pairs("(:bounds (1 2) :name \"a b\")").as_deref(),
            Some("(:bounds (1 2)\n :name \"a b\")")
        );
    }

    #[test]
    fn non_plists_fall_back() {
        assert_eq!(format_plist_pairs("(:a 1 :b)"), None);
        assert_eq!(format_plist_pairs("(1 2 3 4)"), None);
        assert_eq!(format_plist_pairs("plain"), None);
    }

    #[test]
    fn malformed_inputs_fall_back() {
        assert_eq!(format_plist_pairs("(:a 1"), None);
        assert_eq!(format_plist_pairs("(:a 1))"), None);
        assert_eq!(format_plist_pairs("(:a \"unterminated)"), None);
        assert_eq!(format_plist_pairs("()"), None);
    }
}
