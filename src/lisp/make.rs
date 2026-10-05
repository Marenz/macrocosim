//! `(%make-grid-connection-point)`, `(%make-meter)`, `(%make-battery)`,
//! … — the Rust-side constructor primitives the lisp DSL dispatches
//! to. Each takes its arguments as a typed plist via tulisp's
//! `AsPlist!` macro and returns a `ComponentHandle` (an opaque
//! `Shared<dyn TulispAny>` on the lisp side). The user-facing names
//! (`(make-grid-connection-point)`, `(make-meter)`, …) are `defun`
//! wrappers in `sim/defaults.lisp`
//! that prepend a category-default plist to the caller's args
//! before invoking these primitives; AsPlist's last-occurrence-wins
//! key resolution lets per-component values override defaults
//! without any extra plumbing on the Rust side.

use std::time::Duration;

use tulisp::{AsPlist, Error, Plist, TulispContext};

use crate::lisp::value::LispValue;
use crate::sim::{
    Battery, BatteryInverter, Category, ComponentHandle, EvCharger, Grid, Marker, Meter,
    MicrogridSite, OperationalMode, ReactiveSource, SolarInverter, SteamBoiler, SunlightDrive,
    battery::BatteryConfig,
    dynamic_scalar::DynamicScalar,
    ev_charger::{EvChargerConfig, EvIdle},
    ev_presets::{EvPreset, preset},
    inverter::battery_inverter::BatteryInverterConfig,
    inverter::solar_inverter::SolarInverterConfig,
    runtime::{CommandMode, Health, TelemetryMode},
    steam_boiler::SteamBoilerConfig,
};

// -----------------------------------------------------------------------------
// make-grid-connection-point
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct GridArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        rated_fuse_current<":rated-fuse-current">: Option<i64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-meter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct MeterArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        /// Constant, lambda, or symbol. Resolved into a
        /// [`DynamicScalar`] in the constructor — see
        /// [`crate::sim::dynamic_scalar::DynamicScalar::from_lisp`].
        power: Option<LispValue> {= None},
        /// Direct VAr source: constant, lambda, or symbol. Mutually
        /// exclusive with `:power-factor`.
        reactive_power<":reactive-power">: Option<LispValue> {= None},
        /// Derive Q from this meter's own live P via a power factor
        /// (true cos φ) in `(0.0, 1.0]`. Mutually exclusive with
        /// `:reactive-power`.
        power_factor<":power-factor">: Option<f64> {= None},
        /// Whether the power-factor-derived Q is leading (negative)
        /// rather than lagging. Requires `:power-factor`.
        leading: Option<bool> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        hidden: Option<bool> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        capacity_wh<":capacity">: Option<f64> {= None},
        initial_soc<":initial-soc">: Option<f64> {= None},
        soc_lower<":soc-lower">: Option<f64> {= None},
        soc_upper<":soc-upper">: Option<f64> {= None},
        voltage: Option<f64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        soc_protect_margin<":soc-protect-margin">: Option<f64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-battery-inverter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct BatteryInverterArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        successors: Option<Vec<ComponentHandle>> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        /// Time a command takes to reach the output once the
        /// component has it, in ms, on both axes. 100 unless set.
        device_delay_ms<":device-delay-ms">: Option<i64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
        /// PF-style Q cap: |Q| ≤ k × |P|. Pass 0 to disable (nil inherits the default).
        reactive_pf_limit<":reactive-pf-limit">: Option<f64> {= None},
        /// kVA-style Q cap: P² + Q² ≤ apparent². Pass 0 to disable (nil inherits the default).
        reactive_apparent_va<":reactive-apparent-va">: Option<f64> {= None},
        /// Inverter-internal latency before a Q setpoint starts
        /// being tracked. Defaults to 100 ms.
        reactive_command_delay_ms<":reactive-command-delay-ms">: Option<i64> {= None},
        /// Reactive slew rate (VAR/s). Default 2000 ≈ IEEE 1547-2018
        /// Cat B 5 s OLRT for a 10 kVAR window.
        reactive_ramp_rate<":reactive-ramp-rate">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-solar-inverter
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct SolarInverterArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        /// Cloud-cover percentage. May be a number, a lambda, or a
        /// symbol — see [`crate::sim::dynamic_scalar::DynamicScalar::from_lisp`].
        /// Resolved each tick via the scheduler's pre-tick hook.
        /// Leaving it out is the opt-in to the site's weather: the
        /// inverter tracks the sky instead of a driven number, shaped
        /// by `:weather-lag-s` / `:weather-jitter-pct`.
        sunlight_pct<":sunlight%">: Option<LispValue> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        /// The array's peak DC output (Wp), positive — not an
        /// instantaneous power. Defaults to |:rated-lower|: a matched
        /// array. Oversizing produces midday clipping.
        array_peak_w<":array-peak-w">: Option<f64> {= None},
        /// How far behind the sky a weather-following array samples,
        /// in seconds. Left out, each inverter gets a small stable
        /// offset derived from its id (0–60 s) so a cloud sweeps
        /// across a multi-PV site; `0` opts out of that. Only
        /// meaningful without `:sunlight%`.
        weather_lag_s<":weather-lag-s">: Option<f64> {= None},
        /// Per-tick uniform ±roughening of a weather-following
        /// sample, in percent of the value. Only meaningful without
        /// `:sunlight%`.
        weather_jitter_pct<":weather-jitter-pct">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        /// Time a command takes to reach the output once the
        /// component has it, in ms, on both axes. 100 unless set.
        device_delay_ms<":device-delay-ms">: Option<i64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
        /// PF-style Q cap: |Q| ≤ k × |P|. Pass 0 to disable.
        reactive_pf_limit<":reactive-pf-limit">: Option<f64> {= None},
        /// kVA-style Q cap: P² + Q² ≤ apparent². Pass 0 to disable.
        reactive_apparent_va<":reactive-apparent-va">: Option<f64> {= None},
        /// Inverter-internal latency before a Q setpoint starts being
        /// tracked. Defaults to 100 ms.
        reactive_command_delay_ms<":reactive-command-delay-ms">: Option<i64> {= None},
        /// Reactive slew rate (VAR/s). Default 2000.
        reactive_ramp_rate<":reactive-ramp-rate">: Option<f64> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-ev-charger
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct EvChargerArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        /// Time a command takes to reach the output once the
        /// component has it, in ms, on both axes. 100 unless set.
        device_delay_ms<":device-delay-ms">: Option<i64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        // The pack kwargs a charger used to own. The pack is the car's
        // now, so these are taken and ignored (with a warning) rather
        // than rejected: a managed file, a snapshot, or an
        // `enterprise.lisp` whose persisted `ev-charger-defaults` still
        // carries `:soc-protect-margin` was written by the previous
        // binary and must still load — the same courtesy
        // `load-overrides` in sim/common.lisp pays a config that
        // predates managed files. Rejecting them would fail the whole
        // microgrid, and in the enterprise case every charger in the
        // process.
        capacity_wh<":capacity">: Option<f64> {= None},
        initial_soc<":initial-soc">: Option<f64> {= None},
        soc_lower<":soc-lower">: Option<f64> {= None},
        soc_upper<":soc-upper">: Option<f64> {= None},
        soc_protect_margin<":soc-protect-margin">: Option<f64> {= None},
        /// 1 or 3: the phases the charger is wired on.
        phases<":phases">: Option<i64> {= None},
        /// What the charger offers with no command standing: `'paused`
        /// (nothing, the default) or `'full` (its whole rating).
        idle<":idle">: Option<EvIdle> {= None},
        /// Keep the armed command through a health fault and ramp back
        /// on recovery; off (the default), the fault clears the command
        /// and recovery waits for a new one.
        resume_on_recovery<":resume-on-recovery">: Option<bool> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// make-steam-boiler
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct SteamBoilerArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        interval: Option<i64> {= None},
        /// kg/h; number seeds a constant, lambda/symbol installs a
        /// dynamic source resolved each tick.
        demand<":demand">: Option<LispValue> {= None},
        rated_lower<":rated-lower">: Option<f64> {= None},
        rated_upper<":rated-upper">: Option<f64> {= None},
        target_bar<":target-bar">: Option<f64> {= None},
        max_bar<":max-bar">: Option<f64> {= None},
        initial_bar<":initial-bar">: Option<f64> {= None},
        capacity_wh_per_bar<":capacity-wh-per-bar">: Option<f64> {= None},
        wh_per_kg<":wh-per-kg">: Option<f64> {= None},
        command_delay_ms<":command-delay-ms">: Option<i64> {= None},
        ramp_rate<":ramp-rate">: Option<f64> {= None},
        /// Time a command takes to reach the output once the
        /// component has it, in ms, on both axes. 100 unless set.
        device_delay_ms<":device-delay-ms">: Option<i64> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// the marker categories (chp, wind turbine, power transformer,
// breaker)
// -----------------------------------------------------------------------------

AsPlist! {
    pub struct MarkerArgs {
        id: Option<i64> {= None},
        name: Option<String> {= None},
        stream_jitter_pct<":stream-jitter-pct">: Option<f64> {= None},
        operational_mode<":operational-mode">: Option<OperationalMode> {= None},
        health<":health">: Option<Health> {= None},
        telemetry_mode<":telemetry-mode">: Option<TelemetryMode> {= None},
        command_mode<":command-mode">: Option<CommandMode> {= None},
    }
}

// -----------------------------------------------------------------------------
// Registration
// -----------------------------------------------------------------------------

pub fn register(ctx: &mut TulispContext, router: crate::sim::microgrids::SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "%make-grid-connection-point",
        move |_ctx: &mut TulispContext, args: Plist<GridArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&r, &w, a.id)?;
            let rated_active_bounds = match (a.rated_lower, a.rated_upper) {
                (Some(l), Some(u)) => Some((l as f32, u as f32)),
                (None, None) => None,
                // Real site exports can carry only one side (the
                // importer emits each side independently), but the
                // bounds plumbing wants a full pair — so a half pair
                // keeps the grid unbounded. Warn instead of staying
                // silent; one-sided support is a todo.org item.
                (l, u) => {
                    log::warn!(
                        "make-grid-connection-point: only one of :rated-lower/:rated-upper \
                         given ({l:?}, {u:?}); the grid stays unbounded"
                    );
                    None
                }
            };
            let fuse = match a.rated_fuse_current.unwrap_or(0) {
                // The proto field is unsigned; a negative here would
                // wrap through `as u32` into a ~4 GA fuse.
                f if f < 0 => {
                    return Err(Error::invalid_argument(format!(
                        ":rated-fuse-current must be non-negative, got {f}"
                    )));
                }
                f => f as u32,
            };
            let grid = Grid::new(
                id,
                fuse,
                rated_active_bounds,
                a.stream_jitter_pct.unwrap_or(0.0) as f32,
            );
            let h = register_with_modes(
                &w,
                grid,
                a.operational_mode,
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-meter",
        move |_ctx: &mut TulispContext, args: Plist<MeterArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&r, &w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let hidden = a.hidden.unwrap_or(false);
            // :power may be a number, a lambda, or a symbol. The
            // wrapper-expanded category default lands in `a.power`
            // when no per-component value was passed; otherwise the
            // per-component value overrides via AsPlist's last-wins.
            // `DynamicScalar::from_lisp` dispatches on shape — constant
            // for numbers, eval/funcall for the rest.
            let power_source = a
                .power
                .as_ref()
                .and_then(|v| DynamicScalar::from_lisp(v.as_inner(), 0.0));
            let reactive_source =
                meter_reactive_source(a.reactive_power.as_ref(), a.power_factor, a.leading)?;
            let meter = Meter::new(
                id,
                interval,
                power_source,
                reactive_source,
                a.stream_jitter_pct.unwrap_or(0.0) as f32,
                hidden,
            );
            let h = register_with_modes(
                &w,
                meter,
                a.operational_mode,
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-battery",
        move |_ctx: &mut TulispContext, args: Plist<BatteryArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&r, &w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = BatteryConfig::default();
            if let Some(v) = a.capacity_wh {
                cfg.capacity_wh = v as f32;
            }
            if let Some(v) = a.initial_soc {
                cfg.initial_soc_pct = v as f32;
            }
            if let Some(v) = a.soc_lower {
                cfg.soc_lower_pct = v as f32;
            }
            if let Some(v) = a.soc_upper {
                cfg.soc_upper_pct = v as f32;
            }
            if let Some(v) = a.voltage {
                cfg.voltage_v = v as f32;
            }
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.soc_protect_margin {
                cfg.soc_protect_margin_pct = v as f32;
            }
            if let Some(v) = a.stream_jitter_pct {
                cfg.stream_jitter_pct = v as f32;
            }
            let h = register_with_modes(
                &w,
                Battery::new(id, interval, cfg),
                a.operational_mode,
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-battery-inverter",
        move |_ctx: &mut TulispContext, args: Plist<BatteryInverterArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&r, &w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = BatteryInverterConfig::default();
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = checked_ramp_rate(":ramp-rate", v)?;
            }
            if let Some(v) = a.device_delay_ms {
                cfg.device_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.stream_jitter_pct {
                cfg.stream_jitter_pct = v as f32;
            }
            ReactiveKwargs {
                pf_limit: a.reactive_pf_limit,
                apparent_va: a.reactive_apparent_va,
                command_delay_ms: a.reactive_command_delay_ms,
                ramp_rate: a.reactive_ramp_rate,
            }
            .apply(
                &mut cfg.reactive,
                &mut cfg.reactive_command_delay,
                &mut cfg.reactive_ramp_rate_var_per_s,
            )?;
            let h = register_with_modes(
                &w,
                BatteryInverter::new(id, interval, cfg),
                a.operational_mode,
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            connect_successors(&w, id, &a.successors);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-solar-inverter",
        move |_ctx: &mut TulispContext, args: Plist<SolarInverterArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&r, &w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = SolarInverterConfig::default();
            // :sunlight% accepts a number, lambda, or symbol. Number
            // seeds the initial ramp target on `cfg.sunlight_pct`;
            // lambda / symbol installs a dynamic source that takes
            // effect on the first `refresh_inputs`, over a constant
            // slot still holding the default fallback. The wrapper-
            // expanded category default lands in `a.sunlight_pct`
            // already; per-component plist overrides via last-wins.
            let mut dynamic_sunlight: Option<DynamicScalar> = None;
            match a.sunlight_pct.as_ref() {
                // No `:sunlight%` at all is the opt-in to weather —
                // `None` is exactly the config shape that starts the
                // inverter on a `Follow` slot, and the absent kwarg is
                // what `constructor_kwargs` renders back for one, so a
                // saved-and-reloaded weather-following inverter comes
                // back following the weather instead of frozen at a
                // Manual 100%.
                None => cfg.sunlight_pct = None,
                Some(v) => {
                    let raw = v.as_inner();
                    if raw.numberp() {
                        if let Ok(pct) = f64::try_from(raw) {
                            cfg.sunlight_pct = Some(pct as f32);
                        }
                    } else {
                        let fallback = cfg.sunlight_pct.unwrap_or(100.0);
                        dynamic_sunlight = DynamicScalar::from_lisp(raw, fallback);
                        cfg.sunlight_dynamic = true;
                    }
                }
            }
            if let Some(v) = a.weather_lag_s {
                // Bounded at the hour of cloud history `Weather` keeps
                // — a longer lag samples pruned events, so the array
                // silently never clouds over. The shared validator
                // also does the fallible seconds→Duration conversion,
                // because `from_secs_f64` PANICS on a non-finite or
                // overflowing value and one typo'd kwarg must not
                // abort the whole config load.
                cfg.weather_lag = Some(
                    crate::sim::weather::validate::weather_lag_s(v)
                        .map_err(|e| Error::invalid_argument(format!(":weather-lag-s {e}")))?,
                );
            }
            if let Some(v) = a.weather_jitter_pct {
                // 100% would let a sample reach zero (or double), which
                // is weather, not roughening — the upper end is open.
                if !(v.is_finite() && (0.0..100.0).contains(&v)) {
                    return Err(Error::invalid_argument(format!(
                        ":weather-jitter-pct must be within [0, 100), got {v}"
                    )));
                }
                cfg.weather_jitter_pct = v as f32;
            }
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            // Defaults to a matched array (|:rated-lower|) now that
            // :rated-lower has landed on cfg above.
            if let Some(v) = a.array_peak_w {
                // A non-finite or non-positive array inverts the band
                // (min_avail_w goes positive, intersects emptily with
                // rated) and silently parks the inverter at 0 forever
                // — the same failure mode :peak% is guarded against
                // above, with nothing in telemetry to say why.
                if !(v.is_finite() && v > 0.0) {
                    return Err(Error::invalid_argument(format!(
                        ":array-peak-w must be a positive number, got {v}"
                    )));
                }
            }
            cfg.array_peak_w = a
                .array_peak_w
                .map(|v| v as f32)
                .unwrap_or_else(|| cfg.rated_lower_w.abs());
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = checked_ramp_rate(":ramp-rate", v)?;
            }
            if let Some(v) = a.device_delay_ms {
                cfg.device_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.stream_jitter_pct {
                cfg.stream_jitter_pct = v as f32;
            }
            ReactiveKwargs {
                pf_limit: a.reactive_pf_limit,
                apparent_va: a.reactive_apparent_va,
                command_delay_ms: a.reactive_command_delay_ms,
                ramp_rate: a.reactive_ramp_rate,
            }
            .apply(
                &mut cfg.reactive,
                &mut cfg.reactive_command_delay,
                &mut cfg.reactive_ramp_rate_var_per_s,
            )?;
            let inverter = SolarInverter::new(id, interval, cfg);
            if let Some(scalar) = dynamic_sunlight {
                inverter.set_sunlight_source(scalar);
            }
            let h = register_with_modes(
                &w,
                inverter,
                a.operational_mode,
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-ev-charger",
        move |_ctx: &mut TulispContext, args: Plist<EvChargerArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&r, &w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = EvChargerConfig::default();
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = checked_ramp_rate(":ramp-rate", v)?;
            }
            if let Some(v) = a.device_delay_ms {
                cfg.device_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.stream_jitter_pct {
                cfg.stream_jitter_pct = v as f32;
            }
            // The retired pack kwargs: accepted so an older file still
            // loads, ignored because the charger has no pack, and named
            // once per charger so the config gets cleaned up eventually.
            let retired: Vec<&str> = [
                (":capacity", a.capacity_wh.is_some()),
                (":initial-soc", a.initial_soc.is_some()),
                (":soc-lower", a.soc_lower.is_some()),
                (":soc-upper", a.soc_upper.is_some()),
                (":soc-protect-margin", a.soc_protect_margin.is_some()),
            ]
            .iter()
            .filter(|(_, present)| *present)
            .map(|(name, _)| *name)
            .collect();
            if !retired.is_empty() {
                let one = retired.len() == 1;
                log::warn!(
                    "make-ev-charger {id}: {} {} ignored — the pack belongs to the \
                     plugged car (`plug-ev`); drop {} from the config",
                    retired.join(", "),
                    if one { "is" } else { "are" },
                    if one { "it" } else { "them" },
                );
            }
            cfg.resume_on_recovery = a.resume_on_recovery.unwrap_or(false);
            if let Some(v) = a.phases {
                if v != 1 && v != 3 {
                    return Err(Error::invalid_argument(format!(
                        ":phases must be 1 or 3, got {v}"
                    )));
                }
                cfg.phases = v as u8;
            }
            if let Some(v) = a.idle {
                cfg.idle = v;
            }
            // The charger is registered empty: the car it charges is
            // runtime state, plugged by `(plug-ev …)` in the script
            // section, never carried by the constructor.
            let charger = EvCharger::new(id, interval, cfg);
            let h = register_with_modes(
                &w,
                charger,
                a.operational_mode,
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    let r = router.clone();
    ctx.defun(
        "%make-steam-boiler",
        move |_ctx: &mut TulispContext, args: Plist<SteamBoilerArgs>| {
            let w = r.site();
            let a = args.into_inner();
            let id = id_or_next(&r, &w, a.id)?;
            let interval = ms_to_duration(a.interval, 1000);
            let mut cfg = SteamBoilerConfig::default();
            if let Some(v) = a.rated_lower {
                cfg.rated_lower_w = v as f32;
            }
            if let Some(v) = a.rated_upper {
                cfg.rated_upper_w = v as f32;
            }
            if let Some(v) = a.target_bar {
                cfg.target_bar = v as f32;
            }
            if let Some(v) = a.max_bar {
                cfg.max_bar = v as f32;
            }
            if let Some(v) = a.initial_bar {
                cfg.initial_bar = Some(v as f32);
            }
            if let Some(v) = a.capacity_wh_per_bar {
                cfg.capacity_wh_per_bar = v as f32;
            }
            if let Some(v) = a.wh_per_kg {
                cfg.wh_per_kg = v as f32;
            }
            if let Some(v) = a.command_delay_ms {
                cfg.command_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.ramp_rate {
                cfg.ramp_rate_w_per_s = checked_ramp_rate(":ramp-rate", v)?;
            }
            if let Some(v) = a.device_delay_ms {
                cfg.device_delay = Duration::from_millis(v.max(0) as u64);
            }
            if let Some(v) = a.stream_jitter_pct {
                cfg.stream_jitter_pct = v as f32;
            }
            if !(cfg.max_bar >= cfg.target_bar && cfg.target_bar > 0.0) {
                return Err(Error::invalid_argument(
                    "%make-steam-boiler: :max-bar must be >= :target-bar > 0",
                ));
            }
            // :demand may be a number, a lambda, or a symbol. The
            // wrapper-expanded category default lands in `a.demand`
            // when no per-component value was passed; otherwise the
            // per-component value overrides via AsPlist's last-wins.
            let mut dynamic_demand: Option<DynamicScalar> = None;
            if let Some(v) = a.demand.as_ref() {
                let raw = v.as_inner();
                if raw.numberp() {
                    if let Ok(kg_h) = f64::try_from(raw) {
                        cfg.demand_kg_h = kg_h as f32;
                    }
                } else {
                    dynamic_demand = DynamicScalar::from_lisp(raw, cfg.demand_kg_h);
                    cfg.demand_dynamic = true;
                }
            }
            let boiler = SteamBoiler::new(id, interval, cfg);
            if let Some(scalar) = dynamic_demand {
                boiler.set_steam_demand_source(scalar);
            }
            let h = register_with_modes(
                &w,
                boiler,
                a.operational_mode,
                a.health,
                a.telemetry_mode,
                a.command_mode,
            )?;
            apply_initial_name(&w, id, a.name);
            Ok::<_, Error>(h)
        },
    );

    // The marker categories: no physics of their own — they
    // complete the topology and classify the meters around them;
    // power is set on the neighboring meter. One registration loop
    // because the bodies are identical up to the category.
    for (form, category) in [
        ("%make-chp", Category::Chp),
        ("%make-wind-turbine", Category::WindTurbine),
        ("%make-power-transformer", Category::PowerTransformer),
        ("%make-breaker", Category::Breaker),
    ] {
        let r = router.clone();
        ctx.defun(
            form,
            move |_ctx: &mut TulispContext, args: Plist<MarkerArgs>| {
                let w = r.site();
                let a = args.into_inner();
                let id = id_or_next(&r, &w, a.id)?;
                let jitter = a.stream_jitter_pct.unwrap_or(0.0) as f32;
                let h = register_with_modes(
                    &w,
                    Marker::new(id, category, jitter),
                    a.operational_mode,
                    a.health,
                    a.telemetry_mode,
                    a.command_mode,
                )?;
                apply_initial_name(&w, id, a.name);
                Ok::<_, Error>(h)
            },
        );
    }
}

fn connect_successors(
    site: &MicrogridSite,
    parent: u64,
    successors: &Option<Vec<ComponentHandle>>,
) {
    if let Some(list) = successors {
        for child in list {
            // Every edge — hidden or not — lands in `MicrogridSite::connections`.
            // Visibility filtering happens at the `connections()` /
            // `hidden_connections()` boundary that drives gRPC and the
            // UI; the aggregation paths walk the unfiltered graph.
            site.connect(parent, child.id());
        }
    }
}

fn ms_to_duration(ms: Option<i64>, default_ms: u64) -> Duration {
    Duration::from_millis(ms.map(|x| x.max(0) as u64).unwrap_or(default_ms))
}

/// The reactive kwargs shared by both inverter constructors,
/// gathered by field name so the two same-typed `Option<f64>` caps
/// can't be transposed at a call site.
struct ReactiveKwargs {
    pf_limit: Option<f64>,
    apparent_va: Option<f64>,
    command_delay_ms: Option<i64>,
    ramp_rate: Option<f64>,
}

impl ReactiveKwargs {
    /// Apply to a config's (capability, command-delay, ramp-rate)
    /// triple.
    ///
    /// Reactive capability semantics:
    ///   value ≤ 0.0  → that constraint is disabled
    ///   absent       → inherit the existing field on the config (the
    ///                  type's default; for the PF limit, microsim's
    ///                  0.35)
    /// `:reactive-apparent-va 50000` adds a kVA cap *without* silently
    /// dropping the inherited PF limit; previously that subtle
    /// interaction was the easy way to ship a misconfigured inverter.
    fn apply(
        self,
        reactive: &mut crate::sim::reactive::ReactiveCapability,
        command_delay: &mut Duration,
        ramp_rate: &mut f32,
    ) -> Result<(), Error> {
        let merge = |input: Option<f64>, fallback: Option<f32>| -> Option<f32> {
            match input {
                Some(v) if v > 0.0 => Some(v as f32),
                Some(_) => None,
                None => fallback,
            }
        };
        reactive.pf_limit = merge(self.pf_limit, reactive.pf_limit);
        reactive.apparent_va = merge(self.apparent_va, reactive.apparent_va);
        if let Some(v) = self.command_delay_ms {
            *command_delay = Duration::from_millis(v.max(0) as u64);
        }
        if let Some(v) = self.ramp_rate {
            *ramp_rate = checked_ramp_rate(":reactive-ramp-rate", v)?;
        }
        Ok(())
    }
}

/// A ramp rate must be a non-negative, non-NaN number: `Ramp::advance`
/// steps `actual` by `rate · dt` toward the target, so a negative rate
/// walks away from the target without bound and NaN never satisfies
/// the arrival test.
fn checked_ramp_rate(kw: &str, v: f64) -> Result<f32, Error> {
    if v.is_nan() || v < 0.0 {
        return Err(Error::invalid_argument(format!(
            "{kw} must be a non-negative rate, got {v}"
        )));
    }
    Ok(v as f32)
}

/// Resolve a car off the EV catalog from a `'symbol` kwarg — the shape
/// `(plug-ev …)` takes. `label` names the caller so the error says
/// which one complained.
pub(crate) fn preset_from_lisp(raw: &LispValue, label: &str) -> Result<&'static EvPreset, Error> {
    let name = raw.as_inner().as_symbol().map_err(|_| {
        Error::type_mismatch(format!(
            "{label}: expected a preset symbol, got {}",
            raw.as_inner()
        ))
    })?;
    preset(&name)
        .ok_or_else(|| Error::invalid_argument(format!("{label}: unknown preset '{name}'")))
}

/// Build a meter's `ReactiveSource` from its `:reactive-power` /
/// `:power-factor` / `:leading` kwargs. The two source kwargs are
/// mutually exclusive; `:leading` only makes sense alongside
/// `:power-factor`; and `:power-factor` must be a true cos φ in
/// `(0.0, 1.0]` — 0 (undefined tan) and anything above 1.0 (not a
/// valid power factor) are rejected here rather than left to produce
/// nonsense Q downstream.
fn meter_reactive_source(
    reactive_power: Option<&LispValue>,
    power_factor: Option<f64>,
    leading: Option<bool>,
) -> Result<Option<ReactiveSource>, Error> {
    if reactive_power.is_some() && power_factor.is_some() {
        return Err(Error::invalid_argument(
            "make-meter: :reactive-power and :power-factor are mutually exclusive",
        ));
    }
    if leading.is_some() && power_factor.is_none() {
        return Err(Error::invalid_argument(
            "make-meter: :leading requires :power-factor",
        ));
    }
    if let Some(pf) = power_factor {
        if !(0.0..=1.0).contains(&pf) || pf == 0.0 {
            return Err(Error::invalid_argument(format!(
                "make-meter: :power-factor must be in (0.0, 1.0], got {pf}"
            )));
        }
        return Ok(Some(ReactiveSource::PowerFactor {
            pf: pf as f32,
            leading: leading.unwrap_or(false),
        }));
    }
    Ok(reactive_power
        .and_then(|v| DynamicScalar::from_lisp(v.as_inner(), 0.0))
        .map(ReactiveSource::Var))
}

/// Resolve the component id from an `:id` plist value, falling back to
/// `MicrogridSite::next_id()` when omitted. Centralized so the
/// validation stays in one place — every make-* funnels through here.
///
/// An explicit `:id` must be positive (a negative would cast to a
/// giant u64 that registers but never matches) and must not collide
/// with a component already on this site (a collision would push a
/// second ticking component while only one stays addressable, and
/// parent meters would double-count). The auto path skips allocator
/// values an explicit `:id` already pinned for the same reason.
///
/// Component ids are enterprise-unique, so an explicit `:id` is also
/// checked against every OTHER registered microgrid — a duplicate
/// across microgrids makes gRPC lookups and the cross-microgrid UI
/// views ambiguous. The auto path needs no such check: every site
/// draws from the one enterprise-wide allocator.
fn id_or_next(
    router: &crate::sim::microgrids::SharedSiteRouter,
    site: &MicrogridSite,
    explicit: Option<i64>,
) -> Result<u64, Error> {
    match explicit {
        Some(x) => {
            if x <= 0 {
                return Err(Error::invalid_argument(format!(
                    ":id must be positive, got {x}"
                )));
            }
            let id = x as u64;
            if site.get(id).is_some() {
                return Err(Error::invalid_argument(format!(
                    "component id {id} is already registered"
                )));
            }
            // This site doesn't have the id (checked just above), so
            // any owner the registry reports is a foreign microgrid.
            if let Some(mg) = router.owner_of_component(id) {
                return Err(Error::invalid_argument(format!(
                    "component id {id} is already registered in microgrid {mg}"
                )));
            }
            site.reserve_id(id);
            Ok(id)
        }
        None => loop {
            let id = site.next_id();
            if site.get(id).is_none() {
                return Ok(id);
            }
        },
    }
}

/// Register a freshly-built component, then apply any initial runtime
/// mode args. Returns the handle so the caller can also wire
/// connections / `setq` it for cross-references.
///
/// Centralising this guarantees every make-* applies modes in the
/// same order (health, then telemetry, then command) right after
/// registration — before any tick or subscriber runs.
fn register_with_modes<C: crate::sim::SimulatedComponent + 'static>(
    site: &MicrogridSite,
    component: C,
    operational_mode: Option<OperationalMode>,
    health: Option<Health>,
    telemetry: Option<TelemetryMode>,
    command: Option<CommandMode>,
) -> Result<ComponentHandle, Error> {
    let id = component.id();
    let h = site.register(component);
    // The operational mode (config) goes first: it derives the
    // runtime-knob defaults, which the explicit knob kwargs then
    // refine — and are validated against. A mode error after the
    // register above must unregister again: make-* is atomic to its
    // callers, and without the rollback a rejected form (say
    // :operational-mode 'inactive :telemetry-mode 'normal) would
    // leave a half-configured ghost that ticks physics and collides
    // with the retried :id.
    let modes = (|| {
        if let Some(m) = operational_mode {
            site.set_operational_mode(id, m)
                .map_err(Error::invalid_argument)?;
        }
        apply_initial_modes(site, id, health, telemetry, command)
    })();
    if let Err(e) = modes {
        site.remove_component(id);
        return Err(e);
    }
    Ok(h)
}

/// Apply a `:name "…"` plist arg after registration. Stores it as a
/// display-name override so the gRPC `ListElectricalComponents`
/// response and the UI's topology endpoint both pick it up. No-op
/// when the user didn't pass `:name`.
fn apply_initial_name(site: &MicrogridSite, id: u64, name: Option<String>) {
    if let Some(n) = name {
        site.rename(id, n);
    }
}

/// Apply initial runtime mode args from a plist constructor. Each
/// `make-*` calls this immediately after `site.register(...)` so a
/// component declared with `:health 'error` is broken from the very
/// first tick. Symbol → enum parsing happens in the `TryFrom` impls
/// (`src/lisp/runtime_modes.rs`); by the time we get here the values
/// are typed.
fn apply_initial_modes(
    site: &MicrogridSite,
    id: u64,
    health: Option<Health>,
    telemetry: Option<TelemetryMode>,
    command: Option<CommandMode>,
) -> Result<(), Error> {
    if let Some(h) = health {
        site.set_health(id, h).map_err(Error::invalid_argument)?;
    }
    // The checked setters reject a knob the component's operational
    // mode forbids (e.g. `:telemetry-mode 'normal` on an inactive
    // component) — that combination is a config error, not a poke.
    if let Some(t) = telemetry {
        site.set_telemetry_mode(id, t)
            .map_err(Error::invalid_argument)?;
    }
    // An explicit `:command-mode` refines the default the health coupling
    // just applied — but it must not re-enable a device declared errored.
    // `set_health(Error)` couples the command mode to Error (error ⇒ no
    // commands accepted); letting `:command-mode 'normal` override that
    // would yield a broken-but-accepting component the runtime path can
    // never produce. Health wins for an errored device.
    if let Some(c) = command
        && health != Some(Health::Error)
    {
        site.set_command_mode(id, c)
            .map_err(Error::invalid_argument)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a context wired to a fresh MicrogridSite, evaluates `src`, and
    /// returns the MicrogridSite so the test can introspect what got registered.
    fn run(src: &str) -> MicrogridSite {
        run_with_ctx(src).0
    }

    /// Like [`run`] but also surfaces the context — needed for tests
    /// that drive `refresh_inputs` (lambda / symbol `:power` etc.)
    /// after the components have registered.
    fn run_with_ctx(src: &str) -> (MicrogridSite, TulispContext) {
        use crate::sim::microgrids::{SiteRouter, new_current_microgrid, new_registry};
        let site = MicrogridSite::new();
        let mut ctx = TulispContext::new();
        crate::lisp::handle::register(&mut ctx);
        let router = SiteRouter::new(new_registry(), new_current_microgrid(), site.clone());
        register(&mut ctx, router);
        // Load the `make-*` wrappers + `*-defaults` plists so tests
        // exercise the same wrapper → primitive path config.lisp uses.
        ctx.eval_string(include_str!("../../sim/defaults.lisp"))
            .expect("defaults.lisp");
        ctx.eval_string(src).expect("eval lisp source");
        (site, ctx)
    }

    /// `:name "..."` on any %make-* lands as a display-name override
    /// — same path as `(rename-component …)` — so the gRPC
    /// listing and the UI's topology endpoint both pick it up.
    /// Omitting `:name` falls through to the component's
    /// auto-generated default (`category-id`).
    /// A make-* whose mode kwargs are rejected must leave nothing
    /// behind: the component registered before the mode check runs,
    /// so the error path unregisters it again. Without the rollback
    /// the failed form left a ghost that ticks physics and collides
    /// with the corrected form's :id.
    #[test]
    fn rejected_modes_unregister_the_component() {
        use crate::sim::microgrids::{SiteRouter, new_current_microgrid, new_registry};
        let site = MicrogridSite::new();
        let mut ctx = TulispContext::new();
        crate::lisp::handle::register(&mut ctx);
        let router = SiteRouter::new(new_registry(), new_current_microgrid(), site.clone());
        register(&mut ctx, router);
        // Inactive provides no telemetry, so :telemetry-mode 'normal
        // is rejected — after the component was registered.
        let err = ctx
            .eval_string("(%make-meter :id 7 :operational-mode 'inactive :telemetry-mode 'normal)");
        assert!(err.is_err(), "conflicting modes should error");
        assert!(site.get(7).is_none(), "the failed make must roll back");
        // The corrected form reuses the id without a collision.
        ctx.eval_string("(%make-meter :id 7)").expect("retry");
        assert!(site.get(7).is_some());
    }

    #[test]
    fn name_arg_sets_display_name() {
        let site = run(r#"(%make-battery :id 200 :name "main-batt")"#);
        assert_eq!(site.display_name(200).as_deref(), Some("main-batt"));
        let site = run(r#"(%make-battery :id 201)"#);
        assert_eq!(site.display_name(201).as_deref(), Some("bat-201"));
    }

    #[test]
    fn primitive_plist_args_set_fields() {
        // %make-battery is the primitive — every field arrives as a
        // plist key. Defaults are applied by wrappers, not here.
        let site = run(
            r#"(%make-battery :id 100 :capacity 50000.0 :initial-soc 20.0
                              :rated-lower -8000.0 :rated-upper 8000.0)"#,
        );
        let t = site.get(100).unwrap().telemetry(&site);
        assert_eq!(t.capacity_wh, Some(50_000.0));
        assert!((t.soc_pct.unwrap() - 20.0).abs() < 1e-3);
    }

    #[test]
    fn wrapper_merges_category_defaults() {
        // `make-battery` (wrapper) prepends `battery-defaults` (plist
        // literal in sim/defaults.lisp) to the caller's args. AsPlist's
        // last-occurrence-wins resolution lets the per-component plist
        // override individual default fields while inheriting the rest.
        let site = run(r#"(make-battery :id 200 :capacity 50000.0)"#);
        let t = site.get(200).unwrap().telemetry(&site);
        // From per-component plist:
        assert_eq!(t.capacity_wh, Some(50_000.0));
        // From battery-defaults — :health ok carried through.
        assert_eq!(site.runtime_of(200).health, crate::sim::runtime::Health::Ok);
    }

    #[test]
    fn primitive_skips_wrapper_defaults() {
        // Calling %make-battery directly bypasses the wrapper's
        // default-prepending, so the BatteryConfig::default values
        // stand for every unset field.
        let site = run("(%make-battery :id 103)");
        let t = site.get(103).unwrap().telemetry(&site);
        assert_eq!(t.capacity_wh, Some(92_000.0));
    }

    #[test]
    fn defaults_accept_bare_symbol_for_health() {
        // Health is an enum carried as a bare symbol through the plist;
        // verify the wrapper-applied default (battery-defaults sets
        // :health ok) lands.
        let site = run("(make-battery :id 102)");
        assert_eq!(site.runtime_of(102).health, crate::sim::runtime::Health::Ok);
    }

    /// `:health 'error` couples command mode to Error (error ⇒ no commands
    /// accepted). An explicit `:command-mode` must not re-enable an errored
    /// device — health wins. A non-errored device still honours its mode.
    #[test]
    fn errored_health_overrides_explicit_command_mode() {
        use crate::sim::runtime::{CommandMode, Health};
        let site = run(
            r#"(%make-battery :id 300 :health 'error :command-mode 'normal)
               (%make-battery :id 301 :health 'ok :command-mode 'timeout)"#,
        );
        let errored = site.runtime_of(300);
        assert_eq!(errored.health, Health::Error);
        assert_eq!(errored.command, CommandMode::Error);

        let ok = site.runtime_of(301);
        assert_eq!(ok.health, Health::Ok);
        assert_eq!(ok.command, CommandMode::Timeout);
    }

    /// `:power N` lands as a constant DynamicScalar — aggregate_power_w
    /// reads it through immediately, no refresh required.
    #[test]
    fn meter_power_constant_reads_through() {
        let site = run("(%make-meter :id 7 :power 1875.0)");
        let m = site.get(7).unwrap();
        assert!((m.aggregate_power_w(&site) - 1875.0).abs() < 1e-3);
    }

    /// `:power (lambda () N)` produces a dynamic source that the
    /// scheduler-driven refresh path resolves on each pass. The
    /// fallback (0.0) is what aggregate_power_w sees before the
    /// first refresh; after one refresh it matches the lambda's
    /// return.
    #[test]
    fn meter_power_lambda_resolves_each_refresh() {
        let (site, mut ctx) = run_with_ctx(r#"(%make-meter :id 8 :power (lambda () 1234.5))"#);
        let m = site.get(8).unwrap();
        // Pre-refresh: cached fallback.
        assert_eq!(m.aggregate_power_w(&site), 0.0);
        // After refresh_inputs: the lambda's value is cached.
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 1234.5).abs() < 1e-3);
    }

    /// `:power 'symbol` derefs the variable each refresh — mutating
    /// the bound value between refreshes is what scenarios use to
    /// drive consumer load curves declaratively.
    #[test]
    fn meter_power_symbol_derefs_each_refresh() {
        let (site, mut ctx) = run_with_ctx(
            r#"(setq consumer-power 1500.0)
               (%make-meter :id 9 :power 'consumer-power)"#,
        );
        let m = site.get(9).unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 1500.0).abs() < 1e-3);
        // Mutate the symbol; next refresh picks up the new value.
        ctx.eval_string("(setq consumer-power 2750.0)").unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 2750.0).abs() < 1e-3);
    }

    /// `:reactive-power` / `:power-factor` / `:leading` validation:
    /// the two source kwargs are mutually exclusive, `:leading`
    /// requires `:power-factor`, and `:power-factor` must land in
    /// `(0.0, 1.0]` — 0 and > 1 are rejected, 1.0 is accepted and
    /// yields Q = 0 (unity power factor, cos φ = 1 → φ = 0).
    #[test]
    fn meter_reactive_kwargs_validate() {
        let mut ctx = TulispContext::new();
        use crate::sim::microgrids::{SiteRouter, new_current_microgrid, new_registry};
        let site = MicrogridSite::new();
        crate::lisp::handle::register(&mut ctx);
        let router = SiteRouter::new(new_registry(), new_current_microgrid(), site.clone());
        register(&mut ctx, router);

        let err = ctx
            .eval_string("(%make-meter :id 1 :reactive-power 500.0 :power-factor 0.9)")
            .unwrap_err();
        assert!(format!("{err}").contains("mutually exclusive"), "{err}");

        let err = ctx
            .eval_string("(%make-meter :id 2 :leading t)")
            .unwrap_err();
        assert!(format!("{err}").contains(":leading"), "{err}");

        let err = ctx
            .eval_string("(%make-meter :id 3 :power-factor 0.0)")
            .unwrap_err();
        assert!(format!("{err}").contains(":power-factor"), "{err}");

        let err = ctx
            .eval_string("(%make-meter :id 4 :power-factor 1.5)")
            .unwrap_err();
        assert!(format!("{err}").contains(":power-factor"), "{err}");

        // 1.0 is a valid (unity) power factor — Q collapses to 0.
        ctx.eval_string("(%make-meter :id 5 :power 1000.0 :power-factor 1.0)")
            .expect("pf 1.0 is valid");
        let m = site.get(5).unwrap();
        assert!((m.aggregate_reactive_var(&site)).abs() < 1e-3);
    }

    /// `:sunlight%` accepts a lambda the same way meter `:power`
    /// does — the make-path detects the non-numeric value and wires
    /// it into the inverter's DynamicScalar. Refresh resolves it
    /// each tick; the resolved sunlight% is the floor for incoming
    /// setpoints, observable as a clip on the ramp output.
    #[test]
    fn solar_inverter_sunlight_lambda_clips_setpoint() {
        let (site, mut ctx) = run_with_ctx(
            r#"(%make-solar-inverter :id 11
                                    :sunlight% (lambda () 25.0)
                                    :rated-lower -8000.0
                                    :rated-upper 0.0)"#,
        );
        let inv = site.get(11).unwrap();
        // Refresh runs the lambda → sunlight_pct = 25 →
        // min_avail = -2000 W.
        inv.refresh_inputs(&mut ctx);
        // Issue a setpoint below min_avail; CommandDelay default is
        // zero so the next tick promotes it. Ramp default is
        // infinity so the actual jumps straight to the target,
        // floored at min_avail.
        site.gateway()
            .command(11, crate::timeout_tracker::SetpointAxis::Active, -5000.0)
            .expect("setpoint within rated");
        site.tick_n(3, Duration::from_millis(100));
        let p = inv
            .telemetry(&site)
            .active_power_w
            .expect("active power present");
        assert!(
            (p - (-2000.0)).abs() < 1.0,
            "expected sunlight-clipped -2000 W, got {p}"
        );
    }

    /// `:array-peak-w` sizes the DC array separately from `:rated-lower`'s
    /// AC clamp: a 15 kW array on a 10 kW inverter at 50% sun clamps
    /// production to 7.5 kW, not 5 kW. Omitting `:array-peak-w` leaves
    /// behavior identical to a matched array — the lambda test above
    /// stays green with no `:array-peak-w` at all.
    #[test]
    fn solar_inverter_array_peak_w_sizes_the_dc_array_separately_from_rated() {
        let (site, mut ctx) = run_with_ctx(
            r#"(%make-solar-inverter :id 12
                                    :sunlight% 50.0
                                    :rated-lower -10000.0
                                    :array-peak-w 15000.0)"#,
        );
        let inv = site.get(12).unwrap();
        inv.refresh_inputs(&mut ctx);
        // Demand the full AC rating; ramp is infinite by default so
        // the actual jumps straight to whatever the array/sun clamp
        // allows.
        site.gateway()
            .command(12, crate::timeout_tracker::SetpointAxis::Active, -10_000.0)
            .expect("setpoint within rated");
        site.tick_n(3, Duration::from_millis(100));
        let p = inv
            .telemetry(&site)
            .active_power_w
            .expect("active power present");
        assert!(
            (p - (-7_500.0)).abs() < 1.0,
            "expected array-clamped -7500 W, got {p}"
        );
    }

    /// A non-finite or non-positive `:array-peak-w` inverts the DC band —
    /// `min_avail_w` goes positive, intersects emptily with rated, and
    /// parks the inverter at 0 forever with nothing in telemetry to
    /// say why. Rejected at the door instead, naming the kwarg.
    #[test]
    fn array_peak_w_rejects_non_positive_values() {
        let (_s, mut ctx) = run_with_ctx("");
        let err = ctx
            .eval_string("(%make-solar-inverter :id 30 :rated-lower -10000.0 :array-peak-w -5000)")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":array-peak-w"), "{err}");
        let err = ctx
            .eval_string("(%make-solar-inverter :id 31 :rated-lower -10000.0 :array-peak-w 0)")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":array-peak-w"), "{err}");
    }

    /// Omitting `:sunlight%` is the opt-in to weather: the inverter
    /// starts as a `Follow` source (the "weather" marker in
    /// `sunlight_reading().expr`) and renders back without the kwarg.
    /// Passing `:sunlight%` — a number or a lambda — keeps the
    /// historical `Manual` slot.
    #[test]
    fn omitting_sunlight_pct_follows_the_site_weather() {
        let site = run(r#"(%make-solar-inverter :id 13 :rated-lower -10000.0)
               (%make-solar-inverter :id 14 :rated-lower -10000.0 :sunlight% 40.0)"#);
        assert_eq!(
            site.get(13)
                .unwrap()
                .sunlight_drive()
                .unwrap()
                .sunlight_reading()
                .expr,
            Some("weather".into()),
            "no :sunlight% ⇒ follows the weather"
        );
        assert_eq!(
            site.get(14)
                .unwrap()
                .sunlight_drive()
                .unwrap()
                .sunlight_reading()
                .expr,
            None,
            "an explicit :sunlight% stays manual"
        );
        let kw = |id: u64| {
            site.get(id)
                .unwrap()
                .constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(!kw(13).contains(":sunlight%"), "{}", kw(13));
        assert!(kw(14).contains(":sunlight% 40"), "{}", kw(14));
    }

    /// `:weather-lag-s` / `:weather-jitter-pct` shape how a `Follow`
    /// source samples the sky. Both are validated at the make door
    /// (a negative lag and an out-of-band jitter are config bugs, not
    /// something to clamp silently), and both round-trip through
    /// `constructor_kwargs` — the lag whenever it was given
    /// explicitly, zero included (zero is the opt-out from the
    /// id-derived default sweep, so dropping it would silently
    /// re-lag the inverter on reload); the jitter only when nonzero.
    /// An inverter that gave neither renders with no extra noise.
    #[test]
    fn solar_weather_lag_and_jitter_validate_and_round_trip() {
        let site = run(r#"(%make-solar-inverter :id 15
                                    :rated-lower -10000.0
                                    :weather-lag-s 90
                                    :weather-jitter-pct 5.0)
               (%make-solar-inverter :id 16 :rated-lower -10000.0)
               (%make-solar-inverter :id 17 :rated-lower -10000.0
                                    :weather-lag-s 0)"#);
        let kw = |id: u64| {
            site.get(id)
                .unwrap()
                .constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kw(15).contains(":weather-lag-s 90"), "{}", kw(15));
        assert!(kw(15).contains(":weather-jitter-pct 5"), "{}", kw(15));
        assert!(
            !kw(16).contains(":weather-"),
            "an unset lag renders nothing (the id-derived default re-derives on reload): {}",
            kw(16)
        );
        assert!(
            kw(17).contains(":weather-lag-s 0"),
            "an explicit zero is the sweep opt-out and must round-trip: {}",
            kw(17)
        );

        // Round-trip: re-making from the rendered kwargs reproduces
        // both values.
        let (site2, _ctx) = run_with_ctx(&format!("(%make-solar-inverter :id 15 {})", kw(15)));
        let kw2 = site2
            .get(15)
            .unwrap()
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(kw2, kw(15));

        let (_s, mut ctx) = run_with_ctx("");
        let err = ctx
            .eval_string("(%make-solar-inverter :id 17 :weather-lag-s -1)")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":weather-lag-s"), "{err}");
        let err = ctx
            .eval_string("(%make-solar-inverter :id 18 :weather-jitter-pct 100)")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":weather-jitter-pct"), "{err}");
        let err = ctx
            .eval_string("(%make-solar-inverter :id 19 :weather-jitter-pct -0.5)")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":weather-jitter-pct"), "{err}");
        // A lag that overflows Duration must ERROR, not panic the
        // config load — `Duration::from_secs_f64` would abort here.
        let err = ctx
            .eval_string("(%make-solar-inverter :id 20 :weather-lag-s 1e300)")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":weather-lag-s"), "{err}");
        // And a merely LARGE lag — well inside `Duration`, but past
        // the hour of cloud history `Weather` retains — is rejected
        // too, saying why: a sample that lands in pruned history
        // reads clear sky forever instead of failing loudly.
        let err = ctx
            .eval_string("(%make-solar-inverter :id 21 :weather-lag-s 7200)")
            .unwrap_err()
            .to_string();
        assert!(err.contains(":weather-lag-s"), "{err}");
        assert!(err.contains("3600"), "the bound is named: {err}");
        assert!(err.contains("pruned"), "…and so is the reason: {err}");
    }

    /// `(set-meter-power id W)` is the existing imperative setter
    /// for driving curves from `(every)` callbacks. It collapses the
    /// source back to a constant — even if the meter was originally
    /// constructed with a lambda — so subsequent refreshes don't
    /// silently overwrite the user's intent.
    #[test]
    fn meter_set_power_collapses_to_constant() {
        let (site, mut ctx) = run_with_ctx(r#"(%make-meter :id 10 :power (lambda () 1000.0))"#);
        let m = site.get(10).unwrap();
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 1000.0).abs() < 1e-3);

        // External setter wins; refresh becomes a no-op on the
        // collapsed constant.
        m.meter_drive().unwrap().set_active_power_override(7777.0);
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&site) - 7777.0).abs() < 1e-3);
    }

    /// An explicit `:id` colliding with a registered component is a
    /// config error — pre-validation it double-registered: two ticking
    /// components, one addressable, double-counted by parent meters.
    #[test]
    fn duplicate_explicit_id_rejects() {
        let (site, mut ctx) = run_with_ctx("(%make-battery :id 100)");
        let res = ctx.eval_string("(%make-meter :id 100)");
        assert!(res.is_err(), "expected duplicate-id error, got {res:?}");
        assert_eq!(site.components().len(), 1, "no half-registration");
    }

    /// `:id 0` / negative ids are rejected — a negative cast to u64
    /// would register under a giant id that nothing ever matches.
    #[test]
    fn non_positive_explicit_id_rejects() {
        let (site, mut ctx) = run_with_ctx("(%make-battery :id 7)");
        for bad in ["(%make-meter :id 0)", "(%make-meter :id -1)"] {
            let res = ctx.eval_string(bad);
            assert!(res.is_err(), "expected error from {bad}, got {res:?}");
        }
        assert_eq!(site.components().len(), 1);
    }

    /// Auto-allocation skips over a value an explicit `:id` pinned
    /// before the allocator reached it.
    #[test]
    fn auto_id_skips_a_pinned_value() {
        let first = crate::sim::component::FIRST_AUTO_ID;
        let (site, mut ctx) = run_with_ctx(&format!("(%make-battery :id {first})"));
        ctx.eval_string("(%make-meter)").unwrap();
        assert_eq!(site.components().len(), 2);
        let auto_id = site
            .components()
            .iter()
            .map(|c| c.id())
            .find(|id| *id != first)
            .expect("the meter got its own id");
        assert!(
            auto_id > first,
            "skipped past the pinned {first}, got {auto_id}"
        );
    }

    /// `:operational-mode` is stored as config and derives the
    /// runtime knobs; a knob kwarg the mode forbids is a config
    /// error.
    #[test]
    fn operational_mode_kwarg_derives_and_validates() {
        use crate::sim::runtime::TelemetryMode;

        let site = run("(%make-meter :id 7 :operational-mode 'inactive)");
        assert_eq!(site.operational_mode(7), OperationalMode::Inactive);
        assert_eq!(site.runtime_of(7).telemetry, TelemetryMode::Silent);

        let (_, mut ctx) = run_with_ctx("");
        let err = ctx
            .eval_string("(%make-meter :id 8 :operational-mode 'inactive :telemetry-mode 'normal)")
            .unwrap_err();
        assert!(format!("{err:?}").contains("streams no telemetry"));
    }

    /// The UI wraps a palette button's `data-make` in parens and posts
    /// it to the eval endpoint, so a typo in the attribute only shows
    /// up on click.
    #[test]
    fn every_palette_form_makes_a_component() {
        let html = include_str!("../../ui-assets/index.html");
        let (_, rest) = html
            .split_once("<div id=\"palette\">")
            .expect("index.html has the palette");
        let (palette, _) = rest
            .split_once("</div>")
            .expect("the palette div is closed");
        assert!(
            !palette.contains("<div"),
            "a nested div truncates the scrape"
        );
        let forms: Vec<&str> = palette
            .split("data-make=\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        let (site, mut ctx) = run_with_ctx("");
        for form in &forms {
            ctx.eval_string(&format!("({form})"))
                .unwrap_or_else(|e| panic!("{form}: {}", e.format(&ctx)));
        }
        for name in crate::lisp::microgrid_file::COMPONENT_MAKE_FNS {
            let head = format!("make-{name}");
            assert!(
                forms.iter().any(|f| f.split(' ').next().unwrap() == head),
                "no palette button for {head}"
            );
        }
        let made = site.components();
        assert_eq!(made.len(), forms.len());
        let hidden = made.iter().filter(|c| c.is_hidden()).count();
        assert_eq!(hidden, 1, "the hidden-meter button alone sets :hidden");
    }

    /// The boiler constructor takes physics kwargs, keeps every marker
    /// kwarg valid (bare :id forms in existing worlds must load), and a
    /// dynamic :demand installs a source instead of a constant.
    #[test]
    fn make_steam_boiler_kwargs_and_marker_compat() {
        let site = run(r#"(%make-steam-boiler :id 41 :rated-upper 100000.0
                                  :target-bar 6.0 :demand 40.0
                                  :operational-mode 'control-only)"#);
        let b = site.get(41).unwrap();
        assert_eq!(b.category().as_str(), "steam-boiler");
        assert_eq!(b.rated_active_bounds(), Some((0.0, 100_000.0)));
        assert_eq!(b.pressure_target_bar(), Some(6.0));

        // Bare marker-style form — existing worlds' `(%make-steam-boiler
        // :id 1642)` must keep loading now that SteamBoilerArgs is a
        // strict superset of MarkerArgs.
        let site = run("(%make-steam-boiler :id 42)");
        assert!(site.get(42).is_some());

        let site = run("(%make-steam-boiler :id 43 :demand (lambda () 25.0))");
        let b = site.get(43).unwrap();
        assert!(b.has_unrenderable_source());
    }

    /// `:phases` and `:idle` shape the charger's config and each
    /// rejects a value it can't mean; a pack kwarg the config no
    /// longer owns is taken, warned about and ignored, so a file the
    /// previous binary wrote loads; and the car itself is not a
    /// constructor kwarg at all — `:ev` is an unknown key.
    #[test]
    fn make_ev_charger_ignores_the_pack_kwargs_and_rejects_bad_phases_and_idle() {
        use super::super::test_support::config_with;
        let (cfg, _dir) = config_with("nil");
        assert!(
            cfg.eval("(%make-ev-charger :id 9 :phases 2)").is_err(),
            "a charger has 1 or 3 phases"
        );
        assert!(
            cfg.eval("(%make-ev-charger :id 10 :idle 'sometimes)")
                .is_err(),
            "idle is paused or full"
        );
        cfg.eval("(%make-ev-charger :id 11 :capacity 30000)")
            .expect("a retired pack kwarg is ignored, not a load failure");
        let charger = cfg.site().get(11).expect("the charger was still built");
        assert!(
            charger.ev_info().is_none(),
            ":capacity buys no pack — the charger is empty"
        );

        // The car is runtime state: a charger takes no preset at
        // construction, so `:ev` is not a key the primitive knows.
        assert!(
            cfg.eval("(%make-ev-charger :id 12 :ev 'sedan)").is_err(),
            "the plugged car is not a constructor kwarg"
        );
    }

    /// `:device-delay-ms` reaches each controllable component and is
    /// written back only when it is not the 100 ms default, so
    /// existing managed files are not rewritten.
    #[test]
    fn device_delay_ms_round_trips_and_the_default_renders_nothing() {
        let (site, _ctx) = run_with_ctx(
            r#"(%make-battery-inverter :id 21 :device-delay-ms 250)
               (%make-solar-inverter :id 22 :device-delay-ms 0)
               (%make-ev-charger :id 23 :device-delay-ms 40)
               (%make-steam-boiler :id 24 :device-delay-ms 300)
               (%make-battery-inverter :id 25)
               (%make-ev-charger :id 26 :device-delay-ms 100)"#,
        );
        let kw = |id: u64| {
            site.get(id)
                .unwrap()
                .constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kw(21).contains(":device-delay-ms 250"), "{}", kw(21));
        assert!(kw(22).contains(":device-delay-ms 0"), "{}", kw(22));
        assert!(kw(23).contains(":device-delay-ms 40"), "{}", kw(23));
        assert!(kw(24).contains(":device-delay-ms 300"), "{}", kw(24));
        assert!(!kw(25).contains(":device-delay-ms"), "{}", kw(25));
        assert!(!kw(26).contains(":device-delay-ms"), "{}", kw(26));
    }
}
