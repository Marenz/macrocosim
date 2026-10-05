//! Scenario lifecycle + event journal.
//!
//! A scenario is a Lisp script that drives the simulator through a
//! sequence of events (sudden load spikes, component outages, …)
//! while a Rust observer records what happened. `ScenarioJournal`
//! is that observer. The script calls `(scenario-start NAME)` to
//! begin, `(scenario-event KIND PAYLOAD)` to record interesting
//! moments, and `(scenario-stop)` to freeze the report so it can
//! be fetched.
//!
//! Events are append-only with a capped ring so a long-running
//! scenario doesn't grow without bound. Each event carries a stable
//! monotonic id so HTTP clients can poll the events endpoint with a
//! `since=` cursor and only see new entries.

use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};

/// Cap on the in-memory event ring. A typical scenario fires
/// dozens of events per minute (component outages, setpoint
/// responses, milestone markers); 4096 entries covers ~30 minutes
/// of dense traffic before the oldest start aging out. The
/// overflow is silent; clients reading the events endpoint with a
/// `since=` cursor will see a gap.
const SCENARIO_EVENT_CAPACITY: usize = 4096;

/// Window length for the grid-power average ring (15 minutes in
/// seconds, UTC-aligned).
pub(crate) const WINDOW_AVG_LENGTH_S: i64 = 15 * 60;

/// Cap on retained check results. The pass/fail *counters* keep
/// counting past this; only the per-check detail ages out — so a
/// scenario asserting in a tight `(every …)` loop still reports
/// accurate totals while the report payload stays bounded.
const SCENARIO_CHECK_CAPACITY: usize = 1024;

/// Cap on retained 15-minute windows. 96 covers a full UTC day —
/// most scenarios don't run that long, and the BTreeMap drops
/// the oldest by key so a multi-day run keeps only the most
/// recent day.
const WINDOW_AVG_CAPACITY: usize = 96;

/// Single entry in the scenario journal. `id` is monotonic and
/// stable across the journal's lifetime — it does not reset on
/// `scenario_start`. Clients use it as the `since=` cursor for
/// `/api/mg/{mg}/scenario/events`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScenarioEvent {
    pub id: u64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub payload: String,
}

/// One `(scenario-expect …)` result. Recorded when the assertion
/// runs — checks are instantaneous reads of current state, not
/// watched conditions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScenarioCheck {
    pub ts: DateTime<Utc>,
    pub component_id: u64,
    /// Canonical metric name (`Metric::as_str`).
    pub metric: String,
    /// Human-readable expectation, e.g. `"approx -15000 (tol 500)"`
    /// or `"in [20, 80]"`.
    pub expectation: String,
    /// Observed value. `None` when the component wasn't registered
    /// or doesn't publish the metric — recorded as a failure.
    pub actual: Option<f32>,
    pub passed: bool,
}

/// Battery-side energy integrals accumulated since `scenario_start`.
/// Sign convention follows macrocosim's internals: positive DC power
/// = charging; negative = discharging. Both quantities are recorded
/// as positive Wh (absolute energy that crossed the bus in each
/// direction).
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct BatteryIntegrals {
    pub charge_wh: f64,
    pub discharge_wh: f64,
}

/// Solar-inverter integrals. PV publishes negative active power
/// while sourcing; `produced_wh` is the absolute energy delivered.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct PvIntegrals {
    pub produced_wh: f64,
}

/// Scenario journal: name + lifecycle + capped event ring + the
/// metric accumulators the reporter exposes via
/// `/api/mg/{mg}/scenario/report`.
#[derive(Debug, Default)]
pub struct ScenarioJournal {
    pub name: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    events: VecDeque<ScenarioEvent>,
    next_id: u64,
    /// Maximum positive active power seen on the loopback's
    /// `grid_power` formula stream (the LM's site import/export)
    /// since the scenario started. Resets on `start`, freezes — like
    /// elapsed — once `stop` lands, but keeps absorbing samples
    /// until then. Stored as f64 to match the units the integrals
    /// below use.
    peak_grid_active_w: f64,
    /// The grid streams' (P, Q) pair at the instant of maximum |Q|
    /// seen since the scenario started, so the report can derive
    /// power-factor-at-peak-Q. P and Q ride two independent formula
    /// streams, so the pair is the peak Q against the last-seen P
    /// (see `last_grid_p`); both streams are driven by the LM's
    /// shared 1 Hz tick, so the two are normally at most one sample
    /// apart — nothing here enforces it, and a forwarder that lags or
    /// exits can widen the pairing.
    /// `None` before `start` or before the first Q sample that
    /// follows a P sample; resets on `start`, freezes on `stop` like
    /// `peak_grid_active_w`.
    peak_grid_pq: Option<(f64, f64)>,
    /// Last `grid_power` sample seen, held so the next
    /// `grid_reactive_power` sample can be paired with it. P and Q
    /// arrive on two independent formula streams, so "the same
    /// instant" is the closest P before this Q. `None` until the
    /// first P of the run.
    last_grid_p: Option<f64>,
    /// Per-battery charge / discharge energy integrals since
    /// `scenario_start`. BTreeMap so report ordering is stable.
    per_battery: BTreeMap<u64, BatteryIntegrals>,
    /// Per-solar-inverter produced-energy integrals.
    per_pv: BTreeMap<u64, PvIntegrals>,
    /// Average grid active-power per 15-minute UTC-aligned
    /// window. Key is the window-start unix timestamp (seconds);
    /// value is `(sum_w, sample_count)` so the report derives the
    /// mean cheaply and additional samples accumulate without
    /// floating-point drift. Bounded at WINDOW_AVG_CAPACITY
    /// most-recent windows so a multi-day scenario doesn't grow
    /// without bound.
    window_avgs: BTreeMap<i64, (f64, u64)>,
    /// Wall-clock timestamp of the previous integration sample —
    /// used to compute `dt` for the energy integrals. `None` until
    /// `start` runs; updated at the end of each
    /// `MicrogridSite::record_history_snapshot` pass.
    prev_sample_ts: Option<DateTime<Utc>>,
    /// Recent `(scenario-expect …)` results, oldest evicted past
    /// SCENARIO_CHECK_CAPACITY. The counters below cover the full
    /// run regardless of eviction.
    checks: VecDeque<ScenarioCheck>,
    checks_passed: u64,
    checks_failed: u64,
    /// Per-component cumulative-energy totals (Wh) snapshotted when the
    /// scenario started (`MicrogridSite::scenario_start` fills it in).
    /// Energy checks subtract this baseline, so a check reads energy
    /// accrued during the scenario, not since boot — a live server that
    /// ran for hours beforehand behaves like a fresh stepped run.
    pub energy_baseline_wh: BTreeMap<u64, f64>,
}

impl ScenarioJournal {
    /// Begin a fresh scenario. Replaces any prior name, clears the
    /// stop marker, and empties the event ring. Event ids continue
    /// monotonically — a client polling the scenario events route
    /// across a restart sees a gap rather than an id rewind.
    pub fn start(&mut self, name: String, now: DateTime<Utc>) {
        self.name = Some(name);
        self.started_at = Some(now);
        self.ended_at = None;
        self.events.clear();
        self.peak_grid_active_w = 0.0;
        self.peak_grid_pq = None;
        self.last_grid_p = None;
        self.per_battery.clear();
        self.per_pv.clear();
        self.window_avgs.clear();
        self.checks.clear();
        self.checks_passed = 0;
        self.checks_failed = 0;
        self.energy_baseline_wh.clear();
        // Seed the integration cursor at start so the first
        // snapshot's dt covers `now → snapshot_ts`.
        self.prev_sample_ts = Some(now);
    }

    /// Hand one loopback `grid_power` sample (the LM grid formula's
    /// site import/export) to the reporter: peak-so-far, the
    /// 15-minute window averages, and the P side of the PF-at-peak
    /// pair. Skipped before `start` and after `stop`, so the metrics
    /// reflect only the active scenario window. `now` buckets the
    /// value into a 15-minute UTC-aligned window for the per-window
    /// average accumulator. ~1 Hz resampled — coarser than the raw
    /// telemetry the retired main-meter path read, accepted in the
    /// spec addendum.
    pub fn record_grid_power(&mut self, value: f32, now: DateTime<Utc>) {
        if !self.is_running() {
            return;
        }
        let v = value as f64;
        if v > self.peak_grid_active_w {
            self.peak_grid_active_w = v;
        }
        let window_start = (now.timestamp() / WINDOW_AVG_LENGTH_S) * WINDOW_AVG_LENGTH_S;
        let entry = self.window_avgs.entry(window_start).or_insert((0.0, 0));
        entry.0 += v;
        entry.1 += 1;
        while self.window_avgs.len() > WINDOW_AVG_CAPACITY {
            self.window_avgs.pop_first();
        }
        self.last_grid_p = Some(v);
    }

    pub fn peak_grid_active_w(&self) -> f64 {
        self.peak_grid_active_w
    }

    /// The Q twin: kept only while the |Q|-peak-so-far updates, so the
    /// stored pair is the last-seen P alongside the largest |Q| —
    /// exactly what a PF-at-peak-Q reading needs, and the same last-P
    /// pairing the UI's PF chips use. Q samples arriving before any P
    /// sample can't form a pair and are skipped. No-op outside the
    /// running window, matching `record_grid_power`. `now` is taken
    /// for symmetry with the P side; the pair carries no window of
    /// its own.
    pub fn record_grid_reactive(&mut self, value: f32, _now: DateTime<Utc>) {
        if !self.is_running() {
            return;
        }
        let Some(p) = self.last_grid_p else { return };
        let q = value as f64;
        let is_new_peak = match self.peak_grid_pq {
            Some((_, prev_q)) => q.abs() > prev_q.abs(),
            None => true,
        };
        if is_new_peak {
            self.peak_grid_pq = Some((p, q));
        }
    }

    /// The (P, Q) pair at the instant of maximum |Q| since the
    /// scenario started. `None` before any pairable sample.
    pub fn peak_grid_pq(&self) -> Option<(f64, f64)> {
        self.peak_grid_pq
    }

    /// Compute the integration window for this snapshot relative
    /// to the previous one. Used by `record_battery_sample` and
    /// `record_pv_sample`. Returns 0 if there is no prior cursor
    /// (scenario not running) or if time went backwards.
    fn integration_dt_s(&self, now: DateTime<Utc>) -> f64 {
        if !self.is_running() {
            return 0.0;
        }
        match self.prev_sample_ts {
            Some(prev) => (now - prev)
                .to_std()
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            None => 0.0,
        }
    }

    fn is_running(&self) -> bool {
        self.started_at.is_some() && self.ended_at.is_none()
    }

    /// Accumulate one battery DC-power sample. Positive = charging,
    /// negative = discharging; both go into the integrals as
    /// absolute Wh.
    pub fn record_battery_sample(&mut self, id: u64, dc_power_w: f32, now: DateTime<Utc>) {
        let dt_s = self.integration_dt_s(now);
        if dt_s <= 0.0 {
            return;
        }
        let p = dc_power_w as f64;
        let entry = self.per_battery.entry(id).or_default();
        if p > 0.0 {
            entry.charge_wh += p * dt_s / 3600.0;
        } else if p < 0.0 {
            entry.discharge_wh += (-p) * dt_s / 3600.0;
        }
    }

    /// Accumulate one solar-inverter active-power sample. Negative
    /// values (PV sourcing) contribute to `produced_wh`; non-
    /// negative values are no-ops.
    pub fn record_pv_sample(&mut self, id: u64, active_power_w: f32, now: DateTime<Utc>) {
        let dt_s = self.integration_dt_s(now);
        if dt_s <= 0.0 {
            return;
        }
        let p = active_power_w as f64;
        if p < 0.0 {
            self.per_pv.entry(id).or_default().produced_wh += (-p) * dt_s / 3600.0;
        }
    }

    /// Advance the integration cursor at the end of a snapshot
    /// pass — so the next snapshot's dt is measured from this one's
    /// timestamp.
    pub fn advance_sample_cursor(&mut self, now: DateTime<Utc>) {
        if self.is_running() {
            self.prev_sample_ts = Some(now);
        }
    }

    pub fn per_battery(&self) -> &BTreeMap<u64, BatteryIntegrals> {
        &self.per_battery
    }

    pub fn per_pv(&self) -> &BTreeMap<u64, PvIntegrals> {
        &self.per_pv
    }

    /// `(window_start_secs, (sum_w, sample_count))` per retained
    /// 15-minute window. The report layer divides on the way out.
    pub fn window_avgs(&self) -> &BTreeMap<i64, (f64, u64)> {
        &self.window_avgs
    }

    /// Record one assertion result. Mirrors `record` (events): no
    /// running-window gate — a check fired after `stop` is a script
    /// bug better surfaced in the report than silently dropped.
    pub fn record_check(&mut self, check: ScenarioCheck) {
        if check.passed {
            self.checks_passed += 1;
        } else {
            self.checks_failed += 1;
        }
        if self.checks.len() >= SCENARIO_CHECK_CAPACITY {
            self.checks.pop_front();
        }
        self.checks.push_back(check);
    }

    pub fn checks(&self) -> impl Iterator<Item = &ScenarioCheck> {
        self.checks.iter()
    }

    pub fn checks_passed(&self) -> u64 {
        self.checks_passed
    }

    pub fn checks_failed(&self) -> u64 {
        self.checks_failed
    }

    /// Mark the scenario as ended. Idempotent; subsequent calls
    /// keep the first stop time.
    pub fn stop(&mut self, now: DateTime<Utc>) {
        if self.ended_at.is_none() {
            self.ended_at = Some(now);
        }
    }

    /// Append an event. Drops the oldest entry when the ring is
    /// full. Returns the new event's id.
    pub fn record(&mut self, kind: String, payload: String, ts: DateTime<Utc>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        if self.events.len() >= SCENARIO_EVENT_CAPACITY {
            self.events.pop_front();
        }
        self.events.push_back(ScenarioEvent {
            id,
            ts,
            kind,
            payload,
        });
        id
    }

    /// Seconds since `started_at`, or 0 if the scenario hasn't
    /// started. Freezes at `ended_at` once stopped.
    pub fn elapsed_s(&self, now: DateTime<Utc>) -> f64 {
        let Some(start) = self.started_at else {
            return 0.0;
        };
        let end = self.ended_at.unwrap_or(now);
        (end - start)
            .to_std()
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    /// Returns events with id >= `from_id`, capped at `limit`.
    /// Oldest first. The `from_id` cursor is inclusive — clients
    /// poll `scenario/events?since=N` and pass back
    /// `next_event_id` from the previous response unchanged: that's
    /// "the id of the next event that hasn't been written yet", so
    /// id >= cursor naturally returns only new entries.
    pub fn events_since(&self, from_id: u64, limit: usize) -> Vec<ScenarioEvent> {
        self.events
            .iter()
            .filter(|e| e.id >= from_id)
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn next_event_id(&self) -> u64 {
        self.next_id
    }

    /// Smallest event id currently retained. After the ring evicts
    /// `K` events this is `K` — so a polling client comparing
    /// `since` against this can tell whether their cursor fell
    /// inside the evicted window and how many entries they lost.
    /// Returns `next_event_id` when the ring is empty (no gap).
    pub fn earliest_event_id(&self) -> u64 {
        self.events.front().map(|e| e.id).unwrap_or(self.next_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().unwrap()
    }

    #[test]
    fn lifecycle_sets_and_freezes_elapsed() {
        let mut j = ScenarioJournal::default();
        // Pre-start: elapsed is 0.
        assert_eq!(j.elapsed_s(ts(100)), 0.0);
        j.start("warmup".into(), ts(100));
        assert_eq!(j.name.as_deref(), Some("warmup"));
        assert!((j.elapsed_s(ts(160)) - 60.0).abs() < 1e-6);
        // Stop freezes elapsed at the stop ts.
        j.stop(ts(160));
        assert!((j.elapsed_s(ts(9999)) - 60.0).abs() < 1e-6);
        // Idempotent stop keeps the first time.
        j.stop(ts(99999));
        assert!((j.elapsed_s(ts(9999)) - 60.0).abs() < 1e-6);
    }

    #[test]
    fn record_assigns_monotonic_ids_and_filters_since() {
        let mut j = ScenarioJournal::default();
        j.start("s".into(), ts(0));
        let id0 = j.record("note".into(), "hi".into(), ts(1));
        let id1 = j.record("outage".into(), "bat-1003".into(), ts(2));
        let id2 = j.record("note".into(), "bye".into(), ts(3));
        assert_eq!((id0, id1, id2), (0, 1, 2));

        // from_id=0 (inclusive) returns everything.
        let from_0 = j.events_since(0, 10);
        assert_eq!(
            from_0.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![0, 1, 2],
        );

        // from_id=2 returns only id 2 onward.
        let from_2 = j.events_since(2, 10);
        assert_eq!(from_2.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2]);

        // limit caps the result.
        let capped = j.events_since(0, 2);
        assert_eq!(capped.iter().map(|e| e.id).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn start_clears_events_but_ids_continue() {
        let mut j = ScenarioJournal::default();
        j.start("first".into(), ts(0));
        j.record("a".into(), String::new(), ts(1));
        j.record("b".into(), String::new(), ts(2));
        assert_eq!(j.event_count(), 2);

        j.start("second".into(), ts(100));
        assert_eq!(j.event_count(), 0);
        assert_eq!(j.next_event_id(), 2);

        let id = j.record("c".into(), String::new(), ts(101));
        assert_eq!(id, 2);
    }

    #[test]
    fn battery_integrals_split_by_sign() {
        let mut j = ScenarioJournal::default();
        j.start("integ".into(), ts(0));
        // 10 seconds of charging at 1800 W: charge_wh += 1800 *
        // 10 / 3600 = 5.0 Wh.
        j.record_battery_sample(1, 1800.0, ts(10));
        j.advance_sample_cursor(ts(10));
        // 5 more seconds at -3600 W (discharging): 3600*5/3600 = 5 Wh.
        j.record_battery_sample(1, -3600.0, ts(15));
        j.advance_sample_cursor(ts(15));
        let b = j.per_battery().get(&1).unwrap();
        assert!((b.charge_wh - 5.0).abs() < 1e-6, "got {b:?}");
        assert!((b.discharge_wh - 5.0).abs() < 1e-6, "got {b:?}");
    }

    #[test]
    fn pv_integrals_treat_negative_power_as_production() {
        let mut j = ScenarioJournal::default();
        j.start("pv".into(), ts(0));
        // PV publishes negative power while sourcing — 3600 s at
        // -7200 W is 7200 Wh produced.
        j.record_pv_sample(2, -7200.0, ts(3600));
        j.advance_sample_cursor(ts(3600));
        // A spurious positive sample shouldn't decrement.
        j.record_pv_sample(2, 500.0, ts(3601));
        j.advance_sample_cursor(ts(3601));
        let p = j.per_pv().get(&2).unwrap();
        assert!((p.produced_wh - 7200.0).abs() < 1e-6, "got {p:?}");
    }

    #[test]
    fn integrals_skip_outside_running_window() {
        let mut j = ScenarioJournal::default();
        // Pre-start: no integration.
        j.record_battery_sample(1, 1000.0, ts(10));
        assert!(j.per_battery().get(&1).is_none());

        j.start("s".into(), ts(20));
        j.record_battery_sample(1, 1800.0, ts(80)); // 60 s at 1800 W = 30 Wh
        j.advance_sample_cursor(ts(80));
        j.stop(ts(80));

        // Post-stop samples are dropped.
        j.record_battery_sample(1, 7200.0, ts(140));
        j.advance_sample_cursor(ts(140));
        let b = j.per_battery().get(&1).unwrap();
        assert!((b.charge_wh - 30.0).abs() < 1e-6, "got {b:?}");
        assert_eq!(b.discharge_wh, 0.0);
    }

    #[test]
    fn restart_clears_integrals() {
        let mut j = ScenarioJournal::default();
        j.start("first".into(), ts(0));
        j.record_battery_sample(1, 3600.0, ts(60)); // 60 Wh
        j.advance_sample_cursor(ts(60));
        assert!(j.per_battery().get(&1).is_some());

        j.start("second".into(), ts(100));
        assert!(j.per_battery().is_empty());
        assert!(j.per_pv().is_empty());
        assert!(j.window_avgs().is_empty());
    }

    /// Grid-power samples bucket into 15-minute UTC-aligned windows,
    /// and the peak is the largest positive P seen. Each window
    /// accumulates a `(sum, count)` so the report layer can derive
    /// the mean. First window: (3000+5000+4000)/3 = 4000. Second
    /// window: (6000+9000)/2 = 7500.
    #[test]
    fn grid_peak_and_windows_track_grid_power_samples() {
        let mut j = ScenarioJournal::default();
        j.start("avg".into(), ts(0));
        j.record_grid_power(3000.0, ts(100));
        j.record_grid_power(5000.0, ts(300));
        j.record_grid_power(4000.0, ts(800));
        j.record_grid_power(6000.0, ts(900));
        j.record_grid_power(9000.0, ts(1500));

        assert_eq!(j.peak_grid_active_w(), 9000.0);

        let avgs = j.window_avgs();
        assert_eq!(avgs.len(), 2);
        let (sum_a, n_a) = avgs[&0];
        let (sum_b, n_b) = avgs[&900];
        assert_eq!(n_a, 3);
        assert_eq!(n_b, 2);
        assert!((sum_a / n_a as f64 - 4000.0).abs() < 1e-6);
        assert!((sum_b / n_b as f64 - 7500.0).abs() < 1e-6);
    }

    /// The window ring is bounded: past WINDOW_AVG_CAPACITY windows
    /// the oldest key is evicted, so a multi-day run stays capped.
    #[test]
    fn window_averages_evict_the_oldest_past_capacity() {
        let mut j = ScenarioJournal::default();
        j.start("cap".into(), ts(0));
        for w in 0..(WINDOW_AVG_CAPACITY as i64 + 5) {
            j.record_grid_power(1000.0, ts(w * WINDOW_AVG_LENGTH_S));
        }
        let avgs = j.window_avgs();
        assert_eq!(avgs.len(), WINDOW_AVG_CAPACITY);
        // The first 5 windows aged out; the oldest retained starts at
        // window 5.
        assert_eq!(*avgs.keys().next().unwrap(), 5 * WINDOW_AVG_LENGTH_S);
    }

    /// The stored PF pair is the last-seen P alongside the largest
    /// |Q| — a smaller-magnitude Q afterwards doesn't displace it.
    #[test]
    fn grid_pq_pairs_last_p_with_peak_q() {
        let mut j = ScenarioJournal::default();
        j.start("pf".into(), ts(0));
        j.record_grid_power(8000.0, ts(100));
        j.record_grid_reactive(6000.0, ts(101));
        j.record_grid_power(2000.0, ts(200));
        j.record_grid_reactive(-1000.0, ts(201)); // |Q| smaller: not a new peak
        assert_eq!(j.peak_grid_pq(), Some((8000.0, 6000.0)));
    }

    /// A Q sample arriving before any P sample can't form a pair, so
    /// it is skipped rather than pairing against a fabricated zero.
    #[test]
    fn grid_reactive_without_a_prior_p_forms_no_pair() {
        let mut j = ScenarioJournal::default();
        j.start("pf".into(), ts(0));
        j.record_grid_reactive(6000.0, ts(100));
        assert_eq!(j.peak_grid_pq(), None);
    }

    /// Samples outside the running window are dropped, matching the
    /// integrals — the report covers only the active scenario.
    #[test]
    fn grid_samples_ignored_outside_the_running_window() {
        let mut j = ScenarioJournal::default();
        // Pre-start.
        j.record_grid_power(9000.0, ts(100));
        j.record_grid_reactive(9000.0, ts(101));
        assert_eq!(j.peak_grid_active_w(), 0.0);
        assert_eq!(j.peak_grid_pq(), None);
        assert!(j.window_avgs().is_empty());

        j.start("s".into(), ts(200));
        j.record_grid_power(1000.0, ts(210));
        j.stop(ts(220));
        // Post-stop.
        j.record_grid_power(9000.0, ts(300));
        assert_eq!(j.peak_grid_active_w(), 1000.0);
    }

    fn check(passed: bool, at: DateTime<Utc>) -> ScenarioCheck {
        ScenarioCheck {
            ts: at,
            component_id: 200,
            metric: "active_power_w".into(),
            expectation: "approx 5000 (tol 100)".into(),
            actual: Some(if passed { 5000.0 } else { 9999.0 }),
            passed,
        }
    }

    /// Counters track every recorded check; the detail ring evicts
    /// past capacity without disturbing them.
    #[test]
    fn check_counters_survive_ring_eviction() {
        let mut j = ScenarioJournal::default();
        j.start("checks".into(), ts(0));
        for i in 0..(SCENARIO_CHECK_CAPACITY + 10) {
            j.record_check(check(i % 2 == 0, ts(i as i64)));
        }
        assert_eq!(j.checks().count(), SCENARIO_CHECK_CAPACITY);
        assert_eq!(
            j.checks_passed() + j.checks_failed(),
            (SCENARIO_CHECK_CAPACITY + 10) as u64,
        );
    }

    /// `start` resets both the check ring and the counters.
    #[test]
    fn restart_clears_checks() {
        let mut j = ScenarioJournal::default();
        j.start("first".into(), ts(0));
        j.record_check(check(true, ts(1)));
        j.record_check(check(false, ts(2)));
        assert_eq!(j.checks_failed(), 1);

        j.start("second".into(), ts(100));
        assert_eq!(j.checks().count(), 0);
        assert_eq!(j.checks_passed(), 0);
        assert_eq!(j.checks_failed(), 0);
    }

    #[test]
    fn ring_caps_at_capacity_and_drops_oldest() {
        let mut j = ScenarioJournal::default();
        j.start("flood".into(), ts(0));
        for i in 0..(SCENARIO_EVENT_CAPACITY + 50) {
            j.record("tick".into(), i.to_string(), ts(i as i64));
        }
        assert_eq!(j.event_count(), SCENARIO_EVENT_CAPACITY);
        // Oldest 50 ids dropped.
        let earliest = j.events_since(0, 1).into_iter().next().unwrap();
        assert_eq!(earliest.id, 50);
    }
}
