//! Tracks the deadline by which the most recent set-power request for
//! each (component, power axis) pair expires. Active and reactive
//! setpoints carry independent request lifetimes, so a short-lived Q
//! command must not clear a long-lived P command when it expires (and
//! vice versa).
//!
//! Deadlines are absolute times on the owning site's clock
//! (`MicrogridSite::now`): wall time in a live server, sim time in a
//! headless run. Every method takes the `now` to judge against, so
//! the tracker never reads a clock of its own.
//!
//! Implementation is a `HashMap<(u64, SetpointAxis), DateTime<Utc>>`
//! swept by `drain_expired`. The sweep is O(N) over active entries; a
//! `BinaryHeap` keyed on the deadline is the upgrade if the scan ever
//! shows up in a profile.

use std::{collections::HashMap, time::Duration};

use chrono::{DateTime, Utc};

/// Which setpoint a request lifetime governs. Active and reactive
/// commands time out independently; expiry resets only its own axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SetpointAxis {
    Active,
    Reactive,
}

impl SetpointAxis {
    /// The axis's unit label, as named in setpoint error messages
    /// and in the active-setpoint readout.
    pub fn unit(self) -> &'static str {
        match self {
            SetpointAxis::Active => "W",
            SetpointAxis::Reactive => "VAr",
        }
    }
}

/// `now + lifetime`, saturating at the latest representable instant,
/// so an absurd lifetime means "never expires" rather than a panic.
pub fn deadline_after(now: DateTime<Utc>, lifetime: Duration) -> DateTime<Utc> {
    chrono::Duration::from_std(lifetime)
        .ok()
        .and_then(|d| now.checked_add_signed(d))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// Absolute expiry time of each armed (component, axis) request.
type Deadlines = HashMap<(u64, SetpointAxis), DateTime<Utc>>;

#[derive(Default)]
pub struct TimeoutTracker {
    deadlines: Deadlines,
}

impl TimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Time left at `now` before the (id, axis) setpoint expires.
    /// `None` when the pair isn't tracked or its deadline is at or
    /// before `now`.
    pub fn remaining(&self, id: u64, axis: SetpointAxis, now: DateTime<Utc>) -> Option<Duration> {
        let deadline = *self.deadlines.get(&(id, axis))?;
        (deadline - now).to_std().ok().filter(|d| !d.is_zero())
    }

    /// Arm the (id, axis) deadline at `now + lifetime`, replacing any
    /// earlier one, and return it.
    pub fn arm(
        &mut self,
        id: u64,
        axis: SetpointAxis,
        now: DateTime<Utc>,
        lifetime: Duration,
    ) -> DateTime<Utc> {
        let deadline = deadline_after(now, lifetime);
        self.deadlines.insert((id, axis), deadline);
        deadline
    }

    /// Remove and return every (id, axis) whose deadline is at or
    /// before `now`.
    pub fn drain_expired(&mut self, now: DateTime<Utc>) -> Vec<(u64, SetpointAxis)> {
        let mut expired = Vec::new();
        self.deadlines.retain(|&key, deadline| {
            if *deadline <= now {
                expired.push(key);
                false
            } else {
                true
            }
        });
        expired
    }

    /// Drop every armed deadline. Called when a site resets: a
    /// deadline armed before a hot reload must not fire against
    /// whatever component the rebuilt config registers under the same
    /// id. Nothing is actuated.
    pub fn clear(&mut self) {
        self.deadlines.clear();
    }

    /// Drop both of one component's deadlines (active AND reactive).
    /// Called when `id` leaves the registry and when an id is
    /// (re-)registered.
    pub fn remove_component(&mut self, id: u64) {
        self.deadlines.remove(&(id, SetpointAxis::Active));
        self.deadlines.remove(&(id, SetpointAxis::Reactive));
    }

    /// Drop one (id, axis) deadline.
    pub fn remove(&mut self, id: u64, axis: SetpointAxis) {
        self.deadlines.remove(&(id, axis));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(t0: DateTime<Utc>, s: i64) -> DateTime<Utc> {
        t0 + chrono::Duration::seconds(s)
    }

    /// The two axes hold independent deadlines for the same
    /// component: an elapsed reactive lifetime drains alone.
    #[test]
    fn axes_expire_independently() {
        let t0 = Utc::now();
        let mut t = TimeoutTracker::new();
        t.arm(7, SetpointAxis::Active, t0, Duration::from_secs(3600));
        t.arm(7, SetpointAxis::Reactive, t0, Duration::from_secs(1));
        let reset = t.drain_expired(secs(t0, 2));
        assert_eq!(reset, vec![(7, SetpointAxis::Reactive)]);
        let reset = t.drain_expired(secs(t0, 2));
        assert_eq!(reset, Vec::new());
    }

    /// Latest-set-wins is per axis.
    #[test]
    fn rearming_one_axis_keeps_the_other() {
        let t0 = Utc::now();
        let mut t = TimeoutTracker::new();
        t.arm(7, SetpointAxis::Reactive, t0, Duration::ZERO);
        t.arm(7, SetpointAxis::Active, t0, Duration::ZERO);
        t.arm(7, SetpointAxis::Active, t0, Duration::from_secs(3600));
        let reset = t.drain_expired(t0);
        assert_eq!(reset, vec![(7, SetpointAxis::Reactive)]);
    }

    /// The returned deadline is `now + lifetime` on the caller's
    /// clock, however far that clock is from wall time.
    #[test]
    fn the_deadline_is_on_the_callers_clock() {
        let sim0 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2020, 1, 1, 0, 0, 0).unwrap();
        let mut t = TimeoutTracker::new();
        let d = t.arm(1, SetpointAxis::Active, sim0, Duration::from_secs(30));
        assert_eq!(d, secs(sim0, 30));
        assert_eq!(
            t.remaining(1, SetpointAxis::Active, secs(sim0, 10)),
            Some(Duration::from_secs(20))
        );
    }

    /// `remaining` reports the time left for a tracked deadline, and
    /// None for an untracked axis or id, or one at or past its
    /// deadline.
    #[test]
    fn remaining_reports_time_left_and_none_when_absent_or_due() {
        let t0 = Utc::now();
        let mut t = TimeoutTracker::new();
        t.arm(1, SetpointAxis::Active, t0, Duration::from_secs(60));
        assert_eq!(
            t.remaining(1, SetpointAxis::Active, t0),
            Some(Duration::from_secs(60))
        );
        assert_eq!(t.remaining(1, SetpointAxis::Reactive, t0), None);
        assert_eq!(t.remaining(2, SetpointAxis::Active, t0), None);
        assert_eq!(t.remaining(1, SetpointAxis::Active, secs(t0, 60)), None);
        assert_eq!(t.remaining(1, SetpointAxis::Active, secs(t0, 61)), None);
    }

    /// An absurd lifetime saturates to "never" instead of panicking.
    #[test]
    fn a_huge_lifetime_never_expires() {
        let t0 = Utc::now();
        let mut t = TimeoutTracker::new();
        t.arm(1, SetpointAxis::Active, t0, Duration::MAX);
        let reset = t.drain_expired(secs(t0, 1_000_000));
        assert!(reset.is_empty());
    }

    /// `clear` drops every armed deadline.
    #[test]
    fn clear_drops_every_deadline() {
        let t0 = Utc::now();
        let mut t = TimeoutTracker::new();
        t.arm(7, SetpointAxis::Active, t0, Duration::ZERO);
        t.arm(9, SetpointAxis::Active, t0, Duration::from_secs(3600));
        t.clear();
        let reset = t.drain_expired(secs(t0, 1));
        assert_eq!(reset, Vec::new());
        assert_eq!(t.remaining(9, SetpointAxis::Active, t0), None);
    }

    /// `remove_component` drops both axes of one id only.
    #[test]
    fn remove_component_drops_both_axes_of_that_id_only() {
        let t0 = Utc::now();
        let mut t = TimeoutTracker::new();
        for (id, axis) in [
            (7, SetpointAxis::Active),
            (7, SetpointAxis::Reactive),
            (8, SetpointAxis::Active),
        ] {
            t.arm(id, axis, t0, Duration::from_secs(3600));
        }
        t.remove_component(7);
        assert_eq!(t.remaining(7, SetpointAxis::Active, t0), None);
        assert_eq!(t.remaining(7, SetpointAxis::Reactive, t0), None);
        assert!(t.remaining(8, SetpointAxis::Active, t0).is_some());
        t.remove_component(999);
        assert!(t.remaining(8, SetpointAxis::Active, t0).is_some());
    }
}
