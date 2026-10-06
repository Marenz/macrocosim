//! Reactive-power knobs: `(set-reactive-pf-limit)` and
//! `(set-reactive-apparent-va)`. Mirror what a SunSpec /
//! IEEE 1547-2018 EMS pushes via Modbus.

use tulisp::{Error, TulispContext};

use crate::sim::microgrids::SharedSiteRouter;

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Same opt-in convention as the make-* plist args:
    //   value > 0  → that constraint is active with this magnitude
    //   value ≤ 0  → that constraint is disabled
    // Mirrors what a SunSpec / IEEE 1547-2018 EMS pushes via Modbus.
    let r = router.clone();
    ctx.defun(
        "set-reactive-pf-limit",
        move |id: i64, k: f64| -> Result<bool, Error> {
            let w = r.site();
            match w.get(id as u64) {
                Some(c) => {
                    let clamped = if k > 0.0 { Some(k as f32) } else { None };
                    if let Some(r) = c.reactive_limits() {
                        r.set_reactive_pf_limit(clamped);
                    }
                    w.note_knob_changed(id as u64, "reactive-pf-limit", clamped, None, None);
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-reactive-pf-limit: component {id} not found"
                ))),
            }
        },
    );

    let r = router;
    ctx.defun(
        "set-reactive-apparent-va",
        move |id: i64, va: f64| -> Result<bool, Error> {
            let w = r.site();
            match w.get(id as u64) {
                Some(c) => {
                    let clamped = if va > 0.0 { Some(va as f32) } else { None };
                    if let Some(r) = c.reactive_limits() {
                        r.set_reactive_apparent_va(clamped);
                    }
                    w.note_knob_changed(id as u64, "reactive-apparent-va", clamped, None, None);
                    Ok(true)
                }
                None => Err(Error::invalid_argument(format!(
                    "set-reactive-apparent-va: component {id} not found"
                ))),
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::{assert_lenient_noop, config_with};
    use crate::sim::events::SiteEvent;

    /// `set-reactive-pf-limit` on a component with no reactive
    /// limits returns t, changes nothing, and still broadcasts.
    #[test]
    fn set_reactive_pf_limit_without_limits_is_a_lenient_noop() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power-w 1500.0)");
        assert_lenient_noop(
            &cfg,
            7,
            "(set-reactive-pf-limit 7 0.9)",
            Some("reactive-pf-limit"),
        );
    }

    /// `set-reactive-apparent-va` on a component with no reactive
    /// limits returns t, changes nothing, and still broadcasts.
    #[test]
    fn set_reactive_apparent_va_without_limits_is_a_lenient_noop() {
        let (cfg, _dir) = config_with("(%make-meter :id 7 :power-w 1500.0)");
        assert_lenient_noop(
            &cfg,
            7,
            "(set-reactive-apparent-va 7 3000.0)",
            Some("reactive-apparent-va"),
        );
    }

    /// `(set-reactive-pf-limit id K)` broadcasts a `KnobChanged` with
    /// the clamped-active value; `k <= 0` clears the limit and the
    /// broadcast carries `value: None`.
    #[test]
    fn set_reactive_pf_limit_broadcasts_knob_changed() {
        let (cfg, _dir) = config_with("(%make-solar-inverter :id 4)");
        let mut rx = cfg.site().subscribe_events();

        cfg.eval("(set-reactive-pf-limit 4 0.95)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged { id: 4, knob: "reactive-pf-limit", value: Some(v), expr: None, .. }
                    if (*v - 0.95).abs() < 1e-6
            )),
            "no matching KnobChanged (set) on the bus; saw: {seen:?}"
        );

        cfg.eval("(set-reactive-pf-limit 4 0)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 4,
                    knob: "reactive-pf-limit",
                    value: None,
                    expr: None,
                    ..
                }
            )),
            "no matching KnobChanged (clear) on the bus; saw: {seen:?}"
        );
    }

    /// `(set-reactive-apparent-va id VA)` mirrors the pf-limit case
    /// above — clearing broadcasts `value: None`.
    #[test]
    fn set_reactive_apparent_va_broadcasts_knob_changed() {
        let (cfg, _dir) = config_with("(%make-solar-inverter :id 4)");
        let mut rx = cfg.site().subscribe_events();

        cfg.eval("(set-reactive-apparent-va 4 3000.0)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged { id: 4, knob: "reactive-apparent-va", value: Some(v), expr: None, .. }
                    if (*v - 3000.0).abs() < 1e-6
            )),
            "no matching KnobChanged (set) on the bus; saw: {seen:?}"
        );

        cfg.eval("(set-reactive-apparent-va 4 0)").unwrap();
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                SiteEvent::KnobChanged {
                    id: 4,
                    knob: "reactive-apparent-va",
                    value: None,
                    expr: None,
                    ..
                }
            )),
            "no matching KnobChanged (clear) on the bus; saw: {seen:?}"
        );
    }
}
