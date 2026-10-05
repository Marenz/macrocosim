//! `(set-meter-power)` + `(set-solar-sunlight)` — drive a
//! component's input slot from Lisp. Both accept a number (constant
//! override), a lambda (re-resolved on every refresh tick), or a
//! quoted symbol (deref the bound variable per refresh). Plus
//! `(set-battery-soc)` — teleport a battery's charge state, for
//! arranging a precondition from a scenario cue. Plus the reactive
//! Q twins: `(set-meter-reactive-power)` (same number / lambda /
//! symbol dispatch as `set-meter-power`) and `(set-meter-power-factor)`
//! (hold Q at a power factor that tracks the meter's own live P).
//! Plus the EV-charger doors: `(plug-ev)` / `(%plug-ev)`,
//! `(unplug-ev)`, `(ev-info)` and `(ev-presets)` — plug state is a
//! scenario knob, so a run that plugs or unplugs a car is undone by
//! `(scenario-stop)`.

use tulisp::{AsPlist, Error, Plist, TulispContext, TulispObject};

use crate::lisp::make::preset_from_lisp;
use crate::lisp::value::LispValue;
use crate::sim::component::KnobKind;
use crate::sim::ev_presets::{ConnectedEv, EvOverrides, PRESETS};
use crate::sim::microgrids::SharedSiteRouter;

// `%plug-ev`'s kwargs: the charger to plug into, the catalog car,
// and the per-plug overrides on top of that car's preset values.
AsPlist! {
    pub struct PlugEvArgs {
        id: Option<i64> {= None},
        preset: Option<LispValue> {= None},
        soc: Option<f64> {= None},
        target_soc<":target-soc">: Option<f64> {= None},
        phases: Option<i64> {= None},
        max_current_a<":max-current-a">: Option<f64> {= None},
        capacity_kwh<":capacity-kwh">: Option<f64> {= None},
        taper_start<":taper-start">: Option<f64> {= None},
        taper_floor<":taper-floor">: Option<f64> {= None},
    }
}

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Drive a meter's `:power` slot from Lisp. Accepts a number, a
    // lambda, or a symbol — numeric values land as a constant
    // override (microsim-style timer-driven load curve); lambda /
    // symbol values install a DynamicScalar that the scheduler
    // re-resolves on every tick. UI's `:power` text input piggy-
    // backs on this: whatever the user types becomes the second
    // argument here.
    let r = router.clone();
    ctx.defun(
        "set-meter-power",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-meter-power: component {id} not found"
                )));
            };
            w.scenario_snapshot_knob(id as u64, KnobKind::MeterPower);
            if value.numberp() {
                let watts = f64::try_from(&value)?;
                // Lenient: a non-meter is left unchanged, and the
                // snapshot and broadcast happen all the same.
                if let Some(m) = c.meter_drive() {
                    m.set_active_power_override(watts as f32);
                }
                w.note_knob_changed(id as u64, "meter-power", Some(watts as f32), None, None);
            } else if let Some(scalar) =
                crate::sim::dynamic_scalar::DynamicScalar::from_lisp(&value, 0.0)
            {
                // Printed source (same text `source_text` would report)
                // and the cached value right after construction, before
                // the scalar moves into the component.
                let printed = value.to_string();
                let resolved_now = scalar.get();
                if let Some(m) = c.meter_drive() {
                    m.set_active_power_source(scalar);
                }
                w.note_knob_changed(
                    id as u64,
                    "meter-power",
                    Some(resolved_now),
                    Some(printed),
                    None,
                );
            } else {
                return Err(Error::invalid_argument(format!(
                    "set-meter-power: expected a number, lambda, or symbol — got {value}"
                )));
            }
            Ok(true)
        },
    );

    // Drive a meter's `:reactive-power` slot from Lisp. The Q twin of
    // set-meter-power above — same number / lambda / symbol dispatch,
    // equally lenient on a non-meter (the typed control API is the
    // strict door).
    let r = router.clone();
    ctx.defun(
        "set-meter-reactive-power",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-meter-reactive-power: component {id} not found"
                )));
            };
            w.scenario_snapshot_knob(id as u64, KnobKind::MeterReactive);
            if value.numberp() {
                let vars = f64::try_from(&value)?;
                if let Some(m) = c.meter_drive() {
                    m.set_reactive_power_override(vars as f32);
                }
                w.note_knob_changed(
                    id as u64,
                    "meter-reactive-power",
                    Some(vars as f32),
                    None,
                    None,
                );
            } else if let Some(scalar) =
                crate::sim::dynamic_scalar::DynamicScalar::from_lisp(&value, 0.0)
            {
                // Printed source and the cached value right after
                // construction, before the scalar moves into the
                // component — same pattern as set-meter-power above.
                let printed = value.to_string();
                let resolved_now = scalar.get();
                if let Some(m) = c.meter_drive() {
                    m.set_reactive_power_source(scalar);
                }
                w.note_knob_changed(
                    id as u64,
                    "meter-reactive-power",
                    Some(resolved_now),
                    Some(printed),
                    None,
                );
            } else {
                return Err(Error::invalid_argument(format!(
                    "set-meter-reactive-power: expected a number, lambda, or symbol — got {value}"
                )));
            }
            Ok(true)
        },
    );

    // Drop a meter's active-power override, returning it to measuring
    // its children — the way back from set-meter-power. Strict: a
    // component without a meter drive is an error.
    let r = router.clone();
    ctx.defun("clear-meter-power", move |id: i64| -> Result<bool, Error> {
        let w = r.site();
        let Some(c) = w.get(id as u64) else {
            return Err(Error::invalid_argument(format!(
                "clear-meter-power: component {id} not found"
            )));
        };
        w.scenario_snapshot_knob(id as u64, KnobKind::MeterPower);
        let Some(m) = c.meter_drive() else {
            return Err(Error::invalid_argument(format!(
                "clear-meter-power: component {id} is not a meter"
            )));
        };
        m.clear_active_power_source();
        w.note_knob_changed(id as u64, "meter-power", None, None, None);
        Ok(true)
    });

    // Drop a meter's reactive-power override — whichever of Var /
    // PowerFactor is set, it's the same slot — returning it to summing
    // children's Q. The Q twin of clear-meter-power above.
    let r = router.clone();
    ctx.defun(
        "clear-meter-reactive",
        move |id: i64| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "clear-meter-reactive: component {id} not found"
                )));
            };
            w.scenario_snapshot_knob(id as u64, KnobKind::MeterReactive);
            let Some(m) = c.meter_drive() else {
                return Err(Error::invalid_argument(format!(
                    "clear-meter-reactive: component {id} is not a meter"
                )));
            };
            m.clear_reactive_power_source();
            // Two tokens, one slot: the inspector's power-factor input
            // is a knob of its own ("meter-power-factor"), separate
            // from "meter-reactive-power" — a PowerFactor-shaped clear
            // must blank both or the PF input keeps showing a stale
            // number until the next full snapshot.
            w.note_knob_changed(id as u64, "meter-reactive-power", None, None, None);
            w.note_knob_changed(id as u64, "meter-power-factor", None, None, None);
            Ok(true)
        },
    );

    // Hold a meter's reactive power at a power factor that tracks its
    // own live active power. PF is validated here and again by the
    // typed control API, not in the meter:
    // `MeterDrive::set_power_factor` does no range checking, so this
    // defun and the drive op are the only doors that enforce
    // `0.0 < pf <= 1.0` before the value reaches the meter.
    let r = router.clone();
    ctx.defun(
        "set-meter-power-factor",
        move |id: i64, pf: f64, leading: Option<bool>| -> Result<bool, Error> {
            if !(pf > 0.0 && pf <= 1.0) {
                return Err(Error::invalid_argument(format!(
                    "set-meter-power-factor: pf must be in (0.0, 1.0], got {pf}"
                )));
            }
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-meter-power-factor: component {id} not found"
                )));
            };
            w.scenario_snapshot_knob(id as u64, KnobKind::MeterReactive);
            // Lenient: a non-meter is left unchanged; only the typed
            // control API rejects it.
            let leading = leading.unwrap_or(false);
            if let Some(m) = c.meter_drive() {
                m.set_power_factor(pf as f32, leading);
            }
            w.note_knob_changed(
                id as u64,
                "meter-power-factor",
                Some(pf as f32),
                None,
                Some(leading),
            );
            Ok(true)
        },
    );

    // Teleport a battery's state of charge (clamped to 0..=100 by the
    // battery). Scenario cues use it to arrange a precondition — a
    // nearly-empty or nearly-full pool — without simulating the charge.
    // Follows this file's convention: unknown id errors, but a
    // non-battery component is a lenient no-op (the typed control API
    // is the strict door).
    let r = router.clone();
    ctx.defun(
        "set-battery-soc",
        move |id: i64, pct: f64| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-battery-soc: component {id} not found"
                )));
            };
            if let Some(port) = c.ev_port() {
                // A charger's SoC is the plugged car's; with no car
                // there is nothing to move, and silently doing
                // nothing would hide a scenario's ordering bug.
                if port.ev_info().is_none() {
                    return Err(Error::invalid_argument(format!(
                        "set-battery-soc: charger {id} has no EV plugged in"
                    )));
                }
                // Past that guard the charger has a car, and this IS
                // a write to the car — so it takes the plug knob's
                // snapshot, like `plug-ev` and `unplug-ev` do. The
                // `Ev` baseline holds the whole car, SoC included, so
                // teardown puts it back exactly as it was whatever
                // order a run did its plugging and its SoC writes in.
                w.scenario_snapshot_knob(id as u64, KnobKind::Ev);
                let _ = port.set_ev_soc_pct(pct as f32);
            } else if let Some(store) = c.dc_storage() {
                // A battery's SoC has no snapshot on any door.
                store.set_soc_pct(pct as f32);
            }
            Ok(true)
        },
    );

    // PV analogue of set-meter-power. Same numeric / dynamic
    // dispatch — drives `(set-solar-sunlight id (lambda () …))` and
    // friends from scenarios or the UI. Per-tick `min-avail =
    // max(-array-peak-w × sunlight%/100, rated-lower)` clamp — the AC
    // rating floor — picks up the new value on the next refresh +
    // tick pair.
    let r = router.clone();
    ctx.defun(
        "set-solar-sunlight",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-solar-sunlight: component {id} not found"
                )));
            };
            // A non-finite sunlight is a scripting mistake (NaN would
            // read as no sun, +inf as full output), so say so
            // instead. Checked as the f32 the inverter stores: a
            // huge f64 like 1e300 overflows to +inf in the cast.
            let pct = if value.numberp() {
                let pct = f64::try_from(&value)? as f32;
                if !pct.is_finite() {
                    return Err(Error::invalid_argument(format!(
                        "set-solar-sunlight: sunlight must be finite, got {value}"
                    )));
                }
                Some(pct)
            } else {
                None
            };
            w.scenario_snapshot_knob(id as u64, KnobKind::Sunlight);
            // Lenient: a component without a sunlight knob is left
            // as it is, but the snapshot, the broadcast and the `t`
            // still happen.
            let sun = c.sunlight_drive();
            if let Some(pct) = pct {
                if let Some(sun) = sun {
                    sun.set_sunlight_pct(pct);
                }
                w.note_knob_changed(id as u64, "solar-sunlight", Some(pct), None, None);
            } else if let Some(scalar) =
                crate::sim::dynamic_scalar::DynamicScalar::from_lisp(&value, 100.0)
            {
                // Printed source and the cached value right after
                // construction — same pattern as set-meter-power above.
                let printed = value.to_string();
                let resolved_now = scalar.get();
                if let Some(sun) = sun {
                    sun.set_sunlight_source(scalar);
                }
                w.note_knob_changed(
                    id as u64,
                    "solar-sunlight",
                    Some(resolved_now),
                    Some(printed),
                    None,
                );
            } else {
                return Err(Error::invalid_argument(format!(
                    "set-solar-sunlight: expected a number, lambda, or symbol — got {value}"
                )));
            }
            Ok(true)
        },
    );

    // The way back from set-solar-sunlight — the trip the sunlight
    // knob never had one. Drops whatever is driving the slot and
    // returns the inverter to following the site's weather, exactly
    // as a freshly-constructed one with no `:sunlight%` does.
    // Strict: a component without a sunlight knob errors.
    let r = router.clone();
    ctx.defun(
        "clear-solar-sunlight",
        move |id: i64| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "clear-solar-sunlight: component {id} not found"
                )));
            };
            w.scenario_snapshot_knob(id as u64, KnobKind::Sunlight);
            let Some(sun) = c.sunlight_drive() else {
                return Err(Error::invalid_argument(format!(
                    "clear-solar-sunlight: component {id} does not take a sunlight clear"
                )));
            };
            sun.clear_sunlight_source();
            // Unlike clear-meter-power, the cleared slot is not
            // "nothing" — a `Follow` source has a live percentage of
            // its own (the last sky the inverter resolved, which is
            // full sun until its first tick), so the inspector gets
            // that value rather than a blanked input.
            let now_pct = sun.sunlight_reading().value;
            w.note_knob_changed(
                id as u64,
                "solar-sunlight",
                Some(now_pct),
                Some("weather".into()),
                None,
            );
            Ok(true)
        },
    );

    // Steam boiler analogue of set-meter-power / set-solar-sunlight:
    // drive the `:demand` (kg/h) input from Lisp. Same numeric /
    // dynamic dispatch — a number installs a constant, a lambda or
    // symbol installs a DynamicScalar the scheduler re-resolves each
    // refresh tick. Strict: unlike the meter/solar setters, a
    // non-boiler must reject here, not silently no-op — this is a
    // first-class inspector knob.
    let r = router.clone();
    ctx.defun(
        "set-boiler-demand",
        move |id: i64, value: TulispObject| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-boiler-demand: component {id} not found"
                )));
            };
            let Some(boiler) = c.steam_drive() else {
                return Err(Error::invalid_argument(format!(
                    "set-boiler-demand: component {id} is not a steam boiler"
                )));
            };
            w.scenario_snapshot_knob(id as u64, KnobKind::BoilerDemand);
            if value.numberp() {
                let kg_h = f64::try_from(&value)?;
                boiler.set_steam_demand_kg_h(kg_h as f32);
                w.note_knob_changed(id as u64, "boiler-demand", Some(kg_h as f32), None, None);
            } else if let Some(scalar) =
                crate::sim::dynamic_scalar::DynamicScalar::from_lisp(&value, 0.0)
            {
                // Printed source and the cached value right after
                // construction — same pattern as set-meter-power above.
                let printed = value.to_string();
                let resolved_now = scalar.get();
                boiler.set_steam_demand_source(scalar);
                w.note_knob_changed(
                    id as u64,
                    "boiler-demand",
                    Some(resolved_now),
                    Some(printed),
                    None,
                );
            } else {
                return Err(Error::invalid_argument(format!(
                    "set-boiler-demand: expected a number, lambda, or symbol — got {value}"
                )));
            }
            Ok(true)
        },
    );

    // Steam boiler pressure override — numeric only (unlike demand,
    // pressure has no dynamic-source door on `SteamDrive`). Strict,
    // for the same reason as set-boiler-demand.
    let r = router.clone();
    ctx.defun(
        "set-boiler-pressure",
        move |id: i64, bar: f64| -> Result<bool, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "set-boiler-pressure: component {id} not found"
                )));
            };
            let Some(boiler) = c.steam_drive() else {
                return Err(Error::invalid_argument(format!(
                    "set-boiler-pressure: component {id} is not a steam boiler"
                )));
            };
            boiler.set_pressure_bar(bar as f32);
            w.note_knob_changed(id as u64, "boiler-pressure", Some(bar as f32), None, None);
            Ok(true)
        },
    );

    // Plug a preset car into a charger. The Lisp-facing `plug-ev` in
    // sim/common.lisp spreads (plug-ev ID PRESET &rest OVERRIDES) into
    // this plist form, the same two-layer shape as make-*.
    let r = router.clone();
    ctx.defun(
        "%plug-ev",
        move |args: Plist<PlugEvArgs>| -> Result<bool, Error> {
            let a = args.into_inner();
            let w = r.site();
            let id = a
                .id
                .ok_or_else(|| Error::invalid_argument("plug-ev: :id is required".to_string()))?
                as u64;
            let Some(c) = w.get(id) else {
                return Err(Error::invalid_argument(format!(
                    "plug-ev: component {id} not found"
                )));
            };
            let Some(port) = c.ev_port() else {
                return Err(Error::invalid_argument(format!(
                    "plug-ev: component {id} is not an EV charger"
                )));
            };
            // Rejected BEFORE the snapshot below, not by `plug_ev`'s own
            // guard afterwards: an `Ev` baseline holds a live clone of
            // the car, so seeding one from a refused plug would make
            // `(scenario-stop)` rewind that car's SoC and accumulated
            // energy to the instant of a call that changed nothing.
            if port.ev_info().is_some() {
                return Err(Error::invalid_argument(format!(
                    "plug-ev: component {id}: an EV is already plugged in"
                )));
            }
            let raw = a.preset.as_ref().ok_or_else(|| {
                Error::invalid_argument(format!(
                    "plug-ev: component {id}: a preset symbol is required"
                ))
            })?;
            let p = preset_from_lisp(raw, &format!("plug-ev: component {id}"))?;
            // Guarded here rather than left to `ConnectedEv::new`: the
            // cast below is what would go wrong, and an i64 outside
            // 1..=3 truncates into a plausible-looking u8.
            let phases = match a.phases {
                None => None,
                Some(v) if (1..=3).contains(&v) => Some(v as u8),
                Some(v) => {
                    return Err(Error::invalid_argument(format!(
                        "plug-ev: component {id}: phases must be 1, 2 or 3 (got {v})"
                    )));
                }
            };
            let o = EvOverrides {
                soc_pct: a.soc.map(|v| v as f32),
                target_soc_pct: a.target_soc.map(|v| v as f32),
                phases,
                max_current_a: a.max_current_a.map(|v| v as f32),
                capacity_wh: a.capacity_kwh.map(|v| (v * 1000.0) as f32),
                taper_start_pct: a.taper_start.map(|v| v as f32),
                taper_floor: a.taper_floor.map(|v| v as f32),
            };
            // The car is built and validated in full BEFORE the
            // snapshot and the write: a rejected override leaves the
            // charger exactly as it was.
            let ev = ConnectedEv::new(p, &o, chrono::Utc::now())
                .map_err(|e| Error::invalid_argument(format!("plug-ev: component {id}: {e}")))?;
            let soc = ev.soc_pct;
            w.scenario_snapshot_knob(id, KnobKind::Ev);
            port.plug_ev(ev)
                .map_err(|e| Error::invalid_argument(format!("plug-ev: component {id}: {e}")))?;
            // `expr` is for printed Lisp source a write installed;
            // the preset name is not that, so it stays out of it.
            w.note_knob_changed(id, "ev", Some(soc), None, None);
            Ok(true)
        },
    );

    let r = router.clone();
    ctx.defun("unplug-ev", move |id: i64| -> Result<bool, Error> {
        let w = r.site();
        let Some(c) = w.get(id as u64) else {
            return Err(Error::invalid_argument(format!(
                "unplug-ev: component {id} not found"
            )));
        };
        let Some(port) = c.ev_port() else {
            return Err(Error::invalid_argument(format!(
                "unplug-ev: component {id} is not an EV charger"
            )));
        };
        // Only when there is a car to take away: this suppresses a
        // teardown restore (and its `knob_changed`) on a charger the
        // run never displaced. It does NOT protect a car an operator
        // plugs mid-scenario — `plug-ev` snapshots `Ev(None)` itself,
        // and teardown then unplugs it by design, per the transient-
        // knob contract.
        if port.ev_info().is_some() {
            w.scenario_snapshot_knob(id as u64, KnobKind::Ev);
        }
        let had = port.unplug_ev();
        if had {
            w.note_knob_changed(id as u64, "ev", None, None, None);
        }
        Ok(had)
    });

    // The simulator's private view of the car: a plist, or nil for an
    // empty charger or a component that takes no EV.
    let r = router;
    ctx.defun(
        "ev-info",
        move |ctx: &mut TulispContext, id: i64| -> Result<TulispObject, Error> {
            let w = r.site();
            let Some(c) = w.get(id as u64) else {
                return Err(Error::invalid_argument(format!(
                    "ev-info: component {id} not found"
                )));
            };
            let Some(info) = c.ev_port().and_then(|p| p.ev_info()) else {
                return Ok(TulispObject::nil());
            };
            let ev = info.ev;
            Ok(vec![
                ctx.intern(":preset"),
                ctx.intern(ev.preset),
                ctx.intern(":soc"),
                (ev.soc_pct as f64).into(),
                ctx.intern(":target-soc"),
                (ev.target_soc_pct as f64).into(),
                ctx.intern(":phases"),
                (ev.phases as i64).into(),
                ctx.intern(":max-current-a"),
                (ev.max_current_a as f64).into(),
                ctx.intern(":capacity-kwh"),
                (ev.capacity_wh as f64 / 1000.0).into(),
                ctx.intern(":energy-wh"),
                (ev.energy_wh as f64).into(),
                ctx.intern(":plugged-at"),
                ev.plugged_at.to_rfc3339().into(),
                ctx.intern(":state"),
                ctx.intern(info.state.as_str()),
            ]
            .into_iter()
            .collect())
        },
    );

    // The catalog `plug-ev` names, so a scenario author (or the UI)
    // can list what is on offer without reading the Rust source.
    ctx.defun(
        "ev-presets",
        move |ctx: &mut TulispContext| -> Result<TulispObject, Error> {
            Ok(PRESETS
                .iter()
                .map(|p| {
                    vec![
                        ctx.intern(":name"),
                        ctx.intern(p.name),
                        ctx.intern(":phases"),
                        (p.phases as i64).into(),
                        ctx.intern(":max-current-a"),
                        (p.max_current_a as f64).into(),
                        ctx.intern(":capacity-kwh"),
                        (p.capacity_wh as f64 / 1000.0).into(),
                    ]
                    .into_iter()
                    .collect::<TulispObject>()
                })
                .collect())
        },
    );
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::{assert_lenient_noop, config_with, err_line};
    use crate::sim::component::ReactiveReading;
    use crate::sim::events::SiteEvent;

    /// `(set-meter-power id (lambda () X))` installs a dynamic
    /// source. `Config::refresh_once` resolves the lambda and
    /// `aggregate_power_w` reflects it on the next read.
    #[test]
    fn set_meter_power_accepts_a_lambda() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        cfg.eval("(set-meter-power 7 (lambda () 1234.5))").unwrap();
        cfg.refresh_once();
        let m = cfg.site().get(7).unwrap();
        assert!((m.aggregate_power_w(&cfg.site()) - 1234.5).abs() < 1e-3);
    }

    /// `(set-meter-power id 'symbol)` derefs the symbol's variable
    /// value each refresh — scenarios use this to drive a load
    /// curve from a global that another timer mutates.
    #[test]
    fn set_meter_power_accepts_a_symbol() {
        let (cfg, _dir) = config_with(
            "(setq consumer-power 1500.0)
             (%make-meter :id 7)",
        );
        cfg.eval("(set-meter-power 7 'consumer-power)").unwrap();
        cfg.refresh_once();
        let m = cfg.site().get(7).unwrap();
        assert!((m.aggregate_power_w(&cfg.site()) - 1500.0).abs() < 1e-3);
        // Mutate the bound variable; next refresh picks up the new value.
        cfg.eval("(setq consumer-power 2750.0)").unwrap();
        cfg.refresh_once();
        assert!((m.aggregate_power_w(&cfg.site()) - 2750.0).abs() < 1e-3);
    }

    /// `(set-solar-sunlight id (lambda () X))` mirrors
    /// `set-meter-power` for PV. Refresh resolves the lambda; the
    /// next setpoint clip surfaces the new floor.
    #[test]
    fn set_solar_sunlight_accepts_a_lambda() {
        let (cfg, _dir) =
            config_with("(%make-solar-inverter :id 8 :rated-lower -8000.0 :rated-upper 0.0)");
        cfg.eval("(set-solar-sunlight 8 (lambda () 25.0))").unwrap();
        cfg.refresh_once();
        let inv = cfg.site().get(8).unwrap();
        // Issue a setpoint below sunlight-derated min_avail so the
        // ramp clips — observable through telemetry's active_power.
        cfg.site()
            .gateway()
            .command(8, crate::timeout_tracker::SetpointAxis::Active, -5000.0)
            .expect("within rated");
        cfg.site().tick_n(3, std::time::Duration::from_millis(100));
        let p = inv
            .telemetry(&cfg.site())
            .active_power_w
            .expect("active power present");
        // 25% of -8000 = -2000 W floor.
        assert!(
            (p - (-2000.0)).abs() < 1.0,
            "expected sunlight-clipped -2000 W, got {p}",
        );
    }

    /// A non-finite sunlight is refused at the door, and the sunlight
    /// already set stands.
    #[test]
    fn set_solar_sunlight_rejects_a_non_finite_number() {
        let (cfg, _dir) =
            config_with("(%make-solar-inverter :id 8 :rated-lower -8000.0 :rated-upper 0.0)");
        cfg.eval("(set-solar-sunlight 8 25.0)").unwrap();
        for bad in ["(/ 0.0 0.0)", "1e300"] {
            let err = cfg
                .eval(&format!("(set-solar-sunlight 8 {bad})"))
                .expect_err("a non-finite sunlight must be refused");
            assert!(err.contains("finite"), "{bad}: unexpected error: {err}");
        }
        // The 25 % still stands: a setpoint past its floor clips to
        // 25 % of -8000 W.
        let inv = cfg.site().get(8).unwrap();
        cfg.site()
            .gateway()
            .command(8, crate::timeout_tracker::SetpointAxis::Active, -5000.0)
            .expect("within rated");
        cfg.site().tick_n(3, std::time::Duration::from_millis(100));
        let p = inv.telemetry(&cfg.site()).active_power_w.unwrap();
        assert!((p - (-2000.0)).abs() < 1.0, "expected -2000 W, got {p}");
    }

    /// `(set-meter-power id V)` broadcasts a `KnobChanged` on the
    /// site event bus so live UI inspector tabs can refresh their
    /// edit-in-place input without a full topology refetch.
    #[test]
    fn set_meter_power_broadcasts_knob_changed() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        let mut rx = cfg.site().subscribe_events();
        cfg.eval("(set-meter-power 7 1500)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged { id: 7, knob: "meter-power", value: Some(v), expr: None, .. }
                    if (*v - 1500.0).abs() < 1e-6
            )),
            "no matching KnobChanged on the bus; saw: {seen:?}"
        );
    }

    /// `(set-meter-reactive-power id V)` broadcasts a `KnobChanged`
    /// mirroring the active-power case above.
    #[test]
    fn set_meter_reactive_power_broadcasts_knob_changed() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        let mut rx = cfg.site().subscribe_events();
        cfg.eval("(set-meter-reactive-power 7 500.0)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged { id: 7, knob: "meter-reactive-power", value: Some(v), expr: None, .. }
                    if (*v - 500.0).abs() < 1e-6
            )),
            "no matching KnobChanged on the bus; saw: {seen:?}"
        );
    }

    /// `(set-meter-power-factor id PF LEADING)` broadcasts a
    /// `KnobChanged` carrying the `leading` flag — the inspector's
    /// PF input needs it to render the lagging/leading toggle.
    #[test]
    fn set_meter_power_factor_broadcasts_knob_changed() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 8000.0)");
        let mut rx = cfg.site().subscribe_events();
        cfg.eval("(set-meter-power-factor 7 0.8 t)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 7,
                    knob: "meter-power-factor",
                    value: Some(v),
                    expr: None,
                    leading: Some(true),
                    ..
                } if (*v - 0.8).abs() < 1e-6
            )),
            "no matching KnobChanged on the bus; saw: {seen:?}"
        );
    }

    /// `(set-solar-sunlight id V)` broadcasts a `KnobChanged`
    /// mirroring the meter-power case.
    #[test]
    fn set_solar_sunlight_broadcasts_knob_changed() {
        let (cfg, _dir) = config_with("(%make-solar-inverter :id 8)");
        let mut rx = cfg.site().subscribe_events();
        cfg.eval("(set-solar-sunlight 8 63)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged { id: 8, knob: "solar-sunlight", value: Some(v), expr: None, .. }
                    if (*v - 63.0).abs() < 1e-6
            )),
            "no matching KnobChanged on the bus; saw: {seen:?}"
        );
    }

    /// A dynamic (lambda) source carries the printed source text in
    /// `expr` instead of `None`.
    #[test]
    fn set_meter_power_knob_changed_carries_expr_for_lambda() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        let mut rx = cfg.site().subscribe_events();
        cfg.eval("(set-meter-power 7 (lambda () 25))").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 7,
                    knob: "meter-power",
                    expr: Some(_),
                    ..
                }
            )),
            "no matching KnobChanged with expr on the bus; saw: {seen:?}"
        );
    }

    /// `(clear-meter-power id)` after `(set-meter-power id V)`
    /// restores measuring: `meter_power_reading()` goes back to
    /// `None` and aggregation reads the (empty, here zero) children
    /// sum instead of the constant. Errors on a non-meter.
    #[test]
    fn clear_meter_power_restores_measuring() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        cfg.eval("(set-meter-power 7 5000.0)").unwrap();
        let site = cfg.site();
        let m = site.get(7).unwrap();
        assert!(m.meter_drive().unwrap().meter_power_reading().is_some());

        cfg.eval("(clear-meter-power 7)").unwrap();
        assert!(m.meter_drive().unwrap().meter_power_reading().is_none());
        assert_eq!(m.aggregate_power_w(&site), 0.0);

        // Non-meter: no meter drive, so the clear errors.
        let (cfg2, _dir2) = config_with("(%make-battery :id 4)");
        assert_eq!(
            err_line(&cfg2, "(clear-meter-power 4)"),
            "ERR InvalidArgument: clear-meter-power: component 4 is not a meter"
        );

        // Unknown id errors too.
        assert!(cfg.eval("(clear-meter-power 99)").is_err());
    }

    /// `(clear-meter-reactive id)` clears whichever reactive state is
    /// set — a `Var` override here — restoring the children sum. The
    /// same defun also clears a `PowerFactor` state (one slot, both
    /// shapes go through the same `MeterDrive` method).
    #[test]
    fn clear_meter_reactive_restores_measuring() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        cfg.eval("(set-meter-reactive-power 7 500.0)").unwrap();
        let site = cfg.site();
        let m = site.get(7).unwrap();
        assert!(m.meter_drive().unwrap().meter_reactive_reading().is_some());

        cfg.eval("(clear-meter-reactive 7)").unwrap();
        assert!(m.meter_drive().unwrap().meter_reactive_reading().is_none());
        assert_eq!(m.aggregate_reactive_var(&site), 0.0);

        let err = cfg.eval("(clear-meter-reactive 99)").unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");

        // Non-meter: same "not a meter" error branch as
        // clear-meter-power above.
        let (cfg2, _dir2) = config_with("(%make-battery :id 4)");
        assert_eq!(
            err_line(&cfg2, "(clear-meter-reactive 4)"),
            "ERR InvalidArgument: clear-meter-reactive: component 4 is not a meter"
        );
    }

    /// `(clear-meter-reactive id)` broadcasts on BOTH knob tokens: the
    /// inspector's power-factor input is a separate knob
    /// ("meter-power-factor") from "meter-reactive-power", so a
    /// PowerFactor-shaped clear must blank both or the PF input keeps
    /// showing a stale number until the next full snapshot.
    #[test]
    fn clear_meter_reactive_broadcasts_both_knob_tokens() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 8000.0)");
        cfg.eval("(set-meter-power-factor 7 0.8 t)").unwrap();
        let mut rx = cfg.site().subscribe_events();
        cfg.eval("(clear-meter-reactive 7)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 7,
                    knob: "meter-reactive-power",
                    value: None,
                    expr: None,
                    ..
                }
            )),
            "no meter-reactive-power KnobChanged on the bus; saw: {seen:?}"
        );
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 7,
                    knob: "meter-power-factor",
                    value: None,
                    expr: None,
                    ..
                }
            )),
            "no meter-power-factor KnobChanged on the bus; saw: {seen:?}"
        );
    }

    /// `(clear-meter-power id)` broadcasts a `KnobChanged` with a
    /// `None` value so a live inspector tab blanks the `:power`
    /// input instead of showing a stale number.
    #[test]
    fn clear_meter_power_broadcasts_knob_changed_with_none() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        cfg.eval("(set-meter-power 7 1500)").unwrap();
        let mut rx = cfg.site().subscribe_events();
        cfg.eval("(clear-meter-power 7)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 7,
                    knob: "meter-power",
                    value: None,
                    expr: None,
                    ..
                }
            )),
            "no matching KnobChanged on the bus; saw: {seen:?}"
        );
    }

    /// `(set-meter-power id "garbage")` should error rather than
    /// silently passing through the from_eval branch and tripping
    /// the non-numeric refresh fallback every tick.
    #[test]
    fn set_meter_power_rejects_bare_string() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        // A bare string is from_eval-eligible (returns Some) and
        // would never resolve to a number — but it doesn't roundtrip
        // through a useful curve, so users should reach for a lambda
        // or symbol instead. This assertion documents the behaviour:
        // the call succeeds (string isn't nil) and refresh just keeps
        // the fallback.
        assert!(
            cfg.eval("(set-meter-power 7 \"garbage\")").is_ok(),
            "string is accepted as an eval source — fallback governs",
        );
    }

    /// `(set-meter-reactive-power id VALUE)` mirrors `set-meter-power`'s
    /// dispatch: a number installs a constant Q override, a lambda is
    /// resolved on refresh, and a symbol re-derefs its bound variable
    /// each refresh. Read back through `aggregate_reactive_var`.
    #[test]
    fn set_meter_reactive_power_accepts_number_lambda_symbol() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");

        // Number → constant Q override, no refresh needed.
        cfg.eval("(set-meter-reactive-power 7 500.0)").unwrap();
        let m = cfg.site().get(7).unwrap();
        assert!((m.aggregate_reactive_var(&cfg.site()) - 500.0).abs() < 1e-3);

        // Lambda → dynamic source, resolved on the next refresh.
        cfg.eval("(set-meter-reactive-power 7 (lambda () 1234.5))")
            .unwrap();
        cfg.refresh_once();
        assert!((m.aggregate_reactive_var(&cfg.site()) - 1234.5).abs() < 1e-3);

        // Symbol → deref the bound variable each refresh.
        cfg.eval(
            "(setq reactive-var-src 750.0)
             (set-meter-reactive-power 7 'reactive-var-src)",
        )
        .unwrap();
        cfg.refresh_once();
        assert!((m.aggregate_reactive_var(&cfg.site()) - 750.0).abs() < 1e-3);
        cfg.eval("(setq reactive-var-src 900.0)").unwrap();
        cfg.refresh_once();
        assert!((m.aggregate_reactive_var(&cfg.site()) - 900.0).abs() < 1e-3);
    }

    /// `(set-meter-power-factor id PF &optional LEADING)` derives Q
    /// from the meter's own live P: 0.8 lagging on 8000 W → 6000 VAr;
    /// LEADING flips the sign; an out-of-range PF errors before ever
    /// touching the meter.
    #[test]
    fn set_meter_power_factor_derives_from_live_p() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 8000.0)");
        let m = cfg.site().get(7).unwrap();

        cfg.eval("(set-meter-power-factor 7 0.8)").unwrap();
        assert!((m.aggregate_reactive_var(&cfg.site()) - 6_000.0).abs() < 1.0);

        // Leading flips the sign.
        cfg.eval("(set-meter-power-factor 7 0.8 t)").unwrap();
        assert!((m.aggregate_reactive_var(&cfg.site()) - -6_000.0).abs() < 1.0);

        // Out of (0.0, 1.0] errors, naming the range, and never
        // reaches `MeterDrive::set_power_factor` (which does no
        // validation itself).
        let err = cfg.eval("(set-meter-power-factor 7 1.5)").unwrap_err();
        assert!(err.to_string().contains("(0.0, 1.0]"), "{err}");
        let err = cfg.eval("(set-meter-power-factor 7 0.0)").unwrap_err();
        assert!(err.to_string().contains("(0.0, 1.0]"), "{err}");

        // Unknown id errors.
        assert!(cfg.eval("(set-meter-power-factor 99 0.8)").is_err());
    }

    /// `(set-battery-soc id PCT)` teleports the charge state; the next
    /// telemetry read reflects it. An unknown id errors.
    #[test]
    fn set_battery_soc_teleports_state() {
        let (cfg, _dir) = config_with("(%make-battery :id 4 :initial-soc 60.0)");
        cfg.eval("(set-battery-soc 4 12.5)").unwrap();
        let site = cfg.site();
        let soc = site.get(4).unwrap().telemetry(&site).soc_pct.unwrap();
        assert!((soc - 12.5).abs() < 1e-3, "{soc}");
        assert!(cfg.eval("(set-battery-soc 99 50.0)").is_err());
    }

    /// `meter_power_reading` round-trips a constant `:power` override
    /// (no source text) and a dynamic lambda override (some source
    /// text, opaque as it is — see the `expr` assertion below) — the
    /// knob read-back Task 6's inspector snapshot pulls from.
    #[test]
    fn meter_power_reading_round_trips_constant_and_expr() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        cfg.eval("(set-meter-power 7 1500)").unwrap();
        let site = cfg.site();
        let c = site.get(7).unwrap();
        let r = c
            .meter_drive()
            .unwrap()
            .meter_power_reading()
            .expect("reading");
        assert_eq!(r.value, 1500.0);
        assert_eq!(r.expr, None);

        cfg.eval("(set-meter-power 7 (lambda () 25))").unwrap();
        cfg.refresh_once();
        let r = site
            .get(7)
            .unwrap()
            .meter_drive()
            .unwrap()
            .meter_power_reading()
            .expect("reading");
        assert_eq!(r.value, 25.0);
        // An unquoted lambda evaluates to a compiled function before
        // `DynamicScalar::from_lisp` ever sees it, so it routes
        // through the funcall branch and prints as the opaque
        // "CompiledDefun" — not the literal source text. That's
        // `source_text`'s documented behavior (see Task 1); the
        // read-back contract here is just "dynamic source ⇒ some
        // printed text", not pretty-printing.
        assert!(r.expr.is_some(), "dynamic source should carry expr text");
    }

    /// `meter_reactive_reading` reports the `PowerFactor` shape (pf +
    /// leading) once `(set-meter-power-factor)` has installed one —
    /// the `Var` shape is exercised implicitly by every other
    /// reactive-power test in this file.
    #[test]
    fn meter_power_factor_reading_reports_pf_and_leading() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        cfg.eval("(set-meter-power 7 1000)").unwrap();
        cfg.eval("(set-meter-power-factor 7 0.9 t)").unwrap();
        let site = cfg.site();
        match site
            .get(7)
            .unwrap()
            .meter_drive()
            .unwrap()
            .meter_reactive_reading()
        {
            Some(ReactiveReading::PowerFactor { pf, leading }) => {
                assert!((pf - 0.9).abs() < 1e-6);
                assert!(leading);
            }
            other => panic!("expected PowerFactor, got {other:?}"),
        }
    }

    /// `(clear-solar-sunlight id)` is the way back from a driven
    /// sunlight knob: the source returns to following the site's
    /// weather (the "weather" marker in `sunlight_reading().expr`),
    /// the `KnobChanged` broadcast carries that same marker (so the
    /// inspector doesn't render a bare, markerless 100 right after
    /// the clear — the opposite of the mode the door just installed),
    /// a non-solar component errors, and so does an unknown id.
    #[test]
    fn clear_solar_sunlight_returns_to_following_weather() {
        let (cfg, _dir) = config_with("(%make-solar-inverter :id 8 :sunlight% 40)");
        let site = cfg.site();
        let inv = site.get(8).unwrap();
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().expr,
            None,
            "starts manual"
        );

        cfg.eval("(set-solar-sunlight 8 10.0)").unwrap();
        let mut rx = site.subscribe_events();
        cfg.eval("(clear-solar-sunlight 8)").unwrap();
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().expr,
            Some("weather".into()),
            "cleared back to following the weather"
        );
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 8,
                    knob: "solar-sunlight",
                    expr: Some(e),
                    ..
                } if e == "weather"
            )),
            "no matching KnobChanged with the weather marker on the bus; saw: {seen:?}"
        );

        // Non-solar: no sunlight knob, so the clear errors.
        let (cfg2, _dir2) = config_with("(%make-meter :id 7)");
        assert_eq!(
            err_line(&cfg2, "(clear-solar-sunlight 7)"),
            "ERR InvalidArgument: clear-solar-sunlight: component 7 does not take a \
             sunlight clear"
        );

        // Unknown id errors too.
        let err = cfg.eval("(clear-solar-sunlight 99)").unwrap_err();
        assert!(err.contains("not found"), "{err}");
    }

    /// The clear must survive a save/reload: a `:sunlight%`-built
    /// inverter that has been cleared renders WITHOUT the kwarg, and
    /// re-making from those kwargs yields a weather-following
    /// inverter — not a `Manual(100)` one that silently ignores the
    /// sky forever.
    #[test]
    fn cleared_sunlight_round_trips_through_constructor_kwargs() {
        let (cfg, _dir) = config_with("(%make-solar-inverter :id 8 :sunlight% 40)");
        cfg.eval("(clear-solar-sunlight 8)").unwrap();
        let kwargs = cfg
            .site()
            .get(8)
            .unwrap()
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !kwargs.contains(":sunlight%"),
            "a Follow source renders as the ABSENT kwarg: {kwargs}"
        );

        cfg.eval(&format!("(%make-solar-inverter :id 9 {kwargs})"))
            .unwrap();
        assert_eq!(
            cfg.site()
                .get(9)
                .unwrap()
                .sunlight_drive()
                .unwrap()
                .sunlight_reading()
                .expr,
            Some("weather".into()),
            "the rebuilt inverter follows the weather, not Manual(100)"
        );
    }

    /// `sunlight_reading` reads back the PV inverter's cloud-cover
    /// knob after `(set-solar-sunlight)` pokes in a constant.
    #[test]
    fn sunlight_reading_reads_back_percentage() {
        let (cfg, _dir) = config_with("(%make-solar-inverter :id 4)");
        cfg.eval("(set-solar-sunlight 4 63)").unwrap();
        cfg.refresh_once();
        let site = cfg.site();
        let r = site
            .get(4)
            .unwrap()
            .sunlight_drive()
            .expect("reading")
            .sunlight_reading();
        assert_eq!(r.value, 63.0);
    }

    /// `(set-boiler-demand id N)` installs a constant kg/h demand,
    /// read back immediately through `demand_reading` (no tick
    /// needed — it reads the source directly).
    #[test]
    fn set_boiler_demand_accepts_a_number() {
        let (cfg, _dir) = config_with("(%make-steam-boiler :id 9)");
        cfg.eval("(set-boiler-demand 9 40.0)").unwrap();
        let site = cfg.site();
        let r = site
            .get(9)
            .unwrap()
            .steam_drive()
            .expect("reading")
            .demand_reading();
        assert_eq!(r.value, 40.0);
    }

    /// `(set-boiler-demand id (lambda () X))` installs a dynamic
    /// source, re-resolved on refresh — mirrors
    /// `set_solar_sunlight_accepts_a_lambda` above.
    #[test]
    fn set_boiler_demand_accepts_a_lambda() {
        let (cfg, _dir) = config_with("(%make-steam-boiler :id 9)");
        cfg.eval("(set-boiler-demand 9 (lambda () 25.0))").unwrap();
        cfg.refresh_once();
        cfg.site()
            .tick_once(chrono::Utc::now(), std::time::Duration::from_millis(100));
        let site = cfg.site();
        let r = site
            .get(9)
            .unwrap()
            .steam_drive()
            .expect("reading")
            .demand_reading();
        assert!((r.value - 25.0).abs() < 1e-6, "{}", r.value);
    }

    /// `(set-boiler-pressure id BAR)` overwrites the pressure state,
    /// reflected immediately in telemetry.
    #[test]
    fn set_boiler_pressure_moves_state() {
        let (cfg, _dir) = config_with("(%make-steam-boiler :id 9)");
        cfg.eval("(set-boiler-pressure 9 9.0)").unwrap();
        let site = cfg.site();
        let r = site
            .get(9)
            .unwrap()
            .steam_drive()
            .expect("reading")
            .pressure_reading();
        assert_eq!(r.value, 9.0);
    }

    /// Both boiler defuns error on a non-boiler component instead of
    /// silently no-opping — they're first-class inspector knobs, so
    /// this door is strict (unlike e.g. `set-battery-soc`).
    #[test]
    fn set_boiler_defuns_error_on_non_boiler() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        assert_eq!(
            err_line(&cfg, "(set-boiler-demand 7 40.0)"),
            "ERR InvalidArgument: set-boiler-demand: component 7 is not a steam boiler"
        );
        assert_eq!(
            err_line(&cfg, "(set-boiler-pressure 7 9.0)"),
            "ERR InvalidArgument: set-boiler-pressure: component 7 is not a steam boiler"
        );
    }

    // ── scenario teardown: these setters/clears snapshot BEFORE they
    // mutate, and `(scenario-stop)` restores from that baseline. ─────

    /// A dynamic (symbol) sunlight source survives a scenario driving
    /// it to a constant and back: `(scenario-stop)` restores the
    /// exact captured `DynamicScalar`, not a re-parse of its text, so
    /// both the printed source AND its live tracking come back.
    #[test]
    fn scenario_stop_restores_a_dynamic_sunlight_source() {
        let (cfg, _dir) = config_with(
            "(setq sun-src 40.0)
             (%make-solar-inverter :id 8 :rated-lower -8000.0 :rated-upper 0.0)",
        );
        cfg.eval("(set-solar-sunlight 8 'sun-src)").unwrap();
        cfg.refresh_once();
        let inv = cfg.site().get(8).unwrap();
        let before = inv.sunlight_drive().unwrap().sunlight_reading();
        assert!(
            before.expr.is_some(),
            "baseline is the dynamic symbol source"
        );
        assert_eq!(before.value, 40.0);

        cfg.eval("(scenario-start \"sun\")").unwrap();
        cfg.eval("(set-solar-sunlight 8 10.0)").unwrap();
        assert!(
            inv.sunlight_drive()
                .unwrap()
                .sunlight_reading()
                .expr
                .is_none(),
            "scenario collapsed it to a constant"
        );

        cfg.eval("(scenario-stop)").unwrap();
        let after = inv.sunlight_drive().unwrap().sunlight_reading();
        assert!(after.expr.is_some(), "dynamic source restored");
        assert_eq!(after.expr, before.expr);

        // And it still tracks live: mutate the global, refresh, see
        // it move — proves restore put back the real symbol source,
        // not a frozen snapshot of its last-read value.
        cfg.eval("(setq sun-src 77.0)").unwrap();
        cfg.refresh_once();
        assert_eq!(inv.sunlight_drive().unwrap().sunlight_reading().value, 77.0);
    }

    /// The sunlight twin of
    /// `scenario_stop_restores_a_constructed_power_kwarg_a_clear_dropped`:
    /// a `:sunlight%`-built inverter whose FIRST touch inside the run
    /// is the CLEAR, so the restored baseline can only have come from
    /// the snapshot `clear-solar-sunlight` takes on its way in. The
    /// clear is a user-intent verb — mid-run it really clears (the
    /// source is `Follow`, "weather" marker and all, and the
    /// `:sunlight%` kwarg is dropped so the inverter would save as
    /// weather-following) — but a scenario only borrowed the knob, so
    /// `(scenario-stop)` must put the Manual 40 back, markerless, with
    /// its kwarg.
    #[test]
    fn scenario_stop_restores_a_constructed_sunlight_kwarg_a_clear_dropped() {
        let (cfg, _dir) = config_with("(%make-solar-inverter :id 8 :sunlight% 40)");
        let inv = cfg.site().get(8).unwrap();
        let kwargs = || {
            inv.constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(inv.sunlight_drive().unwrap().sunlight_reading().value, 40.0);
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().expr,
            None,
            "the constructed baseline is Manual, markerless"
        );
        assert!(kwargs().contains(":sunlight% 40"), "{}", kwargs());

        cfg.eval("(scenario-start \"clear-sun\")").unwrap();
        cfg.eval("(clear-solar-sunlight 8)").unwrap();
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().expr,
            Some("weather".into()),
            "the clear must really clear while the scenario runs"
        );
        assert!(
            !kwargs().contains(":sunlight%"),
            "the clear drops the constructed kwarg too: {}",
            kwargs()
        );

        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(inv.sunlight_drive().unwrap().sunlight_reading().value, 40.0);
        assert_eq!(
            inv.sunlight_drive().unwrap().sunlight_reading().expr,
            None,
            "back to Manual 40, not left following the weather"
        );
        assert!(
            kwargs().contains(":sunlight% 40"),
            "the constructed kwarg must come back, not just the live source: {}",
            kwargs()
        );
    }

    /// A meter with no baseline override (measuring its children):
    /// driven by a scenario, then `(scenario-stop)` returns it to
    /// measuring — `meter_power_reading()` is `None` again, not some
    /// leftover scenario value.
    #[test]
    fn scenario_stop_restores_meter_with_no_baseline_to_measuring() {
        let (cfg, _dir) = config_with("(%make-meter :id 7)");
        let m = cfg.site().get(7).unwrap();
        assert!(
            m.meter_drive().unwrap().meter_power_reading().is_none(),
            "starts measuring"
        );

        cfg.eval("(scenario-start \"m-measuring\")").unwrap();
        cfg.eval("(set-meter-power 7 5000.0)").unwrap();
        assert!(m.meter_drive().unwrap().meter_power_reading().is_some());

        cfg.eval("(scenario-stop)").unwrap();
        assert!(
            m.meter_drive().unwrap().meter_power_reading().is_none(),
            "meter must return to measuring, not stay at the scenario's driven value"
        );
    }

    /// A meter constructed with `:power 5000.0`, driven by a scenario
    /// to a dynamic source, then stopped: the reading AND the
    /// `:power` constructor kwarg both come back — restore is
    /// mechanical (unlike `clear-meter-power`, which would drop the
    /// kwarg), and `has_unrenderable_source` reports the meter is
    /// plain-savable again.
    #[test]
    fn scenario_stop_restores_constructed_meter_power_and_kwarg() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 5000.0)");
        let m = cfg.site().get(7).unwrap();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            5000.0
        );

        cfg.eval("(scenario-start \"m-constructed\")").unwrap();
        cfg.eval("(set-meter-power 7 (lambda () 42.0))").unwrap();
        cfg.refresh_once();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            42.0
        );

        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            5000.0
        );
        assert!(!m.has_unrenderable_source());
        let kw = m
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(kw.contains(":power 5000"), "{kw}");
    }

    /// PF/Var aliasing: a baseline `Var` reactive source, driven to a
    /// `PowerFactor` by `set-meter-power-factor`, restores back to
    /// `Var` on `(scenario-stop)` — the whole `ReactiveSource` shape
    /// round-trips, not just a number.
    #[test]
    fn scenario_stop_restores_var_reactive_after_power_factor_drive() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 8000.0)");
        cfg.eval("(set-meter-reactive-power 7 500.0)").unwrap();
        let m = cfg.site().get(7).unwrap();
        match m.meter_drive().unwrap().meter_reactive_reading().unwrap() {
            ReactiveReading::Var(r) => assert_eq!(r.value, 500.0),
            ReactiveReading::PowerFactor { .. } => panic!("expected baseline Var"),
        }

        cfg.eval("(scenario-start \"pf\")").unwrap();
        cfg.eval("(set-meter-power-factor 7 0.8 t)").unwrap();
        match m.meter_drive().unwrap().meter_reactive_reading().unwrap() {
            ReactiveReading::PowerFactor { .. } => {}
            ReactiveReading::Var(_) => panic!("expected PowerFactor after the scenario drive"),
        }

        cfg.eval("(scenario-stop)").unwrap();
        match m.meter_drive().unwrap().meter_reactive_reading().unwrap() {
            ReactiveReading::Var(r) => assert_eq!(r.value, 500.0),
            ReactiveReading::PowerFactor { .. } => panic!("expected Var restored"),
        }
    }

    /// First-snapshot-wins: a scenario driving the same knob twice —
    /// its own drive, then a second direct eval standing in for a
    /// cue re-setting it later (no real timer needed to exercise
    /// this; a cue re-driving the same knob compiles to exactly this
    /// same `set-meter-power` call) — still restores to the value
    /// from BEFORE the scenario ever touched it, not to either driven
    /// value.
    #[test]
    fn scenario_stop_restores_first_snapshot_despite_repeated_drives() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 1200.0)");
        let m = cfg.site().get(7).unwrap();

        cfg.eval("(scenario-start \"first-wins\")").unwrap();
        cfg.eval("(set-meter-power 7 3000.0)").unwrap(); // captures the 1200.0 baseline
        cfg.eval("(set-meter-power 7 7000.0)").unwrap(); // a second direct drive — no-op on the baseline
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            7000.0
        );

        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            1200.0,
            "restore must land on the FIRST pre-scenario value, not an intermediate drive"
        );
    }

    /// `(scenario-stop)` is idempotent, and nothing resurrects a
    /// manual poke made AFTER it: the baseline map was drained by the
    /// first stop, so a second stop restores nothing and leaves a
    /// later poke exactly as the user left it.
    #[test]
    fn scenario_stop_is_idempotent_and_a_post_stop_poke_sticks() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 1500.0)");
        let m = cfg.site().get(7).unwrap();

        cfg.eval("(scenario-start \"idempotent\")").unwrap();
        cfg.eval("(set-meter-power 7 4000.0)").unwrap();
        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            1500.0
        );

        // A manual poke after stop: nothing tracks it anymore, so it
        // just sticks.
        cfg.eval("(set-meter-power 7 9999.0)").unwrap();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            9999.0
        );

        // A second stop must not disturb it.
        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            9999.0,
            "second stop must be a no-op — it must not resurrect a pre-scenario value \
             over a later manual poke"
        );
    }

    /// The case restore exists for, on its real path: a scenario
    /// CLEARS a knob the component was constructed with. `clear` is a
    /// user-intent verb — it drops the `:power` kwarg too, so the
    /// component saves as "measuring" — which is right for a user and
    /// wrong for a scenario that only borrowed the knob. Mid-scenario
    /// the clear must take full effect (no reading, no kwarg); at stop
    /// BOTH halves must come back, or the meter is left permanently
    /// unable to write its own `:power` back to disk.
    #[test]
    fn scenario_stop_restores_a_constructed_power_kwarg_a_clear_dropped() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 5000.0)");
        let m = cfg.site().get(7).unwrap();
        let kwargs = || {
            cfg.site()
                .get(7)
                .unwrap()
                .constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kwargs().contains(":power 5000"), "{}", kwargs());

        cfg.eval("(scenario-start \"clear-p\")").unwrap();
        cfg.eval("(clear-meter-power 7)").unwrap();
        assert!(
            m.meter_drive().unwrap().meter_power_reading().is_none(),
            "the clear must really clear while the scenario runs"
        );
        assert!(
            !kwargs().contains(":power"),
            "the clear drops the constructed kwarg too: {}",
            kwargs()
        );

        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(
            m.meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            5000.0
        );
        assert!(
            kwargs().contains(":power 5000"),
            "the constructed kwarg must come back, not just the live source: {}",
            kwargs()
        );
    }

    /// The reactive twin: a `:reactive-power`-constructed meter,
    /// cleared mid-scenario, gets both its `Var` source and its
    /// `:reactive-power` kwarg back at stop.
    #[test]
    fn scenario_stop_restores_a_constructed_reactive_kwarg_a_clear_dropped() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 8000.0 :reactive-power 500.0)");
        let m = cfg.site().get(7).unwrap();
        let kwargs = || {
            cfg.site()
                .get(7)
                .unwrap()
                .constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kwargs().contains(":reactive-power 500"), "{}", kwargs());

        cfg.eval("(scenario-start \"clear-q\")").unwrap();
        cfg.eval("(clear-meter-reactive 7)").unwrap();
        assert!(m.meter_drive().unwrap().meter_reactive_reading().is_none());
        assert!(!kwargs().contains(":reactive-power"), "{}", kwargs());

        cfg.eval("(scenario-stop)").unwrap();
        match m.meter_drive().unwrap().meter_reactive_reading().unwrap() {
            ReactiveReading::Var(r) => assert_eq!(r.value, 500.0),
            ReactiveReading::PowerFactor { .. } => panic!("expected the constructed Var back"),
        }
        assert!(
            kwargs().contains(":reactive-power 500"),
            "the constructed kwarg must come back: {}",
            kwargs()
        );
        // The active axis was never touched, so its own kwarg stands.
        assert!(kwargs().contains(":power 8000"), "{}", kwargs());
    }

    /// Same again for the OTHER `ConstructedReactive` shape: a meter
    /// built with `:power-factor` (+ `:leading`) round-trips the pf
    /// pair, not a number — the reactive snapshot carries the enum.
    #[test]
    fn scenario_stop_restores_a_constructed_power_factor_a_clear_dropped() {
        let (cfg, _dir) =
            config_with("(%make-meter :id 7 :power 8000.0 :power-factor 0.8 :leading t)");
        let m = cfg.site().get(7).unwrap();
        let kwargs = || {
            cfg.site()
                .get(7)
                .unwrap()
                .constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kwargs().contains(":power-factor 0.8"), "{}", kwargs());
        assert!(kwargs().contains(":leading t"), "{}", kwargs());

        cfg.eval("(scenario-start \"clear-pf\")").unwrap();
        cfg.eval("(clear-meter-reactive 7)").unwrap();
        assert!(m.meter_drive().unwrap().meter_reactive_reading().is_none());
        assert!(!kwargs().contains(":power-factor"), "{}", kwargs());

        cfg.eval("(scenario-stop)").unwrap();
        match m.meter_drive().unwrap().meter_reactive_reading().unwrap() {
            ReactiveReading::PowerFactor { pf, leading } => {
                assert_eq!(pf, 0.8);
                assert!(leading, "the leading flag is part of the constructed pair");
            }
            ReactiveReading::Var(_) => panic!("expected the constructed PowerFactor back"),
        }
        assert!(kwargs().contains(":power-factor 0.8"), "{}", kwargs());
        assert!(kwargs().contains(":leading t"), "{}", kwargs());
    }

    /// Boiler-demand twin of the sunlight test above: a constant
    /// baseline, driven to a dynamic (lambda) source by a scenario,
    /// restores to the constant on `(scenario-stop)`.
    #[test]
    fn scenario_stop_restores_boiler_demand_after_dynamic_drive() {
        let (cfg, _dir) = config_with("(%make-steam-boiler :id 9)");
        cfg.eval("(set-boiler-demand 9 40.0)").unwrap();
        let b = cfg.site().get(9).unwrap();
        assert_eq!(b.steam_drive().unwrap().demand_reading().value, 40.0);
        assert!(!b.has_unrenderable_source());

        cfg.eval("(scenario-start \"boiler\")").unwrap();
        cfg.eval("(set-boiler-demand 9 (lambda () 99.0))").unwrap();
        assert!(b.has_unrenderable_source());

        cfg.eval("(scenario-stop)").unwrap();
        assert_eq!(b.steam_drive().unwrap().demand_reading().value, 40.0);
        assert!(!b.has_unrenderable_source());
    }

    /// `(plug-ev ID PRESET …)` spreads its overrides into `%plug-ev`,
    /// the car lands on the charger, `(ev-info ID)` prints it as a
    /// plist, and `(unplug-ev ID)` takes it back off.
    #[test]
    fn plug_ev_plugs_a_preset_with_overrides_and_unplug_clears_it() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7)");
        let ev = cfg.site().get(7).unwrap();
        cfg.eval("(plug-ev 7 'sedan :soc 30 :phases 2 :target-soc 80)")
            .unwrap();
        let info = ev.ev_port().unwrap().ev_info().expect("plugged");
        assert_eq!(
            (
                info.ev.preset,
                info.ev.soc_pct,
                info.ev.phases,
                info.ev.target_soc_pct
            ),
            ("sedan", 30.0, 2, 80.0)
        );
        let printed = cfg.eval("(ev-info 7)").unwrap();
        assert!(
            printed.contains(":preset") && printed.contains("sedan") && printed.contains(":soc"),
            "{printed}"
        );
        cfg.eval("(unplug-ev 7)").unwrap();
        assert!(ev.ev_port().unwrap().ev_info().is_none());
        assert_eq!(cfg.eval("(ev-info 7)").unwrap(), "nil");
    }

    /// Every rejection names the component and the reason; a
    /// non-charger's `set-battery-soc` stays lenient.
    #[test]
    fn plug_ev_error_cases_name_the_reason() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7) (%make-meter :id 8)");
        let err = |src: &str| cfg.eval(src).unwrap_err().to_string();
        assert!(err("(plug-ev 99 'sedan)").contains("not found"));
        assert_eq!(
            err_line(&cfg, "(plug-ev 8 'sedan)"),
            "ERR InvalidArgument: plug-ev: component 8 is not an EV charger"
        );
        assert!(err("(plug-ev 7 'unicorn)").contains("unknown preset"));
        assert!(err("(plug-ev 7 'sedan :phases 5)").contains("phases"));
        let soc_err = err("(plug-ev 7 'sedan :soc 120)");
        assert!(soc_err.contains("soc"), "{soc_err}");
        // Every reason names the component it was about, so a
        // scenario log says WHICH charger refused the plug.
        assert!(soc_err.contains("component 7"), "{soc_err}");
        assert!(err("(plug-ev 7 'sedan :target-soc 120)").contains("target-soc"));
        // `ev-info` is a query, not a write: "is there a car?" is
        // answered nil for a meter as much as for an empty charger.
        // Only a missing id is an error.
        assert_eq!(cfg.eval("(ev-info 8)").unwrap(), "nil");
        assert!(err("(ev-info 99)").contains("not found"));
        assert_eq!(
            cfg.eval("(unplug-ev 7)").unwrap(),
            "nil",
            "unplugging an empty charger is a no-op, not an error"
        );
        cfg.eval("(plug-ev 7 'sedan)").unwrap();
        assert_eq!(
            err_line(&cfg, "(plug-ev 7 'van)"),
            "ERR InvalidArgument: plug-ev: component 7: an EV is already plugged in"
        );
        assert!(
            cfg.eval("(set-battery-soc 8 50)").is_ok(),
            "non-chargers stay lenient"
        );
    }

    /// A refused plug must not seed the scenario's plug baseline:
    /// an `Ev` snapshot holds the whole car, so a baseline taken
    /// during a rejected call would make teardown rewind the SoC and
    /// energy the car accumulated before that call.
    #[test]
    fn a_refused_plug_does_not_seed_the_scenario_baseline() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7)");
        let ev = cfg.site().get(7).unwrap();
        cfg.eval("(plug-ev 7 'van)").unwrap();
        cfg.eval("(scenario-start \"ev\")").unwrap();
        // Rejected: the charger is occupied.
        assert!(cfg.eval("(plug-ev 7 'sedan)").is_err());
        // Move the car through the trait setter rather than
        // `set-battery-soc`: every Lisp/HTTP door onto a charger's SoC
        // now takes the plug snapshot itself, so a door here would
        // seed the very baseline this test is trying to prove absent.
        assert!(ev.ev_port().unwrap().set_ev_soc_pct(77.0));
        cfg.eval("(scenario-stop)").unwrap();
        let info = ev
            .ev_port()
            .unwrap()
            .ev_info()
            .expect("the van is still plugged in");
        assert_eq!(info.ev.preset, "van");
        assert_eq!(
            info.ev.soc_pct, 77.0,
            "teardown must not rewind a car the scenario never plugged"
        );
    }

    /// `set-battery-soc` on a charger moves the plugged car's SoC,
    /// and says so when there is no car to move.
    #[test]
    fn set_battery_soc_on_a_charger_needs_a_car() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7)");
        assert_eq!(
            err_line(&cfg, "(set-battery-soc 7 50)"),
            "ERR InvalidArgument: set-battery-soc: charger 7 has no EV plugged in"
        );
        cfg.eval("(plug-ev 7 'sedan)").unwrap();
        cfg.eval("(set-battery-soc 7 50)").unwrap();
        assert_eq!(
            cfg.site()
                .get(7)
                .unwrap()
                .ev_port()
                .unwrap()
                .ev_info()
                .unwrap()
                .ev
                .soc_pct,
            50.0
        );
    }

    /// Plug state is a scenario knob: teardown undoes both
    /// directions — a car a scenario plugged comes back out, a car it
    /// unplugged goes back in.
    #[test]
    fn scenario_stop_unplugs_what_a_scenario_plugged_and_replugs_what_it_unplugged() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7) (%make-ev-charger :id 8)");
        let site = cfg.site();
        cfg.eval("(plug-ev 8 'van)").unwrap();
        cfg.eval("(scenario-start \"ev\")").unwrap();
        cfg.eval("(plug-ev 7 'sedan)").unwrap();
        cfg.eval("(unplug-ev 8)").unwrap();
        assert!(site.get(7).unwrap().ev_port().unwrap().ev_info().is_some());
        assert!(site.get(8).unwrap().ev_port().unwrap().ev_info().is_none());
        cfg.eval("(scenario-stop)").unwrap();
        assert!(
            site.get(7).unwrap().ev_port().unwrap().ev_info().is_none(),
            "teardown unplugs"
        );
        assert_eq!(
            site.get(8)
                .unwrap()
                .ev_port()
                .unwrap()
                .ev_info()
                .unwrap()
                .ev
                .preset,
            "van",
            "teardown replugs"
        );
    }

    /// `set-battery-soc` on a charger is a write to the CAR, so it
    /// takes the plug knob's snapshot like `plug-ev` / `unplug-ev` do
    /// — and teardown therefore puts the car back exactly as it was
    /// whatever order the run moved the SoC and pulled the plug in.
    #[test]
    fn scenario_stop_undoes_a_chargers_set_battery_soc() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7)");
        let site = cfg.site();
        cfg.eval("(plug-ev 7 'van :soc 40)").unwrap();
        cfg.eval("(scenario-start \"ev\")").unwrap();
        cfg.eval("(set-battery-soc 7 10)").unwrap();
        // The unplug's own snapshot comes second, so only the SoC
        // write's baseline can carry the van's pre-scenario 40 %.
        cfg.eval("(unplug-ev 7)").unwrap();
        cfg.eval("(scenario-stop)").unwrap();
        let info = site
            .get(7)
            .unwrap()
            .ev_port()
            .unwrap()
            .ev_info()
            .expect("the van is back");
        assert_eq!(
            (info.ev.preset, info.ev.soc_pct),
            ("van", 40.0),
            "teardown restores the car the run found, at the SoC it found it at"
        );
    }

    /// An unplug that takes nothing away must not claim the plug
    /// knob's baseline: seeding `Ev(None)` on an empty charger would
    /// have teardown "restore" an emptiness the scenario never
    /// displaced, and announce it with a knob event nobody asked for.
    #[test]
    fn unplug_ev_on_an_empty_charger_does_not_seed_the_scenario_baseline() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7)");
        let site = cfg.site();
        let mut rx = site.subscribe_events();
        cfg.eval("(scenario-start \"ev\")").unwrap();
        assert_eq!(
            cfg.eval("(unplug-ev 7)").unwrap(),
            "nil",
            "there was no car to take away"
        );
        cfg.eval("(scenario-stop)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            !seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 7,
                    knob: "ev",
                    ..
                }
            )),
            "teardown restored a plug knob the scenario never displaced; saw: {seen:?}"
        );
    }

    /// `unplug-ev` on a component that is not a charger is an error
    /// naming the component, not a quiet `nil`.
    #[test]
    fn unplug_ev_errors_on_a_non_charger() {
        let (cfg, _dir) = config_with("(%make-meter :id 8)");
        assert_eq!(
            err_line(&cfg, "(unplug-ev 8)"),
            "ERR InvalidArgument: unplug-ev: component 8 is not an EV charger"
        );
    }

    /// `ev-info` answers `nil` both for a component that takes no car
    /// and for a charger that never had one.
    #[test]
    fn ev_info_is_nil_without_a_car() {
        let (cfg, _dir) = config_with("(%make-ev-charger :id 7) (%make-battery :id 4)");
        assert_eq!(cfg.eval("(ev-info 7)").unwrap(), "nil");
        assert_eq!(cfg.eval("(ev-info 4)").unwrap(), "nil");
    }

    // ── lenient setters: on a component of the wrong kind each one
    // returns t, changes nothing, and still broadcasts its knob. ─────

    /// `set-meter-power` on a battery, numeric and lambda.
    #[test]
    fn set_meter_power_on_a_non_meter_is_a_lenient_noop() {
        let (cfg, _dir) = config_with("(%make-battery :id 4)");
        assert_lenient_noop(&cfg, 4, "(set-meter-power 4 5000.0)", Some("meter-power"));
        assert_lenient_noop(
            &cfg,
            4,
            "(set-meter-power 4 (lambda () 42.0))",
            Some("meter-power"),
        );
    }

    /// `set-meter-reactive-power` on a battery, numeric and lambda.
    #[test]
    fn set_meter_reactive_power_on_a_non_meter_is_a_lenient_noop() {
        let (cfg, _dir) = config_with("(%make-battery :id 4)");
        assert_lenient_noop(
            &cfg,
            4,
            "(set-meter-reactive-power 4 500.0)",
            Some("meter-reactive-power"),
        );
        assert_lenient_noop(
            &cfg,
            4,
            "(set-meter-reactive-power 4 (lambda () 42.0))",
            Some("meter-reactive-power"),
        );
    }

    /// `set-meter-power-factor` on a battery.
    #[test]
    fn set_meter_power_factor_on_a_non_meter_is_a_lenient_noop() {
        let (cfg, _dir) = config_with("(%make-battery :id 4)");
        assert_lenient_noop(
            &cfg,
            4,
            "(set-meter-power-factor 4 0.8 t)",
            Some("meter-power-factor"),
        );
    }

    /// `set-solar-sunlight` on a meter, numeric and lambda.
    #[test]
    fn set_solar_sunlight_on_a_non_solar_is_a_lenient_noop() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 1500.0)");
        assert_lenient_noop(
            &cfg,
            7,
            "(set-solar-sunlight 7 40.0)",
            Some("solar-sunlight"),
        );
        assert_lenient_noop(
            &cfg,
            7,
            "(set-solar-sunlight 7 (lambda () 40.0))",
            Some("solar-sunlight"),
        );
    }

    /// `set-battery-soc` on a component that is neither a battery nor
    /// a charger. The defun broadcasts no knob, so none is checked.
    #[test]
    fn set_battery_soc_on_a_non_battery_is_a_lenient_noop() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power 1500.0)");
        assert_lenient_noop(&cfg, 7, "(set-battery-soc 7 50)", None);
    }

    /// `(ev-presets)` prints the whole catalog, one plist per car.
    #[test]
    fn ev_presets_lists_the_catalog() {
        let (cfg, _dir) = config_with("nil");
        let printed = cfg.eval("(ev-presets)").unwrap();
        for name in ["phev", "city", "sedan", "van"] {
            assert!(printed.contains(name), "{printed}");
        }
    }
}
