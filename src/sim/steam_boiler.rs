//! Steam boiler — a hybrid gas/electric load. Electricity displaces
//! (unmodelled) gas: the electric draw is the gateway's command, held
//! by the heater inside a per-tick band [0, need_w]; the implied gas
//! burner holds pressure at the thermostat target, so pressure lives
//! in [target, max]: above only via set-pressure / :initial-bar,
//! decaying back at the steam-demand rate.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};

use crate::sim::{
    Category, MicrogridSite, SimulatedComponent, Telemetry,
    bounds::VecBounds,
    component::{GatewaySettings, KnobKind, KnobSnapshot, ScalarReading},
    device_axis::DeviceAxis,
    dynamic_scalar::DynamicScalar,
    runtime::Health,
};
use crate::timeout_tracker::SetpointAxis;

#[derive(Clone, Debug)]
pub struct SteamBoilerConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub target_bar: f32,
    pub max_bar: f32,
    /// None = start at target.
    pub initial_bar: Option<f32>,
    pub capacity_wh_per_bar: f32,
    pub wh_per_kg: f32,
    /// Seed for the demand source when it is a plain number.
    pub demand_kg_h: f32,
    /// True when :demand was a lambda/symbol at construction — the
    /// kwarg renderer omits :demand then (unrenderable source).
    pub demand_dynamic: bool,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
    /// Time a command takes to reach the heater once the boiler has
    /// it; default 100 ms.
    pub device_delay: Duration,
}

impl Default for SteamBoilerConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: 0.0,
            rated_upper_w: 250_000.0,
            target_bar: 8.0,
            max_bar: 10.0,
            initial_bar: None,
            capacity_wh_per_bar: 10_000.0,
            wh_per_kg: 627.0,
            demand_kg_h: 0.0,
            demand_dynamic: false,
            command_delay: Duration::from_millis(500),
            ramp_rate_w_per_s: f32::INFINITY,
            stream_jitter_pct: 0.0,
            device_delay: crate::sim::inverter::DEFAULT_DEVICE_DELAY,
        }
    }
}

pub struct SteamBoiler {
    id: u64,
    name: String,
    interval: Duration,
    cfg: SteamBoilerConfig,
    state: Mutex<BoilerState>,
    /// Steam-demand kg/h. Either a constant (the cfg default or a
    /// numeric `:demand`) or a Lisp expression re-resolved each tick
    /// by `refresh_inputs`.
    demand_source: RwLock<DynamicScalar>,
    /// The electric heater: the command handed in through
    /// `set_command`, delayed and held inside rated ∩ [0, need].
    heater: DeviceAxis,
}

#[derive(Debug, Clone)]
struct BoilerState {
    pressure_bar: f32,
}

impl SteamBoiler {
    /// Bring a config into the range `tick` can run on: a pressure
    /// window of positive normal numbers with max at or above target,
    /// a finite initial pressure, and positive normal constants for
    /// the two factors tick divides and multiplies by. The Lisp door
    /// rejects most of these configs with an error; a direct caller
    /// gets a running boiler and a warning per correction instead.
    fn guard(mut cfg: SteamBoilerConfig, id: u64) -> SteamBoilerConfig {
        if !cfg.target_bar.is_finite() || cfg.target_bar < f32::MIN_POSITIVE {
            let fallback = SteamBoilerConfig::default().target_bar;
            log::warn!(
                "steam-boiler {id}: target_bar {} is not a positive normal number; using {fallback}",
                cfg.target_bar
            );
            cfg.target_bar = fallback;
        }
        if !cfg.max_bar.is_finite() || cfg.max_bar < cfg.target_bar {
            log::warn!(
                "steam-boiler {id}: max_bar {} is not a finite value at or above target_bar {}; using target_bar",
                cfg.max_bar,
                cfg.target_bar
            );
            cfg.max_bar = cfg.target_bar;
        }
        if let Some(bar) = cfg.initial_bar
            && !bar.is_finite()
        {
            log::warn!(
                "steam-boiler {id}: initial_bar {bar} is not finite; starting at target_bar"
            );
            cfg.initial_bar = None;
        }
        let defaults = SteamBoilerConfig::default();
        for (name, value, fallback) in [
            (
                "capacity_wh_per_bar",
                &mut cfg.capacity_wh_per_bar,
                defaults.capacity_wh_per_bar,
            ),
            ("wh_per_kg", &mut cfg.wh_per_kg, defaults.wh_per_kg),
        ] {
            if !value.is_finite() || *value < f32::MIN_POSITIVE {
                log::warn!(
                    "steam-boiler {id}: {name} {value} is not a positive normal number; using {fallback}"
                );
                *value = fallback;
            }
        }
        cfg
    }

    pub fn new(id: u64, interval: Duration, cfg: SteamBoilerConfig) -> Self {
        let cfg = Self::guard(cfg, id);
        let init_bar = cfg
            .initial_bar
            .unwrap_or(cfg.target_bar)
            .clamp(f32::MIN_POSITIVE, cfg.max_bar);
        let heater = DeviceAxis::new(cfg.device_delay, 0.0);
        let demand_kg_h = cfg.demand_kg_h;
        Self {
            id,
            name: format!("steam-boiler-{id}"),
            interval,
            cfg,
            state: Mutex::new(BoilerState {
                pressure_bar: init_bar,
            }),
            demand_source: RwLock::new(DynamicScalar::constant(demand_kg_h)),
            heater,
        }
    }

    /// Replace the steam-demand source with a Lisp expression that
    /// `refresh_inputs` re-resolves each tick. Mirrors
    /// `SolarInverter::set_sunlight_source`.
    pub fn set_steam_demand_source(&self, scalar: DynamicScalar) {
        *self.demand_source.write() = scalar;
    }

    /// Steam demand in W (kg/h × Wh/kg); negative or non-finite reads
    /// as 0.
    fn demand_w(&self) -> f32 {
        let raw = self.demand_source.read().get();
        let kg_h = if raw.is_finite() { raw.max(0.0) } else { 0.0 };
        kg_h * self.cfg.wh_per_kg
    }

    /// The electricity the boiler can take over a tick of `dt_s`
    /// seconds: none above target; at or below it, the demand plus
    /// whatever closes the pressure gap this tick, capped at the
    /// rating.
    fn need_w(&self, pressure: f32, demand_w: f32, dt_s: f32) -> f32 {
        if pressure > self.cfg.target_bar {
            return 0.0;
        }
        let recovery_w =
            (self.cfg.target_bar - pressure) * self.cfg.capacity_wh_per_bar * 3600.0 / dt_s;
        (demand_w + recovery_w).min(self.cfg.rated_upper_w)
    }

    /// `[0, need]` for a tick of `dt`, from the current pressure and
    /// demand.
    fn heat_band(&self, dt: Duration) -> VecBounds {
        let dt_s = dt.as_secs_f32();
        let need = if dt_s > 0.0 {
            let pressure = self.state.lock().pressure_bar;
            self.need_w(pressure, self.demand_w(), dt_s)
        } else {
            0.0
        };
        VecBounds::single(0.0, need)
    }
}

impl fmt::Display for SteamBoiler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for SteamBoiler {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::SteamBoiler
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }
    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn refresh_inputs(&self, ctx: &mut tulisp::TulispContext) {
        self.demand_source.read().refresh(ctx);
    }

    fn tick(&self, world: &MicrogridSite, now: DateTime<Utc>, dt: Duration) {
        let dt_s = dt.as_secs_f32();
        if dt_s <= 0.0 {
            return;
        }
        let demand_w = self.demand_w();
        let pressure = self.state.lock().pressure_bar;
        let need_w = self.need_w(pressure, demand_w, dt_s);

        // A faulted or standby heater is offline: no draw, nothing
        // left in the delay line. The steam side keeps going — the
        // gas burner still holds pressure and an above-target excess
        // still decays.
        let p = if world.runtime_of(self.id).health != Health::Ok {
            self.heater.trip();
            0.0
        } else {
            let band = VecBounds::single(0.0, need_w).intersect(&VecBounds::single(
                self.cfg.rated_lower_w,
                self.cfg.rated_upper_w,
            ));
            self.heater.tick(now, Some(&band))
        };

        // Integrate, then let the implied gas burner floor the result
        // at target (the ceiling guards overshoot).
        let mut s = self.state.lock();
        s.pressure_bar += (p - demand_w) * dt_s / 3600.0 / self.cfg.capacity_wh_per_bar;
        s.pressure_bar = s.pressure_bar.clamp(self.cfg.target_bar, self.cfg.max_bar);
    }

    fn telemetry(&self, site: &MicrogridSite) -> Telemetry {
        let grid = site.grid_state();
        let p = self.heater.output();
        let s = self.state.lock().clone();
        Telemetry {
            id: self.id,
            category: Some(Category::SteamBoiler),
            active_power_w: Some(p),
            // A P-only AC load — see EvCharger::telemetry's identical
            // note on why Q is advertised as an explicit 0 rather than
            // left absent.
            reactive_power_var: Some(0.0),
            pressure_bar: Some(s.pressure_bar),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            frequency_hz: Some(grid.frequency_hz),
            component_state: Some(crate::sim::component::power_state(p)),
            ..Default::default()
        }
    }

    fn active_power_w(&self, _site: &MicrogridSite) -> Option<f32> {
        Some(self.heater.output())
    }

    fn aggregate_power_w(&self, _world: &MicrogridSite) -> f32 {
        self.heater.output()
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn has_axis(&self, axis: SetpointAxis) -> bool {
        axis == SetpointAxis::Active
    }

    fn set_command(&self, axis: SetpointAxis, value: f32) {
        if axis == SetpointAxis::Active {
            self.heater.set_command(value);
        }
    }

    fn physical_band(&self, axis: SetpointAxis, dt: Duration) -> Option<VecBounds> {
        (axis == SetpointAxis::Active).then(|| self.heat_band(dt))
    }

    /// No command: gas holds pressure, the heater draws nothing.
    fn idle_value(&self, _axis: SetpointAxis) -> Option<f32> {
        Some(0.0)
    }

    /// On the active axis the need is part of the reported bounds,
    /// and an augmentation disjoint from it would park the heater at
    /// 0 W for its whole lifetime, so it is refused.
    fn bounds_follow_physical_band(&self, axis: SetpointAxis) -> bool {
        axis == SetpointAxis::Active
    }

    fn gateway_settings(&self) -> GatewaySettings {
        GatewaySettings {
            command_delay: self.cfg.command_delay,
            ramp_rate_w_per_s: self.cfg.ramp_rate_w_per_s,
            ..GatewaySettings::default()
        }
    }

    fn set_pressure_bar(&self, bar: f32) -> bool {
        if !bar.is_finite() {
            log::warn!("SteamBoiler::set_pressure_bar ignored non-finite value");
            return true;
        }
        self.state.lock().pressure_bar = bar.clamp(f32::MIN_POSITIVE, self.cfg.max_bar);
        true
    }

    fn takes_pressure_bar(&self) -> bool {
        true
    }

    fn set_steam_demand_kg_h(&self, kg_h: f32) -> bool {
        *self.demand_source.write() = DynamicScalar::constant(kg_h);
        true
    }

    fn takes_steam_demand(&self) -> bool {
        true
    }

    fn set_steam_demand_source(&self, scalar: DynamicScalar) {
        SteamBoiler::set_steam_demand_source(self, scalar);
    }

    fn demand_reading(&self) -> Option<ScalarReading> {
        let s = self.demand_source.read();
        Some(ScalarReading {
            value: s.get(),
            expr: s.source_text(),
        })
    }

    fn pressure_reading(&self) -> Option<ScalarReading> {
        Some(ScalarReading {
            value: self.state.lock().pressure_bar,
            expr: None,
        })
    }

    fn pressure_target_bar(&self) -> Option<f32> {
        Some(self.cfg.target_bar)
    }

    fn snapshot_knob(&self, kind: KnobKind) -> Option<KnobSnapshot> {
        match kind {
            KnobKind::BoilerDemand => Some(KnobSnapshot::BoilerDemand(
                self.demand_source.read().clone(),
            )),
            _ => None,
        }
    }

    fn restore_knob(&self, snap: KnobSnapshot) -> bool {
        match snap {
            KnobSnapshot::BoilerDemand(scalar) => {
                *self.demand_source.write() = scalar;
                true
            }
            _ => false,
        }
    }

    fn make_fn(&self) -> &'static str {
        "%make-steam-boiler"
    }

    fn has_unrenderable_source(&self) -> bool {
        self.cfg.demand_dynamic || self.demand_source.read().is_dynamic()
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        let lf = crate::lisp::lisp_float32;
        let d = SteamBoilerConfig::default();
        let mut kw = Vec::new();
        if self.cfg.rated_lower_w != d.rated_lower_w {
            kw.push((":rated-lower", lf(self.cfg.rated_lower_w)));
        }
        kw.push((":rated-upper", lf(self.cfg.rated_upper_w)));
        kw.push((":target-bar", lf(self.cfg.target_bar)));
        kw.push((":max-bar", lf(self.cfg.max_bar)));
        if self.cfg.capacity_wh_per_bar != d.capacity_wh_per_bar {
            kw.push((":capacity-wh-per-bar", lf(self.cfg.capacity_wh_per_bar)));
        }
        if self.cfg.wh_per_kg != d.wh_per_kg {
            kw.push((":wh-per-kg", lf(self.cfg.wh_per_kg)));
        }
        if let Some(initial) = self.cfg.initial_bar
            && initial != self.cfg.target_bar
        {
            kw.push((":initial-bar", lf(initial)));
        }
        if !self.cfg.demand_dynamic {
            kw.push((":demand", lf(self.cfg.demand_kg_h)));
        }
        kw.push((
            ":command-delay-ms",
            self.cfg.command_delay.as_millis().to_string(),
        ));
        if self.cfg.ramp_rate_w_per_s.is_finite() {
            kw.push((":ramp-rate", lf(self.cfg.ramp_rate_w_per_s)));
        }
        kw.extend(crate::sim::inverter::device_delay_kw(self.cfg.device_delay));
        if self.interval != Duration::from_millis(1000) {
            kw.push((":interval", self.interval.as_millis().to_string()));
        }
        if self.cfg.stream_jitter_pct != d.stream_jitter_pct {
            kw.push((":stream-jitter-pct", lf(self.cfg.stream_jitter_pct)));
        }
        kw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimulatedComponent;
    use crate::timeout_tracker::SetpointAxis;
    use chrono::Utc;
    use std::time::Duration;

    fn boiler(cfg: SteamBoilerConfig) -> SteamBoiler {
        SteamBoiler::new(
            700,
            Duration::from_secs(1),
            SteamBoilerConfig {
                command_delay: Duration::ZERO,
                device_delay: Duration::ZERO,
                ..cfg
            },
        )
    }

    fn dt() -> Duration {
        Duration::from_secs(1)
    }

    /// At target with demand set and an allotment above the demand
    /// equivalent, the boiler consumes exactly demand_w (full gas
    /// displacement) and pressure holds at target.
    #[test]
    fn at_target_consumes_exactly_demand_when_allotted() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig::default());
        // 100 kg/h × 627 Wh/kg = 62_700 W
        assert!(b.set_steam_demand_kg_h(100.0));
        b.set_command(SetpointAxis::Active, 200_000.0);
        b.tick(&w, Utc::now(), dt());
        assert!((b.aggregate_power_w(&w) - 62_700.0).abs() < 1.0);
        let t = b.telemetry(&w);
        assert_eq!(t.pressure_bar, Some(8.0));
    }

    /// Allotment below the demand equivalent: consume the allotment;
    /// the (unmodelled) gas covers the rest so pressure stays pinned
    /// at target.
    #[test]
    fn allotment_below_demand_is_consumed_gas_covers_rest() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig::default());
        b.set_steam_demand_kg_h(100.0); // 62.7 kW equivalent
        b.set_command(SetpointAxis::Active, 40_000.0);
        b.tick(&w, Utc::now(), dt());
        assert!((b.aggregate_power_w(&w) - 40_000.0).abs() < 1.0);
        assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0));
    }

    /// No command: zero electric draw, gas holds pressure.
    #[test]
    fn no_command_draws_nothing() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig::default());
        b.set_steam_demand_kg_h(100.0);
        b.tick(&w, Utc::now(), dt());
        assert_eq!(b.aggregate_power_w(&w), 0.0);
        assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0));
    }

    /// Above-target perturbation: electricity is declined (need = 0)
    /// and pressure decays at exactly demand_w per tick until target,
    /// where it holds.
    #[test]
    fn above_target_declines_power_and_decays_at_demand_rate() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig::default());
        b.set_steam_demand_kg_h(100.0); // 62_700 W draw
        assert!(b.set_pressure_bar(9.0));
        b.set_command(SetpointAxis::Active, 100_000.0);
        b.tick(&w, Utc::now(), dt());
        // Declined the allotment entirely.
        assert_eq!(b.aggregate_power_w(&w), 0.0);
        // One second of 62.7 kW draw = 62_700/3600 Wh ≈ 17.4167 Wh
        // → /10_000 Wh-per-bar ≈ 0.0017417 bar below 9.0.
        let p = b.telemetry(&w).pressure_bar.unwrap();
        assert!((p - (9.0 - 62_700.0 / 3600.0 / 10_000.0)).abs() < 1e-4);
        // Decay ≈ 0.00174 bar/tick over the 1.0 bar gap to target: at
        // least ~575 ticks are needed to close it. 1_000 ticks
        // provably reaches (and holds at) target — reduced from the
        // spec's 250_000 per the controller's ruling.
        for _ in 0..1_000 {
            b.tick(&w, Utc::now(), dt());
        }
        assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0));
    }

    /// Below-target start + allotment: the recovery term gives
    /// electricity first claim, so the first tick shows a burst above
    /// the steady demand equivalent, and pressure is back at target
    /// (the gas floor guarantees the state either way).
    #[test]
    fn below_target_start_bursts_electric_when_allotted() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig {
            initial_bar: Some(7.9),
            ..Default::default()
        });
        b.set_steam_demand_kg_h(100.0);
        b.set_command(SetpointAxis::Active, 250_000.0);
        b.tick(&w, Utc::now(), dt());
        // Burst: demand_w (62.7 kW) + recovery for 0.1 bar
        // (0.1 × 10_000 Wh × 3600 / 1 s = 3.6 MW, capped at rated
        // 250 kW) → full rated draw this tick.
        assert!((b.aggregate_power_w(&w) - 250_000.0).abs() < 1.0);
        assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0));
    }

    /// Same start with no command: gas eats the gap invisibly and
    /// pressure is still at target after the tick.
    #[test]
    fn below_target_start_without_command_gas_restores() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig {
            initial_bar: Some(7.0),
            ..Default::default()
        });
        b.tick(&w, Utc::now(), dt());
        assert_eq!(b.aggregate_power_w(&w), 0.0);
        assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0));
    }

    /// Demand sanitize: negative and non-finite readings count as 0.
    #[test]
    fn demand_sanitizes_negative_and_non_finite() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig::default());
        b.set_steam_demand_kg_h(-50.0);
        b.set_command(SetpointAxis::Active, 10_000.0);
        b.tick(&w, Utc::now(), dt());
        assert_eq!(b.aggregate_power_w(&w), 0.0, "negative demand is 0");
        assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0));

        // Same outcome for a non-finite reading. Infinity is the case
        // that tells: without the sanitize it would pass f32::min and
        // pin the need at the rated ceiling.
        for raw in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let b = boiler(SteamBoilerConfig::default());
            b.set_steam_demand_kg_h(raw);
            b.set_command(SetpointAxis::Active, 10_000.0);
            b.tick(&w, Utc::now(), dt());
            assert_eq!(b.aggregate_power_w(&w), 0.0, "{raw} demand is 0");
            assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0), "{raw}");
        }
    }

    /// A max_bar below target_bar is lifted to target_bar, so tick's
    /// pressure clamp has a valid range.
    #[test]
    fn new_guards_max_bar_below_target() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig {
            target_bar: 8.0,
            max_bar: 4.0,
            ..Default::default()
        });
        assert_eq!(b.cfg.max_bar, 8.0);
        // A few ticks must not panic.
        let now = Utc::now();
        for i in 0..5 {
            b.tick(
                &w,
                now + chrono::Duration::seconds(i),
                Duration::from_secs(1),
            );
        }
        assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0));
    }

    /// A max_bar that is not a finite value at or above target_bar —
    /// NaN, infinite, zero, or subnormal — falls back to target_bar.
    #[test]
    fn new_guards_non_finite_and_subnormal_max_bar() {
        for max in [f32::NAN, f32::INFINITY, 0.0, 1e-40] {
            let b = boiler(SteamBoilerConfig {
                target_bar: 6.0,
                max_bar: max,
                ..Default::default()
            });
            assert_eq!(b.cfg.max_bar, 6.0, "max_bar {max}");
        }
    }

    /// A NaN initial_bar survives every later clamp and pins the
    /// electric need at the rated ceiling; an infinite one is no start
    /// pressure at all. Both start at target instead.
    #[test]
    fn new_guards_non_finite_initial_bar() {
        let w = crate::sim::MicrogridSite::new();
        for bar in [f32::NAN, f32::INFINITY] {
            let b = boiler(SteamBoilerConfig {
                initial_bar: Some(bar),
                ..Default::default()
            });
            b.tick(&w, Utc::now(), dt());
            assert_eq!(b.telemetry(&w).pressure_bar, Some(8.0), "initial_bar {bar}");
            assert_eq!(b.aggregate_power_w(&w), 0.0, "initial_bar {bar}");
        }
    }

    /// The two constants tick divides and multiplies by must be positive
    /// normal numbers: zero or NaN makes the integrator NaN, infinity
    /// turns a zero demand into NaN, a negative value inverts the
    /// integrator, a subnormal one overflows it.
    #[test]
    fn new_guards_capacity_and_wh_per_kg() {
        let defaults = SteamBoilerConfig::default();
        for v in [f32::NAN, f32::INFINITY, 0.0, -1.0, 1e-40] {
            let b = boiler(SteamBoilerConfig {
                capacity_wh_per_bar: v,
                wh_per_kg: v,
                ..Default::default()
            });
            assert_eq!(
                b.cfg.capacity_wh_per_bar, defaults.capacity_wh_per_bar,
                "{v}"
            );
            assert_eq!(b.cfg.wh_per_kg, defaults.wh_per_kg, "{v}");
        }
    }

    /// A target_bar that is not a positive normal number falls back to
    /// the default target; a max_bar below that is then lifted to it.
    #[test]
    fn new_guards_bad_target_bar() {
        for target in [f32::NAN, -1.0, 0.0, 1e-40] {
            let b = boiler(SteamBoilerConfig {
                target_bar: target,
                max_bar: 5.0,
                ..Default::default()
            });
            assert_eq!(
                b.cfg.target_bar,
                SteamBoilerConfig::default().target_bar,
                "target {target}"
            );
            assert_eq!(
                b.cfg.max_bar,
                SteamBoilerConfig::default().target_bar,
                "target {target}"
            );
        }
    }

    /// set_pressure_bar sanitizes: non-finite rejected, values
    /// clamped into (0, max_bar].
    #[test]
    fn set_pressure_clamps_to_max() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig::default());
        assert!(b.set_pressure_bar(50.0));
        // Clamped to max (10.0); a tick with no demand keeps it there
        // (nothing draws it down).
        b.tick(&w, Utc::now(), dt());
        assert_eq!(b.telemetry(&w).pressure_bar, Some(10.0));
        assert!(b.set_pressure_bar(f32::NAN));
        assert_eq!(b.telemetry(&w).pressure_bar, Some(10.0), "NaN ignored");
    }

    /// The reported bounds advertise the live heat need, computed
    /// fresh — no tick is needed after the demand changes.
    #[test]
    fn bounds_track_need_before_any_tick() {
        let (w, b) = sited(boiler(SteamBoilerConfig::default()));
        assert_eq!(
            w.bounds_of(700, SetpointAxis::Active).unwrap().0[0].upper,
            Some(0.0)
        );
        b.set_steam_demand_kg_h(100.0);
        let upper = w.bounds_of(700, SetpointAxis::Active).unwrap().0[0]
            .upper
            .unwrap();
        assert!((upper - 62_700.0).abs() < 1.0, "got {upper}");
    }

    /// An augmentation must overlap the heat need: at idle the need
    /// is [0, 0], so a strictly positive band is refused, naming it.
    #[test]
    fn an_augmentation_must_overlap_the_heat_need() {
        let (w, _b) = sited(boiler(SteamBoilerConfig::default()));
        let e = w
            .gateway()
            .augment(
                700,
                w.run_generation(),
                SetpointAxis::Active,
                VecBounds::single(10_000.0, 20_000.0),
                Duration::from_secs(30),
            )
            .unwrap_err();
        assert!(e.to_string().contains("current envelope [0, 0]"), "{e}");
    }

    /// Telemetry advertises explicit zero reactive (P-only AC load)
    /// and the pressure fields.
    #[test]
    fn telemetry_shape() {
        let w = crate::sim::MicrogridSite::new();
        let b = boiler(SteamBoilerConfig::default());
        let t = b.telemetry(&w);
        assert_eq!(t.reactive_power_var, Some(0.0));
        assert_eq!(t.pressure_bar, Some(8.0));
        assert_eq!(b.pressure_target_bar(), Some(8.0));
        assert!(b.takes_pressure_bar());
        assert!(b.takes_steam_demand());
    }

    /// Every construction kwarg round-trips; :ramp-rate renders only
    /// when finite, :interval only off-default, :demand only when
    /// the source is a plain number, :initial-bar only when it
    /// departs from target.
    #[test]
    fn constructor_kwargs_round_trip() {
        let b = SteamBoiler::new(
            9,
            Duration::from_millis(500),
            SteamBoilerConfig {
                rated_upper_w: 100_000.0,
                target_bar: 6.0,
                max_bar: 9.0,
                demand_kg_h: 40.0,
                stream_jitter_pct: 5.0,
                ..Default::default()
            },
        );
        assert_eq!(b.make_fn(), "%make-steam-boiler");
        let s = b
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(s.contains(":rated-upper 100000.0"));
        assert!(s.contains(":target-bar 6.0"));
        assert!(s.contains(":max-bar 9.0"));
        assert!(s.contains(":demand 40.0"));
        assert!(s.contains(":interval 500"));
        assert!(s.contains(":stream-jitter-pct 5.0"));
        assert!(!s.contains(":ramp-rate"), "infinite ramp omitted");
        assert!(!s.contains(":initial-bar"), "default initial omitted");

        // Dynamic demand is omitted entirely (unrenderable source).
        let b2 = SteamBoiler::new(
            10,
            Duration::from_secs(1),
            SteamBoilerConfig {
                demand_dynamic: true,
                ..Default::default()
            },
        );
        let s2 = b2
            .constructor_kwargs()
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!s2.contains(":demand"));
        assert!(b2.has_unrenderable_source());
    }

    /// Snapshot/restore round-trip for the boiler-demand knob — the
    /// steam-boiler twin of the sunlight test: a dynamic source
    /// survives a scenario collapsing it to a constant and back.
    #[test]
    fn snapshot_restore_round_trip_boiler_demand_dynamic_source() {
        let mut ctx = tulisp::TulispContext::new();
        let b = boiler(SteamBoilerConfig::default());
        let lambda = ctx.eval_string("(lambda () 77.0)").unwrap();
        let scalar = DynamicScalar::from_lisp(&lambda, 0.0).unwrap();
        b.set_steam_demand_source(scalar);
        let text_before = b.demand_reading().unwrap().expr;
        assert!(text_before.is_some());

        let snap = b.snapshot_knob(KnobKind::BoilerDemand).unwrap();

        // A scenario collapses it to a constant.
        assert!(b.set_steam_demand_kg_h(15.0));
        assert!(b.demand_reading().unwrap().expr.is_none());
        assert!(!b.has_unrenderable_source());

        assert!(b.restore_knob(snap));
        assert_eq!(b.demand_reading().unwrap().expr, text_before);
        assert!(b.has_unrenderable_source(), "dynamic source restored");
    }

    /// The one mismatch case worth a test: `Sunlight` and
    /// `BoilerDemand` are the two scalar knobs a scenario drives the
    /// same way, and are told apart only by their variant — a
    /// sunlight snapshot wrapping the very same `DynamicScalar` a
    /// boiler demand would carry must be refused, not written into
    /// the demand slot. Every other cross-knob pairing is refused by
    /// the same single `match` on the variant.
    #[test]
    fn restore_knob_rejects_a_sunlight_snapshot() {
        use crate::sim::inverter::solar_inverter::SunlightSource;
        let b = boiler(SteamBoilerConfig::default());
        assert!(
            !b.restore_knob(KnobSnapshot::Sunlight(SunlightSource::manual(
                DynamicScalar::constant(1.0)
            )))
        );
        assert_eq!(b.demand_reading().unwrap().value, 0.0);
    }

    /// Register `b` in a fresh site and hand back both, so a test can
    /// drive the boiler's health through `MicrogridSite`.
    fn sited(b: SteamBoiler) -> (MicrogridSite, std::sync::Arc<dyn SimulatedComponent>) {
        let w = MicrogridSite::new();
        w.register(b);
        let b = w.get(700).unwrap();
        (w, b)
    }

    /// A boiler that slews at 10 kW/s (with `boiler`'s zero command
    /// delay), so a trip — one tick to zero — reads differently from
    /// a reset, seven ticks down from the demand.
    fn slewing_boiler() -> SteamBoiler {
        boiler(SteamBoilerConfig {
            ramp_rate_w_per_s: 10_000.0,
            ..Default::default()
        })
    }

    /// `n` one-second ticks.
    fn tick_n(w: &MicrogridSite, _b: &std::sync::Arc<dyn SimulatedComponent>, n: usize) {
        w.tick_n(n, dt());
    }

    /// The heat need is a physical limit: when the steam demand
    /// drops, the electric draw drops to the new need in the same
    /// tick instead of slewing down at the ramp rate.
    #[test]
    fn a_falling_demand_cuts_the_draw_at_once() {
        let (w, b) = sited(slewing_boiler());
        assert!(b.set_steam_demand_kg_h(100.0)); // 62_700 W equivalent
        w.gateway()
            .command(700, SetpointAxis::Active, 200_000.0)
            .unwrap();
        tick_n(&w, &b, 7);
        assert!((b.aggregate_power_w(&w) - 62_700.0).abs() < 1.0);
        assert!(b.set_steam_demand_kg_h(10.0)); // 6_270 W equivalent
        tick_n(&w, &b, 1);
        assert!(
            (b.aggregate_power_w(&w) - 6_270.0).abs() < 1.0,
            "the draw follows the need down in one tick, got {}",
            b.aggregate_power_w(&w),
        );
    }

    /// An errored or standby boiler is electrically offline: zero
    /// draw, the command gone, recovery waiting for a re-dispatch —
    /// while the gas burner keeps pressure at target throughout.
    #[test]
    fn faulted_boiler_trips_and_awaits_redispatch() {
        for health in [Health::Error, Health::Standby] {
            let (w, b) = sited(slewing_boiler());
            assert!(b.set_steam_demand_kg_h(100.0)); // 62_700 W equivalent
            w.gateway()
                .command(700, SetpointAxis::Active, 200_000.0)
                .unwrap();
            tick_n(&w, &b, 7);
            assert!(
                (b.aggregate_power_w(&w) - 62_700.0).abs() < 1.0,
                "healthy boiler displaces demand, got {}",
                b.aggregate_power_w(&w),
            );

            w.set_health(700, health).unwrap();
            tick_n(&w, &b, 1);
            assert_eq!(
                b.aggregate_power_w(&w),
                0.0,
                "a {health:?} boiler draws nothing"
            );
            assert_eq!(
                b.telemetry(&w).pressure_bar,
                Some(8.0),
                "gas holds pressure through the fault",
            );

            w.set_health(700, Health::Ok).unwrap();
            tick_n(&w, &b, 5);
            assert_eq!(b.aggregate_power_w(&w), 0.0, "no command survives the trip");

            w.gateway()
                .command(700, SetpointAxis::Active, 200_000.0)
                .unwrap();
            tick_n(&w, &b, 7);
            assert!(
                (b.aggregate_power_w(&w) - 62_700.0).abs() < 1.0,
                "a new command resumes displacement, got {}",
                b.aggregate_power_w(&w),
            );
        }
    }

    /// The steam side runs on through a fault: an above-target excess
    /// still decays at the demand rate with the heater offline, and
    /// the reported envelope keeps tracking need.
    #[test]
    fn faulted_boiler_keeps_integrating_pressure() {
        let (w, b) = sited(boiler(SteamBoilerConfig {
            initial_bar: Some(9.0),
            ..Default::default()
        }));
        assert!(b.set_steam_demand_kg_h(100.0)); // 62_700 W = 17.4 Wh/s
        w.set_health(700, Health::Error).unwrap();
        tick_n(&w, &b, 10);
        let bar = b.telemetry(&w).pressure_bar.unwrap();
        // 10 s × 62_700 W / 3600 / 10_000 Wh/bar ≈ 0.017 bar off.
        assert!(
            bar < 8.99 && bar > 8.9,
            "excess decays while tripped, got {bar}"
        );
        assert_eq!(b.aggregate_power_w(&w), 0.0, "still no draw");
        // Back at target the boiler would take demand: that is what
        // it reports even while offline.
        assert!(b.set_pressure_bar(8.0));
        tick_n(&w, &b, 1);
        let eff = w.bounds_of(700, SetpointAxis::Active).unwrap();
        assert_eq!(
            eff.0[0].upper,
            Some(62_700.0),
            "bounds keep tracking need while tripped",
        );
    }

    /// TTL expiry slews the draw down at the ramp rate instead of
    /// snapping to zero in one tick, as the inverters and the EV
    /// charger do.
    #[test]
    fn ttl_expiry_slews_down() {
        let (w, b) = sited(slewing_boiler());
        b.set_steam_demand_kg_h(100.0); // 62_700 W equivalent
        w.gateway()
            .command(700, SetpointAxis::Active, 200_000.0)
            .unwrap();
        tick_n(&w, &b, 7);
        assert!((b.aggregate_power_w(&w) - 62_700.0).abs() < 1.0);

        w.gateway().reset(700, SetpointAxis::Active);
        tick_n(&w, &b, 1);
        let p = b.aggregate_power_w(&w);
        assert!(p > 50_000.0 && p < 62_700.0, "one tick of slew, got {p}");
    }
}
