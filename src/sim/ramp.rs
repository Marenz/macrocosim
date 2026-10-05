//! Two orthogonal control-loop primitives shared by inverters, EV
//! chargers, anything else that does not respond instantly to a
//! set-point command.
//!
//! A real inverter takes some time to acknowledge a SCADA command (the
//! `CommandDelay`) and then ramps power toward the target at a slew
//! rate (the `Ramp`) — exceeding the slew rate would damage capacitors,
//! breakers, or the battery itself. Tests for both live next to the
//! implementations.

use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// Holds a pending set-point that becomes "armed" only after `delay`
/// has elapsed on the tick clock.
///
/// Models a device that takes `delay` to execute each command. A
/// command carries no clock when submitted — the first `poll` after
/// it arrives stamps it with the tick clock, and it arms one `delay`
/// later on that same clock. Submission happens on gRPC/UI threads
/// with no access to the site's clock, and stamping there with wall
/// time would break simulated/stepped clocks (a command stamped in
/// wall-2026 never looks due to a sim-2020 poll).
///
/// While one command executes, only the newest later arrival is kept
/// (a one-deep waiting slot). A controller that re-sends faster than
/// `delay` therefore trails by about one delay but always makes
/// progress — a fresh command must never restart the executing
/// command's clock, or a fast re-send cadence would starve the device
/// forever.
#[derive(Debug)]
pub struct CommandDelay {
    state: Mutex<State>,
    delay: Duration,
    /// `delay` converted once — `poll` runs every tick for every
    /// delayed component, so the conversion must not repeat per call.
    delay_chrono: chrono::Duration,
}

#[derive(Debug, Clone)]
struct State {
    /// The command being executed; arms once its due time passes.
    /// The timestamp is `None` until the first `poll` stamps it.
    executing: Option<(Option<DateTime<Utc>>, f32)>,
    /// The newest command that arrived while another was executing.
    /// Also stamped by `poll`, so it arms at its own stamp + delay —
    /// commands pipeline; they are not serialized one per delay.
    waiting: Option<(Option<DateTime<Utc>>, f32)>,
    armed: Option<f32>,
}

impl State {
    /// Stamp unstamped commands with the tick clock, then arm the
    /// executing command if its execution has finished by `now`.
    fn promote(&mut self, now: DateTime<Utc>, delay: chrono::Duration) {
        if let Some((stamp @ None, _)) = &mut self.executing {
            *stamp = Some(now);
        }
        if let Some((stamp @ None, _)) = &mut self.waiting {
            *stamp = Some(now);
        }
        // Arm at most one command per poll: a device applies commands
        // one at a time, so a burst never collapses into "only the
        // newest value was ever visible".
        if let Some((Some(set_at), v)) = self.executing
            // checked add: a saturated "forever" delay must mean the
            // command never arms, not a panic on overflow. A stamp
            // after `now` means the wall clock stepped back; it counts
            // as due, so the command doesn't wait for the clock to
            // catch up.
            && (set_at > now || set_at.checked_add_signed(delay).is_some_and(|due| now >= due))
        {
            self.armed = Some(v);
            self.executing = self.waiting.take();
        }
    }
}

impl CommandDelay {
    pub fn new(delay: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                executing: None,
                waiting: None,
                armed: None,
            }),
            delay,
            // Saturate UP on overflow: an absurd :command-delay
            // means commands never arm — falling back to zero
            // armed them immediately instead.
            delay_chrono: chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX),
        }
    }

    pub fn set_target(&self, value: f32) {
        let mut s = self.state.lock();
        if self.delay.is_zero() {
            s.armed = Some(value);
            s.executing = None;
            s.waiting = None;
            return;
        }
        if s.executing.is_none() {
            s.executing = Some((None, value));
        } else {
            s.waiting = Some((None, value));
        }
    }

    /// Stamp and promote finished commands, then return the currently
    /// armed value (None until the first command finishes executing).
    pub fn poll(&self, now: DateTime<Utc>) -> Option<f32> {
        let mut s = self.state.lock();
        s.promote(now, self.delay_chrono);
        s.armed
    }

    pub fn reset(&self) {
        let mut s = self.state.lock();
        s.armed = None;
        s.executing = None;
        s.waiting = None;
    }

    /// Inspect the armed value without advancing the delay clock.
    pub fn armed(&self) -> Option<f32> {
        self.state.lock().armed
    }
}

/// Slew-rate-limited tracker: `actual` moves toward `target` at most
/// `rate_w_per_s` per second.
///
/// Use `rate = f32::INFINITY` to make the tracker pass-through (the
/// behaviour of microsim's inverters today).
#[derive(Debug)]
pub struct Ramp {
    state: Mutex<RampState>,
    rate_w_per_s: f32,
}

#[derive(Debug, Clone)]
struct RampState {
    actual: f32,
    target: f32,
}

impl Ramp {
    pub fn new(rate_w_per_s: f32, initial: f32) -> Self {
        Self {
            state: Mutex::new(RampState {
                actual: initial,
                target: initial,
            }),
            rate_w_per_s,
        }
    }

    pub fn set_target(&self, target: f32) {
        // NaN propagating through the slew math poisons `actual`
        // permanently; reject it at the door. ±∞ is left through —
        // a target of f32::INFINITY combined with a finite rate still
        // gives a well-defined per-tick step.
        if target.is_nan() {
            log::warn!("Ramp::set_target ignored NaN");
            return;
        }
        self.state.lock().target = target;
    }

    pub fn snap_to(&self, value: f32) {
        // Same hazard as `set_target`: a NaN here poisons `actual`
        // permanently, since every later slew step propagates it.
        if value.is_nan() {
            log::warn!("Ramp::snap_to ignored NaN");
            return;
        }
        let mut s = self.state.lock();
        s.target = value;
        s.actual = value;
    }

    /// Move `actual` without touching the target, for an output a
    /// physical limit has cut short of where the slew put it.
    pub fn set_actual(&self, value: f32) {
        if value.is_nan() {
            log::warn!("Ramp::set_actual ignored NaN");
            return;
        }
        self.state.lock().actual = value;
    }

    pub fn actual(&self) -> f32 {
        self.state.lock().actual
    }

    pub fn target(&self) -> f32 {
        self.state.lock().target
    }

    /// Advance `actual` by the most it is allowed to move in `dt`.
    pub fn advance(&self, dt: Duration) -> f32 {
        let mut s = self.state.lock();
        if !self.rate_w_per_s.is_finite() {
            s.actual = s.target;
            return s.actual;
        }
        let max_step = self.rate_w_per_s * dt.as_secs_f32();
        let diff = s.target - s.actual;
        if diff.abs() <= max_step {
            s.actual = s.target;
        } else {
            s.actual += diff.signum() * max_step;
        }
        s.actual
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_delay_zero_arms_immediately() {
        let cd = CommandDelay::new(Duration::ZERO);
        cd.set_target(5000.0);
        assert_eq!(cd.armed(), Some(5000.0));
    }

    #[test]
    fn command_delay_blocks_until_due() {
        let t0 = Utc::now();
        let cd = CommandDelay::new(Duration::from_secs(2));
        cd.set_target(5000.0);
        assert_eq!(cd.poll(t0), None); // first poll stamps the command
        assert_eq!(cd.poll(t0 + chrono::Duration::seconds(1)), None);
        assert_eq!(cd.poll(t0 + chrono::Duration::seconds(2)), Some(5000.0));
    }

    /// Commands carry no clock when submitted: the tick clock stamps
    /// them. A simulated clock far from wall time still arms them.
    #[test]
    fn command_delay_follows_the_tick_clock() {
        let sim0 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2020, 1, 1, 0, 0, 0).unwrap();
        let cd = CommandDelay::new(Duration::from_secs(1));
        cd.set_target(500.0);
        assert_eq!(cd.poll(sim0), None);
        assert_eq!(cd.poll(sim0 + chrono::Duration::seconds(1)), Some(500.0));
    }

    /// A controller re-sending faster than the delay must not starve
    /// the device: the executing command keeps its own due time, so
    /// commands keep arming even under a continuous fast stream.
    #[test]
    fn command_delay_survives_fast_resend_cadence() {
        let t0 = Utc::now();
        let cd = CommandDelay::new(Duration::from_millis(1500));
        // One command every 500 ms, values ramping 1000, 2000, …,
        // with the tick clock polling every 100 ms like the physics
        // loop.
        let mut armed = None;
        for tick in 0..46 {
            if tick % 5 == 0 {
                cd.set_target((tick / 5 + 1) as f32 * 1000.0);
            }
            armed = cd.poll(t0 + chrono::Duration::milliseconds(100 * tick));
        }
        // At t0+4.5s the stream has been running for 9+ commands; the
        // device must have armed several of them by now, not none.
        assert!(armed.is_some(), "fast re-sends starved the device");
        // And it keeps progressing: the newest command arms within a
        // couple more polls (one command arms per poll).
        cd.poll(t0 + chrono::Duration::seconds(60));
        let settled = cd.poll(t0 + chrono::Duration::seconds(61));
        assert_eq!(settled, Some(10_000.0));
    }

    /// A wall clock that steps back (an NTP step) leaves the
    /// executing command stamped after `now`. It counts as due, so
    /// the device keeps arming commands instead of stalling until the
    /// clock catches up.
    #[test]
    fn command_delay_survives_a_backward_clock_step() {
        let t0 = Utc::now();
        let cd = CommandDelay::new(Duration::from_secs(1));
        cd.set_target(1000.0);
        assert_eq!(cd.poll(t0), None);
        cd.set_target(2000.0);
        assert_eq!(cd.poll(t0 + chrono::Duration::milliseconds(500)), None);

        let back = t0 - chrono::Duration::seconds(10);
        let at = |m| back + chrono::Duration::milliseconds(m);
        assert_eq!(cd.poll(back), Some(1000.0), "the stranded command is due");
        cd.set_target(3000.0);
        assert_eq!(
            cd.poll(at(100)),
            Some(2000.0),
            "so is the next stranded one"
        );
        assert_eq!(cd.poll(at(500)), Some(2000.0), "3000 waits its delay");
        assert_eq!(cd.poll(at(1100)), Some(3000.0));
    }

    /// While one command executes, only the newest waiting command
    /// survives — intermediate values are superseded, not queued.
    #[test]
    fn command_delay_newest_waiting_command_wins() {
        let t0 = Utc::now();
        let cd = CommandDelay::new(Duration::from_secs(2));
        cd.set_target(1000.0);
        assert_eq!(cd.poll(t0), None); // stamps 1000 at t0
        cd.set_target(2000.0);
        cd.set_target(3000.0); // replaces 2000 in the waiting slot
        // Stamps the waiting 3000 at t0+0.4s.
        assert_eq!(cd.poll(t0 + chrono::Duration::milliseconds(400)), None);
        // 1000 arms at its own due time, t0+2s; 3000 at t0+2.4s —
        // its own stamp plus the delay. 2000 never arms.
        assert_eq!(cd.poll(t0 + chrono::Duration::seconds(2)), Some(1000.0));
        assert_eq!(
            cd.poll(t0 + chrono::Duration::milliseconds(2300)),
            Some(1000.0)
        );
        assert_eq!(
            cd.poll(t0 + chrono::Duration::milliseconds(2400)),
            Some(3000.0)
        );
    }

    #[test]
    fn ramp_step_limit() {
        let r = Ramp::new(1000.0, 0.0);
        r.set_target(5000.0);
        assert_eq!(r.advance(Duration::from_secs(1)), 1000.0);
        assert_eq!(r.advance(Duration::from_secs(1)), 2000.0);
        // Big jump → step caps it
        assert_eq!(r.advance(Duration::from_secs(2)), 4000.0);
    }

    #[test]
    fn ramp_pass_through_when_infinite() {
        let r = Ramp::new(f32::INFINITY, 0.0);
        r.set_target(5000.0);
        assert_eq!(r.advance(Duration::from_millis(1)), 5000.0);
    }

    #[test]
    fn ramp_ignores_nan_target() {
        let r = Ramp::new(1000.0, 0.0);
        r.set_target(5000.0);
        r.advance(Duration::from_secs(1)); // → 1000
        r.set_target(f32::NAN); // no-op, target stays at 5000
        let v = r.advance(Duration::from_secs(1));
        assert!(
            v.is_finite() && (v - 2000.0).abs() < 1e-3,
            "expected 2000, got {v}"
        );
    }
}
