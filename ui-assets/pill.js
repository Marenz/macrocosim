// The pill node: one model builder (pure) and one canvas renderer,
// driving the Topology canvas through vis-network's
// `shape: "custom"`. The model says *what* a node shows; the
// renderer (further down) says how it looks.

import { formatScaled } from "./live.js";

// Colours come from the :root tokens in style.css so a re-theme
// reaches the canvas; the literal is the fallback for a stylesheet
// without them (never a transparent pill).
export function cssToken(name, fallback = "") {
  if (typeof document === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export const COLORS = {
  export: cssToken("--flow-export", "#6bd9a5"),
  import: cssToken("--flow-import", "#79b8ff"),
  dim: cssToken("--flow-dim", "#5a626d"),
  exportDull: cssToken("--flow-export-dull", "#4f9a78"),
  importDull: cssToken("--flow-import-dull", "#5a87bd"),
  edgeRest: cssToken("--edge-rest", "#6b7280"),
  surface: cssToken("--pill-surface", "#242a33"),
  border: cssToken("--pill-border", "#323a45"),
  fg: cssToken("--pill-fg", "#d5dbe3"),
  muted: cssToken("--pill-muted", "#7d848e"),
  accent: cssToken("--accent", "#79b8ff"),
  hover: cssToken("--pill-hover", "#b0b8c1"),
  bad: cssToken("--bad", "#e58275"),
  standby: cssToken("--standby", "#c4ad55"),
  socFill: cssToken("--flow-export", "#6bd9a5"),
};

const finite = (v) => v != null && Number.isFinite(v);

// Linear blend of two #rrggbb colours; t = 0 gives a, t = 1 gives b.
export function mixHex(a, b, t) {
  if (!/^#[0-9a-f]{6}$/i.test(a) || !/^#[0-9a-f]{6}$/i.test(b)) return a;
  const ch = (h, i) => parseInt(h.slice(i, i + 2), 16);
  const out = [1, 3, 5].map((i) => Math.round(ch(a, i) + (ch(b, i) - ch(a, i)) * t));
  return `#${out.map((v) => Math.max(0, Math.min(255, v)).toString(16).padStart(2, "0")).join("")}`;
}

// The pill border carries a hint of the category so the node still
// reads as "a battery" when the bar is a few pixels wide.
export function borderColor(catColor) {
  return mixHex(COLORS.border, catColor, 0.35);
}

// A faint flow tint on the surface: export leans green, import blue,
// a dead or unknown value stays neutral. Never with values off.
export function surfaceColor(heroValue, deadBand, valuesOn) {
  if (!valuesOn || !finite(heroValue) || Math.abs(heroValue) < deadBand) return COLORS.surface;
  return mixHex(COLORS.surface, heroValue < 0 ? COLORS.export : COLORS.import, 0.07);
}

// Consumption-positive: import blue, export green, dead band dim.
export function powerColor(value, deadBand) {
  if (!finite(value) || Math.abs(value) < deadBand) return COLORS.dim;
  return value < 0 ? COLORS.export : COLORS.import;
}

// Reactive power in duller versions of the same hues, by its own
// sign (+Q inductive). Beside the active colour, a matching hue reads
// as lagging and a contrasting one as leading.
export function reactiveColor(value, deadBand) {
  if (!finite(value) || Math.abs(value) < deadBand) return COLORS.dim;
  return value < 0 ? COLORS.exportDull : COLORS.importDull;
}

// Grid frequency by its distance from the 50 Hz nominal: plain text
// under 0.1 Hz away, the standby amber from 0.1 Hz, bad from 0.2 Hz.
// The bands are the ones a control room alarms on, not the sim's own
// noise floor.
const NOMINAL_HZ = 50;
export function frequencyColor(hz) {
  if (!finite(hz)) return COLORS.dim;
  const dev = Math.abs(hz - NOMINAL_HZ);
  if (dev >= 0.2) return COLORS.bad;
  if (dev >= 0.1) return COLORS.standby;
  return COLORS.fg;
}

// The frequency readout with its colour, or null before a sample.
export function frequencyReadout(hz) {
  return finite(hz) ? { text: `${hz.toFixed(2)} Hz`, color: frequencyColor(hz) } : null;
}

// Long names get shortened on the pill; the full name lives in the
// hover card / tooltip.
export function shortName(name) {
  return name.length > 22 ? `${name.slice(0, 20)}…` : name;
}

function effectiveHealth(c) {
  const health = c.health || "ok";
  return health === "ok" && c.provides_telemetry === false ? "standby" : health;
}

function socAux(soc) {
  if (!finite(soc)) return null;
  const pct = Math.round(soc);
  return { kind: "soc", pct, text: `${pct}%` };
}

// An empty charger's aux slot: a charger's SoC is its car's, so with
// nothing plugged in there is no gauge to draw. Frozen and shared —
// every empty charger on the canvas shows the same readout.
const NO_EV_AUX = Object.freeze({ kind: "text", text: "no EV" });

// Text-only readout, no gauge — a boiler's pressure isn't a 0–100%
// quantity like SoC, so it just prints the bar value.
function pressureAux(pressure) {
  if (!finite(pressure)) return null;
  return { kind: "text", text: `${pressure.toFixed(1)} bar` };
}

// component: a component of /api/mg/{mg}/topology; live: { p, q,
// soc, dc, hz } or null; options: { valuesOn, catColor, deadBand }.
export function pillModel(c, live, { valuesOn, catColor, deadBand }) {
  let hero = null;
  let aux = null;
  let power = null;
  if (valuesOn && live) {
    if (c.category === "grid") {
      hero = frequencyReadout(live.hz);
    } else {
      power = c.category === "battery" ? live.dc : live.p;
      if (c.category === "battery") {
        aux = socAux(live.soc);
      } else if (c.category === "ev-charger") {
        aux = socAux(live.soc) ?? NO_EV_AUX;
      } else if (c.category === "steam-boiler") {
        aux = pressureAux(live.pressure);
      } else if (finite(live.q)) {
        aux = { kind: "reactive", text: formatScaled(live.q, "VAr"), color: reactiveColor(live.q, deadBand) };
      }
      if (finite(power)) hero = { text: formatScaled(power, "W"), color: powerColor(power, deadBand) };
      else if (aux) hero = { text: "—", color: COLORS.dim };
    }
  }
  const heroValue = finite(power) ? power : null;
  return {
    id: c.id,
    name: shortName(c.name),
    fullName: c.name,
    idText: `#${c.id}`,
    catColor,
    health: effectiveHealth(c),
    hidden: Boolean(c.hidden),
    valuesOn: Boolean(valuesOn),
    hero,
    heroValue,
    deadBand,
    aux,
    highlight: "none",
    surface: surfaceColor(heroValue, deadBand, valuesOn),
    border: borderColor(catColor),
  };
}

// ── renderer ────────────────────────────────────────────────────
// Everything below draws in canvas units; vis-network applies the
// zoom and device-pixel scaling, so 14 px here is 14 px at scale 1.

export const FONT_SANS = '"IBM Plex Sans", system-ui, -apple-system, "Segoe UI", sans-serif';
export const FONT_MONO = '"IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, monospace';

const GEOM = {
  minWidth: 96,
  maxWidth: 200,
  padX: 10,
  padY: 6,
  bar: 6,
  barGap: 10,
  idGap: 6,
  rowGap: 3,
  dividerGap: 8,
  radius: 10,
  socBarW: 40,
  socBarH: 5,
  socGap: 6,
};

function fonts(model) {
  return model.valuesOn
    ? { name: `500 11.5px ${FONT_SANS}`, id: `400 10px ${FONT_MONO}`, row1H: 14 }
    : { name: `500 13px ${FONT_SANS}`, id: `400 11px ${FONT_MONO}`, row1H: 16 };
}
const FONT_HERO = `600 14px ${FONT_MONO}`;

// Level of detail by canvas scale. Text is unreadable below ~0.8, so
// the pill drops to its hero number, and below 0.4 to no text at all.
// A tier changes only once the scale is 0.05 past the threshold it is
// sitting on, so panning at a boundary does not flicker; a jump clear
// past the far threshold takes that one raw.
const LOD_FULL = 0.8;
const LOD_HERO = 0.4;
const LOD_HYST = 0.05;
export function lodFor(scale, prev) {
  if (!Number.isFinite(scale)) return prev ?? "full";
  const up = (t) => t + LOD_HYST;
  const down = (t) => t - LOD_HYST;
  if (prev === "full") return scale >= down(LOD_FULL) ? "full" : scale >= LOD_HERO ? "hero" : "marker";
  if (prev === "hero") return scale >= up(LOD_FULL) ? "full" : scale >= down(LOD_HERO) ? "hero" : "marker";
  if (prev === "marker") return scale >= up(LOD_FULL) ? "full" : scale >= up(LOD_HERO) ? "hero" : "marker";
  return scale >= LOD_FULL ? "full" : scale >= LOD_HERO ? "hero" : "marker";
}
const FONT_HERO_ONLY = `600 16px ${FONT_MONO}`;
const FONT_AUX = `500 11px ${FONT_MONO}`;
const ROW2_H = 17;

// Canvas text is not a DOM use, so the browser never loads a web
// font for it on its own; ask for the faces explicitly.
export const pillFontsReady =
  typeof document !== "undefined" && document.fonts
    ? Promise.all([
        document.fonts.load('500 11.5px "IBM Plex Sans"'),
        document.fonts.load('600 14px "IBM Plex Mono"'),
        document.fonts.load('400 10px "IBM Plex Mono"'),
        document.fonts.load('500 11px "IBM Plex Mono"'),
      ]).then(() => undefined, () => undefined)
    : Promise.resolve();

const measureCache = new Map();
export function invalidateMeasureCache() {
  measureCache.clear();
}

function textWidth(ctx, font, text) {
  ctx.font = font;
  return ctx.measureText(text).width;
}

function hasRow2(model) {
  return Boolean(model.valuesOn && (model.hero || model.aux));
}

function auxWidth(ctx, aux) {
  if (!aux) return 0;
  if (aux.kind === "soc") return GEOM.socBarW + GEOM.socGap + textWidth(ctx, FONT_AUX, aux.text);
  return textWidth(ctx, FONT_AUX, aux.text);
}

// Measures the pill for `model`. Cached by the strings that affect
// size; hover/selection never change a pill's size.
//
// `model.minWidth` is an optional lower bound the canvas owner may
// raise per node (topology.js ratchets it to the widest the node has
// ever been, so a value that shrinks by a character doesn't shuffle
// the layout). The content is laid out from the bar either way, so a
// floored pill simply carries more padding on its right.
export function measurePill(ctx, model) {
  const key = [model.valuesOn ? 1 : 0, model.minWidth || 0, model.name, model.idText, model.hero?.text ?? "", model.aux?.kind ?? "", model.aux?.text ?? ""].join("\u0000");
  const hit = measureCache.get(key);
  if (hit) return hit;
  const f = fonts(model);
  const textLeft = GEOM.bar + GEOM.barGap;
  const row2 = hasRow2(model);
  let row2W = 0;
  let heroW = 0;
  if (row2) {
    heroW = textWidth(ctx, FONT_HERO, model.hero ? model.hero.text : "—");
    row2W = heroW;
    if (model.aux) row2W += GEOM.dividerGap * 2 + 1 + auxWidth(ctx, model.aux);
  }
  const idW = textWidth(ctx, f.id, model.idText);
  // Row 1 may need to give way: the pill must stay inside the
  // layout's column separation, so the name truncates until it fits.
  let name = model.name;
  let nameW = textWidth(ctx, f.name, name);
  let row1W = nameW + GEOM.idGap + idW;
  const budget = GEOM.maxWidth - textLeft - GEOM.padX;
  while (row1W > budget && name.length > 4) {
    name = `${name.replace(/…$/, "").slice(0, -1)}…`;
    nameW = textWidth(ctx, f.name, name);
    row1W = nameW + GEOM.idGap + idW;
  }
  const content = Math.max(row1W, row2W);
  const floor = Math.max(GEOM.minWidth, model.minWidth || 0);
  const width = Math.round(Math.min(GEOM.maxWidth, Math.max(floor, textLeft + content + GEOM.padX)));
  const height = GEOM.padY * 2 + f.row1H + (row2 ? GEOM.rowGap + ROW2_H : 0);
  const dims = { width, height, name, nameW, heroW, textLeft, row1H: f.row1H, row2H: row2 ? ROW2_H : 0 };
  // Live value strings make most keys single-use; the cap keeps a
  // day-long tab flat while still serving the repeated draws between flushes.
  if (measureCache.size > 1024) measureCache.clear();
  measureCache.set(key, dims);
  return dims;
}

function roundRect(ctx, x, y, w, h, r) {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

function borderStyle(model, state) {
  if (model.highlight === "subtracted") return { color: COLORS.bad, width: 2 };
  if (state.selected) return { color: COLORS.accent, width: 2 };
  if (state.hover) return { color: COLORS.hover, width: 1.5 };
  return { color: model.border, width: 1.5 };
}

// Draws the pill centred on (x, y). `state` is vis's
// { selected, hover }; a formula "subtracted" highlight lives in the
// model itself (it is not a vis selection).
export function drawPill(ctx, x, y, model, state, lod = "full") {
  const d = measurePill(ctx, model);
  const f = fonts(model);
  const left = x - d.width / 2;
  const top = y - d.height / 2;
  // surface + border
  roundRect(ctx, left, top, d.width, d.height, GEOM.radius);
  ctx.fillStyle = model.surface;
  ctx.fill();
  const b = borderStyle(model, state);
  ctx.setLineDash(model.hidden ? [4, 3] : []);
  ctx.lineWidth = b.width;
  ctx.strokeStyle = b.color;
  ctx.stroke();
  ctx.setLineDash([]);
  // category bar on the left edge, clipped to the rounded corner
  ctx.save();
  roundRect(ctx, left, top, d.width, d.height, GEOM.radius);
  ctx.clip();
  ctx.fillStyle = model.catColor;
  ctx.fillRect(left, top, GEOM.bar, d.height);
  ctx.restore();
  // health ring around the whole pill
  if (model.health !== "ok") {
    roundRect(ctx, left - 2.5, top - 2.5, d.width + 5, d.height + 5, GEOM.radius + 2.5);
    ctx.lineWidth = 1.5;
    ctx.strokeStyle = model.health === "error" ? COLORS.bad : COLORS.standby;
    ctx.stroke();
  }
  if (lod === "marker") return;
  if (lod === "hero") {
    // one centred row: the hero power only (a node without a hero
    // value shows the bare pill)
    if (!model.hero) return;
    ctx.textBaseline = "middle";
    ctx.textAlign = "left";
    ctx.font = FONT_HERO_ONLY;
    ctx.fillStyle = model.hero.color;
    ctx.fillText(model.hero.text, left + d.textLeft, top + d.height / 2 + 1, d.width - d.textLeft - GEOM.padX);
    return;
  }
  // row 1: name + id
  const row1Y = top + GEOM.padY + f.row1H / 2;
  ctx.textBaseline = "middle";
  ctx.textAlign = "left";
  let tx = left + d.textLeft;
  ctx.font = f.name;
  ctx.fillStyle = COLORS.fg;
  ctx.fillText(d.name, tx, row1Y);
  tx += d.nameW + GEOM.idGap;
  ctx.font = f.id;
  ctx.fillStyle = COLORS.muted;
  ctx.fillText(model.idText, tx, row1Y + 0.5);
  if (!d.row2H) return;
  // row 2: hero | aux
  const row2Y = top + GEOM.padY + f.row1H + GEOM.rowGap + ROW2_H / 2;
  tx = left + d.textLeft;
  ctx.font = FONT_HERO;
  const hero = model.hero || { text: "—", color: COLORS.dim };
  ctx.fillStyle = hero.color;
  ctx.fillText(hero.text, tx, row2Y);
  if (!model.aux) return;
  tx += d.heroW + GEOM.dividerGap;
  ctx.fillStyle = COLORS.border;
  ctx.fillRect(Math.round(tx), row2Y - 6, 1, 12);
  tx += 1 + GEOM.dividerGap;
  if (model.aux.kind === "soc") {
    const barY = row2Y - GEOM.socBarH / 2;
    roundRect(ctx, tx, barY, GEOM.socBarW, GEOM.socBarH, GEOM.socBarH / 2);
    ctx.fillStyle = COLORS.border;
    ctx.fill();
    const fillW = Math.max(0, Math.min(GEOM.socBarW, (GEOM.socBarW * model.aux.pct) / 100));
    if (fillW > 0) {
      roundRect(ctx, tx, barY, fillW, GEOM.socBarH, GEOM.socBarH / 2);
      ctx.fillStyle = COLORS.socFill;
      ctx.fill();
    }
    tx += GEOM.socBarW + GEOM.socGap;
    ctx.font = FONT_AUX;
    ctx.fillStyle = COLORS.fg;
    ctx.fillText(model.aux.text, tx, row2Y);
  } else if (model.aux.kind === "text") {
    // Plain readout, no gauge and no signed-value colouring — a
    // boiler's pressure bar (or any future non-power aux).
    ctx.font = FONT_AUX;
    ctx.fillStyle = COLORS.fg;
    ctx.fillText(model.aux.text, tx, row2Y);
  } else {
    ctx.font = FONT_AUX;
    ctx.fillStyle = model.aux.color;
    ctx.fillText(model.aux.text, tx, row2Y);
  }
}

// The vis-network `ctxRenderer` for one node. vis calls it on every
// draw with { ctx, id, x, y, state, style, label }; `onSize` lets the
// canvas owner notice size changes (vis applies `nodeDimensions` one
// draw late — see topology.js).
export function pillRenderer(model, onSize, getLod) {
  return ({ ctx, id, x, y, state }) => {
    const d = measurePill(ctx, model);
    if (onSize) onSize(id, d.width, d.height);
    return {
      drawNode() {
        drawPill(ctx, x, y, model, state || { selected: false, hover: false }, getLod ? getLod() : "full");
      },
      nodeDimensions: { width: d.width, height: d.height },
    };
  };
}
