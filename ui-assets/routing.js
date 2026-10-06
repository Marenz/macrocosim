// SPA routing + persistence: which mode / microgrid / subview is
// active, how state round-trips through localStorage + the URL
// hash, and the keyboard / button hooks that drive transitions.
// Owns mgPath / mgFetch (per-microgrid URL helpers), the density
// toggle, and the refreshTopology fetch that ferries the selected
// microgrid's topology into the canvas + chrome pulse bar.

import { dispatchesPanel, notify, setStatus } from "./app.js";
import { pulseBar } from "./chrome.js";
import { errorText, getJson } from "./http.js";
import { refitCharts, showComponent } from "./inspect.js";
import { microgridsPanel, scenariosPanel } from "./panels.js";
import { closeAllPanels } from "./side-panel.js";
import { topology } from "./topology.js";

// ─── Per-mg URL helper ─────────────────────────────────────────────────────
// `/api/mg/{selected}/{suffix}`, or null with no microgrid selected:
// callers skip the request then.
export function mgPath(suffix) {
  const id = readSelectedMg();
  return id == null ? null : `/api/mg/${id}/${suffix}`;
}

// fetch() of `mgPath(suffix)`: null, with no request made, when no
// microgrid is selected. A 404 saying the selected microgrid is not
// registered drops the selection for the list view, as a vanished
// microgrid in a fresh listing does; the response is still returned
// for the caller's own error path.
export async function mgFetch(suffix, opts) {
  const id = readSelectedMg();
  const path = mgPath(suffix);
  if (path == null) return null;
  const res = await fetch(path, opts);
  if (res.status === 404) {
    const text = await errorText(res.clone());
    if (text === `microgrid ${id} not registered` && readSelectedMg() === id) {
      dropSelection(id);
      if (document.body.dataset.mode === "microgrids") microgridsPanel.refresh();
    }
  }
  return res;
}

// The JSON body of `mgFetch(suffix, opts)`; throws with no
// microgrid selected, and with the message of a failed response.
export async function mgJson(suffix, opts) {
  const res = await mgFetch(suffix, opts);
  if (res == null) throw new Error("no microgrid selected");
  if (!res.ok) throw new Error(await errorText(res));
  return res.json();
}

// The microgrid the scenario readouts show: the selected one, else
// the lowest id of the last microgrid listing (`/api/microgrids`
// fetched while none has landed), else null. Display only: the
// selection and the route stay as they are.
export async function scenarioMgId() {
  const id = readSelectedMg();
  if (id != null) return id;
  const ids = mgFlags.size
    ? [...mgFlags.keys()]
    : (await getJson("/api/microgrids")).map((m) => m.id);
  return ids.length ? Math.min(...ids) : null;
}

// ─── Per-microgrid file flags ──────────────────────────────────────────────
// The `managed` / `unsaved` / `source` fields off the last
// /api/microgrids listing, keyed by id. microgridsPanel publishes on
// every refresh; the inspector, the editor and the canvas read it to
// decide whether a structural affordance is offered at all — only a
// managed file can have its structure rewritten, so on an unmanaged
// one the edit would run live and then be lost on the next reload.
let mgFlags = new Map();

export function publishMgFlags(rows) {
  mgFlags = new Map(rows.map((m) => [m.id, m]));
}

export function currentMgEntry() {
  const id = readSelectedMg();
  return id == null ? null : mgFlags.get(id) || null;
}

// Permissive before the first listing lands (or for an id the
// listing doesn't carry): greying the whole editor out on a
// not-yet-known microgrid would claim "unmanaged file" about a file
// we have not heard of. The server is the real gate — an edit to an
// unmanaged file still evaluates, it just doesn't persist.
export function structureEditable() {
  const entry = currentMgEntry();
  return entry == null || entry.managed === true;
}

export const READ_ONLY_TITLE =
  "unmanaged file — structure is read-only (Adopt to edit)";

// ─── Density toggle ────────────────────────────────────────────────────────
// CSS-only mode that shrinks pane + pulse-bar paddings and fonts.
// For power users on long soak runs who want more rows on screen at
// once. Default = normal (the 32" 4K target keeps the comfortable
// layout the landing one). Preference persists in localStorage so a
// refresh keeps you put.
const DENSITY_KEY = "macrocosim-density";

function applyDensity(mode) {
  if (mode === "compact") {
    document.body.dataset.density = "compact";
  } else {
    delete document.body.dataset.density;
  }
}

export function setupDensityToggle() {
  const chip = document.getElementById("density-toggle");
  if (chip) {
    const stored = localStorage.getItem(DENSITY_KEY);
    applyDensity(stored);
    chip.classList.toggle("active", stored === "compact");
    chip.addEventListener("click", () => {
      const next =
        document.body.dataset.density === "compact" ? "normal" : "compact";
      localStorage.setItem(DENSITY_KEY, next);
      applyDensity(next);
      chip.classList.toggle("active", next === "compact");
    });
  }
}

// ─── Route state keys + read helpers ───────────────────────────────────────
const MODE_KEY = "macrocosim-mode";
const MG_SELECTED_KEY = "macrocosim-selected-mg";
const MG_SUBVIEW_KEY = "macrocosim-mg-subview";
const VALID_MODES = new Set(["microgrids", "scenarios"]);
const VALID_SUBVIEWS = new Set(["topology", "dispatches"]);

export function readSelectedMg() {
  const raw = localStorage.getItem(MG_SELECTED_KEY);
  if (raw == null || raw === "" || raw === "null") return null;
  const n = Number(raw);
  return Number.isFinite(n) ? n : null;
}

export function readSubview() {
  const v = localStorage.getItem(MG_SUBVIEW_KEY);
  return VALID_SUBVIEWS.has(v) ? v : "topology";
}

// The subview name, but only when the mode and mgView flags say the
// subview pane is actually on screen — null otherwise. Keyboard
// shortcuts and WS refreshes must gate on this, never on
// data-subview alone: the subview persists across mode switches and
// would leave canvas shortcuts armed on Scenarios or the microgrid
// list, firing against a hidden canvas's selection.
export function visibleSubview() {
  const { mode, mgView, subview } = document.body.dataset;
  return mode === "microgrids" && mgView === "selected" ? subview : null;
}

// ─── URL routing ────────────────────────────────────────────────────────────
function currentRoute() {
  return {
    mode: localStorage.getItem(MODE_KEY) || "microgrids",
    selectedMg: readSelectedMg(),
    subview: readSubview(),
  };
}

function routeToHash({ mode, selectedMg, subview }) {
  if (mode === "scenarios") return "#scenarios";
  if (selectedMg == null) return "#microgrids";
  return `#microgrids/${selectedMg}/${subview}`;
}

function parseHash(hash) {
  // Empty / bare `#` → fall through to localStorage. Returning a
  // default here would overwrite the user's last-seen state every
  // time they refresh `/`. Explicit `#microgrids` (no trailing
  // segments) still resets to the list view, matching what
  // `routeToHash` emits when selectedMg is null.
  if (!hash || hash === "#") return null;
  if (hash === "#microgrids") {
    return { mode: "microgrids", selectedMg: null, subview: "topology" };
  }
  if (hash === "#scenarios") {
    return { mode: "scenarios", selectedMg: null, subview: "topology" };
  }
  const m = /^#microgrids\/(\d+)(?:\/(dashboard|topology|formulas|dispatches))?$/.exec(hash);
  // `formulas` was its own subview before the formula explorer became
  // a floating panel, and `dashboard` was retired for the metrics
  // panel; the regex still matches both so old bookmarks land on
  // Topology instead of a dead route.
  if (m && (m[2] === "formulas" || m[2] === "dashboard")) m[2] = "topology";
  if (m) {
    return {
      mode: "microgrids",
      selectedMg: Number(m[1]),
      subview: m[2] || "topology",
    };
  }
  return null;
}

function writeRouteToStorage({ mode, selectedMg, subview }) {
  if (mode) localStorage.setItem(MODE_KEY, mode);
  if (selectedMg != null) {
    localStorage.setItem(MG_SELECTED_KEY, String(selectedMg));
  } else if (selectedMg === null) {
    localStorage.removeItem(MG_SELECTED_KEY);
  }
  if (subview) localStorage.setItem(MG_SUBVIEW_KEY, subview);
}

export function navigateTo(next) {
  const cur = currentRoute();
  const merged = { ...cur, ...next };
  writeRouteToStorage(merged);
  const hash = routeToHash(merged);
  if (location.hash !== hash) {
    history.pushState(merged, "", hash);
  }
  applyMode(merged.mode);
}

function setupRouterPopstate() {
  window.addEventListener("popstate", () => {
    const parsed = parseHash(location.hash);
    if (!parsed) return;
    writeRouteToStorage(parsed);
    applyMode(parsed.mode);
    // Back/forward can land on a different microgrid; refetch so
    // the canvas rebuilds for it (the click path does this via
    // selectMicrogrid).
    if (parsed.selectedMg != null) refreshTopology();
  });
}

function applyInitialRoute() {
  const parsed = parseHash(location.hash);
  if (parsed) {
    writeRouteToStorage(parsed);
  }
  const cur = currentRoute();
  // Replace rather than push so the back button doesn't pop into a
  // synthetic empty entry — the user lands on the page; the first
  // back press should leave the SPA, not bounce them inside it.
  history.replaceState(cur, "", routeToHash(cur));
  applyMode(cur.mode);
}

// The (mode, mg, subview) triple the last applyRoute() settled on, so a
// re-entrant call for the SAME route can be told apart from a real
// navigation. null until the first call, which therefore always counts
// as a change.
let lastAppliedRoute = null;

// Applies the stored route, then refreshes the mode's panel.
function applyMode(mode) {
  mode = applyRoute(mode);
  if (mode === "microgrids") microgridsPanel.refresh();
  if (mode === "scenarios") scenariosPanel.refresh();
}

// Puts the stored (mode, mg, subview) on the DOM — body flags, toggle
// buttons, panel dismissal, canvas nudges — without touching any
// panel's data. Returns the mode it applied.
function applyRoute(mode) {
  if (!VALID_MODES.has(mode)) mode = "microgrids";
  const selected = readSelectedMg();
  const subview = readSubview();
  const routeChanged =
    lastAppliedRoute === null ||
    lastAppliedRoute.mode !== mode ||
    lastAppliedRoute.selected !== selected ||
    lastAppliedRoute.subview !== subview;
  lastAppliedRoute = { mode, selected, subview };
  document.body.dataset.mode = mode;
  document.body.dataset.mgView = selected == null ? "list" : "selected";
  document.body.dataset.subview = subview;
  // Switching tab/mode dismisses the floating panels — the inspector's
  // selection no longer applies, and the add panel is topology-only.
  // The canvases keep their selection, so tell them the inspector is
  // gone: without resetNotify, re-clicking the still-selected node
  // after switching back would dedup and never reopen the inspector.
  //
  // Only on a REAL navigation, though: navigateTo() runs applyMode
  // unconditionally, so a no-op route write — jumpToTopology() asking
  // for the topology subview while already on it, say — would otherwise
  // dismiss the very panel the user clicked from (the formula
  // explorer's #N links).
  if (routeChanged) {
    closeAllPanels();
    topology.resetNotify();
    document.getElementById("add-panel").classList.remove("open");
  }
  for (const btn of document.querySelectorAll("#mode-toggle .mode-btn")) {
    btn.classList.toggle("active", btn.dataset.mode === mode);
  }
  for (const btn of document.querySelectorAll("#mg-subtoggle .mode-btn")) {
    btn.classList.toggle("active", btn.dataset.subview === subview);
  }
  // vis-network needs a redraw nudge when its container goes from
  // display:none back to visible — the canvas was sized to 0×0 while
  // hidden. Same shape the splitter resize handler uses. Defer the
  // fit one animation-frame so the just-flipped `data-subview` has
  // settled the CSS visibility before vis-network measures.
  if (mode === "microgrids" && selected != null && subview === "topology") {
    refitCharts();
    requestAnimationFrame(() => {
      topology.fit();
      // Arriving at a topology view hands the keyboard to the canvas,
      // so a paste or select-all works without a click first.
      if (routeChanged) topology.focusCanvas();
    });
    topology.flushLive();
  }
  if (mode === "microgrids" && selected != null && subview === "dispatches") {
    dispatchesPanel.render(selected);
  }
  return mode;
}

// Jump to the topology subview within the current mode and select
// `id` on the canvas. Used by the formula explorer's #N refs.
// Pushes a history entry so the back button returns the user to
// where they clicked from.
export function jumpToTopology(id) {
  navigateTo({ subview: "topology" });
  // An explicit jump always notifies, even when it lands on the node
  // that is already selected. notifySelection() dedups on the
  // selection set, so without this a repeat jump to the same id would
  // reach none of the selection handlers — the panels they feed can
  // have been closed behind the canvas's back in the meantime (Esc on
  // the inspector while its node stays selected). The inspector itself
  // is opened directly below, so this is about the other handlers
  // staying in step, and about the invariant holding if that direct
  // call ever moves.
  topology.resetNotify();
  topology.select([id]);
  topology.focusCanvas();
  const c = topology.get(id);
  if (c) showComponent(c);
  // Center the node in the part of the canvas the inspector isn't
  // covering — a fit alone can leave the jumped-to node hidden
  // behind the panel.
  const inspector = document.getElementById("inspector");
  topology.reveal(id, inspector ? inspector.getBoundingClientRect().width : 0);
}

// The selection must name a microgrid the server has. The list panel
// calls this with every list fresh from the server; a remembered id
// the server no longer knows — a fresh state dir, a file unloaded
// from the REPL — is dropped for the list view, saying why. A
// correction rather than a navigation (replaceState, not push), so
// Back does not walk into the dead route. Applies the route only:
// the caller holds the fresh list and renders it itself.
export function reconcileSelection(rows) {
  const id = readSelectedMg();
  if (id == null || rows.some((m) => m.id === id)) return;
  dropSelection(id);
}

// Drops the selected microgrid `id`, which the server no longer
// has, for the list view: replaces the route, says why, and applies
// the route only.
function dropSelection(id) {
  const next = { ...currentRoute(), selectedMg: null };
  writeRouteToStorage(next);
  history.replaceState(next, "", routeToHash(next));
  // A topology fetch for the dead id may already have stamped its
  // 404 into the status pill; the list view has nothing to say there.
  setStatus("", "");
  notify(`Microgrid #${id} is no longer loaded`);
  applyRoute(next.mode);
  renderReplMgChip();
}

export function selectMicrogrid(id) {
  navigateTo({ mode: "microgrids", selectedMg: id });
  renderReplMgChip();
  // Refetch the per-mg topology so the canvas + the empty-hint
  // overlay (D5) reflect the newly-selected microgrid. Without
  // this the canvas keeps showing the previous microgrid's
  // components until a WS topology_changed event arrives — which
  // never happens just because the selection changed client-side.
  if (id != null) refreshTopology();
}

// REPL chip — surfaces which microgrid the REPL form's POSTs
// route to: "→ {name}" when a microgrid is selected, "→ enterprise"
// (the whole-site /api/eval) otherwise. Clicking jumps to the
// Microgrids list so the operator can pick a different one.
export function renderReplMgChip() {
  const chip = document.getElementById("repl-mg-chip");
  if (!chip) return;
  const id = readSelectedMg();
  if (id == null) {
    chip.textContent = "→ enterprise";
    chip.classList.add("muted");
    return;
  }
  chip.classList.remove("muted");
  // Pull the name from the microgridsPanel's cache if available;
  // fall back to "#id" so the chip never sits empty.
  const cached = (window.__mgPanelCache || []).find((m) => m.id === id);
  chip.textContent = `→ ${cached ? cached.name || `#${id}` : `#${id}`}`;
}

export function setupReplMgChip() {
  const chip = document.getElementById("repl-mg-chip");
  if (!chip) return;
  chip.addEventListener("click", () => {
    navigateTo({ mode: "microgrids", selectedMg: null });
    renderReplMgChip();
  });
  renderReplMgChip();
}

export function setupModeToggle() {
  for (const btn of document.querySelectorAll("#mode-toggle .mode-btn")) {
    btn.addEventListener("click", () => {
      const mode = btn.dataset.mode;
      // Microgrids button returns the user to the list. Picking a
      // microgrid (D2 cards) re-enters the selected view.
      navigateTo({
        mode,
        selectedMg: mode === "microgrids" ? null : currentRoute().selectedMg,
      });
    });
  }
  for (const btn of document.querySelectorAll("#mg-subtoggle .mode-btn")) {
    btn.addEventListener("click", () => {
      const sv = btn.dataset.subview;
      if (!VALID_SUBVIEWS.has(sv)) return;
      navigateTo({ subview: sv });
    });
  }
  const backBtn = document.getElementById("mg-back");
  if (backBtn) backBtn.addEventListener("click", () => selectMicrogrid(null));
  applyInitialRoute();
  setupRouterPopstate();
  // Keyboard chord — 1 → Microgrids list, 2 → Scenarios. Skip
  // when a text input has focus so digits typed into the REPL /
  // search boxes don't trigger a mode flip.
  document.addEventListener("keydown", (ev) => {
    if (ev.ctrlKey || ev.metaKey || ev.altKey) return;
    const t = ev.target;
    const tag = t?.tagName;
    if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || t?.isContentEditable)
      return;
    let mode = null;
    if (ev.key === "1") mode = "microgrids";
    else if (ev.key === "2") mode = "scenarios";
    if (!mode) return;
    ev.preventDefault();
    navigateTo({
      mode,
      selectedMg: mode === "microgrids" ? null : currentRoute().selectedMg,
    });
  });
}

export async function refreshTopology() {
  // Remember the microgrid the fetch was for: the user can switch mg
  // before it lands, and a response for a no-longer-selected site
  // must not repaint this one's panels.
  const mg = readSelectedMg();
  try {
    const res = await mgFetch("topology");
    if (res == null) return;
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const data = await res.json();
    // The user can switch microgrid while the fetch is in flight
    // (selectMicrogrid fires a fresh one). A response for a mg that
    // is no longer selected must be dropped whole: applying it would
    // repaint every panel with the old site's data.
    if (readSelectedMg() !== mg) return;
    topology.apply(data);
    // Pulse bar's health counters + graph pill read from the
    // same topology fetch — one round-trip carries both
    // signals + a hot-reload's WS topology_changed nudge
    // already drives a refresh.
    pulseBar.applyTopology(data.components || [], data.graph_status);
  } catch (err) {
    // Same rule as a late success: an error for a microgrid that is
    // no longer selected (switched away, or dropped as vanished)
    // belongs to a view that is gone.
    if (readSelectedMg() !== mg) return;
    setStatus(`error: ${err.message}`, "error");
  }
}
