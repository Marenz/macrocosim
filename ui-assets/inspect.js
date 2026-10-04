// Side-panel inspector: live charts per metric, per-category
// knobs / inputs, setpoint event log, and the small utility
// chooseScale / liveCharts machinery the charts route through.
// `showComponent` renders the whole side panel for a selected node,
// registering `liveCharts.clear()` as the node panel's teardown
// (side-panel.js runs it on re-render or close).

import { escapeHtml, inspectEl } from "./app.js";
import { requireUplot } from "./chart-lib.js";
import { evalQuoted, jsToLispString } from "./eval.js";
import { deadBandW, formatScaled } from "./live.js";
import { metricsStore } from "./metrics-store.js";
import { powerColor, reactiveColor } from "./pill.js";
import { mgPath, READ_ONLY_TITLE, structureEditable } from "./routing.js";
import { openPanel } from "./side-panel.js";
import { topology } from "./topology.js";

// A recency token for work that awaits and then installs something a
// teardown must be able to disown. `capture()` before the await,
// `stale(token)` after: a teardown that ran in between `bump()`s the
// token, and the late result is dropped instead of installed. Both
// uses here are orthogonal to `showGen`, which only moves when a NEW
// node is selected — closePanel("node") and a node-panel re-render
// run the panel's teardown WITHOUT touching showGen, so a late result
// would otherwise pass the unchanged gen check and install a uPlot,
// a live metricsStore subscription, or a TTL timer that nothing is
// registered to ever clear. Three live in this file: one for the
// deferred chart builds, one for the /api/component snapshot fetches
// and one for the EV card's poll (the latter two also keep a sequence
// so two in-flight fetches for the same open node resolve in start
// order).
function aliveToken() {
  let alive = 0;
  return {
    capture: () => alive,
    stale: (token) => token !== alive,
    bump: () => {
      alive++;
    },
  };
}
const chartsAlive = aliveToken();
const snapshotAlive = aliveToken();
const evAlive = aliveToken();

// Per-component history metrics, keyed by category. No `grid` entry:
// the sim's Grid publishes no per-component telemetry by design, so
// the GCP charts the site-wide grid_frequency stream instead — see
// buildGridFrequencyChart below.
const CHARTS_BY_CATEGORY = {
  meter: ["active_power_w", "reactive_power_var"],
  inverter: ["active_power_w", "reactive_power_var"],
  battery: ["soc_pct", "dc_power_w"],
  "ev-charger": ["soc_pct", "active_power_w"],
  chp: ["active_power_w"],
  "steam-boiler": ["pressure_bar", "active_power_w"],
};

// Display-only labels per metric. Scaling, units, and the
// "is this a power-family quantity?" decision now come off the
// /api/history response's `quantity` + `unit` fields — see
// chooseScale below. Anything not in this table falls back to the
// raw metric name as the chart title.
const METRIC_TITLES = {
  active_power_w:     "Active Power",
  reactive_power_var: "Reactive Power",
  soc_pct:            "SoC",
  dc_power_w:         "DC Power",
  pressure_bar:       "Steam pressure",
};

// Pick a display scale from a typed quantity + base unit. Power-
// family quantities autoscale W → kW → MW based on the data range;
// everything else uses the base unit verbatim. The `quantity` /
// `unit` arguments mirror the `Sample<Q>` / `Q.base_unit()` shape
// upstream in frequenz-microgrid, so the same code can serve any
// `Power` / `ReactivePower` / `Frequency` / `Percentage` payload.
function chooseScale(quantity, unit, values) {
  const isPower = quantity === "Power" || quantity === "ReactivePower";
  if (isPower && values.length) {
    const max = Math.max(...values.map((v) => Math.abs(v)));
    if (max >= 1e6) return { div: 1e6, unit: `M${unit}` };
    if (max >= 1e3) return { div: 1e3, unit: `k${unit}` };
    return { div: 1, unit };
  }
  return { div: 1, unit: unit || "" };
}

// Live-chart state for whichever component the user has selected.
// Replaced wholesale on every selection change; the previous uPlots
// get destroyed in clear(). All access to the per-selection chart
// session goes through this module so the surrounding code never
// has to spell out the "is the right component selected, has the
// metric been wired" preconditions for the live push paths.
export const refitCharts = () => {
  liveCharts.refit();
  const parent = gridChart?.plot.root.parentElement;
  if (parent) gridChart.plot.setSize({ width: parent.clientWidth, height: 140 });
};

// The inspector's charts follow their card's width the same way the
// metrics and weather charts do: observe the content root once, and
// coalesce a burst of resize notifications into one refit per frame.
let refitQueued = false;
new ResizeObserver(() => {
  if (refitQueued) return;
  refitQueued = true;
  requestAnimationFrame(() => {
    refitQueued = false;
    refitCharts();
  });
}).observe(document.getElementById("inspect"));

export const liveCharts = (() => {
  let active = null; // { id, charts: Map<metric, {plot, xs, ys, scale}> }
  return {
    set(id, charts) {
      active = { id, charts };
    },
    clear() {
      if (!active) return;
      for (const ch of active.charts.values()) ch.plot.destroy();
      active = null;
    },
    pushSample(id, metric, ts_ms, value) {
      if (!active || active.id !== Number(id)) return;
      const series = active.charts.get(metric);
      if (!series) return;
      series.xs.push(ts_ms / 1000);
      // Apply the chart's chosen unit scale so live samples stay
      // consistent with the backfilled ones.
      series.ys.push(value / series.scale.div);
      // Cap to 5-minute window so the chart doesn't grow forever.
      const cutoff = Date.now() / 1000 - 300;
      while (series.xs.length && series.xs[0] < cutoff) {
        series.xs.shift();
        series.ys.shift();
      }
      series.plot.setData([series.xs, series.ys]);
    },
    refit() {
      if (!active) return;
      for (const series of active.charts.values()) {
        const parent = series.plot.root.parentElement;
        if (!parent) continue;
        series.plot.setSize({
          width: parent.clientWidth,
          height: 140,
        });
      }
    },
  };
})();

// The GCP's one chart is fed by the metrics store, not by the
// per-component history liveCharts owns, so its cleanup (unsubscribe
// + destroy) can't live in liveCharts.clear(). It parks here instead,
// under the same one-active-selection slot discipline, and the node
// panel's single teardown closure runs both. Read-and-null so a
// teardown that runs twice — re-render then close — can't drop the
// store subscription twice or double-destroy the plot.
let gridChart = null; // { plot, destroy }
function clearGridChart() {
  const chart = gridChart;
  gridChart = null;
  chart?.destroy();
}

// The operational modes a component can be declared with, plus a
// hover hint each — shared with the topology context menu's bulk
// set so the two surfaces offer the same vocabulary. The tokens
// round-trip through (set-component-operational-mode …) verbatim.
export const OPERATIONAL_MODES = [
  { value: "unspecified", hint: "not explicitly set; treated as full capability" },
  { value: "inactive", hint: "no telemetry, no control" },
  { value: "telemetry-only", hint: "streams telemetry, rejects commands" },
  { value: "control-only", hint: "accepts commands, streams no telemetry" },
  { value: "control-and-telemetry", hint: "full capability, explicitly declared" },
];

// Categories that the gRPC server actually accepts setpoints on.
// command-mode (timeout / error fault simulation) only makes sense
// for these — grids and meters have no setpoint surface, so we hide
// the dropdown rather than offering a knob that does nothing.
// Categories whose components accept setpoint commands. Shared with
// the dispatch dialog's target pickers, so "controllable" means the
// same thing in both places.
export const ACCEPTS_SETPOINTS = new Set(["battery", "inverter", "ev-charger", "chp", "steam-boiler"]);

// Per-category runtime knobs the inspector exposes as numeric
// inputs. Each one binds to an existing Lisp setter — so this is
// just UI sugar over what the REPL could already do. Construction-
// time args (capacity, rated bounds, …) aren't here because most
// aren't runtime-mutable on the underlying component yet.
// `dynamic: true` knobs accept either a numeric literal or a Lisp
// expression (lambda, quoted symbol, …) — the underlying defun
// dispatches on input kind. Inputs with `dynamic` render as text,
// everything else as numeric. See the renderInspect Knobs block.
// `unit`, on a dynamic knob only, labels the resolved per-tick value
// line paintKnobEntry shows beneath an expression's printed source —
// see knobDisplay/paintKnobEntry below.
// `group` places each knob in the Component card: "config" rows are
// device configuration (a Config sub-section of their own), anything
// else is an environment/simulation driver and joins the Simulation
// rows — a meter's sources and a PV's sunlight steer what the sim
// produces, the same family as health/telemetry.
// `clear`, on a meter knob only, names the Lisp defun a small
// "measure" button next to the input calls to drop the override and
// hand the reading back to the children's sum (clear-meter-power /
// clear-meter-reactive — Task 3). The button is hidden until the
// token actually carries a live override; see the knobRow/
// paintKnobEntry/setKnobText comments below for the show/hide
// discipline. Reactive power and power factor share one clear defun
// (one slot server-side, mutually exclusive readings) so both rows
// carry the same `clear` value.
const KNOBS_BY_CATEGORY = {
  meter: [
    {
      label: "power (W or expr)",
      defun: "set-meter-power",
      dynamic: true,
      unit: "W",
      clear: "clear-meter-power",
    },
    {
      label: "reactive power (VAr or expr)",
      defun: "set-meter-reactive-power",
      dynamic: true,
      unit: "VAr",
      clear: "clear-meter-reactive",
    },
    {
      label: "power factor (0–1]",
      defun: "set-meter-power-factor",
      flag: "leading",
      clear: "clear-meter-reactive",
    },
  ],
  // No direct VAr-setpoint input here: reactive power on an inverter
  // is driven by control apps over the gRPC API (the REPL still has
  // set-reactive-power). The inspector carries only component config
  // and measurements — the API-driven setpoint shows up read-only as
  // the Power card's marker + TTL row and in the setpoint log.
  inverter: [
    { label: "reactive PF limit", defun: "set-reactive-pf-limit", group: "config" },
    { label: "reactive apparent (VA)", defun: "set-reactive-apparent-va", group: "config" },
  ],
  "steam-boiler": [
    { label: "demand (kg/h or expr)", defun: "set-boiler-demand", dynamic: true, unit: "kg/h" },
    { label: "pressure (bar)", defun: "set-boiler-pressure", unit: "bar" },
  ],
};

function knobsFor(d) {
  const knobs = [...(KNOBS_BY_CATEGORY[d.category] || [])];
  // Solar inverters also get a sunlight knob — driven by the same
  // (set-solar-sunlight ID PCT) defun the cloud-curve timer uses.
  if (d.category === "inverter" && d.subtype === "solar") {
    knobs.unshift({
      label: "sunlight (% or expr)",
      defun: "set-solar-sunlight",
      dynamic: true,
      unit: "%",
    });
  }
  return knobs;
}

// Tint class for a health chip's "on" state: the three states carry
// their own severity colours.
function segTint(value) {
  if (value === "error") return "on-bad";
  if (value === "standby") return "on-warn";
  return "on-good"; // ok
}

// The health row is the one state knob rendered as chips: three
// short options fit on a single line and the active chip's colour
// makes the health readable at a glance. The longer vocabularies
// (mode / telemetry / commands) wrap badly as chips in the 420 px
// panel and render as <select>s via selectField below instead.
// Clicks aren't wired per-row: the single delegated listener below
// handles every `[data-knob][data-value]` chip the panel renders.
function renderSegRow(knobKey, current, options) {
  const chips = options
    .map((o) => {
      const on = o === current;
      const cls = on ? `seg-chip ${segTint(o)}` : "seg-chip";
      return `<button type="button" class="${cls}" data-knob="${knobKey}" data-value="${escapeHtml(o)}">${escapeHtml(o)}</button>`;
    })
    .join("");
  return `<div class="seg">${chips}</div>`;
}

// `disabledReason` (a string) greys the select out and becomes its
// hover tooltip — used when the operational mode forbids the knob,
// where the backend would reject the set with the same message. The
// current value stays selected while disabled, so the row still
// shows what state the component is in.
function selectField(knob, current, options, disabledReason = null) {
  const opts = options
    .map(
      (o) =>
        `<option value="${escapeHtml(o)}"${o === current ? " selected" : ""}>${escapeHtml(o)}</option>`,
    )
    .join("");
  const attrs = disabledReason
    ? ` disabled title="${escapeHtml(disabledReason)}"`
    : "";
  return `<select data-knob="${knob}"${attrs}>${opts}</select>`;
}

// Chip knob token → the same Lisp setter the old <select> onchange
// handlers called.
const KNOB_DEFUNS = {
  "operational-mode": "set-component-operational-mode",
  health: "set-component-health",
  "telemetry-mode": "set-component-telemetry-mode",
  "command-mode": "set-component-command-mode",
};

// One delegated click listener for every chip row the panel will
// ever render, attached once from app.js init() — inspectEl itself
// is never replaced (only its innerHTML, on every selection), so a
// listener attached inside renderInspect would accumulate one copy
// per selection. The target id rides on inspectEl's own dataset
// (set at the top of renderInspect below) rather than a closure,
// since a closure would go stale the moment a different node is
// selected. This must NOT run at module load: inspectEl is a const
// that app.js initializes in its module body, which runs after this
// module's — touching it here at top level is a TDZ ReferenceError
// that kills the whole module graph at boot.
export function setupInspectorChips() {
  inspectEl.addEventListener("click", (e) => {
    const btn = e.target.closest("[data-knob][data-value]");
    if (!btn || btn.disabled) return;
    const defun = KNOB_DEFUNS[btn.dataset.knob];
    const id = inspectEl.dataset.inspectId;
    if (!defun || !id) return;
    evalQuoted(`(${defun} ${id} '${btn.dataset.value})`);
  });
  // The state <select>s (mode / telemetry / commands) go through the
  // same delegation: change bubbles, and the select's own value is
  // the option token.
  inspectEl.addEventListener("change", (e) => {
    const sel = e.target.closest("select[data-knob]");
    if (!sel) return;
    const defun = KNOB_DEFUNS[sel.dataset.knob];
    const id = inspectEl.dataset.inspectId;
    if (!defun || !id) return;
    evalQuoted(`(${defun} ${id} '${sel.value})`);
  });
}

// Per-card fold state, persisted across selections and reloads.
// Reads/writes are wrapped in try/catch (private-mode /
// quota-exceeded storage throws) and fall back to the card's default
// on any failure. Power starts open — the P/Q readouts are the
// panel's headline — the other cards start folded.
const CARD_KEY_PREFIX = "mc-inspector-card-";
const CARD_DEFAULT_OPEN = { component: false, ev: true, power: true, charts: false, setpoints: false };
function loadCardOpen(name) {
  try {
    const v = localStorage.getItem(CARD_KEY_PREFIX + name);
    return v == null ? (CARD_DEFAULT_OPEN[name] ?? false) : v === "1";
  } catch {
    return CARD_DEFAULT_OPEN[name] ?? false;
  }
}
function saveCardOpen(name, open) {
  try {
    localStorage.setItem(CARD_KEY_PREFIX + name, open ? "1" : "0");
  } catch {
    // Storage unavailable — the fold still works for this session,
    // it just won't remember next time.
  }
}

// ── EV card ────────────────────────────────────────────────────────
//
// The charger's own telemetry deliberately says nothing about the car
// in it, so this card reads the simulator's private view over
// /api/mg/{mg}/ev/{id} instead and writes back through plug-ev /
// unplug-ev — the same eval path every other inspector control uses.
// The preset catalog is the server's (`presets`, sent plugged or not),
// never a copy of it here: a car added to the catalog shows up in this
// dropdown without a UI change.

function evBodyHtml(info) {
  if (!info.plugged) {
    const options = info.presets.map((p) => `<option value="${escapeHtml(p)}">${escapeHtml(p)}</option>`).join("");
    return `
      <p>no EV plugged in</p>
      <dl>
        <dt>preset</dt><dd><select id="ev-preset">${options}</select></dd>
        <dt>initial SoC (%)</dt><dd><input id="ev-soc" type="number" step="any" min="0" max="100" placeholder="preset default" /></dd>
        <dt>target SoC (%)</dt><dd><input id="ev-target" type="number" step="any" min="0" max="100" placeholder="100" /></dd>
      </dl>
      <button type="button" id="ev-plug" class="btn">Plug in</button>`;
  }
  const pct = Math.round(info.soc_pct);
  const mins = Math.max(0, Math.round((Date.now() - Date.parse(info.plugged_at)) / 60000));
  return `
    <dl>
      <dt>car</dt><dd>${escapeHtml(info.preset)} · ${info.phases} phase${info.phases > 1 ? "s" : ""} · ${info.max_current_a} A</dd>
      <dt>SoC</dt><dd><div class="ev-soc-bar"><div class="ev-soc-fill" style="width:${pct}%"></div><div class="ev-soc-target" style="left:${Math.round(info.target_soc_pct)}%"></div></div> ${pct}% of ${formatScaled(info.capacity_wh, "Wh")}, target ${Math.round(info.target_soc_pct)}%</dd>
      <dt>session</dt><dd>${formatScaled(info.energy_wh, "Wh")} in ${mins} min</dd>
      <dt>state</dt><dd>${escapeHtml(info.state)}</dd>
    </dl>
    <button type="button" id="ev-unplug" class="btn">Unplug</button>`;
}

// Call when starting an EV-card fetch; pass the token to evFetchStale
// once it settles. Same shape as beginSnapshotFetch/snapshotFetchStale:
// these run fire-and-forget off a 2 s interval and off the two click
// handlers, so a slow response must not paint over a newer one that
// already landed, and a teardown in between must disown it outright.
let evSeq = 0;
function beginEvFetch() {
  return { alive: evAlive.capture(), seq: ++evSeq };
}
function evFetchStale(token) {
  return evAlive.stale(token.alive) || token.seq !== evSeq;
}

// Fill the EV card from the sim's private view of the car. Re-run
// after every plug/unplug and on a timer while the card is open, so
// the SoC and session lines move. No generation parameter: every
// teardown path (panel close, panel re-render, a new selection) runs
// stopEvTimer, which bumps evAlive — and the body element is re-read
// after the await, so a paint into a detached card is impossible.
async function refreshEvCard(id) {
  // A folded card is not worth a poll: nothing behind it is visible,
  // and the fold's own toggle refreshes it the moment it opens.
  if (!document.getElementById("card-ev")?.classList.contains("open")) return;
  if (!document.getElementById("ev-body")) return;
  const token = beginEvFetch();
  let info;
  try {
    const res = await fetch(`${mgPath("ev")}/${id}`);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    info = await res.json();
  } catch (err) {
    if (evFetchStale(token)) return;
    const body = document.getElementById("ev-body");
    if (!body) return;
    body.textContent = `EV view unavailable: ${err.message}`;
    // `evHtml` is only ever what is on screen — leaving the last good
    // body in it here would make the memo below swallow the repaint
    // that recovers from this error.
    evHtml = "";
    evHtmlId = null;
    return;
  }
  // Re-read the element rather than trusting one captured before the
  // await: a re-render during the fetch would leave that one detached.
  const body = evFetchStale(token) ? null : document.getElementById("ev-body");
  if (!body) return;
  body.classList.remove("hint");
  // An identical repaint is skipped, not re-applied. What this saves
  // is the UNPLUGGED form: innerHTML would reset it every two seconds,
  // throwing away a half-typed SoC or a picked preset under the user's
  // hands. A charging car's body changes text every poll, so it repaints
  // regardless — it holds nothing the user is typing into.
  const html = evBodyHtml(info);
  if (html === evHtml) return;
  evHtml = html;
  evHtmlId = id;
  body.innerHTML = html;
  // The inputs commit through this button, not on Enter or a wheel,
  // so the numeric fields need none of the knob rows' spinner/wheel
  // handling (see AGENTS.md's UI input convention).
  document.getElementById("ev-plug")?.addEventListener("click", async () => {
    const preset = document.getElementById("ev-preset").value;
    const soc = document.getElementById("ev-soc").value.trim();
    const target = document.getElementById("ev-target").value.trim();
    const args = `${soc ? ` :soc ${soc}` : ""}${target ? ` :target-soc ${target}` : ""}`;
    const res = await evalQuoted(`(plug-ev ${id} '${preset}${args})`, "Plug in failed");
    if (res.ok) refreshEvCard(id);
  });
  document.getElementById("ev-unplug")?.addEventListener("click", async () => {
    const res = await evalQuoted(`(unplug-ev ${id})`, "Unplug failed");
    if (res.ok) refreshEvCard(id);
  });
}

// One timer for the one selected charger, cleared by the node panel's
// teardown and by the next render. `evHtml` is the last body painted
// — and nothing else, so the repaint memo never skips a body that
// isn't already on screen — cleared alongside the timer so the next
// charger's first refresh always paints over its "loading…"
// placeholder. The evAlive bump is what disowns a fetch still parked
// on its await (see aliveToken).
let evTimer = null;
let evHtml = "";
// The charger whose body `evHtml` is, so a re-render of that same
// charger can keep the body instead of starting from "loading…".
let evHtmlId = null;
function stopEvTimer() {
  if (evTimer) clearInterval(evTimer);
  evTimer = null;
  evHtml = "";
  evHtmlId = null;
  evAlive.bump();
}

function renderInspect(d, parentIds, childIds) {
  // The delegated chip-click listener above reads the target id off
  // this dataset entry rather than a closure — see its comment.
  inspectEl.dataset.inspectId = d.id;

  // The grid connection point is the one category with nothing to
  // put in Component / Power / Setpoints: the sim's Grid takes no
  // knobs, accepts no setpoints, and publishes no per-component
  // telemetry to bound or plot. It gets Charts (the site frequency
  // stream) + Connections and nothing else, rather than three cards
  // that would each read blank.
  const isGrid = d.category === "grid";

  // Rename and disconnect rewrite the microgrid's file; on an
  // unmanaged one they are shown but inert, with the reason on
  // hover. Everything below them — modes, health, knobs — is
  // runtime state and stays live either way.
  const locked = !structureEditable();
  const lockAttrs = locked ? ` disabled title="${escapeHtml(READ_ONLY_TITLE)}"` : "";
  const renderEdgeRow = (id, dataAttr) => {
    const c = topology.get(id);
    const label = c ? c.name : `id ${id}`;
    return `<li>${escapeHtml(label)} <button class="link-btn" ${dataAttr}="${id}"${lockAttrs}>✕</button></li>`;
  };
  const parentList = parentIds.length
    ? parentIds.map((id) => renderEdgeRow(id, "data-disconnect-from")).join("")
    : '<li class="hint">none</li>';
  const childList = childIds.length
    ? childIds.map((id) => renderEdgeRow(id, "data-disconnect-to")).join("")
    : '<li class="hint">none</li>';

  const knobs = knobsFor(d);
  const knobRow = (k) => {
      const inputAttrs = k.dynamic
        ? `type="text" placeholder="value or (lambda () ...)"`
        : `type="number" step="any" placeholder="value"`;
      // `k.flag`, when set, renders a checkbox alongside the input
      // for an optional boolean arg (e.g. power factor's LEADING).
      // The checkbox by itself never submits anything — it only
      // qualifies whatever value is next entered into the input, at
      // which point the change handler below reads it and appends
      // it to the eval'd expression.
      const flagHtml = k.flag
        ? `<label class="knob-flag"><input type="checkbox" class="knob-flag-input" /> ${escapeHtml(k.flag)}</label>`
        : "";
      // Dynamic knobs (meter-power / meter-reactive-power / solar-
      // sunlight) additionally carry a small "expr" chip, shown
      // whenever the input's current text is a Lisp expression
      // rather than a plain number — see knobDisplay/toggleExprChip
      // below.
      const exprChipHtml = k.dynamic
        ? `<span class="knob-expr-chip" hidden>expr</span>`
        : "";
      // Same chip condition drives a resolved-value line beneath the
      // input — the printed source alone doesn't say what it's
      // currently evaluating to. Hidden until paintKnobEntry has a
      // reading to show; `data-unit` carries the display unit through
      // to paintKnobEntry without another lookup table there.
      const resolvedHtml = k.dynamic ? `<div class="knob-resolved" hidden></div>` : "";
      // `k.clear`: the small "✕ measure" button, styled like the
      // link-btn disconnect affordances above. Starts hidden — shown
      // only while this token carries a live override, toggled by
      // paintKnobEntry/setKnobText alongside the input text itself.
      const measureBtnHtml = k.clear
        ? `<button type="button" class="link-btn knob-measure-btn" data-clear="${k.clear}" hidden title="clear override, measure from children again">✕</button>`
        : "";
      return `<dt>${escapeHtml(k.label)}</dt><dd>
        <input ${inputAttrs} class="knob-input"
               data-defun="${k.defun}"${k.dynamic ? ` data-dynamic="1" data-unit="${escapeHtml(k.unit || "")}"` : ""} />${exprChipHtml}${flagHtml}${measureBtnHtml}${resolvedHtml}
      </dd>`;
    };
  const configKnobsHtml = knobs.filter((k) => k.group === "config").map(knobRow).join("");
  const simKnobsHtml = knobs.filter((k) => k.group !== "config").map(knobRow).join("");

  // Charts fold summary — just a metric count; the charts themselves
  // aren't fetched until first unfold (see renderNode/buildCharts).
  const metrics = CHARTS_BY_CATEGORY[d.category] || [];
  const chartsSummary = isGrid
    ? "site frequency"
    : metrics.length
      ? `${metrics.length} metric${metrics.length === 1 ? "" : "s"}`
      : "none";

  inspectEl.innerHTML = `
    <h2><input id="rename" class="name-input" value="${escapeHtml(d.name)}"${lockAttrs} /></h2>
    <div class="insp-meta">
      <span class="insp-meta-id">id ${d.id}</span>
      <span class="insp-chip insp-cat-chip" style="color:var(--cat-${escapeHtml(d.category)});border-color:var(--cat-${escapeHtml(d.category)})">${d.subtype ? `${escapeHtml(d.category)} · ${escapeHtml(d.subtype)}` : escapeHtml(d.category)}</span>
      <span class="insp-chip insp-health-chip ${escapeHtml(d.health)}">${escapeHtml(d.health)}</span>
      <span class="insp-chip insp-augmented" id="insp-augmented" hidden>augmented</span>
    </div>

    ${isGrid ? "" : `
    <div class="insp-card fold" id="card-component" data-card="component">
      <h3 class="fold-toggle" data-fold-toggle>Component<span class="fold-summary"><span class="fold-chevron">▾</span></span></h3>
      <div class="fold-body">
        <h4>Graph</h4>
        <dl>
          <dt>mode</dt><dd>${selectField("operational-mode", d.operational_mode, OPERATIONAL_MODES.map((m) => m.value))}</dd>
        </dl>
        ${configKnobsHtml ? `<h4>Config</h4><dl>${configKnobsHtml}</dl>` : ""}
        <h4>Simulation</h4>
        <dl>
          <dt>health</dt><dd>${renderSegRow("health", d.health, ["ok", "error", "standby"])}</dd>
          <dt>telemetry</dt><dd>${selectField("telemetry-mode", d.telemetry_mode, ["normal", "silent", "closed", "error-empty", "not-found"], d.provides_telemetry === false ? `operational mode ${d.operational_mode} streams no telemetry` : null)}</dd>
          ${ACCEPTS_SETPOINTS.has(d.category)
            ? `<dt>commands</dt><dd>${selectField("command-mode", d.command_mode, ["normal", "timeout", "error", "over-bound"], d.accepts_control === false ? `operational mode ${d.operational_mode} accepts no commands` : null)}</dd>`
            : ""}
          ${simKnobsHtml}
        </dl>
        <p class="hint" id="knob-readback-hint" hidden></p>
      </div>
    </div>

    ${d.category === "ev-charger" ? `
    <div class="insp-card fold" id="card-ev" data-card="ev">
      <h3 class="fold-toggle" data-fold-toggle>EV<span class="fold-summary"><span class="fold-chevron">▾</span></span></h3>
      <div class="fold-body"><div id="ev-body" class="hint">loading…</div></div>
    </div>
    ` : ""}

    <div class="insp-card fold" id="card-power" data-card="power">
      <h3 class="fold-toggle" data-fold-toggle>Power<span class="fold-summary"><span class="fold-chevron">▾</span></span></h3>
      <div class="fold-body">
        <div class="env-axis">
          <div class="env-head"><span class="env-label">P</span><span class="env-val" data-envelope-val="active">—</span></div>
          <div class="env-bar" data-envelope="active"><div class="env-live" hidden></div><div class="env-sp" hidden></div></div>
          <div class="env-ends" data-envelope-ends="active"></div>
          <div class="env-setpoint hint" data-envelope-setpoint="active" hidden></div>
        </div>
        <div class="env-axis">
          <div class="env-head"><span class="env-label">Q</span><span class="env-val" data-envelope-val="reactive">—</span></div>
          <div class="env-bar" data-envelope="reactive"><div class="env-live" hidden></div><div class="env-sp" hidden></div></div>
          <div class="env-ends" data-envelope-ends="reactive"></div>
          <div class="env-setpoint hint" data-envelope-setpoint="reactive" hidden></div>
        </div>
      </div>
    </div>
    `}

    <div class="insp-card fold" id="card-charts" data-card="charts">
      <h3 class="fold-toggle" data-fold-toggle>Charts<span class="fold-summary">${chartsSummary}<span class="fold-chevron">▾</span></span></h3>
      <div class="fold-body"><div id="charts"></div></div>
    </div>

    ${isGrid ? "" : `
    <div class="insp-card fold" id="card-setpoints" data-card="setpoints">
      <h3 class="fold-toggle" data-fold-toggle>Setpoints<span class="fold-summary"><span class="fold-chevron">▾</span></span></h3>
      <div class="fold-body"><div id="setpoints-section"></div></div>
    </div>
    `}

    <div class="fold" id="connections-fold">
      <h3 class="fold-toggle" data-fold-toggle>Connections<span class="fold-summary">${parentIds.length} parents · ${childIds.length} children<span class="fold-chevron">▾</span></span></h3>
      <div class="fold-body">
        <div class="conns">
          <div><strong>parents</strong><ul>${parentList}</ul></div>
          <div><strong>children</strong><ul>${childList}</ul></div>
        </div>
      </div>
    </div>
  `;

  // Wire form callbacks. Every action POSTs to /api/eval; the WS
  // TopologyChanged refresh re-reads the form state from the server
  // and re-renders this panel automatically. Mode/health/telemetry/
  // commands chip clicks are handled by the single delegated
  // listener on inspectEl, above — not wired per-render here.
  document.getElementById("rename").addEventListener("change", (e) => {
    const name = e.target.value.trim();
    if (!name) return;
    evalQuoted(`(rename-component ${d.id} "${jsToLispString(name)}")`);
  });
  // Edit-in-place: focus freezes the input's and its flag checkbox's
  // visible WRITES against live/snapshot updates (data-editing — read
  // by paintKnobEntry/setKnobText, which still keep dataset.live/the
  // flag's own dataset.live current underneath so nothing newer is
  // lost). Enter commits via evalQuoted and, on success, remembers
  // the committed text (and flag state) as the new "live" baseline
  // (data-live) instead of clearing the field. Esc and a plain blur
  // both restore data-live (input text AND flag checked state), so
  // anything typed but never committed — or rejected by the server —
  // reverts rather than sticking around.
  // A sibling `.knob-flag-input`, if this knob has one, qualifies the
  // committed value as an optional trailing `t` arg (e.g. power
  // factor's LEADING).
  for (const inp of inspectEl.querySelectorAll(".knob-input")) {
    inp.dataset.live = inp.value;
    const flag = inp.closest("dd")?.querySelector(".knob-flag-input");
    if (flag) flag.dataset.live = flag.checked ? "1" : "0";
    // Same reason the native spinners are hidden (style.css): in Firefox
    // a wheel over a focused type=number steps the value without
    // committing it, and the next snapshot reverts it. Arrow KEYS are
    // left alone — those are deliberate keyboard edits, one Enter from a
    // commit. Non-passive so preventDefault holds; number fields only,
    // so a wheel over a text knob still scrolls the inspector.
    if (inp.type === "number") {
      inp.addEventListener("wheel", (e) => e.preventDefault(), { passive: false });
    }
    inp.addEventListener("focus", () => {
      inp.dataset.editing = "1";
    });
    inp.addEventListener("blur", () => {
      delete inp.dataset.editing;
      inp.value = inp.dataset.live ?? "";
      if (flag) flag.checked = flag.dataset.live === "1";
    });
    inp.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        const v = inp.value.trim();
        if (v === "") return;
        evalQuoted(
          `(${inp.dataset.defun} ${d.id} ${v}${flag?.checked ? " t" : ""})`,
        ).then((res) => {
          if (!res.ok) {
            inp.value = inp.dataset.live ?? "";
            if (flag) flag.checked = flag.dataset.live === "1";
            return;
          }
          inp.dataset.live = v;
          if (flag) flag.dataset.live = flag.checked ? "1" : "0";
          if (inp.dataset.dynamic === "1") {
            const isExpr = !(v !== "" && Number.isFinite(Number(v)));
            toggleExprChip(inp, isExpr);
            // The actual resolved-value reading arrives shortly via
            // the WS knob_changed event (paintKnobEntry); until then,
            // a just-committed plain number can't be showing a stale
            // resolved line from a previous expression.
            if (!isExpr) paintResolved(inp, false, null);
          }
        });
      } else if (e.key === "Escape") {
        e.preventDefault();
        inp.blur();
      }
    });
  }
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-from]")) {
    btn.addEventListener("click", () =>
      evalQuoted(`(disconnect ${btn.dataset.disconnectFrom} ${d.id})`),
    );
  }
  for (const btn of inspectEl.querySelectorAll("[data-disconnect-to]")) {
    btn.addEventListener("click", () =>
      evalQuoted(`(disconnect ${d.id} ${btn.dataset.disconnectTo})`),
    );
  }
  // Meter knob "measure" buttons — see the `clear` field on
  // KNOBS_BY_CATEGORY. The WS knob_changed broadcast the clear defun
  // triggers is what actually blanks the input and hides the button
  // again (paintKnobEntry, via inspectorLive.applyKnob).
  for (const btn of inspectEl.querySelectorAll(".knob-measure-btn")) {
    btn.addEventListener("click", () => evalQuoted(`(${btn.dataset.clear} ${d.id})`));
  }

  // Component, EV, Power, and Setpoints cards: persisted per-card fold
  // state. Only the EV card does work behind its fold — its poll skips
  // a folded card, so unfolding asks for one refresh (the setpoint list
  // is rendered, and keeps accumulating WS events, whether or not its
  // card is open). The Charts card (persisted too, but with a
  // first-unfold chart build) is wired by renderNode below — it
  // needs the render's generation guard, which only renderNode has.
  // A grid selection renders none of these, and the EV card only
  // exists on a charger, so the lookup is allowed to come back empty
  // rather than being asserted.
  for (const name of ["component", "ev", "power", "setpoints"]) {
    const card = document.getElementById(`card-${name}`);
    if (!card) continue;
    card.classList.toggle("open", loadCardOpen(name));
    card.querySelector("[data-fold-toggle]").addEventListener("click", () => {
      const open = !card.classList.contains("open");
      card.classList.toggle("open", open);
      saveCardOpen(name, open);
      // The EV poll skips a folded card, so unfolding has to ask for
      // the one refresh that fills it (the timer takes over from there).
      if (open && name === "ev") refreshEvCard(d.id);
    });
  }

  // Connections fold: session-only toggle, always starts folded.
  const connFold = document.getElementById("connections-fold");
  connFold.querySelector("[data-fold-toggle]").addEventListener("click", () => {
    connFold.classList.toggle("open");
  });
}

// ── Live read-back ─────────────────────────────────────────────────
//
// Snapshot fetch (/api/component), edit-in-place knob prefill,
// envelope bars, and the setpoint TTL row for whichever node is
// currently shown. One session slot (`liveState`), mirroring
// liveCharts' one-active-selection discipline: rebuilt wholesale on
// every showComponent, dropped on teardown (stopTtlTimer, called from
// the node tenant's teardown alongside liveCharts.clear()).
//
// liveState = {
//   id,
//   knobEntries: Map<token, { input: HTMLInputElement }>,
//   axes: {
//     active:   { lo, hi, liveVal, sp: { value, deadlineMs } | null },
//     reactive: { lo, hi, liveVal, sp: { value, deadlineMs } | null },
//   },
// }
let liveState = null;
let ttlTimerId = null;

// The start-order half of the /api/component fetch guard, alongside
// `snapshotAlive`: inspectorLive.applySetpoint's re-fetch has no
// `gen` at all (it isn't triggered by a render), so two accepted
// setpoint events for the same still-open node can resolve out of
// order. Every snapshot-triggering fetch (the initial one AND every
// applySetpoint re-fetch) bumps this on start, and only the
// most-recently-STARTED one's resolution is allowed to paint.
let snapshotSeq = 0;

// Call when starting any /api/component fetch that will (on success)
// call applySnapshot. Returns a token to pass to snapshotFetchStale
// once the fetch settles.
function beginSnapshotFetch() {
  return { alive: snapshotAlive.capture(), seq: ++snapshotSeq };
}
function snapshotFetchStale(token) {
  return snapshotAlive.stale(token.alive) || token.seq !== snapshotSeq;
}

const EXPR_PLACEHOLDER = "(expression)";

// Decide what a knob input should show, given a snapshot/WS reading's
// `value` + `expr`. The server ships `expr` unfiltered on both paths
// (no server-side normalization) — an unquoted `(lambda …)` literal
// evaluates to tulisp's opaque `CompiledDefun` closure before the
// component ever sees it, and that opaque Display is caught here,
// the one place both the snapshot and WS paths funnel through, so
// they can't render it differently.
//
// `value == null && expr == null` on a dynamic knob is a CLEARED
// override (clear-meter-power/clear-meter-reactive broadcast exactly
// this) — blank the input, same as any other knob with nothing
// configured. That's distinct from a dynamic source with a captured
// expression but no resolved value yet, which the `expr != null`
// branch above already renders (placeholder for the opaque closure
// Display, printed source otherwise) — there's no live case left
// where a dynamic knob's null/null pair means anything but "cleared".
function knobDisplay(value, expr) {
  if (expr != null) {
    const opaque = expr.startsWith("CompiledDefun");
    return { text: opaque ? EXPR_PLACEHOLDER : expr, hasExpr: true };
  }
  return { text: value != null ? String(value) : "", hasExpr: false };
}

function toggleExprChip(input, show) {
  const chip = input.closest("dd")?.querySelector(".knob-expr-chip");
  if (chip) chip.hidden = !show;
}

// The meter knobs' small "✕ measure" button — visible only while
// this token carries a live override (a value present, whether or
// not it's expression-driven). Shared by paintKnobEntry (WS/snapshot
// updates) and setKnobText (the "no state for this token" blank-out),
// so a cleared or never-set override can't leave the button showing.
function toggleMeasureBtn(input, show) {
  const btn = input.closest("dd")?.querySelector(".knob-measure-btn");
  if (btn) btn.hidden = !show;
}

// The small muted line beneath an expression-driven knob's input,
// showing what it's currently resolving to this tick — the printed
// source above doesn't say that on its own. Always live, never frozen
// by data-editing: it's read-only, reporting the last-committed
// expression's current output, not whatever the user is mid-typing.
function paintResolved(input, show, value) {
  const el = input.closest("dd")?.querySelector(".knob-resolved");
  if (!el) return;
  if (!show || value == null) {
    // Clear as well as hide: a stale "→ …" line must not reappear
    // if anything ever unhides the element without repainting it.
    el.textContent = "";
    el.hidden = true;
    return;
  }
  el.textContent = `→ ${formatScaled(value, input.dataset.unit || "")} this tick`;
  el.hidden = false;
}

// Paint one knob entry's input (+ its optional leading checkbox + its
// resolved-value line + its measure button) from a {value, expr,
// leading} reading — shared by the snapshot prefill and the WS
// knob_changed apply, so the two paths can't drift.
//
// `input.dataset.live` (and the flag's own `dataset.live`) are ALWAYS
// refreshed to the newest reading, even while the user has the field
// focused — otherwise a live update that arrives during an edit is
// silently dropped, and Esc/blur restores a value as stale as focus
// time instead of the actual current one. Only the visible WRITES
// (input.value, the expr chip, the flag checkbox) are frozen while
// `data-editing` is set, so a live update can't yank text out from
// under the user mid-edit. The measure button is deliberately NOT
// frozen — like `paintResolved`, it runs above the editing guard
// below, always live.
function paintKnobEntry(entry, value, expr, leading) {
  const { input } = entry;
  const disp = knobDisplay(value, expr);
  input.dataset.live = disp.text;
  const flag = input.closest("dd")?.querySelector(".knob-flag-input");
  // `leading` is only meaningful on the power-factor knob — every
  // other token's event carries it null, and there's no flag sibling
  // there to write anyway. On the power-factor knob's OWN flag,
  // though, a null value means the reactive clear just blanked this
  // token: there's no override left to be leading/lagging, so force
  // it unchecked rather than leaving the last reading's state stuck.
  if (flag) {
    if (leading != null) flag.dataset.live = leading ? "1" : "0";
    else if (value == null) flag.dataset.live = "0";
  }
  paintResolved(input, disp.hasExpr, value);
  toggleMeasureBtn(input, value != null);
  if (input.dataset.editing) return;
  input.value = disp.text;
  toggleExprChip(input, disp.hasExpr);
  if (flag) {
    if (leading != null) flag.checked = leading;
    else if (value == null) flag.checked = false;
  }
}

// Write a plain (non-expr) text reading to a knob input — used by
// prefillKnobs' "no state for this token" blank-out, which doesn't
// go through knobDisplay. Same live-vs-editing discipline as
// paintKnobEntry: dataset.live always updates, the visible write is
// frozen while the user has the field focused. No state for the
// token also means no override in force, so the measure button hides
// along with everything else.
function setKnobText(input, text) {
  input.dataset.live = text;
  paintResolved(input, false, null);
  toggleMeasureBtn(input, false);
  if (input.dataset.editing) return;
  input.value = text;
  toggleExprChip(input, false);
}

// Bar geometry — same clamped formula as hovercard.js's envelopeBar
// (hovercard.js:112-121).
function markerPct(value, lo, hi) {
  if (!Number.isFinite(value) || !Number.isFinite(lo) || !Number.isFinite(hi) || hi <= lo) {
    return null;
  }
  return Math.max(0, Math.min(100, ((value - lo) / (hi - lo)) * 100));
}

function paintBar(axis, unit) {
  const st = liveState.axes[axis];
  const bar = inspectEl.querySelector(`[data-envelope="${axis}"]`);
  const ends = inspectEl.querySelector(`[data-envelope-ends="${axis}"]`);
  if (!bar || !ends) return;
  // The per-axis readout above the bar, fed by the same live WS
  // samples as the marker. Coloured with the topology nodes' flow
  // convention (export green / import blue / dim in the dead band),
  // with the dead band derived from this component's own envelope
  // rather than the site-wide reference the canvas uses.
  const valEl = inspectEl.querySelector(`[data-envelope-val="${axis}"]`);
  if (valEl) {
    valEl.textContent = Number.isFinite(st.liveVal)
      ? formatScaled(st.liveVal, unit)
      : "—";
    const maxAbs = Math.max(Math.abs(st.lo ?? 0), Math.abs(st.hi ?? 0));
    const db = deadBandW(Number.isFinite(maxAbs) ? maxAbs : 0);
    valEl.style.color =
      axis === "active" ? powerColor(st.liveVal, db) : reactiveColor(st.liveVal, db);
  }
  const liveEl = bar.querySelector(".env-live");
  const spEl = bar.querySelector(".env-sp");
  const livePct = markerPct(st.liveVal, st.lo, st.hi);
  const spPct = markerPct(st.sp?.value, st.lo, st.hi);
  liveEl.hidden = livePct == null;
  if (livePct != null) liveEl.style.left = `${livePct.toFixed(1)}%`;
  spEl.hidden = spPct == null;
  if (spPct != null) spEl.style.left = `${spPct.toFixed(1)}%`;
  ends.innerHTML =
    Number.isFinite(st.lo) && Number.isFinite(st.hi)
      ? `<span>${escapeHtml(formatScaled(st.lo, unit))}</span><span>${escapeHtml(formatScaled(st.hi, unit))}</span>`
      : "";
}

function ttlText(value, unit, remainingMs) {
  const ttl = remainingMs == null ? "—" : `${Math.max(0, Math.ceil(remainingMs / 1000))}s`;
  return `▾ ${formatScaled(value, unit)} · TTL ${ttl}`;
}

function paintTtlRow(axis, unit) {
  const el = inspectEl.querySelector(`[data-envelope-setpoint="${axis}"]`);
  if (!el) return;
  const sp = liveState.axes[axis].sp;
  if (!sp) {
    el.hidden = true;
    return;
  }
  el.hidden = false;
  const remaining = sp.deadlineMs == null ? null : Math.max(0, sp.deadlineMs - Date.now());
  el.textContent = ttlText(sp.value, unit, remaining);
}

function paintAxis(axis, unit) {
  paintBar(axis, unit);
  paintTtlRow(axis, unit);
}

function paintAugmented(flags) {
  const el = document.getElementById("insp-augmented");
  if (!el) return;
  const parts = [];
  if (flags?.active) parts.push("P");
  if (flags?.reactive) parts.push("Q");
  el.hidden = parts.length === 0;
  el.title = parts.length ? `bounds narrowed: ${parts.join(", ")}` : "";
}

// Prefill every knob input from a snapshot's `knobs` list, keyed by
// stripping the input's own `set-` defun prefix down to the server's
// token (set-meter-power → meter-power, …).
// An input with `data-editing` (the user has it focused) still gets
// its `dataset.live` refreshed — only the visible input/chip/checkbox
// write is skipped — matching the same freeze setKnobText/
// paintKnobEntry apply, and inspectorLive.applyKnob relies on below.
function prefillKnobs(snap) {
  const byToken = new Map((snap.knobs || []).map((k) => [k.knob, k]));
  for (const [token, entry] of liveState.knobEntries) {
    const state = byToken.get(token);
    if (!state) {
      setKnobText(entry.input, "");
      continue;
    }
    paintKnobEntry(entry, state.value, state.expr, state.leading);
  }
}

// Rebuild liveState from a fresh /api/component snapshot and repaint
// everything it drives: knob prefill, both envelope bars, both TTL
// rows, the augmented badge. Shared by the initial fetch (renderNode)
// and inspectorLive.applySetpoint's re-fetch on an accepted setpoint
// — same shape either way, so there's exactly one place that turns a
// snapshot into DOM.
function applySnapshot(id, snap) {
  const hintEl = document.getElementById("knob-readback-hint");
  if (hintEl) hintEl.hidden = true;

  // Live P/Q values only ever come from WS sample events (the
  // snapshot doesn't carry them) — carry the last-observed ones
  // across a re-fetch instead of blanking the marker for a beat.
  const prevAxes = liveState && liveState.id === id ? liveState.axes : null;
  const knobEntries = new Map();
  for (const input of inspectEl.querySelectorAll(".knob-input[data-defun]")) {
    const token = input.dataset.defun.replace(/^set-/, "");
    knobEntries.set(token, { input });
  }
  liveState = {
    id,
    knobEntries,
    axes: {
      active: { lo: null, hi: null, liveVal: prevAxes?.active.liveVal ?? null, sp: null },
      reactive: { lo: null, hi: null, liveVal: prevAxes?.reactive.liveVal ?? null, sp: null },
    },
  };

  // A null snapshot axis (no setpoint envelope AND no own bounds —
  // rare, but the server can still return it) must not blank out
  // bounds the WS bound-Sample stream already drew (see applySample
  // below): fall back to the last-known lo/hi the same way liveVal
  // carries over above. Without this, every accepted setpoint's
  // re-fetch raced the stream and flickered the graduation empty for
  // a beat.
  const [aLo, aHi] = snap.envelope?.active ?? [null, null];
  liveState.axes.active.lo = aLo ?? prevAxes?.active.lo ?? null;
  liveState.axes.active.hi = aHi ?? prevAxes?.active.hi ?? null;
  const [rLo, rHi] = snap.envelope?.reactive ?? [null, null];
  liveState.axes.reactive.lo = rLo ?? prevAxes?.reactive.lo ?? null;
  liveState.axes.reactive.hi = rHi ?? prevAxes?.reactive.hi ?? null;

  const now = Date.now();
  for (const sp of snap.setpoints || []) {
    if (sp.axis !== "active" && sp.axis !== "reactive") continue;
    liveState.axes[sp.axis].sp = {
      value: sp.value,
      deadlineMs: sp.remaining_ms != null ? now + sp.remaining_ms : null,
    };
  }

  prefillKnobs(snap);
  paintAxis("active", "W");
  paintAxis("reactive", "VAr");
  paintAugmented(snap.augmented);
  startTtlTimer();
}

// The raw /api/component GET, factored out so a single in-flight
// fetch can be shared by fetchSnapshot (knob read-back) and, for a
// steam boiler, buildCharts's pressure-target title suffix — both
// want the same snapshot in the same render pass, and issuing it
// twice would double the GETs every time a boiler is selected with
// the Charts card pinned open.
function fetchComponentSnapshot(id) {
  return fetch(`${mgPath("component")}?id=${id}`).then(async (res) => {
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return res.json();
  });
}

async function fetchSnapshot(id, gen, snapshotJsonP) {
  const token = beginSnapshotFetch();
  try {
    const snap = await snapshotJsonP;
    // gen !== showGen: a newer selection has started. snapshotFetchStale:
    // the tenant was torn down (closePanel / switched tenants) or a
    // more-recent snapshot fetch (reselect or an applySetpoint
    // re-fetch) has since started — either way this resolve is stale
    // and must not paint or (re)start the TTL timer.
    if (gen !== showGen || snapshotFetchStale(token)) return;
    applySnapshot(id, snap);
  } catch (err) {
    if (gen !== showGen || snapshotFetchStale(token)) return;
    // Degrade rule: knobs stay blank write-only, everything else
    // (charts, connections, mode/health chips) still renders — this
    // fetch is the only thing that failed.
    const hintEl = document.getElementById("knob-readback-hint");
    if (hintEl) {
      hintEl.hidden = false;
      hintEl.textContent = `read-back unavailable: ${err.message}`;
    }
  }
}

// The 1 s TTL countdown, owned by whichever node is currently shown.
// Idempotent start (a snapshot re-fetch calls this again on every
// accepted setpoint); stop also drops the live-readback session
// itself, mirroring liveCharts.clear() — this is the node tenant's
// second teardown hook (see showComponent below).
function startTtlTimer() {
  if (ttlTimerId != null) return;
  ttlTimerId = setInterval(() => {
    if (!liveState) return;
    paintTtlRow("active", "W");
    paintTtlRow("reactive", "VAr");
  }, 1000);
}
function stopTtlTimer() {
  if (ttlTimerId != null) {
    clearInterval(ttlTimerId);
    ttlTimerId = null;
  }
  liveState = null;
  // Disown any /api/component fetch still in flight for the panel
  // being torn down, so it can't resurrect this timer (see aliveToken).
  snapshotAlive.bump();
}

// Prepend one WS setpoint event to the Recent setpoints list, keyed
// off `liveState.id` rather than the Charts fold's `liveCharts` — the
// list has to keep growing whether or not the user has ever unfolded
// Charts (liveState is populated as soon as a node is selected, charts
// only on first unfold). Shared shape with the REST backfill via
// setpointEventLi, below.
function appendSetpointEvent(ev) {
  const list = inspectEl.querySelector(".sp-list");
  if (!list) return;
  // Drop the "none" placeholder if it's still showing.
  const empty = list.querySelector(".sp-empty");
  if (empty) empty.remove();
  // The WS event carries the setpoint kind on `setpoint_kind` to
  // dodge collision with the SiteEvent discriminator (also called
  // `kind`).
  list.prepend(setpointEventLi(ev.ts_ms, ev.setpoint_kind, ev.value, ev.accepted, ev.reason));
  // Trim if the list is getting long — match the 600s window used by
  // the initial fetch.
  while (list.children.length > 100) list.removeChild(list.lastChild);
}

// Exported so the WS hub (repl.js openWebSocket) can feed it knob /
// sample / setpoint events for whichever node the inspector currently
// has open. Every entry point no-ops when nothing is open or the
// event is for a different component — same "is the right component
// selected" guard liveCharts.pushSample uses.
export const inspectorLive = {
  applyKnob(ev) {
    if (!liveState || Number(ev.id) !== liveState.id) return;
    const entry = liveState.knobEntries.get(ev.knob);
    if (!entry) return;
    // paintKnobEntry always refreshes dataset.live (and the resolved
    // line); it only skips the visible input/chip/checkbox write
    // while the field is focused (data-editing).
    paintKnobEntry(entry, ev.value, ev.expr, ev.leading);
  },
  applySample(ev) {
    if (!liveState || Number(ev.id) !== liveState.id) return;
    const axes = liveState.axes;
    switch (ev.metric) {
      case "active_power_w":
        axes.active.liveVal = ev.value;
        break;
      case "reactive_power_var":
        axes.reactive.liveVal = ev.value;
        break;
      case "active_power_lower_bound_w":
        axes.active.lo = ev.value;
        break;
      case "active_power_upper_bound_w":
        axes.active.hi = ev.value;
        break;
      case "reactive_power_lower_bound_var":
        axes.reactive.lo = ev.value;
        break;
      case "reactive_power_upper_bound_var":
        axes.reactive.hi = ev.value;
        break;
      default:
        return;
    }
    paintAxis("active", "W");
    paintAxis("reactive", "VAr");
  },
  applySetpoint(ev) {
    if (!liveState || Number(ev.id) !== liveState.id) return;
    // The Recent setpoints list gets every event, accepted or not —
    // unlike the envelope/TTL refresh below, which only an accepted
    // setpoint can move.
    appendSetpointEvent(ev);
    if (!ev.accepted) return;
    const id = liveState.id;
    // Neither the snapshot envelope's freshness nor the setpoint's
    // remaining-TTL is on the WS frame itself, so an accepted
    // active/reactive/augment_* event just re-fetches the one
    // endpoint that carries all of it. Two accepted events for the
    // same component (or a reselect racing this re-fetch) can
    // resolve out of order — snapshotFetchStale drops any resolve
    // that isn't the most-recently-started snapshot fetch, and/or
    // whose tenant has since been torn down, so an older response
    // can never paint over a newer one.
    const token = beginSnapshotFetch();
    fetch(`${mgPath("component")}?id=${id}`)
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        return res.json();
      })
      .then((snap) => {
        if (snapshotFetchStale(token)) return;
        if (liveState && liveState.id === id) applySnapshot(id, snap);
      })
      .catch(() => {
        // Best-effort refresh only — the setpoint itself already
        // landed in the Recent setpoints list via appendSetpointEvent,
        // above; the bars/TTL just keep showing whatever the last
        // successful snapshot had.
      });
  },
};

// Bumped on every showComponent call. Rapid node selection races two
// async renders; an await that resolves after a newer call started
// carries a stale generation and bails out (destroying any uPlots it
// already built) — same last-STARTED-wins guard as dispatchesPanel's
// render and formula-panel's refreshFormula.
let showGen = 0;

export function showComponent(d) {
  if (!d) return;
  const gen = ++showGen;
  // Any eval bumps the topology version, and the refresh re-renders
  // the inspector through here. For the charger already on screen,
  // keep its EV body: a half-filled plug form survives, and the card
  // does not flash back to "loading…". Read before openPanel runs the
  // teardown, which forgets the memo.
  const keptEvCard =
    d.category === "ev-charger" && evHtmlId === d.id
      ? { body: document.getElementById("ev-body"), html: evHtml, focused: document.activeElement }
      : null;
  // The three clears below are the node panel's whole teardown —
  // side-panel.js runs the previously registered one before
  // renderNode paints, so re-selecting a node tears the old node's
  // charts, store subscription, and timers down first.
  openPanel("node", () => renderNode(d, gen, keptEvCard), () => {
    // The bump IS the teardown for any chart build still parked on
    // its await (see aliveToken).
    chartsAlive.bump();
    liveCharts.clear();
    clearGridChart();
    stopTtlTimer();
    stopEvTimer();
  });
}

async function renderNode(d, gen, keptEvCard) {
  // vis-network's getConnectedNodes(id, direction) returns the
  // ids on either side of the selected node — cheaper than walking
  // /api/topology for the disconnect buttons. Display labels get
  // resolved by renderInspect via topology.get().
  const parentIds = topology.parentsOf(d.id);
  const childIds = topology.childrenOf(d.id);
  renderInspect(d, parentIds, childIds);

  stopEvTimer();
  if (d.category === "ev-charger") {
    if (keptEvCard?.body) {
      document.getElementById("ev-body")?.replaceWith(keptEvCard.body);
      // Moving the body out and back drops focus; a field the user
      // was typing in gets it back.
      if (keptEvCard.body.contains(keptEvCard.focused)) keptEvCard.focused.focus();
      evHtml = keptEvCard.html;
      evHtmlId = d.id;
    }
    refreshEvCard(d.id);
    evTimer = setInterval(() => refreshEvCard(d.id), 2000);
  }

  // Charts card: history fetch + live-chart wiring are deferred to
  // first unfold (folded by default; open state persists across
  // selections via localStorage). `built` guards against wiring the
  // charts twice if the card is folded/unfolded repeatedly.
  const isGrid = d.category === "grid";
  // One /api/component fetch per selection — shared by the knob
  // read-back below (fetchSnapshot) and buildCharts's pressure-target
  // title suffix — kicked off here so it's already in flight by the
  // time either consumer wants it, whichever runs first. The grid has
  // no knobs/setpoints and no pressure chart, so it never asks.
  const snapshotJsonP = isGrid ? null : fetchComponentSnapshot(d.id);
  const chartsFold = document.getElementById("card-charts");
  const chartsContainer = document.getElementById("charts");
  let built = false;
  const buildChartsOnce = async () => {
    if (built) return;
    built = true;
    // Captured before the await: a reselect moves showGen, a panel
    // close moves only the token, and either one makes this build's
    // result stale (see aliveToken).
    const alive = chartsAlive.capture();
    const stale = () => gen !== showGen || chartsAlive.stale(alive);
    if (isGrid) {
      const chart = await buildGridFrequencyChart(chartsContainer);
      // Destroy here rather than parking a live store subscription in
      // a slot nothing will ever clear again.
      if (stale()) chart?.destroy();
      else gridChart = chart;
      return;
    }
    const charts = await buildCharts(d, chartsContainer, snapshotJsonP);
    // Same discipline: bail without activating the live-push session
    // so a stale node's charts can't clobber whatever the newer
    // selection is showing, or outlive a closed panel.
    if (stale()) {
      for (const ch of charts.values()) ch.plot.destroy();
      return;
    }
    liveCharts.set(d.id, charts);
  };
  chartsFold
    .querySelector("[data-fold-toggle]")
    .addEventListener("click", () => {
      const open = !chartsFold.classList.contains("open");
      chartsFold.classList.toggle("open", open);
      // The grid's fold state stays session-local: the key is shared
      // across categories, so persisting a fold of the GCP's ONLY
      // card would silently fold every other component's Charts too.
      if (!isGrid) saveCardOpen("charts", open);
      if (open) buildChartsOnce();
    });
  // The grid opens on arrival whatever the shared persisted state
  // says — its Charts card is the panel's only card, and a GCP that
  // renders as a single folded header reads as a broken panel.
  if (isGrid || loadCardOpen("charts")) {
    chartsFold.classList.add("open");
    buildChartsOnce();
  }

  // A grid selection has no Component / Power / Setpoints card to
  // fill, and the sim's Grid takes neither knobs nor setpoints — so
  // neither fetch below has anywhere to paint.
  if (isGrid) return;

  // Setpoint events: list recent control-app requests + outcome.
  // Unlike charts this always runs on open — it's cheap, and the
  // list has to exist (hidden inside the folded card) so incoming WS
  // events keep accumulating via appendSetpointEvent. The read-back
  // snapshot (knobs, envelope bars, TTL row) fetches concurrently —
  // same generation guard, its own failure path (fetchSnapshot).
  const snapshotP = fetchSnapshot(d.id, gen, snapshotJsonP);
  await renderSetpoints(d.id, document.getElementById("setpoints-section"));
  await snapshotP;
}

// The grid connection point's single chart. It reads the site-wide
// `grid_frequency` stream out of the metrics store rather than
// /api/history, because the sim's Grid publishes no per-component
// telemetry — the old per-component frequency_hz fetch is exactly
// what drew an empty chart here. Same source as the metrics panel's
// Frequency card, and the store is fed by repl.js on every
// microgrid_sample frame regardless of that panel's open state, so
// this chart grows live on its own.
//
// Returns { plot, destroy }; the caller owns the slot (see
// gridChart / clearGridChart above).
async function buildGridFrequencyChart(container) {
  const slot = document.createElement("div");
  slot.className = "chart";
  container.appendChild(slot);
  const Plot = requireUplot(slot);
  if (!Plot) return null;
  // Backfill first so the chart opens on the last 15 min of trend
  // instead of growing from empty; it is a no-op-ish refresh when
  // the metrics panel has already pulled it.
  await metricsStore.backfill();
  const series = () => {
    const { xs, ys } = metricsStore.series("grid_frequency", 600);
    return [xs, ys];
  };
  const opts = {
    width: slot.clientWidth || 280,
    height: 140,
    title: "Frequency (site) (Hz)",
    cursor: { drag: { x: false, y: false } },
    legend: { show: false },
    scales: { x: { time: true } },
    axes: [
      { stroke: "#7d848e", grid: { stroke: "#353a45", width: 0.5 } },
      { stroke: "#7d848e", grid: { stroke: "#353a45", width: 0.5 }, size: 60 },
    ],
    series: [
      {},
      { stroke: "#79b8ff", width: 1.5, points: { show: false }, spanGaps: false },
    ],
  };
  const plot = new Plot(opts, series(), slot);
  // The store notifies per sample; coalesce to one repaint a frame so
  // a burst of streams can't schedule a redraw each. `dead` covers the
  // frame already queued when teardown lands — unsubscribing stops new
  // notifications but not a callback that is already in flight, and
  // setData on a destroyed uPlot throws.
  let queued = false;
  let dead = false;
  const unsub = metricsStore.subscribe(() => {
    if (queued) return;
    queued = true;
    requestAnimationFrame(() => {
      queued = false;
      if (!dead) plot.setData(series());
    });
  });
  return {
    plot,
    destroy() {
      dead = true;
      unsub();
      plot.destroy();
    },
  };
}

// Build the per-metric uPlot charts for `d` into `container` and
// return them as a metric → {plot, xs, ys, scale} map. Extracted from
// the old inline showComponent chart code so the Charts fold row can
// call it lazily, on first unfold, instead of on every selection.
// Doesn't touch `liveCharts` itself or check the render generation —
// the caller (renderNode) owns both, since only it knows this
// render's `gen` and whether a newer selection has since started.
async function buildCharts(d, container, snapshotJsonP) {
  const metrics = CHARTS_BY_CATEGORY[d.category] || [];
  const charts = new Map(); // metric → { plot, xs, ys }
  const Plot = requireUplot(container);
  if (!Plot) return charts;

  // A steam boiler's pressure chart annotates the controller's
  // thermostat target in its title — no cheap reference-line idiom
  // in this hand-rolled uPlot setup, so this reads it off the
  // /api/component snapshot the caller already has in flight
  // (fetchComponentSnapshot, shared with fetchSnapshot's knob
  // read-back) rather than issuing its own fetch. `null` on any
  // failure just means no suffix, same degrade-quietly discipline as
  // the history fetches below.
  const targetP = metrics.includes("pressure_bar")
    ? snapshotJsonP.then((snap) => snap?.pressure_target_bar ?? null).catch(() => null)
    : Promise.resolve(null);

  // All metric histories fetched concurrently — inspector open
  // latency is the slowest round-trip, not the sum. Errors settle
  // into the result slot so a stale-generation return can't leak an
  // unhandled rejection.
  const slots = metrics.map((metric) => {
    const slot = document.createElement("div");
    slot.className = "chart";
    container.appendChild(slot);
    return { metric, slot };
  });
  const results = await Promise.all(
    slots.map(({ metric }) =>
      fetch(`${mgPath("history")}?id=${d.id}&metric=${metric}&window_s=300`)
        .then(async (res) => {
          if (!res.ok) throw new Error(`HTTP ${res.status}`);
          return res.json();
        })
        .then(
          (resp) => ({ resp }),
          (err) => ({ err }),
        ),
    ),
  );
  const target = await targetP;
  for (const [i, { metric, slot }] of slots.entries()) {
    const { resp, err } = results[i];
    if (err) {
      // Same discipline as renderSetpoints below: one failed metric
      // renders an "unavailable" slot instead of breaking the panel.
      slot.innerHTML = `<p class="hint">${escapeHtml(
        METRIC_TITLES[metric] || metric,
      )} unavailable: ${escapeHtml(err.message)}</p>`;
      continue;
    }
    const samples = resp.samples || [];
    const xs = samples.map(([t]) => t / 1000);
    const ys = samples.map(([, v]) => v);
    const { plot, scale } = makePlot(
      Plot,
      slot,
      metric,
      resp.quantity,
      resp.unit,
      xs,
      ys,
      metric === "pressure_bar" ? target : null,
    );
    // Stored ys are pre-scaled (already divided by scale.div) so the
    // live push path can append by dividing each new sample once.
    charts.set(metric, { plot, xs, ys: ys.map((y) => y / scale.div), scale });
  }
  return charts;
}

// One setpoint-event row, shared by the live-WS push and the REST
// backfill so the markup and escape discipline can't drift. Every
// interpolation goes through `escapeHtml` — the server currently
// only emits fixed-shape strings and a numeric `value`, but
// defense-in-depth: anything that lands in `innerHTML` is escaped.
function setpointEventLi(ts, kind, value, accepted, reason) {
  const li = document.createElement("li");
  li.className = `sp-event ${accepted ? "accepted" : "rejected"}`;
  const time = new Date(ts).toLocaleTimeString();
  const tag = escapeHtml(String(kind ?? "").replace("_", " "));
  const head = `<span class="sp-ts">${escapeHtml(time)}</span> <span class="sp-tag">${tag}</span> <span class="sp-val">${escapeHtml(String(value))}</span>`;
  const body = accepted
    ? '<span class="sp-ok">✓ accepted</span>'
    : `<span class="sp-bad">✕ ${escapeHtml(reason || "")}</span>`;
  li.innerHTML = `${head}<br/>${body}`;
  return li;
}

async function renderSetpoints(id, container) {
  const wrap = document.createElement("div");
  wrap.className = "setpoints";
  wrap.innerHTML = "<h3>Recent setpoints</h3>";
  container.appendChild(wrap);
  try {
    const res = await fetch(`${mgPath("setpoints")}?id=${id}&window_s=600`);
    const data = await res.json();
    // Always create the list element, even when empty —
    // appendSetpointEvent appends to it on incoming WS events. A
    // no-events placeholder
    // hint sits inside the list and gets dropped once the first
    // event lands.
    const list = document.createElement("ol");
    list.className = "sp-list";
    if (!data.events.length) {
      const empty = document.createElement("li");
      empty.className = "hint sp-empty";
      empty.textContent = "none in the last 10 min";
      list.appendChild(empty);
    }
    // Newest first reads better in a chronological log.
    for (const e of data.events.slice().reverse()) {
      const accepted = e.outcome.kind === "accepted";
      list.appendChild(setpointEventLi(e.ts, e.kind, e.value, accepted, e.outcome.reason));
    }
    wrap.appendChild(list);
  } catch (err) {
    wrap.insertAdjacentHTML(
      "beforeend",
      `<p class="hint">setpoints unavailable: ${escapeHtml(err.message)}</p>`,
    );
  }
}

function makePlot(Plot, container, metric, quantity, unit, xs, ys, target = null) {
  const title = METRIC_TITLES[metric] || metric;
  const scale = chooseScale(quantity, unit, ys);
  const scaledYs = ys.map((y) => y / scale.div);
  const baseTitle = scale.unit ? `${title} (${scale.unit})` : title;
  const fullTitle =
    target != null ? `${baseTitle} — target ${target} ${scale.unit || unit}` : baseTitle;
  const opts = {
    width: container.clientWidth || 280,
    height: 140,
    title: fullTitle,
    cursor: { drag: { x: false, y: false } },
    legend: { show: false },
    scales: { x: { time: true } },
    axes: [
      { stroke: "#7d848e", grid: { stroke: "#353a45", width: 0.5 } },
      // size = pixels reserved for the y-axis labels. 60 fits values
      // up to 6 chars (e.g. -32.5 kW) without truncation.
      {
        stroke: "#7d848e",
        grid: { stroke: "#353a45", width: 0.5 },
        size: 60,
      },
    ],
    series: [
      {},
      { stroke: "#79b8ff", width: 1.5, points: { show: false } },
    ],
  };
  return { plot: new Plot(opts, [xs, scaledYs], container), scale };
}
