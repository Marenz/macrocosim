//! Stub components the gateway's unit tests share.

use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, MicrogridSite, SimulatedComponent, Telemetry,
    bounds::VecBounds,
    component::GatewaySettings,
    sim_clock::{NowSource, headless_base},
};
use crate::timeout_tracker::SetpointAxis;

/// Register `c` and hand it back, so a test can read its state.
pub(crate) fn put<C: SimulatedComponent + 'static>(site: &MicrogridSite, c: Arc<C>) -> Arc<C> {
    let d: Arc<dyn SimulatedComponent> = c.clone();
    site.register_arc(d);
    c
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
    pub checks_physical: bool,
    pub advertises: bool,
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
            checks_physical: false,
            advertises: false,
            settings: GatewaySettings::default(),
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
    fn advertises_physical_band(&self, _: SetpointAxis) -> bool {
        self.advertises
    }
    fn augment_checks_physical_band(&self, _: SetpointAxis) -> bool {
        self.checks_physical
    }
    fn gateway_settings(&self) -> Option<GatewaySettings> {
        Some(self.settings)
    }
    fn make_fn(&self) -> &'static str {
        "%make-test-hw"
    }
    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}
