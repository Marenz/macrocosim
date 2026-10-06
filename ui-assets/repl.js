// REPL panel: live syntax-highlighted Lisp input, autocomplete,
// the log-line tap above it, and the WebSocket pump that fans out
// /ws/events frames to liveCharts / the metrics store / pulseBar /
// the log panel.

import { dispatchesPanel, escapeHtml, notify, setStatus } from "./app.js";
import { pulseBar } from "./chrome.js";
import { errorText } from "./http.js";
import { inspectorLive, liveCharts } from "./inspect.js";
import { metricsStore } from "./metrics-store.js";
import {
  COMPLETIONS,
  indentForNewline,
  rainbowHighlight,
  wordAtCursor,
} from "./repl-syntax.js";
import { mgPath, readSelectedMg, readSubview } from "./routing.js";
import { closePanel } from "./side-panel.js";
import { topology } from "./topology.js";


// The Logs panel's tail. /api/logs gives the load-time backfill
// (ring of recent records); /ws/events kind:"log" appends each new
// record live. Capped at 500 DOM rows so a chatty session doesn't
// freeze the panel.
function appendLog(ev) {
  const box = document.getElementById("logs");
  const el = document.createElement("div");
  el.className = `log-line ${(ev.level || "info").toLowerCase()}`;
  const ts = new Date(ev.ts).toLocaleTimeString();
  el.innerHTML =
    `<span class="log-ts">${ts}</span>` +
    `<span class="log-lvl">${escapeHtml(ev.level || "")}</span>` +
    `<span class="log-msg">${escapeHtml(ev.message || "")}</span>`;
  // Scroll-pin: only auto-scroll if the user hadn't scrolled away.
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 30;
  box.appendChild(el);
  while (box.children.length > 500) box.removeChild(box.firstChild);
  if (atBottom) box.scrollTop = box.scrollHeight;
}

export async function backfillLogs() {
  try {
    const lines = await (await fetch("/api/logs")).json();
    for (const ln of lines) appendLog(ln);
  } catch (_) {}
}

// The Logs panel head: the minimum level goes on #logs as a class the
// stylesheet reads, and persists; clear empties the tail (new lines
// keep arriving).
const LOGS_LEVEL_KEY = "mc-logs-level";

export function setupLogsPanel() {
  const box = document.getElementById("logs");
  const level = document.getElementById("logs-level");
  // The select's own options are the list of levels: a level added
  // there gets its min-* class handled here without a second edit.
  const LOG_LEVELS = [...level.options].map((o) => o.value);
  const apply = () => {
    for (const l of LOG_LEVELS) box.classList.toggle(`min-${l}`, l === level.value);
    localStorage.setItem(LOGS_LEVEL_KEY, level.value);
  };
  const stored = localStorage.getItem(LOGS_LEVEL_KEY);
  level.value = LOG_LEVELS.includes(stored) ? stored : "info";
  level.addEventListener("change", apply);
  apply();
  document.getElementById("logs-clear").addEventListener("click", () => box.replaceChildren());
}

// Hardcoded completion candidates for the REPL. Until tulisp exposes
// obarray enumeration upstream, this list has to track the surface
// macrocosim exposes by hand. Drop-in replacement: hit /api/symbols
// (TBD) and merge the response into this array.

export function setupRepl() {
  const form = document.getElementById("repl-form");
  const input = document.getElementById("repl-input");
  const overlay = document.getElementById("repl-input-overlay");
  const output = document.getElementById("repl-output");
  const completions = document.getElementById("repl-completions");
  let selectedIdx = 0;
  let active = []; // current list of candidates

  // Electric-pair: typed open chars insert their close + leave the
  // cursor between. Closing char typed when the next char is the
  // same close just steps over instead of doubling up. Backspace
  // immediately after an empty pair eats both halves.
  const PAIRS = { "(": ")", "[": "]", "{": "}", "\"": "\"" };
  const CLOSES = new Set(Object.values(PAIRS));

  function refreshOverlay() {
    overlay.innerHTML = rainbowHighlight(input.value);
    overlay.scrollTop = input.scrollTop;
  }

  function renderCompletions() {
    if (!active.length) {
      completions.hidden = true;
      completions.innerHTML = "";
      return;
    }
    completions.hidden = false;
    completions.innerHTML = active
      .map(
        (c, i) =>
          `<li class="${i === selectedIdx ? "selected" : ""}" data-i="${i}">${escapeHtml(c)}</li>`,
      )
      .join("");
    for (const li of completions.querySelectorAll("li")) {
      li.addEventListener("mousedown", (e) => {
        e.preventDefault(); // don't blur the textarea
        selectedIdx = Number(li.dataset.i);
        applyCompletion();
      });
    }
  }

  function refresh() {
    const { prefix } = wordAtCursor(input);
    if (!prefix || prefix.length < 1) {
      active = [];
    } else {
      active = COMPLETIONS.filter((c) => c.startsWith(prefix)).slice(0, 12);
      // If the only match is exactly what's typed, no point showing a popup.
      if (active.length === 1 && active[0] === prefix) active = [];
    }
    selectedIdx = 0;
    renderCompletions();
  }

  function applyCompletion() {
    if (!active.length) return;
    const choice = active[selectedIdx];
    const { start, end } = wordAtCursor(input);
    const v = input.value;
    input.value = v.slice(0, start) + choice + v.slice(end);
    const newCursor = start + choice.length;
    input.setSelectionRange(newCursor, newCursor);
    active = [];
    renderCompletions();
    // Programmatic .value assignment doesn't fire `input`; nudge
    // the overlay (and other input listeners) explicitly.
    refreshOverlay();
  }

  // Send the current textarea contents through /api/format and
  // replace them with the result. Cursor preservation is best-
  // effort: we count non-whitespace characters before the old
  // cursor and place the new cursor after the same count of
  // non-whitespace characters in the formatted output. The
  // formatter only rearranges whitespace, so this lands the
  // cursor at the same logical token.
  async function formatInput() {
    const src = input.value;
    if (!src.trim()) return;
    const oldCursor = input.selectionStart;
    let nonWsBefore = 0;
    for (let i = 0; i < oldCursor; i++) {
      if (!/\s/.test(src[i])) nonWsBefore++;
    }
    let res;
    try {
      res = await fetch("/api/format?width=60", {
        method: "POST",
        body: src,
      });
    } catch (_) {
      return;
    }
    if (!res.ok) {
      // 400 carries the parse diagnostic (unbalanced parens etc.) —
      // a silent no-op Tab gives the user nothing to act on.
      notify(await errorText(res));
      return;
    }
    let formatted = await res.text();
    // tulisp-fmt always emits a trailing newline; the textarea
    // looks tidier without one for typical REPL fragments.
    if (formatted.endsWith("\n")) formatted = formatted.slice(0, -1);
    let newCursor = formatted.length;
    let seen = 0;
    for (let i = 0; i < formatted.length; i++) {
      if (!/\s/.test(formatted[i])) {
        if (seen === nonWsBefore) {
          newCursor = i;
          break;
        }
        seen++;
      }
    }
    input.value = formatted;
    input.setSelectionRange(newCursor, newCursor);
    refreshOverlay();
  }

  async function run() {
    const src = input.value.trim();
    if (!src) return;
    const entry = document.createElement("div");
    entry.className = "repl-entry";
    entry.innerHTML = `<pre class="repl-prompt">▸ ${escapeHtml(src)}</pre>`;
    output.appendChild(entry);
    output.scrollTop = output.scrollHeight;
    try {
      const res = await fetch(mgPath("eval") ?? "/api/eval", { method: "POST", body: src });
      const klass = res.ok ? "repl-value" : "repl-error";
      const text = res.ok ? (await res.json()).value : await errorText(res);
      const out = document.createElement("pre");
      out.className = klass;
      out.textContent = text;
      entry.appendChild(out);
    } catch (err) {
      const out = document.createElement("pre");
      out.className = "repl-error";
      out.textContent = `transport error: ${err.message}`;
      entry.appendChild(out);
    }
    input.value = "";
    refreshOverlay();
    output.scrollTop = output.scrollHeight;
  }

  form.addEventListener("submit", (e) => {
    e.preventDefault();
    run();
  });
  input.addEventListener("input", () => {
    refreshOverlay();
    refresh();
  });
  input.addEventListener("scroll", () => {
    overlay.scrollTop = input.scrollTop;
  });
  input.addEventListener("blur", () => {
    // Defer hide so click-on-li handlers fire first.
    setTimeout(() => {
      active = [];
      renderCompletions();
    }, 100);
  });
  input.addEventListener("keydown", (e) => {
    // Completion popup keys take priority when it's open.
    if (active.length) {
      if (e.key === "Tab" || (e.key === "Enter" && !e.ctrlKey && !e.metaKey)) {
        e.preventDefault();
        applyCompletion();
        return;
      }
      if (e.key === "ArrowDown") {
        e.preventDefault();
        selectedIdx = (selectedIdx + 1) % active.length;
        renderCompletions();
        return;
      }
      if (e.key === "ArrowUp") {
        e.preventDefault();
        selectedIdx = (selectedIdx - 1 + active.length) % active.length;
        renderCompletions();
        return;
      }
      if (e.key === "Escape") {
        e.preventDefault();
        active = [];
        renderCompletions();
        return;
      }
    }
    // With no completion popup to dismiss, Esc closes the panel,
    // matching Esc on every other card.
    if (e.key === "Escape") {
      e.preventDefault();
      closePanel("repl-btn");
      return;
    }
    if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
      e.preventDefault();
      run();
      return;
    }
    // Tab when the completion popup isn't open: roundtrip the
    // textarea contents through /api/format. The popup-open case
    // is handled in the block above.
    if (e.key === "Tab" && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault();
      formatInput();
      return;
    }
    // Plain Enter: walk back through the typed text to the
    // innermost still-open paren and indent the new line at its
    // column + 2. Strings / comments are skipped during the walk
    // so a `;` inside a comment (and an unbalanced `(` inside a
    // string) doesn't perturb the depth count. Doesn't replicate
    // tulisp-fmt's special-form rules (let bindings align under
    // first arg, etc.) — Tab roundtrips through the formatter for
    // canonical layout.
    if (e.key === "Enter" && !e.ctrlKey && !e.metaKey && !e.altKey) {
      const cursor = input.selectionStart;
      const end = input.selectionEnd;
      const indent = indentForNewline(input.value, cursor);
      const insert = `\n${" ".repeat(indent)}`;
      const v = input.value;
      e.preventDefault();
      input.value = v.slice(0, cursor) + insert + v.slice(end);
      input.setSelectionRange(cursor + insert.length, cursor + insert.length);
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return;
    }
    // Electric-pair: skip if user is also holding a modifier (so
    // Ctrl-9 etc. on layouts that produce `(` directly still
    // works as the user expects).
    if (e.ctrlKey || e.metaKey || e.altKey) return;

    const v = input.value;
    const s = input.selectionStart;
    const e2 = input.selectionEnd;
    if (e.key in PAIRS) {
      e.preventDefault();
      const open = e.key;
      const close = PAIRS[open];
      // Step-over when typing a quote and cursor is already
      // immediately before that same quote.
      if (open === close && s === e2 && v[s] === open) {
        input.setSelectionRange(s + 1, s + 1);
        return;
      }
      if (s === e2) {
        input.value = v.slice(0, s) + open + close + v.slice(s);
        input.setSelectionRange(s + 1, s + 1);
      } else {
        input.value = v.slice(0, s) + open + v.slice(s, e2) + close + v.slice(e2);
        input.setSelectionRange(s + 1, e2 + 1);
      }
      input.dispatchEvent(new Event("input", { bubbles: true }));
      return;
    }
    if (CLOSES.has(e.key) && s === e2 && v[s] === e.key) {
      // Cursor sitting right before a matching close — just step
      // past instead of double-typing.
      e.preventDefault();
      input.setSelectionRange(s + 1, s + 1);
      return;
    }
    if (e.key === "Backspace" && s === e2 && s > 0) {
      const before = v[s - 1];
      const after = v[s];
      if (before in PAIRS && PAIRS[before] === after) {
        e.preventDefault();
        input.value = v.slice(0, s - 1) + v.slice(s + 1);
        input.setSelectionRange(s - 1, s - 1);
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
    }
  });
  // Initial paint so the overlay shows whatever the textarea was
  // pre-filled with (e.g. browser back-button restored content).
  refreshOverlay();
}

// Self-reconnecting WS with exponential backoff. Starts at 1 s,
// doubles on each close, caps at 30 s, resets to 1 s on a clean
// onopen. A laptop returning from sleep, a server bounce, or a
// notify-reload that briefly drops connections all heal without
// a manual page refresh — important for an overnight soak run.
//
// On reconnect (i.e. open after a previous open) we also nudge a
// topology refresh because samples may have moved while we were
// away. The very first open is a no-op there because init()
// already awaited refreshTopology before opening the WS.
export function openWebSocket(onTopologyChanged) {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}/ws/events`;
  const MIN_DELAY = 1000;
  const MAX_DELAY = 30000;
  let delay = MIN_DELAY;
  let everConnected = false;
  let lastConfigError = { message: null, ts_ms: 0 };
  function connect() {
    const ws = new WebSocket(url);
    ws.onopen = () => {
      delay = MIN_DELAY;
      if (everConnected) {
        // Catch up state the canvas and inspector cached from
        // before the drop. Loopback pill + metrics panel also
        // self-heal via their next poll / WS frame.
        onTopologyChanged(0);
      }
      everConnected = true;
    };
    ws.onmessage = (msg) => {
      // Defensive: vis-network and other libs sometimes pump non-string
      // frames (binary, blob) through fetch / WS pipelines that look
      // identical from a try/catch perspective. Surface the actual
      // payload type to console so a "JSON.parse undefined" surprise
      // points straight at the offending frame.
      if (typeof msg.data !== "string") {
        console.warn("WS: non-string payload, skipping:", msg.data);
        return;
      }
      let ev;
      try {
        ev = JSON.parse(msg.data);
      } catch (e) {
        console.warn("WS: JSON parse failed:", e.message, "payload was:", msg.data);
        return;
      }
      // Per-microgrid events carry mg_id (post-D3); we filter out
      // anything from a microgrid other than the currently-selected
      // one, and all of them with nothing selected, so the metrics
      // panel doesn't paint with samples from a neighbour. The one
      // exception is topology_changed with nothing selected: the list
      // view refreshes its microgrid cards on it. Enterprise-scoped
      // events (log, lagged) ship mg_id = undefined and pass through
      // regardless.
      const selectedMg = readSelectedMg();
      const perMg = ev.kind === "sample" || ev.kind === "microgrid_sample"
                 || ev.kind === "topology_changed" || ev.kind === "setpoint"
                 || ev.kind === "dispatch_changed" || ev.kind === "knob_changed";
      const forList = ev.kind === "topology_changed" && selectedMg == null;
      if (perMg && !forList && ev.mg_id != null && ev.mg_id !== selectedMg) {
        return;
      }
      if (ev.kind === "sample") {
        liveCharts.pushSample(ev.id, ev.metric, ev.ts_ms, ev.value);
        topology.applySample(ev);
        inspectorLive.applySample(ev);
      } else if (ev.kind === "microgrid_sample") {
        metricsStore.applySample({ ...ev, t_s: ev.ts_ms / 1000 });
        topology.applyMicrogridSample(ev);
      } else if (ev.kind === "topology_changed") {
        onTopologyChanged(ev.version);
      } else if (ev.kind === "setpoint") {
        topology.noteSetpoint(ev);
        pulseBar.recordSetpoint();
        inspectorLive.applySetpoint(ev);
      } else if (ev.kind === "knob_changed") {
        inspectorLive.applyKnob(ev);
        // Some knobs also move the canvas — the plug knob is the only
        // signal an unplug gives the live overlay, since an empty
        // charger stops sending soc_pct rather than sending a null
        // one. topology ignores the tokens it does not draw.
        topology.applyKnob(ev);
      } else if (ev.kind === "log") {
        appendLog({ ...ev, ts: ev.ts_ms });
      } else if (ev.kind === "config_error") {
        // A failed hot-reload (site already reset) or an eval the
        // microgrid's file could not record — the one moment the user most
        // needs an explanation, so toast it and keep it in the log.
        // A reload failure is broadcast on every registered site's
        // bus (the WS pump has no enterprise channel for it), so a
        // multi-microgrid setup delivers N copies — collapse repeats
        // of the same message arriving close together into one toast.
        if (
          ev.message === lastConfigError.message &&
          ev.ts_ms - lastConfigError.ts_ms < 1000
        ) {
          return;
        }
        lastConfigError = { message: ev.message, ts_ms: ev.ts_ms };
        notify(ev.message);
        appendLog({ ts: ev.ts_ms, level: "error", message: ev.message });
      } else if (ev.kind === "dispatch_changed") {
        // The dispatch store changed for ev.mg_id; refetch only if
        // we're actually looking at that microgrid's Dispatches tab.
        if (selectedMg != null && readSubview() === "dispatches") {
          dispatchesPanel.render(selectedMg);
        }
      }
    };
    ws.onclose = () => {
      const secs = Math.round(delay / 1000);
      setStatus(`disconnected — retry in ${secs}s`, "error");
      setTimeout(connect, delay);
      delay = Math.min(delay * 2, MAX_DELAY);
    };
    // onerror fires alongside onclose; setStatus message stays as
    // the "retry in Xs" we just set so the user sees the recovery
    // plan rather than an opaque "ws error".
    ws.onerror = () => {};
  }
  connect();
}

