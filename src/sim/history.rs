//! Per-component telemetry history — bounded ring buffers feeding the
//! UI's time-series charts. One task per site samples EVERY
//! component at a fixed 1 Hz (see `MicrogridSite::spawn_history_sampler`
//! in `microgrid_site/history.rs`), so the UI works even with zero
//! gRPC subscribers.
//!
//! Storage is sparse per metric — a `Battery` doesn't carry AC
//! voltage, a `Meter` doesn't carry SoC, so we only allocate a buffer
//! for metrics each component actually publishes.
//!
//! Pure data structures — no async, no I/O.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};

use crate::sim::Telemetry;

/// Single metric we track over time. Trimmed to the values that show
/// up in v1 charts; per-phase / DC metrics can join later if they
/// turn out to be load-bearing for control-app evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Metric {
    ActivePowerW,
    ReactivePowerVar,
    FrequencyHz,
    SocPct,
    /// Steam pressure (bar) — the steam boiler's SoC-analog state.
    /// Pinned to the boiler's target in steady state (the implied
    /// gas burner holds it); moves on perturbation and decays back
    /// at the steam-demand rate.
    PressureBar,
    /// DC-side active power, set by batteries / EV chargers /
    /// solar inverters that report a DC bus reading distinct from
    /// their AC `ActivePowerW`. Same units as ActivePowerW (`W`,
    /// quantity `Power`); kept as its own metric so the inspector
    /// can chart the AC + DC pair side-by-side.
    DcPowerW,
    /// Cumulative AC active energy (Wh) since the component's first
    /// sample — the running integral of `ActivePowerW` over wall-clock
    /// time. Signed with the same convention as active power (import /
    /// consumption positive, export / supply negative), so the running
    /// total is the *net* energy across the bus. Integrated on the
    /// physics tick via the shared trapezoidal accumulator (`EnergyAccum`,
    /// see `MicrogridSite::tick_once`), so it accrues in both the live
    /// server and the headless stepped runner; a component in
    /// `Health::Error` stops accruing until it recovers. The history
    /// sampler snapshots the running total into this ring for the UI
    /// charts. A P·dt integral related to the battery SoC one (see
    /// `battery.rs` `tick`), but a different quadrature — trapezoidal over
    /// the sample gap here, rectangular over one physics tick there.
    EnergyWh,
    ActivePowerLowerBoundW,
    ActivePowerUpperBoundW,
    ReactivePowerLowerBoundVar,
    ReactivePowerUpperBoundVar,
}

impl Metric {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ActivePowerW => "active_power_w",
            Self::ReactivePowerVar => "reactive_power_var",
            Self::FrequencyHz => "frequency_hz",
            Self::SocPct => "soc_pct",
            Self::PressureBar => "pressure_bar",
            Self::DcPowerW => "dc_power_w",
            Self::EnergyWh => "energy_wh",
            Self::ActivePowerLowerBoundW => "active_power_lower_bound_w",
            Self::ActivePowerUpperBoundW => "active_power_upper_bound_w",
            Self::ReactivePowerLowerBoundVar => "reactive_power_lower_bound_var",
            Self::ReactivePowerUpperBoundVar => "reactive_power_upper_bound_var",
        }
    }

    /// Typed quantity this metric carries, mirroring the
    /// frequenz-microgrid `Sample<Q>` `Q` parameter — `Power`,
    /// `ReactivePower`, `Frequency`, `Percentage`. The UI uses this
    /// to pick a scale family (power → W/kW/MW autoscale; linear →
    /// fixed unit) without having to special-case metric names in
    /// the SPA.
    pub fn quantity(self) -> &'static str {
        match self {
            Self::ActivePowerW
            | Self::DcPowerW
            | Self::ActivePowerLowerBoundW
            | Self::ActivePowerUpperBoundW => "Power",
            Self::ReactivePowerVar
            | Self::ReactivePowerLowerBoundVar
            | Self::ReactivePowerUpperBoundVar => "ReactivePower",
            Self::FrequencyHz => "Frequency",
            Self::SocPct => "Percentage",
            Self::EnergyWh => "Energy",
            // New quantity string; the UI treats unknown quantities as
            // fixed-unit, which is right for bar.
            Self::PressureBar => "Pressure",
        }
    }

    /// Base SI-ish unit string the raw samples are stored in. Power
    /// readings auto-scale to k/M on the UI side using this as the
    /// suffix; linear-kind quantities (Hz, %) display as-is.
    pub fn unit(self) -> &'static str {
        match self {
            Self::ActivePowerW
            | Self::DcPowerW
            | Self::ActivePowerLowerBoundW
            | Self::ActivePowerUpperBoundW => "W",
            Self::ReactivePowerVar
            | Self::ReactivePowerLowerBoundVar
            | Self::ReactivePowerUpperBoundVar => "VAr",
            Self::FrequencyHz => "Hz",
            Self::SocPct => "%",
            Self::EnergyWh => "Wh",
            Self::PressureBar => "bar",
        }
    }
}

impl std::str::FromStr for Metric {
    type Err = ();

    /// Parse a metric name (the string `as_str` returns) back into
    /// the enum. Used by the HTTP layer where the metric arrives as
    /// a query-string field. `Err(())` for an unknown name.
    fn from_str(s: &str) -> Result<Self, ()> {
        // Single source of truth: enumerate ALL once here so a new
        // variant in the enum stays in lockstep with both as_str
        // and from_str.
        const ALL: &[Metric] = &[
            Metric::ActivePowerW,
            Metric::ReactivePowerVar,
            Metric::FrequencyHz,
            Metric::SocPct,
            Metric::PressureBar,
            Metric::DcPowerW,
            Metric::EnergyWh,
            Metric::ActivePowerLowerBoundW,
            Metric::ActivePowerUpperBoundW,
            Metric::ReactivePowerLowerBoundVar,
            Metric::ReactivePowerUpperBoundVar,
        ];
        ALL.iter().copied().find(|m| m.as_str() == s).ok_or(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub ts: DateTime<Utc>,
    pub value: f32,
}

/// Bounded ring buffer of samples for a single metric. Pushes past
/// `capacity` evict the oldest sample.
#[derive(Debug)]
pub struct History {
    capacity: usize,
    ring: VecDeque<Sample>,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ring: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, ts: DateTime<Utc>, value: f32) {
        // `>=` not `==`: a capacity of 0 would never satisfy `== 0` after
        // the first push, growing the ring without bound.
        if self.ring.len() >= self.capacity {
            self.ring.pop_front();
        }
        self.ring.push_back(Sample { ts, value });
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Iterate samples whose timestamp is `>= since`. Order is oldest
    /// → newest. Useful when a UI client wants only the recent N
    /// minutes from a buffer that holds longer history.
    pub fn iter_window(&self, since: DateTime<Utc>) -> impl Iterator<Item = &Sample> {
        // Ring is monotonic-time-ordered (samples push at the tail
        // with ts == "now"), so partition_point finds the first
        // sample at-or-after `since` without scanning the whole ring.
        let cut = self.ring.partition_point(|s| s.ts < since);
        self.ring.iter().skip(cut)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.ring.iter()
    }
}

/// Per-component metric histories. Sparse — only metrics this
/// component actually publishes get a buffer.
#[derive(Debug)]
pub struct ComponentHistory {
    capacity: usize,
    series: HashMap<Metric, History>,
}

impl ComponentHistory {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            series: HashMap::new(),
        }
    }

    /// Record everything in `snapshot` that maps to a tracked Metric.
    /// Missing fields (Option::None on the snapshot) are skipped, so a
    /// Meter's tick produces 1–2 metric pushes; a BatteryInverter's
    /// produces 5–6.
    ///
    /// Returns the list of `(metric, value)` pairs that were actually
    /// recorded — the caller (typically `MicrogridSite::record_history_snapshot`)
    /// uses this to fan out per-sample broadcast events without
    /// re-walking the snapshot.
    pub fn push_snapshot(&mut self, ts: DateTime<Utc>, snapshot: &Telemetry) -> Vec<(Metric, f32)> {
        let mut pushed = Vec::new();
        let mut record = |this: &mut Self, m: Metric, v: f32| {
            this.push(ts, m, v);
            pushed.push((m, v));
        };
        if let Some(v) = snapshot.active_power_w {
            record(self, Metric::ActivePowerW, v);
        }
        if let Some(v) = snapshot.reactive_power_var {
            record(self, Metric::ReactivePowerVar, v);
        }
        if let Some(v) = snapshot.frequency_hz {
            record(self, Metric::FrequencyHz, v);
        }
        if let Some(v) = snapshot.soc_pct {
            record(self, Metric::SocPct, v);
        }
        if let Some(v) = snapshot.pressure_bar {
            record(self, Metric::PressureBar, v);
        }
        if let Some(v) = snapshot.dc_power_w {
            record(self, Metric::DcPowerW, v);
        }
        if let Some(b) = &snapshot.active_power_bounds {
            // Charts plot a single envelope band, so take the first
            // bounds segment. Components that emit multi-segment
            // VecBounds (split by a forbidden gap — e.g. an
            // augmentation that disjointly narrows the rated range)
            // lose the inner detail in the chart view; live values
            // still go through the gRPC stream un-collapsed. If the
            // UI ever needs the gap, push every segment instead and
            // teach the chart to plot piecewise.
            if let Some(first) = b.0.first() {
                if let Some(v) = first.lower {
                    record(self, Metric::ActivePowerLowerBoundW, v);
                }
                if let Some(v) = first.upper {
                    record(self, Metric::ActivePowerUpperBoundW, v);
                }
            }
        }
        if let Some(b) = &snapshot.reactive_power_bounds {
            // Same first-segment collapse as the active-bounds block
            // above — a multi-band Q envelope (split by a live Q
            // augmentation) charts only its first band.
            if let Some(first) = b.0.first() {
                if let Some(v) = first.lower {
                    record(self, Metric::ReactivePowerLowerBoundVar, v);
                }
                if let Some(v) = first.upper {
                    record(self, Metric::ReactivePowerUpperBoundVar, v);
                }
            }
        }
        pushed
    }

    fn push(&mut self, ts: DateTime<Utc>, metric: Metric, value: f32) {
        self.series
            .entry(metric)
            .or_insert_with(|| History::new(self.capacity))
            .push(ts, value);
    }

    /// Snapshot an externally-integrated cumulative value (the `EnergyWh`
    /// running total, integrated on the physics tick) into its ring.
    /// Unlike `push_snapshot`'s instantaneous metrics this value is
    /// computed elsewhere; recording it here keeps the UI charts fed.
    pub fn record(&mut self, ts: DateTime<Utc>, metric: Metric, value: f32) {
        self.push(ts, metric, value);
    }

    pub fn get(&self, metric: Metric) -> Option<&History> {
        self.series.get(&metric)
    }

    pub fn metrics(&self) -> impl Iterator<Item = Metric> + '_ {
        self.series.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    /// pressure_bar must round-trip the full metric plumbing: parse from
    /// the query-string name, carry quantity/unit for the UI scale
    /// picker, and get recorded by push_snapshot from a Telemetry that
    /// carries it (the returned pairs drive the WS fan-out).
    #[test]
    fn pressure_bar_metric_round_trips() {
        use std::str::FromStr;
        let m = Metric::from_str("pressure_bar").expect("parses");
        assert_eq!(m, Metric::PressureBar);
        assert_eq!(m.as_str(), "pressure_bar");
        assert_eq!(m.quantity(), "Pressure");
        assert_eq!(m.unit(), "bar");

        let mut h = ComponentHistory::new(10);
        let t = crate::sim::Telemetry {
            id: 7,
            pressure_bar: Some(8.25),
            ..Default::default()
        };
        let pushed = h.push_snapshot(chrono::Utc::now(), &t);
        assert!(pushed.contains(&(Metric::PressureBar, 8.25)));
    }

    #[test]
    fn ring_evicts_oldest_when_full() {
        let mut h = History::new(3);
        h.push(t(1), 10.0);
        h.push(t(2), 20.0);
        h.push(t(3), 30.0);
        h.push(t(4), 40.0); // evicts t=1

        let values: Vec<_> = h.iter().map(|s| s.value).collect();
        assert_eq!(values, vec![20.0, 30.0, 40.0]);
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn iter_window_skips_older_samples() {
        let mut h = History::new(5);
        for i in 1..=5 {
            h.push(t(i), i as f32 * 10.0);
        }
        let in_window: Vec<_> = h.iter_window(t(3)).map(|s| s.value).collect();
        assert_eq!(in_window, vec![30.0, 40.0, 50.0]);
    }

    #[test]
    fn component_history_records_only_published_metrics() {
        let mut ch = ComponentHistory::new(10);
        // Meter-style snapshot — has P/Q + frequency, no SoC.
        let snap = Telemetry {
            active_power_w: Some(1500.0),
            reactive_power_var: Some(200.0),
            frequency_hz: Some(50.01),
            ..Default::default()
        };
        ch.push_snapshot(t(1), &snap);
        let metrics: std::collections::HashSet<_> = ch.metrics().collect();
        assert!(metrics.contains(&Metric::ActivePowerW));
        assert!(metrics.contains(&Metric::ReactivePowerVar));
        assert!(metrics.contains(&Metric::FrequencyHz));
        assert!(!metrics.contains(&Metric::SocPct));
        assert_eq!(ch.get(Metric::ActivePowerW).unwrap().len(), 1);
    }

    #[test]
    fn component_history_extracts_active_bounds_envelope() {
        use crate::sim::bounds::VecBounds;
        let mut ch = ComponentHistory::new(10);
        let snap = Telemetry {
            active_power_w: Some(0.0),
            active_power_bounds: Some(VecBounds::single(-5000.0, 5000.0)),
            ..Default::default()
        };
        ch.push_snapshot(t(1), &snap);
        let lo = ch.get(Metric::ActivePowerLowerBoundW).unwrap();
        let hi = ch.get(Metric::ActivePowerUpperBoundW).unwrap();
        assert_eq!(lo.iter().next().unwrap().value, -5000.0);
        assert_eq!(hi.iter().next().unwrap().value, 5000.0);
    }

    /// A two-band Q envelope (a live Q augmentation splitting the
    /// caps band) collapses to its FIRST band in the chart history —
    /// same collapse rule as active bounds, applied to reactive.
    #[test]
    fn component_history_reactive_bounds_collapse_to_first_band() {
        use crate::proto::common::metrics::Bounds;
        use crate::sim::bounds::VecBounds;
        let mut ch = ComponentHistory::new(10);
        let snap = Telemetry {
            reactive_power_var: Some(0.0),
            reactive_power_bounds: Some(VecBounds(vec![
                Bounds {
                    lower: Some(-2000.0),
                    upper: Some(-500.0),
                },
                Bounds {
                    lower: Some(500.0),
                    upper: Some(2000.0),
                },
            ])),
            ..Default::default()
        };
        ch.push_snapshot(t(1), &snap);
        let lo = ch.get(Metric::ReactivePowerLowerBoundVar).unwrap();
        let hi = ch.get(Metric::ReactivePowerUpperBoundVar).unwrap();
        assert_eq!(lo.iter().next().unwrap().value, -2000.0);
        assert_eq!(hi.iter().next().unwrap().value, -500.0);
    }

    /// A zero-headroom Q envelope normalizes to a PRESENT single
    /// `(0.0, 0.0)` band at the `Telemetry` boundary (see
    /// `VecBounds::or_zero_band`), so the chart history still gets a
    /// scalar push for both edges — an absent band would otherwise
    /// leave the chart on its last (stale, non-zero) reading exactly
    /// when the operator should see "no headroom".
    #[test]
    fn component_history_zero_headroom_band_still_emits_scalars() {
        use crate::sim::bounds::VecBounds;
        let mut ch = ComponentHistory::new(10);
        let snap = Telemetry {
            reactive_power_var: Some(0.0),
            reactive_power_bounds: Some(VecBounds::single(0.0, 0.0)),
            ..Default::default()
        };
        ch.push_snapshot(t(1), &snap);
        let lo = ch.get(Metric::ReactivePowerLowerBoundVar).unwrap();
        let hi = ch.get(Metric::ReactivePowerUpperBoundVar).unwrap();
        assert_eq!(lo.iter().next().unwrap().value, 0.0);
        assert_eq!(hi.iter().next().unwrap().value, 0.0);
    }
}
