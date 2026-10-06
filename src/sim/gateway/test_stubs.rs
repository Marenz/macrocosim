//! Stub components the gateway's unit tests share.

use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Battery, BatteryInverter, Category, Controllable, MicrogridSite, ReactiveLimits,
    SimulatedComponent, Telemetry,
    battery::BatteryConfig,
    bounds::VecBounds,
    component::GatewaySettings,
    inverter::battery_inverter::BatteryInverterConfig,
    reactive::ReactiveCapability,
    sim_clock::{NowSource, headless_base},
};
use crate::timeout_tracker::SetpointAxis;

/// Register `c` and hand it back, so a test can read its state.
pub(crate) fn put<C: SimulatedComponent + 'static>(site: &MicrogridSite, c: Arc<C>) -> Arc<C> {
    let d: Arc<dyn SimulatedComponent> = c.clone();
    site.register_arc(d);
    c
}

/// A battery inverter rated ±10 kW with no gateway delay, no ramp and
/// no device delay.
pub(crate) fn instant_inverter(id: u64) -> BatteryInverter {
    BatteryInverter::new(
        id,
        Duration::from_secs(1),
        BatteryInverterConfig {
            rated_lower_w: -10_000.0,
            rated_upper_w: 10_000.0,
            device_delay: Duration::ZERO,
            ..Default::default()
        },
    )
}

/// `instant_inverter` with a 1 kW/s gateway ramp.
pub(crate) fn ramping_inverter(id: u64) -> BatteryInverter {
    BatteryInverter::new(
        id,
        Duration::from_secs(1),
        BatteryInverterConfig {
            rated_lower_w: -10_000.0,
            rated_upper_w: 10_000.0,
            ramp_rate_w_per_s: 1_000.0,
            device_delay: Duration::ZERO,
            ..Default::default()
        },
    )
}

/// A 1 kWh battery at 89.5 % SoC, rated ±5 kW, whose window closes at
/// a 90 % `:soc-upper-pct` with no protect margin.
pub(crate) fn nearly_full_pack(id: u64) -> Battery {
    Battery::new(
        id,
        Duration::from_secs(1),
        BatteryConfig {
            capacity_wh: 1_000.0,
            initial_soc_pct: 89.5,
            soc_upper_pct: 90.0,
            soc_protect_margin_pct: 0.0,
            rated_lower_w: -5_000.0,
            rated_upper_w: 5_000.0,
            ..Default::default()
        },
    )
}

/// A site on a hand-advanced clock that starts at `headless_base`.
pub(crate) fn sim_site() -> (MicrogridSite, Arc<tulisp_async::ManualClock>) {
    let clock = Arc::new(tulisp_async::ManualClock::new());
    let site = MicrogridSite::new();
    site.set_now_source(NowSource::sim(headless_base(), clock.clone()));
    (site, clock)
}

/// A component with a gateway-owned active axis, rated ±1000 W, that
/// records every command the gateway hands it.
pub(crate) struct Hw {
    pub id: u64,
    pub commands: Mutex<Vec<f32>>,
    pub physical: Mutex<Option<VecBounds>>,
    pub idle: Option<f32>,
    pub initial: f32,
    pub keeps: bool,
    pub follows_physical: bool,
    pub settings: GatewaySettings,
}

impl Hw {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            commands: Mutex::new(Vec::new()),
            physical: Mutex::new(None),
            idle: None,
            initial: 0.0,
            keeps: false,
            follows_physical: false,
            settings: GatewaySettings::default(),
        }
    }

    /// A stub whose active-axis bounds follow its physical band.
    pub fn following_physical(id: u64) -> Self {
        Self {
            follows_physical: true,
            ..Self::new(id)
        }
    }

    /// The last command handed in.
    pub fn last(&self) -> Option<f32> {
        self.commands.lock().last().copied()
    }
}

impl fmt::Display for Hw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "hw-{}", self.id)
    }
}

impl SimulatedComponent for Hw {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Inverter
    }
    fn name(&self) -> &str {
        "hw"
    }
    fn stream_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
    fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
        Telemetry {
            id: self.id,
            active_power_w: Some(self.last().unwrap_or(0.0)),
            ..Default::default()
        }
    }
    fn active_power_w(&self, _: &MicrogridSite) -> Option<f32> {
        Some(self.last().unwrap_or(0.0))
    }
    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((-1000.0, 1000.0))
    }
    fn controllable(&self) -> Option<&dyn Controllable> {
        Some(self)
    }
    fn make_fn(&self) -> &'static str {
        "%make-test-hw"
    }
    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}

impl Controllable for Hw {
    fn has_axis(&self, axis: SetpointAxis) -> bool {
        axis == SetpointAxis::Active
    }
    fn set_command(&self, axis: SetpointAxis, value: f32) {
        if axis == SetpointAxis::Active {
            self.commands.lock().push(value);
        }
    }
    fn physical_band(&self, axis: SetpointAxis, _: Duration) -> Option<VecBounds> {
        (axis == SetpointAxis::Active)
            .then(|| self.physical.lock().clone())
            .flatten()
    }
    fn idle_value(&self, _: SetpointAxis) -> Option<f32> {
        self.idle
    }
    fn initial_value(&self, _: SetpointAxis) -> f32 {
        self.initial
    }
    fn keeps_command_through_fault(&self, _: SetpointAxis) -> bool {
        self.keeps
    }
    fn bounds_follow_physical_band(&self, axis: SetpointAxis) -> bool {
        axis == SetpointAxis::Active && self.follows_physical
    }
    fn gateway_settings(&self) -> GatewaySettings {
        self.settings
    }
}

/// A two-axis component whose measured P follows its last active
/// command at once, with a pure 1 kVA reactive cap that is also its
/// reactive physical band at that P. Always id 1.
pub(crate) struct Pq {
    pub p: Mutex<f32>,
    pub q: Mutex<Option<f32>>,
}

impl Pq {
    pub fn new() -> Self {
        Self {
            p: Mutex::new(0.0),
            q: Mutex::new(None),
        }
    }
}

impl fmt::Display for Pq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pq")
    }
}

impl SimulatedComponent for Pq {
    fn id(&self) -> u64 {
        1
    }
    fn category(&self) -> Category {
        Category::Inverter
    }
    fn name(&self) -> &str {
        "pq"
    }
    fn stream_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
    fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
        Telemetry::default()
    }
    fn active_power_w(&self, _: &MicrogridSite) -> Option<f32> {
        Some(*self.p.lock())
    }
    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((-1000.0, 1000.0))
    }
    fn controllable(&self) -> Option<&dyn Controllable> {
        Some(self)
    }
    fn reactive_limits(&self) -> Option<&dyn ReactiveLimits> {
        Some(self)
    }
    fn make_fn(&self) -> &'static str {
        "%make-test-pq"
    }
    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}

/// Fixed caps: the setters are never called on the stub.
impl ReactiveLimits for Pq {
    fn reactive_capability(&self) -> ReactiveCapability {
        ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(1000.0),
        }
    }
    fn set_reactive_pf_limit(&self, _: Option<f32>) {}
    fn set_reactive_apparent_va(&self, _: Option<f32>) {}
}

impl Controllable for Pq {
    fn has_axis(&self, _: SetpointAxis) -> bool {
        true
    }
    fn set_command(&self, axis: SetpointAxis, value: f32) {
        match axis {
            SetpointAxis::Active => *self.p.lock() = value,
            SetpointAxis::Reactive => *self.q.lock() = Some(value),
        }
    }
    fn physical_band(&self, axis: SetpointAxis, _: Duration) -> Option<VecBounds> {
        let cap = self.reactive_capability();
        (axis == SetpointAxis::Reactive).then(|| cap.q_band_at(*self.p.lock()))
    }
}
