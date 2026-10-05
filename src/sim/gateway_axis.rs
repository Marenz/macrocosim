//! `GatewayAxis`: the Microgrid API half of one power axis — the
//! standing command behind the gateway delay, the slew ramp, and the
//! axis's bounds augmentations. The component supplies the static
//! shape on every call (`base`): its rated band for an active axis,
//! its reactive capability evaluated at the live P for a reactive
//! one. The gateway owns one of these per controllable component and
//! axis.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::sim::{
    bounds::{ComponentBounds, VecBounds},
    ramp::{CommandDelay, Ramp},
};

/// Static configuration for one `GatewayAxis`.
pub struct GatewayAxisConfig {
    /// The API overhead between accepting a command and acting on it.
    pub command_delay: Duration,
    /// Slew rate per second; `f32::INFINITY` disables ramping.
    pub ramp_rate_per_s: f32,
    /// Where the ramp starts: the component's `initial_value`.
    pub initial: f32,
}

/// Per-tick inputs `advance` needs beyond the axis's own state.
pub struct AdvanceCtx<'a> {
    /// The component's static shape (rated band, or caps at live P).
    pub base: &'a VecBounds,
    /// The component's current physical band; the output is never
    /// left outside it. It contains 0 for every component.
    pub physical: Option<&'a VecBounds>,
    /// A battery inverter's window share (active axis); the output is
    /// never left outside it either. It contains 0.
    pub share: Option<&'a VecBounds>,
}

pub struct GatewayAxis {
    /// Live augmentations only; the static shape is the caller's
    /// `base`, never stored here.
    augs: ComponentBounds,
    delay: CommandDelay,
    ramp: Ramp,
}

impl GatewayAxis {
    pub fn new(cfg: GatewayAxisConfig) -> Self {
        Self {
            augs: ComponentBounds::default(),
            delay: CommandDelay::new(cfg.command_delay),
            ramp: Ramp::new(cfg.ramp_rate_per_s, cfg.initial),
        }
    }

    /// `base` ∩ the augmentations live at `now`. With none live it is
    /// `base` itself; with live ones that exclude each other it is
    /// empty, a real constraint under which only 0 is legal.
    pub fn validation_envelope(&self, base: &VecBounds, now: DateTime<Utc>) -> VecBounds {
        if self.augs.has_live_augmentations(now) {
            base.intersect(&self.augs.effective_at(now))
        } else {
            base.clone()
        }
    }

    /// NaN is refused; 0 is always accepted; any other value must lie
    /// inside the validation envelope. An empty envelope rejects
    /// every nonzero value. On `Err` the payload is the validation
    /// envelope the value missed.
    pub fn check(&self, value: f32, base: &VecBounds, now: DateTime<Utc>) -> Result<(), VecBounds> {
        let envelope = self.validation_envelope(base, now);
        if !value.is_finite() || (value != 0.0 && !envelope.contains(value)) {
            return Err(envelope);
        }
        Ok(())
    }

    /// Arm `value` behind the gateway delay. The caller has checked
    /// it.
    pub fn accept(&mut self, value: f32) {
        self.delay.set_target(value);
    }

    /// Compose, check and insert in one `&mut` borrow (the gateway
    /// lock makes it atomic): `bounds` goes in only if validation
    /// envelope ∩ `physical` ∩ `bounds` is non-empty. On `Err`
    /// nothing was stored and the payload is the current envelope
    /// (validation ∩ `physical`): what a client could still command,
    /// possibly empty when the axis is already boxed in.
    ///
    /// `physical` is the component's physical band where its
    /// emptiness check includes it (`bounds_follow_physical_band`),
    /// else `None`. A later move of the live P or of the physical
    /// band can still empty the envelope after an `Ok`.
    pub fn try_augment(
        &mut self,
        base: &VecBounds,
        physical: Option<&VecBounds>,
        ts: DateTime<Utc>,
        bounds: VecBounds,
        lifetime: Duration,
    ) -> Result<(), VecBounds> {
        let mut current = self.validation_envelope(base, ts);
        if let Some(p) = physical {
            current = current.intersect(p);
        }
        if current.intersect(&bounds).0.is_empty() {
            return Err(current);
        }
        self.augs.add_augmentation(ts, bounds, lifetime);
        Ok(())
    }

    /// True while at least one augmentation is live at `now`.
    pub fn augmented(&self, now: DateTime<Utc>) -> bool {
        self.augs.has_live_augmentations(now)
    }

    /// Reap augmentations whose lifetime has run out by `now`.
    pub fn drop_expired(&mut self, now: DateTime<Utc>) {
        self.augs.drop_expired(now);
    }

    /// The armed command, without advancing the delay clock.
    #[cfg(test)]
    pub fn armed(&self) -> Option<f32> {
        self.delay.armed()
    }

    /// Step 1 of a tick: the armed command (promoted by the gateway
    /// delay at `now`), else `idle`, else `None` (hold).
    pub fn target(&mut self, now: DateTime<Utc>, idle: Option<f32>) -> Option<f32> {
        if let Some(armed) = self.delay.poll(now) {
            return Some(armed);
        }
        match idle {
            Some(v) if v.is_finite() => Some(v),
            Some(_) => {
                log::debug!("GatewayAxis::target ignored a non-finite idle value");
                None
            }
            None => None,
        }
    }

    /// Steps 2-4 of a tick: clamp `target` to the tracking envelope
    /// (validation ∩ physical ∩ share; 0 is never pulled to an edge
    /// and an empty envelope parks at 0), ramp toward it, then hold
    /// the ramp's own value inside the physical band and the share —
    /// a narrowing is followed at once, a widening is climbed at the
    /// ramp rate. Returns the command to hand to the component.
    pub fn advance(
        &mut self,
        target: Option<f32>,
        now: DateTime<Utc>,
        dt: Duration,
        ctx: &AdvanceCtx<'_>,
    ) -> f32 {
        if let Some(v) = target {
            let mut env = self.validation_envelope(ctx.base, now);
            for band in [ctx.physical, ctx.share].into_iter().flatten() {
                env = env.intersect(band);
            }
            self.ramp.set_target(env.clamp_or_park(v));
        }
        let mut actual = self.ramp.advance(dt);
        for band in [ctx.physical, ctx.share].into_iter().flatten() {
            if actual != 0.0 && !band.contains(actual) {
                actual = band.clamp_or_park(actual);
                self.ramp.set_actual(actual);
            }
        }
        actual
    }

    /// Expiry or explicit reset: clear the command and ramp toward
    /// `park` at the ramp rate.
    pub fn reset(&mut self, park: f32) {
        self.delay.reset();
        self.ramp.set_target(park);
    }

    /// Health trip: the ramp snaps to 0; the command is cleared
    /// unless `keep_command`, in which case it is ramped back to on
    /// recovery.
    pub fn trip(&mut self, keep_command: bool) {
        if !keep_command {
            self.delay.reset();
        }
        self.ramp.snap_to(0.0);
    }

    /// The ramp's current value.
    #[cfg(test)]
    pub fn actual(&self) -> f32 {
        self.ramp.actual()
    }

    /// The ramp's current target (what a held axis keeps aiming at).
    pub fn ramp_target(&self) -> f32 {
        self.ramp.target()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::common::metrics::Bounds;
    use crate::sim::reactive::ReactiveCapability;

    fn axis() -> GatewayAxis {
        GatewayAxis::new(GatewayAxisConfig {
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            initial: 0.0,
        })
    }

    fn kva(va: f32) -> ReactiveCapability {
        ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(va),
        }
    }

    /// 0 is always accepted, NaN never; anything else must lie in
    /// base ∩ live augmentations.
    #[test]
    fn zero_is_always_accepted_and_nan_never() {
        let mut ax = axis();
        let rated = VecBounds::single(-100.0, 100.0);
        let t0 = Utc::now();
        ax.try_augment(
            &rated,
            None,
            t0,
            VecBounds::single(50.0, 100.0),
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(ax.check(0.0, &rated, t0).is_ok());
        assert!(ax.check(60.0, &rated, t0).is_ok());
        assert!(ax.check(40.0, &rated, t0).is_err());
        assert!(ax.check(f32::NAN, &rated, t0).is_err());
    }

    /// A reactive axis validates against the caps band at the live P
    /// narrowed by its augmentations.
    #[test]
    fn q_validation_follows_caps_and_augmentations() {
        let mut ax = axis();
        let t0 = Utc::now();
        // At P=3000, the 5 kVA circle allows |Q| ≤ 4000.
        let base = kva(5_000.0).q_band_at(3_000.0);
        assert!(ax.check(4_500.0, &base, t0).is_err());
        assert!(ax.check(3_500.0, &base, t0).is_ok());
        ax.try_augment(
            &base,
            None,
            t0,
            VecBounds::single(-1_000.0, 1_000.0),
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(ax.check(3_500.0, &base, t0).is_err());
        assert!(ax.check(0.0, &base, t0).is_ok());
    }

    /// An out-of-envelope error carries the envelope the value
    /// missed.
    #[test]
    fn out_of_bounds_error_carries_the_envelope() {
        let ax = axis();
        let base = ReactiveCapability {
            pf_limit: Some(0.5),
            apparent_va: None,
        }
        .q_band_at(10_000.0);
        match ax.check(6_000.0, &base, Utc::now()) {
            Err(envelope) => {
                let b = envelope.0.first().expect("single band");
                assert!((b.lower.unwrap() + 5_000.0).abs() < 1.0);
                assert!((b.upper.unwrap() - 5_000.0).abs() < 1.0);
            }
            other => panic!("expected an envelope, got {other:?}"),
        }
    }

    /// Fully unbounded augmentations, even stacked, narrow nothing.
    #[test]
    fn stacked_unbounded_augmentations_narrow_nothing() {
        let mut ax = axis();
        let t0 = Utc::now();
        let base = kva(5_000.0).q_band_at(3_000.0);
        let open = VecBounds::new(vec![Bounds {
            lower: None,
            upper: None,
        }]);
        ax.try_augment(&base, None, t0, open.clone(), Duration::from_secs(60))
            .unwrap();
        ax.try_augment(&base, None, t0, open, Duration::from_secs(60))
            .unwrap();
        assert!(ax.check(3_500.0, &base, t0).is_ok());
        assert!(ax.check(4_500.0, &base, t0).is_err());
    }

    /// An envelope emptied by an augmentation rejects every nonzero
    /// value: a P base emptied by a disjoint augmentation, and a Q
    /// caps band that shrank away from a live augmentation.
    #[test]
    fn an_emptied_envelope_rejects_everything_but_zero() {
        let mut ax = axis();
        let t0 = Utc::now();
        let rated = VecBounds::single(0.0, 100.0);
        ax.try_augment(
            &rated,
            None,
            t0,
            VecBounds::single(50.0, 300.0),
            Duration::from_secs(60),
        )
        .unwrap();
        let narrowed = VecBounds::single(0.0, 40.0);
        assert!(ax.validation_envelope(&narrowed, t0).0.is_empty());
        assert!(ax.check(20.0, &narrowed, t0).is_err());
        assert!(ax.check(0.0, &narrowed, t0).is_ok());

        let mut q = axis();
        let at_idle = kva(5_000.0).q_band_at(0.0);
        q.try_augment(
            &at_idle,
            None,
            t0,
            VecBounds::single(-4_000.0, -3_000.0),
            Duration::from_secs(60),
        )
        .unwrap();
        let at_rim = kva(5_000.0).q_band_at(5_000.0);
        assert!(q.validation_envelope(&at_rim, t0).0.is_empty());
        assert!(q.check(400.0, &at_rim, t0).is_err());
    }

    /// A second augmentation disjoint from a live one is refused, and
    /// the refusal names the current envelope, not the empty result.
    #[test]
    fn try_augment_rejects_a_band_disjoint_with_live_augmentations() {
        let mut ax = axis();
        let t0 = Utc::now();
        let rated = VecBounds::single(-10_000.0, 10_000.0);
        ax.try_augment(
            &rated,
            None,
            t0,
            VecBounds::single(5_000.0, 8_000.0),
            Duration::from_secs(60),
        )
        .unwrap();
        let current = ax
            .try_augment(
                &rated,
                None,
                t0,
                VecBounds::single(-8_000.0, -5_000.0),
                Duration::from_secs(60),
            )
            .expect_err("disjoint from the live augmentation");
        assert_eq!(
            (current.0[0].lower, current.0[0].upper),
            (Some(5_000.0), Some(8_000.0))
        );
        assert!(ax.check(6_000.0, &rated, t0).is_ok());
    }

    /// The physical band takes part in the emptiness check when the
    /// caller passes it, and the refusal names the narrowed envelope.
    #[test]
    fn try_augment_rejects_a_band_disjoint_with_the_physical_band() {
        let mut ax = axis();
        let t0 = Utc::now();
        let rated = VecBounds::single(0.0, 22_000.0);
        let need = VecBounds::single(0.0, 2_000.0);
        let current = ax
            .try_augment(
                &rated,
                Some(&need),
                t0,
                VecBounds::single(10_000.0, 22_000.0),
                Duration::from_secs(60),
            )
            .expect_err("disjoint from the physical band");
        assert_eq!(
            (current.0[0].lower, current.0[0].upper),
            (Some(0.0), Some(2_000.0))
        );
        assert!(!ax.augmented(t0), "nothing was stored");
        assert!(
            ax.try_augment(
                &rated,
                Some(&need),
                t0,
                VecBounds::single(1_000.0, 5_000.0),
                Duration::from_secs(60)
            )
            .is_ok()
        );
        // Without the physical band the same disjoint request goes
        // in.
        let mut pv = axis();
        assert!(
            pv.try_augment(
                &rated,
                None,
                t0,
                VecBounds::single(10_000.0, 22_000.0),
                Duration::from_secs(60)
            )
            .is_ok()
        );
    }

    /// `augmented` is true only while an augmentation is live, and
    /// `drop_expired` reaps lapsed ones.
    #[test]
    fn augmented_reflects_live_augmentations_and_expiry() {
        let mut ax = axis();
        let t0 = Utc::now();
        let rated = VecBounds::single(0.0, 100.0);
        assert!(!ax.augmented(t0));
        ax.try_augment(
            &rated,
            None,
            t0,
            VecBounds::single(10.0, 300.0),
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(ax.augmented(t0));
        let later = t0 + chrono::Duration::seconds(120);
        assert!(!ax.augmented(later));
        ax.drop_expired(later);
        assert_eq!(
            ax.validation_envelope(&rated, t0).to_string(),
            "[0, 100]",
            "a reaped augmentation is gone at any time"
        );
    }

    /// `accept` arms the value behind the gateway delay.
    #[test]
    fn accept_arms_the_value() {
        let mut ax = axis();
        ax.accept(1_500.0);
        assert_eq!(ax.armed(), Some(1_500.0));
    }

    fn ramped(rate: f32, delay: Duration) -> GatewayAxis {
        GatewayAxis::new(GatewayAxisConfig {
            command_delay: delay,
            ramp_rate_per_s: rate,
            initial: 0.0,
        })
    }

    fn step(
        ax: &mut GatewayAxis,
        now: DateTime<Utc>,
        dt: Duration,
        idle: Option<f32>,
        ctx: &AdvanceCtx<'_>,
    ) -> f32 {
        let target = ax.target(now, idle);
        ax.advance(target, now, dt, ctx)
    }

    fn plain(base: &VecBounds) -> AdvanceCtx<'_> {
        AdvanceCtx {
            base,
            physical: None,
            share: None,
        }
    }

    /// The armed target follows a tightening envelope and comes back
    /// when it lapses.
    #[test]
    fn armed_target_follows_a_tightening_envelope_and_restores() {
        let mut ax = ramped(f32::INFINITY, Duration::ZERO);
        let rated = VecBounds::single(-10_000.0, 10_000.0);
        let t0 = Utc::now();
        ax.accept(8_000.0);
        let dt = Duration::from_secs(1);
        assert_eq!(step(&mut ax, t0, dt, None, &plain(&rated)), 8_000.0);
        ax.try_augment(
            &rated,
            None,
            t0,
            VecBounds::single(-3_000.0, 3_000.0),
            Duration::from_secs(2),
        )
        .unwrap();
        let t1 = t0 + chrono::Duration::seconds(1);
        assert_eq!(step(&mut ax, t1, dt, None, &plain(&rated)), 3_000.0);
        let t3 = t0 + chrono::Duration::seconds(3);
        assert_eq!(step(&mut ax, t3, dt, None, &plain(&rated)), 8_000.0);
    }

    /// 0 holds inside an exclusion gap; nonzero values are pulled to
    /// the nearest edge.
    #[test]
    fn zero_holds_inside_an_exclusion_gap() {
        let mut ax = ramped(f32::INFINITY, Duration::ZERO);
        let rated = VecBounds::single(-10_000.0, 10_000.0);
        let t0 = Utc::now();
        let gap = VecBounds::new(vec![
            Bounds {
                lower: Some(-5_000.0),
                upper: Some(-1_000.0),
            },
            Bounds {
                lower: Some(1_000.0),
                upper: Some(5_000.0),
            },
        ]);
        ax.try_augment(&rated, None, t0, gap, Duration::from_secs(60))
            .unwrap();
        let dt = Duration::from_secs(1);
        assert_eq!(step(&mut ax, t0, dt, Some(0.0), &plain(&rated)), 0.0);
        ax.accept(0.0);
        assert_eq!(step(&mut ax, t0, dt, None, &plain(&rated)), 0.0);
        ax.accept(3_000.0);
        assert_eq!(step(&mut ax, t0, dt, None, &plain(&rated)), 3_000.0);
        ax.try_augment(
            &rated,
            None,
            t0,
            VecBounds::single(-10_000.0, 2_000.0),
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(step(&mut ax, t0, dt, None, &plain(&rated)), 2_000.0);
    }

    /// An armed value with an empty tracking envelope parks at 0;
    /// hold with nothing armed leaves the ramp alone.
    #[test]
    fn empty_tracking_envelope_parks_at_zero_and_hold_leaves_the_ramp() {
        let mut ax = ramped(f32::INFINITY, Duration::ZERO);
        let rated = VecBounds::single(0.0, 22_000.0);
        let t0 = Utc::now();
        ax.accept(10_000.0);
        let disjoint = VecBounds::single(30_000.0, 40_000.0);
        let ctx = AdvanceCtx {
            base: &rated,
            physical: Some(&disjoint),
            share: None,
        };
        assert_eq!(step(&mut ax, t0, Duration::from_secs(1), None, &ctx), 0.0);

        let mut idle = ramped(f32::INFINITY, Duration::ZERO);
        assert_eq!(
            step(&mut idle, t0, Duration::from_secs(1), None, &plain(&rated)),
            0.0
        );
    }

    /// With nothing armed an idle value is tracked and clamped.
    #[test]
    fn idle_value_tracks_and_clamps() {
        let mut ax = ramped(f32::INFINITY, Duration::ZERO);
        let rated = VecBounds::single(-30_000.0, 0.0);
        let t0 = Utc::now();
        let dt = Duration::from_secs(1);
        assert_eq!(
            step(&mut ax, t0, dt, Some(-6_000.0), &plain(&rated)),
            -6_000.0
        );
        ax.try_augment(
            &rated,
            None,
            t0,
            VecBounds::single(-2_000.0, 0.0),
            Duration::from_secs(60),
        )
        .unwrap();
        let t1 = t0 + chrono::Duration::seconds(1);
        assert_eq!(
            step(&mut ax, t1, dt, Some(-6_000.0), &plain(&rated)),
            -2_000.0
        );
    }

    /// A non-finite idle value is treated as hold.
    #[test]
    fn non_finite_idle_is_treated_as_hold() {
        let mut ax = ramped(f32::INFINITY, Duration::ZERO);
        let rated = VecBounds::single(-10_000.0, 10_000.0);
        let t0 = Utc::now();
        let dt = Duration::from_secs(1);
        assert_eq!(
            step(&mut ax, t0, dt, Some(4_000.0), &plain(&rated)),
            4_000.0
        );
        assert_eq!(
            step(&mut ax, t0, dt, Some(f32::NAN), &plain(&rated)),
            4_000.0
        );
    }

    /// The physical band is a snap-down limit: a narrowing is
    /// followed at once, a widening is climbed at the ramp rate.
    #[test]
    fn output_never_sits_outside_the_physical_band() {
        let mut ax = ramped(2_000.0, Duration::ZERO);
        let rated = VecBounds::single(-30_000.0, 0.0);
        let t0 = Utc::now();
        let mut sun = |avail: f32| {
            let band = VecBounds::single(avail, 0.0);
            let ctx = AdvanceCtx {
                base: &rated,
                physical: Some(&band),
                share: None,
            };
            step(&mut ax, t0, Duration::from_millis(100), Some(avail), &ctx)
        };
        for _ in 0..160 {
            sun(-30_000.0);
        }
        assert_eq!(sun(-30_000.0), -30_000.0);
        assert_eq!(sun(-6_000.0), -6_000.0);
        assert_eq!(sun(-30_000.0), -6_200.0);
    }

    /// The window share is a snap-down limit too, and a standing
    /// command climbs back at the ramp rate as the share widens.
    #[test]
    fn the_share_snaps_down_and_a_widening_is_climbed() {
        let mut ax = ramped(1_000.0, Duration::ZERO);
        let rated = VecBounds::single(-10_000.0, 10_000.0);
        let t0 = Utc::now();
        ax.accept(5_000.0);
        let dt = Duration::from_secs(1);
        let mut with_share = |s: f32| {
            let band = VecBounds::single(0.0, s);
            let ctx = AdvanceCtx {
                base: &rated,
                physical: None,
                share: Some(&band),
            };
            step(&mut ax, t0, dt, None, &ctx)
        };
        for _ in 0..6 {
            with_share(10_000.0);
        }
        assert_eq!(with_share(10_000.0), 5_000.0);
        assert_eq!(
            with_share(1_500.0),
            1_500.0,
            "narrowing is followed at once"
        );
        assert_eq!(with_share(10_000.0), 2_500.0, "widening climbs at 1 kW/s");
        assert_eq!(ax.armed(), Some(5_000.0), "the command is kept");
    }

    /// A trip snaps to 0 and clears the command unless told to keep
    /// it; a kept command is ramped back to.
    #[test]
    fn trip_snaps_and_keeps_or_clears_the_command() {
        let rated = VecBounds::single(-10_000.0, 10_000.0);
        let t0 = Utc::now();
        let dt = Duration::from_secs(2);

        let mut ax = ramped(1_000.0, Duration::ZERO);
        ax.accept(5_000.0);
        step(&mut ax, t0, dt, None, &plain(&rated));
        ax.trip(false);
        assert_eq!(ax.actual(), 0.0);
        assert_eq!(ax.armed(), None);

        let mut kept = ramped(1_000.0, Duration::ZERO);
        kept.accept(5_000.0);
        step(&mut kept, t0, dt, None, &plain(&rated));
        kept.trip(true);
        assert_eq!(kept.actual(), 0.0);
        assert_eq!(kept.armed(), Some(5_000.0));
        assert_eq!(step(&mut kept, t0, dt, None, &plain(&rated)), 2_000.0);
    }

    /// A reset clears the command and ramps toward the park value
    /// without snapping.
    #[test]
    fn reset_ramps_toward_the_park_value() {
        let mut ax = ramped(1_000.0, Duration::ZERO);
        let rated = VecBounds::single(-10_000.0, 10_000.0);
        let t0 = Utc::now();
        ax.accept(5_000.0);
        step(&mut ax, t0, Duration::from_secs(1), None, &plain(&rated));
        ax.reset(0.0);
        assert_eq!(ax.armed(), None);
        let v = step(
            &mut ax,
            t0,
            Duration::from_millis(500),
            None,
            &plain(&rated),
        );
        assert!((v - 500.0).abs() < 1.0, "ramps toward the park value: {v}");
    }

    /// The gateway delay holds a command back; the ramp then slews.
    #[test]
    fn q_accept_then_advance_drives_to_target() {
        let mut ax = GatewayAxis::new(GatewayAxisConfig {
            command_delay: Duration::from_millis(100),
            ramp_rate_per_s: 1_000.0,
            initial: 0.0,
        });
        let base = kva(10_000.0).q_band_at(0.0);
        let now = Utc::now();
        ax.accept(5_000.0);
        let q = step(&mut ax, now, Duration::from_millis(50), None, &plain(&base));
        assert!(q.abs() < 1.0, "nothing before the delay, got {q}");
        let q = step(
            &mut ax,
            now + chrono::Duration::milliseconds(1100),
            Duration::from_millis(1000),
            None,
            &plain(&base),
        );
        assert!((q - 1_000.0).abs() < 1.0, "got {q}");
        let q = step(
            &mut ax,
            now + chrono::Duration::milliseconds(6100),
            Duration::from_millis(5000),
            None,
            &plain(&base),
        );
        assert!((q - 5_000.0).abs() < 1.0, "got {q}");
    }

    /// A settled Q re-clamps when P moves under it, and comes back.
    #[test]
    fn q_re_clamps_on_p_drift_after_settle() {
        let mut ax = ramped(f32::INFINITY, Duration::ZERO);
        let now = Utc::now();
        let dt = Duration::from_millis(100);
        ax.accept(8_000.0);
        let mut at = |p: f32| {
            let base = kva(10_000.0).q_band_at(p);
            step(&mut ax, now, dt, None, &plain(&base))
        };
        assert!((at(0.0) - 8_000.0).abs() < 1.0);
        let q = at(9_000.0);
        assert!(q < 4_400.0 && q > 4_350.0, "≈4359, got {q}");
        assert!((at(0.0) - 8_000.0).abs() < 1.0);
    }

    /// The ramp starts at the configured initial value.
    #[test]
    fn the_ramp_starts_at_the_initial_value() {
        let mut ax = GatewayAxis::new(GatewayAxisConfig {
            command_delay: Duration::ZERO,
            ramp_rate_per_s: 2_000.0,
            initial: -6_000.0,
        });
        assert_eq!(ax.actual(), -6_000.0);
        let rated = VecBounds::single(-30_000.0, 0.0);
        let v = step(
            &mut ax,
            Utc::now(),
            Duration::from_millis(100),
            Some(-6_000.0),
            &plain(&rated),
        );
        assert_eq!(v, -6_000.0, "no slew up from zero");
    }
}
