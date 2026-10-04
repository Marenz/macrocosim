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
    component::SetpointError,
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
    /// "W" or "VAr", carried into `SetpointError::OutOfBounds`.
    pub unit: &'static str,
}

pub struct GatewayAxis {
    /// Live augmentations only; the static shape is the caller's
    /// `base`, never stored here.
    augs: ComponentBounds,
    delay: CommandDelay,
    #[expect(dead_code, reason = "read by `advance`, which lands next")]
    ramp: Ramp,
    unit: &'static str,
}

impl GatewayAxis {
    pub fn new(cfg: GatewayAxisConfig) -> Self {
        Self {
            augs: ComponentBounds::augmentations_only(),
            delay: CommandDelay::new(cfg.command_delay),
            ramp: Ramp::new(cfg.ramp_rate_per_s, cfg.initial),
            unit: cfg.unit,
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
    /// every nonzero value.
    pub fn check(
        &self,
        value: f32,
        base: &VecBounds,
        now: DateTime<Utc>,
    ) -> Result<(), SetpointError> {
        let envelope = self.validation_envelope(base, now);
        if !value.is_finite() || (value != 0.0 && !envelope.contains(value)) {
            return Err(SetpointError::OutOfBounds {
                value,
                unit: self.unit,
                envelope,
            });
        }
        Ok(())
    }

    /// Arm `value` behind the gateway delay. The caller has checked
    /// it.
    pub fn accept(&self, value: f32) {
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
    /// emptiness check includes it (`augment_checks_physical_band`),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::common::metrics::Bounds;
    use crate::sim::reactive::ReactiveCapability;

    fn axis(unit: &'static str) -> GatewayAxis {
        GatewayAxis::new(GatewayAxisConfig {
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            initial: 0.0,
            unit,
        })
    }

    /// The caps band a reactive axis is validated against at `p`.
    fn caps_at(cap: ReactiveCapability, p: f32) -> VecBounds {
        let (lo, hi) = cap.q_bounds_at(p);
        VecBounds::single(lo, hi)
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
        let mut ax = axis("W");
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
        assert!(matches!(
            ax.check(40.0, &rated, t0),
            Err(SetpointError::OutOfBounds { unit: "W", .. })
        ));
        assert!(ax.check(f32::NAN, &rated, t0).is_err());
    }

    /// A reactive axis validates against the caps band at the live P
    /// narrowed by its augmentations.
    #[test]
    fn q_validation_follows_caps_and_augmentations() {
        let mut ax = axis("VAr");
        let t0 = Utc::now();
        // At P=3000, the 5 kVA circle allows |Q| ≤ 4000.
        let base = caps_at(kva(5_000.0), 3_000.0);
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

    /// An out-of-envelope error names the value, the unit and the
    /// envelope it missed.
    #[test]
    fn out_of_bounds_error_names_the_envelope() {
        let ax = axis("VAr");
        let base = caps_at(
            ReactiveCapability {
                pf_limit: Some(0.5),
                apparent_va: None,
            },
            10_000.0,
        );
        match ax.check(6_000.0, &base, Utc::now()) {
            Err(SetpointError::OutOfBounds {
                value, envelope, ..
            }) => {
                assert_eq!(value, 6_000.0);
                let b = envelope.0.first().expect("single band");
                assert!((b.lower.unwrap() + 5_000.0).abs() < 1.0);
                assert!((b.upper.unwrap() - 5_000.0).abs() < 1.0);
            }
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    /// Fully unbounded augmentations, even stacked, narrow nothing.
    #[test]
    fn stacked_unbounded_augmentations_narrow_nothing() {
        let mut ax = axis("VAr");
        let t0 = Utc::now();
        let base = caps_at(kva(5_000.0), 3_000.0);
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
        let mut ax = axis("W");
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

        let mut q = axis("VAr");
        let at_idle = caps_at(kva(5_000.0), 0.0);
        q.try_augment(
            &at_idle,
            None,
            t0,
            VecBounds::single(-4_000.0, -3_000.0),
            Duration::from_secs(60),
        )
        .unwrap();
        let at_rim = caps_at(kva(5_000.0), 5_000.0);
        assert!(q.validation_envelope(&at_rim, t0).0.is_empty());
        assert!(q.check(400.0, &at_rim, t0).is_err());
    }

    /// A second augmentation disjoint from a live one is refused, and
    /// the refusal names the current envelope, not the empty result.
    #[test]
    fn try_augment_rejects_a_band_disjoint_with_live_augmentations() {
        let mut ax = axis("W");
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
        let mut ax = axis("W");
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
        let mut pv = axis("W");
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
        let mut ax = axis("W");
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
        let ax = axis("W");
        ax.accept(1_500.0);
        assert_eq!(ax.armed(), Some(1_500.0));
    }
}
