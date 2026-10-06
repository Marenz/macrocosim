// REPL editor syntax helpers: completion vocabulary, paren-depth
// rainbow highlighter, and the same-line-indent logic the textarea's
// Enter handler uses. Pure data + string transforms — no DOM, no
// async, no app-state coupling. Imported by app.js's setupRepl.

// Symbol vocabulary used by the Tab-completion popup. Drawn from
// Rust defuns + the standard tulisp surface. Lookups walk the list
// and pick prefix matches — case-sensitive so `make-*` and lisp
// keywords keep their case.
export const COMPLETIONS = [
  // Microgrid topology mutations
  "connect",
  "disconnect",
  "remove-component",
  "rename-component",
  "reset-microgrid",
  // Make-* primitives
  "%make-grid-connection-point",
  "%make-meter",
  "%make-battery",
  "%make-battery-inverter",
  "%make-solar-inverter",
  "%make-ev-charger",
  "%make-chp",
  "%make-wind-turbine",
  "%make-steam-boiler",
  "%make-power-transformer",
  "%make-breaker",
  // Make-* lisp wrappers
  "make-grid-connection-point",
  "make-meter",
  "make-battery",
  "make-battery-inverter",
  "make-solar-inverter",
  "make-ev-charger",
  "make-chp",
  "make-wind-turbine",
  "make-steam-boiler",
  "make-power-transformer",
  "make-breaker",
  // Setters
  "set-component-health",
  "set-component-telemetry-mode",
  "set-component-command-mode",
  "set-component-operational-mode",
  "set-battery-soc",
  "plug-ev",
  "%plug-ev",
  "unplug-ev",
  "ev-info",
  "ev-presets",
  "set-active-power",
  "set-reactive-power",
  "augment-active-bounds",
  "augment-reactive-bounds",
  "set-meter-power",
  "clear-meter-power",
  "clear-meter-reactive",
  "set-solar-sunlight",
  "clear-solar-sunlight",
  "set-reactive-pf-limit",
  "set-reactive-apparent-va",
  "set-boiler-demand",
  "set-boiler-pressure",
  "set-physics-tick-s",
  "set-physics-tick-ms",
  "set-voltage-per-phase",
  "set-frequency",
  // Site weather — the sky the solar inverters follow. Same doors the
  // weather panel drives over /api/mg/{mg}/weather.
  "make-weather",
  "set-weather",
  "pass-cloud",
  "weather-status",
  // Metadata
  "set-enterprise-id",
  "set-microgrid-name",
  "set-microgrid-tso",
  "set-socket-addr",
  "set-default-request-lifetime-s",
  "set-default-augment-lifetime-s",
  "set-default-request-lifetime-ms",
  "set-default-augment-lifetime-ms",
  // Scenarios — lifecycle, journal + reporter, CSV recording.
  // Lifecycle defuns are Rust-side; the *-end-after / random-*
  // helpers are Lisp wrappers in sim/common.lisp + sim/scenarios.lisp.
  "scenario-start",
  "scenario-stop",
  "scenario-event",
  "scenario-elapsed",
  "scenario-end-after",
  "scenario-end-after-s",
  "scenario-record-csv",
  "scenario-stop-csv",
  "drive-boiler",
  "random-outage",
  "random-pick",
  "random-uniform",
  // Utilities
  "every",
  "run-with-timer",
  "cancel-timer",
  "sleep-for",
  "timerp",
  "now-seconds",
  "window-elapsed",
  "load",
  "load-overrides",
  "watch-file",
  "file-exists-p",
  "reset-state",
  "log.info",
  "log.warn",
  "log.error",
  "log.debug",
  "log.trace",
  "ceiling",
  "floor",
  "random",
  "csv-load",
  "csv-lookup",
  "csv-fields",
  "component-id",
  // Per-category defaults variables
  "grid-defaults",
  "meter-defaults",
  "battery-defaults",
  "battery-inverter-defaults",
  "solar-inverter-defaults",
  "ev-charger-defaults",
  "steam-boiler-defaults",
  "marker-defaults",
  // Common Lisp built-ins
  "defun",
  "defmacro",
  "setq",
  "let",
  "let*",
  "if",
  "when",
  "unless",
  "cond",
  "lambda",
  "progn",
  "quote",
  "list",
  "cons",
  "car",
  "cdr",
  "nth",
  "length",
  "append",
  "reverse",
  "mapcar",
  "dolist",
  "dotimes",
  "while",
  "and",
  "or",
  "not",
  "eq",
  "equal",
  "format",
  "concat",
  "intern",
  "symbol-value",
  "plist-get",
  "alist-get",
  "assoc",
  "boundp",
  "fboundp",
  "null",
  "consp",
  "listp",
  "stringp",
  "numberp",
  "symbolp",
];

// Locate the Lisp identifier the cursor is inside (or just past),
// returning its substring start / end indices for replacement and
// the prefix typed so far. Used by Tab-completion to know what to
// match against COMPLETIONS.
export function wordAtCursor(input) {
  const v = input.value;
  const c = input.selectionStart;
  let start = c;
  // Lisp identifiers: alnum + - _ % . : * (let*, %make-*)
  while (start > 0 && /[a-zA-Z0-9_%\-.:*]/.test(v[start - 1])) start--;
  return { prefix: v.slice(start, c), start, end: c };
}

// Number of rotating paren-depth colours; CSS picks via .paren-N.
const RAINBOW_DEPTHS = 7;

// Symbols that head a list and are syntax keywords rather than
// callable functions. Drives the .repl-special-form class so the
// shape of a form is visible at a glance: `defun`, `let`, `when`
// pop one colour; ordinary function calls get a different one.
const SPECIAL_FORMS = new Set([
  "defun", "defmacro", "defvar", "defconst", "defspecial",
  "let", "let*", "letrec",
  "if", "when", "unless", "cond", "case", "pcase",
  "progn", "prog1", "prog2",
  "lambda", "function",
  "while", "dolist", "dotimes",
  "condition-case", "catch", "throw", "unwind-protect",
  "setq", "setq-default",
  "and", "or", "not",
  "quote",
  "if-let", "when-let", "while-let",
  "save-excursion", "save-restriction", "with-current-buffer",
]);

function escapeHtml(s) {
  return String(s).replace(
    /[<>&"']/g,
    (c) => ({ "<": "&lt;", ">": "&gt;", "&": "&amp;", '"': "&quot;", "'": "&#39;" })[c],
  );
}

// Render `src` as HTML with paren depth highlighting + simple
// string / comment colouring. Walks character-by-character so we
// don't have to ship a real parser. Mismatched closes (more
// closes than opens at some prefix) get their own class so they
// stand out instead of silently absorbing whatever colour the
// stack happened to be at.
export function rainbowHighlight(src) {
  let out = "";
  let depth = 0;
  let inString = false;
  let inComment = false;
  let buf = "";
  // True when the next non-whitespace symbol token in `buf` is the
  // head of a freshly-opened list. Set on `(`, cleared once the
  // head is emitted (or on `)` for safety).
  let expectingHead = false;
  // Flush `buf` as plain text, except when `expectingHead` is set
  // — then split off the first non-whitespace token, classify it
  // as a special-form or function-call head, and clear the flag.
  // String / comment / mismatched-paren spans bypass this path
  // and pass an explicit class.
  const flush = (cls) => {
    if (!buf) return;
    if (cls) {
      out += `<span class="${cls}">${escapeHtml(buf)}</span>`;
    } else if (expectingHead) {
      const m = buf.match(/^(\s*)(\S+)([\s\S]*)$/);
      if (m) {
        const [, ws, head, rest] = m;
        const headCls = SPECIAL_FORMS.has(head)
          ? "repl-special-form"
          : "repl-function-head";
        out += escapeHtml(ws);
        out += `<span class="${headCls}">${escapeHtml(head)}</span>`;
        out += escapeHtml(rest);
        expectingHead = false;
      } else {
        // Buffer is whitespace-only; the head is still pending.
        out += escapeHtml(buf);
      }
    } else {
      out += escapeHtml(buf);
    }
    buf = "";
  };
  const opens = new Set(["(", "[", "{"]);
  const closes = new Set([")", "]", "}"]);
  for (let i = 0; i < src.length; i++) {
    const ch = src[i];
    if (inComment) {
      buf += ch;
      if (ch === "\n") {
        flush("repl-comment");
        inComment = false;
      }
      continue;
    }
    if (inString) {
      buf += ch;
      if (ch === "\\" && i + 1 < src.length) {
        buf += src[++i];
        continue;
      }
      if (ch === "\"") {
        flush("repl-string");
        inString = false;
      }
      continue;
    }
    if (ch === ";") {
      flush(null);
      buf = ch;
      inComment = true;
      continue;
    }
    if (ch === "\"") {
      flush(null);
      buf = ch;
      inString = true;
      continue;
    }
    if (opens.has(ch)) {
      flush(null);
      const cls = `paren-${depth % RAINBOW_DEPTHS}`;
      out += `<span class="${cls}">${ch}</span>`;
      depth++;
      expectingHead = true;
      continue;
    }
    if (closes.has(ch)) {
      flush(null);
      if (depth === 0) {
        out += `<span class="paren-mismatch">${ch}</span>`;
      } else {
        depth--;
        const cls = `paren-${depth % RAINBOW_DEPTHS}`;
        out += `<span class="${cls}">${ch}</span>`;
      }
      // The head of the just-closed form was already consumed (or
      // the form was empty); the parent's head was consumed
      // earlier. Either way, no head is pending here.
      expectingHead = false;
      continue;
    }
    buf += ch;
  }
  // Flush trailing text (string / comment / plain).
  flush(inString ? "repl-string" : inComment ? "repl-comment" : null);
  // Browsers swallow a textarea's trailing newline visually; add a
  // sentinel so the overlay's height matches the textarea row count.
  if (src.endsWith("\n")) out += " ";
  return out;
}

// Walk text[0..cursor] tracking columns and a stack of open-paren
// columns, skipping over string and ;-line-comment regions. The
// indent for a newline at `cursor` is the innermost still-open
// paren's column + 2; if no paren is open we land at column 0.
export function indentForNewline(text, cursor) {
  let col = 0;
  const stack = [];
  let inString = false;
  let inComment = false;
  for (let i = 0; i < cursor; i++) {
    const ch = text[i];
    if (inComment) {
      if (ch === "\n") {
        inComment = false;
        col = 0;
      } else {
        col++;
      }
      continue;
    }
    if (inString) {
      if (ch === "\\" && i + 1 < cursor) {
        col += 2;
        i++;
        continue;
      }
      if (ch === "\"") inString = false;
      if (ch === "\n") col = 0;
      else col++;
      continue;
    }
    if (ch === ";") {
      inComment = true;
      col++;
      continue;
    }
    if (ch === "\"") {
      inString = true;
      col++;
      continue;
    }
    if (ch === "\n") {
      col = 0;
      continue;
    }
    if (ch === "(" || ch === "[" || ch === "{") {
      stack.push(col);
    } else if (ch === ")" || ch === "]" || ch === "}") {
      stack.pop();
    }
    col++;
  }
  if (stack.length === 0) return 0;
  return stack[stack.length - 1] + 2;
}
