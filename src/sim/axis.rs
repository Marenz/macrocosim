//! `PowerAxis`: one per-axis control path shared by active (P) and
//! reactive (Q) power. Composes the three primitives every
//! setpoint-taking axis already needs — `CommandDelay` (the SCADA
//! ack lag), `Ramp` (the slew limit), and `ComponentBounds` (rated
//! band + TTL augmentations) — plus, for a Q axis, a `ReactiveCapability`
//! band computed from the OTHER axis's live value.
//!
//! A P axis is configured with `rated: Some((lo, hi))`, `caps: None`.
//! A Q axis is configured with `rated: None`, `caps: Some(cap)` — its
//! static shape comes from the PF/kVA caps evaluated at the live P,
//! not from a rated pair of its own.

use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    bounds::{ComponentBounds, VecBounds},
    component::SetpointError,
    ramp::{CommandDelay, Ramp},
    reactive::ReactiveCapability,
};

/// Static configuration for one `PowerAxis`.
pub struct AxisConfig {
    /// Static rated bounds. `None` for a Q axis (its static shape is the caps).
    pub rated: Option<(f32, f32)>,
    /// PF/kVA caps. `Some` for Q axes, `None` for P axes.
    pub caps: Option<ReactiveCapability>,
    pub command_delay: Duration,
    /// Slew rate per second; `f32::INFINITY` disables ramping.
    pub ramp_rate_per_s: f32,
    /// "W" or "VAr" — carried into `SetpointError::OutOfBounds`.
    pub unit: &'static str,
}

/// What `step` should target when no command is armed yet.
pub enum IdleTarget {
    /// Leave the ramp target alone when no command is armed.
    Hold,
    /// Track this value when no command is armed (0 holds; any other
    /// value is clamped into the tracking envelope, and an empty
    /// envelope parks at 0).
    Value(f32),
}

/// Per-tick context `step` needs beyond the axis's own state.
pub struct StepCtx<'a> {
    /// The OTHER axis's live value (P for a Q axis). Ignored when `caps` is None.
    pub other_axis: f32,
    /// Extra per-tick envelope from the component (solar sunlight
    /// floor, boiler heat need). Intersected into the tracking
    /// envelope only — never into validation — and the published
    /// output is held inside it without waiting on the ramp. It must
    /// contain 0, or a ramp toward 0 is held at its edge.
    pub dynamic: Option<&'a VecBounds>,
    pub idle: IdleTarget,
}

/// One axis's full control path: command delay → slew ramp →
/// published value, gated by a composed envelope (static rated/caps
/// band ∩ live TTL augmentations, plus a per-tick dynamic hook for
/// tracking only).
pub struct PowerAxis {
    /// Mirrors `AxisConfig::rated` — used to decide whether an empty
    /// static-augmentation product means "no constraint" (Q axis, no
    /// rated band of its own) or a real, if degenerate, constraint.
    rated: Option<(f32, f32)>,
    /// Rated band (P axis) or augmentations-only (Q axis) plus the
    /// live TTL augmentation queue. See `ComponentBounds::augmentations_only`.
    augs: Mutex<ComponentBounds>,
    caps: Mutex<Option<ReactiveCapability>>,
    delay: CommandDelay,
    ramp: Ramp,
    published: Mutex<f32>,
    unit: &'static str,
}

impl PowerAxis {
    pub fn new(cfg: AxisConfig) -> Self {
        let augs = match cfg.rated {
            Some((lo, hi)) => ComponentBounds::rated(lo, hi),
            None => ComponentBounds::augmentations_only(),
        };
        Self {
            rated: cfg.rated,
            augs: Mutex::new(augs),
            caps: Mutex::new(cfg.caps),
            delay: CommandDelay::new(cfg.command_delay),
            ramp: Ramp::new(cfg.ramp_rate_per_s, 0.0),
            published: Mutex::new(0.0),
            unit: cfg.unit,
        }
    }

    /// Shared composition for `validation_envelope_at` /
    /// `tracking_envelope_at`: static effective ∩ caps band (if any)
    /// ∩ dynamic (if given). Starts from "nothing folded in yet" and
    /// intersects each present piece in.
    ///
    /// The static piece is skipped entirely when `rated` is `None`
    /// (a Q axis) AND no augmentation is live — that combination means
    /// "no static constraint", not "constrained to nothing". Note the
    /// second half: when augmentations ARE live and their product is
    /// empty, they exclude one another — the opposite answer — so
    /// `has_live_augmentations`, not emptiness alone, decides. Any
    /// OTHER empty result — a P axis's rated band
    /// emptied by a disjoint augmentation, a Q axis's caps band emptied
    /// by a disjoint augmentation, two live augmentations disjoint from
    /// each other, or the intersection of otherwise non-empty pieces
    /// coming up empty — IS folded in as a real, if degenerate,
    /// constraint: `accept` rejects every nonzero value against it,
    /// it does NOT treat it as unconstrained (see `accept`'s doc).
    ///
    /// Ending with nothing folded in at all (no static piece, no caps,
    /// no dynamic) is unreached in practice — P axes always carry
    /// `rated`, Q axes always carry `caps` — but if it happens the
    /// result is empty `VecBounds` too, and gets the same treatment:
    /// `accept` rejects every nonzero value (the safe default for an
    /// unconfigured axis) and `step` parks the ramp at 0.
    fn envelope(
        &self,
        now: DateTime<Utc>,
        other_axis: f32,
        dynamic: Option<&VecBounds>,
    ) -> VecBounds {
        let augs = self.augs.lock();
        self.envelope_locked(&augs, now, other_axis, dynamic)
    }

    /// Same composition as [`Self::envelope`], but taking an
    /// already-held `augs` guard instead of locking it itself —
    /// parking_lot's `Mutex` is non-reentrant, so a caller that needs
    /// to hold the augmentation lock across more than this one read
    /// (`try_augment`'s compose-check-insert) can't go through
    /// `envelope` without deadlocking. Lock order is augs → caps
    /// everywhere that both are taken.
    fn envelope_locked(
        &self,
        augs: &ComponentBounds,
        now: DateTime<Utc>,
        other_axis: f32,
        dynamic: Option<&VecBounds>,
    ) -> VecBounds {
        let mut acc: Option<VecBounds> = None;

        let static_eff = augs.effective_at(now);
        let any_live = augs.has_live_augmentations(now);
        if !(self.rated.is_none() && static_eff.0.is_empty() && !any_live) {
            acc = Some(static_eff);
        }

        if let Some(caps) = *self.caps.lock() {
            let (lo, hi) = caps.q_bounds_at(other_axis);
            let band = VecBounds::single(lo, hi);
            acc = Some(match acc {
                None => band,
                Some(a) => a.intersect(&band),
            });
        }

        if let Some(d) = dynamic {
            acc = Some(match acc {
                None => d.clone(),
                Some(a) => a.intersect(d),
            });
        }

        acc.unwrap_or_default()
    }

    /// rated ∩ caps@other ∩ live augmentations (NO dynamic hook).
    pub fn validation_envelope_at(&self, now: DateTime<Utc>, other_axis: f32) -> VecBounds {
        self.envelope(now, other_axis, None)
    }

    /// validation envelope ∩ the dynamic hook.
    pub fn tracking_envelope_at(
        &self,
        now: DateTime<Utc>,
        other_axis: f32,
        dynamic: Option<&VecBounds>,
    ) -> VecBounds {
        self.envelope(now, other_axis, dynamic)
    }

    /// NaN → OutOfBounds; 0 always accepted (park rule); else must be
    /// inside the validation envelope. On Ok, enqueues into the delay.
    pub fn accept(
        &self,
        value: f32,
        now: DateTime<Utc>,
        other_axis: f32,
    ) -> Result<(), SetpointError> {
        let envelope = self.validation_envelope_at(now, other_axis);
        if !value.is_finite() {
            return Err(SetpointError::OutOfBounds {
                value,
                unit: self.unit,
                envelope,
            });
        }
        // Mirrors ComponentBounds::validate_active_setpoint's park rule
        // exactly — including the absence of an "empty envelope means
        // unconstrained" carve-out. VecBounds::contains on an empty
        // VecBounds is always false, so an empty envelope rejects every
        // nonzero value here, same as any other envelope nothing fits in.
        if value != 0.0 && !envelope.contains(value) {
            return Err(SetpointError::OutOfBounds {
                value,
                unit: self.unit,
                envelope,
            });
        }
        self.delay.set_target(value);
        Ok(())
    }

    /// drop_expired → promote (poll) → re-clamp the retained armed
    /// value into the tracking envelope (park 0 for a 0 value or an
    /// empty envelope) or apply the idle rule → slew → publish → return
    /// the new actual.
    pub fn step(&self, now: DateTime<Utc>, dt: Duration, ctx: StepCtx<'_>) -> f32 {
        self.augs.lock().drop_expired(now);

        // CommandDelay re-returns the armed value on every poll, so
        // re-clamping it here every tick is what makes tighten→follow /
        // re-widen→restore work with no extra state on this side.
        let target = match self.delay.poll(now) {
            Some(armed) => Some(armed),
            None => match ctx.idle {
                IdleTarget::Value(v) if v.is_finite() => Some(v),
                // A non-finite idle target (NaN from a bad
                // dynamic-scalar read, say) must not reach the ramp —
                // Ramp::set_target already drops NaN downstream, but
                // validating at the door here matches accept's posture
                // instead of relying on that as the only net.
                IdleTarget::Value(_) => {
                    log::debug!("PowerAxis::step ignored a non-finite idle target");
                    None
                }
                IdleTarget::Hold => None,
            },
        };
        // No target: leave the ramp target untouched.
        if let Some(v) = target {
            // "For power metrics, regardless of the bounds, 0W is
            // always allowed" (Microgrid API), which is `accept`'s park
            // rule, so 0 is never pulled to a band edge. An empty
            // envelope parks at 0.
            let tracked = if v == 0.0 {
                0.0
            } else {
                let env = self.tracking_envelope_at(now, ctx.other_axis, ctx.dynamic);
                if env.0.is_empty() { 0.0 } else { env.clamp(v) }
            };
            self.ramp.set_target(tracked);
        }

        let mut actual = self.ramp.advance(dt);
        // The dynamic band is physical (the sun a PV array gets, the
        // heat a boiler can take): the ramp climbs toward it, but the
        // output never sits outside it, so a narrowing band cuts the
        // output at once.
        if let Some(band) = ctx.dynamic
            && actual != 0.0
            && !band.contains(actual)
        {
            actual = band.clamp(actual);
            self.ramp.set_actual(actual);
        }
        *self.published.lock() = actual;
        actual
    }

    pub fn augment(&self, ts: DateTime<Utc>, bounds: VecBounds, lifetime: Duration) {
        self.augs.lock().add_augmentation(ts, bounds, lifetime);
    }

    /// Atomic compose-check-insert: one `augs` guard spans computing
    /// what the envelope would become with `bounds` folded in
    /// (rated/caps-only static piece ∩ live augmentations ∩ caps@
    /// `other_axis` ∩ `dynamic` ∩ `bounds`), checking whether that
    /// result is empty, and — only if not — inserting.
    ///
    /// `dynamic` is the component's per-tick derate band, passed
    /// exactly as `step` passes it (`StepCtx::dynamic`): an EV's SoC
    /// clamp, a boiler's `[0, need]`. It is folded into the emptiness
    /// check because an augmentation disjoint from the live derate
    /// would ACK and then park the axis at 0 W for the whole TTL — a
    /// derated EV must not accept a 10–22 kW narrowing. Components
    /// with no derate on this axis (a battery inverter, a solar
    /// inverter — both report `effective_active_bounds()` as rated ∩
    /// augmentations and nothing more) pass `None`.
    ///
    /// `bounds` and `dynamic` are folded in through `envelope_locked`'s
    /// single "dynamic" slot as one pre-intersected piece; set
    /// intersection is associative, so that composes the same envelope
    /// as folding them in separately.
    ///
    /// This is what `augment` above does NOT do: `augment` pushes
    /// unconditionally after a caller has validated against a
    /// separately-locked read. `try_augment` closes exactly that
    /// window: two concurrent callers with mutually disjoint bands
    /// can no longer both validate against the same pre-state, both
    /// apply, and leave the composed envelope empty between them.
    ///
    /// On `Err` nothing was mutated and the returned `VecBounds` is
    /// the CURRENT pre-insert envelope — static ∩ live augmentations ∩
    /// caps@`other_axis` ∩ `dynamic`, i.e. what the caller could still
    /// command right now — NOT the composed result, which is
    /// necessarily empty on this path and so says nothing a constant
    /// couldn't. (The current envelope can itself be empty when the
    /// axis is already boxed in, e.g. a caps band that shrank to
    /// nothing; the payload is then honest about that rather than
    /// uninformative by construction.)
    ///
    /// What this does NOT close: the envelope can still go empty
    /// later with no race involved at all, same as before this fix.
    /// A Q axis's caps band is a function of the OTHER axis's live
    /// value (`other_axis`) — it can shrink out from under an
    /// augmentation that was perfectly legal when inserted, purely
    /// because P ramped afterward (see
    /// `q_axis_re_clamps_on_p_drift_after_settle` and the caps-shrink
    /// path `a_disjoint_q_augmentation_is_rejected_at_zero_headroom`
    /// exercises in `tests/grpc.rs`). The same goes for `dynamic`: it
    /// is a snapshot taken by the caller before this call, and the
    /// next tick can move it. Either case can park `step`'s tracking
    /// target at 0 well after this call returned `Ok`.
    pub fn try_augment(
        &self,
        ts: DateTime<Utc>,
        bounds: VecBounds,
        lifetime: Duration,
        other_axis: f32,
        dynamic: Option<&VecBounds>,
    ) -> Result<(), VecBounds> {
        let mut augs = self.augs.lock();
        let probe = match dynamic {
            Some(d) => bounds.intersect(d),
            None => bounds.clone(),
        };
        if self
            .envelope_locked(&augs, ts, other_axis, Some(&probe))
            .0
            .is_empty()
        {
            return Err(self.envelope_locked(&augs, ts, other_axis, dynamic));
        }
        augs.add_augmentation(ts, bounds, lifetime);
        Ok(())
    }

    /// True while at least one unexpired augmentation is narrowing
    /// this axis — the inspector's "augmented" badge. Delegates to
    /// `ComponentBounds::has_live_augmentations`, the same live/expired
    /// predicate `effective_at` already applies to the queue.
    pub fn augmented(&self, now: DateTime<Utc>) -> bool {
        self.augs.lock().has_live_augmentations(now)
    }

    /// delay.reset + ramp.set_target(park) + publish `park`. The
    /// published write is for Q axes, whose telemetry reads the slot:
    /// a reset zeroes the reported Q immediately instead of waiting a
    /// tick for `step` to republish the ramp. P-side consumers never
    /// read `published` (they report `actual()` or their own measured
    /// value), so the write is inert there.
    pub fn reset(&self, park: f32) {
        self.delay.reset();
        self.ramp.set_target(park);
        *self.published.lock() = park;
    }

    /// ramp.snap_to(v) alone: the live output jumps to `v` with no
    /// slew, while the command delay AND the published value are left
    /// exactly as they were — so an armed command survives the snap
    /// and is still there to be ramped back to. Use it over
    /// [`Self::trip`] whenever the output must move now but the
    /// standing command must outlive the move; callers say in their
    /// own comments why they want that.
    pub fn snap_output(&self, v: f32) {
        self.ramp.snap_to(v);
    }

    /// delay.reset + ramp.snap_to(0) + publish 0 — the health-trip snap.
    pub fn trip(&self) {
        self.delay.reset();
        self.ramp.snap_to(0.0);
        *self.published.lock() = 0.0;
    }

    pub fn override_published(&self, v: f32) {
        *self.published.lock() = v;
    }

    pub fn published(&self) -> f32 {
        *self.published.lock()
    }

    /// ramp.actual()
    pub fn actual(&self) -> f32 {
        self.ramp.actual()
    }

    /// delay.armed()
    pub fn armed(&self) -> Option<f32> {
        self.delay.armed()
    }

    /// Static side only: rated ∩ augmentations (telemetry's
    /// effective_active_bounds shape). Empty VecBounds when rated is None
    /// and no augmentations are live.
    pub fn effective_static(&self) -> VecBounds {
        self.effective_static_at(Utc::now())
    }

    pub fn effective_static_at(&self, now: DateTime<Utc>) -> VecBounds {
        self.augs.lock().effective_at(now)
    }

    /// No-op + debug log when caps is None.
    pub fn set_pf_limit(&self, pf: Option<f32>) {
        match &mut *self.caps.lock() {
            Some(c) => c.pf_limit = pf,
            None => log::debug!("PowerAxis::set_pf_limit ignored: axis has no reactive caps"),
        }
    }

    /// No-op + debug log when caps is None.
    pub fn set_apparent_va(&self, va: Option<f32>) {
        match &mut *self.caps.lock() {
            Some(c) => c.apparent_va = va,
            None => log::debug!("PowerAxis::set_apparent_va ignored: axis has no reactive caps"),
        }
    }

    pub fn capability(&self) -> Option<ReactiveCapability> {
        *self.caps.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::common::metrics::Bounds;

    /// A Q axis's per-tick context: the live P as the other axis, no
    /// dynamic band, and hold when nothing is armed.
    fn q_ctx(p_live: f32) -> StepCtx<'static> {
        StepCtx {
            other_axis: p_live,
            dynamic: None,
            idle: IdleTarget::Hold,
        }
    }

    /// The `(lower, upper)` Q allowance at `p_live` — the first band
    /// of the validation envelope.
    fn q_bounds(ax: &PowerAxis, p_live: f32) -> (f32, f32) {
        let env = ax.validation_envelope_at(Utc::now(), p_live);
        let b = env.0.first().expect("a caps band is always one bucket");
        (b.lower.unwrap(), b.upper.unwrap())
    }

    fn q_axis(caps: ReactiveCapability, command_delay: Duration, ramp: f32) -> PowerAxis {
        PowerAxis::new(AxisConfig {
            rated: None,
            caps: Some(caps),
            command_delay,
            ramp_rate_per_s: ramp,
            unit: "VAr",
        })
    }

    // P-flavored axis: rated bounds + augmentation TTL + re-clamp.
    #[test]
    fn armed_target_follows_a_tightening_envelope_and_restores() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((-10_000.0, 10_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        ax.accept(8_000.0, t0, 0.0).unwrap();
        fn ctx(d: Option<&VecBounds>) -> StepCtx<'_> {
            StepCtx {
                other_axis: 0.0,
                dynamic: d,
                idle: IdleTarget::Hold,
            }
        }
        assert_eq!(ax.step(t0, Duration::from_secs(1), ctx(None)), 8_000.0);
        // Tighten via a 2 s augmentation → follows; expire → restores.
        ax.augment(
            t0,
            VecBounds::single(-3_000.0, 3_000.0),
            Duration::from_secs(2),
        );
        assert_eq!(
            ax.step(
                t0 + chrono::Duration::seconds(1),
                Duration::from_secs(1),
                ctx(None)
            ),
            3_000.0
        );
        assert_eq!(
            ax.step(
                t0 + chrono::Duration::seconds(3),
                Duration::from_secs(1),
                ctx(None)
            ),
            8_000.0
        );
    }

    /// 0 W is always allowed, whatever the bounds (Microgrid API): a 0
    /// setpoint, or an idle 0, holds 0 inside an exclusion gap instead
    /// of being pulled to the nearest band edge; any other value still
    /// is.
    #[test]
    fn zero_holds_inside_an_exclusion_gap() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((-10_000.0, 10_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
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
        ax.augment(t0, gap, Duration::from_secs(60));
        let ctx = |idle| StepCtx {
            other_axis: 0.0,
            dynamic: None,
            idle,
        };
        let dt = Duration::from_secs(1);
        assert_eq!(ax.step(t0, dt, ctx(IdleTarget::Value(0.0))), 0.0);
        ax.accept(0.0, t0, 0.0).unwrap();
        assert_eq!(ax.step(t0, dt, ctx(IdleTarget::Hold)), 0.0);
        ax.accept(3_000.0, t0, 0.0).unwrap();
        assert_eq!(ax.step(t0, dt, ctx(IdleTarget::Hold)), 3_000.0);
        // A later tightening still pulls a non-zero value to the edge.
        ax.augment(
            t0,
            VecBounds::single(-10_000.0, 2_000.0),
            Duration::from_secs(60),
        );
        assert_eq!(ax.step(t0, dt, ctx(IdleTarget::Hold)), 2_000.0);
    }

    #[test]
    fn empty_tracking_envelope_parks_at_zero_but_idle_hold_is_untouched() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((0.0, 22_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        // Armed target vs a dynamic envelope that doesn't intersect it → park 0.
        ax.accept(10_000.0, t0, 0.0).unwrap();
        let derate = VecBounds::single(30_000.0, 40_000.0); // disjoint from rated
        assert_eq!(
            ax.step(
                t0,
                Duration::from_secs(1),
                StepCtx {
                    other_axis: 0.0,
                    dynamic: Some(&derate),
                    idle: IdleTarget::Hold
                }
            ),
            0.0
        );
        // Unarmed + zero-excluding augmentation: Hold means 0 stays 0.
        let ax2 = PowerAxis::new(AxisConfig {
            rated: Some((0.0, 22_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        ax2.augment(
            t0,
            VecBounds::single(5_000.0, 22_000.0),
            Duration::from_secs(60),
        );
        assert_eq!(
            ax2.step(
                t0,
                Duration::from_secs(1),
                StepCtx {
                    other_axis: 0.0,
                    dynamic: None,
                    idle: IdleTarget::Hold
                }
            ),
            0.0
        );
    }

    #[test]
    fn q_axis_validates_against_caps_and_augmentations_but_not_dynamic() {
        let ax = PowerAxis::new(AxisConfig {
            rated: None,
            caps: Some(ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(5_000.0),
            }),
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "VAr",
        });
        let t0 = Utc::now();
        // At P=3000, kVA circle allows |Q| ≤ 4000.
        assert!(ax.accept(4_500.0, t0, 3_000.0).is_err());
        ax.accept(3_500.0, t0, 3_000.0).unwrap();
        // A TTL augmentation narrows the Q envelope — the spec's failing RPC case.
        ax.augment(
            t0,
            VecBounds::single(-1_000.0, 1_000.0),
            Duration::from_secs(60),
        );
        assert!(
            ax.accept(3_500.0, t0 + chrono::Duration::seconds(1), 3_000.0)
                .is_err()
        );
        let err = ax.accept(f32::NAN, t0, 3_000.0).unwrap_err();
        assert!(matches!(
            err,
            SetpointError::OutOfBounds { unit: "VAr", .. }
        ));
        // 0 always accepted.
        ax.accept(0.0, t0, 3_000.0).unwrap();
    }

    /// A fully-unbounded augmentation band (both proto edges absent =
    /// "no bound") must read as no extra constraint — even stacked.
    /// Two of them used to intersect into the disjoint sentinel and
    /// empty the Q envelope, parking the axis at 0 and bouncing every
    /// later augmentation for their whole lifetime.
    #[test]
    fn q_axis_survives_stacked_unbounded_augmentations() {
        let ax = q_axis(
            ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(5_000.0),
            },
            Duration::ZERO,
            f32::INFINITY,
        );
        let t0 = Utc::now();
        let unbounded = VecBounds::new(vec![crate::proto::common::metrics::Bounds {
            lower: None,
            upper: None,
        }]);
        ax.augment(t0, unbounded.clone(), Duration::from_secs(60));
        ax.augment(t0, unbounded, Duration::from_secs(60));
        // The envelope is still the caps band: |Q| ≤ 4000 at P=3000.
        ax.accept(3_500.0, t0 + chrono::Duration::seconds(1), 3_000.0)
            .unwrap();
        assert!(
            ax.accept(4_500.0, t0 + chrono::Duration::seconds(1), 3_000.0)
                .is_err(),
            "caps still bind — unbounded augmentations widen nothing"
        );
    }

    #[test]
    fn idle_value_tracks_and_clamps() {
        // Solar-shaped: unarmed axis tracks the provided idle value,
        // clamped by an augmentation cap.
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((-30_000.0, 0.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        assert_eq!(
            ax.step(
                t0,
                Duration::from_secs(1),
                StepCtx {
                    other_axis: 0.0,
                    dynamic: None,
                    idle: IdleTarget::Value(-6_000.0)
                }
            ),
            -6_000.0
        );
        ax.augment(
            t0,
            VecBounds::single(-2_000.0, 0.0),
            Duration::from_secs(60),
        );
        assert_eq!(
            ax.step(
                t0 + chrono::Duration::seconds(1),
                Duration::from_secs(1),
                StepCtx {
                    other_axis: 0.0,
                    dynamic: None,
                    idle: IdleTarget::Value(-6_000.0)
                }
            ),
            -2_000.0
        );
    }

    /// The dynamic band is a physical limit: when it narrows (a cloud
    /// cuts the sun a PV array gets) the output drops to it at once,
    /// while a widening band is still climbed at the ramp rate.
    #[test]
    fn output_never_sits_outside_the_dynamic_band() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((-30_000.0, 0.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: 2_000.0,
            unit: "W",
        });
        let t0 = Utc::now();
        let sun = |avail: f32, ax: &PowerAxis| {
            let band = VecBounds::single(avail, 0.0);
            ax.step(
                t0,
                Duration::from_millis(100),
                StepCtx {
                    other_axis: 0.0,
                    dynamic: Some(&band),
                    idle: IdleTarget::Value(avail),
                },
            )
        };
        for _ in 0..160 {
            sun(-30_000.0, &ax);
        }
        assert_eq!(sun(-30_000.0, &ax), -30_000.0);
        assert_eq!(sun(-6_000.0, &ax), -6_000.0);
        assert_eq!(sun(-30_000.0, &ax), -6_200.0);
    }

    #[test]
    fn trip_snaps_and_reset_parks() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((-10_000.0, 10_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: 1_000.0,
            unit: "W",
        });
        let t0 = Utc::now();
        ax.accept(5_000.0, t0, 0.0).unwrap();
        ax.step(
            t0,
            Duration::from_secs(2),
            StepCtx {
                other_axis: 0.0,
                dynamic: None,
                idle: IdleTarget::Hold,
            },
        ); // → 2000
        ax.trip();
        assert_eq!(ax.actual(), 0.0);
        assert_eq!(ax.published(), 0.0);
        assert_eq!(ax.armed(), None);
        // reset(park) re-targets without snapping.
        ax.accept(5_000.0, t0, 0.0).unwrap();
        ax.step(
            t0,
            Duration::from_secs(1),
            StepCtx {
                other_axis: 0.0,
                dynamic: None,
                idle: IdleTarget::Hold,
            },
        ); // → 1000
        ax.reset(0.0);
        // The park is published immediately — a Q reader between the
        // reset and the next tick sees 0, not the pre-reset value.
        assert_eq!(ax.published(), 0.0);
        let v = ax.step(
            t0,
            Duration::from_secs(1) / 2,
            StepCtx {
                other_axis: 0.0,
                dynamic: None,
                idle: IdleTarget::Hold,
            },
        );
        assert!(
            (v - 500.0).abs() < 1.0,
            "ramps toward the park value, no snap: {v}"
        );
    }

    /// A P axis whose static effective bounds are emptied by a
    /// disjoint augmentation must reject every nonzero value, not
    /// silently accept anything — an empty envelope is a real
    /// constraint (nothing fits), not "unconstrained". 0 stays
    /// accepted via the park rule regardless.
    #[test]
    fn accept_rejects_everything_when_static_bounds_are_emptied_by_augmentation() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((0.0, 100.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        ax.augment(t0, VecBounds::single(200.0, 300.0), Duration::from_secs(60));
        assert!(ax.accept(150.0, t0, 0.0).is_err());
        assert!(ax.accept(0.0, t0, 0.0).is_ok());
    }

    /// Same bug, Q-axis flavor: a caps band emptied by a disjoint
    /// augmentation must reject every nonzero value too.
    #[test]
    fn accept_rejects_everything_when_caps_band_is_emptied_by_augmentation() {
        let ax = PowerAxis::new(AxisConfig {
            rated: None,
            caps: Some(ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(4_000.0),
            }),
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "VAr",
        });
        let t0 = Utc::now();
        // At P=0 the kVA cap allows ±4000; a disjoint [5000, 6000]
        // augmentation intersects it down to nothing.
        ax.augment(
            t0,
            VecBounds::single(5_000.0, 6_000.0),
            Duration::from_secs(60),
        );
        assert!(ax.accept(3_500.0, t0, 0.0).is_err());
    }

    /// Two live Q augmentations that are mutually disjoint leave no
    /// legal band at all. "Live, but nothing fits" is a real
    /// constraint — it is NOT the "no augmentation live" case a Q axis
    /// skips because it has no static band of its own. Skipping it
    /// would silently revert the axis to the bare caps band and let
    /// through values neither augmentation permits, so `accept` must
    /// reject every nonzero value and `step` must park at 0.
    #[test]
    fn mutually_disjoint_live_q_augmentations_leave_no_legal_band() {
        let ax = q_axis(
            ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(5_000.0),
            },
            Duration::ZERO,
            f32::INFINITY,
        );
        let t0 = Utc::now();
        // Baseline: at P = 0 the caps band is ±5 kVAr and 2 kVAr rides it.
        ax.accept(2_000.0, t0, 0.0).unwrap();
        assert_eq!(ax.step(t0, Duration::from_secs(1), q_ctx(0.0)), 2_000.0);

        // Two live augmentations with no overlap between them.
        ax.augment(
            t0,
            VecBounds::single(-4_000.0, -3_000.0),
            Duration::from_secs(60),
        );
        ax.augment(
            t0,
            VecBounds::single(-500.0, 500.0),
            Duration::from_secs(60),
        );

        let env = ax.validation_envelope_at(t0, 0.0);
        assert!(
            env.0.is_empty(),
            "disjoint live augmentations leave nothing legal, got {env}"
        );
        // Neither augmentation's own band is accepted either — there is
        // no value both permit.
        assert!(ax.accept(2_000.0, t0, 0.0).is_err());
        assert!(ax.accept(-3_500.0, t0, 0.0).is_err());
        assert!(ax.accept(400.0, t0, 0.0).is_err());
        // The armed 2 kVAr is re-clamped to the park, not to the caps band.
        assert_eq!(
            ax.step(
                t0 + chrono::Duration::seconds(1),
                Duration::from_secs(1),
                q_ctx(0.0)
            ),
            0.0
        );
        // The park rule survives regardless.
        assert!(ax.accept(0.0, t0, 0.0).is_ok());
    }

    /// Two disjoint augmentations no longer stack into an empty
    /// envelope: the second is rejected atomically and the first stays
    /// in force.
    #[test]
    fn try_augment_rejects_a_band_disjoint_with_live_augmentations() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((-10_000.0, 10_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        assert!(
            ax.try_augment(
                t0,
                VecBounds::single(5_000.0, 8_000.0),
                Duration::from_secs(60),
                0.0,
                None
            )
            .is_ok(),
            "the first augmentation overlaps the rated band"
        );
        let current = ax
            .try_augment(
                t0,
                VecBounds::single(-8_000.0, -5_000.0),
                Duration::from_secs(60),
                0.0,
                None,
            )
            .expect_err("disjoint from the live [5000, 8000] augmentation — rejected, not stacked");
        // The `Err` payload is the CURRENT pre-insert envelope — what
        // the caller could still command — not the composed result,
        // which is empty by construction on this path and would name
        // nothing.
        assert_eq!(current.0.len(), 1, "expected one band, got {current}");
        assert_eq!(
            (current.0[0].lower, current.0[0].upper),
            (Some(5_000.0), Some(8_000.0)),
            "the rejection must name the live [5000, 8000] envelope, got {current}"
        );
        // The first augmentation is still exactly what's in force.
        let eff = ax.effective_static_at(t0);
        assert_eq!(eff.0.len(), 1);
        assert_eq!(
            (eff.0[0].lower, eff.0[0].upper),
            (Some(5_000.0), Some(8_000.0))
        );
        assert!(ax.accept(6_000.0, t0, 0.0).is_ok());
    }

    /// The concurrent twin of the test above: two threads race
    /// `try_augment` with genuinely disjoint bands against a shared,
    /// fresh axis. The `augs` lock must span compose, check, AND
    /// insert as one critical section — if compose and insert were
    /// two separate lock acquisitions (the exact shape the gateway's
    /// original race had, just relocated into the axis instead of
    /// fixed), both threads could compose against the same
    /// pre-insert state, both see a non-empty result, both insert,
    /// and leave two live mutually disjoint augmentations behind —
    /// the bug this task closes. Looped for scheduling variety rather
    /// than trusting a single race window to land.
    #[test]
    fn try_augment_races_leave_exactly_one_winner() {
        for i in 0..50 {
            let ax = std::sync::Arc::new(PowerAxis::new(AxisConfig {
                rated: Some((-10_000.0, 10_000.0)),
                caps: None,
                command_delay: Duration::ZERO,
                ramp_rate_per_s: f32::INFINITY,
                unit: "W",
            }));
            let t0 = Utc::now();

            let ax1 = ax.clone();
            let h1 = std::thread::spawn(move || {
                ax1.try_augment(
                    t0,
                    VecBounds::single(5_000.0, 8_000.0),
                    Duration::from_secs(60),
                    0.0,
                    None,
                )
            });
            let ax2 = ax.clone();
            let h2 = std::thread::spawn(move || {
                ax2.try_augment(
                    t0,
                    VecBounds::single(-8_000.0, -5_000.0),
                    Duration::from_secs(60),
                    0.0,
                    None,
                )
            });
            let r1 = h1.join().unwrap();
            let r2 = h2.join().unwrap();

            let ok_count = r1.is_ok() as u8 + r2.is_ok() as u8;
            assert_eq!(
                ok_count, 1,
                "iteration {i}: exactly one of two mutually disjoint \
                 augments must win, got {r1:?} / {r2:?}"
            );

            let eff = ax.effective_static_at(t0);
            assert_eq!(
                eff.0.len(),
                1,
                "iteration {i}: the winner's band must be the sole \
                 survivor, got {eff}"
            );
            let winner = if r1.is_ok() {
                (5_000.0, 8_000.0)
            } else {
                (-8_000.0, -5_000.0)
            };
            assert_eq!(
                (eff.0[0].lower, eff.0[0].upper),
                (Some(winner.0), Some(winner.1)),
                "iteration {i}: effective_static must contain exactly \
                 the winner's band"
            );
        }
    }

    /// The `dynamic` derate band is part of the emptiness check, not
    /// just of `step`'s tracking envelope: an augmentation disjoint
    /// from the live derate is rejected (it would otherwise ACK and
    /// then park the axis at 0 W for the whole TTL), and the `Err`
    /// names the derated envelope the caller can still command. An
    /// augmentation that overlaps the derate still goes in.
    #[test]
    fn try_augment_rejects_a_band_disjoint_with_the_dynamic_derate() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((0.0, 22_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        // The component is derated to [0, 2000] (an EV near its SoC
        // ceiling, a boiler with little headroom).
        let derate = VecBounds::single(0.0, 2_000.0);

        let current = ax
            .try_augment(
                t0,
                VecBounds::single(10_000.0, 22_000.0),
                Duration::from_secs(60),
                0.0,
                Some(&derate),
            )
            .expect_err("[10000, 22000] is disjoint from the [0, 2000] derate");
        assert_eq!(current.0.len(), 1, "expected one band, got {current}");
        assert_eq!(
            (current.0[0].lower, current.0[0].upper),
            (Some(0.0), Some(2_000.0)),
            "the rejection must name the derated envelope, got {current}"
        );
        // Nothing was inserted: the static side is still the raw rated band.
        let eff = ax.effective_static_at(t0);
        assert_eq!(
            (eff.0[0].lower, eff.0[0].upper),
            (Some(0.0), Some(22_000.0))
        );

        // A band that overlaps the derate is still accepted.
        assert!(
            ax.try_augment(
                t0,
                VecBounds::single(1_000.0, 5_000.0),
                Duration::from_secs(60),
                0.0,
                Some(&derate),
            )
            .is_ok(),
            "[1000, 5000] overlaps the [0, 2000] derate"
        );
    }

    #[test]
    fn q_axis_accept_then_step_drives_to_target() {
        // No PF cap, ±10 kVA envelope. 100 ms delay, 1000 VAR/s slew.
        let p = q_axis(
            ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(10_000.0),
            },
            Duration::from_millis(100),
            1000.0,
        );
        let now = Utc::now();
        // P = 0; envelope is ±10 kVA. Q=5000 is in-range.
        assert!(p.accept(5000.0, now, 0.0).is_ok());

        // Before the 100 ms delay elapses, no movement.
        let q = p.step(
            now + chrono::Duration::milliseconds(50),
            Duration::from_millis(50),
            q_ctx(0.0),
        );
        assert!(q.abs() < 1.0, "expected ~0 before delay, got {q}");

        // After 1 s past the delay, ramp at 1000 VAR/s reaches 1 kVAR.
        let q = p.step(
            now + chrono::Duration::milliseconds(1100),
            Duration::from_millis(1000),
            q_ctx(0.0),
        );
        assert!((q - 1000.0).abs() < 1.0, "expected ~1000, got {q}");

        // 6 s past the delay: ramp settled at 5000.
        let q = p.step(
            now + chrono::Duration::milliseconds(6100),
            Duration::from_millis(5000),
            q_ctx(0.0),
        );
        assert!((q - 5000.0).abs() < 1.0, "expected 5000, got {q}");
        assert_eq!(p.published(), q);
    }

    #[test]
    fn q_axis_rejects_out_of_envelope() {
        let p = q_axis(
            ReactiveCapability {
                pf_limit: Some(0.5),
                apparent_va: None,
            },
            Duration::ZERO,
            f32::INFINITY,
        );
        // PF=0.5 at P=10k → ±5000 envelope. 6000 is out.
        match p.accept(6000.0, Utc::now(), 10_000.0) {
            Err(SetpointError::OutOfBounds {
                value, envelope, ..
            }) => {
                assert_eq!(value, 6000.0);
                let b = envelope.0.first().expect("single-bucket envelope");
                let (lower, upper) = (b.lower.unwrap(), b.upper.unwrap());
                assert!((lower + 5000.0).abs() < 1.0);
                assert!((upper - 5000.0).abs() < 1.0);
            }
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn q_axis_re_clamps_at_promotion() {
        // Cap = ±10 kVA. No delay, no slew. Accept at P=0 (full
        // envelope), then have P drift to 9 kW before tick — the
        // command should be re-clamped to √(100-81)*1000 ≈ ±4359.
        let p = q_axis(
            ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(10_000.0),
            },
            Duration::ZERO,
            f32::INFINITY,
        );
        assert!(p.accept(8000.0, Utc::now(), 0.0).is_ok());
        let q = p.step(Utc::now(), Duration::from_millis(100), q_ctx(9000.0));
        assert!(q < 4400.0 && q > 4350.0, "expected ~4359, got {q}");
    }

    /// P drifting AFTER a Q command settled must also re-clamp: the
    /// delay queue keeps returning the armed value on every poll, so
    /// each step re-clamps it to the live envelope at the current P —
    /// a sustained Volt/VAR setpoint can't push apparent power past
    /// the kVA rating when active power later rises.
    #[test]
    fn q_axis_re_clamps_on_p_drift_after_settle() {
        let p = q_axis(
            ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(10_000.0),
            },
            Duration::ZERO,
            f32::INFINITY,
        );
        assert!(p.accept(8000.0, Utc::now(), 0.0).is_ok());
        // Settles at the full 8 kVAR while P = 0.
        let q = p.step(Utc::now(), Duration::from_millis(100), q_ctx(0.0));
        assert!((q - 8000.0).abs() < 1.0, "expected 8000, got {q}");
        // P rises to 9 kW under the settled Q — the next tick clamps
        // to √(100−81)·1000 ≈ 4359.
        let q = p.step(Utc::now(), Duration::from_millis(100), q_ctx(9000.0));
        assert!(q < 4400.0 && q > 4350.0, "expected ~4359, got {q}");
        // P falls back — the original commanded Q is restored.
        let q = p.step(Utc::now(), Duration::from_millis(100), q_ctx(0.0));
        assert!((q - 8000.0).abs() < 1.0, "expected 8000 back, got {q}");
    }

    #[test]
    fn q_axis_override_published_wins() {
        // step() publishes the ramp's value; override_published
        // overwrites it. Mirrors the BatteryInverter's "measured = sum
        // of children's accepted" path.
        let p = q_axis(
            ReactiveCapability::microsim_default(),
            Duration::ZERO,
            f32::INFINITY,
        );
        let _ = p.accept(3000.0, Utc::now(), 10_000.0);
        let q = p.step(Utc::now(), Duration::from_millis(100), q_ctx(10_000.0));
        assert!((q - 3000.0).abs() < 1.0);
        p.override_published(2500.0);
        assert_eq!(p.published(), 2500.0);
    }

    #[test]
    fn q_axis_runtime_caps() {
        let p = q_axis(
            ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(10_000.0),
            },
            Duration::ZERO,
            f32::INFINITY,
        );
        // Initial: at P=0, Q can be ±10k.
        assert!((q_bounds(&p, 0.0).1 - 10_000.0).abs() < 1.0);
        // Drop kVA cap, add tight PF cap: at P=2k, Q ≤ 0.2×2k = ±400.
        p.set_apparent_va(None);
        p.set_pf_limit(Some(0.2));
        let (lo, hi) = q_bounds(&p, 2000.0);
        assert!((hi - 400.0).abs() < 1.0);
        assert!((lo + 400.0).abs() < 1.0);
    }

    /// A non-finite idle target (e.g. a bad dynamic-scalar read) must
    /// not move the ramp — treated the same as `IdleTarget::Hold`.
    #[test]
    fn idle_value_non_finite_is_treated_as_hold() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((-10_000.0, 10_000.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        // Establish a non-zero ramp target via a normal idle value first.
        let v = ax.step(
            t0,
            Duration::from_secs(1),
            StepCtx {
                other_axis: 0.0,
                dynamic: None,
                idle: IdleTarget::Value(4_000.0),
            },
        );
        assert_eq!(v, 4_000.0);
        // A NaN idle target must not move the ramp target.
        let v = ax.step(
            t0,
            Duration::from_secs(1),
            StepCtx {
                other_axis: 0.0,
                dynamic: None,
                idle: IdleTarget::Value(f32::NAN),
            },
        );
        assert_eq!(v, 4_000.0);
    }

    /// `augmented` tracks the same live/expired split as `accept`'s
    /// static envelope: false before any augmentation, true while one
    /// is live, false again once its TTL has elapsed.
    #[test]
    fn augmented_reflects_live_augmentations_and_expiry() {
        let ax = PowerAxis::new(AxisConfig {
            rated: Some((0.0, 100.0)),
            caps: None,
            command_delay: Duration::ZERO,
            ramp_rate_per_s: f32::INFINITY,
            unit: "W",
        });
        let t0 = Utc::now();
        assert!(!ax.augmented(t0));
        ax.augment(t0, VecBounds::single(200.0, 300.0), Duration::from_secs(60));
        assert!(ax.augmented(t0));
        assert!(!ax.augmented(t0 + chrono::Duration::seconds(120)));
    }
}
