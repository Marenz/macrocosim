// Chrome around the SPA's main views: the configurable-zone clock
// and the always-on pulse bar.

import { mgFetch, setupDensityToggle } from "./routing.js";

// ─── Clock + TZ toggle ─────────────────────────────────────────────────────
//
// macrocosim's physics + gRPC boundary speak UTC. The UI displays
// timestamps in either UTC or the IANA zone the operator set via
// (set-timezone …) — defaulting to "Europe/Berlin" matching the
// configured demo target. clockState pulls the zone name once at
// boot via /api/clock; the TZ chip in the pulse bar flips between
// the local-zone short label (CET / CEST / EST / etc., picked via
// Intl) and "UTC". Persists in localStorage.
const TZ_PREF_KEY = "macrocosim-tz";
export const clockState = (() => {
  let simTz = "Europe/Berlin";
  let simLabel = "local";
  let mode = "local"; // "local" or "utc"
  function probeShortLabel(tz) {
    // Try `short` (CEST / EST) and `shortGeneric` (CET / EST) in
    // sequence, preferring a compact 3-4-char abbreviation. Some
    // browser/CLDR combinations return offset notation ("GMT+2")
    // or wordy generics ("Germany Time"); both are uglier than
    // the IANA city segment for chip display, so fall back to
    // that whenever the probe is offset-y or multi-word.
    for (const kind of ["short", "shortGeneric"]) {
      try {
        const parts = new Intl.DateTimeFormat("en-US", {
          timeZone: tz,
          timeZoneName: kind,
        }).formatToParts(new Date());
        const tag = parts.find((p) => p.type === "timeZoneName");
        if (tag && !/^GMT[+-]/i.test(tag.value) && !/\s/.test(tag.value)) {
          return tag.value;
        }
      } catch (_) {
        /* try next */
      }
    }
    const seg = tz.split("/").pop();
    return seg ? seg.replace(/_/g, " ") : tz;
  }
  function timeZoneInUse() {
    return mode === "utc" ? "UTC" : simTz;
  }
  function updateChip() {
    const chip = document.getElementById("tz-toggle");
    if (!chip) return;
    chip.textContent = mode === "utc" ? "UTC" : simLabel.toLowerCase();
    chip.classList.toggle("active", mode === "utc");
  }
  function applyMode(next) {
    mode = next === "utc" ? "utc" : "local";
    updateChip();
  }
  return {
    async init() {
      try {
        const res = await fetch("/api/clock");
        if (res.ok) {
          const j = await res.json();
          if (j.tz) simTz = j.tz;
        }
      } catch (_) {
        // Keep the default; the chip label will show "local" + the
        // browser's local zone short. Not ideal but harmless.
      }
      simLabel = probeShortLabel(simTz);
      applyMode(localStorage.getItem(TZ_PREF_KEY) || "local");
      const chip = document.getElementById("tz-toggle");
      if (chip) {
        chip.addEventListener("click", () => {
          const next = mode === "utc" ? "local" : "utc";
          localStorage.setItem(TZ_PREF_KEY, next);
          applyMode(next);
          renderClockNow();
        });
      }
    },
    formatNow() {
      const d = new Date();
      try {
        return d.toLocaleTimeString("en-GB", {
          hour: "2-digit",
          minute: "2-digit",
          second: "2-digit",
          hour12: false,
          timeZone: timeZoneInUse(),
        });
      } catch (_) {
        return d.toTimeString().slice(0, 8);
      }
    },
    tzInUse() {
      return timeZoneInUse();
    },
  };
})();

function renderClockNow() {
  const el = document.getElementById("pulse-clock");
  if (el) el.textContent = clockState.formatNow();
}
// ─── Pulse bar ─────────────────────────────────────────────────────────────
//
// Always-on system pulse strip. The live sources:
//   - Setpoint sparkbar: rate of /ws/events kind="setpoint" frames,
//     bucketed into 12 × 5 s windows over the last minute.
//   - Health pill: rolling counters from the topology route's health
//     field — recomputed every refreshTopology() call (WS push on
//     topology_changed already drives this).
//   - Graph pill: the topology route's graph_status — ✓ when the
//     component-graph validator accepted the topology, ⚠ (with the
//     error on click) when it rejected.
//   - Loopback pill: metrics/status polled every 5 s. ✓ when
//     connected, ⚠ when still booting.
//   - Wall clock at the right edge, ticked every second.
//
// All panels are read-only and tolerant of partial data — a
// page loaded before the loopback comes up shows ⚠ and flips to
// ✓ on the next poll. Mirrors tradingsim's `.pulse` shape so the
// developer sees the same "is the sim alive" pattern across both
// simulators.
export const pulseBar = (() => {
  const SPARK_BUCKETS = 12;
  const BUCKET_MS = 5000;
  const buckets = new Array(SPARK_BUCKETS).fill(0);
  let lastSpan = pulseBucketIndex();
  function pulseBucketIndex() {
    // Floor of (now / BUCKET_MS) — when this rolls forward, every
    // bucket between lastSpan and now shifts in as a 0.
    return Math.floor(Date.now() / BUCKET_MS);
  }
  function rotateIfNeeded() {
    const idx = pulseBucketIndex();
    const advance = Math.min(idx - lastSpan, SPARK_BUCKETS);
    for (let i = 0; i < advance; i++) {
      buckets.shift();
      buckets.push(0);
    }
    lastSpan = idx;
  }
  function recordSetpoint() {
    rotateIfNeeded();
    buckets[SPARK_BUCKETS - 1] += 1;
    renderSpark();
  }
  function renderSpark() {
    const svg = document.getElementById("pulse-spark");
    if (!svg) return;
    const max = Math.max(1, ...buckets);
    // SVG viewBox is 0..60 wide × 0..16 tall. 5 px wide per bar
    // with no gap (the trace reads as a continuous histogram). Bar
    // height proportional to bucket / max; minimum 1 px so a single
    // event is still visible.
    const bw = 60 / SPARK_BUCKETS;
    const bars = buckets
      .map((v, i) => {
        const h = v === 0 ? 0 : Math.max(1, (v / max) * 16);
        const x = i * bw;
        const y = 16 - h;
        return `<rect class="bar" x="${x.toFixed(2)}" y="${y.toFixed(2)}" width="${(bw - 0.5).toFixed(2)}" height="${h.toFixed(2)}" />`;
      })
      .join("");
    svg.innerHTML = bars;
  }
  function renderHealth(components) {
    const counts = { ok: 0, standby: 0, error: 0 };
    for (const c of components) {
      const h = (c.health || "ok").toLowerCase();
      if (h in counts) counts[h] += 1;
    }
    const el = document.getElementById("pulse-health");
    if (!el) return;
    el.innerHTML = `
      <span class="health-chip ok"      title="ok components">OK ${counts.ok}</span>
      <span class="health-chip standby" title="standby components">STDBY ${counts.standby}</span>
      <span class="health-chip error"   title="error components">ERR ${counts.error}</span>`;
  }
  function renderGraph(status) {
    const el = document.getElementById("pulse-graph");
    if (!el) return;
    if (status == null) {
      el.textContent = "✓";
      el.className = "pulse-pill ok";
      el.title = "frequenz-microgrid-component-graph accepted the topology";
      el.onclick = null;
    } else {
      // Compact for the pill, full message in the title + alert on
      // click so the dev can read past the truncation.
      el.textContent = "⚠ rejected";
      el.className = "pulse-pill bad";
      el.title = status;
      el.onclick = () => alert(`Graph validator rejected the topology:\n\n${status}`);
    }
  }
  async function refreshLoopback() {
    const el = document.getElementById("pulse-loopback");
    if (!el) return;
    try {
      const res = await mgFetch("metrics/status");
      if (res == null) {
        el.textContent = "…";
        el.className = "pulse-pill";
        return;
      }
      const j = await res.json();
      if (res.ok && j.connected) {
        el.textContent = `✓ ${j.component_count ?? "?"} nodes`;
        el.className = "pulse-pill ok";
      } else {
        el.textContent = "⚠ connecting";
        el.className = "pulse-pill warn";
      }
    } catch (_) {
      el.textContent = "✗ unreachable";
      el.className = "pulse-pill bad";
    }
  }
  function renderClock() {
    const el = document.getElementById("pulse-clock");
    if (!el) return;
    el.textContent = clockState.formatNow();
  }
  return {
    setup() {
      renderSpark();
      renderHealth([]);
      renderGraph(null);
      refreshLoopback();
      renderClock();
      setupDensityToggle();
      // Loopback poll: every 5 s while not connected, every 15 s
      // once connected (cheap heartbeat, picks up a server restart
      // within one cycle). Constants kept generous so a slow page
      // doesn't see the pill flicker on a stalled fetch.
      setInterval(refreshLoopback, 5000);
      // 1 Hz clock + spark rotation; the spark rotator also handles
      // the case where no setpoints fire for a while (buckets
      // advance + drop off the left).
      setInterval(() => {
        renderClock();
        rotateIfNeeded();
        renderSpark();
      }, 1000);
    },
    recordSetpoint,
    applyTopology(components, graphStatus) {
      renderHealth(components);
      // `graphStatus === undefined` keeps the existing display
      // (e.g. an older server build without the field); the field
      // is reported as `null` for healthy graphs.
      if (graphStatus !== undefined) renderGraph(graphStatus);
    },
  };
})();
