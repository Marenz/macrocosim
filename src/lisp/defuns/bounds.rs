//! `(augment-active-bounds)` and `(augment-reactive-bounds)` — narrow a
//! component's power envelope with a time-limited bounds augmentation,
//! like gRPC's `AugmentElectricalComponentBounds`.

use std::sync::Arc;

use parking_lot::RwLock;
use tulisp::{Error, Rest, TulispContext, TulispObject};

use crate::proto::common::metrics::Bounds;
use crate::sim::bounds::VecBounds;
use crate::sim::microgrids::SharedSiteRouter;
use crate::timeout_tracker::SetpointAxis;

use super::super::Metadata;
use super::setpoints::parse_tail;

/// One band edge: a number, or nil for "unbounded on this side".
fn edge(name: &str, band: &TulispObject, o: TulispObject) -> Result<Option<f32>, Error> {
    if o.null() {
        return Ok(None);
    }
    f64::try_from(&o).map(|v| Some(v as f32)).map_err(|_| {
        Error::invalid_argument(format!(
            "{name}: expected a number or nil as a band edge — got {o} in {band}"
        ))
    })
}

/// The elements of a list; `None` if it ends in a non-nil atom (or is
/// one).
fn elements(list: &TulispObject) -> Option<Vec<TulispObject>> {
    let mut it = list.base_iter();
    let items = it.by_ref().collect();
    it.take_error().ok().map(|()| items)
}

/// One `(LOWER UPPER)` band.
fn band(name: &str, b: &TulispObject) -> Result<Bounds, Error> {
    let Some(Ok([lower, upper])) = elements(b).map(<[TulispObject; 2]>::try_from) else {
        return Err(Error::invalid_argument(format!(
            "{name}: expected a two-element (LOWER UPPER) band — got {b}"
        )));
    };
    Ok(Bounds {
        lower: edge(name, b, lower)?,
        upper: edge(name, b, upper)?,
    })
}

/// BOUNDS is either one `(LOWER UPPER)` band or a list of them,
/// e.g. `'((-10000 -1000) (1000 10000))`. A list whose first element is
/// itself a list is read as a list of bands.
fn parse_bounds(name: &str, arg: &TulispObject) -> Result<VecBounds, Error> {
    if !arg.consp() {
        return Err(Error::invalid_argument(format!(
            "{name}: expected a (LOWER UPPER) band or a list of bands — got {arg}"
        )));
    }
    let bands = if arg.car()?.consp() {
        elements(arg)
            .ok_or_else(|| {
                Error::invalid_argument(format!(
                    "{name}: expected a list of (LOWER UPPER) bands — got {arg}"
                ))
            })?
            .iter()
            .map(|b| band(name, b))
            .collect::<Result<_, _>>()?
    } else {
        vec![band(name, arg)?]
    };
    Ok(VecBounds::new(bands))
}

fn augment(
    router: &SharedSiteRouter,
    metadata: &RwLock<Metadata>,
    name: &str,
    axis: SetpointAxis,
    id: i64,
    bounds: &TulispObject,
    rest: Vec<TulispObject>,
) -> Result<bool, Error> {
    let w = router.site();
    let generation = w.run_generation();
    if w.get(id as u64).is_none() {
        return Err(Error::invalid_argument(format!(
            "{name}: component {id} not found"
        )));
    }
    let proposed = parse_bounds(name, bounds)?;
    let lifetime = parse_tail(name, rest, false)?
        .lifetime
        .unwrap_or_else(|| metadata.read().default_augment_lifetime);
    w.gateway()
        .augment(id as u64, generation, axis, proposed, lifetime)
        .map_err(|e| Error::invalid_argument(format!("{name}: component {id}: {e}")))?;
    Ok(true)
}

/// `(augment-active-bounds ID BOUNDS &key :lifetime-s)` — narrow
/// component ID's active-power envelope for `:lifetime-s` seconds, like
/// gRPC's `AugmentElectricalComponentBounds` for `AC_POWER_ACTIVE`, with
/// the same shape and envelope checks. Returns `t`; signals an error if
/// the component doesn't exist, stores no augmentation on this axis
/// (batteries, meters, the grid), BOUNDS is malformed, or the result
/// would leave no valid setpoint.
///
/// BOUNDS is one `(LOWER UPPER)` band, or a list of bands — two bands
/// such as `'((-10000 -1000) (1000 10000))` make an exclusion zone
/// around zero. A nil edge is unbounded on that side. The augmentation
/// composes with the rated bounds and any other live augmentation.
///
/// `:lifetime-s` omitted falls back to `default-augment-lifetime-s`,
/// like a gRPC request without `request_lifetime`. Unlike the gRPC
/// route there is no [5 s, 15 min] window, no fault gating and no
/// setpoint journal. The augmentation is stamped on the site clock,
/// so in a headless (sim-clock) run it lapses after that many seconds
/// of sim time, everywhere at once. The positional `LIFETIME-MS` form
/// still works, read in milliseconds, and warns.
///
/// `(augment-reactive-bounds ID BOUNDS &key :lifetime-s)` — the same
/// over the reactive axis (`AC_POWER_REACTIVE`), in VAr.
pub(super) fn register(
    ctx: &mut TulispContext,
    router: SharedSiteRouter,
    metadata: Arc<RwLock<Metadata>>,
) {
    for (name, axis) in [
        ("augment-active-bounds", SetpointAxis::Active),
        ("augment-reactive-bounds", SetpointAxis::Reactive),
    ] {
        let (r, m) = (router.clone(), metadata.clone());
        ctx.defun(
            name,
            move |id: i64, bounds: TulispObject, rest: Rest<TulispObject>| {
                let rest = rest.into_iter().collect::<Vec<TulispObject>>();
                augment(&r, &m, name, axis, id, &bounds, rest)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;

    fn rig() -> (crate::lisp::Config, std::path::PathBuf) {
        config_with(
            "(setq b1 (%make-battery :id 1 :rated-lower-w -10000.0 :rated-upper-w 10000.0))
             (%make-battery-inverter :id 2 :rated-lower-w -10000.0 :rated-upper-w 10000.0
                                     :reactive-pf-limit 0
                                     :reactive-apparent-va 5000.0
                                     :successors (list b1))",
        )
    }

    fn eval_err(cfg: &crate::lisp::Config, expr: &str) -> String {
        format!("{:?}", cfg.eval(expr).expect_err(expr))
    }

    /// Inverter 2's effective active bounds, as text.
    fn active_bounds(cfg: &crate::lisp::Config) -> String {
        cfg.site()
            .bounds_of(2, crate::timeout_tracker::SetpointAxis::Active)
            .unwrap()
            .to_string()
    }

    /// Two bands carve an exclusion zone around zero: the reported
    /// edges stay at the outer limits, a setpoint inside the gap is
    /// rejected by `set-active-power`, one outside it is accepted.
    #[test]
    fn two_bands_make_an_exclusion_zone() {
        let (cfg, _dir) = rig();
        cfg.eval("(augment-active-bounds 2 '((-4000 -1000) (1000 4000)) 60000)")
            .unwrap();
        assert_eq!(active_bounds(&cfg), "[-4000, -1000], [1000, 4000]");
        assert!(cfg.eval("(set-active-power 2 500.0)").is_err());
        cfg.eval("(set-active-power 2 2000.0)").unwrap();
    }

    /// A single band narrows the rated envelope; a nil edge leaves that
    /// side at the rated bound.
    #[test]
    fn single_band_with_an_open_edge() {
        let (cfg, _dir) = rig();
        cfg.eval("(augment-active-bounds 2 '(nil 3000.0) 60000)")
            .unwrap();
        assert_eq!(active_bounds(&cfg), "[-10000, 3000]");
    }

    /// Without LIFETIME-MS the default augment lifetime applies: a 0
    /// default lapses the band at once, a long one keeps it.
    #[test]
    fn default_lifetime_comes_from_the_metadata() {
        let (cfg, _dir) = rig();
        cfg.eval("(set-default-augment-lifetime-ms 0)").unwrap();
        cfg.eval("(augment-active-bounds 2 '(-1000 1000))").unwrap();
        assert_eq!(active_bounds(&cfg), "[-10000, 10000]");
        cfg.eval("(set-default-augment-lifetime-ms 60000)").unwrap();
        cfg.eval("(augment-active-bounds 2 '(-1000 1000))").unwrap();
        assert_eq!(active_bounds(&cfg), "[-1000, 1000]");
    }

    /// The reactive twin narrows the Q envelope and leaves P alone.
    #[test]
    fn reactive_twin_narrows_only_q() {
        let (cfg, _dir) = rig();
        cfg.eval("(augment-reactive-bounds 2 '(-1000 1000) 60000)")
            .unwrap();
        let site = cfg.site();
        let q = site
            .bounds_of(2, crate::timeout_tracker::SetpointAxis::Reactive)
            .unwrap();
        assert_eq!(q.to_string(), "[-1000, 1000]");
        assert_eq!(active_bounds(&cfg), "[-10000, 10000]");
    }

    /// The augmentation lapses after its lifetime, like a gRPC one.
    #[test]
    fn augmentation_expires() {
        let (cfg, _dir) = rig();
        cfg.eval("(augment-active-bounds 2 '(-1000 1000) 0)")
            .unwrap();
        assert_eq!(active_bounds(&cfg), "[-10000, 10000]");
    }

    /// The gRPC route's rejections.
    #[test]
    fn rejections_match_the_grpc_route() {
        let (cfg, _dir) = rig();
        let err = eval_err(&cfg, "(augment-active-bounds 2 '(1000 -1000))");
        assert!(err.contains("is inverted (lower > upper)"), "{err}");
        let err = eval_err(&cfg, "(augment-active-bounds 2 '(20000 30000))");
        assert!(
            err.contains("disjoint from the component's current envelope"),
            "{err}"
        );
        let err = eval_err(&cfg, "(augment-active-bounds 1 '(-1000 1000))");
        assert!(err.contains("stores no augmentation on this axis"), "{err}");
        let err = eval_err(&cfg, "(augment-active-bounds 99 '(-1000 1000))");
        assert!(err.contains("component 99 not found"), "{err}");
        let err = eval_err(&cfg, "(augment-active-bounds 2 '(1000))");
        assert!(err.contains("two-element (LOWER UPPER) band"), "{err}");
        let err = eval_err(&cfg, "(augment-active-bounds 2 '(1000 4000 . 9))");
        assert!(err.contains("two-element (LOWER UPPER) band"), "{err}");
        let err = eval_err(&cfg, "(augment-active-bounds 2 '((1 2) (3 4) . 5))");
        assert!(err.contains("a list of (LOWER UPPER) bands"), "{err}");
        let err = eval_err(&cfg, "(augment-active-bounds 2 '(a 1000))");
        assert!(err.contains("a number or nil as a band edge"), "{err}");
        let err = eval_err(&cfg, "(augment-active-bounds 2 5)");
        assert!(err.contains("band or a list of bands"), "{err}");
    }

    #[test]
    fn augment_lifetime_keyword_is_seconds() {
        let (cfg, _dir) = rig();
        cfg.eval("(augment-active-bounds 2 '(-1000 1000) :lifetime-s 0)")
            .unwrap();
        assert_eq!(active_bounds(&cfg), "[-10000, 10000]");
        cfg.eval("(augment-active-bounds 2 '(-1000 1000) :lifetime-s 60)")
            .unwrap();
        assert_eq!(active_bounds(&cfg), "[-1000, 1000]");
    }

    #[test]
    fn a_positional_augment_lifetime_is_still_milliseconds() {
        let (cfg, _dir) = rig();
        cfg.eval("(augment-active-bounds 2 '(-1000 1000) 60000)")
            .unwrap();
        assert_eq!(active_bounds(&cfg), "[-1000, 1000]");
    }

    #[test]
    fn augment_refuses_clamp() {
        let (cfg, _dir) = rig();
        let err = eval_err(&cfg, "(augment-active-bounds 2 '(-1000 1000) :clamp t)");
        assert!(err.contains(":clamp"), "{err}");
    }

    #[test]
    fn a_positional_augment_lifetime_takes_one_item_and_no_keyword() {
        let (cfg, _dir) = rig();
        let err = eval_err(&cfg, "(augment-active-bounds 2 '(-1000 1000) 60000 t)");
        assert!(err.contains("augment-active-bounds"), "{err}");
        assert!(err.contains("at most 1"), "{err}");
        let err = eval_err(
            &cfg,
            "(augment-active-bounds 2 '(-1000 1000) 60000 :lifetime-s 5)",
        );
        assert!(err.contains("augment-active-bounds"), "{err}");
        assert!(err.contains(":lifetime-s"), "{err}");
        assert_eq!(active_bounds(&cfg), "[-10000, 10000]");
    }
}
