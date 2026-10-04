//! `MicrogridGateway`: the Frequenz Microgrid API rules of one
//! microgrid — setpoint validation, request lifetimes, bounds
//! augmentations and their expiry. The gRPC service and the Lisp
//! setpoint commands talk to it; the component models stay hardware.
//!
//! [`MicrogridGateway`] is the state, held by `MicrogridSite` for the
//! site's whole life and cleared by `MicrogridSite::reset`.
//! `MicrogridSite::gateway` hands out a [`Gateway`], which pairs that
//! state with its site and carries the interface.
//!
//! Locking: one `Mutex<GatewayState>`. Every public method takes it
//! once for its whole body; `step` holds it from start to end. Lock
//! order: gateway → registry read guards (`by_id`, `connections`,
//! `runtime`) → a component's own locks. Site methods call into the
//! gateway only while holding no registry guard. The lock is never
//! held across an `.await`.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    AugmentError, MicrogridSite, SetpointError, SimulatedComponent, bounds::VecBounds,
};
use crate::timeout_tracker::{SetpointAxis, TimeoutTracker, deadline_after};

#[cfg(test)]
mod test_stubs;

/// The refusal text for a request that outlived its site's run.
pub const SITE_RESET: &str = "site was reset since the request was looked up";

/// What happens to a setpoint outside the envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Refuse it, as the gRPC route does.
    Reject,
    /// Clamp it into the envelope under the same lock (Lisp `CLAMP`).
    Clamp,
}

/// An accepted setpoint: the value applied and when its request
/// lifetime runs out on the site clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Applied {
    pub value: f32,
    pub deadline: DateTime<Utc>,
}

/// Why a setpoint was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum GatewayError {
    /// No component with this id is registered.
    NotFound(u64),
    /// The component takes no command on this axis.
    NoAxis { id: u64, axis: SetpointAxis },
    /// NaN or ±∞.
    NonFinite { value: f32 },
    /// Outside the envelope; carries the whole message.
    OutOfEnvelope(String),
    /// The site was reset between the lookup and the command.
    SiteReset,
}

impl GatewayError {
    fn from_setpoint(id: u64, axis: SetpointAxis, e: SetpointError) -> Self {
        match e {
            SetpointError::Unsupported => Self::NoAxis { id, axis },
            e @ SetpointError::OutOfBounds { .. } => Self::OutOfEnvelope(e.to_string()),
        }
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "component {id} not found"),
            Self::NoAxis { .. } => write!(f, "operation not supported by this component type"),
            Self::NonFinite { value } => write!(f, "set-point {value} is non-finite"),
            Self::OutOfEnvelope(m) => write!(f, "{m}"),
            Self::SiteReset => write!(f, "{SITE_RESET}"),
        }
    }
}

impl std::error::Error for GatewayError {}

/// The per-microgrid API state.
#[derive(Default)]
pub struct MicrogridGateway {
    state: Mutex<GatewayState>,
}

#[derive(Default)]
struct GatewayState {
    /// Request lifetimes, on the site clock.
    lifetimes: TimeoutTracker,
}

impl GatewayState {
    /// Drop everything held for `id`.
    fn forget(&mut self, id: u64) {
        self.lifetimes.remove_component(id);
    }
}

impl MicrogridGateway {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop every command, lifetime and augmentation (a site reset).
    pub(crate) fn clear(&self) {
        self.state.lock().lifetimes.clear();
    }

    /// Set up a component that has just been (re-)registered,
    /// dropping whatever an earlier occupant of its id left behind.
    pub(crate) fn on_register(&self, c: &dyn SimulatedComponent) {
        self.state.lock().forget(c.id());
    }

    /// Drop what is held for a removed component.
    pub(crate) fn forget(&self, id: u64) {
        self.state.lock().forget(id);
    }
}

/// A site's gateway, paired with the site it reads.
pub struct Gateway<'a> {
    site: &'a MicrogridSite,
    gw: &'a MicrogridGateway,
}

impl<'a> Gateway<'a> {
    pub(crate) fn new(site: &'a MicrogridSite, gw: &'a MicrogridGateway) -> Self {
        Self { site, gw }
    }

    /// An active-power setpoint; see [`Self::set_power`].
    pub fn set_active_power(
        &self,
        id: u64,
        generation: u64,
        value: f32,
        lifetime: Duration,
        mode: Mode,
    ) -> Result<Applied, GatewayError> {
        self.set_power(SetpointAxis::Active, id, generation, value, lifetime, mode)
    }

    /// A reactive-power setpoint; see [`Self::set_power`].
    pub fn set_reactive_power(
        &self,
        id: u64,
        generation: u64,
        value: f32,
        lifetime: Duration,
        mode: Mode,
    ) -> Result<Applied, GatewayError> {
        self.set_power(
            SetpointAxis::Reactive,
            id,
            generation,
            value,
            lifetime,
            mode,
        )
    }

    /// Validate (or clamp) `value` for `id`'s `axis`, apply it and
    /// arm its lifetime, all under the gateway lock. `generation` is
    /// the site's `run_generation` the caller observed when it looked
    /// the component up; a moved generation is refused.
    pub fn set_power(
        &self,
        axis: SetpointAxis,
        id: u64,
        generation: u64,
        value: f32,
        lifetime: Duration,
        mode: Mode,
    ) -> Result<Applied, GatewayError> {
        let st = self.gw.state.lock();
        if self.site.run_generation() != generation {
            return Err(GatewayError::SiteReset);
        }
        let c = self.site.get(id).ok_or(GatewayError::NotFound(id))?;
        if !value.is_finite() {
            return Err(GatewayError::NonFinite { value });
        }
        let value = match mode {
            Mode::Reject => {
                self.site
                    .gate_setpoint(id, axis, value)
                    .map_err(GatewayError::OutOfEnvelope)?;
                value
            }
            Mode::Clamp => {
                let envelope = match axis {
                    SetpointAxis::Active => self
                        .site
                        .active_setpoint_envelope(id)
                        .or_else(|| c.effective_active_bounds()),
                    SetpointAxis::Reactive => self
                        .site
                        .reactive_setpoint_envelope(id)
                        .or_else(|| c.reactive_bounds()),
                };
                clamp_into(value, envelope)
            }
        };
        let deadline = st
            .lifetimes
            .actuate_and_arm(id, axis, self.site.now(), lifetime, || match axis {
                SetpointAxis::Active => c.set_active_setpoint(value),
                SetpointAxis::Reactive => c.set_reactive_setpoint(value),
            })
            .map_err(|e| GatewayError::from_setpoint(id, axis, e))?;
        Ok(Applied { value, deadline })
    }

    /// Check `bounds`' shape, then check it against `id`'s live
    /// `axis` envelope and store it, atomically. Returns the deadline
    /// on the site clock.
    pub fn augment(
        &self,
        id: u64,
        generation: u64,
        axis: SetpointAxis,
        bounds: VecBounds,
        lifetime: Duration,
    ) -> Result<DateTime<Utc>, AugmentError> {
        let _st = self.gw.state.lock();
        if self.site.run_generation() != generation {
            return Err(AugmentError::SiteReset);
        }
        let c = self.site.get(id).ok_or(AugmentError::NotFound(id))?;
        let now = self.site.now();
        c.try_augment_bounds(axis, now, bounds, lifetime)?;
        Ok(deadline_after(now, lifetime))
    }

    /// Clear `id`'s `axis` command and lifetime and park the axis.
    pub fn reset(&self, id: u64, axis: SetpointAxis) {
        let st = self.gw.state.lock();
        st.lifetimes.remove(id, axis);
        if let Some(c) = self.site.get(id) {
            c.reset_setpoint_axis(axis);
        }
    }

    /// Time left on the site clock before `id`'s `axis` command
    /// expires.
    pub fn remaining_lifetime(&self, id: u64, axis: SetpointAxis) -> Option<Duration> {
        let st = self.gw.state.lock();
        st.lifetimes.remaining(id, axis, self.site.now())
    }

    /// One physics tick of the API rules, run by
    /// `MicrogridSite::tick_once` before the components tick.
    pub fn step(&self, now: DateTime<Utc>, _dt: Duration) {
        self.expire(now);
    }

    /// Expire every lifetime at or before `now` and reset its axis.
    pub(crate) fn expire(&self, now: DateTime<Utc>) {
        let st = self.gw.state.lock();
        let mut expired = Vec::new();
        st.lifetimes.reset_expired_with(now, |id, axis| {
            if let Some(c) = self.site.get(id) {
                c.reset_setpoint_axis(axis);
            }
            expired.push((id, axis));
        });
        drop(st);
        for (id, axis) in expired {
            log::info!("Request timeout for component {id} ({axis:?}) — resetting that axis");
        }
    }

    /// Actuate `f` and arm a lifetime for it under the gateway lock.
    pub(crate) fn arm<E>(
        &self,
        id: u64,
        axis: SetpointAxis,
        lifetime: Duration,
        f: impl FnOnce() -> Result<(), E>,
    ) -> Result<DateTime<Utc>, E> {
        let st = self.gw.state.lock();
        st.lifetimes
            .actuate_and_arm(id, axis, self.site.now(), lifetime, f)
    }

    /// `set_power` with the current generation, a one-hour lifetime
    /// and `Mode::Reject`.
    #[cfg(test)]
    pub(crate) fn command(
        &self,
        id: u64,
        axis: SetpointAxis,
        value: f32,
    ) -> Result<Applied, GatewayError> {
        self.set_power(
            axis,
            id,
            self.site.run_generation(),
            value,
            Duration::from_secs(3600),
            Mode::Reject,
        )
    }
}

/// `value` pulled into `envelope`; 0 and a missing envelope leave it
/// alone.
fn clamp_into(value: f32, envelope: Option<VecBounds>) -> f32 {
    if value == 0.0 {
        return value;
    }
    envelope.map_or(value, |env| env.clamp(value))
}

#[cfg(test)]
mod tests {
    use super::test_stubs::{Cmd, put, sim_site};
    use super::*;
    use crate::sim::sim_clock::headless_base;

    const HOUR: Duration = Duration::from_secs(3600);

    /// An accepted setpoint reports its deadline on the site clock,
    /// and `remaining_lifetime` counts down on the same clock.
    #[test]
    fn a_command_returns_its_deadline_on_the_site_clock() {
        let (site, clock) = sim_site();
        put(&site, Cmd::new(1));
        let applied = site
            .gateway()
            .set_active_power(
                1,
                site.run_generation(),
                500.0,
                Duration::from_secs(30),
                Mode::Reject,
            )
            .unwrap();
        assert_eq!(applied.value, 500.0);
        assert_eq!(
            applied.deadline,
            headless_base() + chrono::Duration::seconds(30)
        );
        clock.advance(Duration::from_secs(10));
        assert_eq!(
            site.gateway().remaining_lifetime(1, SetpointAxis::Active),
            Some(Duration::from_secs(20))
        );
    }

    /// A request whose site was reset after its lookup is refused, so
    /// it cannot drive the new run's component.
    #[test]
    fn a_request_looked_up_before_a_reset_is_refused() {
        let site = MicrogridSite::new();
        put(&site, Cmd::new(1));
        let generation = site.run_generation();
        site.reset();
        let fresh = put(&site, Cmd::new(1));
        let err = site
            .gateway()
            .set_active_power(1, generation, 500.0, HOUR, Mode::Reject)
            .unwrap_err();
        assert_eq!(err, GatewayError::SiteReset);
        assert!(err.to_string().contains("site was reset"));
        let aug = site
            .gateway()
            .augment(
                1,
                generation,
                SetpointAxis::Active,
                VecBounds::single(-1.0, 1.0),
                HOUR,
            )
            .unwrap_err();
        assert!(matches!(aug, AugmentError::SiteReset));
        assert_eq!(
            *fresh.last.lock(),
            None,
            "the new run's component was never driven"
        );
    }

    /// The refusals carry today's wording.
    #[test]
    fn refusals_follow_the_error_table() {
        let site = MicrogridSite::new();
        put(&site, Cmd::new(1));
        site.register(crate::sim::Meter::new(
            5,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));
        let generation = site.run_generation();
        let gw = site.gateway();

        let e = gw
            .set_active_power(99, generation, 1.0, HOUR, Mode::Reject)
            .unwrap_err();
        assert_eq!(e.to_string(), "component 99 not found");

        let e = gw
            .set_active_power(5, generation, 1.0, HOUR, Mode::Reject)
            .unwrap_err();
        assert!(matches!(
            e,
            GatewayError::NoAxis {
                id: 5,
                axis: SetpointAxis::Active
            }
        ));

        let e = gw
            .set_active_power(1, generation, f32::NAN, HOUR, Mode::Reject)
            .unwrap_err();
        assert!(e.to_string().contains("non-finite"), "{e}");

        let e = gw
            .set_active_power(1, generation, 5_000.0, HOUR, Mode::Reject)
            .unwrap_err();
        assert!(matches!(e, GatewayError::OutOfEnvelope(_)));
        assert!(e.to_string().contains("out of bounds [-1000, 1000]"), "{e}");
        assert_eq!(
            gw.remaining_lifetime(1, SetpointAxis::Active),
            None,
            "a refusal arms nothing"
        );
    }

    /// `Clamp` pulls an out-of-envelope value to the edge and applies
    /// it; 0 is applied as-is.
    #[test]
    fn clamp_mode_pulls_into_the_envelope() {
        let site = MicrogridSite::new();
        let cmd = put(&site, Cmd::new(1));
        let gw = site.gateway();
        let applied = gw
            .set_active_power(1, site.run_generation(), 5_000.0, HOUR, Mode::Clamp)
            .unwrap();
        assert_eq!(applied.value, 1_000.0);
        assert_eq!(*cmd.last.lock(), Some(1_000.0));
        let applied = gw
            .set_active_power(1, site.run_generation(), 0.0, HOUR, Mode::Clamp)
            .unwrap();
        assert_eq!(applied.value, 0.0);
    }

    /// The physics step expires lifetimes at its `now`, on sim time:
    /// short of the deadline nothing resets, at it the axis does.
    #[test]
    fn step_expires_lifetimes_on_sim_time() {
        let (site, clock) = sim_site();
        let cmd = put(&site, Cmd::new(1));
        site.gateway()
            .set_active_power(
                1,
                site.run_generation(),
                500.0,
                Duration::from_secs(10),
                Mode::Reject,
            )
            .unwrap();
        clock.advance(Duration::from_secs(9));
        site.tick_once(site.now(), Duration::from_millis(100));
        assert!(cmd.resets.lock().is_empty());
        clock.advance(Duration::from_secs(1));
        site.tick_once(site.now(), Duration::from_millis(100));
        assert_eq!(*cmd.resets.lock(), vec![SetpointAxis::Active]);
        assert_eq!(
            site.gateway().remaining_lifetime(1, SetpointAxis::Active),
            None
        );
    }

    /// An explicit reset clears the lifetime and resets the axis.
    #[test]
    fn reset_clears_the_lifetime_and_the_axis() {
        let site = MicrogridSite::new();
        let cmd = put(&site, Cmd::new(1));
        site.gateway()
            .command(1, SetpointAxis::Active, 500.0)
            .unwrap();
        site.gateway().reset(1, SetpointAxis::Active);
        assert_eq!(
            site.gateway().remaining_lifetime(1, SetpointAxis::Active),
            None
        );
        assert_eq!(*cmd.resets.lock(), vec![SetpointAxis::Active]);
    }

    /// An accepted augmentation reports its deadline on the site
    /// clock.
    #[test]
    fn augment_returns_its_deadline() {
        let (site, _clock) = sim_site();
        put(&site, Cmd::new(1));
        let deadline = site
            .gateway()
            .augment(
                1,
                site.run_generation(),
                SetpointAxis::Active,
                VecBounds::single(-500.0, 500.0),
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(deadline, headless_base() + chrono::Duration::seconds(30));
    }

    /// Through the façade a command crosses the component's own
    /// delay and ramp once: a zero-delay, unramped battery inverter
    /// answers on the very next tick, as it did before the gateway.
    #[test]
    fn facade_command_reaches_the_output_in_one_tick() {
        use crate::sim::{
            Battery, BatteryInverter, battery::BatteryConfig,
            inverter::battery_inverter::BatteryInverterConfig,
        };
        let site = MicrogridSite::new();
        site.register(Battery::new(
            1,
            Duration::from_secs(1),
            BatteryConfig::default(),
        ));
        site.register(BatteryInverter::new(
            2,
            Duration::from_secs(1),
            BatteryInverterConfig::default(),
        ));
        site.connect(2, 1);
        site.gateway()
            .command(2, SetpointAxis::Active, 3_000.0)
            .unwrap();
        site.tick_n(1, Duration::from_millis(100));
        assert_eq!(site.get(2).unwrap().aggregate_power_w(&site), 3_000.0);
    }
}
