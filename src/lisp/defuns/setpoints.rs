//! `(set-active-power)` and `(set-reactive-power)` — apply a setpoint
//! on one power axis through the microgrid's gateway, like gRPC's
//! `SetElectricalComponentPower`, and arm its request lifetime. They
//! skip the gRPC fault gates and the setpoint journal.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tulisp::{Error, TulispContext};

use crate::sim::gateway::Mode;
use crate::sim::microgrids::SharedSiteRouter;
use crate::timeout_tracker::SetpointAxis;

use super::super::Metadata;

/// Lower bound on a non-zero request lifetime the setpoint defuns
/// install. Expiry runs on the physics tick (100 ms by default), so a
/// shorter lifetime can expire before the component ever acts on the
/// command. `0` is kept as "expire at once" and bypasses the floor.
const MIN_SETPOINT_LIFETIME_MS: u64 = 150;

/// The request lifetime for a `LIFETIME-MS` argument: omitted falls
/// back to `default-request-lifetime-ms`, `0` expires at once, any
/// other value is floored at [`MIN_SETPOINT_LIFETIME_MS`].
fn lifetime_from_arg(lifetime_ms: Option<i64>, metadata: &RwLock<Metadata>) -> Duration {
    lifetime_ms
        .map(|ms| {
            let raw = ms.max(0) as u64;
            let clamped = if raw == 0 {
                0
            } else {
                raw.max(MIN_SETPOINT_LIFETIME_MS)
            };
            Duration::from_millis(clamped)
        })
        .unwrap_or_else(|| metadata.read().default_request_lifetime)
}

/// Shared body of the two defuns: one gateway command on `axis`.
#[expect(clippy::too_many_arguments, reason = "one body for both defuns")]
fn set_power(
    router: &SharedSiteRouter,
    metadata: &RwLock<Metadata>,
    name: &str,
    axis: SetpointAxis,
    id: i64,
    value: f64,
    lifetime_ms: Option<i64>,
    clamp: Option<bool>,
) -> Result<bool, Error> {
    let w = router.site();
    let mode = if clamp.unwrap_or(false) {
        Mode::Clamp
    } else {
        Mode::Reject
    };
    let lifetime = lifetime_from_arg(lifetime_ms, metadata);
    w.gateway()
        .set_power(
            axis,
            id as u64,
            w.run_generation(),
            value as f32,
            lifetime,
            mode,
        )
        .map_err(|e| Error::invalid_argument(format!("{name}: {e}")))?;
    Ok(true)
}

/// `(set-active-power ID WATTS &OPTIONAL LIFETIME-MS CLAMP)` — apply
/// an active-power setpoint through the gateway and arm its request
/// lifetime. Returns `t`; signals an error if the component doesn't
/// exist, takes no active setpoint, or the value is refused.
///
/// `LIFETIME-MS` is how long the setpoint stands before the physics
/// step expires it and the axis ramps back to idle. Omitted falls
/// back to `default-request-lifetime-ms`; `0` expires at once; any
/// other value is floored at 150 ms.
///
/// `CLAMP` (default nil) — when non-nil, a value outside the setpoint
/// envelope (the component's own bounds intersected with its
/// children's) is clamped into it and applied instead of refused, and
/// an empty envelope clamps to 0: the primitive an in-sim controller
/// scripted with `(every …)` uses to command "max within whatever cap
/// the limiter allows" each tick. 0 W (the fail-safe park) is applied
/// as-is either way.
///
/// `(set-reactive-power ID VARS &OPTIONAL LIFETIME-MS CLAMP)` — the
/// same on the reactive axis, in VAr, against the reactive envelope:
/// the component's live Q band (its PF / apparent-power caps at its
/// current active power, ∩ any live augmentation) narrowed by
/// whatever Q bounds its children report. 0 VAr always passes.
pub(super) fn register(
    ctx: &mut TulispContext,
    router: SharedSiteRouter,
    metadata: Arc<RwLock<Metadata>>,
) {
    for (name, axis) in [
        ("set-active-power", SetpointAxis::Active),
        ("set-reactive-power", SetpointAxis::Reactive),
    ] {
        let (r, m) = (router.clone(), metadata.clone());
        ctx.defun(
            name,
            move |id: i64,
                  value: f64,
                  lifetime_ms: Option<i64>,
                  clamp: Option<bool>|
                  -> Result<bool, Error> {
                set_power(&r, &m, name, axis, id, value, lifetime_ms, clamp)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::super::test_support::config_with;
    use crate::timeout_tracker::SetpointAxis;

    const DT: Duration = Duration::from_millis(100);

    /// set-active-power applies a setpoint and arms its request
    /// lifetime in the gateway. We verify both: the lifetime is
    /// armed, and once a 0 ms lifetime elapses the axis ramps back to
    /// idle.
    #[test]
    fn set_active_power_applies_setpoint_and_arms_timeout() {
        let (cfg, _dir) = config_with(
            "(setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :successors (list b1))",
        );
        let site = cfg.site();
        let inv = site.get(2).unwrap();
        // 30-second lifetime — applies the setpoint and arms the
        // tracker; nothing should be expired yet.
        cfg.eval("(set-active-power 2 1500.0 30000)").unwrap();
        assert!(
            site.gateway()
                .remaining_lifetime(2, SetpointAxis::Active)
                .is_some()
        );
        cfg.eval("(set-active-power 2 1500.0 0)").unwrap();
        assert_eq!(
            site.gateway().remaining_lifetime(2, SetpointAxis::Active),
            None
        );
        site.tick_n(3, DT);
        let p = inv.aggregate_power_w(&site);
        assert!(p.abs() < 1.0, "expected reset to 0 W, got {p}");
    }

    /// set-active-power gates against the *intersection* of the
    /// inverter's own bounds and its battery child's bounds — not just
    /// the inverter's own — so a value the inverter alone would accept
    /// but the battery can't is rejected, not silently saturated.
    #[test]
    fn set_active_power_rejects_outside_battery_inverter_intersection() {
        let (cfg, _dir) = config_with(
            // Inverter rated ±5 kW, but its battery only ±1 kW -> the
            // combined envelope is ±1 kW.
            "(setq b1 (%make-battery :id 1 :rated-lower -1000.0 :rated-upper 1000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :successors (list b1))",
        );
        // +3 kW is inside the inverter's own ±5 kW but outside the
        // battery's ±1 kW -> rejected against the intersection.
        let res = cfg.eval("(set-active-power 2 3000.0 30000)");
        assert!(res.is_err(), "expected rejection, got {res:?}");
        assert!(
            res.as_ref().unwrap_err().contains("envelope"),
            "expected 'envelope' in error, got {res:?}"
        );
        // Discharge side mirrors it.
        assert!(cfg.eval("(set-active-power 2 -3000.0 30000)").is_err());
        // Within the ±1 kW intersection is accepted.
        cfg.eval("(set-active-power 2 800.0 30000)").unwrap();
        // 0 W (the fail-safe park) is always accepted.
        cfg.eval("(set-active-power 2 0.0 30000)").unwrap();
    }

    /// With the CLAMP arg, an out-of-envelope setpoint is clamped into
    /// the battery∩inverter envelope and applied instead of rejected —
    /// the primitive an in-sim controller uses to track the live cap.
    #[test]
    fn set_active_power_clamp_arg_clamps_into_envelope() {
        let (cfg, _dir) = config_with(
            // Inverter ±5 kW, battery ±1 kW -> combined envelope ±1 kW.
            "(setq b1 (%make-battery :id 1 :rated-lower -1000.0 :rated-upper 1000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :successors (list b1))",
        );
        // Without clamp, +3 kW is rejected.
        assert!(cfg.eval("(set-active-power 2 3000.0 30000)").is_err());
        // With clamp = t, +3 kW is pulled to the +1 kW edge and applied.
        cfg.eval("(set-active-power 2 3000.0 30000 t)").unwrap();
        let site = cfg.site();
        let inv = site.get(2).unwrap();
        // command-delay is zero and ramp is infinite on the primitive
        // inverter, so one tick settles the commanded power.
        site.tick_n(3, DT);
        let p = inv.aggregate_power_w(&site);
        assert!((p - 1000.0).abs() < 1.0, "expected clamp to +1 kW, got {p}");
        // Discharge side clamps symmetrically.
        cfg.eval("(set-active-power 2 -3000.0 30000 t)").unwrap();
        site.tick_n(3, DT);
        let p = inv.aggregate_power_w(&site);
        assert!((p + 1000.0).abs() < 1.0, "expected clamp to -1 kW, got {p}");
    }

    /// set-active-power on an unknown id surfaces an error, and a setpoint
    /// rejected by the component (e.g. unsupported kind on a meter)
    /// also propagates rather than silently no-op'ing.
    #[test]
    fn set_active_power_rejects_unknown_or_unsupported() {
        let (cfg, _dir) = config_with("(%make-meter :id 1)");
        let res = cfg.eval("(set-active-power 999 1500.0)");
        assert!(res.is_err(), "expected error, got {res:?}");
        assert!(res.unwrap_err().contains("999"));
        // Meter takes no active setpoints — the gateway answers
        // Unsupported, which we surface as a Lisp error.
        let res = cfg.eval("(set-active-power 1 1500.0)");
        assert!(res.is_err(), "expected error, got {res:?}");
    }

    /// A battery-inverter with a 5 kVA apparent-power cap and no PF
    /// limit (the inherited default would pin Q to 0 at idle) has a
    /// ±5 kVAr reactive band at idle. Used by the reactive tests below.
    const REACTIVE_SITE: &str =
        "(setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
         (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                   :reactive-pf-limit 0
                                   :reactive-apparent-va 5000.0
                                   :reactive-command-delay-ms 0
                                   :reactive-ramp-rate 1e9
                                   :successors (list b1))";

    /// set-reactive-power applies a setpoint and arms the *reactive*
    /// axis of the timeout tracker, leaving the active axis alone;
    /// once it elapses, that axis ramps back to idle.
    #[test]
    fn set_reactive_power_applies_setpoint_and_arms_reactive_timeout() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let site = cfg.site();
        let inv = site.get(2).unwrap();
        cfg.eval("(set-reactive-power 2 1500.0 30000)").unwrap();
        assert!(
            site.gateway()
                .remaining_lifetime(2, SetpointAxis::Reactive)
                .is_some()
        );
        cfg.eval("(set-reactive-power 2 1500.0 0)").unwrap();
        assert_eq!(
            site.gateway().remaining_lifetime(2, SetpointAxis::Reactive),
            None
        );
        site.tick_n(3, DT);
        let q = inv.aggregate_reactive_var(&site);
        assert!(q.abs() < 1.0, "expected reactive reset to 0 VAr, got {q}");
    }

    /// Outside the inverter's live reactive band the request is
    /// rejected, like gRPC's SetElectricalComponentPower(Reactive);
    /// inside it, and 0 VAr, are accepted.
    #[test]
    fn set_reactive_power_rejects_outside_band() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let res = cfg.eval("(set-reactive-power 2 6000.0 30000)");
        assert!(res.is_err(), "expected rejection, got {res:?}");
        assert!(cfg.eval("(set-reactive-power 2 -6000.0 30000)").is_err());
        cfg.eval("(set-reactive-power 2 3000.0 30000)").unwrap();
        cfg.eval("(set-reactive-power 2 0.0 30000)").unwrap();
    }

    /// With CLAMP, an out-of-band request is pulled to the band edge
    /// and applied: the published Q settles at ±5 kVAr.
    #[test]
    fn set_reactive_power_clamp_arg_clamps_into_band() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        assert!(cfg.eval("(set-reactive-power 2 6000.0 30000)").is_err());
        cfg.eval("(set-reactive-power 2 6000.0 30000 t)").unwrap();
        let site = cfg.site();
        let inv = site.get(2).unwrap();
        site.tick_n(3, DT);
        let q = inv.telemetry(&site).reactive_power_var.unwrap();
        assert!(
            (q - 5000.0).abs() < 1.0,
            "expected clamp to +5 kVAr, got {q}"
        );
        cfg.eval("(set-reactive-power 2 -6000.0 30000 t)").unwrap();
        site.tick_n(3, DT);
        let q = inv.telemetry(&site).reactive_power_var.unwrap();
        assert!(
            (q + 5000.0).abs() < 1.0,
            "expected clamp to -5 kVAr, got {q}"
        );
    }

    /// A non-finite value (a lambda that divided by zero) is rejected
    /// instead of riding the ramp into telemetry as NaN.
    #[test]
    fn set_reactive_power_rejects_nan() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let res = cfg.eval("(set-reactive-power 2 (/ 0.0 0.0) 30000)");
        assert!(res.is_err(), "expected rejection, got {res:?}");
        assert!(
            cfg.eval("(set-reactive-power 2 (/ 0.0 0.0) 30000 t)")
                .is_err()
        );
    }

    /// A live Q augmentation narrows what `set-reactive-power`
    /// accepts, CLAMP pulls a too-big request down to the narrowed
    /// edge, and once the augmentation expires the wide band is back.
    /// The DSL-visible half of the `AugmentElectricalComponentBounds`
    /// round trip on the reactive axis.
    #[test]
    fn reactive_augmentation_narrows_accepts_and_expires() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        // Wide band to start with: ±5 kVAr at P = 0.
        cfg.eval("(set-reactive-power 2 3000.0 30000)").unwrap();

        let site = cfg.site();
        let inv = site.get(2).unwrap();
        // Narrow Q to ±1 kVAr for two seconds of site time.
        cfg.eval("(augment-reactive-bounds 2 '(-1000 1000) 2000)")
            .unwrap();

        // 3 kVAr no longer fits the live band.
        let res = cfg.eval("(set-reactive-power 2 3000.0 30000)");
        assert!(
            res.is_err(),
            "expected rejection under the augmentation, got {res:?}"
        );
        // With CLAMP it is pulled to the +1 kVAr edge and applied.
        cfg.eval("(set-reactive-power 2 3000.0 30000 t)").unwrap();
        site.tick_n(3, DT);
        let q = inv.telemetry(&site).reactive_power_var.unwrap();
        assert!(
            (q - 1000.0).abs() < 1.0,
            "expected clamp to the augmented +1 kVAr edge, got {q}"
        );

        // Past the augmentation's lifetime the caps band alone
        // applies.
        site.tick_n(20, DT);
        cfg.eval("(set-reactive-power 2 3000.0 30000)")
            .expect("3 kVAr fits the rated band again once the augmentation expires");
    }

    /// The reactive axis carries the same gateway shape as the active
    /// one. An inverter's battery child exposes no Q bounds (reactive
    /// power terminates at the inverter), so the gateway reports no
    /// child envelope and falls through to the component's own band —
    /// which still rejects an out-of-band request, with the same
    /// wording the active arm uses.
    #[test]
    fn reactive_gateway_mirrors_active() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let site = cfg.site();
        let gw = site.gateway();
        // The active side has a combined envelope: the battery child
        // reports DC bounds and they get summed in.
        assert!(
            gw.child_envelope(2, SetpointAxis::Active).is_some(),
            "the battery child reports active bounds, so P has a combined envelope"
        );
        // The reactive side has none: no child reports Q bounds.
        assert!(
            gw.child_envelope(2, SetpointAxis::Reactive).is_none(),
            "a battery exposes no reactive bounds"
        );
        assert_eq!(
            gw.setpoint_envelope(2, SetpointAxis::Reactive)
                .unwrap()
                .to_string(),
            "[-5000, 5000]",
            "with no Q-reporting child the envelope is the inverter's own band"
        );
        // With no gateway envelope, the component's own band decides,
        // and the error reads like the active arm's.
        let res = cfg.eval("(set-reactive-power 2 9000.0 30000)");
        assert!(res.is_err(), "expected rejection, got {res:?}");
        let msg = res.unwrap_err();
        assert!(
            msg.contains("set-reactive-power") && msg.contains("VAr"),
            "expected the set-reactive-power / VAr wording, got {msg:?}"
        );
    }

    /// The moment a child *does* report Q bounds, the reactive
    /// gateway gates on the intersection — exactly like the active
    /// one, down to the "exceeds combined envelope" wording. No
    /// production topology nests a Q-reporting child under an
    /// inverter yet, so this hangs a solar inverter (which does
    /// report a Q band) off the battery inverter to reach the branch.
    /// The battery sibling gives the inverter a real DC sink, so the
    /// clamped Q is published instead of zeroed by the no-sink rule.
    #[test]
    fn reactive_gateway_rejects_outside_the_child_intersection() {
        let (cfg, _dir) = config_with(
            // Battery inverter: ±5 kVAr at P = 0. Its child solar
            // inverter carries a 1 kVA cap -> ±1 kVAr, so the
            // combined Q envelope is ±1 kVAr.
            "(setq pv (%make-solar-inverter :id 3 :sunlight% 0
                                            :rated-lower -1000.0 :rated-upper 0.0
                                            :reactive-pf-limit 0
                                            :reactive-apparent-va 1000.0))
             (setq bat (%make-battery :id 4 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :reactive-pf-limit 0
                                       :reactive-apparent-va 5000.0
                                       :reactive-command-delay-ms 0
                                       :reactive-ramp-rate 1e9
                                       :successors (list pv bat))",
        );
        let site = cfg.site();
        let envelope = site
            .gateway()
            .setpoint_envelope(2, SetpointAxis::Reactive)
            .expect("the solar child reports Q bounds, so there is a combined envelope");
        assert_eq!(envelope.0.len(), 1, "expected one band, got {envelope}");
        assert_eq!(envelope.0[0].lower, Some(-1000.0));
        assert_eq!(envelope.0[0].upper, Some(1000.0));

        // 3 kVAr fits the inverter's own ±5 kVAr but not the
        // intersection — rejected at the gateway, same wording as
        // `set-active-power`'s.
        let res = cfg.eval("(set-reactive-power 2 3000.0 30000)");
        assert!(res.is_err(), "expected rejection, got {res:?}");
        let msg = res.unwrap_err();
        assert!(
            msg.contains("exceeds combined envelope"),
            "expected the active arm's envelope wording, got {msg:?}"
        );
        // 0 VAr (the fail-safe park) still passes.
        cfg.eval("(set-reactive-power 2 0.0 30000)").unwrap();
        // CLAMP pulls it into the combined envelope instead.
        cfg.eval("(set-reactive-power 2 3000.0 30000 t)").unwrap();
        let inv = site.get(2).unwrap();
        site.tick_n(3, DT);
        let q = inv.telemetry(&site).reactive_power_var.unwrap();
        assert!(
            (q - 1000.0).abs() < 1.0,
            "expected clamp to the combined +1 kVAr edge, got {q}"
        );
    }

    /// A Q augmentation that fit the caps band when P was idle is
    /// disjoint from it once P sits at the kVA rim: zero headroom.
    /// CLAMP then pulls any request to 0, which the park rule always
    /// accepts; without CLAMP the request is refused.
    #[test]
    fn set_reactive_power_clamps_to_zero_at_zero_headroom() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let site = cfg.site();
        let inv = site.get(2).unwrap();
        cfg.eval("(augment-reactive-bounds 2 '(-4000 -3000) 60000)")
            .unwrap();
        cfg.eval("(set-active-power 2 5000.0 60000)").unwrap();
        site.tick_n(5, DT);
        assert!((inv.aggregate_power_w(&site) - 5000.0).abs() < 1.0);
        let band = site
            .bounds_of(2, SetpointAxis::Reactive)
            .expect("the inverter publishes Q bounds");
        assert_eq!(band.to_string(), "[0, 0]");
        assert!(cfg.eval("(set-reactive-power 2 3000.0 30000)").is_err());
        cfg.eval("(set-reactive-power 2 3000.0 nil t)").unwrap();
        site.tick_n(3, DT);
        let q = inv.telemetry(&site).reactive_power_var.unwrap();
        assert!(q.abs() < 1.0, "expected clamp to 0 VAr, got {q}");
    }

    /// A non-finite value is refused by the gateway, which names it
    /// as such.
    #[test]
    fn a_non_finite_value_is_refused_as_non_finite() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let err = cfg
            .eval("(set-reactive-power 2 (/ 0.0 0.0) 30000)")
            .unwrap_err();
        assert!(err.contains("non-finite"), "{err}");
    }

    /// A request whose lifetime is 0 never shows: the next physics
    /// step expires it before the component ticks.
    #[test]
    fn a_zero_lifetime_expires_on_the_next_step() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let site = cfg.site();
        cfg.eval("(set-active-power 2 1500.0 0)").unwrap();
        assert_eq!(
            site.gateway().remaining_lifetime(2, SetpointAxis::Active),
            None
        );
        site.tick_n(3, DT);
        assert!(site.get(2).unwrap().aggregate_power_w(&site).abs() < 1.0);
    }

    /// A 150 ms lifetime armed after `tick_n` is stamped on the same
    /// clock the ticks advance: it survives one more 100 ms tick and
    /// is gone once a second has passed.
    #[test]
    fn a_short_lifetime_armed_after_tick_n_runs_on_the_tick_clock() {
        let (cfg, _dir) = config_with(REACTIVE_SITE);
        let site = cfg.site();
        site.tick_n(10, DT);
        cfg.eval("(set-active-power 2 1500.0 150)").unwrap();
        site.tick_n(1, DT);
        assert!(
            site.gateway()
                .remaining_lifetime(2, SetpointAxis::Active)
                .is_some()
        );
        site.tick_n(1, DT);
        assert_eq!(
            site.gateway().remaining_lifetime(2, SetpointAxis::Active),
            None
        );
    }

    /// Unknown ids and components without a reactive axis (a meter)
    /// error out instead of silently no-op'ing.
    #[test]
    fn set_reactive_power_rejects_unknown_or_unsupported() {
        let (cfg, _dir) = config_with("(%make-meter :id 1)");
        let res = cfg.eval("(set-reactive-power 999 100.0)");
        assert!(res.is_err(), "expected error, got {res:?}");
        assert!(res.unwrap_err().contains("999"));
        assert!(cfg.eval("(set-reactive-power 1 100.0)").is_err());
    }
}
