//! Scenario lifecycle + reporting on a `MicrogridSite`.
//!
//! Three concerns live here:
//!
//! - The scenario journal: start / stop / record / events_since /
//!   summary / report, plus the elapsed-time accessor.
//! - CSV sink open / close — paired with scenario start / stop so
//!   the recorded files match the journal window.
//! - The scenario-report shape and its SoC-stats helper.
//!
//! All persistent state lives in `MicrogridSiteInner`; this file
//! only adds methods.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use tulisp::TulispContext;

use crate::sim::component::{KnobKind, ReactiveReading};
use crate::sim::scenario::{ScenarioCheck, ScenarioEvent};
use crate::sim::scenario_csv::{CsvSink, CsvSinks};

use super::MicrogridSite;

/// Snapshot of `ScenarioJournal` lifecycle state for `/api/scenario`.
/// Excludes the events themselves — those live behind a paginated
/// `/api/scenario/events` endpoint with a `since=` cursor.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScenarioSummary {
    pub name: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub elapsed_s: f64,
    pub event_count: usize,
    /// One past the highest event id ever recorded. Stable cursor
    /// for `/api/scenario/events?since=N` — clients pass this back
    /// unchanged to mean "anything newer than what I last saw".
    pub next_event_id: u64,
    /// Lowest event id still retained in the ring. Clients compare
    /// their `since` cursor against this: if `since < earliest_event_id`
    /// they're polling into a window that has already been evicted,
    /// so some events were missed.
    pub earliest_event_id: u64,
}

/// Snapshot of scenario-scoped metrics for `/api/scenario/report`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScenarioReport {
    /// The scenario the report belongs to (`None` before any start).
    /// Clients check this in the same response they judge, so a report
    /// can never be silently attributed to the wrong scenario.
    pub name: Option<String>,
    pub scenario_elapsed_s: f64,
    /// Maximum active power seen on the loopback's `grid_power`
    /// formula stream since the scenario started — the site's
    /// import peak, resampled at ~1 Hz.
    pub peak_grid_w: f64,
    /// Maximum |reactive power| seen on the `grid_reactive_power`
    /// stream since the scenario started — the Q twin of
    /// `peak_grid_w`, tracked by magnitude rather than signed max
    /// (Q swings both ways).
    pub peak_grid_var: f64,
    /// Power factor `|P| / sqrt(P^2 + Q^2)` at the grid connection
    /// point at the instant `peak_grid_var` was recorded — paired
    /// against the last P sample, not an independently-peaked P.
    /// `None` before any pairable PQ sample, or when both P and Q
    /// were 0 at that instant.
    pub site_pf_at_peak_var: Option<f64>,
    pub total_battery_charged_wh: f64,
    pub total_battery_discharged_wh: f64,
    pub total_pv_produced_wh: f64,
    pub per_battery: Vec<PerBatteryReport>,
    pub per_pv: Vec<PerPvReport>,
    /// Stats over the *current* SoC of every registered battery.
    /// Computed lazily on each report fetch — cheap O(N) over a
    /// handful of batteries. None when no batteries are registered.
    pub soc_stats: Option<SocStats>,
    /// Per-15-minute UTC-aligned window average of grid active
    /// power. Sorted oldest-first.
    pub grid_window_averages: Vec<WindowAverageEntry>,
    /// Full-run `(scenario-expect …)` totals. Count every check
    /// even after the detail ring below starts evicting.
    pub checks_passed: u64,
    pub checks_failed: u64,
    /// Recent check results, oldest first (bounded ring).
    pub checks: Vec<ScenarioCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PerBatteryReport {
    pub id: u64,
    pub charge_wh: f64,
    pub discharge_wh: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PerPvReport {
    pub id: u64,
    pub produced_wh: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SocStats {
    /// Arithmetic mean of every battery's current SoC.
    pub mean_pct: f64,
    /// Median (lower of the two middle values for an even count).
    pub median_pct: f64,
    /// Mode bucketed to integer percent. If multiple buckets tie,
    /// returns the lowest. None for an empty set.
    pub mode_pct: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WindowAverageEntry {
    pub window_start: DateTime<Utc>,
    pub avg_w: f64,
}

/// Compute mean / median / integer-bucketed mode over a battery
/// SoC sample set. Returns `None` for an empty input.
fn compute_soc_stats(socs: &[f32]) -> Option<SocStats> {
    if socs.is_empty() {
        return None;
    }
    let mean_pct = socs.iter().map(|v| *v as f64).sum::<f64>() / socs.len() as f64;
    let mut sorted: Vec<f32> = socs.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_pct = sorted[sorted.len() / 2 - usize::from(sorted.len().is_multiple_of(2))] as f64;
    // Mode: integer-bucketed, lowest-bucket on tie.
    let mut histogram = [0u32; 101];
    for v in socs {
        let bucket = v.clamp(0.0, 100.0).round() as usize;
        histogram[bucket] += 1;
    }
    // Pick the lowest bucket on a count tie. `max_by_key` keeps
    // the LAST max seen; iterate ascending and update only on
    // strictly greater so the lowest bucket wins.
    let mut mode_pct: u8 = 0;
    let mut best_count: u32 = 0;
    for (idx, count) in histogram.iter().enumerate() {
        if *count > best_count {
            best_count = *count;
            mode_pct = idx as u8;
        }
    }
    Some(SocStats {
        mean_pct,
        median_pct,
        mode_pct: Some(mode_pct),
    })
}

impl MicrogridSite {
    /// Open fresh CSV sinks per registered component under `dir`:
    /// a telemetry file for every component, plus a received-setpoints
    /// and an effective-bounds file for each component that reports an
    /// active-power envelope (the ones a control app commands), plus
    /// an effective-reactive-bounds file for each component that
    /// reports a Q envelope (`bounds_of(id, Reactive)` is `Some` — a
    /// different set: an inverter has one, the battery behind it
    /// doesn't). Returns the total file count opened. Existing sinks
    /// are dropped first so a re-call replaces (rather than appends
    /// to) the prior recording.
    pub(crate) fn scenario_open_csv(&self, dir: &Path) -> std::io::Result<usize> {
        std::fs::create_dir_all(dir)?;
        let components = self.inner.components.read().clone();
        let mut telemetry = CsvSinks::new();
        let mut setpoints = CsvSinks::new();
        let mut bounds = CsvSinks::new();
        let mut reactive_bounds = CsvSinks::new();
        for c in components.iter() {
            telemetry.insert(c.id(), CsvSink::open(dir, c.id(), c.category())?);
            if self
                .bounds_of(c.id(), crate::timeout_tracker::SetpointAxis::Active)
                .is_some()
            {
                setpoints.insert(c.id(), CsvSink::open_setpoints(dir, c.id())?);
                bounds.insert(c.id(), CsvSink::open_bounds(dir, c.id())?);
            }
            if self
                .bounds_of(c.id(), crate::timeout_tracker::SetpointAxis::Reactive)
                .is_some()
            {
                reactive_bounds.insert(c.id(), CsvSink::open_reactive_bounds(dir, c.id())?);
            }
        }
        let count = telemetry.len() + setpoints.len() + bounds.len() + reactive_bounds.len();
        *self.inner.scenario_csv.write() = telemetry;
        *self.inner.scenario_setpoints_csv.write() = setpoints;
        *self.inner.scenario_bounds_csv.write() = bounds;
        *self.inner.scenario_reactive_bounds_csv.write() = reactive_bounds;
        *self.inner.scenario_csv_dir.write() = Some(dir.to_path_buf());
        Ok(count)
    }

    /// The directory the active/most-recent recording wrote to, plus
    /// the `.csv` file names in it (sorted). `None` if nothing has been
    /// recorded. Used by the UI to offer the CSVs for download.
    pub(crate) fn scenario_csv_listing(&self) -> Option<(std::path::PathBuf, Vec<String>)> {
        let dir = self.inner.scenario_csv_dir.read().clone()?;
        let mut files: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(".csv").then_some(name)
            })
            .collect();
        files.sort();
        Some((dir, files))
    }

    /// Drop every active CSV sink (telemetry, setpoints, bounds).
    /// Each underlying `BufWriter` flushes on drop. Returns the
    /// total file count closed.
    pub(crate) fn scenario_close_csv(&self) -> usize {
        let mut count = 0;
        for sinks in [
            &self.inner.scenario_csv,
            &self.inner.scenario_setpoints_csv,
            &self.inner.scenario_bounds_csv,
            &self.inner.scenario_reactive_bounds_csv,
        ] {
            let mut g = sinks.write();
            count += g.len();
            g.clear();
        }
        count
    }

    /// Begin a fresh scenario at `now`. Empties the event ring,
    /// clears the stop marker, sets the name, and snapshots the
    /// per-component energy totals as the scenario's baseline (energy
    /// checks read the accrual past it). Used by `(scenario-start)`.
    pub(crate) fn scenario_start(&self, name: String, now: DateTime<Utc>) {
        let baseline: std::collections::BTreeMap<u64, f64> = self
            .inner
            .component_energy
            .read()
            .iter()
            .map(|(id, e)| (*id, e.total_wh))
            .collect();
        let mut journal = self.inner.scenario.write();
        journal.start(name, now);
        journal.energy_baseline_wh = baseline;
        drop(journal);
        // Fresh run, fresh baseline — any knobs a PRIOR scenario
        // displaced and never got to restore (a crash, a hot-reload)
        // must not leak into this one's teardown.
        self.inner.scenario_knob_baseline.write().clear();
    }

    /// Is a scenario RUNNING right now — started and not yet stopped?
    /// The single source of truth for "a scenario is in progress":
    /// `scenario_snapshot_knob` self-gates on it, and the
    /// `(scenario-running-p)` defun exposes it to Lisp so the timer
    /// tracking in `sim/scenarios.lisp` needs no flag of its own to
    /// keep in sync with the journal.
    pub(crate) fn scenario_is_running(&self) -> bool {
        let g = self.inner.scenario.read();
        g.started_at.is_some() && g.ended_at.is_none()
    }

    /// Capture `id`'s `kind` knob into the scenario's pre-run
    /// baseline, first-snapshot-wins, so `scenario_stop` can put it
    /// back exactly as it was before the scenario touched it — even
    /// if the scenario re-drives the same knob many times in between.
    /// No-op unless a scenario is currently RUNNING (`started_at` set,
    /// `ended_at` unset): a snapshot taken outside a run has nothing
    /// to restore against, and would otherwise sit in the map forever
    /// waiting for a `scenario_stop` that never comes for it. Also a
    /// no-op if `id` isn't registered or doesn't support `kind` — the
    /// component's own `snapshot_knob` reports that via `None`.
    ///
    /// Called from EVERY door onto these knobs before it mutates one:
    /// the `set-meter-power` / `set-meter-reactive-power` /
    /// `set-meter-power-factor` / `set-solar-sunlight` /
    /// `set-boiler-demand` / `clear-meter-power` /
    /// `clear-meter-reactive` Lisp defuns, and the typed
    /// `POST /api/component/:id/drive` route's equivalent fields. A
    /// door that skipped it would not just leak its own write past
    /// teardown — its first-touch write would become the "baseline"
    /// a later drive captures, and `scenario_stop` would restore THAT
    /// instead of the pre-scenario state.
    pub(crate) fn scenario_snapshot_knob(&self, id: u64, kind: KnobKind) {
        if !self.scenario_is_running() {
            return;
        }
        let Some(component) = self.get(id) else {
            return;
        };
        let mut baseline = self.inner.scenario_knob_baseline.write();
        use std::collections::btree_map::Entry;
        if let Entry::Vacant(slot) = baseline.entry((id, kind))
            && let Some(snap) = component.snapshot_knob(kind)
        {
            slot.insert(snap);
        }
    }

    /// Mark the scenario as ended at `now`. Also closes any active
    /// CSV sinks so the file flushes before a downstream loader
    /// might pick it up, and restores every knob a running scenario
    /// displaced back to its pre-scenario baseline. Idempotent: the
    /// baseline map is drained (`mem::take`) on the first call, so a
    /// second `scenario_stop` finds it empty and restores/broadcasts
    /// nothing.
    ///
    /// Three phases, in this order, because the middle one is what
    /// makes a restored knob READ correctly:
    ///
    /// 1. restore — write each snapshot back into its component;
    /// 2. re-resolve — with `ctx`, call `refresh_inputs` on every
    ///    component that got something back. A snapshot carries the
    ///    live `DynamicScalar` AND the cached number it last resolved
    ///    to, frozen at capture time; whatever the underlying lambda
    ///    or symbol means NOW (a scenario cue may well have moved it)
    ///    only lands in that cache on a refresh. Without this pass a
    ///    caller reading the component the instant `(scenario-stop)`
    ///    returns — `run_scenario_stepped` does exactly that — sees a
    ///    stale value that self-corrects one refresh tick later, and
    ///    the broadcast below would ship that stale number to the UI;
    /// 3. broadcast — emit `KnobChanged` per restored knob, reading
    ///    the now-current value back off the component.
    ///
    /// `ctx` is `None` for callers with no interpreter at hand (unit
    /// tests, and any future non-Lisp stop path): phase 2 is skipped
    /// and a dynamic source reads its capture-time value until the
    /// next refresh pass, exactly as before. The `(scenario-stop)`
    /// defun — every real stop path, the HTTP route included — always
    /// passes one.
    ///
    /// Lock order: the baseline map is drained under its own lock
    /// BEFORE any component is touched — `restore_knob`, the refresh
    /// pass and the knob-reading calls below never run while that
    /// lock is held, so there's no window where the baseline lock is
    /// held alongside a component's own lock.
    pub(crate) fn scenario_stop(&self, now: DateTime<Utc>, ctx: Option<&mut TulispContext>) {
        self.inner.scenario.write().stop(now);
        self.scenario_close_csv();
        let baseline = std::mem::take(&mut *self.inner.scenario_knob_baseline.write());
        let mut restored: Vec<(u64, KnobKind)> = Vec::new();
        for ((id, kind), snap) in baseline {
            let Some(component) = self.get(id) else {
                continue;
            };
            if component.restore_knob(snap) {
                restored.push((id, kind));
            }
        }
        if let Some(ctx) = ctx {
            // One refresh per component, not per knob: a meter whose
            // two axes were both restored re-resolves both in one call.
            let mut seen: Vec<u64> = Vec::new();
            for (id, _) in &restored {
                if seen.contains(id) {
                    continue;
                }
                seen.push(*id);
                if let Some(component) = self.get(*id) {
                    component.refresh_inputs(ctx);
                }
            }
        }
        for (id, kind) in restored {
            if let Some(component) = self.get(id) {
                self.broadcast_restored_knob(id, kind, component.as_ref());
            }
        }
    }

    /// Rebroadcast a `KnobChanged` for the token(s) `kind` corresponds
    /// to, reading the freshly-restored value back off the component
    /// — mirrors what the `clear-meter-*` / `set-meter-power-factor`
    /// Lisp defuns broadcast on their own success paths, including the
    /// meter-reactive axis's two-token aliasing (`meter-reactive-power`
    /// AND `meter-power-factor` both go out, since a live reading is
    /// exactly one shape or the other and the inspector's two inputs
    /// both need to hear about it).
    fn broadcast_restored_knob(
        &self,
        id: u64,
        kind: KnobKind,
        component: &dyn crate::sim::SimulatedComponent,
    ) {
        match kind {
            KnobKind::MeterPower => {
                let (value, expr) = match component
                    .meter_drive()
                    .and_then(|m| m.meter_power_reading())
                {
                    Some(r) => (Some(r.value), r.expr),
                    None => (None, None),
                };
                self.note_knob_changed(id, "meter-power", value, expr, None);
            }
            KnobKind::MeterReactive => match component
                .meter_drive()
                .and_then(|m| m.meter_reactive_reading())
            {
                Some(ReactiveReading::Var(r)) => {
                    self.note_knob_changed(id, "meter-reactive-power", Some(r.value), r.expr, None);
                    self.note_knob_changed(id, "meter-power-factor", None, None, None);
                }
                Some(ReactiveReading::PowerFactor { pf, leading }) => {
                    self.note_knob_changed(id, "meter-reactive-power", None, None, None);
                    self.note_knob_changed(id, "meter-power-factor", Some(pf), None, Some(leading));
                }
                None => {
                    self.note_knob_changed(id, "meter-reactive-power", None, None, None);
                    self.note_knob_changed(id, "meter-power-factor", None, None, None);
                }
            },
            KnobKind::Sunlight => {
                let (value, expr) = match component.sunlight_drive().map(|s| s.sunlight_reading()) {
                    Some(r) => (Some(r.value), r.expr),
                    None => (None, None),
                };
                self.note_knob_changed(id, "solar-sunlight", value, expr, None);
            }
            KnobKind::BoilerDemand => {
                let (value, expr) = match component.steam_drive().map(|b| b.demand_reading()) {
                    Some(r) => (Some(r.value), r.expr),
                    None => (None, None),
                };
                self.note_knob_changed(id, "boiler-demand", value, expr, None);
            }
            KnobKind::Ev => {
                // Value only: `expr` carries printed Lisp source, and a
                // restored car's preset name is not that.
                let value = component
                    .ev_port()
                    .and_then(|p| p.ev_info())
                    .map(|i| i.ev.soc_pct);
                self.note_knob_changed(id, "ev", value, None, None);
            }
        }
    }

    /// Append a journal event. Returns the assigned id.
    pub(crate) fn scenario_record(&self, kind: String, payload: String, now: DateTime<Utc>) -> u64 {
        self.inner.scenario.write().record(kind, payload, now)
    }

    /// Record one `(scenario-expect …)` result.
    pub(crate) fn scenario_record_check(&self, check: ScenarioCheck) {
        self.inner.scenario.write().record_check(check);
    }

    /// Wall-clock seconds since the scenario started. 0 if not
    /// running. Freezes once stopped.
    pub(crate) fn scenario_elapsed_s(&self, now: DateTime<Utc>) -> f64 {
        self.inner.scenario.read().elapsed_s(now)
    }

    /// Snapshot of scenario lifecycle for `/api/scenario`.
    pub(crate) fn scenario_summary(&self, now: DateTime<Utc>) -> ScenarioSummary {
        let g = self.inner.scenario.read();
        ScenarioSummary {
            name: g.name.clone(),
            started_at: g.started_at,
            ended_at: g.ended_at,
            elapsed_s: g.elapsed_s(now),
            event_count: g.event_count(),
            next_event_id: g.next_event_id(),
            earliest_event_id: g.earliest_event_id(),
        }
    }

    /// Pull events with id >= `since` (callers pass the cursor from
    /// `next_event_id`), capped at `limit`. Used by
    /// `/api/scenario/events`.
    pub(crate) fn scenario_events_since(&self, since: u64, limit: usize) -> Vec<ScenarioEvent> {
        self.inner.scenario.read().events_since(since, limit)
    }

    /// Aggregate metrics for `/api/scenario/report`. Returns a
    /// snapshot. SoC stats are computed at fetch time from each
    /// battery's current telemetry — cheap, no accumulator needed.
    pub(crate) fn scenario_report(&self, now: DateTime<Utc>) -> ScenarioReport {
        use crate::sim::Category;
        let g = self.inner.scenario.read();
        let mut total_charged = 0.0;
        let mut total_discharged = 0.0;
        let per_battery: Vec<PerBatteryReport> = g
            .per_battery()
            .iter()
            .map(|(id, b)| {
                total_charged += b.charge_wh;
                total_discharged += b.discharge_wh;
                PerBatteryReport {
                    id: *id,
                    charge_wh: b.charge_wh,
                    discharge_wh: b.discharge_wh,
                }
            })
            .collect();
        let mut total_pv = 0.0;
        let per_pv: Vec<PerPvReport> = g
            .per_pv()
            .iter()
            .map(|(id, p)| {
                total_pv += p.produced_wh;
                PerPvReport {
                    id: *id,
                    produced_wh: p.produced_wh,
                }
            })
            .collect();
        let grid_window_averages: Vec<WindowAverageEntry> = g
            .window_avgs()
            .iter()
            .map(|(secs, (sum, count))| WindowAverageEntry {
                window_start: DateTime::<Utc>::from_timestamp(*secs, 0).unwrap_or_else(Utc::now),
                avg_w: if *count > 0 {
                    *sum / (*count as f64)
                } else {
                    0.0
                },
            })
            .collect();
        let checks: Vec<ScenarioCheck> = g.checks().cloned().collect();
        let checks_passed = g.checks_passed();
        let checks_failed = g.checks_failed();
        // Read name/elapsed/peak under the SAME guard as the
        // aggregates above. Re-reading them after drop(g) let a
        // scenario-start in between attribute scenario A's numbers
        // to scenario B's name — the exact mix-up the name field
        // exists to prevent.
        let name = g.name.clone();
        let scenario_elapsed_s = g.elapsed_s(now);
        let peak_grid_w = g.peak_grid_active_w();
        // The peak-|Q| pair also carries the P alongside it, so PF is
        // derived from ONE pairing rather than independently-peaked P
        // and Q (which could pair values from different instants).
        let peak_pq = g.peak_grid_pq();
        drop(g);
        let peak_grid_var = peak_pq.map(|(_, q)| q.abs()).unwrap_or(0.0);
        let site_pf_at_peak_var = peak_pq.and_then(|(p, q)| {
            let apparent = (p * p + q * q).sqrt();
            (apparent != 0.0).then(|| p.abs() / apparent)
        });

        // SoC stats: walk every registered battery, read its
        // current SoC. Out-of-band of the journal because it's
        // current state, not an accumulator.
        let mut socs: Vec<f32> = Vec::new();
        for c in self.inner.components.read().iter() {
            if c.category() == Category::Battery
                && let Some(s) = c.telemetry(self).soc_pct
            {
                socs.push(s);
            }
        }
        let soc_stats = compute_soc_stats(&socs);

        ScenarioReport {
            name,
            scenario_elapsed_s,
            peak_grid_w,
            peak_grid_var,
            site_pf_at_peak_var,
            total_battery_charged_wh: total_charged,
            total_battery_discharged_wh: total_discharged,
            total_pv_produced_wh: total_pv,
            per_battery,
            per_pv,
            soc_stats,
            grid_window_averages,
            checks_passed,
            checks_failed,
            checks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compute_soc_stats;
    use crate::sim::component::KnobKind;
    use crate::sim::dynamic_scalar::DynamicScalar;
    use crate::sim::events::SiteEvent;
    use crate::sim::meter::Meter;
    use crate::sim::microgrid_site::MicrogridSite;
    use chrono::Utc;
    use std::time::Duration;

    fn meter_with_power(id: u64, watts: f32) -> Meter {
        Meter::new(
            id,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(watts)),
            None,
            0.0,
            false,
        )
    }

    /// A snapshot taken outside a running scenario is a no-op: it
    /// never lands in the baseline map, so a subsequent run that
    /// itself takes no snapshot has nothing to restore — the
    /// scenario's own drive is what's left standing after stop.
    #[test]
    fn snapshot_outside_a_running_scenario_is_a_noop() {
        let w = MicrogridSite::new();
        w.register(meter_with_power(1, 1000.0));
        let now = Utc::now();

        // No scenario running yet — this must not seed any baseline.
        w.scenario_snapshot_knob(1, KnobKind::MeterPower);

        w.scenario_start("s".into(), now);
        // The scenario drives the meter WITHOUT ever snapshotting it.
        w.get(1)
            .unwrap()
            .meter_drive()
            .unwrap()
            .set_active_power_override(5000.0);
        w.scenario_stop(now, None);

        assert_eq!(
            w.get(1)
                .unwrap()
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            5000.0,
            "nothing was ever snapshotted this run, so stop must not touch the live value"
        );
    }

    /// First-snapshot-wins: repeated `scenario_snapshot_knob` calls on
    /// the same (id, kind) during one run keep the FIRST captured
    /// value, and `scenario_stop` restores to it regardless of how
    /// many times the scenario re-drove the knob in between.
    #[test]
    fn first_snapshot_wins_and_stop_restores_it() {
        let w = MicrogridSite::new();
        w.register(meter_with_power(2, 1200.0));
        let now = Utc::now();

        w.scenario_start("s2".into(), now);
        w.scenario_snapshot_knob(2, KnobKind::MeterPower); // captures 1200.0
        w.get(2)
            .unwrap()
            .meter_drive()
            .unwrap()
            .set_active_power_override(3000.0);
        w.scenario_snapshot_knob(2, KnobKind::MeterPower); // no-op: already captured
        w.get(2)
            .unwrap()
            .meter_drive()
            .unwrap()
            .set_active_power_override(7000.0);

        w.scenario_stop(now, None);
        assert_eq!(
            w.get(2)
                .unwrap()
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            1200.0,
            "restore must land on the FIRST snapshot, not an intermediate drive"
        );
    }

    /// A second `scenario_stop` is a no-op: the baseline map was
    /// already drained by the first call, so nothing is restored and
    /// no `KnobChanged` is rebroadcast.
    #[test]
    fn second_scenario_stop_is_idempotent() {
        let w = MicrogridSite::new();
        w.register(meter_with_power(3, 1500.0));
        let now = Utc::now();

        w.scenario_start("s3".into(), now);
        w.scenario_snapshot_knob(3, KnobKind::MeterPower);
        w.get(3)
            .unwrap()
            .meter_drive()
            .unwrap()
            .set_active_power_override(4000.0);
        w.scenario_stop(now, None);
        assert_eq!(
            w.get(3)
                .unwrap()
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            1500.0
        );

        // Subscribe only AFTER the first stop's broadcasts have
        // already gone out, so the receiver starts empty.
        let mut rx = w.subscribe_events();
        w.scenario_stop(now, None);
        assert!(
            rx.try_recv().is_err(),
            "a second stop must not rebroadcast a restored knob"
        );
    }

    /// `remove_component` prunes any baseline entry captured for that
    /// id: a component re-registered under the same id during a
    /// scenario run must not inherit a stale snapshot from the
    /// component that used to hold that id.
    #[test]
    fn remove_component_prunes_the_knob_baseline() {
        let w = MicrogridSite::new();
        w.register(meter_with_power(4, 100.0));
        let now = Utc::now();

        w.scenario_start("s4".into(), now);
        w.scenario_snapshot_knob(4, KnobKind::MeterPower); // baseline: 100.0
        w.remove_component(4);
        // Fresh component under the same id, never snapshotted this run.
        w.register(meter_with_power(4, 555.0));
        w.scenario_stop(now, None);

        assert_eq!(
            w.get(4)
                .unwrap()
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            555.0,
            "the removed component's stale baseline must not restore onto the fresh one"
        );
    }

    /// `reset()` clears the whole baseline map: a knob captured before
    /// the reset must not resurface in a later run.
    #[test]
    fn reset_clears_the_knob_baseline() {
        let w = MicrogridSite::new();
        w.register(meter_with_power(5, 200.0));
        let now = Utc::now();

        w.scenario_start("s5".into(), now);
        w.scenario_snapshot_knob(5, KnobKind::MeterPower);
        w.get(5)
            .unwrap()
            .meter_drive()
            .unwrap()
            .set_active_power_override(999.0);
        w.reset();

        w.register(meter_with_power(5, 777.0));
        w.scenario_start("s6".into(), now);
        // No snapshot taken in this fresh run.
        w.scenario_stop(now, None);

        assert_eq!(
            w.get(5)
                .unwrap()
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            777.0,
            "a pre-reset baseline must not resurrect after reset()"
        );
    }

    /// Restoring a meter's reactive axis broadcasts BOTH
    /// `meter-reactive-power` and `meter-power-factor` — the same
    /// two-token aliasing `clear-meter-reactive` uses — and restoring
    /// the active axis broadcasts only `meter-power`.
    #[test]
    fn stop_rebroadcasts_the_right_knob_tokens() {
        use crate::sim::meter::ReactiveSource;

        let w = MicrogridSite::new();
        w.register(Meter::new(
            6,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(8_000.0)),
            Some(ReactiveSource::Var(DynamicScalar::constant(500.0))),
            0.0,
            false,
        ));
        let now = Utc::now();
        w.scenario_start("s7".into(), now);
        w.scenario_snapshot_knob(6, KnobKind::MeterPower);
        w.scenario_snapshot_knob(6, KnobKind::MeterReactive);
        w.get(6)
            .unwrap()
            .meter_drive()
            .unwrap()
            .set_active_power_override(1_000.0);
        w.get(6)
            .unwrap()
            .meter_drive()
            .unwrap()
            .set_power_factor(0.8, true);

        let mut rx = w.subscribe_events();
        w.scenario_stop(now, None);

        let mut saw_power = false;
        let mut saw_reactive = false;
        let mut saw_pf = false;
        while let Ok(ev) = rx.try_recv() {
            if let SiteEvent::KnobChanged { id: 6, knob, .. } = ev {
                match knob {
                    "meter-power" => saw_power = true,
                    "meter-reactive-power" => saw_reactive = true,
                    "meter-power-factor" => saw_pf = true,
                    _ => {}
                }
            }
        }
        assert!(saw_power, "expected a meter-power KnobChanged");
        assert!(saw_reactive, "expected a meter-reactive-power KnobChanged");
        assert!(saw_pf, "expected a meter-power-factor KnobChanged");
    }

    #[test]
    fn soc_stats_compute_on_typical_set() {
        let s = compute_soc_stats(&[20.0, 40.0, 60.0, 80.0]).unwrap();
        // Mean of 20, 40, 60, 80 = 50.
        assert!((s.mean_pct - 50.0).abs() < 1e-6);
        // Median (lower of middle two on even count) = 40.
        assert!((s.median_pct - 40.0).abs() < 1e-6);
        // No clear mode — all equal counts at distinct buckets;
        // returns the lowest tied bucket (20).
        assert_eq!(s.mode_pct, Some(20));
    }

    #[test]
    fn soc_stats_mode_picks_repeated_bucket() {
        let s = compute_soc_stats(&[50.0, 50.4, 50.6, 25.0, 80.0]).unwrap();
        // Three SoCs round to 50 (50, 50, 51 — actually 50.6
        // rounds to 51, so mode is 50 with 2 buckets, vs 51, 25,
        // 80 each at 1).
        assert_eq!(s.mode_pct, Some(50));
    }

    #[test]
    fn soc_stats_empty_returns_none() {
        assert!(compute_soc_stats(&[]).is_none());
    }
}
