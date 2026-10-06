// Unit tests for the metrics store's pure surface: ring push/read
// windowing, PF derivation, and display formatting. DOM/fetch shims
// are inert — the tested paths never call them.
//
// metrics-store.js imports routing.js for mgFetch, and routing.js
// imports app.js — so this pulls in the same whole-graph-plus-init()
// load as tools/boot-smoke.mjs, not just metrics-store.js's own
// surface. Same Proxy-stub shim as boot-smoke, for the same reason:
// every property access answers with another callable stub, enough
// for the DOM/browser API surface the graph touches at import time.
import assert from "node:assert/strict";

function noop() {}
const stub = () =>
  new Proxy(noop, {
    get(_t, p) {
      if (p === Symbol.toPrimitive || p === "toString") return () => "";
      if (p === "then") return undefined;
      return stub();
    },
    apply: () => stub(),
    construct: () => stub(),
  });

globalThis.document = stub();
globalThis.window = globalThis;
Object.defineProperty(globalThis, "navigator", { value: stub(), configurable: true });
globalThis.localStorage = stub();
globalThis.location = { hash: "", pathname: "/", search: "" };
globalThis.history = stub();
globalThis.WebSocket = stub();
globalThis.requestAnimationFrame = () => 0;
globalThis.addEventListener = () => {};
globalThis.getComputedStyle = stub();
globalThis.CSS = stub();
globalThis.ResizeObserver = stub();
globalThis.MutationObserver = stub();
globalThis.fetch = () => new Promise(() => {});

// app.js and routing.js import each other (app.js for its own
// exports, routing.js back for dispatchesPanel/setStatus). Entering
// the graph via metrics-store.js -> routing.js -> app.js — instead
// of boot-smoke's app.js -> ... -> routing.js — flips which side of
// that cycle finishes its own top-level body first, and routing.js's
// init()-triggered setupModeToggle() ran before routing.js reached
// its own `const MODE_KEY` line: a real TDZ, but an artifact of this
// test's entry point, not of production (index.html always enters
// through app.js). Warm the cache with app.js first, same entry
// boot-smoke uses, so the cycle resolves in the same safe order.
await import(new URL("../ui-assets/app.js", import.meta.url));

const { metricsStore, latestSecond, fmtValue, pfValue, pfText } = await import(
  new URL("../ui-assets/metrics-store.js", import.meta.url)
);

// ── pf helpers ──────────────────────────────────────────────────
assert.equal(pfValue(null, 5), null);
assert.equal(pfValue(0, 0), null);
assert.ok(Math.abs(pfValue(100, 0) - 1) < 1e-9);
assert.ok(Math.abs(pfValue(30, 40) - 0.6) < 1e-9);
// Same signs → lagging, opposite → leading, unity (>= 0.995) drops
// the qualifier — the site-PF rule the old dashboard tile used.
assert.equal(pfText(30, 40), "PF 0.60 lag");
assert.equal(pfText(30, -40), "PF 0.60 lead");
assert.equal(pfText(100, 1), "PF 1.00");
assert.equal(pfText(null, 40), "PF —");
// Leading with the P sign flipped, and either side missing/NaN reads
// as undefined PF rather than a sign artifact.
assert.equal(pfText(-8000, 6000), "PF 0.80 lead");
assert.equal(pfText(8000, null), "PF —");
assert.equal(pfText(Number.NaN, 6000), "PF —");

// ── fmtValue ────────────────────────────────────────────────────
// Every branch of the readout is pinned here, because the power
// branch delegates its ladder to live.js and the two have to keep
// producing the same strings: the M and k rungs and their exact
// boundaries, the sub-kilo rung's single decimal, the "VAr"
// spelling on each rung, and the two decimals the non-power branch
// uses instead of the ladder.
assert.equal(fmtValue("Power", "W", 1_234_000), "1.23 MW");
assert.equal(fmtValue("ReactivePower", "VAr", -1234), "-1.23 kVAr");
assert.equal(fmtValue("ReactivePower", "VAr", 1_500_000), "1.50 MVAr");
assert.equal(fmtValue("ReactivePower", "VAr", 500), "500.0 VAr");
assert.equal(fmtValue("Power", "W", 999.94), "999.9 W");
assert.equal(fmtValue("Power", "W", 0), "0.0 W");
assert.equal(fmtValue("Power", "W", -0.04), "-0.0 W");
// The rungs switch at exactly 1e3 and 1e6, not above them.
assert.equal(fmtValue("Power", "W", 1000), "1.00 kW");
assert.equal(fmtValue("Power", "W", 1_000_000), "1.00 MW");
// The ladder is reached by quantity OR by wire unit, independently:
// a power quantity carrying an already-scaled unit still scales, and
// a non-power quantity carrying "VAr" still scales.
assert.equal(fmtValue("Power", "kW", 5), "5.0 kW");
assert.equal(fmtValue("ApparentPower", "VAr", 2500), "2.50 kVAr");
// Non-power quantities skip the ladder entirely: two decimals and
// the raw unit, however large the number.
assert.equal(fmtValue("Frequency", "Hz", 50.0171), "50.02 Hz");
assert.equal(fmtValue("Soc", "%", 83.456), "83.46 %");
assert.equal(fmtValue("Frequency", "Hz", 1_234_567), "1234567.00 Hz");
// Missing and non-finite readings are a dash on both branches.
assert.equal(fmtValue("Power", "W", null), "—");
assert.equal(fmtValue("Power", "W", undefined), "—");
assert.equal(fmtValue("Power", "W", Number.POSITIVE_INFINITY), "—");
assert.equal(fmtValue("Frequency", "Hz", Number.NaN), "—");

// ── ring + series windowing ─────────────────────────────────────
// The ring is keyed by each sample's own server second (second % 900),
// not by a push cursor, so a window is a straight count of consecutive
// seconds ending at an anchor: xs are always 1 s apart and absolute,
// and a second with no sample reads as a null y rather than being
// compacted away.
const t0 = 1_700_000_000_000;
const sec0 = t0 / 1000;
for (let i = 0; i < 10; i++) {
  metricsStore.applySample({
    stream: "grid_power",
    quantity: "Power",
    unit: "W",
    ts_ms: t0 + i * 1000,
    value: i,
  });
}
assert.deepEqual(metricsStore.latest("grid_power"), {
  value: 9,
  quantity: "Power",
  unit: "W",
});
const s5 = metricsStore.series("grid_power", 5);
assert.equal(s5.xs.length, 5);
assert.equal(s5.ys.length, 5);
// Newest-last, values 5..9; older seconds fell outside the window.
assert.deepEqual(s5.ys, [5, 6, 7, 8, 9]);
// xs are the absolute seconds those samples carried: 1 s apart,
// increasing, and ending on the newest second stored.
assert.deepEqual(s5.xs, [sec0 + 5, sec0 + 6, sec0 + 7, sec0 + 8, sec0 + 9]);
for (let i = 1; i < s5.xs.length; i++) {
  assert.equal(s5.xs[i] - s5.xs[i - 1], 1);
}
// A null-value sample still claims its second, and reads as a gap.
metricsStore.applySample({
  stream: "grid_power",
  quantity: "Power",
  unit: "W",
  ts_ms: t0 + 10_000,
  value: null,
});
const s3 = metricsStore.series("grid_power", 3);
assert.deepEqual(s3.ys, [8, 9, null]);
// A window wider than the stream's history pads the front with
// nulls, and every x is still finite and 1 s-spaced — uPlot binary-
// searches xs, so they have to be sorted and gap-free.
const wide = metricsStore.series("grid_power", 20);
assert.equal(wide.ys.length, 20);
assert.equal(wide.ys[0], null);
for (let i = 1; i < wide.xs.length; i++) {
  assert.ok(Number.isFinite(wide.xs[i]));
  assert.equal(wide.xs[i] - wide.xs[i - 1], 1);
}

// ── a wrapped slot is rejected by the second it remembers ────────
// The ring is 900 slots wide, so s and s + 900 map to the same slot.
// Anchoring a window a full ring later must not read the old sample
// back as a fresh one: sec[i] still says s, so the column is null.
metricsStore.applySample({
  stream: "wrap_stream",
  quantity: "Power",
  unit: "W",
  ts_ms: t0,
  value: 7,
});
const wrapped = metricsStore.series("wrap_stream", 3, sec0 + 900);
assert.deepEqual(wrapped.xs, [sec0 + 898, sec0 + 899, sec0 + 900]);
assert.deepEqual(wrapped.ys, [null, null, null]);

// ── a stall is a hole in ys, never a compacted x step ────────────
// Two samples 4 s apart leave the three seconds between them empty:
// they come back as null ys at their own xs, so the trace draws the
// stall as a gap in place instead of as a continuous 1 Hz line.
for (const [i, ts] of [t0, t0 + 4000].entries()) {
  metricsStore.applySample({
    stream: "gap_stream",
    quantity: "Power",
    unit: "W",
    ts_ms: ts,
    value: i,
  });
}
const gap = metricsStore.series("gap_stream", 5);
assert.deepEqual(gap.ys, [0, null, null, null, 1]);
assert.deepEqual(gap.xs, [sec0, sec0 + 1, sec0 + 2, sec0 + 3, sec0 + 4]);

// ── one anchor keeps two streams positionally aligned ────────────
// Same seconds fed to both, different frames dropped on each. Read at
// a shared endSec, column k of one is the same second as column k of
// the other — which is what a multi-series chart needs, and what
// per-stream anchoring broke.
for (const s of [0, 1, 2, 4]) {
  metricsStore.applySample({
    stream: "align_a",
    quantity: "Power",
    unit: "W",
    ts_ms: t0 + s * 1000,
    value: 10 + s,
  });
}
for (const s of [0, 3, 4]) {
  metricsStore.applySample({
    stream: "align_b",
    quantity: "Power",
    unit: "W",
    ts_ms: t0 + s * 1000,
    value: 20 + s,
  });
}
const shared = latestSecond(["align_a", "align_b"]);
assert.equal(shared, sec0 + 4);
const a = metricsStore.series("align_a", 5, shared);
const b = metricsStore.series("align_b", 5, shared);
assert.deepEqual(a.xs, b.xs);
assert.deepEqual(a.ys, [10, 11, 12, null, 14]);
assert.deepEqual(b.ys, [20, null, null, 23, 24]);
// A stream that stopped two seconds ago keeps its trailing gap under
// the shared anchor instead of re-anchoring its last point to it.
const stalled = metricsStore.series("align_a", 5, shared + 2);
assert.deepEqual(stalled.xs, [sec0 + 2, sec0 + 3, sec0 + 4, sec0 + 5, sec0 + 6]);
assert.deepEqual(stalled.ys, [12, null, 14, null, null]);

// ── the fold is a plain max over the streams it is handed ────────
// No outlier policy in the store: whichever stream holds the newest
// second wins, in any argument order, including one stamped an hour
// ahead. Any stream in the fold can therefore take the window with
// it — read at that anchor, a sibling whose samples are all a ring
// behind comes back all-null. That is not a bug to be guessed at
// here; it is why the CALLER picks the fold's inputs (metrics-panel
// folds its visible traces only, and reads bounds/PF partners at the
// result). A guess got it wrong both ways: it inverted onto a merely
// stalled sibling, and a clock jump that moved several streams at
// once slipped past it anyway.
metricsStore.applySample({
  stream: "jump_b",
  quantity: "Power",
  unit: "W",
  ts_ms: t0 + 3_600_000,
  value: 999,
});
assert.equal(latestSecond(["align_a", "jump_b"]), sec0 + 3600);
assert.equal(latestSecond(["jump_b", "align_a"]), sec0 + 3600);
assert.equal(latestSecond(["jump_b"]), sec0 + 3600);
assert.equal(latestSecond(["align_a"]), sec0 + 4);
assert.deepEqual(metricsStore.series("align_a", 5, latestSecond(["align_a", "jump_b"])).ys, [
  null,
  null,
  null,
  null,
  null,
]);

// ── narrowing the fold is the whole policy (the panel's rule) ────
// The panel's shape, at store level: fold ONLY the live stream, then
// read both streams at that one anchor. The stalled stream — a
// change-only bounds stream on an idle site, or a trace that stopped
// — keeps its place in the data array as trailing nulls under the
// live edge, and the live stream still reads its full window. Under
// the old policy the stalled stream could win the fold instead, and
// then it was the live trace that came back blank.
for (let i = 0; i < 5; i++) {
  metricsStore.applySample({
    stream: "fold_live",
    quantity: "Power",
    unit: "W",
    ts_ms: t0 + i * 1000,
    value: i,
  });
}
for (const [i, s] of [0, 1].entries()) {
  metricsStore.applySample({
    stream: "fold_stalled",
    quantity: "Power",
    unit: "W",
    ts_ms: t0 + s * 1000,
    value: 100 + i,
  });
}
const foldAnchor = latestSecond(["fold_live"]);
assert.equal(foldAnchor, sec0 + 4);
const liveRow = metricsStore.series("fold_live", 5, foldAnchor);
const stalledRow = metricsStore.series("fold_stalled", 5, foldAnchor);
assert.deepEqual(liveRow.xs, stalledRow.xs);
assert.deepEqual(liveRow.ys, [0, 1, 2, 3, 4]);
assert.deepEqual(stalledRow.ys, [100, 101, null, null, null]);

// ── empty ring, and samples with no server timestamp ─────────────
assert.deepEqual(metricsStore.series("never_sampled", 5), { xs: [], ys: [] });
assert.equal(latestSecond(["never_sampled"]), null);
assert.equal(latestSecond([]), null);
// No ts_ms means no home second: the sample is dropped rather than
// stamped with the browser clock, which used to file browser time
// into a server-time ring and throw x years out of the window.
metricsStore.applySample({
  stream: "no_ts",
  quantity: "Power",
  unit: "W",
  value: 5,
});
assert.deepEqual(metricsStore.series("no_ts", 5), { xs: [], ys: [] });
assert.equal(metricsStore.latest("no_ts"), null);

// ── subscribe ───────────────────────────────────────────────────
let fired = 0;
const un = metricsStore.subscribe(() => {
  fired++;
});
metricsStore.applySample({
  stream: "pv_power",
  quantity: "Power",
  unit: "W",
  ts_ms: t0,
  value: 1,
});
assert.equal(fired, 1);
un();
metricsStore.applySample({
  stream: "pv_power",
  quantity: "Power",
  unit: "W",
  ts_ms: t0 + 1000,
  value: 2,
});
assert.equal(fired, 1);

console.log("metrics-store-test: all assertions passed");
