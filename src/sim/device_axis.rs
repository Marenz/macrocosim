//! `DeviceAxis`: the hardware half of one power axis. A command
//! handed in at time t reaches the output at t + the device delay,
//! whatever the physics tick, clamped to the band the component
//! passes on each tick (its rated band ∩ its current physical band).
//! The gateway, or a direct writer, hands commands in through
//! `set_command`; nothing here knows about the Microgrid API.

use std::{collections::VecDeque, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::bounds::VecBounds;

/// Upper bound on queued commands; an absurd delay drops the oldest
/// instead of growing without limit.
const MAX_LINE: usize = 4096;

pub struct DeviceAxis {
    delay: chrono::Duration,
    state: Mutex<DeviceState>,
}

struct DeviceState {
    /// The newest command handed in since the last tick, unstamped.
    pending: Option<f32>,
    /// Stamped commands, oldest first, at most one per tick.
    line: VecDeque<(DateTime<Utc>, f32)>,
    /// The newest command that has come out of the delay line.
    delayed: f32,
    /// The clamped output of the last tick.
    output: f32,
}

impl DeviceAxis {
    /// A device that answers after `delay`, starting at `initial`.
    pub fn new(delay: Duration, initial: f32) -> Self {
        Self {
            delay: chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX),
            state: Mutex::new(DeviceState {
                pending: None,
                line: VecDeque::new(),
                delayed: initial,
                output: initial,
            }),
        }
    }

    /// Hand in a command. Stamped with the time of the next tick; a
    /// later call before that tick replaces it.
    pub fn set_command(&self, value: f32) {
        if !value.is_finite() {
            log::debug!("DeviceAxis::set_command ignored a non-finite command");
            return;
        }
        self.state.lock().pending = Some(value);
    }

    /// Stamp the pending command at `now`, let every command whose
    /// delay has run out (or whose stamp is after `now`) reach the
    /// output in order, and clamp the output to `band`. Returns the
    /// output.
    pub fn tick(&self, now: DateTime<Utc>, band: Option<&VecBounds>) -> f32 {
        let mut s = self.state.lock();
        if let Some(v) = s.pending.take() {
            s.line.push_back((now, v));
            while s.line.len() > MAX_LINE {
                s.line.pop_front();
            }
        }
        // A stamp after `now` means the wall clock stepped back; it
        // counts as due, so newer commands don't queue behind it.
        while let Some(&(at, v)) = s.line.front()
            && (at > now
                || at
                    .checked_add_signed(self.delay)
                    .is_some_and(|due| due <= now))
        {
            s.delayed = v;
            s.line.pop_front();
        }
        let out = match band {
            Some(band) => band.clamp_or_park(s.delayed),
            None => s.delayed,
        };
        s.output = out;
        out
    }

    /// A health trip: the output snaps to 0 and every command still
    /// in the delay line is dropped.
    pub fn trip(&self) {
        let mut s = self.state.lock();
        s.pending = None;
        s.line.clear();
        s.delayed = 0.0;
        s.output = 0.0;
    }

    /// The output of the last tick.
    pub fn output(&self) -> f32 {
        self.state.lock().output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t0: DateTime<Utc>, m: i64) -> DateTime<Utc> {
        t0 + chrono::Duration::milliseconds(m)
    }

    /// A command handed in at t reaches the output at t + delay,
    /// whatever the tick spacing: 30 ms ticks see it on the first
    /// tick at or after 100 ms, a 250 ms tick sees it at once.
    #[test]
    fn a_command_reaches_the_output_after_the_delay_at_any_tick() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::from_millis(100), 0.0);
        ax.set_command(3000.0);
        assert_eq!(ax.tick(t0, None), 0.0);
        for m in [30, 60, 90] {
            assert_eq!(ax.tick(ms(t0, m), None), 0.0, "at {m} ms");
        }
        assert_eq!(ax.tick(ms(t0, 120), None), 3000.0);

        let slow = DeviceAxis::new(Duration::from_millis(100), 0.0);
        slow.set_command(500.0);
        assert_eq!(slow.tick(t0, None), 0.0);
        assert_eq!(slow.tick(ms(t0, 250), None), 500.0);
    }

    #[test]
    fn zero_delay_passes_through_in_the_same_tick() {
        let ax = DeviceAxis::new(Duration::ZERO, 0.0);
        ax.set_command(1200.0);
        assert_eq!(ax.tick(Utc::now(), None), 1200.0);
    }

    /// Only the newest command handed in before a tick is stamped.
    #[test]
    fn the_newest_command_per_tick_wins() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::from_millis(100), 0.0);
        ax.set_command(1000.0);
        ax.set_command(2000.0);
        ax.tick(t0, None);
        assert_eq!(ax.tick(ms(t0, 100), None), 2000.0);
    }

    /// With no new command the output holds the last one through.
    #[test]
    fn the_output_holds_without_new_commands() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::ZERO, 0.0);
        ax.set_command(700.0);
        ax.tick(t0, None);
        assert_eq!(ax.tick(ms(t0, 100), None), 700.0);
        assert_eq!(ax.tick(ms(t0, 200), None), 700.0);
    }

    /// The band clamps the output; 0 is never pulled to a band edge,
    /// and an empty band leaves nothing but 0.
    #[test]
    fn the_output_is_clamped_to_the_band_but_zero_is_not_pulled() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::ZERO, 0.0);
        let band = VecBounds::single(1000.0, 5000.0);
        ax.set_command(8000.0);
        assert_eq!(ax.tick(t0, Some(&band)), 5000.0);
        ax.set_command(0.0);
        assert_eq!(ax.tick(ms(t0, 100), Some(&band)), 0.0);
        ax.set_command(3000.0);
        assert_eq!(ax.tick(ms(t0, 200), Some(&VecBounds::default())), 0.0);
    }

    /// A trip zeroes the output and empties the delay line: a command
    /// still in flight does not replay on recovery.
    #[test]
    fn a_trip_zeroes_the_output_and_empties_the_line() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::from_millis(100), 0.0);
        ax.set_command(3000.0);
        ax.tick(t0, None);
        ax.trip();
        assert_eq!(ax.output(), 0.0);
        assert_eq!(ax.tick(ms(t0, 150), None), 0.0, "no replay");
        assert_eq!(ax.tick(ms(t0, 300), None), 0.0, "no replay later either");
    }

    /// Seeded with its initial value before any tick.
    #[test]
    fn seeded_with_the_initial_value() {
        let ax = DeviceAxis::new(Duration::from_millis(100), -6000.0);
        assert_eq!(ax.output(), -6000.0);
        assert_eq!(ax.tick(Utc::now(), None), -6000.0);
    }

    /// A wall clock that steps back (an NTP step) leaves older
    /// commands stamped after `now`. They count as due, so newer
    /// commands keep reaching the output instead of queueing behind
    /// them until the clock catches up.
    #[test]
    fn a_backward_clock_step_does_not_stall_the_output() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::from_millis(100), 0.0);
        ax.set_command(1000.0);
        ax.tick(t0, None);
        assert_eq!(ax.tick(ms(t0, 100), None), 1000.0);
        ax.set_command(2000.0);
        ax.tick(ms(t0, 200), None);

        let back = ms(t0, -10_000);
        ax.set_command(3000.0);
        assert_eq!(ax.tick(back, None), 2000.0, "the stranded command is due");
        assert_eq!(ax.tick(ms(back, 50), None), 2000.0, "3000 waits its delay");
        assert_eq!(ax.tick(ms(back, 100), None), 3000.0);
        for m in (200..5_000).step_by(100) {
            ax.set_command(m as f32);
            ax.tick(ms(back, m), None);
        }
        assert_eq!(ax.output(), 4800.0, "the output follows after the step");
    }

    #[test]
    fn non_finite_commands_are_ignored() {
        let ax = DeviceAxis::new(Duration::ZERO, 0.0);
        ax.set_command(400.0);
        ax.set_command(f32::NAN);
        assert_eq!(ax.tick(Utc::now(), None), 400.0);
    }
}
