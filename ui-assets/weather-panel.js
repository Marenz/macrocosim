// The weather panel: the site's sky as one card — a day curve, the
// live sunlight readout, the config knobs, and a "pass a cloud"
// trigger. It is the UI's face on `GET/POST /api/mg/{mg}/weather`
// (src/ui/handlers/weather.rs), which mirrors the Lisp
// `(make-weather)` / `(set-weather)` / `(pass-cloud)` doors for
// people who aren't driving the site from the console.
//
// Everything here is UTC: the server prints `"HH:MM"` times and
// RFC 3339 instants, the panel reads and writes exactly those. There
// is no timezone conversion anywhere in this module, on purpose — a
// sunrise the user typed has to come back as the sunrise they typed.
//
// The day curve is computed client-side rather than sampled off the
// server: the clear-sky sine and the clouds' trapezoids are four
// lines of arithmetic each (mirrored from `Weather::clear_sky_pct` /
// `CloudEvent::attenuation_at`), and 145 samples an endpoint call
// apiece would be a poll-rate stampede for a curve that only moves
// when the config does.

import { requireUplot } from "./chart-lib.js";
import { mgFetch } from "./routing.js";
import { isPanelOpen, makeSidePanelToggle } from "./side-panel.js";

const PANEL = "weather-btn";
// The scenarios panel's cadence (panels.js:945): weather has no WS
// push of its own, and the readout has to track a passing cloud.
const POLL_MS = 3000;
// The curve's sampling grid: the whole UTC day, every 10 minutes.
// Fine enough that the sine reads as a smooth arch and a multi-minute
// cloud shows as a visible notch, coarse enough to stay 145 points.
const SAMPLE_S = 600;
const DAY_S = 86400;

// contentEl lives here rather than being threaded through every
// helper: there is exactly one weather panel, and teardown() clears
// it (which is also how an in-flight refresh knows to drop its
// result).
let contentEl = null;
let plot = null;
let sizeObserver = null;
let pollTimer = 0;
// Which skeleton is currently painted — "" (nothing yet), "empty"
// (404, the create prompt), "none" (no microgrid selected) or "live".
// A refresh only re-paints the skeleton when this changes; otherwise
// it writes into the DOM that is already there, which is what keeps
// a focused field's text from being blown away every 3 s.
let skeleton = "";
// Where the now-marker goes, in hours-of-day — read by the draw hook,
// which uPlot calls on its own schedule and can't be handed an
// argument. See nowHours() for why the browser's clock is the right
// one to ask.
let markerHour = 0;
// The last reading the panel painted. Kept so the ghost preview can
// re-plot the curve on a keystroke, where there is no fresh payload
// to hand applyLive.
let lastWeather = null;
// The ghost preview's two triggers: the pass-a-cloud fields are being
// filled in (focus), or they have been changed and not fired yet.
// Either one draws the would-be cloud; firing clears both.
let ghostFocused = false;
let ghostEdited = false;

const escapeHtml = (s) =>
  String(s).replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );

const cssColor = (v) => getComputedStyle(document.documentElement).getPropertyValue(v).trim();

// ── the sky, in arithmetic ──────────────────────────────────────────

// `"HH:MM"` → seconds from UTC midnight; null when malformed. The
// same spelling `parse_time_of_day` accepts, checked here too so a
// typo reverts the field locally instead of round-tripping a 400.
function hhmmToSecs(text) {
  const m = /^(\d{1,2}):([0-5]\d)$/.exec(String(text).trim());
  if (!m) return null;
  const h = Number(m[1]);
  return h > 23 ? null : h * 3600 + Number(m[2]) * 60;
}

const pad2 = (n) => String(Math.floor(n)).padStart(2, "0");
const secsToHhmm = (s) => `${pad2(s / 3600)}:${pad2((s % 3600) / 60)}`;
// An absolute instant as its UTC wall time — never the viewer's zone.
const utcHhmm = (ms) => `${pad2(new Date(ms).getUTCHours())}:${pad2(new Date(ms).getUTCMinutes())}`;

// `Weather::clear_sky_pct`, transcribed: zero outside the window,
// else a sine arch peaking at `peak` at the window's midpoint.
function clearSkyPct(secs, sunrise, sunset, peak) {
  if (sunset <= sunrise || secs < sunrise || secs > sunset) return 0;
  return peak * Math.sin(Math.PI * ((secs - sunrise) / (sunset - sunrise)));
}

// `CloudEvent::attenuation_at`, transcribed: a trapezoid ramping to
// `depth` over `ramp`, holding, then ramping back down.
//
// The GET payload gives each event a start, an end and a depth but
// not its ramp split, so `ev.ramp` is an estimate (see daySeries).
// A cloud's ramps are seconds-to-a-minute against a 10-minute
// sampling grid, so the estimate moves the drawn curve by less than
// a pixel; it is the plateau depth and the event's span, both exact,
// that the notch is actually read off.
function attenuationAt(ev, tMs) {
  if (tMs < ev.start || tMs >= ev.end) return 0;
  const depth = Math.min(1, Math.max(0, ev.depth / 100));
  const span = ev.end - ev.start;
  const ramp = Math.min(ev.ramp, span / 2);
  if (!(ramp > 0)) return depth;
  const elapsed = tMs - ev.start;
  if (elapsed < ramp) return depth * (elapsed / ramp);
  if (elapsed < span - ramp) return depth;
  return depth * ((span - elapsed) / ramp);
}

// Midnight UTC of the day the marker sits in — the curve's origin.
function dayStartMs(nowMs) {
  const d = new Date(nowMs);
  return Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate());
}

// The now-marker's hour-of-day. The server evaluates the sky at the
// weather's own anchor, which `advance` re-stamps from the wall clock
// every tick — so the browser's clock is close enough, and asking it
// here costs nothing. The marker deliberately stays on the browser
// clock: skew only nudges a pixel-wide line. The cloud-expiry filter
// is the one place skew has a visible casualty, and it uses the
// payload's server-stamped `now` instead (see `eventsHtml`).
const nowHours = (nowMs) => (nowMs - dayStartMs(nowMs)) / 3600000;

// The curve's x samples, in seconds from UTC midnight: the regular
// 10-minute grid, plus the ghost trapezoid's four corners. Without
// those corners a previewed cloud shorter than one grid step would
// fall between two samples and draw as nothing.
function sampleSecs(ghostEv, origin) {
  const secs = [];
  for (let s = 0; s <= DAY_S; s += SAMPLE_S) secs.push(s);
  if (!ghostEv) return secs;
  const ramp = Math.min(ghostEv.ramp, (ghostEv.end - ghostEv.start) / 2);
  // The trailing inside corner is held a millisecond off the end even
  // when the ramp is zero: sampled AT the end the cloud is already
  // over, so with no ramp the only two corners would be "full depth"
  // and "gone", and the line between them would slope across the
  // whole cloud. One sample just inside makes that fall a step.
  for (const ms of [
    ghostEv.start,
    ghostEv.start + ramp,
    ghostEv.end - Math.max(ramp, 1),
    ghostEv.end,
  ]) {
    const s = (ms - origin) / 1000;
    if (s > 0 && s < DAY_S) secs.push(s);
  }
  // A zero ramp makes the leading corners coincide; the Set drops the
  // repeats.
  return [...new Set(secs)].sort((a, b) => a - b);
}

// The three plotted series over one UTC day: clear-sky, clear-sky
// attenuated by every tracked cloud (Π (1 − attenuation_i), so
// overlapping clouds compound — `Weather::pct_at`'s rule), and the
// ghost — that same attenuated curve with one not-yet-fired cloud
// laid on top of it, or nulls everywhere when there is no preview.
function daySeries(w, nowMs, ghost) {
  const sunrise = hhmmToSecs(w.sunrise) ?? 0;
  const sunset = hhmmToSecs(w.sunset) ?? 0;
  const peak = Number(w.peak_pct) || 0;
  // Ramp estimate: the midpoint of the configured ramp range, which
  // is what an ambient cloud draws from. See attenuationAt.
  const rampMs = (((w.cloud_ramp_s?.[0] ?? 0) + (w.cloud_ramp_s?.[1] ?? 0)) / 2) * 1000;
  const events = (w.events ?? [])
    .map((e) => ({
      start: Date.parse(e.start),
      end: Date.parse(e.end),
      depth: Number(e.depth_pct) || 0,
      ramp: rampMs,
    }))
    .filter((e) => Number.isFinite(e.start) && Number.isFinite(e.end));
  const origin = dayStartMs(nowMs);
  // The previewed cloud starts at the now-marker: it is what firing
  // the button right now would do — so its span is the span
  // `Weather::pass_cloud` would build, not the duration as typed. That
  // door keeps both ramps at full length and saturates only the
  // plateau (`duration - 2*ramp`), so once 2×ramp passes duration the
  // cloud is two ramps back to back and outlives what was asked for.
  // Compressing the ramps to fit the duration instead would preview a
  // cloud the server never makes.
  const ghostEv = ghost
    ? {
        start: nowMs,
        end: nowMs + Math.max(ghost.duration, 2 * ghost.ramp) * 1000,
        depth: ghost.depth,
        ramp: ghost.ramp * 1000,
      }
    : null;
  const xs = [];
  const clear = [];
  const atten = [];
  const preview = [];
  for (const s of sampleSecs(ghostEv, origin)) {
    const t = origin + s * 1000;
    const cs = clearSkyPct(s, sunrise, sunset, peak);
    let transmission = 1;
    for (const e of events) transmission *= 1 - attenuationAt(e, t);
    const lit = cs * transmission;
    xs.push(s / 3600);
    clear.push(cs);
    atten.push(lit);
    // Null outside the previewed cloud's own span, so the ghost draws
    // as a short dip hanging off the real curve rather than a second
    // copy of it. Both its ends sit exactly on the curve, since the
    // trapezoid is zero at its corners.
    preview.push(ghostEv && t >= ghostEv.start && t <= ghostEv.end ? lit * (1 - attenuationAt(ghostEv, t)) : null);
  }
  return [xs, clear, atten, preview];
}

// ── the chart ───────────────────────────────────────────────────────

// The vertical "you are here" line — the metrics panel's drawZeroLine
// (metrics-panel.js:135) turned on its side. Same device-pixel care:
// uPlot hands canvas hooks a DPR-scaled context, so the width, the
// dashes and the half-pixel snap all scale by the canvas's own ratio
// or the line lands as a grey smear on a HiDPI screen.
function drawNowMarker(u) {
  const { min, max } = u.scales.x;
  if (min == null || max == null || markerHour < min || markerHour > max) return;
  const dpr = u.ctx.canvas.width / (u.width || 1);
  const { left, top, width, height } = u.bbox;
  const x = Math.round(u.valToPos(markerHour, "x", true) - dpr / 2) + dpr / 2;
  const ctx = u.ctx;
  ctx.save();
  ctx.beginPath();
  ctx.rect(left, top, width, height);
  ctx.clip();
  ctx.strokeStyle = "#7d848e";
  ctx.lineWidth = dpr;
  ctx.setLineDash([3 * dpr, 3 * dpr]);
  ctx.beginPath();
  ctx.moveTo(x, top);
  ctx.lineTo(x, top + height);
  ctx.stroke();
  ctx.restore();
}

function buildChart(slot, data) {
  const Plot = requireUplot(slot);
  if (!Plot) return;
  const solar = cssColor("--cat-inverter-solar") || "#a8d35a";
  const opts = {
    width: slot.clientWidth || 380,
    height: 140,
    cursor: { drag: { x: false, y: false } },
    legend: { show: false },
    scales: {
      x: { time: false, range: [0, 24] },
      // Pinned to zero at the bottom so a cloudy day doesn't rescale
      // into looking like a clear one; the top follows the peak.
      y: { range: (_u, _min, max) => [0, Math.max(5, max * 1.1)] },
    },
    axes: [
      {
        stroke: "#7d848e",
        grid: { stroke: "#353a45", width: 0.5 },
        splits: [0, 4, 8, 12, 16, 20, 24],
        values: (_u, splits) => splits.map((h) => `${pad2(h)}:00`),
      },
      {
        stroke: "#7d848e",
        grid: { stroke: "#353a45", width: 0.5 },
        size: 42,
        label: "%",
        labelSize: 12,
      },
    ],
    series: [
      {},
      // Clear sky dashed, the actual sky solid, the gap between them
      // filled — the same translucent-band idiom the metrics panel's
      // battery envelope uses (metrics-panel.js:309). uPlot fills
      // from the first listed series down to the second, and
      // clear-sky is by construction the upper of the two.
      { stroke: solar, width: 1, dash: [3, 3], points: { show: false } },
      { stroke: solar, width: 1.5, points: { show: false } },
      // The pass-a-cloud preview: same colour as the real sky but
      // faint and dashed, so it reads as "this would happen" rather
      // than as a reading. It is all nulls unless a preview is up.
      { stroke: solar, width: 1.5, dash: [2, 3], alpha: 0.55, points: { show: false } },
    ],
    bands: [{ series: [1, 2], fill: "rgba(168, 211, 90, 0.14)" }],
    hooks: { draw: [drawNowMarker] },
  };
  plot = new Plot(opts, data, slot);
}

// ── HTTP ────────────────────────────────────────────────────────────

// Both doors answer with a body either way — the weather shape on
// success, `{"error": …}` on a 4xx — so one envelope covers both and
// callers branch on `ok` alone. A dead fetch (server gone) lands here
// as ok:false with a message rather than an unhandled rejection.
async function call(init) {
  try {
    // Weather is site data: it reads and writes the selected
    // microgrid's sky, like every sibling data panel.
    const r = await mgFetch("weather", init);
    if (r == null) return { ok: false, status: 0, body: null, noSelection: true };
    const body = await r.json().catch(() => null);
    return { ok: r.ok, status: r.status, body };
  } catch (e) {
    return { ok: false, status: 0, body: { error: String(e) } };
  }
}

const getWeather = () => call(undefined);
const postWeather = (payload) =>
  call({
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });

const errorOf = (res) =>
  res.noSelection ? "no microgrid selected" : (res.body?.error ?? `request failed (${res.status || "no response"})`);

function showError(text) {
  const el = document.getElementById("weather-error");
  if (!el) return;
  el.textContent = text ?? "";
  el.hidden = !text;
}

// ── editable fields ─────────────────────────────────────────────────

// What each knob means, in one sentence, hung off its label as a
// `title`. Written once here because the same three words (depth,
// duration, ramp) name both a random cloud's range and a fired
// cloud's value, and they have to mean the same thing in both places.
const TIPS = {
  depth: "How much light the cloud blocks at its darkest (%)",
  duration: "Cloud lifetime, fade-in to fade-out (s)",
  ramp: "Fade-in/out time at the cloud's edges (s); the middle holds at full depth. If 2×ramp exceeds duration the cloud is all ramp, and lasts 2×ramp",
  gap: "Average seconds between random clouds (Poisson); 0 = off",
  peak: "Clear-sky maximum at the middle of the day",
  time: "UTC, HH:MM",
  bound: "Each random cloud draws uniformly from lo–hi",
};

// The scalar knobs. `kind` decides both the input type and how the
// typed text becomes a POST value; `key` is the payload field, spelled
// exactly as WeatherPostRequest deserializes it. `sec` is which
// captioned section the row is painted into.
const FIELDS = [
  { id: "weather-sunrise", key: "sunrise", label: "sunrise", kind: "time", sec: "clear", tip: TIPS.time },
  { id: "weather-sunset", key: "sunset", label: "sunset", kind: "time", sec: "clear", tip: TIPS.time },
  { id: "weather-peak-pct", key: "peak_pct", label: "peak %", kind: "num", sec: "clear", tip: TIPS.peak },
  {
    id: "weather-cloud-gap",
    key: "cloud_mean_gap_s",
    label: "gap s",
    kind: "num",
    sec: "clouds",
    tip: TIPS.gap,
    // An empty gap is off, not "unknown" — say so instead of the
    // bare dash every other field uses for "no reading yet".
    placeholder: "off",
  },
];

// The `[lo, hi]` pairs the ambient cloud generator draws from. Either
// half commits the whole pair, since the door takes the range as one
// value — the untouched half rides along at whatever it reads.
const RANGES = [
  {
    key: "cloud_depth_pct",
    label: "depth %",
    lo: "weather-depth-lo",
    hi: "weather-depth-hi",
    tip: TIPS.depth,
  },
  {
    key: "cloud_duration_s",
    label: "duration s",
    lo: "weather-duration-lo",
    hi: "weather-duration-hi",
    tip: TIPS.duration,
  },
  {
    key: "cloud_ramp_s",
    label: "ramp s",
    lo: "weather-ramp-lo",
    hi: "weather-ramp-hi",
    tip: TIPS.ramp,
  },
];

const fieldText = (v) => (v == null ? "" : String(v));

// Write a reading into a field. `dataset.live` is ALWAYS refreshed —
// it is what Esc and blur restore, so freezing it too would revert a
// cancelled edit to a value as stale as focus time. Only the visible
// text is frozen while the field is being edited.
function paintField(inp, text) {
  if (!inp) return;
  inp.dataset.live = text;
  if (inp.dataset.editing) return;
  inp.value = text;
}

// The inspector's edit-in-place contract (inspect.js:515), local to
// this panel because the inspector's is component-bound: focus freezes
// the visible text against the 3 s poll, Enter commits, Esc and blur
// both revert to the last reading. A rejected commit reverts too, so
// nothing the server refused is left sitting in the field looking
// applied.
function wireField(inp, commit) {
  inp.dataset.live = inp.value;
  // The hidden spinner's sibling affordance: in Firefox a wheel over a
  // focused type=number steps it, which is the same uncommitted change
  // an arrow click was — invisible, and reverted by the next poll. Arrow
  // KEYS still step, deliberately: those are keyboard edits, one Enter
  // from a commit. Non-passive so preventDefault holds; number fields
  // only, so a wheel over a text field still scrolls the panel.
  if (inp.type === "number") {
    inp.addEventListener("wheel", (e) => e.preventDefault(), { passive: false });
  }
  inp.addEventListener("focus", () => {
    inp.dataset.editing = "1";
  });
  inp.addEventListener("blur", () => {
    delete inp.dataset.editing;
    inp.value = inp.dataset.live ?? "";
  });
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      commit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      inp.blur();
    }
  });
}

// Commit one scalar field. A malformed time or a non-number is caught
// here rather than round-tripped: the door would reject it anyway, and
// the local check keeps the error next to the field that caused it.
async function commitField(f) {
  const inp = document.getElementById(f.id);
  const text = inp.value.trim();
  if (text === "") return;
  let value;
  if (f.kind === "time") {
    if (hhmmToSecs(text) === null) {
      showError(`${f.label}: expected "HH:MM"`);
      inp.value = inp.dataset.live ?? "";
      return;
    }
    value = text;
  } else {
    value = Number(text);
    if (!Number.isFinite(value)) {
      showError(`${f.label}: expected a number`);
      inp.value = inp.dataset.live ?? "";
      return;
    }
  }
  const res = await postWeather({ [f.key]: value });
  if (!res.ok) {
    showError(errorOf(res));
    inp.value = inp.dataset.live ?? "";
    return;
  }
  showError(null);
  applyLive(res.body);
}

async function commitRange(r) {
  const loEl = document.getElementById(r.lo);
  const hiEl = document.getElementById(r.hi);
  const lo = Number(loEl.value.trim());
  const hi = Number(hiEl.value.trim());
  if (!Number.isFinite(lo) || !Number.isFinite(hi)) {
    showError(`${r.label}: both bounds must be numbers`);
    loEl.value = loEl.dataset.live ?? "";
    hiEl.value = hiEl.dataset.live ?? "";
    return;
  }
  const res = await postWeather({ [r.key]: [lo, hi] });
  if (!res.ok) {
    showError(errorOf(res));
    loEl.value = loEl.dataset.live ?? "";
    hiEl.value = hiEl.dataset.live ?? "";
    return;
  }
  showError(null);
  applyLive(res.body);
}

// ── derived hints and the ghost preview ─────────────────────────────

// The pass-a-cloud row's three inputs, by what they hold.
const PASS_CLOUD = {
  depth: "weather-cloud-depth",
  duration: "weather-cloud-duration",
  ramp: "weather-cloud-ramp",
};

// A field's text as a number. Reading the DOM rather than the last
// payload is deliberate: both callers below want to follow what is
// being typed, before (or without) any commit.
const numIn = (id) => Number(document.getElementById(id)?.value.trim());

// "≈ N clouds overhead on average": how long a cloud lasts over the
// mean gap between clouds is how many of them are up at any one
// moment — the one number the gap and the duration range only mean
// together.
function updateRateHint() {
  const el = document.getElementById("weather-rate-hint");
  if (!el) return;
  const gap = numIn("weather-cloud-gap");
  const meanDuration = (numIn("weather-duration-lo") + numIn("weather-duration-hi")) / 2;
  const overhead = meanDuration / gap;
  const show = Number.isFinite(overhead) && gap > 0;
  el.hidden = !show;
  el.textContent = show ? `≈ ${overhead.toFixed(1)} clouds overhead on average` : "";
}

// The pass-a-cloud fields as a cloud to preview, or null when they
// don't describe one. Silent about bad input — firing is what
// complains; the preview just stays away.
function ghostFromFields() {
  const depth = numIn(PASS_CLOUD.depth);
  const duration = numIn(PASS_CLOUD.duration);
  const ramp = numIn(PASS_CLOUD.ramp);
  if (!Number.isFinite(depth) || !Number.isFinite(duration) || !Number.isFinite(ramp)) return null;
  if (!(depth > 0) || !(duration > 0)) return null;
  return { depth, duration, ramp };
}

const ghostEvent = () => (ghostFocused || ghostEdited ? ghostFromFields() : null);

// Re-plot from the last reading. The ghost's focus / edit / blur
// handlers call this: the sky hasn't changed, only what is drawn on
// top of it.
function redrawCurve() {
  if (!lastWeather || !plot) return;
  plot.setData(daySeries(lastWeather, Date.now(), ghostEvent()));
}

// ── rendering ───────────────────────────────────────────────────────

const fieldInput = (f) =>
  f.kind === "time"
    ? `<input id="${f.id}" class="wfield-input" type="text" placeholder="HH:MM" />`
    : `<input id="${f.id}" class="wfield-input" type="number" step="any" placeholder="${escapeHtml(f.placeholder ?? "—")}" />`;

// One label with its explainer hung off it. The `title` is what a
// hover shows; it is the only place the knobs are spelled out, so
// every label gets one.
const fieldLabel = (forId, text, tip) =>
  `<label for="${forId}" title="${escapeHtml(tip)}">${escapeHtml(text)}</label>`;

const scalarRow = (f) =>
  `<div class="wfield">${fieldLabel(f.id, f.label, f.tip)}
        ${fieldInput(f)}</div>`;

const rangeRow = (r) =>
  `<div class="wfield">${fieldLabel(r.lo, r.label, r.tip)}
        <span class="wfield-pair">
          <input id="${r.lo}" class="wfield-input" type="number" step="any" placeholder="lo" title="${escapeHtml(TIPS.bound)}" />
          <span class="wfield-dash">–</span>
          <input id="${r.hi}" class="wfield-input" type="number" step="any" placeholder="hi" title="${escapeHtml(TIPS.bound)}" />
        </span>
      </div>`;

// What a cloud does to the light, as one picture: time across, how
// much is blocked upwards. The three range knobs under it are the
// three measurements marked on the trapezoid, so the words below
// have something to point at. Static, and drawn in the panel's own
// CSS colours so it follows the theme.
const CLOUD_SKETCH = `<svg class="wsketch" id="weather-cloud-sketch" viewBox="0 0 260 86"
          role="img" aria-label="A cloud's attenuation over time: it ramps up to its
          depth, holds, then ramps back down over its duration.">
          <path class="wsketch-shape" d="M40 60 L78 24 L190 24 L228 60 Z" />
          <line class="wsketch-axis" x1="20" y1="60" x2="242" y2="60" />
          <line class="wsketch-mark" x1="40" y1="14" x2="78" y2="14" />
          <line class="wsketch-mark" x1="40" y1="10" x2="40" y2="18" />
          <line class="wsketch-mark" x1="78" y1="10" x2="78" y2="18" />
          <text x="59" y="7" text-anchor="middle">ramp</text>
          <line class="wsketch-mark" x1="134" y1="24" x2="134" y2="60" />
          <line class="wsketch-mark" x1="130" y1="24" x2="138" y2="24" />
          <line class="wsketch-mark" x1="130" y1="60" x2="138" y2="60" />
          <text x="142" y="45">depth</text>
          <line class="wsketch-mark" x1="40" y1="70" x2="228" y2="70" />
          <line class="wsketch-mark" x1="40" y1="66" x2="40" y2="74" />
          <line class="wsketch-mark" x1="228" y1="66" x2="228" y2="74" />
          <text x="134" y="83" text-anchor="middle">duration</text>
        </svg>`;

// A captioned group of knobs: a quiet header, one line saying what
// the group does, then the rows.
const sectionHtml = (head, caption, rows) => `
      <section class="wsec">
        <h3>${escapeHtml(head)}</h3>
        <p class="hint wsec-cap">${escapeHtml(caption)}</p>
        ${rows}
      </section>`;

// The scalar rows belonging to one section, as one label/value grid.
const scalarRows = (name) => FIELDS.filter((f) => f.sec === name).map(scalarRow).join("");

function liveHtml() {
  const clearSky = sectionHtml(
    "Clear sky",
    "A sine curve between sunrise and sunset, peaking at peak %. Times are UTC.",
    `<div class="wfields">${scalarRows("clear")}</div>`,
  );
  const randomClouds = sectionHtml(
    "Random clouds",
    "On average this many clouds per hour; each draws its depth, duration and ramp from these ranges. 0 or empty = off.",
    `${CLOUD_SKETCH}
        <div class="wfields">${scalarRows("clouds")}
          <p class="hint wrate-hint" id="weather-rate-hint" hidden></p>
          ${RANGES.map(rangeRow).join("")}</div>`,
  );
  const fireCloudSec = sectionHtml(
    "Fire a cloud",
    "One deterministic cloud, right now.",
    `<div class="wcloud-row">
          ${fieldLabel("weather-cloud-depth", "depth %", TIPS.depth)}
          <input id="weather-cloud-depth" class="wfield-input wfield-fire" type="number" step="any" value="60" />
          ${fieldLabel("weather-cloud-duration", "for s", TIPS.duration)}
          <input id="weather-cloud-duration" class="wfield-input wfield-fire" type="number" step="any" value="600" />
          ${fieldLabel("weather-cloud-ramp", "ramp s", TIPS.ramp)}
          <input id="weather-cloud-ramp" class="wfield-input wfield-fire" type="number" step="any" value="60" />
          <button type="button" class="pill" id="weather-cloud-fire">fire</button>
        </div>`,
  );
  return `
    <div class="weather-panel">
      <div class="weather-head">
        <h2>Weather</h2>
        <span class="weather-readout"><span id="weather-pct">—</span><span class="weather-unit">%</span></span>
      </div>
      <div class="wchart" id="weather-chart"></div>
      <p class="hint weather-sub" id="weather-clear-sky">clear sky —</p>
      <p class="hint weather-err" id="weather-error" hidden></p>
      ${clearSky}${randomClouds}${fireCloudSec}
      <section class="wevents">
        <h3>Clouds</h3>
        <ul id="weather-events"><li class="hint">none</li></ul>
      </section>
    </div>`;
}

const EMPTY_HTML = `
  <div class="weather-panel">
    <div class="weather-head"><h2>Weather</h2></div>
    <p class="hint">No weather on this site. Creating it installs the default sky —
      the same one <code>(make-weather)</code> gives, which you can retune here
      afterwards.</p>
    <button type="button" class="pill" id="weather-create">Create weather</button>
    <p class="hint weather-err" id="weather-error" hidden></p>
  </div>`;

const NO_SELECTION_HTML = `
  <div class="weather-panel">
    <div class="weather-head"><h2>Weather</h2></div>
    <p class="hint">Select a microgrid to see its weather.</p>
  </div>`;

// The clouds still overhead. The model keeps an event for about an
// hour after it ends, so lagged inverters can still read the sky they
// were in — but a cloud that has already passed is not news, and a
// list of dead ones buries the live one. The curve keeps using the
// WHOLE list: a cloud that ended at noon still shaped the part of the
// day the curve has already drawn.
function eventsHtml(w) {
  // Against the SERVER's clock, not the browser's: the ends below were
  // stamped by the server, and the two clocks are routinely different
  // machines (a host browser driving a guest VM). A browser running a
  // few seconds ahead would drop a cloud the instant it was fired. The
  // payload's `now` is the same instant the rest of this snapshot was
  // evaluated at; the local clock is only the fallback for a payload
  // that has no readable one.
  const stamped = Date.parse(w.now);
  const now = Number.isFinite(stamped) ? stamped : Date.now();
  const events = (w.events ?? []).filter((e) => {
    const end = Date.parse(e.end);
    // An unreadable end can't be shown to have passed — keep it.
    return !Number.isFinite(end) || end > now;
  });
  if (events.length === 0) return '<li class="hint">none</li>';
  return events
    .map((e) => {
      const start = Date.parse(e.start);
      const end = Date.parse(e.end);
      const span =
        Number.isFinite(start) && Number.isFinite(end)
          ? `${utcHhmm(start)}–${utcHhmm(end)}`
          : "—";
      const depth = Number(e.depth_pct) || 0;
      return `<li><span class="wev-span">${escapeHtml(span)}</span><span class="wev-depth">−${depth.toFixed(0)}%</span></li>`;
    })
    .join("");
}

// Paint the live skeleton once, then wire everything in it. Called
// only on the transition into "live" — every later refresh writes
// values into this DOM instead of replacing it, which is what lets a
// focused field survive the poll.
function paintLiveSkeleton() {
  contentEl.innerHTML = liveHtml();
  // The day chart follows its slot's width (see metrics-panel.js).
  sizeObserver?.disconnect();
  sizeObserver = new ResizeObserver(() => {
    const slot = document.getElementById("weather-chart");
    if (!plot || !slot?.clientWidth || slot.clientWidth === plot.width) return;
    plot.setSize({ width: slot.clientWidth, height: plot.height });
  });
  sizeObserver.observe(contentEl);
  for (const f of FIELDS) {
    wireField(document.getElementById(f.id), () => commitField(f));
  }
  for (const r of RANGES) {
    wireField(document.getElementById(r.lo), () => commitRange(r));
    wireField(document.getElementById(r.hi), () => commitRange(r));
  }
  document.getElementById("weather-cloud-fire").addEventListener("click", fireCloud);
  // The derived "clouds overhead" line follows the keystrokes in the
  // fields it is computed from, not their commits — it is a reading
  // of what you are typing, and nothing is posted for it.
  for (const id of ["weather-cloud-gap", "weather-duration-lo", "weather-duration-hi"]) {
    document.getElementById(id).addEventListener("input", updateRateHint);
  }
  // The ghost preview shows while the pass-a-cloud row is being
  // filled in, and stays after blur if something was changed and not
  // yet fired.
  for (const id of Object.values(PASS_CLOUD)) {
    const el = document.getElementById(id);
    el.addEventListener("focus", () => {
      ghostFocused = true;
      redrawCurve();
    });
    el.addEventListener("blur", () => {
      ghostFocused = false;
      redrawCurve();
    });
    el.addEventListener("input", () => {
      ghostEdited = true;
      redrawCurve();
    });
    // Enter fires, Esc takes the preview back — the two halves of
    // "done with this row". Enter reaches the button these fields are
    // committed by (there is no form here to submit it for us), which
    // is what the rest of the panel's Enter-commit fields have trained
    // the hand to expect. Esc is the other direction: these fields
    // have no committed value to snap back to, so dropping the ghost
    // IS "never mind" — without it an edit pins the preview until
    // something is fired. fireCloud clears both flags on either POST
    // outcome (a validation bail keeps the ghost, deliberately), so
    // Enter needs no cleanup of its own.
    el.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        fireCloud();
        return;
      }
      if (e.key !== "Escape") return;
      e.preventDefault();
      ghostFocused = false;
      ghostEdited = false;
      redrawCurve();
    });
  }
  plot?.destroy();
  plot = null;
}

// Replace the panel body with static markup, dropping any chart.
function paintStatic(html) {
  plot?.destroy();
  plot = null;
  contentEl.innerHTML = html;
}

function paintEmptySkeleton() {
  paintStatic(EMPTY_HTML);
  document.getElementById("weather-create").addEventListener("click", async () => {
    // An empty body is a no-op partial update, which on a site with
    // no weather is exactly "install the defaults" (apply_weather's
    // `unwrap_or_default` branch) — no field list to keep in sync.
    const res = await postWeather({});
    if (!res.ok) {
      showError(errorOf(res));
      return;
    }
    applyLive(res.body);
  });
}

async function fireCloud() {
  const depth = numIn(PASS_CLOUD.depth);
  const duration = numIn(PASS_CLOUD.duration);
  const ramp = numIn(PASS_CLOUD.ramp);
  if (!Number.isFinite(depth) || !Number.isFinite(duration) || !Number.isFinite(ramp)) {
    showError("pass a cloud: depth, duration and ramp must all be numbers");
    return;
  }
  const res = await postWeather({
    pass_cloud: { depth_pct: depth, duration_s: duration, ramp_s: ramp },
  });
  // The fire is over either way — the cloud is in the reading now, or
  // the door refused it. Neither leaves the preview anything to show,
  // so it goes even when the answer was no.
  ghostFocused = false;
  ghostEdited = false;
  if (!res.ok) {
    showError(errorOf(res));
    redrawCurve();
    return;
  }
  showError(null);
  applyLive(res.body);
}

// Take a fresh weather payload as the panel's state and repaint from
// it. Both the poll and every successful POST land here, so the two
// paths can't drift — a POST's response is the same shape as a GET's
// precisely so it can be used this way.
function applyLive(w) {
  if (!w || !contentEl) return;
  if (skeleton !== "live") {
    paintLiveSkeleton();
    skeleton = "live";
  }
  const pct = Number(w.sunlight_pct) || 0;
  const clear = Number(w.clear_sky_pct) || 0;
  document.getElementById("weather-pct").textContent = pct.toFixed(1);
  const sunrise = hhmmToSecs(w.sunrise);
  const sunset = hhmmToSecs(w.sunset);
  const daylight =
    sunrise != null && sunset != null
      ? `${secsToHhmm(sunrise)}–${secsToHhmm(sunset)} UTC`
      : "—";
  document.getElementById("weather-clear-sky").textContent =
    `clear sky ${clear.toFixed(1)}% · daylight ${daylight}`;

  for (const f of FIELDS) paintField(document.getElementById(f.id), fieldText(w[f.key]));
  for (const r of RANGES) {
    const pair = w[r.key] ?? [];
    paintField(document.getElementById(r.lo), fieldText(pair[0]));
    paintField(document.getElementById(r.hi), fieldText(pair[1]));
  }
  document.getElementById("weather-events").innerHTML = eventsHtml(w);
  updateRateHint();

  lastWeather = w;
  const nowMs = Date.now();
  markerHour = nowHours(nowMs);
  const data = daySeries(w, nowMs, ghostEvent());
  const slot = document.getElementById("weather-chart");
  if (plot) plot.setData(data);
  else if (slot) buildChart(slot, data);
}

async function refresh() {
  const res = await getWeather();
  // The panel may have closed while the request was in flight; its
  // teardown already dropped contentEl, and writing into the detached
  // DOM would resurrect a chart nobody can see.
  if (!isPanelOpen(PANEL) || !contentEl) return;
  if (res.noSelection) {
    if (skeleton !== "none") {
      paintStatic(NO_SELECTION_HTML);
      skeleton = "none";
    }
    return;
  }
  if (res.status === 404) {
    if (skeleton !== "empty") {
      paintEmptySkeleton();
      skeleton = "empty";
    }
    return;
  }
  if (!res.ok) {
    showError(errorOf(res));
    return;
  }
  applyLive(res.body);
}

function render(el) {
  contentEl = el;
  skeleton = "";
  el.innerHTML = '<p class="hint">loading weather…</p>';
  refresh();
  // Weather has no WS push — a passing cloud only shows up if the
  // panel asks. The interval lives on the panel's own lifetime, so a
  // closed panel costs nothing (and the headless boot smoke's event
  // loop isn't held open by it).
  pollTimer = setInterval(refresh, POLL_MS);
}

function teardown() {
  clearInterval(pollTimer);
  pollTimer = 0;
  sizeObserver?.disconnect();
  sizeObserver = null;
  plot?.destroy();
  plot = null;
  contentEl = null;
  skeleton = "";
  lastWeather = null;
  ghostFocused = false;
  ghostEdited = false;
}

export function setupWeatherPanel() {
  makeSidePanelToggle(PANEL, render, teardown);
}
