// The metrics panel's sample store: one 900-slot × 1 Hz ring per
// microgrid_sample stream (15 min — the server history cap), plus
// the latest sample per stream and a subscriber list the panel
// renderer hangs off. DOM-free: the renderer (metrics-panel.js)
// subscribes and reads; nothing here touches elements.
//
// Slots are not a push cursor: a sample lands in the slot its own
// server second maps to (`second % 900`), and the slot remembers that
// second. So every stream shares one absolute slot ↔ second mapping,
// and reading two streams over the same second window returns
// positionally aligned rows by construction — no cross-stream
// realignment pass, and a stream that stalls grows a trailing gap
// where a cursor ring would have slid its old trace rightward under
// uPlot's clock axis. Samples arriving without a server timestamp are
// dropped rather than stamped with the browser clock (see
// applySample); the two clocks are unrelated and mixing them threw
// the x-axis years off.

import { formatScaled } from "./live.js";
import { mgFetch } from "./routing.js";

const SPARK_LEN = 900;

// Readout for a raw sample, which arrives as a quantity + a wire unit
// rather than the display unit live.js's helpers take. This is the
// adapter over that difference: a power-family reading delegates to
// formatScaled for the W → kW → MW ladder, so the two surfaces scale
// identically by construction; every other quantity skips the ladder
// and keeps two decimals on its unit as-is.
export function fmtValue(quantity, unit, value) {
  if (value == null || !Number.isFinite(value)) return "—";
  if (quantity === "Power" || quantity === "ReactivePower" || unit === "W" || unit === "VAr") {
    return formatScaled(value, unit);
  }
  return `${value.toFixed(2)} ${unit}`;
}

// Power factor from matching P and Q samples: |P| / hypot(P, Q).
// null when either side is missing or both are zero (PF undefined).
export function pfValue(p, q) {
  if (!Number.isFinite(p) || !Number.isFinite(q) || (p === 0 && q === 0)) return null;
  return Math.abs(p) / Math.hypot(p, q);
}

// Chip readout. Sign convention as the old site-PF tile: opposite
// signs on P and Q read as leading, same as lagging, and the
// qualifier drops once PF rounds to unity (>= 0.995) so a clean
// reading doesn't flicker between the two on noise.
export function pfText(p, q) {
  const pf = pfValue(p, q);
  if (pf == null) return "PF —";
  const tag = pf >= 0.995 ? "" : p * q < 0 ? " lead" : " lag";
  return `PF ${pf.toFixed(2)}${tag}`;
}

export const metricsStore = (() => {
  const sparkBuf = new Map(); // stream -> { values, sec, maxSec }
  const latestMap = new Map(); // stream -> { value, quantity, unit }
  const listeners = new Set();
  let reseedTimer = null;
  let reseedVisHandler = null;

  function buf(stream) {
    let b = sparkBuf.get(stream);
    if (!b) {
      b = {
        values: new Float32Array(SPARK_LEN).fill(NaN),
        // The absolute server second each slot currently holds,
        // parallel to `values`. NaN marks a slot no sample has landed
        // in; a slot the ring has since wrapped past keeps its old
        // second, which reads as a miss for every other second.
        sec: new Float64Array(SPARK_LEN).fill(NaN),
        maxSec: null,
      };
      sparkBuf.set(stream, b);
    }
    return b;
  }
  // Absolute-second placement, shared by the live and backfill paths.
  // Returns false for a sample with no usable server timestamp — see
  // the header: there is no browser-clock fallback.
  function place(b, tsMs, value) {
    if (!Number.isFinite(tsMs)) return false;
    const s = Math.floor(tsMs / 1000);
    const i = ((s % SPARK_LEN) + SPARK_LEN) % SPARK_LEN;
    b.values[i] = value ?? Number.NaN;
    b.sec[i] = s;
    b.maxSec = b.maxSec === null ? s : Math.max(b.maxSec, s);
    return true;
  }
  function notify() {
    for (const cb of listeners) cb();
  }

  return {
    subscribe(cb) {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    latest(stream) {
      return latestMap.get(stream) ?? null;
    },
    // The newest second this stream holds, or null while its ring is
    // empty. `latestSecond` below folds this across streams; charts
    // want the fold, not this.
    maxSecond(stream) {
      return sparkBuf.get(stream)?.maxSec ?? null;
    },
    // One second per x, oldest first, over the `windowS` seconds
    // ending at `endSec` — or at this stream's own newest second when
    // the caller passes none. xs are in SECONDS (uPlot's time scale
    // reads seconds, and it binary-searches them, so they have to
    // come out sorted — here they do by construction: a straight
    // count of consecutive seconds). A second whose slot holds some
    // other second's sample, or no sample, comes back as a null y,
    // which uPlot renders as a gap; that is what a stalled or
    // frame-dropping stream looks like, and it stays put under the
    // clock axis instead of sliding. Passing one shared `endSec`
    // across streams is what makes column k of every series mean the
    // same second. An empty ring has no anchor and returns nothing
    // for the caller to pad or skip.
    series(stream, windowS, endSec = null) {
      const b = sparkBuf.get(stream);
      if (!b || b.maxSec === null) return { xs: [], ys: [] };
      const end = endSec ?? b.maxSec;
      const n = Math.min(windowS, SPARK_LEN);
      const xs = new Array(n);
      const ys = new Array(n);
      for (let k = 0; k < n; k++) {
        const s = end - (n - 1 - k);
        const i = ((s % SPARK_LEN) + SPARK_LEN) % SPARK_LEN;
        xs[k] = s;
        ys[k] = b.sec[i] === s && !Number.isNaN(b.values[i]) ? b.values[i] : null;
      }
      return { xs, ys };
    },
    applySample(ev) {
      // A sample with no server timestamp has no home second, and
      // stamping it with Date.now() would file browser time into a
      // server-time ring — an offset clock filed samples years out of
      // the window. Drop it; the server stamps every Sample frame it
      // sends, and reseedLatest still refreshes the chips.
      if (!place(buf(ev.stream), ev.ts_ms, ev.value)) return;
      latestMap.set(ev.stream, {
        value: ev.value ?? null,
        quantity: ev.quantity,
        unit: ev.unit,
      });
      notify();
    },
    async backfill() {
      // Past 15 min per stream, server-side, so charts show the
      // trend immediately on panel open instead of growing from
      // empty. Best-effort: a 503 mid-rebuild leaves the old rings;
      // WS frames fill forward from here.
      try {
        const hres = await mgFetch("metrics/history");
        if (hres?.ok) {
          const hmap = await hres.json();
          for (const [stream, samples] of Object.entries(hmap)) {
            const b = buf(stream);
            b.values.fill(Number.NaN);
            b.sec.fill(Number.NaN);
            b.maxSec = null;
            // Each history sample carries its own ts_ms, so it lands
            // in the same slot a live frame for that second would —
            // no right-alignment, and a later WS frame for a second
            // already backfilled simply overwrites it in place.
            for (const smp of samples.slice(-SPARK_LEN)) place(b, smp?.ts_ms, smp?.value);
          }
        }
      } catch (_) {
        // Loopback not up yet — the reseed below may still land.
      }
      await this.reseedLatest();
      notify();
    },
    // Value-only refresh from the server's cached latest sample —
    // the WS Sample stream drops frames on lag and a backgrounded
    // tab throttles its receiver, so chips could otherwise freeze on
    // a stale number. No ring push: the ring stays aligned to the
    // WS/backfill sample flow.
    async reseedLatest() {
      try {
        const res = await mgFetch("metrics/latest");
        if (!res?.ok) return;
        const map = await res.json();
        for (const [stream, snap] of Object.entries(map)) {
          latestMap.set(stream, {
            value: snap.value ?? null,
            quantity: snap.quantity,
            unit: snap.unit,
          });
        }
        notify();
      } catch (_) {
        // Best-effort.
      }
    },
    // Safety net against dropped WS frames while the consumer is on
    // screen: slow-poll the latest snapshot, and refresh immediately
    // when the tab returns to the foreground. `isOpen` says whether
    // the consumer is on screen; the store polls only while it is.
    // Idempotent — a second call replaces the timer instead of
    // stacking one.
    startAutoReseed(isOpen, periodMs = 5000) {
      this.stopAutoReseed();
      reseedTimer = setInterval(() => {
        if (isOpen()) this.reseedLatest();
      }, periodMs);
      reseedVisHandler = () => {
        if (!document.hidden && isOpen()) this.reseedLatest();
      };
      document.addEventListener("visibilitychange", reseedVisHandler);
    },
    stopAutoReseed() {
      if (reseedTimer !== null) {
        clearInterval(reseedTimer);
        reseedTimer = null;
      }
      if (reseedVisHandler !== null) {
        document.removeEventListener("visibilitychange", reseedVisHandler);
        reseedVisHandler = null;
      }
    },
  };
})();

// The one anchor second a multi-stream chart should read every column
// at: the newest second any of `streams` holds, or null when none of
// them holds a sample at all (nothing to draw). Feeding it back into
// series(stream, windowS, end) is what keeps a stalled stream's last
// point under the same x as its live sibling's, instead of each
// series ending at its own newest second.
//
// A plain max, with no outlier policy of its own: the CALLER chooses
// which streams may steer the anchor, and that choice is the whole
// policy (metrics-panel.js hands over its visible traces only, never
// its annotation streams). Nothing here tries to spot a clock jump —
// a guard that dropped a stream sitting far beyond its siblings both
// mis-fired on a legitimately stalled sibling and lost to a jump that
// hit several streams at once. maxSecond is monotonic with no reset
// path, so a future-stamped sample leaves that stream's anchor
// pinned ahead of its siblings permanently, until a reload clears it.
export function latestSecond(streams) {
  let max = null;
  for (const s of streams) {
    const m = metricsStore.maxSecond(s);
    if (m !== null && (max === null || m > max)) max = m;
  }
  return max;
}
