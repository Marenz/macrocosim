//! Stub components the gateway's unit tests share.

use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    AugmentError, Category, MicrogridSite, SetpointError, SimulatedComponent, Telemetry,
    bounds::VecBounds,
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

/// A component that takes setpoints through the trait doors: active
/// setpoints inside ±1000 W, any augmentation, and every axis reset
/// recorded.
pub(crate) struct Cmd {
    pub id: u64,
    pub last: Mutex<Option<f32>>,
    pub resets: Mutex<Vec<SetpointAxis>>,
}

impl Cmd {
    pub fn new(id: u64) -> Arc<Self> {
        Arc::new(Self {
            id,
            last: Mutex::new(None),
            resets: Mutex::new(Vec::new()),
        })
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cmd-{}", self.id)
    }
}

impl SimulatedComponent for Cmd {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Inverter
    }
    fn name(&self) -> &str {
        "cmd"
    }
    fn stream_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
    fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
        Telemetry {
            id: self.id,
            active_power_w: Some(self.last.lock().unwrap_or(0.0)),
            ..Default::default()
        }
    }
    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((-1000.0, 1000.0))
    }
    fn set_active_setpoint(&self, v: f32) -> Result<(), SetpointError> {
        if v != 0.0 && !(-1000.0..=1000.0).contains(&v) {
            return Err(SetpointError::OutOfBounds {
                value: v,
                unit: "W",
                envelope: VecBounds::single(-1000.0, 1000.0),
            });
        }
        *self.last.lock() = Some(v);
        Ok(())
    }
    fn reset_setpoint_axis(&self, axis: SetpointAxis) {
        self.resets.lock().push(axis);
    }
    fn try_augment_active_bounds(
        &self,
        _: DateTime<Utc>,
        _: VecBounds,
        _: Duration,
    ) -> Result<(), AugmentError> {
        Ok(())
    }
    fn make_fn(&self) -> &'static str {
        "%make-test-cmd"
    }
    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        Vec::new()
    }
}
