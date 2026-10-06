// The metrics panel: three foldable cards — Power, Reactive power,
// Frequency — each one combined uPlot chart over the loopback's
// aggregate streams plus a chip row that is legend, live readout,
// and series toggle in one. It took over from the retired Dashboard
// subview's tiles; the store (metrics-store.js) owns the data, this
// module owns the DOM. Series colors follow the category palette so
// chart lines mean what the canvas already means.

import { requireUplot, uplot } from "./chart-lib.js";
import { fmtValue, latestSecond, metricsStore, pfText, pfValue } from "./metrics-store.js";
import { isPanelOpen, makeSidePanelToggle } from "./side-panel.js";

const PANEL = "metrics-btn";
const WINDOWS = [
  { key: "1m", secs: 60 },
  { key: "5m", secs: 300 },
  { key: "10m", secs: 600 },
];

const CARDS = [
  {
    key: "power",
    title: "Power",
    headline: "grid_power",
    defaultOpen: true,
    series: [
      { stream: "grid_power", label: "grid", color: "--cat-grid" },
      {
        stream: "battery_pool_power",
        label: "battery",
        color: "--cat-battery",
        band: ["battery_pool_bounds_lower", "battery_pool_bounds_upper"],
      },
      { stream: "pv_power", label: "pv", color: "--cat-inverter-solar" },
      {
        stream: "steam_boiler_pool_power",
        label: "steam boiler",
        color: "--cat-steam-boiler",
        band: ["steam_boiler_pool_bounds_lower", "steam_boiler_pool_bounds_upper"],
      },
      { stream: "consumer_power", label: "consumer", color: "--flow-import" },
      { stream: "producer_power", label: "producer", color: "--flow-export" },
    ],
  },
  {
    key: "reactive",
    title: "Reactive power",
    headline: "grid_reactive_power",
    defaultOpen: false,
    pfOverlay: true,
    series: [
      { stream: "grid_reactive_power", label: "grid", color: "--cat-grid", p: "grid_power" },
      { stream: "pv_reactive_power", label: "pv", color: "--cat-inverter-solar", p: "pv_power" },
      {
        stream: "battery_reactive_power",
        label: "battery",
        color: "--cat-battery",
        p: "battery_pool_power",
      },
    ],
  },
  {
    key: "frequency",
    title: "Frequency",
    headline: "grid_frequency",
    defaultOpen: false,
    series: [{ stream: "grid_frequency", label: "frequency", color: "--accent" }],
  },
];

// ── persisted knobs (same try/catch discipline as the inspector) ──
function loadKey(key, fallback) {
  try {
    const v = localStorage.getItem(key);
    return v == null ? fallback : v;
  } catch (_) {
    return fallback;
  }
}
function saveKey(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch (_) {
    // Storage unavailable — the choice just doesn't stick.
  }
}
const cardOpen = (c) => loadKey(`mc-metrics-card-${c.key}`, c.defaultOpen ? "1" : "0") === "1";
const seriesOn = (s) => loadKey(`mc-metrics-series-${s.stream}`, "1") === "1";
const pfOn = () => loadKey("mc-metrics-pf", "0") === "1";
const windowSecs = () => {
  const k = loadKey("mc-metrics-window", "5m");
  return (WINDOWS.find((w) => w.key === k) ?? WINDOWS[1]).secs;
};

const cssColor = (v) => getComputedStyle(document.documentElement).getPropertyValue(v).trim();

// One live uPlot per unfolded card; rebuilt (not re-dataed) on any
// config change — window, series toggle, PF overlay, unfold — and
// destroyed on fold/close. plots: card key → { plot, unit, div,
// activeKeys, bandKeys, withPf }: the axis-label, y-scale and
// data-shape decisions frozen at build time. repaint() re-derives the
// first three to notice when they have gone stale, and feeds the last
// two back so the data it hands setData() keeps the shape the plot was
// built with.
let plots = new Map();
// Charts size themselves to their slot when built; this re-sizes
// them when the slot changes width afterwards — the card docked into
// a strip, a strip splitter dragged, the card's width set by the
// shell. One observer on the content root covers every card.
let sizeObserver = null;
let unsubscribe = null;
let repaintQueued = false;

function destroyPlots() {
  for (const { plot } of plots.values()) plot.destroy();
  plots = new Map();
}

// Scale choice across every visible series of a card so they share
// one y-axis honestly (the inspector's chooseScale, multi-series).
// Hysteresis, because changing the divisor here costs a full chart
// rebuild: step UP the moment the max reaches a threshold, but step
// back DOWN only once it has fallen a clear 10% below the threshold
// that earned the current divisor. A site idling either side of
// 1 kW (or 1 MW) would otherwise flip W ↔ kW between ticks and
// rebuild the card on every single repaint.
function chooseDiv(values, currentDiv = 1) {
  let max = 0;
  for (const v of values) {
    const a = Math.abs(v);
    if (a > max) max = a;
  }
  const up = max >= 1e6 ? 1e6 : max >= 1e3 ? 1e3 : 1;
  const div = currentDiv > up && max >= 0.9 * currentDiv ? currentDiv : up;
  return { div, prefix: div === 1e6 ? "M" : div === 1e3 ? "k" : "" };
}

// The spec's dashed y=0 line: the import/export divide, drawn only
// where it means something. Guarded twice — the "y" scale only, so
// the PF overlay's right-hand 0.85–1.02 axis never gets one, and
// only when the window's range actually straddles zero, so a chart
// living wholly above it (frequency, a site that never exports)
// doesn't grow a line welded to its floor. uPlot hands canvas hooks
// a device-pixel context (valToPos's third argument asks for the
// same space), so the width and dash lengths scale by the canvas's
// own pixel ratio or they'd be hairlines on a HiDPI screen.
function drawZeroLine(u) {
  const { min, max } = u.scales.y;
  if (min == null || max == null || min > 0 || max < 0) return;
  const dpr = u.ctx.canvas.width / (u.width || 1);
  const { left, top, width, height } = u.bbox;
  // Snap the stroke's centre so its edges land on device-pixel
  // boundaries: an unsnapped 1px line straddles two rows and
  // antialiases into a grey smear half the intended contrast.
  const y = Math.round(u.valToPos(0, "y", true) - dpr / 2) + dpr / 2;
  const ctx = u.ctx;
  ctx.save();
  // Clip to the plot area: a rounded zero on a range whose edge sits
  // a fraction of a pixel away would otherwise bleed into the axis.
  ctx.beginPath();
  ctx.rect(left, top, width, height);
  ctx.clip();
  ctx.strokeStyle = "#3d4450";
  ctx.lineWidth = dpr;
  ctx.setLineDash([4 * dpr, 4 * dpr]);
  ctx.beginPath();
  ctx.moveTo(left, y);
  ctx.lineTo(left + width, y);
  ctx.stroke();
  ctx.restore();
}

// Every stream a card READS: its visible traces, the envelope bounds
// behind the battery trace, and — while the PF overlay is on — the
// active-power partner each reactive trace derives its PF against.
// They all have to share one time anchor, so they are gathered here
// once rather than reached for one call site at a time.
function cardStreams(card, active) {
  const out = new Set();
  const withPf = card.pfOverlay === true && pfOn();
  for (const s of active) {
    out.add(s.stream);
    if (s.band) for (const b of s.band) out.add(b);
    if (withPf && s.p) out.add(s.p);
  }
  return [...out];
}

// The streams allowed to STEER the anchor: the card's active traces,
// and only those. Band bounds and PF `p` partners are annotations —
// they are still read at whatever anchor the traces settle on, like
// every other column, they just don't get a vote in where the window
// ends. The bounds forwarder is change-only, so on an idle site
// `battery_pool_bounds_*` can sit unpublished for the full 15 min ring
// while the traces keep sampling; letting it into the fold put the
// anchor on a dead stream and blanked every live trace on the card.
// A TRACE that stalls behind its siblings is a different thing and
// stays in: it should read as trailing gaps under the live edge,
// which is exactly what the shared anchor gives it.
const anchorStreams = (active) => active.map((s) => s.stream);

// The identity of a card's visible series set, for pinning it on the
// plots entry: same streams in the same order is the same data shape.
const activeKeysOf = (active) => anchorStreams(active).join();

// One read of the store per card per tick: the build-time decisions
// that outlive the build — the axis unit (the headline stream's, so
// "" until the first sample lands) and the shared y divisor over
// everything currently visible — together with the uPlot data array
// those decisions scale. A panel opened before the loopback's first
// samples bakes in unit "" and div 1, and only a rebuild can relabel
// an axis or rescale a trace, so repaint() re-derives both from this
// same frame and compares before re-dataing with its `data`.
//
// Every column is read at ONE anchor second — the newest second any
// of the card's active TRACES holds (see anchorStreams; the band and
// PF streams are read at it, they don't set it) — so column k of the
// traces, of the band edges and of the PF overlay all mean the same
// second. Reading each stream at its own newest second is what let a
// stalled stream slide out from under its live siblings. `data` is
// null when there is nothing to draw yet (no trace has a sample);
// `shape` pins the band/PF layout to what the live plot was built
// with, and deriving it (null) is the build-time path.
function cardFrame(card, secs, currentDiv = 1, shape = null) {
  const active = card.series.filter(seriesOn);
  const streams = cardStreams(card, active);
  const end = latestSecond(anchorStreams(active));
  const unit = metricsStore.latest(card.headline)?.unit ?? "";
  if (!active.length || end === null) return { active, unit, div: 1, data: null };
  const raw = new Map();
  for (const stream of streams) raw.set(stream, metricsStore.series(stream, secs, end));
  // Whichever trace fixed the anchor is non-empty, and every
  // non-empty read at one anchor returns the same xs.
  const anchor = [...raw.values()].find((r) => r.xs.length > 0);
  if (!anchor) return { active, unit, div: 1, data: null };
  const xs = anchor.xs;
  // A stream still empty at this anchor reads as an all-null column,
  // so it keeps its place in the data array instead of shortening it.
  const ysOf = (stream) => {
    const r = raw.get(stream);
    return r && r.ys.length === xs.length ? r.ys : new Array(xs.length).fill(null);
  };
  const all = [];
  for (const s of active) for (const v of ysOf(s.stream)) if (v != null) all.push(v);
  const isPower = unit === "W" || unit === "VAr";
  const { div, prefix } = isPower ? chooseDiv(all, currentDiv) : { div: 1, prefix: "" };
  const scaled = (stream) => ysOf(stream).map((v) => (v == null ? null : v / div));
  // Pool envelopes (battery, steam boiler): lower/upper bounds as
  // invisible series with a translucent band between them, behind
  // the pool's trace. Both edges need two samples in the window
  // before a band earns its place: one point has no area to fill,
  // and an envelope drawn from it would still stretch the shared
  // y-axis out to the pool's rated power and squash every trace for
  // nothing. The bounds forwarder republishes only when the envelope
  // moves, so on an idle site that is exactly what the window holds.
  const bandPoints = (b) => ysOf(b).filter((v) => v != null).length;
  // If a band-owning trace was toggled off out-of-band, it is gone
  // from `active` even though the pinned shape still expects its
  // band; drop it so the frame survives this tick and the activeKeys
  // check right after rebuilds.
  const banded = shape
    ? active.filter((s) => s.band && shape.bandKeys.includes(s.stream))
    : active.filter((s) => s.band?.every((b) => bandPoints(b) >= 2));
  const withPf = shape ? shape.withPf : card.pfOverlay === true && pfOn();
  const data = [xs];
  for (const s of active) data.push(scaled(s.stream));
  for (const s of banded) data.push(scaled(s.band[1]), scaled(s.band[0]));
  if (withPf) {
    for (const s of active) {
      const p = ysOf(s.p);
      const q = ysOf(s.stream);
      data.push(q.map((qv, i) => pfValue(p[i], qv)));
    }
  }
  return { active, unit, div, prefix, banded, withPf, data };
}

function buildChart(card, slot) {
  const Plot = requireUplot(slot);
  if (!Plot) return;
  // Scale + shape decisions are made once per build and stored on the
  // plots entry: the band/PF layout, which repaint() feeds back into
  // cardFrame() so the array it hands setData() matches the built shape
  // exactly, and the set of active streams, which fixes how many
  // columns that array has. Both are pinned rather than re-derived per
  // tick, so neither side has to guess the shape — and repaint() can
  // tell a plot built against a different series set from a live one.
  const secs = windowSecs();
  const { active, unit, div, prefix, banded, withPf, data } = cardFrame(card, secs);
  if (!active.length) {
    slot.innerHTML = '<p class="hint">all series toggled off</p>';
    return;
  }
  if (data === null) {
    // Nothing sampled yet — no anchor second to draw a time axis
    // against. repaint() builds the card the moment one lands.
    slot.innerHTML = '<p class="hint">waiting for samples</p>';
    return;
  }
  const shown = unit;
  const series = [
    {},
    ...active.map((s) => ({
      stroke: cssColor(s.color),
      width: 1.5,
      points: { show: false },
      spanGaps: false,
    })),
  ];
  const bands = [];
  banded.forEach((s, i) => {
    // spanGaps on the edges only: a bound holds until it is
    // republished, so a gap between two bound samples is the same
    // envelope, not missing data — unlike the traces, where a gap
    // means no sample arrived and has to read as one.
    series.push(
      { stroke: "transparent", points: { show: false }, spanGaps: true },
      { stroke: "transparent", points: { show: false }, spanGaps: true },
    );
    // data indices: 0 = xs, 1..active.length = traces, then one
    // (hi, lo) pair per banded trace in `banded` order. uPlot fills
    // a band from series[0] down to series[1] (its default dir), so
    // the upper bound has to be listed first.
    const hi = active.length + 2 * i + 1;
    // The band is its trace's colour at a tenth opacity: the palette
    // is 6-digit hex, so an alpha byte appended makes the fill.
    bands.push({ series: [hi, hi + 1], fill: `${cssColor(s.color)}1a` });
  });
  // PF overlay: dashed per-source PF on a right-hand 0.85–1.02
  // scale, derived at draw time from the matching P and Q rings.
  const axes = [
    { stroke: "#7d848e", grid: { stroke: "#353a45", width: 0.5 } },
    {
      stroke: "#7d848e",
      grid: { stroke: "#353a45", width: 0.5 },
      size: 56,
      label: prefix || shown ? `${prefix}${shown}` : "",
      labelSize: 12,
    },
  ];
  if (withPf) {
    for (const s of active) {
      series.push({
        stroke: cssColor(s.color),
        width: 1,
        dash: [3, 3],
        points: { show: false },
        scale: "pf",
      });
    }
    axes.push({
      scale: "pf",
      side: 1,
      stroke: "#7d848e",
      grid: { show: false },
      size: 44,
    });
  }
  const opts = {
    width: slot.clientWidth || 380,
    height: 150,
    cursor: { drag: { x: false, y: false } },
    legend: { show: false },
    scales: { x: { time: true }, pf: { range: [0.85, 1.02] } },
    axes,
    series,
    bands,
    hooks: { draw: [drawZeroLine] },
  };
  plots.set(card.key, {
    plot: new Plot(opts, data, slot),
    unit,
    div,
    activeKeys: activeKeysOf(active),
    bandKeys: banded.map((s) => s.stream),
    withPf,
  });
}

// Chips + fold summaries repaint on every store notify (rAF-
// coalesced); charts re-data on the same tick.
function repaint(contentEl) {
  for (const card of CARDS) {
    const summary = contentEl.querySelector(`[data-summary="${card.key}"]`);
    if (summary) {
      const head = metricsStore.latest(card.headline);
      let text = head ? fmtValue(head.quantity, head.unit, head.value) : "—";
      if (card.key === "reactive") {
        const p = metricsStore.latest("grid_power")?.value;
        text += ` · ${pfText(p, head?.value ?? null)}`;
      }
      if (card.key === "power") text = `grid ${text}`;
      summary.textContent = text;
    }
    for (const s of card.series) {
      const v = contentEl.querySelector(`[data-chip-value="${s.stream}"]`);
      if (!v) continue;
      const snap = metricsStore.latest(s.stream);
      v.textContent = snap ? fmtValue(snap.quantity, snap.unit, snap.value) : "—";
      if (s.p) {
        const pf = contentEl.querySelector(`[data-chip-pf="${s.stream}"]`);
        if (pf) pf.textContent = pfText(metricsStore.latest(s.p)?.value, snap?.value ?? null);
      }
    }
  }
  // Charts re-data in place from the same frame the staleness check
  // reads: cardFrame() walks each of the card's streams once and
  // returns both the re-derived unit/divisor and the data array built
  // in the shape the plot was created with, so a tick costs one pass
  // over the window per stream — not the two it cost when the probe
  // and the data assembly each did their own reads. Band presence
  // still refreshes on the next rebuild (window / toggle / unfold)
  // rather than per tick, but the axis unit and the kW-vs-MW divisor
  // cannot wait for one: a card built before its stream had any
  // samples would otherwise keep raw watts under a blank axis label
  // for as long as the panel stays open. A rebuild only on a real
  // change — and chooseDiv's hysteresis is what keeps "real" from
  // meaning "the max wobbled across 1e3 again".
  const secs = windowSecs();
  for (const [key, entry] of [...plots]) {
    const card = CARDS.find((c) => c.key === key);
    if (!card) continue;
    const frame = cardFrame(card, secs, entry.div, entry);
    // The visible series set is read from localStorage every tick, so a
    // second tab toggling a chip rewrites `mc-metrics-series-*` under a
    // live plot and the next frame carries a different column count —
    // setData() would then hand uPlot an array its series list doesn't
    // index. A changed series set is as stale as a changed unit.
    // Same class of pin, same failure: the PF overlay flag is read
    // from localStorage too, so a second tab flipping `mc-metrics-pf`
    // changes the column count under a live plot. The in-panel toggle
    // rebuilds on its own click, but an out-of-band change would
    // otherwise leave the pinned `withPf` shape in force forever —
    // PF series built but fed nothing, or dropped and never built.
    const wantPf = card.pfOverlay === true && pfOn();
    if (activeKeysOf(frame.active) !== entry.activeKeys || wantPf !== entry.withPf) {
      rebuildCard(key);
      continue;
    }
    if (frame.data === null) continue;
    if (frame.unit !== entry.unit || frame.div !== entry.div) rebuildCard(key);
    else entry.plot.setData(frame.data);
  }
  // A card unfolded before any of its traces had a sample holds a
  // placeholder instead of a plot; the first sample to land is its
  // build trigger. Same fold as cardFrame's, or the two would
  // disagree: a bounds sample alone gives no anchor, so building on
  // one would draw the placeholder straight back.
  // rebuildCard clears the slot first, so with no uPlot this would
  // rewrite the "unavailable" note every frame.
  if (!uplot) return;
  for (const card of CARDS) {
    if (plots.has(card.key) || !cardOpen(card)) continue;
    const active = card.series.filter(seriesOn);
    if (active.length && latestSecond(anchorStreams(active)) !== null) rebuildCard(card.key);
  }
}

function scheduleRepaint(contentEl) {
  if (repaintQueued) return;
  repaintQueued = true;
  requestAnimationFrame(() => {
    repaintQueued = false;
    if (isPanelOpen(PANEL)) repaint(contentEl);
  });
}

function rebuildCard(key) {
  const entry = plots.get(key);
  const card = CARDS.find((c) => c.key === key);
  const slot = document.querySelector(`#panel-${PANEL} [data-chart="${key}"]`);
  if (entry) {
    entry.plot.destroy();
    plots.delete(key);
  }
  if (card && slot && cardOpen(card)) {
    slot.innerHTML = "";
    buildChart(card, slot);
  }
}

function chipHtml(s) {
  return `
    <button type="button" class="mchip${seriesOn(s) ? "" : " off"}" data-chip="${s.stream}"
            style="--chip-color: var(${s.color})">
      <span class="mchip-dot"></span>
      <span class="mchip-name">${s.label}</span>
      <span class="mchip-value" data-chip-value="${s.stream}">—</span>
      ${s.p ? `<span class="mchip-pf" data-chip-pf="${s.stream}">PF —</span>` : ""}
    </button>`;
}

function cardHtml(card) {
  const open = cardOpen(card);
  const pfChip = card.pfOverlay
    ? `<button type="button" class="mchip mchip-pf-toggle${pfOn() ? "" : " off"}" data-pf-toggle>
         <span class="mchip-name">PF overlay</span>
       </button>`
    : "";
  return `
    <section class="mcard${open ? " open" : ""}" data-card="${card.key}">
      <h3 class="fold-toggle" data-fold-toggle>${card.title}
        <span class="fold-summary"><span data-summary="${card.key}">—</span><span class="fold-chevron">▾</span></span>
      </h3>
      <div class="fold-body">
        <div class="mchart" data-chart="${card.key}"></div>
        <div class="mchips">${card.series.map(chipHtml).join("")}${pfChip}</div>
      </div>
    </section>`;
}

function render(contentEl) {
  const winKey = loadKey("mc-metrics-window", "5m");
  contentEl.innerHTML = `
    <div class="metrics-panel">
      <div class="metrics-head">
        <h2>Metrics</h2>
        <span class="ctl-label">window</span>
        ${WINDOWS.map(
          (w) =>
            `<button type="button" class="pill win-pill${w.key === winKey ? " active" : ""}" data-window="${w.key}">${w.key}</button>`,
        ).join("")}
      </div>
      ${CARDS.map(cardHtml).join("")}
    </div>`;

  sizeObserver?.disconnect();
  sizeObserver = new ResizeObserver(() => {
    for (const [key, entry] of plots) {
      const slot = contentEl.querySelector(`[data-chart="${key}"]`);
      if (!slot?.clientWidth || slot.clientWidth === entry.plot.width) continue;
      entry.plot.setSize({ width: slot.clientWidth, height: entry.plot.height });
    }
  });
  sizeObserver.observe(contentEl);

  contentEl.querySelector(".metrics-panel").addEventListener("click", (ev) => {
    const win = ev.target.closest("[data-window]");
    if (win) {
      saveKey("mc-metrics-window", win.dataset.window);
      for (const b of contentEl.querySelectorAll("[data-window]")) {
        b.classList.toggle("active", b === win);
      }
      for (const c of CARDS) rebuildCard(c.key);
      return;
    }
    const pfToggle = ev.target.closest("[data-pf-toggle]");
    if (pfToggle) {
      saveKey("mc-metrics-pf", pfOn() ? "0" : "1");
      pfToggle.classList.toggle("off", !pfOn());
      rebuildCard("reactive");
      return;
    }
    const chip = ev.target.closest("[data-chip]");
    if (chip) {
      const stream = chip.dataset.chip;
      const card = CARDS.find((c) => c.series.some((s) => s.stream === stream));
      saveKey(`mc-metrics-series-${stream}`, seriesOn({ stream }) ? "0" : "1");
      chip.classList.toggle("off", !seriesOn({ stream }));
      if (card) rebuildCard(card.key);
      return;
    }
    const foldToggle = ev.target.closest("[data-fold-toggle]");
    if (foldToggle) {
      const cardEl = foldToggle.closest("[data-card]");
      const card = CARDS.find((c) => c.key === cardEl.dataset.card);
      const nowOpen = !cardEl.classList.contains("open");
      cardEl.classList.toggle("open", nowOpen);
      saveKey(`mc-metrics-card-${card.key}`, nowOpen ? "1" : "0");
      rebuildCard(card.key);
    }
  });

  for (const card of CARDS) rebuildCard(card.key);
  unsubscribe = metricsStore.subscribe(() => scheduleRepaint(contentEl));
  // The dropped-frame safety net only earns its poll while the panel
  // is on screen, so it runs on the panel's own lifetime rather than
  // the document's — a process-lifetime interval also keeps the
  // headless boot smoke's event loop alive forever.
  metricsStore.startAutoReseed(() => isPanelOpen(PANEL));
  metricsStore.backfill().then(() => {
    if (!isPanelOpen(PANEL)) return;
    for (const c of CARDS) rebuildCard(c.key);
    repaint(contentEl);
  });
}

function teardown() {
  unsubscribe?.();
  unsubscribe = null;
  metricsStore.stopAutoReseed();
  sizeObserver?.disconnect();
  sizeObserver = null;
  destroyPlots();
}

// Re-backfill on topology mutations while open — app.js's debounced
// topology-backfill hook calls this; closed, the next open backfills
// anyway.
export function metricsTopologyRefresh() {
  if (isPanelOpen(PANEL)) metricsStore.backfill();
}

export function setupMetricsPanel() {
  makeSidePanelToggle(PANEL, render, teardown);
}
