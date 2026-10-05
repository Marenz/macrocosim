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

use std::{collections::HashMap, fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    AugmentError, MicrogridSite, SetpointError, SimulatedComponent,
    bounds::VecBounds,
    gateway_axis::{GatewayAxis, GatewayAxisConfig},
};
use crate::timeout_tracker::{SetpointAxis, TimeoutTracker, deadline_after};

mod step;
#[cfg(test)]
mod test_stubs;
mod window;

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
    /// One axis per controllable component and axis.
    axes: HashMap<(u64, SetpointAxis), GatewayAxis>,
}

impl GatewayState {
    /// Drop everything held for `id`.
    fn forget(&mut self, id: u64) {
        self.lifetimes.remove_component(id);
        self.axes.retain(|(cid, _), _| *cid != id);
    }
}

impl MicrogridGateway {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop every command, lifetime and augmentation (a site reset).
    pub(crate) fn clear(&self) {
        let mut st = self.state.lock();
        st.lifetimes.clear();
        st.axes.clear();
    }

    /// Set up a component that has just been (re-)registered: drop
    /// whatever an earlier occupant of its id left, then create an
    /// axis for every axis it takes a command on, its ramp starting
    /// at the component's initial value.
    pub(crate) fn on_register(&self, c: &dyn SimulatedComponent) {
        let id = c.id();
        let mut st = self.state.lock();
        st.forget(id);
        let settings = c.gateway_settings().unwrap_or_default();
        for axis in [SetpointAxis::Active, SetpointAxis::Reactive] {
            if c.has_axis(axis) {
                st.axes.insert(
                    (id, axis),
                    GatewayAxis::new(GatewayAxisConfig {
                        command_delay: settings.delay(axis),
                        ramp_rate_per_s: settings.ramp_rate(axis),
                        initial: c.initial_value(axis),
                        unit: axis.unit(),
                    }),
                );
            }
        }
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
                self.gate_locked(&st, id, axis, value)?;
                value
            }
            Mode::Clamp => clamp_into(value, self.setpoint_envelope_locked(&st, id, axis)),
        };
        let now = self.site.now();
        if let Some(ax) = st.axes.get(&(id, axis)) {
            let base = self.base_of(c.as_ref(), axis);
            ax.check(value, &base, now)
                .map_err(|e| GatewayError::from_setpoint(id, axis, e))?;
            let deadline = st.lifetimes.actuate_and_arm(id, axis, now, lifetime, || {
                ax.accept(value);
                Ok::<(), GatewayError>(())
            })?;
            return Ok(Applied { value, deadline });
        }
        let deadline = st
            .lifetimes
            .actuate_and_arm(id, axis, now, lifetime, || match axis {
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
        let mut st = self.gw.state.lock();
        if self.site.run_generation() != generation {
            return Err(AugmentError::SiteReset);
        }
        let c = self.site.get(id).ok_or(AugmentError::NotFound(id))?;
        let now = self.site.now();
        match st.axes.get_mut(&(id, axis)) {
            Some(ax) => {
                bounds
                    .check_augmentation_shape()
                    .map_err(AugmentError::Malformed)?;
                let base = self.base_of(c.as_ref(), axis);
                let physical = if c.augment_checks_physical_band(axis) {
                    c.physical_band(axis, self.site.physics_tick())
                } else {
                    None
                };
                ax.try_augment(&base, physical.as_ref(), now, bounds, lifetime)
                    .map_err(AugmentError::Disjoint)?;
            }
            None => c.try_augment_bounds(axis, now, bounds, lifetime)?,
        }
        Ok(deadline_after(now, lifetime))
    }

    /// Clear `id`'s `axis` command and lifetime and park the axis.
    pub fn reset(&self, id: u64, axis: SetpointAxis) {
        let st = self.gw.state.lock();
        st.lifetimes.remove(id, axis);
        let Some(c) = self.site.get(id) else {
            return;
        };
        match st.axes.get(&(id, axis)) {
            Some(ax) => ax.reset(c.park_value(axis)),
            None => c.reset_setpoint_axis(axis),
        }
    }

    /// Time left on the site clock before `id`'s `axis` command
    /// expires.
    pub fn remaining_lifetime(&self, id: u64, axis: SetpointAxis) -> Option<Duration> {
        let st = self.gw.state.lock();
        st.lifetimes.remaining(id, axis, self.site.now())
    }

    /// Expire every lifetime at or before `now` and reset its axis.
    pub(crate) fn expire(&self, now: DateTime<Utc>) {
        let st = self.gw.state.lock();
        let expired = self.expire_locked(&st, now);
        drop(st);
        log_expired(&expired);
    }

    /// Expire every lifetime at or before `now`: an owned axis ramps to
    /// its park value, a façade one is reset through the component.
    /// Returns what expired, for logging once the lock is released.
    fn expire_locked(&self, st: &GatewayState, now: DateTime<Utc>) -> Vec<(u64, SetpointAxis)> {
        let mut expired = Vec::new();
        st.lifetimes.reset_expired_with(now, |id, axis| {
            if let Some(c) = self.site.get(id) {
                match st.axes.get(&(id, axis)) {
                    Some(ax) => ax.reset(c.park_value(axis)),
                    None => c.reset_setpoint_axis(axis),
                }
            }
            expired.push((id, axis));
        });
        expired
    }

    /// What every consumer reports as `id`'s bounds on `axis`; `None`
    /// for a component with none on that axis.
    pub fn bounds_of(&self, id: u64, axis: SetpointAxis) -> Option<VecBounds> {
        let st = self.gw.state.lock();
        self.bounds_of_locked(&st, id, axis)
    }

    /// The envelope a setpoint for `id` on `axis` must respect: its
    /// own bounds ∩ the summed bounds of every child that reports
    /// them (each divided by its parent count), or its own bounds
    /// alone when no child reports any.
    pub fn setpoint_envelope(&self, id: u64, axis: SetpointAxis) -> Option<VecBounds> {
        let st = self.gw.state.lock();
        self.setpoint_envelope_locked(&st, id, axis)
    }

    /// The summed bounds of `id`'s children on `axis`, each divided by
    /// its parent count; `None` when no child reports any.
    pub fn child_envelope(&self, id: u64, axis: SetpointAxis) -> Option<VecBounds> {
        let st = self.gw.state.lock();
        self.child_envelope_locked(&st, id, axis)
    }

    /// True while a live augmentation narrows `id`'s `axis`.
    pub fn augmented(&self, id: u64, axis: SetpointAxis) -> bool {
        let st = self.gw.state.lock();
        let now = self.site.now();
        match st.axes.get(&(id, axis)) {
            Some(ax) => ax.augmented(now),
            None => self
                .site
                .get(id)
                .is_some_and(|c| c.augmentation_active(axis, now)),
        }
    }

    fn bounds_of_locked(
        &self,
        st: &GatewayState,
        id: u64,
        axis: SetpointAxis,
    ) -> Option<VecBounds> {
        let c = self.site.get(id)?;
        if let Some(ax) = st.axes.get(&(id, axis)) {
            let env = ax.validation_envelope(&self.base_of(c.as_ref(), axis), self.site.now());
            return Some(match axis {
                SetpointAxis::Active if c.advertises_physical_band(axis) => {
                    match c.physical_band(axis, self.site.physics_tick()) {
                        Some(p) => env.intersect(&p),
                        None => env,
                    }
                }
                SetpointAxis::Active => env,
                // Zero headroom is a present (0, 0) band to a reader,
                // not an absent one.
                SetpointAxis::Reactive => env.or_zero_band(),
            });
        }
        match axis {
            SetpointAxis::Active => c.effective_active_bounds(),
            SetpointAxis::Reactive => c.reactive_bounds(),
        }
    }

    /// The static shape `c` supplies for `axis`: its rated band, or
    /// its reactive capability at its live P (the last tick's
    /// measured active power).
    fn base_of(&self, c: &dyn SimulatedComponent, axis: SetpointAxis) -> VecBounds {
        match axis {
            SetpointAxis::Active => c
                .rated_active_bounds()
                .map(|(lo, hi)| VecBounds::single(lo, hi))
                .unwrap_or_default(),
            SetpointAxis::Reactive => c
                .reactive_capability()
                .map(|cap| cap.q_band_at(c.active_power_w(self.site).unwrap_or(0.0)))
                .unwrap_or_default(),
        }
    }

    fn child_envelope_locked(
        &self,
        st: &GatewayState,
        id: u64,
        axis: SetpointAxis,
    ) -> Option<VecBounds> {
        self.site
            .sum_child_bounds(id, |child| self.bounds_of_locked(st, child.id(), axis))
    }

    /// Own ∩ children; `None` when no child reports bounds.
    fn combined_envelope_locked(
        &self,
        st: &GatewayState,
        id: u64,
        axis: SetpointAxis,
    ) -> Option<VecBounds> {
        let children = self.child_envelope_locked(st, id, axis)?;
        Some(match self.bounds_of_locked(st, id, axis) {
            Some(own) => own.intersect(&children),
            None => children,
        })
    }

    fn setpoint_envelope_locked(
        &self,
        st: &GatewayState,
        id: u64,
        axis: SetpointAxis,
    ) -> Option<VecBounds> {
        self.combined_envelope_locked(st, id, axis)
            .or_else(|| self.bounds_of_locked(st, id, axis))
    }

    /// The children gate: 0 always passes; any other value must sit
    /// inside the combined envelope when there is one.
    fn gate_locked(
        &self,
        st: &GatewayState,
        id: u64,
        axis: SetpointAxis,
        value: f32,
    ) -> Result<(), GatewayError> {
        if value == 0.0 {
            return Ok(());
        }
        if let Some(env) = self.combined_envelope_locked(st, id, axis)
            && !env.contains(value)
        {
            return Err(GatewayError::OutOfEnvelope(format!(
                "set-point {value} {} exceeds combined envelope {env}",
                axis.unit()
            )));
        }
        Ok(())
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

/// One info line per expired request.
fn log_expired(expired: &[(u64, SetpointAxis)]) {
    for (id, axis) in expired {
        log::info!("Request timeout for component {id} ({axis:?}) — resetting that axis");
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
    use std::sync::Arc;

    use super::test_stubs::{Cmd, Hw, put, sim_site};
    use super::*;
    use crate::sim::{component::GatewaySettings, sim_clock::headless_base};

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

    /// A zero-delay, unramped battery inverter answers a gateway
    /// command on the very next tick: the command crosses each delay
    /// once.
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
            BatteryInverterConfig {
                device_delay: Duration::ZERO,
                ..Default::default()
            },
        ));
        site.connect(2, 1);
        site.gateway()
            .command(2, SetpointAxis::Active, 3_000.0)
            .unwrap();
        site.tick_n(1, Duration::from_millis(100));
        assert_eq!(site.get(2).unwrap().aggregate_power_w(&site), 3_000.0);
    }

    /// A battery behind a battery inverter, both ±`bat_w` / ±5 kW, no
    /// delays, PF cap off and a 5 kVA cap so Q is ±5 kVAr at idle.
    fn inverter_over_battery(bat_w: f32) -> MicrogridSite {
        use crate::sim::{
            Battery, BatteryInverter, battery::BatteryConfig,
            inverter::battery_inverter::BatteryInverterConfig, reactive::ReactiveCapability,
        };
        let site = MicrogridSite::new();
        site.register(Battery::new(
            1,
            Duration::from_secs(1),
            BatteryConfig {
                rated_lower_w: -bat_w,
                rated_upper_w: bat_w,
                soc_protect_margin_pct: 0.0,
                ..Default::default()
            },
        ));
        site.register(BatteryInverter::new(
            2,
            Duration::from_secs(1),
            BatteryInverterConfig {
                rated_lower_w: -5_000.0,
                rated_upper_w: 5_000.0,
                reactive: ReactiveCapability {
                    pf_limit: None,
                    apparent_va: Some(5_000.0),
                },
                ..Default::default()
            },
        ));
        site.connect(2, 1);
        site
    }

    /// Own bounds, children's sum, and the setpoint envelope built
    /// from them, on both axes.
    #[test]
    fn reads_compose_own_and_child_bounds() {
        let site = inverter_over_battery(30_000.0);
        let gw = site.gateway();
        gw.augment(
            2,
            site.run_generation(),
            SetpointAxis::Active,
            VecBounds::single(-3_000.0, 3_000.0),
            HOUR,
        )
        .unwrap();
        assert_eq!(
            gw.bounds_of(2, SetpointAxis::Active).unwrap().to_string(),
            "[-3000, 3000]"
        );
        assert_eq!(
            gw.child_envelope(2, SetpointAxis::Active)
                .unwrap()
                .to_string(),
            "[-30000, 30000]"
        );
        assert_eq!(
            gw.setpoint_envelope(2, SetpointAxis::Active)
                .unwrap()
                .to_string(),
            "[-3000, 3000]"
        );
        assert!(gw.augmented(2, SetpointAxis::Active));
        assert!(!gw.augmented(2, SetpointAxis::Reactive));

        assert_eq!(
            gw.bounds_of(2, SetpointAxis::Reactive).unwrap().to_string(),
            "[-5000, 5000]"
        );
        assert!(
            gw.child_envelope(2, SetpointAxis::Reactive).is_none(),
            "a battery reports no Q"
        );
        assert_eq!(
            gw.setpoint_envelope(2, SetpointAxis::Reactive)
                .unwrap()
                .to_string(),
            "[-5000, 5000]",
            "with no Q-reporting child the envelope is the component's own"
        );
        assert_eq!(
            gw.bounds_of(1, SetpointAxis::Active).unwrap().to_string(),
            "[-30000, 30000]"
        );
        assert!(gw.bounds_of(1, SetpointAxis::Reactive).is_none());
        assert!(gw.bounds_of(99, SetpointAxis::Active).is_none());
    }

    /// An owned reactive axis validates against, and reports, the
    /// capability at the inverter's measured P from its last tick; an
    /// envelope a Q augmentation leaves empty reads as `(0, 0)`.
    #[test]
    fn an_owned_reactive_axis_follows_the_capability_at_measured_p() {
        let site = inverter_over_battery(30_000.0);
        let gw = site.gateway();
        let q = SetpointAxis::Reactive;
        assert_eq!(gw.bounds_of(2, q).unwrap().to_string(), "[-5000, 5000]");
        gw.command(2, SetpointAxis::Active, 3_000.0).unwrap();
        assert_eq!(
            gw.bounds_of(2, q).unwrap().to_string(),
            "[-5000, 5000]",
            "a command moves nothing before the inverter measures it"
        );
        site.tick_n(3, Duration::from_millis(100));
        assert_eq!(site.get(2).unwrap().active_power_w(&site), Some(3_000.0));
        // 5 kVA at 3 kW leaves 4 kVAr.
        assert_eq!(gw.bounds_of(2, q).unwrap().to_string(), "[-4000, 4000]");
        let e = gw.command(2, q, 4_500.0).unwrap_err();
        assert!(e.to_string().contains("out of bounds [-4000, 4000]"), "{e}");
        assert!(gw.command(2, q, 3_500.0).is_ok());

        gw.augment(
            2,
            site.run_generation(),
            q,
            VecBounds::single(3_000.0, 3_900.0),
            HOUR,
        )
        .unwrap();
        gw.command(2, SetpointAxis::Active, 4_800.0).unwrap();
        site.tick_n(3, Duration::from_millis(100));
        // 5 kVA at 4.8 kW leaves ±1.4 kVAr, disjoint from [3000, 3900].
        let band = gw.bounds_of(2, q).unwrap();
        assert_eq!(band.to_string(), "[0, 0]");
        assert_eq!(band.0.len(), 1, "a present zero band, not an absent one");
    }

    /// The children gate names the combined envelope; `Clamp` uses it.
    #[test]
    fn the_children_gate_names_the_combined_envelope() {
        let site = inverter_over_battery(1_000.0);
        let gw = site.gateway();
        let e = gw.command(2, SetpointAxis::Active, 3_000.0).unwrap_err();
        assert!(
            e.to_string()
                .contains("set-point 3000 W exceeds combined envelope [-1000, 1000]"),
            "{e}"
        );
        let applied = gw
            .set_active_power(2, site.run_generation(), 3_000.0, HOUR, Mode::Clamp)
            .unwrap();
        assert_eq!(applied.value, 1_000.0);
        assert!(
            gw.command(2, SetpointAxis::Active, 0.0).is_ok(),
            "0 W always passes"
        );
    }

    /// The telemetry overlay replaces bounds with `bounds_of` on a
    /// snapshot that carries a power value, and leaves one without
    /// power (the grid) alone.
    #[test]
    fn telemetry_of_overlays_bounds_on_power_carrying_snapshots() {
        let site = inverter_over_battery(30_000.0);
        site.register(crate::sim::Grid::new(
            9,
            100,
            Some((-50_000.0, 50_000.0)),
            0.0,
        ));
        site.gateway()
            .augment(
                2,
                site.run_generation(),
                SetpointAxis::Active,
                VecBounds::single(-2_000.0, 2_000.0),
                HOUR,
            )
            .unwrap();
        let inv = site.get(2).unwrap();
        let t = site.telemetry_of(inv.as_ref());
        assert_eq!(t.active_power_bounds.unwrap().to_string(), "[-2000, 2000]");
        assert_eq!(
            t.reactive_power_bounds.unwrap().to_string(),
            "[-5000, 5000]"
        );
        let bat = site.telemetry_of(site.get(1).unwrap().as_ref());
        assert_eq!(
            bat.active_power_bounds.unwrap().to_string(),
            "[-30000, 30000]"
        );
        let grid = site.telemetry_of(site.get(9).unwrap().as_ref());
        assert!(grid.active_power_bounds.is_none());
    }

    /// An owned axis validates against rated ∩ augmentations, and the
    /// gateway, not the component, holds the command until a step.
    #[test]
    fn an_owned_axis_validates_against_rated_and_augmentations() {
        let site = MicrogridSite::new();
        let hw = put(&site, Arc::new(Hw::new(1)));
        let gw = site.gateway();
        let g = site.run_generation();
        assert!(gw.command(1, SetpointAxis::Active, 500.0).is_ok());
        assert_eq!(hw.last(), None, "the command waits for the step");
        let e = gw.command(1, SetpointAxis::Active, 5_000.0).unwrap_err();
        assert!(e.to_string().contains("out of bounds [-1000, 1000]"), "{e}");
        gw.augment(
            1,
            g,
            SetpointAxis::Active,
            VecBounds::single(0.0, 300.0),
            HOUR,
        )
        .unwrap();
        assert!(gw.command(1, SetpointAxis::Active, 500.0).is_err());
        assert_eq!(
            gw.bounds_of(1, SetpointAxis::Active).unwrap().to_string(),
            "[0, 300]"
        );
        assert!(gw.augmented(1, SetpointAxis::Active));
        let e = gw.command(1, SetpointAxis::Reactive, 10.0).unwrap_err();
        assert!(matches!(e, GatewayError::NoAxis { .. }));
        let e = gw
            .augment(
                1,
                g,
                SetpointAxis::Reactive,
                VecBounds::single(-1.0, 1.0),
                HOUR,
            )
            .unwrap_err();
        assert!(matches!(e, AugmentError::Unsupported));
    }

    #[test]
    fn clamp_on_an_owned_axis_pulls_into_the_envelope() {
        let site = MicrogridSite::new();
        put(&site, Arc::new(Hw::new(1)));
        let applied = site
            .gateway()
            .set_active_power(1, site.run_generation(), 5_000.0, HOUR, Mode::Clamp)
            .unwrap();
        assert_eq!(applied.value, 1_000.0);
    }

    /// A reset on an owned axis clears the lifetime and ramps the
    /// axis toward the component's park value.
    #[test]
    fn reset_on_an_owned_axis_clears_the_lifetime() {
        let site = MicrogridSite::new();
        let hw = put(
            &site,
            Arc::new(Hw {
                park: -100.0,
                settings: GatewaySettings {
                    ramp_rate_w_per_s: 1_000.0,
                    ..GatewaySettings::default()
                },
                ..Hw::new(1)
            }),
        );
        let dt = Duration::from_millis(100);
        site.gateway()
            .command(1, SetpointAxis::Active, 500.0)
            .unwrap();
        site.tick_n(10, dt);
        assert_eq!(hw.last(), Some(500.0));
        site.gateway().reset(1, SetpointAxis::Active);
        assert_eq!(
            site.gateway().remaining_lifetime(1, SetpointAxis::Active),
            None
        );
        site.tick_n(1, dt);
        assert_eq!(hw.last(), Some(400.0), "heads down at 1 kW/s");
        site.tick_n(10, dt);
        assert_eq!(hw.last(), Some(-100.0), "settles on the park value");
    }

    /// The augmentation emptiness check includes the physical band
    /// only for a component that asks for it.
    #[test]
    fn augment_checks_the_physical_band_only_when_asked() {
        let site = MicrogridSite::new();
        let pv = put(&site, Arc::new(Hw::new(1)));
        *pv.physical.lock() = Some(VecBounds::single(0.0, 200.0));
        let boiler = put(
            &site,
            Arc::new(Hw {
                checks_physical: true,
                ..Hw::new(2)
            }),
        );
        *boiler.physical.lock() = Some(VecBounds::single(0.0, 200.0));
        let gw = site.gateway();
        let g = site.run_generation();
        assert!(
            gw.augment(
                1,
                g,
                SetpointAxis::Active,
                VecBounds::single(500.0, 900.0),
                HOUR
            )
            .is_ok()
        );
        let e = gw
            .augment(
                2,
                g,
                SetpointAxis::Active,
                VecBounds::single(500.0, 900.0),
                HOUR,
            )
            .unwrap_err();
        assert!(e.to_string().contains("current envelope [0, 200]"), "{e}");
    }

    /// `bounds_of` includes the physical band only where the
    /// component advertises it.
    #[test]
    fn bounds_of_includes_the_physical_band_only_when_advertised() {
        let site = MicrogridSite::new();
        let quiet = put(&site, Arc::new(Hw::new(1)));
        *quiet.physical.lock() = Some(VecBounds::single(0.0, 200.0));
        let loud = put(
            &site,
            Arc::new(Hw {
                advertises: true,
                ..Hw::new(2)
            }),
        );
        *loud.physical.lock() = Some(VecBounds::single(0.0, 200.0));
        assert_eq!(
            site.bounds_of(1, SetpointAxis::Active).unwrap().to_string(),
            "[-1000, 1000]"
        );
        assert_eq!(
            site.bounds_of(2, SetpointAxis::Active).unwrap().to_string(),
            "[0, 200]"
        );
    }

    /// Two threads racing mutually disjoint augmentations: exactly
    /// one wins and the envelope never goes empty.
    #[test]
    fn concurrent_disjoint_augments_leave_exactly_one_winner() {
        for i in 0..50 {
            let site = MicrogridSite::new();
            put(&site, Arc::new(Hw::new(1)));
            let g = site.run_generation();
            let (s1, s2) = (site.clone(), site.clone());
            let h1 = std::thread::spawn(move || {
                s1.gateway()
                    .augment(
                        1,
                        g,
                        SetpointAxis::Active,
                        VecBounds::single(500.0, 800.0),
                        HOUR,
                    )
                    .is_ok()
            });
            let h2 = std::thread::spawn(move || {
                s2.gateway()
                    .augment(
                        1,
                        g,
                        SetpointAxis::Active,
                        VecBounds::single(-800.0, -500.0),
                        HOUR,
                    )
                    .is_ok()
            });
            let wins = u8::from(h1.join().unwrap()) + u8::from(h2.join().unwrap());
            assert_eq!(wins, 1, "iteration {i}");
            let b = site.bounds_of(1, SetpointAxis::Active).unwrap();
            assert_eq!(b.0.len(), 1, "iteration {i}: {b}");
        }
    }

    /// A component re-registered under the same id starts with fresh
    /// axes: no augmentation and no lifetime carried over.
    #[test]
    fn a_reregistered_component_starts_with_fresh_axes() {
        let site = MicrogridSite::new();
        put(&site, Arc::new(Hw::new(1)));
        let gw = site.gateway();
        gw.command(1, SetpointAxis::Active, 500.0).unwrap();
        gw.augment(
            1,
            site.run_generation(),
            SetpointAxis::Active,
            VecBounds::single(0.0, 600.0),
            HOUR,
        )
        .unwrap();
        put(&site, Arc::new(Hw::new(1)));
        assert!(!gw.augmented(1, SetpointAxis::Active));
        assert_eq!(gw.remaining_lifetime(1, SetpointAxis::Active), None);
    }
}
