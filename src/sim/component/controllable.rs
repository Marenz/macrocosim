//! `Controllable`: the hardware facts the gateway reads off a
//! component it drives, and the command input it writes to. The
//! Microgrid API rules themselves live in `sim::gateway`, never here.

use std::time::Duration;

use crate::sim::bounds::VecBounds;
use crate::timeout_tracker::SetpointAxis;

/// The gateway delay and ramp knobs of a component's axes. They live
/// in the component's config so `constructor_kwargs` renders them;
/// the gateway reads them once, on registration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GatewaySettings {
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub reactive_command_delay: Duration,
    pub reactive_ramp_rate_var_per_s: f32,
}

impl Default for GatewaySettings {
    fn default() -> Self {
        Self {
            command_delay: Duration::ZERO,
            ramp_rate_w_per_s: f32::INFINITY,
            reactive_command_delay: Duration::ZERO,
            reactive_ramp_rate_var_per_s: f32::INFINITY,
        }
    }
}

impl GatewaySettings {
    /// The gateway delay of `axis`.
    pub fn delay(&self, axis: SetpointAxis) -> Duration {
        match axis {
            SetpointAxis::Active => self.command_delay,
            SetpointAxis::Reactive => self.reactive_command_delay,
        }
    }

    /// The ramp rate of `axis`, per second.
    pub fn ramp_rate(&self, axis: SetpointAxis) -> f32 {
        match axis {
            SetpointAxis::Active => self.ramp_rate_w_per_s,
            SetpointAxis::Reactive => self.reactive_ramp_rate_var_per_s,
        }
    }
}

/// A component the gateway drives: it takes a command on one or more
/// axes. Reached through `SimulatedComponent::controllable`.
pub trait Controllable: Send + Sync {
    /// Whether this component takes a command on `axis`. The gateway
    /// owns one `GatewayAxis` for every axis that answers `true`.
    fn has_axis(&self, axis: SetpointAxis) -> bool;

    /// The command input of `axis`: the output moves to `value` after
    /// the device delay. The gateway calls it every tick; a frontend
    /// without the API rules may call it directly.
    fn set_command(&self, axis: SetpointAxis, value: f32);

    /// The current physical limit on `axis` (PV sunlight, boiler heat
    /// need, the reactive caps at the live P), computed fresh from
    /// the component's state for a tick of length `dt`; `None` when
    /// nothing physical limits the axis. Contains 0.
    fn physical_band(&self, axis: SetpointAxis, dt: Duration) -> Option<VecBounds>;

    /// What `axis` aims at with no command standing; `None` holds the
    /// last target. An expired or reset command ramps to it, or to 0
    /// when `None`.
    fn idle_value(&self, _axis: SetpointAxis) -> Option<f32> {
        None
    }

    /// Where `axis` starts on registration.
    fn initial_value(&self, _axis: SetpointAxis) -> f32 {
        0.0
    }

    /// Whether a health fault keeps the standing command on `axis`.
    fn keeps_command_through_fault(&self, _axis: SetpointAxis) -> bool {
        false
    }

    /// Whether `axis`'s bounds follow the physical band: the bounds
    /// reported for it include the band, and an augmentation on it
    /// must overlap the band. Answered per axis, so a component can
    /// opt in one axis only.
    fn bounds_follow_physical_band(&self, _axis: SetpointAxis) -> bool {
        false
    }

    /// The gateway delay and ramp knobs; the defaults for a component
    /// that sets none.
    fn gateway_settings(&self) -> GatewaySettings {
        GatewaySettings::default()
    }
}
