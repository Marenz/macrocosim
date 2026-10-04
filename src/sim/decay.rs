//! Smooth bound-decay function shared by SoC-protective ramps and the
//! `soc_protected_bounds` helper that uses it. Ported from microsim's
//! `bounded-exp-decay` in `sim/common.lisp`. Both Battery and EvCharger
//! pull from this module so the curve shape stays consistent.

/// Tunable thresholds + safety margins for `soc_protected_bounds`.
#[derive(Clone, Copy, Debug)]
pub struct SocProtect {
    pub soc_lower_pct: f32,
    pub soc_upper_pct: f32,
    /// Width (in % points) of the band where rated bounds taper
    /// toward zero. `0.0` disables the taper; the limits still hold.
    pub margin_pct: f32,
}

impl SocProtect {
    /// Build from the (lower, upper, margin) config triple every
    /// SoC-carrying component stores.
    pub fn new(soc_lower_pct: f32, soc_upper_pct: f32, margin_pct: f32) -> Self {
        Self {
            soc_lower_pct,
            soc_upper_pct,
            margin_pct,
        }
    }

    /// An over-wide protect margin makes BOTH taper bands active
    /// across the whole mid-SoC range, silently derating in both
    /// directions. Warn rather than clamp — the config author may
    /// genuinely want an always-tapering cell, but should know.
    pub fn warn_if_overwide(&self, label: &str) {
        if self.margin_pct * 2.0 > self.soc_upper_pct - self.soc_lower_pct {
            log::warn!(
                "{label}: soc-protect-margin {} covers more than half the \
                 [{}, {}] SoC band; both tapers are active at every SoC",
                self.margin_pct,
                self.soc_lower_pct,
                self.soc_upper_pct,
            );
        }
    }
}

/// Gate a scripted SoC write: reject non-finite values at the door
/// (NaN survives clamp — clamp on NaN self returns NaN — and would
/// poison every later SoC integration), else clamp into [0, 100].
pub fn sanitize_soc_pct(label: &str, pct: f32) -> Option<f32> {
    if !pct.is_finite() {
        log::warn!("{label} ignored non-finite value");
        return None;
    }
    Some(pct.clamp(0.0, 100.0))
}

/// One rectangular P·dt SoC step:
///     ΔE[Wh]  = P[W] · dt[s] / 3600
///     ΔSoC[%] = ΔE[Wh] / capacity[Wh] · 100
/// clamped at the boundaries so unphysical "extra" charge can't
/// accumulate when the protective taper is disabled. Identity when
/// capacity is unset (≤ 0).
pub fn integrate_soc_pct(
    soc_pct: f32,
    power_w: f32,
    dt: std::time::Duration,
    capacity_wh: f32,
) -> f32 {
    if capacity_wh <= 0.0 {
        return soc_pct;
    }
    let delta_soc = power_w * dt.as_secs_f32() / 3600.0 / capacity_wh * 100.0;
    (soc_pct + delta_soc).clamp(0.0, 100.0)
}

/// Apply the smooth taper near both SoC limits to a `(rated_lower,
/// rated_upper)` pair, returning the SoC-protected bounds at the
/// current `soc`. Charge (positive) tapers near `soc_upper`;
/// discharge (negative) tapers near `soc_lower`. With margin = 0
/// there is no taper: the rated pair holds inside the band, and each
/// side closes to 0 once SoC reaches its limit.
pub fn soc_protected_bounds(
    rated_lower: f32,
    rated_upper: f32,
    soc: f32,
    p: SocProtect,
) -> (f32, f32) {
    if p.margin_pct <= 0.0 {
        let upper = if soc >= p.soc_upper_pct {
            0.0
        } else {
            rated_upper
        };
        let lower = if soc <= p.soc_lower_pct {
            0.0
        } else {
            rated_lower
        };
        return (lower, upper);
    }

    let upper = if p.soc_upper_pct - soc < p.margin_pct {
        rated_upper
            * bounded_exp_decay(
                p.soc_upper_pct - p.margin_pct,
                p.soc_upper_pct,
                soc,
                1.2,
                0.3,
            )
    } else {
        rated_upper
    };

    let lower = if soc - p.soc_lower_pct < p.margin_pct {
        rated_lower
            * bounded_exp_decay(
                p.soc_lower_pct + p.margin_pct,
                p.soc_lower_pct,
                soc,
                1.2,
                0.3,
            )
    } else {
        rated_lower
    };

    (lower, upper)
}

/// Returns a multiplier in `[0, 1]` that smoothly tapers from 1 at
/// `start` to ~`min_val` at `stop`, then snaps to 0 at and beyond
/// `stop`. `base` controls how aggressive the curve is (higher = more
/// linear, lower = more bowed); microsim's defaults are `base = 1.2`,
/// `min_val = 0.3`.
pub fn bounded_exp_decay(start: f32, stop: f32, val: f32, base: f32, min_val: f32) -> f32 {
    if start == stop {
        return if val < start { 1.0 } else { 0.0 };
    }
    let base = base.max(1.1);
    let factor = 10.0 / (stop - start);
    let stop_s = start + (stop - start) * factor;
    let val_s = start + (val - start) * factor;
    let shift = min_val - base.powf(start - stop_s - 1.0);

    if val_s >= stop_s {
        0.0
    } else if val_s < start {
        1.0
    } else {
        shift + (1.0 - shift) * base.powf(start - val_s)
    }
}

#[cfg(test)]
mod tests {
    use super::{SocProtect, bounded_exp_decay, soc_protected_bounds};

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn at_start_returns_one() {
        let v = bounded_exp_decay(80.0, 90.0, 80.0, 1.2, 0.3);
        assert!(approx(v, 1.0, 1e-3), "expected 1.0, got {v}");
    }

    #[test]
    fn at_or_past_stop_returns_zero() {
        assert_eq!(bounded_exp_decay(80.0, 90.0, 90.0, 1.2, 0.3), 0.0);
        assert_eq!(bounded_exp_decay(80.0, 90.0, 95.0, 1.2, 0.3), 0.0);
    }

    #[test]
    fn before_start_returns_one() {
        assert_eq!(bounded_exp_decay(80.0, 90.0, 50.0, 1.2, 0.3), 1.0);
    }

    #[test]
    fn soc_protect_tapers_upper_only_near_high_soc() {
        let p = SocProtect {
            soc_lower_pct: 10.0,
            soc_upper_pct: 90.0,
            margin_pct: 10.0,
        };
        // Mid-band: full rated.
        assert_eq!(
            soc_protected_bounds(-30000.0, 30000.0, 50.0, p),
            (-30000.0, 30000.0)
        );
        // Near upper: upper derates, lower untouched.
        let (lo, hi) = soc_protected_bounds(-30000.0, 30000.0, 85.0, p);
        assert_eq!(lo, -30000.0);
        assert!(hi < 30000.0 && hi > 0.0, "expected derated upper, got {hi}");
        // Near lower: lower derates (less negative), upper untouched.
        let (lo, hi) = soc_protected_bounds(-30000.0, 30000.0, 15.0, p);
        assert_eq!(hi, 30000.0);
        assert!(
            lo > -30000.0 && lo < 0.0,
            "expected derated lower, got {lo}"
        );
    }

    /// Margin 0 drops the taper, not the limits: the rated pair holds
    /// strictly inside the band, and the side that would push SoC
    /// past a limit closes once SoC reaches it.
    #[test]
    fn zero_margin_still_closes_at_the_limits() {
        let p = SocProtect::new(10.0, 90.0, 0.0);
        assert_eq!(
            soc_protected_bounds(-30000.0, 30000.0, 89.9, p),
            (-30000.0, 30000.0)
        );
        assert_eq!(
            soc_protected_bounds(-30000.0, 30000.0, 90.0, p),
            (-30000.0, 0.0)
        );
        assert_eq!(
            soc_protected_bounds(-30000.0, 30000.0, 10.0, p),
            (0.0, 30000.0)
        );
        assert_eq!(
            soc_protected_bounds(-30000.0, 30000.0, 5.0, p),
            (0.0, 30000.0)
        );
    }

    #[test]
    fn middle_is_monotone() {
        let a = bounded_exp_decay(80.0, 90.0, 82.0, 1.2, 0.3);
        let b = bounded_exp_decay(80.0, 90.0, 85.0, 1.2, 0.3);
        let c = bounded_exp_decay(80.0, 90.0, 88.0, 1.2, 0.3);
        assert!(
            a > b && b > c,
            "expected monotone decreasing, got {a} {b} {c}"
        );
        assert!((0.0..=1.0).contains(&a));
        assert!((0.0..=1.0).contains(&c));
    }
}
