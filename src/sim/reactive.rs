//! Inverter reactive-power capability envelope: the shape a Q axis is
//! allowed to sit in (PF-limit, kVA-limit, or both). Pure data; cheap
//! to copy. The gateway drives Q through its delay and ramp
//! (`crate::sim::gateway_axis::GatewayAxis`), and each inverter's
//! `DeviceAxis` holds the output inside this envelope at the live P.
//!
//! Real inverters limit Q via two composable constraints:
//!
//! 1. **PF-limit**: `|Q| ≤ k × |P|`. A power-factor floor — when `P → 0`,
//!    Q allowance also collapses to zero. Common interconnection
//!    requirement (e.g. IEEE 1547-2018 default PF ≥ 0.85 ↔ k ≈ 0.62;
//!    microsim's hardcoded 0.35 ↔ PF ≥ 0.94).
//! 2. **Apparent / kVA-limit**: `P² + Q² ≤ S_rated²`. Hardware envelope;
//!    full Q is allowed at P=0, shrinking to √(S² − P²) as P grows.
//!
//! Both are optional; either, both, or neither may be set per
//! inverter. The effective Q-bound at a given P is their
//! intersection.

use crate::sim::bounds::VecBounds;

#[derive(Clone, Copy, Debug, Default)]
pub struct ReactiveCapability {
    /// `|Q| ≤ pf_limit × |P|`. None disables the PF cap.
    pub pf_limit: Option<f32>,
    /// `√(P² + Q²) ≤ apparent_va`. None disables the kVA cap.
    pub apparent_va: Option<f32>,
}

impl ReactiveCapability {
    /// Microsim-compatible default: ±35% of |P|, no kVA cap.
    pub fn microsim_default() -> Self {
        Self {
            pf_limit: Some(0.35),
            apparent_va: None,
        }
    }

    /// Live `(lower, upper)` Q bounds at the given active P.
    /// `(0.0, 0.0)` when P exceeds the apparent envelope (no headroom
    /// for any Q).
    pub fn q_bounds_at(&self, p: f32) -> (f32, f32) {
        let mut lo = f32::NEG_INFINITY;
        let mut hi = f32::INFINITY;

        if let Some(k) = self.pf_limit {
            let q_max = k * p.abs();
            lo = lo.max(-q_max);
            hi = hi.min(q_max);
        }

        if let Some(s) = self.apparent_va {
            if p.abs() >= s {
                return (0.0, 0.0);
            }
            let q_max = (s * s - p * p).max(0.0).sqrt();
            lo = lo.max(-q_max);
            hi = hi.min(q_max);
        }

        // No constraint configured at all → "anything goes" is too
        // dangerous; clamp to a finite default so the proto bounds
        // field doesn't carry ±∞. NOTE the consequence: clearing both
        // caps does NOT mean "unconstrained" — it means |Q| ≤ |P|
        // (PF ≥ 0.707), which is ZERO reactive headroom at P = 0. A
        // config that wants full-circle Q at idle must set
        // :reactive-apparent-va explicitly.
        if !lo.is_finite() || !hi.is_finite() {
            lo = -p.abs();
            hi = p.abs();
        }

        if lo > hi { (0.0, 0.0) } else { (lo, hi) }
    }

    /// [`Self::q_bounds_at`] as a one-interval band.
    pub fn q_band_at(&self, p: f32) -> VecBounds {
        let (lo, hi) = self.q_bounds_at(p);
        VecBounds::single(lo, hi)
    }

    pub fn contains(&self, p: f32, q: f32) -> bool {
        let (lo, hi) = self.q_bounds_at(p);
        q >= lo && q <= hi
    }

    /// The widest Q reachable at ANY P in `[0, p_rated_abs]` — the
    /// capability hull, not the bound at one particular P. Used by a
    /// Q axis's static shape, since a Q axis has no rated band of its
    /// own: the P axis's rated magnitude bounds what Q can ever reach.
    ///
    /// With neither cap set, `q_bounds_at` falls back to `|Q| ≤ |P|`,
    /// which is widest at `P = p_rated_abs`. With only a PF cap,
    /// `k·P` is widest at `P = p_rated_abs` too. With only a kVA cap,
    /// the circle is widest at `P = 0`. With both, the PF line and
    /// the kVA circle cross at `P* = s / √(1+k²)` — below `P*` the PF
    /// line is the tighter (binding) constraint and grows with P, so
    /// the hull is widest right at the crossing; at or past `P*` the
    /// kVA circle is tighter and SHRINKS with P, so the hull is
    /// widest at the smaller of `p_rated_abs` and `P*`.
    pub fn hull(&self, p_rated_abs: f32) -> (f32, f32) {
        let q = match (self.pf_limit, self.apparent_va) {
            (None, None) => p_rated_abs,
            (Some(k), None) => k * p_rated_abs,
            (None, Some(s)) => s,
            (Some(k), Some(s)) => {
                let p_star = s / (1.0 + k * k).sqrt();
                if p_star <= p_rated_abs {
                    k * s / (1.0 + k * k).sqrt()
                } else {
                    let kva_q = (s * s - p_rated_abs * p_rated_abs).max(0.0).sqrt();
                    (k * p_rated_abs).min(kva_q)
                }
            }
        };
        (-q, q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pf_limit_scales_with_p() {
        let cap = ReactiveCapability {
            pf_limit: Some(0.35),
            apparent_va: None,
        };
        assert_eq!(cap.q_bounds_at(0.0), (0.0, 0.0));
        let (lo, hi) = cap.q_bounds_at(10_000.0);
        assert!((lo + 3500.0).abs() < 1e-3);
        assert!((hi - 3500.0).abs() < 1e-3);
        // sign of P doesn't matter (discharge is symmetric)
        assert_eq!(cap.q_bounds_at(-10_000.0), (lo, hi));
    }

    #[test]
    fn kva_limit_is_circle() {
        let cap = ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(10_000.0),
        };
        // P=0 → full Q allowance (±10 kVA)
        let (lo, hi) = cap.q_bounds_at(0.0);
        assert!((lo + 10_000.0).abs() < 1e-3);
        assert!((hi - 10_000.0).abs() < 1e-3);
        // P=6 kW, S=10 kVA → Q_max = sqrt(100-36)*1000 = 8 kVA
        let (lo, hi) = cap.q_bounds_at(6_000.0);
        assert!((lo + 8_000.0).abs() < 1e-3);
        assert!((hi - 8_000.0).abs() < 1e-3);
        // P at the rim → no Q
        assert_eq!(cap.q_bounds_at(10_000.0), (0.0, 0.0));
    }

    #[test]
    fn hull_shapes() {
        let pf = ReactiveCapability {
            pf_limit: Some(0.5),
            apparent_va: None,
        };
        assert_eq!(pf.hull(10_000.0), (-5_000.0, 5_000.0));
        let kva = ReactiveCapability {
            pf_limit: None,
            apparent_va: Some(4_000.0),
        };
        assert_eq!(kva.hull(10_000.0), (-4_000.0, 4_000.0));
        let neither = ReactiveCapability {
            pf_limit: None,
            apparent_va: None,
        };
        assert_eq!(neither.hull(10_000.0), (-10_000.0, 10_000.0));
        // Both: k=1, s=5000 → cross at P*=s/√2≈3536 ≤ rated → hull ±k·s/√2.
        let both = ReactiveCapability {
            pf_limit: Some(1.0),
            apparent_va: Some(5_000.0),
        };
        let (lo, hi) = both.hull(10_000.0);
        assert!((hi - 5_000.0 / 2f32.sqrt()).abs() < 1.0 && (lo + hi).abs() < 1e-3);
        // Both, rated below the crossing: k=1, s=5000, rated 2000 → ±min(2000, √(s²−4M))=±2000.
        let (lo2, hi2) = both.hull(2_000.0);
        assert!((hi2 - 2_000.0).abs() < 1.0 && (lo2 + hi2).abs() < 1e-3);
    }

    #[test]
    fn both_intersect() {
        let cap = ReactiveCapability {
            pf_limit: Some(0.5),
            apparent_va: Some(10_000.0),
        };
        // At P=4 kW: PF gives ±2 kVA, kVA gives ±sqrt(100-16)*1000 ≈ ±9.17 kVA → PF wins
        let (lo, hi) = cap.q_bounds_at(4_000.0);
        assert!((lo + 2_000.0).abs() < 1e-3);
        assert!((hi - 2_000.0).abs() < 1e-3);
        // At P=9 kW: PF gives ±4.5 kVA, kVA gives ±sqrt(100-81)*1000 ≈ ±4.36 kVA → kVA wins
        let (lo, _hi) = cap.q_bounds_at(9_000.0);
        assert!(lo > -4_500.0 && lo < -4_300.0, "got lo={lo}");
    }
}
