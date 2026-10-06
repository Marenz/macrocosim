// DOM-free contract tests for ui-assets/weather-panel.js.
// Run: node tools/weather-panel-test.mjs   (exits non-zero on failure)
//
// The panel's arithmetic — the day curve and the cloud list — is pure
// over one payload, but the module itself imports three browser-bound
// siblings (routing.js reaches the whole SPA graph). So rather than a
// DOM shim, the three import lines are swapped for local stubs and the
// internals under test are re-exported, and the result is imported as
// a data: URL. Nothing inside the functions is touched: this reads the
// real source, and a rename here fails loudly rather than silently
// testing a copy. The field helpers look their inputs up by id, so a
// stand-in `document` hands them plain objects, and the `mgFetch` stub
// records each request.
import { readFileSync } from "node:fs";

const SRC = new URL("../ui-assets/weather-panel.js", import.meta.url);
const IMPORT_LINE = /^import .*$/gm;
const raw = readFileSync(SRC, "utf8");
const stripped = raw.replace(IMPORT_LINE, "");
const removed = (raw.match(IMPORT_LINE) || []).length;
if (removed !== 3) {
  console.error(
    `weather-panel-test: expected 3 single-line imports to stub, found ${removed} — ` +
      "the stubs below no longer cover what weather-panel.js imports",
  );
  process.exit(1);
}
// A stand-in DOM: just the elements the field helpers look up by id,
// each with the few properties they read and write.
const elements = new Map();
const element = (id) => {
  if (!elements.has(id)) elements.set(id, { value: "", dataset: {}, hidden: false, textContent: "" });
  return elements.get(id);
};
globalThis.document = { getElementById: element };
// Every weather request, and the body the next one answers with.
globalThis.weatherRequests = [];
globalThis.weatherReply = null;
const shimmed = [
  "const mgFetch = async (path, init) => {",
  "  globalThis.weatherRequests.push({ path, init });",
  "  return { ok: true, status: 200, json: async () => globalThis.weatherReply };",
  "};",
  "const requireUplot = () => null;",
  "const isPanelOpen = () => false;",
  "const makeSidePanelToggle = () => {};",
  stripped,
  "export { FIELDS, commitField, daySeries, eventsHtml, fieldText, updateRateHint };",
].join("\n");
const { FIELDS, commitField, daySeries, eventsHtml, fieldText, updateRateHint } = await import(
  `data:text/javascript;base64,${Buffer.from(shimmed).toString("base64")}`
);

let failures = 0;
function check(name, cond, detail) {
  if (!cond) {
    failures++;
    console.error(`FAIL ${name}${detail ? `: ${detail}` : ""}`);
  }
}
const near = (got, want, eps = 1e-6) => Math.abs(got - want) <= eps * Math.max(1, Math.abs(want));

// One UTC day, daylight all of it so every sample below sits under a
// bright sky, and a zero ambient ramp so each cloud is a clean
// rectangle (the curve's ramp estimate is read off `cloud_ramp_s`).
const DAY = "2026-01-02";
const at = (hhmm) => `${DAY}T${hhmm}:00Z`;
const NOW = at("12:00");
const PAYLOAD = {
  sunrise: "00:00",
  sunset: "23:59",
  peak_pct: 100,
  cloud_mean_gap_s: null,
  cloud_depth_pct: [0, 0],
  cloud_duration_s: [0, 0],
  cloud_ramp_s: [0, 0],
  sunlight_pct: 49,
  clear_sky_pct: 98,
  // Server-stamped, by the same clock as the event ends below.
  now: NOW,
  events: [
    // Over by `now`: gone from the list, still in the curve.
    { start: at("10:00"), end: at("11:00"), depth_pct: 80 },
    // Overhead at `now`.
    { start: at("11:50"), end: at("12:30"), depth_pct: 50 },
  ],
};

// ── the split: the list drops a passed cloud, the curve keeps it ─────

{
  const html = eventsHtml(PAYLOAD);
  const rows = (html.match(/<li/g) || []).length;
  // The whole point of filtering against the payload's `now` rather
  // than Date.now(): this test runs at a wall-clock time nowhere near
  // the fixture's day, so a browser-clock filter would call BOTH
  // clouds expired (or, on a machine set before 2026, neither).
  check("events: exactly one row survives the filter", rows === 1, html);
  check("events: the live cloud is listed", html.includes("11:50–12:30"), html);
  check("events: the passed cloud is not", !html.includes("10:00–11:00"), html);
  check("events: the live cloud's depth", html.includes("−50%"), html);
  check("events: not the empty placeholder", !html.includes("none"), html);
}

{
  const nowMs = Date.parse(NOW);
  const [xs, clear, atten] = daySeries(PAYLOAD, nowMs);
  const idx = (hours) => xs.indexOf(hours);
  const cloudless = idx(8);
  const passed = idx(10.5);
  const overhead = idx(12);
  check("curve: the 10-minute grid holds the sampled hours",
    cloudless >= 0 && passed >= 0 && overhead >= 0,
    `${cloudless} ${passed} ${overhead}`);
  check("curve: a cloudless hour is unattenuated",
    near(atten[cloudless], clear[cloudless]),
    `${atten[cloudless]} vs ${clear[cloudless]}`);
  // The filtered-out cloud still shaped the day the curve draws.
  check("curve: the passed cloud still darkens its own hour",
    near(atten[passed], clear[passed] * 0.2),
    `${atten[passed]} vs ${clear[passed] * 0.2}`);
  check("curve: the live cloud darkens the now-hour",
    near(atten[overhead], clear[overhead] * 0.5),
    `${atten[overhead]} vs ${clear[overhead] * 0.5}`);
}

// ── the ghost preview spans what pass_cloud would actually build ─────

{
  const nowMs = Date.parse(NOW);
  const clearDay = { ...PAYLOAD, events: [] };
  // The span the ghost is drawn over, in seconds: the preview series
  // is null everywhere outside the previewed cloud.
  const ghostSpanS = (ghost) => {
    const [xs, , , preview] = daySeries(clearDay, nowMs, ghost);
    const inside = xs.filter((_h, i) => preview[i] != null);
    return (inside[inside.length - 1] - inside[0]) * 3600;
  };
  check("ghost: a plateaued cloud spans its duration",
    near(ghostSpanS({ depth: 100, duration: 3600, ramp: 600 }), 3600),
    String(ghostSpanS({ depth: 100, duration: 3600, ramp: 600 })));
  // `Weather::pass_cloud` keeps ramp_in = ramp_out = ramp whole and
  // saturates only the plateau, so 2*ramp past the duration the cloud
  // is two ramps back to back and OUTLASTS what was typed.
  check("ghost: 2×ramp past the duration spans 2×ramp",
    near(ghostSpanS({ depth: 100, duration: 600, ramp: 900 }), 1800),
    String(ghostSpanS({ depth: 100, duration: 600, ramp: 900 })));
  // …and it still reaches full depth, at the apex where the two ramps
  // meet — a compressed-ramp preview would put the apex elsewhere.
  const [xs, , , preview] = daySeries(clearDay, nowMs, { depth: 100, duration: 600, ramp: 900 });
  const apex = xs.indexOf(12.25);
  check("ghost: the apex of an all-ramp cloud is full depth",
    apex >= 0 && near(preview[apex], 0),
    `${apex} ${preview[apex]}`);
}

// ── the cloud gap field: shown and sent as cloud_mean_gap_s ─────────

{
  const gap = FIELDS.find((f) => f.key === "cloud_mean_gap_s");
  check("gap: a field reads and writes cloud_mean_gap_s", gap != null);
  check("gap: the reading is shown as given", fieldText(13) === "13", fieldText(13));
  check("gap: no ambient clouds shows the off placeholder",
    fieldText(null) === "" && gap.placeholder === "off",
    `${fieldText(null)} ${gap.placeholder}`);
  element(gap.id).value = "13";
  globalThis.weatherRequests.length = 0;
  globalThis.weatherReply = { ...PAYLOAD, cloud_mean_gap_s: 13 };
  await commitField(gap);
  const sent = globalThis.weatherRequests.map((r) => JSON.parse(r.init.body));
  check("gap: a commit posts the typed gap under cloud_mean_gap_s",
    sent.length === 1 && sent[0].cloud_mean_gap_s === 13 && Object.keys(sent[0]).length === 1,
    JSON.stringify(sent));
}

// ── the "clouds overhead" hint: mean duration over the gap ──────────

{
  const hint = element("weather-rate-hint");
  element("weather-duration-lo").value = "100";
  element("weather-duration-hi").value = "300";
  element("weather-cloud-gap").value = "100";
  updateRateHint();
  check("hint: mean duration / gap clouds overhead",
    !hint.hidden && hint.textContent === "≈ 2.0 clouds overhead on average",
    `${hint.hidden} ${hint.textContent}`);
  element("weather-cloud-gap").value = "0";
  updateRateHint();
  check("hint: hidden when the clouds are off", hint.hidden && hint.textContent === "",
    `${hint.hidden} ${hint.textContent}`);
}

if (failures) {
  console.error(`${failures} failure(s)`);
  process.exit(1);
}
console.log("weather-panel: all tests passed");
