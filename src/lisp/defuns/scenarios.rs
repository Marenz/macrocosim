//! Scenarios: `(define-scenario …)` for the multi-stage registry +
//! the per-microgrid lifecycle defuns (`scenario-start`,
//! `-stop`, `-event`, `-expect`, `-record-csv`, `-stop-csv`,
//! `-elapsed`, `-running-p`).
//!
//! Both surfaces share data via `MicrogridSite`'s scenario journal;
//! keeping them in one file makes the read-write story obvious.

use tulisp::{Error, TulispContext, TulispObject};

use crate::sim::history::Metric;
use crate::sim::microgrids::SharedSiteRouter;
use crate::sim::scenario::ScenarioCheck;

/// Newtype around `TulispObject` so `Vec<RawForm>` satisfies the
/// AsPlist field bound (which needs `TryFrom<TulispObject, Error =
/// tulisp::Error>`; the blanket impl on `TulispObject` is `Error =
/// Infallible`). Used for the list-valued scenario sections, whose
/// elements are forms produced by the section wrappers (`drive-solar`,
/// `at`, `check`, …) and kept raw for the runner.
pub struct RawForm(tulisp::TulispObject);

impl TryFrom<tulisp::TulispObject> for RawForm {
    type Error = tulisp::Error;
    fn try_from(v: tulisp::TulispObject) -> Result<Self, tulisp::Error> {
        Ok(RawForm(v))
    }
}

impl From<RawForm> for tulisp::TulispObject {
    fn from(v: RawForm) -> tulisp::TulispObject {
        v.0
    }
}

tulisp::AsPlist! {
    pub struct DefineScenarioArgs {
        name: String,
        description: Option<String> {= None},
        /// `relative` (default) or `absolute` — symbol or string.
        schedule: Option<crate::lisp::value::LispValue> {= None},
        /// Default clock driver: `real` (default) or `stepped`.
        clock: Option<crate::lisp::value::LispValue> {= None},
        /// Run length — a human offset string (`"4min"`) or a number
        /// of seconds. `None` runs until stopped.
        length: Option<crate::lisp::value::LispValue> {= None},
        /// Calendar date anchoring an `absolute` schedule, ISO
        /// `YYYY-MM-DD`. `None` falls back to wallclock-today.
        date: Option<String> {= None},
        /// Optional RNG seed (deterministic with a `stepped` clock).
        seed: Option<i64> {= None},
        /// Runs once at start.
        setup: Option<crate::lisp::value::LispValue> {= None},
        /// Continuous environment sources.
        drive: Option<Vec<RawForm>> {= None},
        /// In-sim controllers.
        agents: Option<Vec<RawForm>> {= None},
        /// Discrete timed actions.
        cues: Option<Vec<RawForm>> {= None},
        /// Timed assertions.
        expect: Option<Vec<RawForm>> {= None},
        /// Recording directive (`'csv` or a directory).
        record: Option<crate::lisp::value::LispValue> {= None},
    }
}

/// A single non-nil form from a `LispValue` section arg, or `None`.
fn opt_form(v: Option<crate::lisp::value::LispValue>) -> Option<tulisp::TulispObject> {
    v.map(crate::lisp::value::LispValue::into_inner)
        .filter(|o| !o.null())
}

/// A list-valued section's non-nil forms.
fn form_list(v: Option<Vec<RawForm>>) -> Vec<tulisp::TulispObject> {
    v.unwrap_or_default()
        .into_iter()
        .map(|r| r.0)
        .filter(|o| !o.null())
        .collect()
}

/// Resolve a `:schedule` / `:clock` plist value (symbol or string) to
/// its name, e.g. `'relative` → `"relative"`.
fn sym_name(o: &tulisp::TulispObject) -> Result<String, tulisp::Error> {
    if o.symbolp() {
        Ok(o.to_string())
    } else {
        String::try_from(o.clone())
    }
}

/// `(define-scenario …)` parses the unified scenario model into the
/// registry shared with the UI Scenarios panel + the runners (§J2).
pub(in crate::lisp) fn register_registry(
    ctx: &mut TulispContext,
    scenarios: crate::sim::scenarios::SharedScenarios,
) {
    use crate::sim::scenarios::{ClockDriver, ScenarioDef, Schedule};
    use crate::sim::sim_clock::parse_offset;
    ctx.defun(
        "define-scenario",
        move |_ctx: &mut TulispContext,
              args: tulisp::Plist<DefineScenarioArgs>|
              -> Result<String, tulisp::Error> {
            let a = args.into_inner();

            let schedule = match opt_form(a.schedule) {
                None => Schedule::Relative,
                Some(o) => {
                    let s = sym_name(&o)?;
                    Schedule::parse(&s).ok_or_else(|| {
                        tulisp::Error::os_error(format!(
                            "define-scenario: :schedule must be 'relative or 'absolute, got {s:?}"
                        ))
                    })?
                }
            };
            let clock = match opt_form(a.clock) {
                None => ClockDriver::Real,
                Some(o) => {
                    let s = sym_name(&o)?;
                    ClockDriver::parse(&s).ok_or_else(|| {
                        tulisp::Error::os_error(format!(
                            "define-scenario: :clock must be 'real or 'stepped, got {s:?}"
                        ))
                    })?
                }
            };
            let length_s = match opt_form(a.length) {
                None => None,
                Some(o) if o.numberp() => Some(f64::try_from(o)?),
                Some(o) => {
                    let s = String::try_from(o)?;
                    Some(parse_offset(&s).map(|d| d.as_secs_f64()).ok_or_else(|| {
                        tulisp::Error::os_error(format!(
                            "define-scenario: :length must be a human offset or seconds, got {s:?}"
                        ))
                    })?)
                }
            };
            let date = match a.date.as_deref() {
                None => None,
                Some(s) => Some(
                    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|e| {
                        tulisp::Error::os_error(format!(
                            "define-scenario: :date must be YYYY-MM-DD; got {s:?} ({e})"
                        ))
                    })?,
                ),
            };

            let cues = form_list(a.cues);
            let expect = form_list(a.expect);
            let timeline = crate::sim::scenarios::build_timeline(&cues, &expect);
            let def = ScenarioDef {
                name: a.name.clone(),
                description: a.description.unwrap_or_default(),
                schedule,
                clock,
                length_s,
                date,
                seed: a.seed,
                setup: opt_form(a.setup),
                drive: form_list(a.drive),
                agents: form_list(a.agents),
                cues,
                expect,
                record: opt_form(a.record),
                timeline,
            };
            scenarios.lock().insert(a.name.clone(), def);
            Ok(a.name)
        },
    );
}

tulisp::AsPlist! {
    pub struct ScenarioExpectArgs {
        component: i64,
        /// Metric to read — symbol or string. Dashes normalize to
        /// underscores, so both the canonical `Metric::as_str` names
        /// and lisp-style spellings work; see `parse_expect_metric`
        /// for the shorthand aliases (`soc`, `active-power`,
        /// `active-power-bounds-lower`, …).
        metric: crate::lisp::value::LispValue,
        approx: Option<f64> {= None},
        tol: Option<f64> {= None},
        min: Option<f64> {= None},
        max: Option<f64> {= None},
    }
}

/// What `(scenario-expect …)` compares the observed value against.
enum Expectation {
    Approx { center: f64, tol: f64 },
    Range { min: Option<f64>, max: Option<f64> },
}

impl Expectation {
    fn passes(&self, v: f64) -> bool {
        match self {
            Self::Approx { center, tol } => (v - center).abs() <= *tol,
            Self::Range { min, max } => min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m),
        }
    }

    /// Human-readable form recorded on the check (and shown by
    /// `macroctl scenario report` on failure).
    fn describe(&self) -> String {
        match self {
            Self::Approx { center, tol } => format!("approx {center} (tol {tol})"),
            Self::Range {
                min: Some(l),
                max: Some(u),
            } => format!("in [{l}, {u}]"),
            Self::Range { min: Some(l), .. } => format!(">= {l}"),
            Self::Range { max: Some(u), .. } => format!("<= {u}"),
            Self::Range { .. } => unreachable!("validated at construction"),
        }
    }
}

/// Resolve a lisp-side metric name. Dashes normalize to underscores
/// first, so the canonical names (`active_power_w`, …) and their
/// lisp spellings both parse; on top of that a few shorthands map
/// to the obvious metric — `soc`, `frequency`, `active-power`, and
/// the `…-bounds-lower` / `…-bounds-upper` family from the todo's
/// motivating example.
fn parse_expect_metric(name: &str) -> Option<Metric> {
    let n = name.replace('-', "_");
    if let Ok(m) = n.parse::<Metric>() {
        return Some(m);
    }
    Some(match n.as_str() {
        "active_power" => Metric::ActivePowerW,
        "reactive_power" => Metric::ReactivePowerVar,
        "dc_power" => Metric::DcPowerW,
        "soc" => Metric::SocPct,
        "energy" => Metric::EnergyWh,
        "frequency" => Metric::FrequencyHz,
        "active_power_bounds_lower" => Metric::ActivePowerLowerBoundW,
        "active_power_bounds_upper" => Metric::ActivePowerUpperBoundW,
        "reactive_power_bounds_lower" => Metric::ReactivePowerLowerBoundVar,
        "reactive_power_bounds_upper" => Metric::ReactivePowerUpperBoundVar,
        _ => return None,
    })
}

/// Scenario lifecycle defuns. Scripts call `(scenario-start NAME)`
/// to mark the beginning, drop `(scenario-event KIND PAYLOAD)` markers
/// at interesting moments, assert state via `(scenario-expect …)`,
/// and `(scenario-stop)` when finished. The underlying journal lives
/// on `MicrogridSite` and is read by the `/api/scenario` and
/// `/api/scenario/events` endpoints.
///
/// POLICY — `scenario-stop` returns every driven knob (a meter's
/// `:power` / `:reactive-power` / power-factor override, a solar
/// inverter's `:sunlight%`, a boiler's `:demand`) to its
/// PRE-SCENARIO state: the value/source it had the moment BEFORE the
/// scenario first touched it, captured by `scenario_snapshot_knob`.
/// This holds even over a mid-scenario manual poke or
/// `clear-meter-*` — first-snapshot-wins means only the very first
/// pre-scenario capture matters, and anything that happened to the
/// knob after that (the scenario's own drive, a cue re-driving it, a
/// user poking it through the UI while the scenario is running) is
/// simply gone once `scenario-stop` runs. Simple and uniform: no
/// attempt to distinguish "the scenario's change" from "the user's
/// change" once both have landed on the same knob during the same
/// run.
///
/// EVERY door onto those knobs takes that snapshot, on the same
/// first-touch rule: the `set-meter-power` &c. Lisp defuns and the
/// two `clear-meter-*` defuns (`src/lisp/defuns/load_drivers.rs`),
/// and the typed `POST /api/component/:id/drive` route
/// (`src/ui/handlers/control.rs`). No door is exempt — one that was
/// would not merely leak its own poke past teardown, it would let a
/// first-touch poke through it be captured as the "pre-scenario"
/// baseline by a later drive of the same knob, and teardown would
/// then restore the poke rather than the state that preceded it.
///
/// `scenario-stop` also cancels the timers the running scenario
/// armed (agents, cues, checks, an outage chain — see
/// `scenario--cancel-timers` in `sim/scenarios.lisp`) before the knob
/// restore below, so nothing is left running to immediately re-drive
/// a knob this just put back. And `scenario-start` runs the whole
/// teardown itself when a scenario is still going, so two runs never
/// overlap and leave the first one's knobs displaced with no baseline
/// left to restore them from — that guard lives on the defun, so it
/// covers a bare `(scenario-start …)` from a script or the REPL, not
/// just a `define-scenario` run through `scenario--run`.
pub(super) fn register_lifecycle(
    ctx: &mut TulispContext,
    router: SharedSiteRouter,
    microgrids: crate::sim::microgrids::SharedMicrogrids,
    now: crate::sim::sim_clock::NowSource,
) {
    // scenario-start / scenario-stop fan out across every registered
    // microgrid, matching the HTTP scenario lifecycle — a REPL- or
    // timer-driven scenario would otherwise journal only the current
    // microgrid, leaving the others' journals open and per-mg reports
    // diverging. The per-event defuns below (scenario-event, -expect,
    // -record-csv, …) stay scoped to the current site: an event
    // belongs to the microgrid whose context emitted it. With an
    // empty registry (test fixtures, the tags pass) both fall back to
    // the router-resolved site.
    let reg = microgrids.clone();
    let r = router.clone();
    let nowsrc = now.clone();
    ctx.defun(
        "scenario-start",
        move |ctx: &mut TulispContext, name: String| -> Result<bool, Error> {
            let now = nowsrc.now();
            let sites: Vec<_> = reg.lock().values().map(|e| e.site.clone()).collect();
            // A run still in progress is torn down FIRST, here, in the
            // defun every start path goes through — `scenario--run`,
            // the UI's start route, a bare `(scenario-start …)` from a
            // script or the REPL. It has to be here and not in
            // `scenario--run`: the fan-out below clears each site's
            // knob-restore baseline, so a start over a running
            // scenario without this would strand that scenario's
            // displaced knobs with nothing left to restore them from,
            // and leave its agents and cues firing into the new run.
            //
            // The full `(scenario-stop)` — not just the Rust half —
            // so the previous run's timers are cancelled too. Same
            // `ctx.eval`-on-a-built-form reason as `scenario-stop`'s
            // own call into `scenario--cancel-timers`: `eval_string`
            // would reset `eval_depth` re-entrantly.
            //
            // Termination is structural, not a matter of this check:
            // stopping a scenario never starts one, so there is no
            // cycle to recurse through. What `scenario_stop`'s
            // ended-first ordering buys is a different hazard —
            // its refresh pass runs Lisp (a restored lambda source),
            // and a callback that reached back in here would find
            // `scenario_is_running()` already false rather than
            // re-entering the teardown of a run being torn down.
            //
            // Errors are logged, not propagated: a teardown that
            // somehow fails must not also prevent the new run from
            // starting.
            let running = if sites.is_empty() {
                r.site().scenario_is_running()
            } else {
                sites.iter().any(|s| s.scenario_is_running())
            };
            if running {
                let stop_call: TulispObject =
                    vec![ctx.intern("scenario-stop")].into_iter().collect();
                if let Err(e) = ctx.eval(&stop_call) {
                    log::warn!(
                        "scenario-start: tearing down the running scenario failed: {}",
                        e.format(ctx)
                    );
                }
            }
            if sites.is_empty() {
                r.site().scenario_start(name, now);
            } else {
                for site in sites {
                    site.scenario_start(name.clone(), now);
                }
            }
            Ok(true)
        },
    );

    let reg = microgrids.clone();
    let r = router.clone();
    let nowsrc = now.clone();
    ctx.defun(
        "scenario-stop",
        move |ctx: &mut TulispContext| -> Result<bool, Error> {
            // Cancel the scenario's OWN agent/cue/check timers (see
            // `scenario--cancel-timers` in sim/scenarios.lisp) BEFORE
            // restoring driven knobs below: an agent left running past
            // this point could re-drive a knob a heartbeat after
            // restore puts it back, undoing the restore.
            //
            // `ctx.eval`, NOT `ctx.eval_string`: this defun body runs
            // WHILE `(scenario-stop)` itself is being evaluated, and
            // `eval_string` unconditionally resets `eval_depth` to 0
            // as its top-level entry point — fine called from Rust
            // between evals (`reload`'s own `(cancel-timers)` call),
            // but re-entrant here it corrupts the outer eval's depth
            // bookkeeping and the bytecode interpreter panics
            // ("attempt to subtract with overflow") once IT tries to
            // unwind. Building the call form and using `ctx.eval`
            // (which doesn't touch `eval_depth`) avoids the hazard —
            // same pattern `sim::scenarios::start` uses to invoke
            // `scenario--run` from Rust.
            //
            // Errors are logged rather than propagated — a teardown
            // must still restore knobs even if cancellation itself
            // somehow fails (e.g. scenarios.lisp wasn't loaded in
            // this context).
            let cancel_call: TulispObject = vec![ctx.intern("scenario--cancel-timers")]
                .into_iter()
                .collect();
            if let Err(e) = ctx.eval(&cancel_call) {
                log::warn!(
                    "scenario-stop: scenario--cancel-timers failed: {}",
                    e.format(ctx)
                );
            }
            let now = nowsrc.now();
            let sites: Vec<_> = reg.lock().values().map(|e| e.site.clone()).collect();
            // `ctx` goes through to the restore so each reinstalled
            // dynamic source re-resolves before its value is read back
            // and broadcast — see `MicrogridSite::scenario_stop`'s
            // three phases. This defun is the only stop path with an
            // interpreter in hand, and it is the path every real stop
            // takes (Lisp, the UI's POST /api/scenarios/stop, and the
            // stepped runner all funnel through `(scenario-stop)`).
            if sites.is_empty() {
                r.site().scenario_stop(now, Some(ctx));
            } else {
                for site in sites {
                    site.scenario_stop(now, Some(&mut *ctx));
                }
            }
            Ok(true)
        },
    );

    let r = router.clone();
    let nowsrc = now.clone();
    ctx.defun(
        "scenario-event",
        move |kind: TulispObject, payload: TulispObject| -> Result<i64, Error> {
            let w = r.site();
            // Accept either a string or a symbol for `kind` so
            // scripts can write `(scenario-event 'outage "bat-1003")`
            // alongside `(scenario-event "note" "warming up")`.
            // Payload renders via Display so any Lisp value works.
            let kind_str = sym_name(&kind)?;
            let payload_str = payload.to_string();
            let id = w.scenario_record(kind_str, payload_str, nowsrc.now());
            Ok(id as i64)
        },
    );

    // `(scenario-expect :component ID :metric M :approx V :tol T)` /
    // `(… :min L :max U)` — read the component's current value of M,
    // compare, and record a pass/fail check on the scenario report.
    // Returns t/nil so scripts can branch. A missing component or
    // unpublished metric records a *failure* (it's a runtime
    // condition a test should catch); an unknown metric name or a
    // malformed comparator is a script bug and errors instead.
    let r = router.clone();
    let nowsrc = now.clone();
    ctx.defun(
        "scenario-expect",
        move |_ctx: &mut TulispContext,
              args: tulisp::Plist<ScenarioExpectArgs>|
              -> Result<bool, Error> {
            let a = args.into_inner();
            let metric_obj = a.metric.into_inner();
            let metric_name = sym_name(&metric_obj)?;
            let metric = parse_expect_metric(&metric_name).ok_or_else(|| {
                Error::os_error(format!("scenario-expect: unknown metric {metric_name:?}"))
            })?;
            let expectation = match (a.approx, a.min, a.max) {
                (Some(center), None, None) => Expectation::Approx {
                    center,
                    // Exact float equality is almost never what a
                    // scenario means; a small absolute default keeps
                    // a tol-less :approx from being a footgun.
                    tol: a.tol.unwrap_or(1e-3),
                },
                (None, min, max) if min.is_some() || max.is_some() => {
                    if a.tol.is_some() {
                        return Err(Error::os_error(
                            "scenario-expect: :tol only applies to :approx".to_string(),
                        ));
                    }
                    Expectation::Range { min, max }
                }
                _ => {
                    return Err(Error::os_error(
                        "scenario-expect: pass either :approx (with optional :tol) \
                         or :min / :max"
                            .to_string(),
                    ));
                }
            };
            let id = u64::try_from(a.component).map_err(|_| {
                Error::os_error(format!(
                    "scenario-expect: :component must be non-negative, got {}",
                    a.component
                ))
            })?;
            let w = r.site();
            // Energy is cumulative — integrated on the physics tick and held
            // in the site's per-component accumulator (so it reads back in a
            // headless stepped run too), not on the instantaneous snapshot.
            // Checks compare the energy accrued since `scenario-start` (the
            // baseline the journal snapshotted), so a long-running live
            // server behaves like a fresh stepped run. Compare on the
            // full-precision f64; `actual` keeps the f32 the
            // history/`ScenarioCheck` layer records.
            let (passed, actual) = if metric == Metric::EnergyWh {
                let e = w.component_energy_since_scenario_wh(id);
                (
                    e.is_some_and(|v| expectation.passes(v)),
                    e.map(|v| v as f32),
                )
            } else {
                let v = w
                    .get(id)
                    .and_then(|c| w.telemetry_of(c.as_ref()).metric_value(metric));
                (v.is_some_and(|v| expectation.passes(v as f64)), v)
            };
            w.scenario_record_check(ScenarioCheck {
                ts: nowsrc.now(),
                component_id: id,
                metric: metric.as_str().into(),
                expectation: expectation.describe(),
                actual,
                passed,
            });
            Ok(passed)
        },
    );

    let r = router.clone();
    ctx.defun(
        "scenario-record-csv",
        move |dir: String| -> Result<i64, Error> {
            let w = r.site();
            let path = std::path::PathBuf::from(dir);
            w.scenario_open_csv(&path)
                .map(|n| n as i64)
                .map_err(|e| Error::os_error(format!("scenario-record-csv: {e}")))
        },
    );

    let r = router.clone();
    ctx.defun("scenario-stop-csv", move || -> Result<i64, Error> {
        let w = r.site();
        Ok(w.scenario_close_csv() as i64)
    });

    // `(scenario-running-p)` — t while a scenario is in progress
    // (started, not yet stopped). Site-scoped like `scenario-elapsed`
    // beside it, and read straight off the journal: it IS the
    // "a scenario is running" truth `scenario_snapshot_knob` gates on,
    // so `sim/scenarios.lisp`'s timer tracking can key on it directly
    // instead of shadowing it with a flag of its own that a crash or a
    // mid-run reload could leave out of sync.
    let r = router.clone();
    ctx.defun("scenario-running-p", move || -> Result<bool, Error> {
        Ok(r.site().scenario_is_running())
    });

    let r = router;
    let nowsrc = now;
    ctx.defun("scenario-elapsed", move || -> Result<f64, Error> {
        let w = r.site();
        Ok(w.scenario_elapsed_s(nowsrc.now()))
    });
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;

    /// `(scenario-record-csv DIR)` opens one telemetry CSV per
    /// registered component — plus a setpoints + bounds CSV per
    /// envelope-bearing component; record_history_snapshot writes a
    /// telemetry row per pass; `(scenario-stop-csv)` flushes and
    /// closes them. Test asserts the file exists and contains a
    /// header + N rows.
    #[test]
    fn scenario_csv_records_per_component_files() {
        use chrono::Utc;
        let (cfg, dir) = config_with(
            "(%make-meter :id 1)
             (%make-battery :id 2)",
        );
        let csv_dir = dir.join("csvs");
        cfg.eval("(scenario-start \"csv\")").unwrap();
        let opened: i64 = cfg
            .eval(&format!(
                "(scenario-record-csv {:?})",
                csv_dir.to_str().unwrap()
            ))
            .unwrap()
            .parse()
            .unwrap();
        // Two telemetry files + the battery's setpoints + bounds
        // files; the meter reports no envelope so it gets neither.
        assert_eq!(opened, 4);
        assert!(!csv_dir.join("1-setpoints.csv").exists());
        assert!(!csv_dir.join("1-bounds.csv").exists());
        // Three snapshots → three rows + header.
        for _ in 0..3 {
            cfg.site().record_history_snapshot(Utc::now());
        }
        cfg.eval("(scenario-stop-csv)").unwrap();

        let meter_csv = std::fs::read_to_string(csv_dir.join("1-meter.csv")).unwrap();
        let battery_csv = std::fs::read_to_string(csv_dir.join("2-battery.csv")).unwrap();
        // Header line + 3 data rows = 4 lines (last one ends in
        // newline so split gives 5 elements with trailing empty).
        assert_eq!(meter_csv.lines().count(), 4, "meter csv: {meter_csv}");
        assert_eq!(battery_csv.lines().count(), 4, "battery csv: {battery_csv}");
        assert!(meter_csv.starts_with("ts_iso,active_power_w"));
        // Battery rows have an empty active_power_w cell (it
        // publishes dc_power_w instead) — the column shape stays
        // uniform.
        let first_data = battery_csv.lines().nth(1).unwrap();
        assert!(
            first_data.starts_with("20") && first_data.contains(",,"),
            "expected empty active_power cell, got {first_data}"
        );
    }

    /// The setpoints CSV gets one row per `log_setpoint` (event-
    /// driven), the bounds CSV one row per snapshot pass (sampled) —
    /// the inputs a bound-setting control app pushed plus the
    /// envelope it produced, replayable without a log scrape.
    #[test]
    fn scenario_csv_records_setpoints_and_bounds() {
        use crate::sim::setpoints::{SetpointEvent, SetpointKind, SetpointOutcome};
        use chrono::Utc;
        let (cfg, dir) = config_with(
            "(%make-battery :id 2
                            :capacity 100000.0
                            :rated-lower -10000.0
                            :rated-upper 10000.0)",
        );
        let csv_dir = dir.join("csvs");
        cfg.eval("(scenario-start \"io\")").unwrap();
        cfg.eval(&format!(
            "(scenario-record-csv {:?})",
            csv_dir.to_str().unwrap()
        ))
        .unwrap();

        let now = Utc::now();
        cfg.site().log_setpoint(
            2,
            SetpointEvent {
                ts: now,
                kind: SetpointKind::ActivePower,
                value: 1500.0,
                ttl_s: Some(60),
                outcome: SetpointOutcome::Accepted {
                    effective_value: Some(1500.0),
                },
            },
        );
        cfg.site().log_setpoint(
            2,
            SetpointEvent {
                ts: now,
                kind: SetpointKind::AugmentBounds,
                value: 0.0,
                ttl_s: Some(30),
                outcome: SetpointOutcome::Rejected {
                    reason: "augmentation bound [a, b] is inverted".into(),
                },
            },
        );
        // Two sampling passes → two bounds rows.
        cfg.site().record_history_snapshot(now);
        cfg.site().record_history_snapshot(now);
        cfg.eval("(scenario-stop-csv)").unwrap();

        let setpoints = std::fs::read_to_string(csv_dir.join("2-setpoints.csv")).unwrap();
        let mut lines = setpoints.lines();
        assert_eq!(
            lines.next().unwrap(),
            "ts_iso,kind,value,ttl_s,accepted,effective_value,reason"
        );
        let accepted = lines.next().unwrap();
        assert!(
            accepted.contains(",active_power,1500,60,true,1500,"),
            "accepted row: {accepted}"
        );
        let rejected = lines.next().unwrap();
        // The reason holds a comma, so it must arrive CSV-quoted.
        assert!(
            rejected
                .contains(",augment_bounds,0,30,false,,\"augmentation bound [a, b] is inverted\""),
            "rejected row: {rejected}"
        );
        assert!(lines.next().is_none());

        let bounds = std::fs::read_to_string(csv_dir.join("2-bounds.csv")).unwrap();
        let rows: Vec<&str> = bounds.lines().collect();
        assert_eq!(rows[0], "ts_iso,lower_w,upper_w,bands");
        assert_eq!(rows.len(), 3, "bounds csv: {bounds}");
        // Fresh battery at default SoC — effective == rated.
        assert!(
            rows[1].ends_with(",-10000,10000,-10000:10000"),
            "bounds row: {}",
            rows[1]
        );
    }

    /// The reactive-bounds CSV mirrors the active-bounds one: opened
    /// for components with a Q axis, one row per sampling pass, same
    /// row shape (outer hull + `|`-joined bands) but in VAr and named
    /// `<id>-reactive-bounds.csv`.
    #[test]
    fn reactive_bounds_csv_records_the_live_q_envelope() {
        use chrono::Utc;
        let (cfg, dir) = config_with(
            "(setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :reactive-pf-limit 0
                                       :reactive-apparent-va 5000.0
                                       :reactive-command-delay-ms 0
                                       :reactive-ramp-rate 1e9
                                       :successors (list b1))",
        );
        let csv_dir = dir.join("csvs");
        cfg.eval("(scenario-start \"q\")").unwrap();
        cfg.eval(&format!(
            "(scenario-record-csv {:?})",
            csv_dir.to_str().unwrap()
        ))
        .unwrap();
        cfg.site().record_history_snapshot(Utc::now());
        cfg.eval("(scenario-stop-csv)").unwrap();

        let path = csv_dir.join("2-reactive-bounds.csv");
        assert!(path.exists(), "expected {path:?} to exist");
        let contents = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<&str> = contents.lines().collect();
        assert_eq!(rows[0], "ts_iso,lower_var,upper_var,bands");
        assert_eq!(rows.len(), 2, "reactive bounds csv: {contents}");
        // At idle P the caps band is the full ±5 kVAr envelope.
        assert!(
            rows[1].ends_with(",-5000,5000,-5000:5000"),
            "reactive bounds row: {}",
            rows[1]
        );
    }

    /// The report carries the grid stream's peak |Q| and the power
    /// factor at that instant, paired against the last P sample — the
    /// Q twin of `grid_peak_tracks_active_power` above.
    #[test]
    fn report_carries_peak_q_and_pf_at_peak() {
        use chrono::Utc;
        let (cfg, _dir) = config_with(
            "(%make-grid-connection-point
               :id 1
               :successors (list (%make-meter :id 2 :power 3000.0)))",
        );
        cfg.eval("(scenario-start \"pf\")").unwrap();
        cfg.site().record_grid_power_sample(3000.0, Utc::now());
        cfg.site().record_grid_reactive_sample(4000.0, Utc::now());
        let r = cfg.site().scenario_report(Utc::now());
        assert!(
            (r.peak_grid_var - 4000.0).abs() < 1e-3,
            "expected peak |Q| ~4000, got {}",
            r.peak_grid_var
        );
        // pf = |P| / sqrt(P^2 + Q^2) = 3000 / 5000 = 0.6.
        let expected_pf = 3000.0f64 / (3000.0f64.powi(2) + 4000.0f64.powi(2)).sqrt();
        let pf = r
            .site_pf_at_peak_var
            .expect("pf should be present after a sample");
        assert!(
            (pf - expected_pf).abs() < 1e-6,
            "expected pf ~{expected_pf}, got {pf}"
        );

        // A fresh scenario with no samples yet reports no PF.
        cfg.eval("(scenario-start \"fresh\")").unwrap();
        assert!(
            cfg.site()
                .scenario_report(Utc::now())
                .site_pf_at_peak_var
                .is_none()
        );
    }

    /// `(scenario-expect …)` reads the component's current value,
    /// returns t/nil, and records pass/fail (with the failure
    /// detail) on the scenario report.
    #[test]
    fn scenario_expect_records_checks_in_report() {
        let (cfg, _dir) = config_with(
            "(%make-battery :id 2
                            :capacity 100000.0
                            :rated-lower -10000.0
                            :rated-upper 10000.0)",
        );
        cfg.eval("(scenario-start \"checks\")").unwrap();

        // Bounds metric, lisp-style spelling from the todo example.
        let v = cfg
            .eval(
                "(scenario-expect :component 2
                                  :metric 'active-power-bounds-upper
                                  :approx 10000.0 :tol 1.0)",
            )
            .unwrap();
        assert_eq!(v, "t");
        // Range form on a shorthand metric.
        assert_eq!(
            cfg.eval("(scenario-expect :component 2 :metric 'soc :min 0.0 :max 100.0)")
                .unwrap(),
            "t"
        );
        // A failing range: SoC can't be >= 200 %.
        assert_eq!(
            cfg.eval("(scenario-expect :component 2 :metric 'soc :min 200.0)")
                .unwrap(),
            "nil"
        );
        // Unknown component records a failure (actual unavailable),
        // not an error — the asserted-on component vanishing IS the
        // kind of regression a scenario test exists to catch.
        assert_eq!(
            cfg.eval("(scenario-expect :component 99 :metric 'soc :min 0.0)")
                .unwrap(),
            "nil"
        );

        let report = cfg.site().scenario_report(chrono::Utc::now());
        assert_eq!(report.checks_passed, 2);
        assert_eq!(report.checks_failed, 2);
        assert_eq!(report.checks.len(), 4);
        let soc_fail = &report.checks[2];
        assert_eq!(soc_fail.component_id, 2);
        assert_eq!(soc_fail.metric, "soc_pct");
        assert_eq!(soc_fail.expectation, ">= 200");
        assert!(soc_fail.actual.is_some());
        assert!(!soc_fail.passed);
        let missing = &report.checks[3];
        assert_eq!(missing.actual, None);
        assert!(!missing.passed);

        // A scenario restart clears the slate.
        cfg.eval("(scenario-start \"fresh\")").unwrap();
        let report = cfg.site().scenario_report(chrono::Utc::now());
        assert_eq!(report.checks_passed + report.checks_failed, 0);
    }

    /// Script bugs error instead of recording a check: unknown
    /// metric names, a comparator-less call, mixing :approx with
    /// :min/:max, and :tol without :approx.
    #[test]
    fn scenario_expect_rejects_malformed_calls() {
        let (cfg, _dir) = config_with("(%make-battery :id 2 :capacity 1000.0)");
        for bad in [
            "(scenario-expect :component 2 :metric 'warp-factor :min 1.0)",
            "(scenario-expect :component 2 :metric 'soc)",
            "(scenario-expect :component 2 :metric 'soc :approx 5.0 :min 1.0)",
            "(scenario-expect :component 2 :metric 'soc :min 1.0 :tol 0.5)",
            "(scenario-expect :component -2 :metric 'soc :min 1.0)",
        ] {
            assert!(cfg.eval(bad).is_err(), "expected an error from {bad}");
        }
        // Nothing recorded.
        let report = cfg.site().scenario_report(chrono::Utc::now());
        assert_eq!(report.checks_passed + report.checks_failed, 0);
    }

    /// sim/scenarios.lisp loads cleanly and the random-* helpers
    /// produce values in their stated range.
    #[test]
    fn scenarios_helpers_load_and_run() {
        let (cfg, dir) = config_with("");
        // Copy sim/scenarios.lisp into the test's load dir so
        // (load "sim/scenarios.lisp") finds it.
        let src = std::path::Path::new("sim/scenarios.lisp");
        let dst_dir = dir.join("sim");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::copy(src, dst_dir.join("scenarios.lisp")).unwrap();
        cfg.eval("(load \"sim/scenarios.lisp\")").unwrap();
        // 100 draws of random-uniform should all land in [10, 20).
        for _ in 0..100 {
            let v: f64 = cfg
                .eval("(random-uniform 10.0 20.0)")
                .unwrap()
                .parse()
                .unwrap();
            assert!((10.0..20.0).contains(&v), "out-of-range {v}");
        }
        // random-pick over a 3-element list always returns one of
        // them.
        for _ in 0..100 {
            let v = cfg.eval("(random-pick '(11 22 33))").unwrap();
            assert!(["11", "22", "33"].contains(&v.as_str()), "got {v}");
        }
        // random-pick on empty list returns nil.
        assert_eq!(cfg.eval("(random-pick '())").unwrap(), "nil");
    }

    /// The `define-scenario` section wrappers build introspectable
    /// plists (and, for `event`, a thunk): drive-meter / drive-solar
    /// tag their kind + target + source; controller resolves :every to
    /// ms; at / check resolve human times to seconds (offset and
    /// clock-time forms both); event yields a callable that journals.
    #[test]
    fn section_wrappers_build_introspectable_data() {
        let (cfg, dir) = config_with("");
        let src = std::path::Path::new("sim/scenarios.lisp");
        let dst_dir = dir.join("sim");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::copy(src, dst_dir.join("scenarios.lisp")).unwrap();
        cfg.eval("(load \"sim/scenarios.lisp\")").unwrap();

        // drive-meter / drive-solar shape.
        assert_eq!(
            cfg.eval("(plist-get (drive-meter 100 2000.0) :kind)")
                .unwrap(),
            "drive-meter"
        );
        assert_eq!(
            cfg.eval("(plist-get (drive-meter 100 2000.0) :target)")
                .unwrap(),
            "100"
        );
        assert_eq!(
            cfg.eval("(plist-get (drive-solar 200 50.0) :kind)")
                .unwrap(),
            "drive-solar"
        );

        let f = |e: &str| -> f64 { cfg.eval(e).unwrap().parse().unwrap() };

        // controller resolves :every to milliseconds; defaults to 100ms.
        assert_eq!(
            cfg.eval("(plist-get (controller 'ems :every \"500ms\" (lambda () nil)) :id)")
                .unwrap(),
            "ems"
        );
        assert_eq!(
            f("(plist-get (controller 'ems :every \"500ms\" (lambda () nil)) :every-ms)"),
            500.0
        );
        assert_eq!(
            f("(plist-get (controller 'ems (lambda () nil)) :every-ms)"),
            100.0
        );

        // at / check resolve relative offsets and clock times.
        assert_eq!(f("(plist-get (at \"60s\" (lambda () nil)) :at-s)"), 60.0);
        assert_eq!(
            f("(plist-get (check \"02:00\" :component 2 :metric 'soc :min 0.0) :at-s)"),
            7200.0
        );

        // event yields a thunk that journals a scenario-event when run.
        cfg.eval("(scenario-start \"wrap\")").unwrap();
        cfg.eval("(funcall (event 'clouds \"rolling in\"))")
            .unwrap();
        let events = cfg.site().scenario_events_since(0, 10);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "clouds");
        assert!(events[0].payload.contains("rolling in"));
    }

    /// `drive-meter-reactive` / `drive-meter-pf` tag their kind +
    /// target (+ pf for the latter), mirroring `drive-meter`'s plist
    /// shape — `scenario--drive` dispatches on `:kind`.
    #[test]
    fn reactive_drive_wrappers_tag_their_kind() {
        let (cfg, dir) = config_with("");
        let src = std::path::Path::new("sim/scenarios.lisp");
        let dst_dir = dir.join("sim");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::copy(src, dst_dir.join("scenarios.lisp")).unwrap();
        cfg.eval("(load \"sim/scenarios.lisp\")").unwrap();

        assert_eq!(
            cfg.eval("(plist-get (drive-meter-reactive 100 500.0) :kind)")
                .unwrap(),
            "drive-meter-reactive"
        );
        assert_eq!(
            cfg.eval("(plist-get (drive-meter-reactive 100 500.0) :target)")
                .unwrap(),
            "100"
        );

        assert_eq!(
            cfg.eval("(plist-get (drive-meter-pf 100 0.8) :kind)")
                .unwrap(),
            "drive-meter-pf"
        );
        assert_eq!(
            cfg.eval("(plist-get (drive-meter-pf 100 0.8) :pf)")
                .unwrap(),
            "0.8"
        );
        assert_eq!(
            cfg.eval("(plist-get (drive-meter-pf 100 0.8) :leading)")
                .unwrap(),
            "nil"
        );
        assert_eq!(
            cfg.eval("(plist-get (drive-meter-pf 100 0.8 t) :leading)")
                .unwrap(),
            "t"
        );
    }

    /// `define-scenario` extracts a sorted cue + check timeline from
    /// the `at` / `check` wrappers, with each entry's relative time and
    /// (for checks) the asserted component + metric — what the UI run
    /// view renders and correlates report checks against.
    #[test]
    fn define_scenario_extracts_timeline() {
        use crate::sim::scenarios::TimelineKind;
        let (cfg, dir) = config_with("");
        let src = std::path::Path::new("sim/scenarios.lisp");
        let dst_dir = dir.join("sim");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::copy(src, dst_dir.join("scenarios.lisp")).unwrap();
        cfg.eval("(load \"sim/scenarios.lisp\")").unwrap();
        cfg.eval(
            r#"
            (define-scenario :name "t" :schedule 'relative :length "3min"
              :cues (list (at "60s" (lambda () nil))
                          (at "10s" (lambda () nil)))
              :expect (list (check "120s" :component 2 :metric 'active-power
                                   :approx 5000.0 :tol 100.0)))
            "#,
        )
        .unwrap();
        let regs = cfg.scenarios();
        let r = regs.lock();
        let tl = &r.get("t").unwrap().timeline;
        // Sorted by time: cue@10, cue@60, check@120.
        assert_eq!(tl.len(), 3);
        assert_eq!(tl[0].at_s, 10.0);
        assert_eq!(tl[0].kind, TimelineKind::Cue);
        assert_eq!(tl[1].at_s, 60.0);
        assert_eq!(tl[2].at_s, 120.0);
        assert_eq!(tl[2].kind, TimelineKind::Check);
        assert_eq!(tl[2].component, Some(2));
        assert_eq!(tl[2].metric.as_deref(), Some("active-power"));
    }

    /// The outage chain keeps exactly one live handle on
    /// `active-timers` — each re-schedule drops the chain's previous
    /// (fired) handle instead of consing forever.
    #[test]
    fn random_outage_track_keeps_one_handle_per_chain() {
        let (cfg, dir) = config_with("");
        let src = std::path::Path::new("sim/scenarios.lisp");
        let dst_dir = dir.join("sim");
        std::fs::create_dir_all(&dst_dir).unwrap();
        std::fs::copy(src, dst_dir.join("scenarios.lisp")).unwrap();
        cfg.eval("(load \"sim/scenarios.lisp\")").unwrap();
        // common.lisp normally seeds this; the fixture skips it.
        cfg.eval("(setq active-timers nil)").unwrap();
        // Simulate three re-schedules; each replaces the prior slot.
        for _ in 0..3 {
            cfg.eval("(random-outage--track (run-with-timer 9999 nil (lambda () nil)))")
                .unwrap();
        }
        assert_eq!(cfg.eval("(length active-timers)").unwrap(), "1");
        // An unrelated tracked timer survives the chain's pruning.
        // Entries are (FILE . TIMER) conses; this one is REPL-armed,
        // so its file is nil.
        cfg.eval(
            "(setq active-timers \
               (cons (cons nil (run-with-timer 9999 nil (lambda () nil))) active-timers))",
        )
        .unwrap();
        cfg.eval("(random-outage--track (run-with-timer 9999 nil (lambda () nil)))")
            .unwrap();
        assert_eq!(cfg.eval("(length active-timers)").unwrap(), "2");
    }

    /// `(scenario-start)` opens a scenario, `(scenario-event)`
    /// appends to the journal, `(scenario-elapsed)` returns wall-
    /// clock seconds since start, `(scenario-stop)` freezes it.
    #[test]
    fn scenario_lifecycle_round_trips_through_lisp() {
        let (cfg, _dir) = config_with("");
        cfg.eval("(scenario-start \"warmup\")").unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.name.as_deref(), Some("warmup"));
        assert!(summary.started_at.is_some());
        assert!(summary.ended_at.is_none());
        assert_eq!(summary.event_count, 0);

        // First event id is 0.
        cfg.eval("(scenario-event 'outage \"bat-1003\")").unwrap();
        cfg.eval("(scenario-event \"note\" \"warming up\")")
            .unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.event_count, 2);
        assert_eq!(summary.next_event_id, 2);

        let events = cfg.site().scenario_events_since(0, 100);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "outage");
        assert_eq!(events[1].kind, "note");

        // Stop freezes elapsed; a subsequent (scenario-elapsed)
        // returns the frozen value rather than continuing to grow.
        cfg.eval("(scenario-stop)").unwrap();
        let frozen = cfg.site().scenario_summary(chrono::Utc::now());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(frozen.elapsed_s, later.elapsed_s);
        assert!(frozen.ended_at.is_some());
    }

    /// A second `(scenario-start …)` tears the running scenario down
    /// first. The guard lives on the defun, not in `scenario--run`,
    /// so a bare imperative start — a script, the REPL, the UI's
    /// start route — gets the same teardown a `define-scenario` run
    /// gets. Without it the fresh start clears the knob baseline
    /// Rust-side and the first run's driven meter could never be
    /// restored: its pre-scenario `:power` would be lost for good.
    #[test]
    fn a_second_scenario_start_tears_the_running_one_down() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 1234.0)");
        let m = cfg.site().get(7).unwrap();

        cfg.eval("(scenario-start \"a\")").unwrap();
        cfg.eval("(set-meter-power 7 9000.0)").unwrap();
        assert_eq!(m.meter_power_reading().unwrap().value, 9000.0);

        // B starts with A still running, and with no (scenario-stop)
        // of our own in between.
        cfg.eval("(scenario-start \"b\")").unwrap();
        assert_eq!(
            m.meter_power_reading().unwrap().value,
            1234.0,
            "starting B must restore A's pre-scenario :power, not strand it"
        );
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.name.as_deref(), Some("b"));
        assert!(
            summary.started_at.is_some() && summary.ended_at.is_none(),
            "B must be the running scenario after the teardown"
        );

        // B drove nothing, so its own stop leaves A's restored value.
        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(m.meter_power_reading().unwrap().value, 1234.0);
    }

    /// `(define-scenario)` parses the unified model into the registry:
    /// schedule / clock / length / seed metadata plus the optional
    /// section forms (kept raw for the runner). The section wrappers
    /// are tested separately; here the sections are plain forms.
    #[test]
    fn define_scenario_registers_unified_model() {
        use crate::sim::scenarios::{ClockDriver, Schedule};
        let (cfg, _dir) = config_with("");
        cfg.eval(
            r#"
            (define-scenario
              :name "cloud-fade"
              :description "PV fades; the limiter holds the cap"
              :schedule 'relative
              :clock 'stepped
              :length "4min"
              :seed 42
              :setup (lambda () nil)
              :drive (list (lambda () nil) (lambda () nil))
              :cues (list (lambda () nil))
              :expect (list (lambda () nil) (lambda () nil) (lambda () nil))
              :record 'csv)
            "#,
        )
        .unwrap();
        let regs = cfg.scenarios();
        let r = regs.lock();
        let d = r.get("cloud-fade").expect("registered");
        assert_eq!(d.description, "PV fades; the limiter holds the cap");
        assert_eq!(d.schedule, Schedule::Relative);
        assert_eq!(d.clock, ClockDriver::Stepped);
        assert_eq!(d.length_s, Some(240.0));
        assert_eq!(d.seed, Some(42));
        assert!(d.setup.is_some());
        assert_eq!(d.drive.len(), 2);
        assert!(d.agents.is_empty());
        assert_eq!(d.cues.len(), 1);
        assert_eq!(d.expect.len(), 3);
        assert!(d.record.is_some());
    }

    /// Schedule / clock default to relative / real; an absolute
    /// schedule keeps its `:date`; bad enum values error.
    #[test]
    fn define_scenario_defaults_and_validation() {
        use crate::sim::scenarios::{ClockDriver, Schedule};
        let (cfg, _dir) = config_with("");
        cfg.eval(r#"(define-scenario :name "bare")"#).unwrap();
        cfg.eval(r#"(define-scenario :name "day" :schedule 'absolute :date "2026-06-15")"#)
            .unwrap();
        {
            let regs = cfg.scenarios();
            let r = regs.lock();
            let bare = r.get("bare").unwrap();
            assert_eq!(bare.schedule, Schedule::Relative);
            assert_eq!(bare.clock, ClockDriver::Real);
            assert_eq!(bare.length_s, None);
            assert!(bare.cues.is_empty());
            let day = r.get("day").unwrap();
            assert_eq!(day.schedule, Schedule::Absolute);
            assert_eq!(
                day.date,
                Some(chrono::NaiveDate::from_ymd_opt(2026, 6, 15).unwrap())
            );
        }
        assert!(
            cfg.eval(r#"(define-scenario :name "x" :schedule 'sometime)"#)
                .is_err()
        );
        assert!(
            cfg.eval(r#"(define-scenario :name "x" :clock 'quartz)"#)
                .is_err()
        );
    }

    /// Battery DC power integrates into the journal's per-battery
    /// charge / discharge integrals. Drive a battery via its
    /// inverter, advance physics + sampling, and assert the totals.
    #[test]
    fn battery_charge_discharge_integrates_through_snapshot() {
        use chrono::{Duration as ChronoDuration, Utc};
        let (cfg, _dir) = config_with(
            "(setq b (%make-battery :id 100
                                    :capacity 100000.0
                                    :rated-lower -10000.0
                                    :rated-upper 10000.0))
             (%make-battery-inverter :id 200
                                     :rated-lower -10000.0
                                     :rated-upper 10000.0
                                     :successors (list b))",
        );
        cfg.eval("(scenario-start \"integrate\")").unwrap();
        // Push a charge setpoint of +3600 W for 10 sim-seconds.
        cfg.eval("(set-active-power 200 3600.0 60000)").unwrap();
        // Two short ticks carry the command across the gateway and
        // device delays; default ramp is infinity, so that settles
        // it.
        let mut now = Utc::now();
        for _ in 0..2 {
            now += ChronoDuration::milliseconds(100);
            cfg.site()
                .tick_once(now, std::time::Duration::from_millis(100));
        }
        // Snapshot pass at t0 — first one just seeds the cursor
        // (dt from start is small but non-zero — ignore the result).
        cfg.site().record_history_snapshot(now);
        now += ChronoDuration::seconds(10);
        cfg.site()
            .tick_once(now, std::time::Duration::from_secs(10));
        cfg.site().record_history_snapshot(now);
        let r = cfg.site().scenario_report(now);
        // 3600 W for 10 s = 10 Wh. Allow some slop for the seed
        // sample's dt at start.
        assert!(
            r.total_battery_charged_wh > 8.0 && r.total_battery_charged_wh < 12.0,
            "expected ~10 Wh charged, got {}",
            r.total_battery_charged_wh,
        );
        assert_eq!(r.total_battery_discharged_wh, 0.0);

        // Now flip to discharging.
        cfg.eval("(set-active-power 200 -7200.0 60000)").unwrap();
        for _ in 0..2 {
            now += ChronoDuration::milliseconds(100);
            cfg.site()
                .tick_once(now, std::time::Duration::from_millis(100));
        }
        now += ChronoDuration::seconds(5);
        cfg.site().tick_once(now, std::time::Duration::from_secs(5));
        cfg.site().record_history_snapshot(now);
        let r = cfg.site().scenario_report(now);
        // 7200 W * 5 s / 3600 = 10 Wh discharged.
        assert!(
            r.total_battery_discharged_wh > 8.0 && r.total_battery_discharged_wh < 12.0,
            "expected ~10 Wh discharged, got {}",
            r.total_battery_discharged_wh,
        );
        assert_eq!(r.per_battery.len(), 1);
        assert_eq!(r.per_battery[0].id, 100);
    }

    /// The reporter's site peak comes from the loopback's grid_power
    /// formula samples, handed in through the site hook. No loopback
    /// runs in a stepped config, so the samples are fed directly —
    /// what matters here is the journal's start/peak semantics.
    #[test]
    fn grid_peak_tracks_active_power() {
        use chrono::Utc;
        let (cfg, _dir) = config_with(
            "(%make-grid-connection-point
               :id 1
               :successors (list (%make-meter :id 2 :power 1000.0)))",
        );
        // Pre-start, samples shouldn't update the peak — the
        // scenario hasn't begun.
        cfg.site().record_grid_power_sample(1000.0, Utc::now());
        assert_eq!(cfg.site().scenario_report(Utc::now()).peak_grid_w, 0.0);

        cfg.eval("(scenario-start \"power\")").unwrap();
        cfg.site().record_grid_power_sample(2500.0, Utc::now());
        let r = cfg.site().scenario_report(Utc::now());
        assert!((r.peak_grid_w - 2500.0).abs() < 1e-3);

        // A higher value lifts the peak; a later lower one
        // doesn't.
        cfg.site().record_grid_power_sample(7800.0, Utc::now());
        cfg.site().record_grid_power_sample(1100.0, Utc::now());
        let r = cfg.site().scenario_report(Utc::now());
        assert!((r.peak_grid_w - 7800.0).abs() < 1e-3);

        // scenario-start resets the peak.
        cfg.eval("(scenario-start \"again\")").unwrap();
        cfg.site().record_grid_power_sample(500.0, Utc::now());
        assert!((cfg.site().scenario_report(Utc::now()).peak_grid_w - 500.0).abs() < 1e-3,);
    }

    /// A second `(scenario-start)` clears the previous run's events
    /// but keeps the monotonic id counter so polling clients with a
    /// `since=` cursor see new events immediately rather than
    /// rewinding through stale ids.
    #[test]
    fn scenario_restart_clears_events_keeps_ids_monotonic() {
        let (cfg, _dir) = config_with("");
        cfg.eval("(scenario-start \"first\")").unwrap();
        cfg.eval("(scenario-event 'a \"\")").unwrap();
        cfg.eval("(scenario-event 'b \"\")").unwrap();
        assert_eq!(
            cfg.site()
                .scenario_summary(chrono::Utc::now())
                .next_event_id,
            2
        );
        cfg.eval("(scenario-start \"second\")").unwrap();
        let summary = cfg.site().scenario_summary(chrono::Utc::now());
        assert_eq!(summary.event_count, 0);
        assert_eq!(summary.next_event_id, 2);
        let id = cfg
            .eval("(scenario-event 'c \"\")")
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert_eq!(id, 2);
    }
}
