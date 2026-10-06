//! Read-side DSL primitives for in-sim controllers.
//!
//! A scenario can already *perturb* the world (`set-meter-power`,
//! `set-solar-sunlight`, …) and *actuate* a component
//! (`set-active-power`), but until now it couldn't *sense* live
//! state, so a scripted controller had to command blindly. These
//! getters close that gap: a controller reads a component's current
//! active power and the bounds the gateway reports (`site.bounds_of`)
//! — rated ∩ live augmentations for an inverter, the SoC-throttled
//! window for a battery — i.e. exactly what a real EMS reads off the
//! telemetry stream, including any cap a bounds-driving app has
//! applied.
//!
//! Paired with `(set-active-power … CLAMP)` and scheduled by `(every …)`
//! / `(define-controller …)`, these turn the sim genuinely closed-loop.

use tulisp::{Error, TulispContext};

use crate::sim::microgrids::SharedSiteRouter;
use crate::timeout_tracker::SetpointAxis;

enum Edge {
    Lower,
    Upper,
}

/// The outermost finite edge of a component's effective active-power
/// bounds on the requested side, in watts.
fn bound_edge(router: &SharedSiteRouter, id: i64, edge: Edge) -> Result<f64, Error> {
    let w = router.site();
    if w.get(id as u64).is_none() {
        return Err(Error::invalid_argument(format!(
            "component-bound: component {id} not found"
        )));
    }
    let bounds = w
        .bounds_of(id as u64, SetpointAxis::Active)
        .ok_or_else(|| {
            Error::invalid_argument(format!(
                "component-bound: component {id} has no active bounds"
            ))
        })?;
    let value = match edge {
        Edge::Lower => bounds.0.iter().filter_map(|b| b.lower).reduce(f32::min),
        Edge::Upper => bounds.0.iter().filter_map(|b| b.upper).reduce(f32::max),
    };
    value.map(|v| v as f64).ok_or_else(|| {
        Error::invalid_argument(format!(
            "component-bound: component {id} has an open bound on that side"
        ))
    })
}

/// The outermost finite edge of a component's live reactive-power
/// envelope on the requested side, in VAr. Collapses a multi-band
/// envelope exactly as `bound_edge` collapses P — min over the
/// lowers, max over the uppers. A component with no Q axis at all
/// (`bounds_of(id, Reactive)` is `None`) errors — mirrors
/// `bound_edge`'s not-found-style shape, but names the missing axis
/// instead of missing bounds.
fn reactive_bound_edge(router: &SharedSiteRouter, id: i64, edge: Edge) -> Result<f64, Error> {
    let w = router.site();
    if w.get(id as u64).is_none() {
        return Err(Error::invalid_argument(format!(
            "component-reactive-bound: component {id} not found"
        )));
    }
    let bounds = w
        .bounds_of(id as u64, SetpointAxis::Reactive)
        .ok_or_else(|| {
            Error::invalid_argument(format!(
                "component-reactive-bound: component {id} has no reactive envelope"
            ))
        })?;
    let value = match edge {
        Edge::Lower => bounds.0.iter().filter_map(|b| b.lower).reduce(f32::min),
        Edge::Upper => bounds.0.iter().filter_map(|b| b.upper).reduce(f32::max),
    };
    value.map(|v| v as f64).ok_or_else(|| {
        Error::invalid_argument(format!(
            "component-reactive-bound: component {id} has an open bound on that side"
        ))
    })
}

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    let r = router.clone();
    ctx.defun(
        "component-active-power",
        move |id: i64| -> Result<f64, Error> {
            let w = r.site();
            let c = w.get(id as u64).ok_or_else(|| {
                Error::invalid_argument(format!("component-active-power: component {id} not found"))
            })?;
            Ok(c.aggregate_power_w(&w) as f64)
        },
    );

    let r = router.clone();
    ctx.defun(
        "component-bound-lower",
        move |id: i64| -> Result<f64, Error> { bound_edge(&r, id, Edge::Lower) },
    );

    let r = router.clone();
    ctx.defun(
        "component-bound-upper",
        move |id: i64| -> Result<f64, Error> { bound_edge(&r, id, Edge::Upper) },
    );

    let r = router.clone();
    ctx.defun(
        "component-reactive-power",
        move |id: i64| -> Result<f64, Error> {
            let w = r.site();
            let c = w.get(id as u64).ok_or_else(|| {
                Error::invalid_argument(format!(
                    "component-reactive-power: component {id} not found"
                ))
            })?;
            Ok(c.aggregate_reactive_var(&w) as f64)
        },
    );

    let r = router.clone();
    ctx.defun(
        "component-reactive-bound-lower",
        move |id: i64| -> Result<f64, Error> { reactive_bound_edge(&r, id, Edge::Lower) },
    );

    let r = router;
    ctx.defun(
        "component-reactive-bound-upper",
        move |id: i64| -> Result<f64, Error> { reactive_bound_edge(&r, id, Edge::Upper) },
    );
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;
    use crate::sim::bounds::VecBounds;
    use chrono::Utc;
    use std::time::Duration;

    fn rig() -> (crate::lisp::Config, std::path::PathBuf) {
        config_with(
            "(setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -4000.0 :rated-upper 4000.0
                                       :successors (list b1))",
        )
    }

    /// component-bound-lower/upper report the inverter's effective
    /// bounds, and follow a live augmentation (the cap an EMS reads).
    #[test]
    fn component_bounds_report_rated_then_track_augmentation() {
        let (cfg, _dir) = rig();
        let edge = |cfg: &crate::lisp::Config, expr: &str| -> f64 {
            cfg.eval(expr).unwrap().parse().unwrap()
        };
        // Rated ±4 kW before any cap.
        assert!((edge(&cfg, "(component-bound-upper 2)") - 4000.0).abs() < 1.0);
        assert!((edge(&cfg, "(component-bound-lower 2)") + 4000.0).abs() < 1.0);
        // A bounds-driving app narrows the inverter to [-4 kW, +1 kW].
        cfg.site()
            .gateway()
            .augment(
                2,
                cfg.site().run_generation(),
                crate::timeout_tracker::SetpointAxis::Active,
                VecBounds::single(-4000.0, 1000.0),
                Duration::from_secs(60),
            )
            .unwrap();
        assert!((edge(&cfg, "(component-bound-upper 2)") - 1000.0).abs() < 1.0);
        assert!((edge(&cfg, "(component-bound-lower 2)") + 4000.0).abs() < 1.0);
    }

    /// component-active-power reports the component's current power after
    /// a setpoint settles.
    #[test]
    fn component_active_power_reports_settled_power() {
        let (cfg, _dir) = rig();
        cfg.eval("(set-active-power 2 2000.0 30000)").unwrap();
        let site = cfg.site();
        site.tick_n(3, Duration::from_millis(100));
        let p: f64 = cfg
            .eval("(component-active-power 2)")
            .unwrap()
            .parse()
            .unwrap();
        assert!((p - 2000.0).abs() < 1.0, "expected +2 kW, got {p}");
    }

    /// `component-reactive-power` mirrors `component-active-power`
    /// over the Q axis; the bound queries mirror the active-bound
    /// queries over `bounds_of(id, Reactive)`. A component with no Q
    /// axis (a battery — `bounds_of(id, Reactive)` is `None`) errors
    /// on the bound query instead of returning a bogus edge.
    #[test]
    fn reactive_queries_mirror_active() {
        let (cfg, _dir) = config_with(
            "(setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :reactive-pf-limit 0
                                       :reactive-apparent-va 5000.0
                                       :reactive-command-delay-s 0
                                       :reactive-ramp-rate 1e9
                                       :successors (list b1))",
        );
        // Arm a Q setpoint and let it settle so component-reactive-power
        // reads back the live value, mirroring the active-power test above.
        cfg.eval("(set-reactive-power 2 1500.0 30000)").unwrap();
        let site = cfg.site();
        site.tick_n(3, Duration::from_millis(100));
        let q: f64 = cfg
            .eval("(component-reactive-power 2)")
            .unwrap()
            .parse()
            .unwrap();
        assert!((q - 1500.0).abs() < 1.0, "expected +1.5 kVAr, got {q}");

        // Bound queries return the caps band edges: ±5 kVAr at idle P.
        let upper: f64 = cfg
            .eval("(component-reactive-bound-upper 2)")
            .unwrap()
            .parse()
            .unwrap();
        let lower: f64 = cfg
            .eval("(component-reactive-bound-lower 2)")
            .unwrap()
            .parse()
            .unwrap();
        assert!((upper - 5000.0).abs() < 1.0, "upper {upper}");
        assert!((lower + 5000.0).abs() < 1.0, "lower {lower}");

        // The battery has no Q axis: bounds_of(id, Reactive) is None,
        // so the bound query errors rather than returning a bogus
        // edge.
        let err = cfg.eval("(component-reactive-bound-upper 1)").unwrap_err();
        assert!(
            err.contains("no reactive envelope"),
            "expected 'no reactive envelope' error, got {err}"
        );
    }

    /// A multi-band Q envelope (a live `AC_POWER_REACTIVE` augmentation
    /// splitting the caps band in two) collapses to ONE pair of edges
    /// exactly as the active twin does: min over every band's lower,
    /// max over every band's upper — not the first band's edges.
    #[test]
    fn reactive_bound_queries_collapse_every_band() {
        let (cfg, _dir) = config_with(
            "(setq b1 (%make-battery :id 1 :rated-lower -5000.0 :rated-upper 5000.0))
             (%make-battery-inverter :id 2 :rated-lower -5000.0 :rated-upper 5000.0
                                       :reactive-pf-limit 0
                                       :reactive-apparent-va 5000.0
                                       :reactive-command-delay-s 0
                                       :reactive-ramp-rate 1e9
                                       :successors (list b1))",
        );
        // ±5 kVAr caps ∩ {[-2 kVAr, -0.5 kVAr], [0.5 kVAr, 2 kVAr]}
        // leaves two disjoint bands around a dead zone at zero.
        cfg.site()
            .gateway()
            .augment(
                2,
                cfg.site().run_generation(),
                crate::timeout_tracker::SetpointAxis::Reactive,
                VecBounds(vec![
                    crate::proto::common::metrics::Bounds {
                        lower: Some(-2000.0),
                        upper: Some(-500.0),
                    },
                    crate::proto::common::metrics::Bounds {
                        lower: Some(500.0),
                        upper: Some(2000.0),
                    },
                ]),
                Duration::from_secs(60),
            )
            .unwrap();
        let bands = cfg
            .site()
            .bounds_of(2, crate::timeout_tracker::SetpointAxis::Reactive)
            .expect("the inverter publishes Q");
        assert_eq!(bands.0.len(), 2, "expected two bands, got {bands}");

        let edge = |expr: &str| -> f64 { cfg.eval(expr).unwrap().parse().unwrap() };
        let lower = edge("(component-reactive-bound-lower 2)");
        let upper = edge("(component-reactive-bound-upper 2)");
        assert!(
            (lower + 2000.0).abs() < 1.0,
            "lower = min of lowers: {lower}"
        );
        assert!(
            (upper - 2000.0).abs() < 1.0,
            "upper = max of uppers: {upper}"
        );
    }

    /// A scripted controller reads the live cap and actuates within it:
    /// command the upper bound (clamped), and as the cap moves, the
    /// commanded power follows — closed-loop, no errors.
    #[test]
    fn controller_tracks_the_live_cap() {
        let (cfg, _dir) = rig();
        let site = cfg.site();
        let inv = site.get(2).unwrap();
        // One controller step: charge at whatever the upper bound allows.
        let step = "(set-active-power 2 (component-bound-upper 2) 30000 t)";
        cfg.eval(step).unwrap();
        site.tick_n(3, Duration::from_millis(100));
        assert!((inv.aggregate_power_w(&site) - 4000.0).abs() < 1.0);
        // Cap drops to +1 kW; the next controller step tracks it.
        cfg.site()
            .gateway()
            .augment(
                2,
                cfg.site().run_generation(),
                crate::timeout_tracker::SetpointAxis::Active,
                VecBounds::single(-4000.0, 1000.0),
                Duration::from_secs(60),
            )
            .unwrap();
        cfg.eval(step).unwrap();
        site.tick_n(3, Duration::from_millis(100));
        let p = inv.aggregate_power_w(&site);
        assert!(
            (p - 1000.0).abs() < 1.0,
            "controller should track +1 kW cap, got {p}"
        );
    }

    /// The history ring, the Lisp bound queries and the telemetry
    /// overlay read one source: an augmented inverter reports the
    /// same upper edge through all three.
    #[test]
    fn history_and_lisp_bounds_agree_with_telemetry() {
        use crate::sim::history::Metric;
        use crate::timeout_tracker::SetpointAxis;
        let (cfg, _dir) = rig();
        let site = cfg.site();
        site.gateway()
            .augment(
                2,
                site.run_generation(),
                SetpointAxis::Active,
                VecBounds::single(-4000.0, 1500.0),
                Duration::from_secs(60),
            )
            .unwrap();
        let now = Utc::now();
        site.record_history_snapshot(now);
        let telemetry = site
            .telemetry_of(site.get(2).unwrap().as_ref())
            .active_power_bounds
            .unwrap();
        let history = site
            .history_window(
                2,
                Metric::ActivePowerUpperBoundW,
                now - chrono::Duration::seconds(1),
            )
            .unwrap();
        let lisp: f64 = cfg
            .eval("(component-bound-upper 2)")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(telemetry.0[0].upper, Some(1500.0));
        assert_eq!(history.last().unwrap().value, 1500.0);
        assert!((lisp - 1500.0).abs() < 1e-3);
    }
}
