//! `DeviceAxis`: the hardware half of one power axis. A command
//! handed in is stamped at the next tick and reaches the output on
//! the first tick at or after its stamp + the device delay, clamped
//! to the band the component passes on each tick (its rated band ∩
//! its current physical band). A command can come out up to the
//! jitter allowance early (`ramp::TICK_JITTER`, at most half the
//! delay). The gateway, or a direct writer, hands commands in through
//! `set_command`; nothing here knows about the Microgrid API.

use std::{collections::VecDeque, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{bounds::VecBounds, ramp::is_due};

/// How many slots one delay is cut into. A command stamped less than
/// one slot (`delay / MAX_LINE`) after the newest queued one replaces
/// that one's value and keeps its stamp, so the line holds at most
/// `MAX_LINE + 1` entries whatever the tick, at the cost of a command
/// coming out up to one slot early. Past that the oldest is dropped,
/// a backstop the merging keeps out of reach.
const MAX_LINE: usize = 4096;

pub struct DeviceAxis {
    delay: chrono::Duration,
    /// `delay / MAX_LINE`: the merge width of the delay line.
    slot: chrono::Duration,
    state: Mutex<DeviceState>,
}

struct DeviceState {
    /// The newest command handed in since the last tick, unstamped.
    pending: Option<f32>,
    /// Stamped commands, oldest first, at most one per tick and one
    /// per slot.
    line: VecDeque<(DateTime<Utc>, f32)>,
    /// The newest command that has come out of the delay line.
    delayed: f32,
    /// The clamped output of the last tick.
    output: f32,
}

impl DeviceAxis {
    /// A device that answers after `delay`, starting at `initial`.
    pub fn new(delay: Duration, initial: f32) -> Self {
        let delay = chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX);
        Self {
            delay,
            slot: delay / MAX_LINE as i32,
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

    /// Stamp the pending command at `now`, let every command that is
    /// due by `now` (`ramp::is_due`) reach the output in order, and
    /// clamp the output to `band`. Returns the output.
    pub fn tick(&self, now: DateTime<Utc>, band: Option<&VecBounds>) -> f32 {
        let mut s = self.state.lock();
        if let Some(v) = s.pending.take() {
            match s.line.back_mut() {
                Some(back) if back.0 <= now && now - back.0 < self.slot => back.1 = v,
                Some(_) | None => s.line.push_back((now, v)),
            }
            while s.line.len() > MAX_LINE + 1 {
                s.line.pop_front();
            }
        }
        while let Some(&(at, v)) = s.line.front()
            && is_due(at, self.delay, now)
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

    /// A tick that lands a little early still releases a command one
    /// delay after its stamp: a 99.5 ms gap counts as the 100 ms
    /// delay, and a ramp on gaps that alternate 99.5 and 100.5 ms
    /// lags exactly one tick.
    #[test]
    fn timer_jitter_does_not_hold_a_command_back_a_tick() {
        let t0 = Utc::now();
        let us = |u: i64| t0 + chrono::Duration::microseconds(u);
        let ax = DeviceAxis::new(Duration::from_millis(100), 0.0);
        ax.set_command(1000.0);
        ax.tick(t0, None);
        assert_eq!(ax.tick(us(99_500), None), 1000.0);

        let ramp = DeviceAxis::new(Duration::from_millis(100), 0.0);
        let mut t = 0;
        for k in 1..=50 {
            ramp.set_command(k as f32);
            assert_eq!(ramp.tick(us(t), None), (k - 1) as f32, "tick {k}");
            t += if k % 2 == 0 { 99_500 } else { 100_500 };
        }
    }

    /// The jitter allowance is at most half the delay, so a delay
    /// shorter than the allowance still holds a command until the
    /// next tick: 3 ms on 100 ms ticks is not released in the tick
    /// that stamps it.
    #[test]
    fn a_short_delay_still_waits_a_tick() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::from_millis(3), 0.0);
        ax.set_command(1000.0);
        assert_eq!(ax.tick(t0, None), 0.0, "held in the stamping tick");
        assert_eq!(ax.tick(ms(t0, 100), None), 1000.0);
    }

    /// A delay longer than `MAX_LINE` ticks still moves the output:
    /// on 1 ms ticks with a 5 s delay and command `m` on tick `m`,
    /// the output holds its initial value until the delay (less the
    /// jitter allowance) has run, then trails the commands by that
    /// much, within one merge slot and one tick.
    #[test]
    fn a_delay_longer_than_the_line_still_moves_the_output() {
        let t0 = Utc::now();
        let ax = DeviceAxis::new(Duration::from_millis(5_000), -1.0);
        let lag = 5_000 - crate::sim::ramp::TICK_JITTER.num_milliseconds();
        for m in 0..20_000 {
            ax.set_command(m as f32);
            let out = ax.tick(ms(t0, m), None);
            if m == lag - 1 {
                assert_eq!(out, -1.0, "holds before the delay");
            }
            if m == lag {
                assert!(out >= 0.0, "moves once the delay has run, got {out}");
            }
            if m > lag {
                let want = (m - lag) as f32;
                assert!((out - want).abs() <= 2.0, "at {m}: {out} vs {want}");
            }
        }
        assert!(ax.state.lock().line.len() <= MAX_LINE + 1);
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
