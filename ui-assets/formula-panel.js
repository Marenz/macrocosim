// The formula explorer panel: the graph crate's generated formulas,
// rendered live over the Topology canvas. Fetches
// /api/mg/{id}/formula for the active metric and renders the parsed
// formula with subtracted terms tinted red; hovering a sub-expression
// highlights its components on the canvas, clicking a #N jumps to it.
// Structural editing stays live underneath — app.js re-fetches on
// topology WS events while the panel is open, and the "limit to the
// selected components" toggle re-fetches on selection changes.

import { jumpToTopology, notify } from "./app.js";
import { formulaToHtml, formulaToText, parseFormula } from "./formula-ast.js";
import { errorText } from "./http.js";
import { mgFetch, readSelectedMg } from "./routing.js";
import { isPanelOpen, makeSidePanelToggle } from "./side-panel.js";
import { topology } from "./topology.js";

// Metrics that accept an id set (the "limit to selection" toggle)
// vs. the ones that need exactly one id.
const GROUP_ID_METRICS = new Set([
  "battery",
  "pv",
  "chp",
  "wind_turbine",
  "ev_charger",
  "steam_boiler",
  "battery_ac_coalesce",
  "pv_ac_coalesce",
]);
const SINGLE_ID_METRICS = new Set(["component", "component_ac_coalesce"]);

// Engine-option checkboxes → query params on the formula request.
const ENGINE_OPTIONS = [
  ["cfg-prefer-meters", "prefer_meters"],
  ["cfg-phantom", "phantom_loads"],
  ["cfg-no-fallback", "no_fallback"],
  ["cfg-allow-unconnected", "allow_unconnected"],
  ["cfg-allow-validation-failures", "allow_validation_failures"],
  ["cfg-unspecified-inverters", "allow_unspecified_inverters"],
];

let metric = "grid";
let formulaText = "";
let requestSeq = 0;
let lastSelectionKey = "";
let hovered = null;

const PANEL = "formula-btn";
const useSelectionEl = () => document.getElementById("use-selection");

const MARKUP = `
  <section class="card">
    <h2>Metric</h2>
    <div id="metric-buttons">
      <div class="field">
        <span class="field-label">power</span>
        <button type="button" class="pill metric-btn" data-metric="grid">grid</button>
        <button type="button" class="pill metric-btn" data-metric="consumer">consumer</button>
        <button type="button" class="pill metric-btn" data-metric="producer">producer</button>
        <button type="button" class="pill metric-btn" data-metric="battery">battery</button>
        <button type="button" class="pill metric-btn" data-metric="pv">PV</button>
        <button type="button" class="pill metric-btn" data-metric="chp">CHP</button>
        <button type="button" class="pill metric-btn" data-metric="wind_turbine">wind</button>
        <button type="button" class="pill metric-btn" data-metric="ev_charger">EV</button>
        <button type="button" class="pill metric-btn" data-metric="steam_boiler">boiler</button>
      </div>
      <div class="field">
        <span class="field-label">voltage / freq</span>
        <button type="button" class="pill metric-btn" data-metric="grid_coalesce">grid</button>
        <button type="button" class="pill metric-btn" data-metric="battery_ac_coalesce">battery</button>
        <button type="button" class="pill metric-btn" data-metric="pv_ac_coalesce">PV</button>
      </div>
      <div class="field">
        <span class="field-label">one component</span>
        <button type="button" class="pill metric-btn" data-metric="component">reading</button>
        <button type="button" class="pill metric-btn" data-metric="component_ac_coalesce">voltage / freq</button>
      </div>
    </div>
    <label class="check"><input type="checkbox" id="use-selection" />
      limit to the selected components</label>
  </section>
  <details class="card" id="config-panel">
    <summary><h2>Engine options</h2><span id="config-count" class="muted"></span></summary>
    <div class="field">
      <span class="field-label">formulas</span>
      <label class="check"><input type="checkbox" id="cfg-prefer-meters" />
        prefer meters over component readings</label>
      <label class="check"><input type="checkbox" id="cfg-phantom" />
        include phantom loads in consumer formula</label>
      <label class="check"><input type="checkbox" id="cfg-no-fallback" />
        disable fallback components</label>
    </div>
    <div class="field">
      <span class="field-label">graph building</span>
      <label class="check"><input type="checkbox" id="cfg-allow-unconnected" />
        allow unconnected components</label>
      <label class="check"><input type="checkbox" id="cfg-allow-validation-failures" />
        allow component validation failures</label>
      <label class="check"><input type="checkbox" id="cfg-unspecified-inverters" />
        treat unspecified inverters as battery inverters</label>
    </div>
  </details>
  <section class="card">
    <h2>Formula</h2>
    <p id="formula-error" class="graph-error" hidden></p>
    <pre id="formula-view" class="formula-tree"></pre>
    <div class="field">
      <button type="button" class="tool-btn" id="copy-formula"
        title="Copy the plain formula string">copy</button>
    </div>
    <p class="hint">Hover a part of the formula to highlight its
      components on the canvas — subtracted parts show red. Click a
      <code>#N</code> to jump to that component.</p>
  </section>`;

export const setupFormulaToggle = () =>
  makeSidePanelToggle(PANEL, renderPanel, teardown);

function teardown() {
  clearCrossHighlight();
  hovered = null;
  formulaText = "";
  lastSelectionKey = "";
  // Reopening the panel starts back at the grid metric, like the
  // checkboxes it reopens alongside.
  metric = "grid";
}

function renderPanel(contentEl) {
  contentEl.innerHTML = MARKUP;
  wire(contentEl);
  refreshFormula();
}

function wire(contentEl) {
  // Metric picker.
  const bar = contentEl.querySelector("#metric-buttons");
  // The lit button follows the module-level `metric`, so a re-render
  // can never disagree with the metric the panel actually fetches.
  for (const b of bar.querySelectorAll(".metric-btn")) {
    b.classList.toggle("active", b.dataset.metric === metric);
  }
  bar.addEventListener("click", (ev) => {
    const btn = ev.target.closest(".metric-btn");
    if (!btn) return;
    metric = btn.dataset.metric;
    for (const b of bar.querySelectorAll(".metric-btn")) {
      b.classList.toggle("active", b === btn);
    }
    refreshFormula();
  });
  useSelectionEl().addEventListener("change", refreshFormula);
  // Engine options are client-side request parameters, so a change
  // only needs a re-fetch — nothing to store on the server.
  for (const box of contentEl.querySelectorAll("#config-panel input[type=checkbox]")) {
    box.addEventListener("change", () => {
      updateConfigCount();
      refreshFormula();
    });
  }
  contentEl.querySelector("#copy-formula").addEventListener("click", async () => {
    if (!formulaText) {
      notify("No formula to copy.");
      return;
    }
    try {
      await navigator.clipboard.writeText(formulaText);
      notify("Copied the formula.", "success");
    } catch (e) {
      notify(`Copy failed: ${e.message}`);
    }
  });
  // Hover a sub-expression → cross-highlight its components on the
  // canvas; the ids come straight from the rendered DOM.
  const view = contentEl.querySelector("#formula-view");
  view.addEventListener("mouseover", (ev) => {
    const span = ev.target.closest(".formula-node");
    if (span === hovered) return;
    hovered?.classList.remove("expr-hl");
    hovered = span;
    if (!span) {
      clearCrossHighlight();
      return;
    }
    span.classList.add("expr-hl");
    crossHighlight(idsIn(span));
  });
  view.addEventListener("mouseleave", () => {
    hovered?.classList.remove("expr-hl");
    hovered = null;
    clearCrossHighlight();
  });
  view.addEventListener("click", (ev) => {
    const ref = ev.target.closest(".formula-ref");
    if (!ref) return;
    // End the hover highlight BEFORE jumping. topology.highlight()
    // borrows the vis selection and stashes the user's own; the
    // mouseleave that the opening inspector triggers (the dock reflows
    // the formula out from under the cursor) would then restore that
    // stash over the jump's freshly-selected node. Unhighlighting here
    // drops the stash, so the jump's selection is the one that sticks
    // and the later mouseleave is a no-op.
    hovered?.classList.remove("expr-hl");
    hovered = null;
    clearCrossHighlight();
    jumpToTopology(Number(ref.dataset.id));
  });
}

// The options card is collapsed by default, so a badge on its summary
// keeps active options visible ("2 on"); empty when nothing is set.
function updateConfigCount() {
  const boxes = document.querySelectorAll("#config-panel input[type=checkbox]");
  const on = [...boxes].filter((box) => box.checked).length;
  document.getElementById("config-count").textContent = on ? `${on} on` : "";
}

function idsIn(span) {
  const refs = span.matches(".formula-ref")
    ? [span]
    : [...span.querySelectorAll(".formula-ref")];
  return refs.map((r) => Number(r.dataset.id));
}

// Highlight `ids` on the canvas: subtracted occurrences red, added
// ones blue — per span, read from the parity wrappers the renderer
// emits at every sign flip. The NEAREST wrapper wins: inside
// `#2 - (#3 - #4)` the renderer nests .formula-unsubtracted around
// #4 within #3's .formula-subtracted, and #4 is genuinely added. A
// canvas node turns red only when every covered occurrence of it is
// subtracted.
function crossHighlight(ids) {
  const set = new Set(ids);
  const subIds = new Set();
  const addIds = new Set();
  for (const ref of document.querySelectorAll("#formula-view .formula-ref")) {
    const id = Number(ref.dataset.id);
    const covered = set.has(id);
    const flip = ref.closest(".formula-subtracted, .formula-unsubtracted");
    const inSub = covered && (flip?.classList.contains("formula-subtracted") ?? false);
    ref.classList.toggle("hl", covered);
    ref.classList.toggle("hl-sub", inSub);
    if (covered) (inSub ? subIds : addIds).add(id);
  }
  const subtracted = [...subIds].filter((id) => !addIds.has(id));
  topology.highlight(ids, subtracted);
}

function clearCrossHighlight() {
  topology.unhighlight();
  for (const ref of document.querySelectorAll("#formula-view .formula-ref")) {
    ref.classList.remove("hl", "hl-sub");
  }
}

// Whether a settled request may still paint. Two ways it may not:
// a newer request superseded this one, or the panel closed while the
// fetch was in flight — closePanel() empties the panel's contentEl, so
// every getElementById below would be null and throw. Both are
// ordinary (pick a metric, then close the panel or switch tab before
// the response lands), so bail quietly.
function stale(seq) {
  return seq !== requestSeq || !isPanelOpen(PANEL);
}

// Show `message` in the error banner and clear the formula pane. Only
// ever called from behind a stale() guard, so the elements exist.
function showFormulaError(message) {
  const errorEl = document.getElementById("formula-error");
  errorEl.textContent = message;
  errorEl.hidden = false;
  document.getElementById("formula-view").innerHTML = "";
  formulaText = "";
}

// Canvas selection hook, called from app.js's selection handler. With
// the "limit to selection" toggle on (or a single-component metric),
// the formula follows the selection live.
export function formulaSelectionChanged() {
  if (!isPanelOpen(PANEL)) return;
  const key = topology.selectedIds().join(",");
  if (key === lastSelectionKey) return;
  lastSelectionKey = key;
  if (
    SINGLE_ID_METRICS.has(metric) ||
    (GROUP_ID_METRICS.has(metric) && useSelectionEl().checked)
  ) {
    refreshFormula();
  }
}

export async function refreshFormula() {
  if (!isPanelOpen(PANEL)) return;
  // A debounced call can land after the user left the microgrid.
  if (readSelectedMg() == null) return;

  const params = new URLSearchParams({ metric });
  const selection = topology.selectedIds();
  // Record the selection this request is built from, synchronously:
  // formulaSelectionChanged dedupes against it, so it has to name the
  // selection the last-issued fetch actually used. Left behind, a
  // panel opened over a live selection would fetch that selection's
  // ids while the key still said "none", and the deselect that
  // followed would dedupe away as a no-op.
  lastSelectionKey = selection.join(",");
  if (
    SINGLE_ID_METRICS.has(metric) ||
    (GROUP_ID_METRICS.has(metric) && useSelectionEl().checked && selection.length)
  ) {
    params.set("component_ids", selection.join(","));
  }
  for (const [id, param] of ENGINE_OPTIONS) {
    if (document.getElementById(id).checked) params.set(param, "true");
  }

  const seq = ++requestSeq;
  let data;
  try {
    const res = await mgFetch(`formula?${params}`);
    if (res == null) return;
    // A failure carries the shared `error` body: a 400 is the
    // formula's own error, other statuses are the server refusing
    // the request.
    if (!res.ok) {
      const text = await errorText(res);
      if (stale(seq)) return;
      showFormulaError(res.status === 400 ? text : `${res.status}: ${text}`);
      return;
    }
    data = await res.json();
  } catch (e) {
    if (stale(seq)) return;
    showFormulaError(`Formula request failed: ${e.message}`);
    return;
  }
  if (stale(seq)) return;
  document.getElementById("formula-error").hidden = true;
  formulaText = data.formula;
  const ast = parseFormula(data.formula);
  document.getElementById("formula-view").innerHTML = formulaToHtml(ast);
  // The parsed AST must render back to the exact server string; if
  // the grammars drift, surface it loudly during development.
  const roundTrip = formulaToText(ast);
  if (roundTrip !== data.formula) {
    console.error("formula grammar drift:", { server: data.formula, client: roundTrip });
  }
}
