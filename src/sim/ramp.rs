//! The two primitives behind a gateway axis (`GatewayAxis`): the
//! `CommandDelay`, the API overhead between accepting a command and
//! acting on it, and the `Ramp`, which slews the axis toward its
//! target at a set rate. A component's own response time is its
//! `DeviceAxis`. `is_due` and `TICK_JITTER`, the release rule both
//! delay lines share, live here too. Tests live next to the
//! implementations.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// The most a tick may land early and still release a command that is
/// due, bounded by half the delay so a short delay still waits a
/// tick. The live physics timer jitters by about a millisecond; 5 ms
/// matches tokio's threshold for a late tick.
pub(crate) const TICK_JITTER: chrono::Duration = chrono::Duration::milliseconds(5);

/// Whether a command stamped at `stamp` is due at `now` after
/// `delay`, allowing for a tick up to the smaller of `TICK_JITTER`
/// and half the delay early. A stamp after `now` means the clock
/// stepped back; it counts as due, so the command doesn't wait for
/// the clock to catch up, even under a saturated "forever" delay.
/// Otherwise a due time past the end of time is never reached, so
/// such a delay never releases.
pub(crate) fn is_due(stamp: DateTime<Utc>, delay: chrono::Duration, now: DateTime<Utc>) -> bool {
    let allowance = TICK_JITTER.min(delay / 2);
    stamp > now
        || stamp
            .checked_add_signed(delay - allowance)
            .is_some_and(|due| due <= now)
}

/// Holds a pending set-point that becomes "armed" only after `delay`
/// has elapsed on the tick clock.
///
/// The gateway axis's command delay: each accepted command takes
/// `delay` before the axis acts on it. A command carries no clock
/// when submitted — the first `poll` after it arrives stamps it with
/// the tick clock, and it arms one `delay` later on that same clock,
/// so its stamp and its due time always come from one clock.
///
/// While one command executes, only the newest later arrival is kept
/// (a one-deep waiting slot). A controller that re-sends faster than
/// `delay` therefore trails by about one delay but always makes
/// progress — a fresh command must never restart the executing
/// command's clock, or a fast re-send cadence would starve the axis
/// forever.
#[derive(Debug)]
pub struct CommandDelay {
    state: State,
    delay: Duration,
    /// `delay` converted once — `poll` runs every tick for every
    /// gateway axis, so the conversion must not repeat per call.
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
        // Arm at most one command per poll: the axis applies commands
        // one at a time, so a burst never collapses into "only the
        // newest value was ever visible".
        if let Some((Some(set_at), v)) = self.executing
            && is_due(set_at, delay, now)
        {
            self.armed = Some(v);
            self.executing = self.waiting.take();
        }
    }
}

impl CommandDelay {
    pub fn new(delay: Duration) -> Self {
        Self {
            state: State {
                executing: None,
                waiting: None,
                armed: None,
            },
            delay,
            // Saturate UP on overflow: an absurd :command-delay
            // means commands never arm — falling back to zero
            // armed them immediately instead.
            delay_chrono: chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX),
        }
    }

    pub fn set_target(&mut self, value: f32) {
        let s = &mut self.state;
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
    pub fn poll(&mut self, now: DateTime<Utc>) -> Option<f32> {
        self.state.promote(now, self.delay_chrono);
        self.state.armed
    }

    pub fn reset(&mut self) {
        let s = &mut self.state;
        s.armed = None;
        s.executing = None;
        s.waiting = None;
    }

    /// Inspect the armed value without advancing the delay clock.
    pub fn armed(&self) -> Option<f32> {
        self.state.armed
    }
}

/// Slew-rate-limited tracker: `actual` moves toward `target` at most
/// `rate_w_per_s` per second.
///
/// Use `rate = f32::INFINITY` to make the tracker pass-through, the
/// default for an axis whose component sets no ramp
/// (`GatewaySettings::default`).
#[derive(Debug)]
pub struct Ramp {
    state: RampState,
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
            state: RampState {
                actual: initial,
                target: initial,
            },
            rate_w_per_s,
        }
    }

    pub fn set_target(&mut self, target: f32) {
        // NaN propagating through the slew math poisons `actual`
        // permanently; reject it at the door. ±∞ is left through —
        // a target of f32::INFINITY combined with a finite rate still
        // gives a well-defined per-tick step.
        if target.is_nan() {
            log::warn!("Ramp::set_target ignored NaN");
            return;
        }
        self.state.target = target;
    }

    pub fn snap_to(&mut self, value: f32) {
        // Same hazard as `set_target`: a NaN here poisons `actual`
        // permanently, since every later slew step propagates it.
        if value.is_nan() {
            log::warn!("Ramp::snap_to ignored NaN");
            return;
        }
        self.state.target = value;
        self.state.actual = value;
    }

    /// Move `actual` without touching the target, for an output a
    /// physical limit has cut short of where the slew put it.
    pub fn set_actual(&mut self, value: f32) {
        if value.is_nan() {
            log::warn!("Ramp::set_actual ignored NaN");
            return;
        }
        self.state.actual = value;
    }

    pub fn actual(&self) -> f32 {
        self.state.actual
    }

    pub fn target(&self) -> f32 {
        self.state.target
    }

    /// Advance `actual` by the most it is allowed to move in `dt`.
    pub fn advance(&mut self, dt: Duration) -> f32 {
        let s = &mut self.state;
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
        let mut cd = CommandDelay::new(Duration::ZERO);
        cd.set_target(5000.0);
        assert_eq!(cd.armed(), Some(5000.0));
    }

    #[test]
    fn command_delay_blocks_until_due() {
        let t0 = Utc::now();
        let mut cd = CommandDelay::new(Duration::from_secs(2));
        cd.set_target(5000.0);
        assert_eq!(cd.poll(t0), None); // first poll stamps the command
        assert_eq!(cd.poll(t0 + chrono::Duration::seconds(1)), None);
        assert_eq!(cd.poll(t0 + chrono::Duration::seconds(2)), Some(5000.0));
    }

    /// A tick that lands a little early still arms a command one
    /// delay after its stamp: a 99.5 ms gap counts as the 100 ms
    /// delay.
    #[test]
    fn command_delay_allows_for_timer_jitter() {
        let t0 = Utc::now();
        let mut cd = CommandDelay::new(Duration::from_millis(100));
        cd.set_target(1000.0);
        assert_eq!(cd.poll(t0), None);
        let early = t0 + chrono::Duration::microseconds(99_500);
        assert_eq!(cd.poll(early), Some(1000.0));
    }

    /// The allowance is at most half the delay: a 3 ms command delay
    /// still waits for the next 100 ms tick.
    #[test]
    fn a_short_command_delay_still_waits_a_tick() {
        let t0 = Utc::now();
        let mut cd = CommandDelay::new(Duration::from_millis(3));
        cd.set_target(1000.0);
        assert_eq!(cd.poll(t0), None);
        assert_eq!(
            cd.poll(t0 + chrono::Duration::milliseconds(100)),
            Some(1000.0)
        );
    }

    /// Commands carry no clock when submitted: the tick clock stamps
    /// them. A simulated clock far from wall time still arms them.
    #[test]
    fn command_delay_follows_the_tick_clock() {
        let sim0 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2020, 1, 1, 0, 0, 0).unwrap();
        let mut cd = CommandDelay::new(Duration::from_secs(1));
        cd.set_target(500.0);
        assert_eq!(cd.poll(sim0), None);
        assert_eq!(cd.poll(sim0 + chrono::Duration::seconds(1)), Some(500.0));
    }

    /// A controller re-sending faster than the delay must not starve
    /// the axis: the executing command keeps its own due time, so
    /// commands keep arming even under a continuous fast stream.
    #[test]
    fn command_delay_survives_fast_resend_cadence() {
        let t0 = Utc::now();
        let mut cd = CommandDelay::new(Duration::from_millis(1500));
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
        // axis must have armed several of them by now, not none.
        assert!(armed.is_some(), "fast re-sends starved the axis");
        // And it keeps progressing: the newest command arms within a
        // couple more polls (one command arms per poll).
        cd.poll(t0 + chrono::Duration::seconds(60));
        let settled = cd.poll(t0 + chrono::Duration::seconds(61));
        assert_eq!(settled, Some(10_000.0));
    }

    /// A wall clock that steps back (an NTP step) leaves the
    /// executing command stamped after `now`. It counts as due, so
    /// the axis keeps arming commands instead of stalling until the
    /// clock catches up.
    #[test]
    fn command_delay_survives_a_backward_clock_step() {
        let t0 = Utc::now();
        let mut cd = CommandDelay::new(Duration::from_secs(1));
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
        let mut cd = CommandDelay::new(Duration::from_secs(2));
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
        let mut r = Ramp::new(1000.0, 0.0);
        r.set_target(5000.0);
        assert_eq!(r.advance(Duration::from_secs(1)), 1000.0);
        assert_eq!(r.advance(Duration::from_secs(1)), 2000.0);
        // Big jump → step caps it
        assert_eq!(r.advance(Duration::from_secs(2)), 4000.0);
    }

    #[test]
    fn ramp_pass_through_when_infinite() {
        let mut r = Ramp::new(f32::INFINITY, 0.0);
        r.set_target(5000.0);
        assert_eq!(r.advance(Duration::from_millis(1)), 5000.0);
    }

    #[test]
    fn ramp_ignores_nan_target() {
        let mut r = Ramp::new(1000.0, 0.0);
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
