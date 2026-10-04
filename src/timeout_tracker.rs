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
//! swept by `reset_expired_with`. The sweep is O(N) over active
//! entries; a `BinaryHeap` keyed on the deadline is the upgrade if
//! the scan ever shows up in a profile.

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// Which setpoint a request lifetime governs. Active and reactive
/// commands time out independently; expiry resets only its own axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SetpointAxis {
    Active,
    Reactive,
}

impl SetpointAxis {
    /// The axis's unit label, as carried into setpoint error
    /// messages.
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

#[derive(Clone, Default)]
pub struct TimeoutTracker {
    inner: Arc<Mutex<Deadlines>>,
}

impl TimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Time left at `now` before the (id, axis) setpoint expires.
    /// `None` when the pair isn't tracked or its deadline is at or
    /// before `now`.
    pub fn remaining(&self, id: u64, axis: SetpointAxis, now: DateTime<Utc>) -> Option<Duration> {
        let deadline = *self.inner.lock().get(&(id, axis))?;
        (deadline - now).to_std().ok().filter(|d| !d.is_zero())
    }

    /// Actuate and arm a deadline as one atomic step under the
    /// deadline-map lock: `f` runs while the lock is held, and the
    /// deadline `now + lifetime` is inserted only if `f` succeeds,
    /// before the lock is released. Returns that deadline. A failed
    /// actuation arms nothing, so a later sweep cannot reset an
    /// unrelated, still-valid previous command on this axis.
    ///
    /// Lock order: tracker → components map → component locks. `f`
    /// must touch only an already-resolved component, never the
    /// components map or another tracker.
    pub fn actuate_and_arm<E>(
        &self,
        id: u64,
        axis: SetpointAxis,
        now: DateTime<Utc>,
        lifetime: Duration,
        f: impl FnOnce() -> Result<(), E>,
    ) -> Result<DateTime<Utc>, E> {
        let mut guard = self.inner.lock();
        f()?;
        let deadline = deadline_after(now, lifetime);
        guard.insert((id, axis), deadline);
        Ok(deadline)
    }

    /// Drain and reset every deadline at or before `now` as one
    /// atomic step: the lock is taken once, expired keys are removed
    /// and `f` runs per expired key, all before the lock is released,
    /// so a renewal cannot land between the drain and the reset and
    /// be wiped by it.
    ///
    /// Lock order: tracker → components map → component locks. `f`
    /// may look a component up and reset its axis; nothing takes the
    /// tracker lock while holding either of those.
    pub fn reset_expired_with(&self, now: DateTime<Utc>, mut f: impl FnMut(u64, SetpointAxis)) {
        let mut guard = self.inner.lock();
        guard.retain(|&(id, axis), deadline| {
            if *deadline <= now {
                f(id, axis);
                false
            } else {
                true
            }
        });
    }

    /// Drop every armed deadline. Called when a site resets: a
    /// deadline armed before a hot reload must not fire against
    /// whatever component the rebuilt config registers under the same
    /// id. Nothing is actuated.
    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// Drop both of one component's deadlines (active AND reactive).
    /// Called when `id` leaves the registry and when an id is
    /// (re-)registered.
    pub fn remove_component(&self, id: u64) {
        let mut guard = self.inner.lock();
        guard.remove(&(id, SetpointAxis::Active));
        guard.remove(&(id, SetpointAxis::Reactive));
    }

    /// Drop one (id, axis) deadline.
    pub fn remove(&self, id: u64, axis: SetpointAxis) {
        self.inner.lock().remove(&(id, axis));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> Result<(), ()> {
        Ok(())
    }

    fn secs(t0: DateTime<Utc>, s: i64) -> DateTime<Utc> {
        t0 + chrono::Duration::seconds(s)
    }

    /// The two axes hold independent deadlines for the same
    /// component: an elapsed reactive lifetime drains alone.
    #[test]
    fn axes_expire_independently() {
        let t0 = Utc::now();
        let t = TimeoutTracker::new();
        t.actuate_and_arm(7, SetpointAxis::Active, t0, Duration::from_secs(3600), ok)
            .unwrap();
        t.actuate_and_arm(7, SetpointAxis::Reactive, t0, Duration::from_secs(1), ok)
            .unwrap();
        let mut reset = Vec::new();
        t.reset_expired_with(secs(t0, 2), |id, axis| reset.push((id, axis)));
        assert_eq!(reset, vec![(7, SetpointAxis::Reactive)]);
        let mut reset = Vec::new();
        t.reset_expired_with(secs(t0, 2), |id, axis| reset.push((id, axis)));
        assert_eq!(reset, Vec::new());
    }

    /// Latest-set-wins is per axis.
    #[test]
    fn rearming_one_axis_keeps_the_other() {
        let t0 = Utc::now();
        let t = TimeoutTracker::new();
        t.actuate_and_arm(7, SetpointAxis::Reactive, t0, Duration::ZERO, ok)
            .unwrap();
        t.actuate_and_arm(7, SetpointAxis::Active, t0, Duration::ZERO, ok)
            .unwrap();
        t.actuate_and_arm(7, SetpointAxis::Active, t0, Duration::from_secs(3600), ok)
            .unwrap();
        let mut reset = Vec::new();
        t.reset_expired_with(t0, |id, axis| reset.push((id, axis)));
        assert_eq!(reset, vec![(7, SetpointAxis::Reactive)]);
    }

    /// The returned deadline is `now + lifetime` on the caller's
    /// clock, however far that clock is from wall time.
    #[test]
    fn the_deadline_is_on_the_callers_clock() {
        let sim0 = chrono::TimeZone::with_ymd_and_hms(&Utc, 2020, 1, 1, 0, 0, 0).unwrap();
        let t = TimeoutTracker::new();
        let d = t
            .actuate_and_arm(1, SetpointAxis::Active, sim0, Duration::from_secs(30), ok)
            .unwrap();
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
        let t = TimeoutTracker::new();
        t.actuate_and_arm(1, SetpointAxis::Active, t0, Duration::from_secs(60), ok)
            .unwrap();
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
        let t = TimeoutTracker::new();
        t.actuate_and_arm(1, SetpointAxis::Active, t0, Duration::MAX, ok)
            .unwrap();
        let mut reset = Vec::new();
        t.reset_expired_with(secs(t0, 1_000_000), |id, axis| reset.push((id, axis)));
        assert!(reset.is_empty());
    }

    /// A renewal that lands while the sweep is mid-drain must not be
    /// wiped: from inside the `reset_expired_with` callback (lock
    /// held) a second thread tries to re-arm the same key and must
    /// block until the sweep returns.
    #[test]
    fn rearmed_key_survives_the_expiry_sweep() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let t0 = Utc::now();
        let t = TimeoutTracker::new();
        t.actuate_and_arm(7, SetpointAxis::Active, t0, Duration::ZERO, ok)
            .unwrap();

        let completed = Arc::new(AtomicBool::new(false));
        let mut renewal: Option<std::thread::JoinHandle<()>> = None;
        let mut reset = Vec::new();
        t.reset_expired_with(t0, |id, axis| {
            reset.push((id, axis));
            let t2 = t.clone();
            let completed2 = completed.clone();
            let h = std::thread::spawn(move || {
                t2.actuate_and_arm(id, axis, t0, Duration::from_secs(900), ok)
                    .unwrap();
                completed2.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !completed.load(Ordering::SeqCst),
                "renewal must block on the tracker lock while the sweep callback is running"
            );
            renewal = Some(h);
        });
        renewal.take().unwrap().join().unwrap();
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(reset, vec![(7, SetpointAxis::Active)]);
        assert!(t.remaining(7, SetpointAxis::Active, t0).is_some());
    }

    /// `clear` drops every armed deadline.
    #[test]
    fn clear_drops_every_deadline() {
        let t0 = Utc::now();
        let t = TimeoutTracker::new();
        t.actuate_and_arm(7, SetpointAxis::Active, t0, Duration::ZERO, ok)
            .unwrap();
        t.actuate_and_arm(9, SetpointAxis::Active, t0, Duration::from_secs(3600), ok)
            .unwrap();
        t.clear();
        let mut reset = Vec::new();
        t.reset_expired_with(secs(t0, 1), |id, axis| reset.push((id, axis)));
        assert_eq!(reset, Vec::new());
        assert_eq!(t.remaining(9, SetpointAxis::Active, t0), None);
    }

    /// `remove_component` drops both axes of one id only.
    #[test]
    fn remove_component_drops_both_axes_of_that_id_only() {
        let t0 = Utc::now();
        let t = TimeoutTracker::new();
        for (id, axis) in [
            (7, SetpointAxis::Active),
            (7, SetpointAxis::Reactive),
            (8, SetpointAxis::Active),
        ] {
            t.actuate_and_arm(id, axis, t0, Duration::from_secs(3600), ok)
                .unwrap();
        }
        t.remove_component(7);
        assert_eq!(t.remaining(7, SetpointAxis::Active, t0), None);
        assert_eq!(t.remaining(7, SetpointAxis::Reactive, t0), None);
        assert!(t.remaining(8, SetpointAxis::Active, t0).is_some());
        t.remove_component(999);
        assert!(t.remaining(8, SetpointAxis::Active, t0).is_some());
    }

    /// A failed actuation arms nothing.
    #[test]
    fn failed_actuation_arms_nothing() {
        let t0 = Utc::now();
        let t = TimeoutTracker::new();
        let r = t.actuate_and_arm(7, SetpointAxis::Active, t0, Duration::ZERO, || Err(()));
        assert!(r.is_err());
        t.actuate_and_arm(8, SetpointAxis::Active, t0, Duration::ZERO, ok)
            .unwrap();
        let mut reset = Vec::new();
        t.reset_expired_with(t0, |id, axis| reset.push((id, axis)));
        assert_eq!(reset, vec![(8, SetpointAxis::Active)]);
    }
}
