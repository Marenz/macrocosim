//! Telemetry history sampler + ring-buffer accessors on
//! `MicrogridSite`.
//!
//! `spawn_history_sampler` ticks at 1 Hz and drops a snapshot into
//! every component's per-metric ring. Each snapshot also fans out
//! as a `SiteEvent::Sample` and (when a scenario is running) feeds
//! the scenario journal's integrators. The actual ring lives on
//! `MicrogridSiteInner::histories`.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::sim::Category;
use crate::sim::events::SiteEvent;
use crate::sim::history::{ComponentHistory, History, Metric, Sample};

use super::{HISTORY_CAPACITY, MicrogridSite};

impl MicrogridSite {
    /// Spawn the history sampler — a single task that walks every
    /// component once per second and pushes a snapshot into each
    /// component's per-metric history rings.
    ///
    /// Single-task / fixed-cadence on purpose: a per-component task
    /// at each component's own `stream_interval` would be more
    /// faithful to gRPC stream semantics, but adds task lifecycle
    /// management (cancel-on-reload, re-spawn-per-component) for
    /// little chart-side benefit. 1 Hz × 600-sample capacity = 10
    /// minutes of history per series, plenty for the v1 charts.
    pub fn spawn_history_sampler(self) {
        let cadence = Duration::from_secs(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(cadence);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                self.record_history_snapshot(Utc::now());
            }
        });
    }

    /// Take one snapshot pass: read every component's telemetry and
    /// push to its history rings. Extracted so tests can drive sampling
    /// deterministically without spawning the periodic task.
    ///
    /// Each pushed metric also fans out as a `SiteEvent::Sample` on
    /// the broadcast bus, after the histories lock is released — so
    /// WS subscribers see live samples but can't deadlock against
    /// each other or against /api/history readers.
    pub fn record_history_snapshot(&self, now: DateTime<Utc>) {
        let components = self.inner.components.read().clone();
        let mut emitted: Vec<(u64, Metric, f32)> = Vec::new();
        // Integrals fed to the scenario reporter. We capture them
        // off the telemetry snapshot rather than refilling from the
        // metric stream so batteries (which expose only dc_power_w,
        // not active_power_w) get integrated too.
        let mut battery_samples: Vec<(u64, f32)> = Vec::new();
        let mut pv_samples: Vec<(u64, f32)> = Vec::new();
        // Collect every telemetry snapshot BEFORE taking the
        // histories / CSV write locks: `telemetry()` re-enters
        // `runtime` / `connections` reads, and holding the write
        // guards across it set up a fragile lock ordering against
        // `reset()`'s components→by_id→runtime→histories sequence.
        let snapshots: Vec<_> = components
            .iter()
            .map(|c| {
                let (snap, bounds, reactive_bounds) = self.telemetry_and_bounds(c.as_ref());
                (c, snap, bounds, reactive_bounds)
            })
            .collect();
        // Snapshot the physics-tick energy integrals (this read lock is
        // released before `histories` is taken) so the sampler records the
        // current cumulative total into each component's `EnergyWh` ring
        // for the UI charts. The integration itself lives in `tick_once`.
        let energy_totals = self.inner.component_energy.read().clone();
        {
            let mut histories = self.inner.histories.write();
            let mut csv_sinks = self.inner.scenario_csv.write();
            let mut bounds_sinks = self.inner.scenario_bounds_csv.write();
            let mut reactive_bounds_sinks = self.inner.scenario_reactive_bounds_csv.write();
            for (c, snap, bounds, reactive_bounds) in &snapshots {
                match c.category() {
                    Category::Battery => {
                        if let Some(p) = snap.dc_power_w {
                            battery_samples.push((c.id(), p));
                        }
                    }
                    Category::Inverter if c.subtype() == Some("solar") => {
                        if let Some(p) = snap.active_power_w {
                            pv_samples.push((c.id(), p));
                        }
                    }
                    _ => {}
                }
                if let Some(sink) = csv_sinks.get_mut(&c.id())
                    && let Err(e) = sink.write_row(now, snap)
                {
                    log::warn!("CSV write failed for {}: {e}", c.id());
                }
                if let Some(sink) = bounds_sinks.get_mut(&c.id())
                    && let Some(bounds) = bounds
                    && let Err(e) = sink.write_bounds_row(now, bounds)
                {
                    log::warn!("bounds CSV write failed for {}: {e}", c.id());
                }
                if let Some(sink) = reactive_bounds_sinks.get_mut(&c.id())
                    && let Some(reactive_bounds) = reactive_bounds
                    && let Err(e) = sink.write_bounds_row(now, reactive_bounds)
                {
                    log::warn!("reactive bounds CSV write failed for {}: {e}", c.id());
                }
                let entry = histories
                    .entry(c.id())
                    .or_insert_with(|| ComponentHistory::new(HISTORY_CAPACITY));
                for (m, v) in entry.push_snapshot(now, snap) {
                    emitted.push((c.id(), m, v));
                }
                if let Some(e) = energy_totals.get(&c.id()) {
                    let wh = e.total_wh as f32;
                    entry.record(now, Metric::EnergyWh, wh);
                    emitted.push((c.id(), Metric::EnergyWh, wh));
                }
            }
        }
        // Hand the battery / PV integration samples to the scenario
        // reporter so the metrics endpoint stays current. Only
        // meaningful while a scenario is running; the journal
        // short-circuits outside it. The site-level P/Q metrics do
        // NOT come through here — they ride the UI loopback's grid
        // formula streams and are fed from its publish path.
        // Integrals advance the cursor at the end so the next
        // snapshot's dt is measured from now.
        {
            let mut journal = self.inner.scenario.write();
            for (id, dc_power_w) in &battery_samples {
                journal.record_battery_sample(*id, *dc_power_w, now);
            }
            for (id, active_power_w) in &pv_samples {
                journal.record_pv_sample(*id, *active_power_w, now);
            }
            journal.advance_sample_cursor(now);
        }
        let ts_ms = now.timestamp_millis();
        for (id, metric, value) in emitted {
            let _ = self.inner.events.send(SiteEvent::Sample {
                id,
                metric: metric.as_str(),
                ts_ms,
                value,
            });
        }
    }

    /// Read a windowed slice of one component's history for one
    /// metric. Returns owned samples so the caller can release the
    /// read lock immediately. `None` if the component or metric has
    /// no recorded history yet.
    pub fn history_window(
        &self,
        id: u64,
        metric: Metric,
        since: DateTime<Utc>,
    ) -> Option<Vec<Sample>> {
        let h = self.inner.histories.read();
        let c = h.get(&id)?;
        let series: &History = c.get(metric)?;
        Some(series.iter_window(since).copied().collect())
    }

    /// List the metrics for which `id` has any recorded history.
    pub fn history_metrics(&self, id: u64) -> Vec<Metric> {
        self.inner
            .histories
            .read()
            .get(&id)
            .map(|c| c.metrics().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};

    use super::MicrogridSite;
    use crate::sim::component::SimulatedComponent;
    use crate::sim::events::SiteEvent;
    use crate::sim::history::Metric;

    /// A stub reporting a fixed AC active power every tick — enough to
    /// drive `integrate_energy` without a full component model.
    struct ConstPower {
        id: u64,
        watts: f32,
    }
    impl std::fmt::Display for ConstPower {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "const-{}", self.id)
        }
    }
    impl SimulatedComponent for ConstPower {
        fn id(&self) -> u64 {
            self.id
        }
        fn category(&self) -> crate::sim::Category {
            crate::sim::Category::Meter
        }
        fn name(&self) -> &str {
            "const"
        }
        fn stream_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
        fn telemetry(&self, _: &MicrogridSite) -> crate::sim::Telemetry {
            crate::sim::Telemetry {
                active_power_w: Some(self.watts),
                ..Default::default()
            }
        }
        fn make_fn(&self) -> &'static str {
            "%make-test-stub"
        }
        fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
            Vec::new()
        }
    }

    /// Driving `record_history_snapshot` directly populates the
    /// per-component ring buffers. Verified across multiple ticks via
    /// the windowed reader and the metric-set introspection.
    #[test]
    fn history_snapshot_populates_rings() {
        use crate::sim::Telemetry;
        struct FixedFlow {
            id: u64,
        }
        impl std::fmt::Display for FixedFlow {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "fixed-{}", self.id)
            }
        }
        impl SimulatedComponent for FixedFlow {
            fn id(&self) -> u64 {
                self.id
            }
            fn category(&self) -> crate::sim::Category {
                crate::sim::Category::Battery
            }
            fn name(&self) -> &str {
                "fixed"
            }
            fn stream_interval(&self) -> Duration {
                Duration::from_secs(1)
            }
            fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
            fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
                Telemetry {
                    active_power_w: Some(2500.0),
                    soc_pct: Some(72.5),
                    ..Default::default()
                }
            }
            fn make_fn(&self) -> &'static str {
                "%make-test-stub"
            }
            fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
                Vec::new()
            }
        }

        let w = MicrogridSite::new();
        w.register(FixedFlow { id: 7 });
        let t0 = Utc.timestamp_opt(1_000, 0).unwrap();
        let t1 = Utc.timestamp_opt(1_001, 0).unwrap();
        w.record_history_snapshot(t0);
        w.record_history_snapshot(t1);

        let metrics: std::collections::HashSet<_> = w.history_metrics(7).into_iter().collect();
        assert!(metrics.contains(&Metric::ActivePowerW));
        assert!(metrics.contains(&Metric::SocPct));

        let p = w
            .history_window(7, Metric::ActivePowerW, Utc.timestamp_opt(0, 0).unwrap())
            .unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].value, 2500.0);

        // Windowed read drops samples before `since`.
        let recent = w.history_window(7, Metric::ActivePowerW, t1).unwrap();
        assert_eq!(recent.len(), 1);
    }

    /// `record_history_snapshot` fans out a Sample event per pushed
    /// metric. We push two metrics (P + SoC) on the same tick, so
    /// expect two events at the same timestamp.
    #[tokio::test]
    async fn record_history_snapshot_emits_sample_events() {
        use crate::sim::Telemetry;
        struct PVStub;
        impl std::fmt::Display for PVStub {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "stub")
            }
        }
        impl SimulatedComponent for PVStub {
            fn id(&self) -> u64 {
                7
            }
            fn category(&self) -> crate::sim::Category {
                crate::sim::Category::Inverter
            }
            fn name(&self) -> &str {
                "stub"
            }
            fn stream_interval(&self) -> Duration {
                Duration::from_secs(1)
            }
            fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
            fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
                Telemetry {
                    active_power_w: Some(-12345.0),
                    soc_pct: Some(60.0),
                    ..Default::default()
                }
            }
            fn make_fn(&self) -> &'static str {
                "%make-test-stub"
            }
            fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
                Vec::new()
            }
        }
        let w = MicrogridSite::new();
        w.register(PVStub);
        let mut rx = w.subscribe_events();
        let now = Utc.timestamp_opt(1_000, 0).unwrap();
        // Integrate energy on the physics tick first so the sampler has a
        // running EnergyWh total to snapshot into the ring.
        w.tick_once(now, Duration::from_secs(1));
        w.record_history_snapshot(now);

        // Drain the receiver until we've seen one event per emitted
        // metric. There's no inter-event ordering guarantee so we
        // collect into a set keyed by metric.
        // active_power_w and soc_pct come straight off the snapshot;
        // energy_wh is the running integral, accrued on the physics tick
        // above and snapshotted into the ring here — so three events.
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for _ in 0..3 {
            match rx.recv().await.unwrap() {
                SiteEvent::Sample {
                    id,
                    metric,
                    ts_ms,
                    value: _,
                } => {
                    assert_eq!(id, 7);
                    assert_eq!(ts_ms, 1_000_000);
                    seen.insert(metric);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(seen.contains("active_power_w"));
        assert!(seen.contains("soc_pct"));
        assert!(seen.contains("energy_wh"));
    }

    /// Energy integrates on the physics tick, so `component_energy_wh` is
    /// populated in a headless stepped run that never calls the 1 Hz
    /// `record_history_snapshot`. (Regression: energy assertions used to
    /// read an unpopulated history layer under `--stepped`.)
    #[test]
    fn tick_integrates_component_energy_without_the_sampler() {
        let w = MicrogridSite::new();
        w.register(ConstPower {
            id: 4,
            watts: 3600.0,
        });
        let t = |s| Utc.timestamp_opt(s, 0).unwrap();
        // First tick only seeds the integral — no interval yet.
        w.tick_once(t(0), Duration::from_secs(0));
        assert_eq!(w.component_energy_wh(4), Some(0.0));
        // +1 h at a constant 3600 W → 3600 Wh, no history sampler involved.
        w.tick_once(t(3600), Duration::from_secs(3600));
        let wh = w.component_energy_wh(4).unwrap();
        assert!((wh - 3600.0).abs() < 1e-6, "{wh}");
    }

    /// A battery (no AC active power) never starts integrating, so it has
    /// no per-component energy — matching the sparse `EnergyWh` metric.
    #[test]
    fn battery_without_active_power_has_no_energy() {
        struct DcOnly;
        impl std::fmt::Display for DcOnly {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "dc")
            }
        }
        impl SimulatedComponent for DcOnly {
            fn id(&self) -> u64 {
                9
            }
            fn category(&self) -> crate::sim::Category {
                crate::sim::Category::Battery
            }
            fn name(&self) -> &str {
                "dc"
            }
            fn stream_interval(&self) -> Duration {
                Duration::from_secs(1)
            }
            fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
            fn telemetry(&self, _: &MicrogridSite) -> crate::sim::Telemetry {
                crate::sim::Telemetry {
                    dc_power_w: Some(5000.0),
                    ..Default::default()
                }
            }
            fn make_fn(&self) -> &'static str {
                "%make-test-stub"
            }
            fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
                Vec::new()
            }
        }
        let w = MicrogridSite::new();
        w.register(DcOnly);
        let t = |s| Utc.timestamp_opt(s, 0).unwrap();
        w.tick_once(t(0), Duration::from_secs(0));
        w.tick_once(t(3600), Duration::from_secs(3600));
        assert_eq!(w.component_energy_wh(9), None);
    }

    /// `Health::Error` pauses energy integration: the faulted span is not
    /// counted (the cursor is dropped and re-seeds on recovery), while the
    /// healthy spans on either side accrue normally.
    #[test]
    fn error_health_pauses_energy_integration() {
        use crate::sim::runtime::Health;
        let w = MicrogridSite::new();
        w.register(ConstPower {
            id: 4,
            watts: 3600.0,
        });
        let t = |s| Utc.timestamp_opt(s, 0).unwrap();
        w.tick_once(t(0), Duration::from_secs(0)); // seed
        w.tick_once(t(3600), Duration::from_secs(3600)); // +3600 Wh
        // Fault the component: the next span must not be integrated.
        w.set_health(4, Health::Error).unwrap();
        w.tick_once(t(7200), Duration::from_secs(3600)); // cursor dropped
        // Recover: the first healthy tick re-seeds, so nothing accrued
        // across the faulted window.
        w.set_health(4, Health::Ok).unwrap();
        w.tick_once(t(10800), Duration::from_secs(3600)); // re-seed
        w.tick_once(t(14400), Duration::from_secs(3600)); // +3600 Wh
        let wh = w.component_energy_wh(4).unwrap();
        assert!((wh - 7200.0).abs() < 1e-6, "{wh}");
    }

    /// Energy scenario checks read the accrual since `scenario_start`:
    /// the baseline snapshotted at start is subtracted from the running
    /// total, so a long-running live server behaves like a fresh
    /// stepped run.
    #[test]
    fn scenario_energy_counts_from_scenario_start() {
        let w = MicrogridSite::new();
        w.register(ConstPower {
            id: 4,
            watts: 3600.0,
        });
        let t = |s| Utc.timestamp_opt(s, 0).unwrap();
        w.tick_once(t(0), Duration::from_secs(0));
        w.tick_once(t(3600), Duration::from_secs(3600)); // 3600 Wh pre-scenario

        w.scenario_start("s".into(), t(3600));
        assert_eq!(w.component_energy_since_scenario_wh(4), Some(0.0));

        w.tick_once(t(7200), Duration::from_secs(3600)); // +3600 Wh during
        let wh = w.component_energy_since_scenario_wh(4).unwrap();
        assert!((wh - 3600.0).abs() < 1e-6, "{wh}");
        // The boot total still carries the full history.
        let total = w.component_energy_wh(4).unwrap();
        assert!((total - 7200.0).abs() < 1e-6, "{total}");
    }

    /// Removing a component mid-scenario drops its energy baseline too:
    /// a component re-registered under the same id restarts at zero, and
    /// subtracting the stale baseline would read negative energy.
    #[test]
    fn removed_component_drops_its_scenario_energy_baseline() {
        let w = MicrogridSite::new();
        w.register(ConstPower {
            id: 4,
            watts: 3600.0,
        });
        let t = |s| Utc.timestamp_opt(s, 0).unwrap();
        w.tick_once(t(0), Duration::from_secs(0));
        w.tick_once(t(3600), Duration::from_secs(3600)); // 3600 Wh
        w.scenario_start("s".into(), t(3600)); // baseline: 3600 Wh

        w.remove_component(4);
        w.register(ConstPower {
            id: 4,
            watts: 3600.0,
        });
        w.tick_once(t(7200), Duration::from_secs(3600)); // seed only
        w.tick_once(t(10800), Duration::from_secs(3600)); // +3600 Wh
        let wh = w.component_energy_since_scenario_wh(4).unwrap();
        assert!(wh >= 0.0, "stale baseline read negative energy: {wh}");
        assert!((wh - 3600.0).abs() < 1e-6, "{wh}");
    }
}
