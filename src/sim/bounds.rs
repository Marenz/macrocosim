//! Power-bound containers, ported from microsim's lisp/bounds module.
//!
//! Two layers:
//! - [`VecBounds`] is a sorted, normalized list of disjoint
//!   [`Bounds`] (proto type, reused so the values flow straight into a
//!   `MetricSample` without a copy).
//! - [`ComponentBounds`] holds a queue of time-limited augmentations
//!   submitted via the gRPC AugmentBounds RPC. `effective_at()`
//!   intersects the live ones.

use std::{collections::VecDeque, fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::proto::common::metrics::Bounds;
use crate::timeout_tracker::deadline_after;

#[derive(Debug, Clone, Default)]
pub struct VecBounds(pub Vec<Bounds>);

impl fmt::Display for VecBounds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return write!(f, "[]");
        }
        let mut first = true;
        for b in &self.0 {
            if !first {
                write!(f, ", ")?;
            }
            first = false;
            write!(f, "{}", BoundsDisplay(b))?;
        }
        Ok(())
    }
}

struct BoundsDisplay<'a>(&'a Bounds);
impl fmt::Display for BoundsDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn side(b: Option<f32>) -> String {
            b.map(|v| format!("{v}")).unwrap_or_else(|| "*".into())
        }
        write!(f, "[{}, {}]", side(self.0.lower), side(self.0.upper))
    }
}

impl VecBounds {
    pub fn single(lower: f32, upper: f32) -> Self {
        Self(vec![Bounds {
            lower: Some(lower),
            upper: Some(upper),
        }])
    }

    pub fn new(mut bounds: Vec<Bounds>) -> Self {
        bounds.sort_by(|a, b| {
            a.lower
                .unwrap_or(f32::MIN)
                .partial_cmp(&b.lower.unwrap_or(f32::MIN))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        VecBounds(bounds)
    }

    /// The shape checks a bounds augmentation must pass: at least one
    /// band, no non-finite edge, no inverted band. The check against
    /// the live envelope is the component's
    /// (`GatewayAxis::try_augment`).
    pub fn check_augmentation_shape(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Err("augmentation contains no bounds".to_string());
        }
        // NaN edges sail through every later comparison (all false),
        // storing a de-facto no-op augmentation acknowledged with an
        // expiry — reject them as a protocol error instead.
        if let Some(b) = self.0.iter().find(|b| {
            b.lower.is_some_and(|l| !l.is_finite()) || b.upper.is_some_and(|u| !u.is_finite())
        }) {
            return Err(format!(
                "augmentation bound [{:?}, {:?}] has a non-finite edge",
                b.lower, b.upper
            ));
        }
        if let Some(b) = self
            .0
            .iter()
            .find(|b| matches!((b.lower, b.upper), (Some(l), Some(u)) if l > u))
        {
            return Err(format!(
                "augmentation bound [{:?}, {:?}] is inverted (lower > upper)",
                b.lower, b.upper
            ));
        }
        Ok(())
    }

    pub fn contains(&self, value: f32) -> bool {
        self.0.iter().any(|b| bounds_contains(b, value))
    }

    /// The outer edges, first band's lower to last band's upper, as
    /// `Telemetry::metric_value` reads them; `None` when there is no
    /// band or either outer side is open.
    pub fn outer_edges(&self) -> Option<(f32, f32)> {
        Some((self.0.first()?.lower?, self.0.last()?.upper?))
    }

    /// Normalize an empty band list to the single band `(0.0, 0.0)`.
    /// An empty `VecBounds` usually means "no information" (see
    /// `sum_single`'s doc), but a live Q envelope with no legal band
    /// left — e.g. a live Q augmentation entirely disjoint from the
    /// caps band at the current P (the caps band alone never goes
    /// empty; `ReactiveCapability::q_bounds_at` always returns a
    /// well-formed `lo <= hi` pair, `(0, 0)` at worst) — means
    /// something different: zero headroom, a real answer every
    /// telemetry consumer (proto stream, WS scalar, history chart)
    /// needs to see as a present `(0, 0)` band, not an absent one
    /// that leaves stale bounds on screen. Callers with an actually-
    /// empty "no information" case must NOT reach for this — it's
    /// for the Q envelope boundary only.
    pub fn or_zero_band(self) -> Self {
        if self.0.is_empty() {
            Self::single(0.0, 0.0)
        } else {
            self
        }
    }

    /// Pull `value` to the closest edge of any bound when it is outside
    /// the union; identity if it is already inside.
    pub fn clamp(&self, value: f32) -> f32 {
        if self.0.is_empty() || self.contains(value) {
            return value;
        }
        let mut prev_upper: Option<f32> = None;
        for b in &self.0 {
            if let Some(lower) = b.lower
                && value < lower
            {
                return match prev_upper {
                    // <= so equidistant ties pull to the lower-magnitude
                    // edge (matches microsim's behaviour).
                    Some(pu) if (value - pu).abs() <= (lower - value).abs() => pu,
                    _ => lower,
                };
            }
            if let Some(upper) = b.upper {
                prev_upper = Some(upper);
            }
        }
        prev_upper.unwrap_or(value)
    }

    /// Clamp a commanded `value` to this band, with two exceptions: 0
    /// always stays 0 (a device may idle outside its band), and an
    /// empty band leaves nothing but 0. Otherwise as [`Self::clamp`].
    pub fn clamp_or_park(&self, value: f32) -> f32 {
        if value == 0.0 || self.0.is_empty() {
            0.0
        } else {
            self.clamp(value)
        }
    }

    /// Scale every edge by `factor` (positive, so band order is
    /// preserved). Divides a shared child's envelope across its
    /// parallel parents, mirroring the meter's power share.
    pub fn scale(&self, factor: f32) -> Self {
        VecBounds(
            self.0
                .iter()
                .map(|b| Bounds {
                    lower: b.lower.map(|l| l * factor),
                    upper: b.upper.map(|u| u * factor),
                })
                .collect(),
        )
    }

    /// Add bound containers element-wise into one `[lower, upper]`
    /// band. A multi-band item is collapsed to its hull (lowest
    /// lower, highest upper) first — microsim's general-case add
    /// (tracking multi-band exclusion zones through the sum) is
    /// overkill for a gate: the hull never rejects a reachable
    /// value, and a value inside a child's interior gap is still
    /// pulled to a band edge by that child's own clamp.
    ///
    /// Children with no bounds are skipped; if EVERY child is
    /// empty, the result is an empty `VecBounds` (not `[0, 0]`),
    /// so callers can tell "no information" from "pinned at zero".
    pub fn sum_single(items: impl IntoIterator<Item = Self>) -> Self {
        let mut lower = 0.0_f32;
        let mut upper = 0.0_f32;
        let mut any = false;
        for vb in items {
            if vb.0.is_empty() {
                continue;
            }
            any = true;
            // An edge joins the hull only when EVERY band has it:
            // one absent edge makes that whole side unbounded, and
            // an unbounded side contributes nothing to the sum (the
            // same as before for single-band items).
            if let Some(l) =
                vb.0.iter()
                    .try_fold(f32::INFINITY, |a, b| b.lower.map(|l| a.min(l)))
            {
                lower += l;
            }
            if let Some(u) =
                vb.0.iter()
                    .try_fold(f32::NEG_INFINITY, |a, b| b.upper.map(|u| a.max(u)))
            {
                upper += u;
            }
        }
        if !any {
            return Self::default();
        }
        Self::single(lower, upper)
    }

    pub fn intersect(&self, other: &Self) -> Self {
        let mut result = Vec::new();
        for b1 in &self.0 {
            for b2 in &other.0 {
                if let Some(int) = bounds_intersect(b1, b2) {
                    result.push(int);
                }
            }
        }
        squash(result)
    }
}

fn bounds_contains(b: &Bounds, value: f32) -> bool {
    if let Some(l) = b.lower
        && value < l
    {
        return false;
    }
    if let Some(u) = b.upper
        && value > u
    {
        return false;
    }
    true
}

/// `None` means the two bands are disjoint. A `Some` with an absent
/// edge keeps that side unbounded — an edgeless proto band means "no
/// bound on this side", so the intersection of two fully-unbounded
/// bands is a fully-unbounded band, not an empty one.
fn bounds_intersect(a: &Bounds, b: &Bounds) -> Option<Bounds> {
    fn pick(a: Option<f32>, b: Option<f32>, op: impl FnOnce(f32, f32) -> f32) -> Option<f32> {
        match (a, b) {
            (Some(a), Some(b)) => Some(op(a, b)),
            (Some(x), None) | (None, Some(x)) => Some(x),
            (None, None) => None,
        }
    }
    let lower = pick(a.lower, b.lower, f32::max);
    let upper = pick(a.upper, b.upper, f32::min);
    if let (Some(l), Some(u)) = (lower, upper)
        && l > u
    {
        return None;
    }
    Some(Bounds { lower, upper })
}

fn merge_if_overlapping(a: &Bounds, b: &Bounds) -> Option<Bounds> {
    if bounds_intersect(a, b).is_some() {
        Some(Bounds {
            lower: a.lower.and_then(|x| b.lower.map(|y| x.min(y))),
            upper: a.upper.and_then(|x| b.upper.map(|y| x.max(y))),
        })
    } else {
        None
    }
}

fn squash(mut input: Vec<Bounds>) -> VecBounds {
    input.sort_by(|a, b| {
        a.lower
            .unwrap_or(f32::MIN)
            .partial_cmp(&b.lower.unwrap_or(f32::MIN))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if input.is_empty() {
        return VecBounds(input);
    }
    let mut squashed = Vec::new();
    let mut current = input[0];
    for next in &input[1..] {
        if let Some(merged) = merge_if_overlapping(&current, next) {
            current = merged;
        } else {
            squashed.push(current);
            current = *next;
        }
    }
    squashed.push(current);
    VecBounds(squashed)
}

/// A queue of time-limited augmentations. It holds no static band:
/// the caller intersects [`ComponentBounds::effective_at`] into its
/// own.
#[derive(Debug, Clone, Default)]
pub struct ComponentBounds {
    augmented: VecDeque<Aug>,
}

#[derive(Debug, Clone)]
struct Aug {
    create_ts: DateTime<Utc>,
    bounds: VecBounds,
    lifetime: Duration,
}

impl Aug {
    /// Live at `now` if `now` is before the advertised `valid_until`
    /// (create_ts + lifetime, saturating) — the same inclusive
    /// horizon handed back to the client, so an absurdly long
    /// lifetime means "effectively forever", not "never live".
    fn live_at(&self, now: DateTime<Utc>) -> bool {
        deadline_after(self.create_ts, self.lifetime) > now
    }
}

impl ComponentBounds {
    pub fn add_augmentation(
        &mut self,
        create_ts: DateTime<Utc>,
        bounds: VecBounds,
        lifetime: Duration,
    ) {
        self.augmented.push_back(Aug {
            create_ts,
            bounds,
            lifetime,
        });
    }

    pub fn drop_expired(&mut self, now: DateTime<Utc>) {
        // Augmentations are stored in arrival order, but lifetimes are
        // per-request, so expiry order need not match arrival order — a
        // front-only pop would strand a short-lived entry behind a
        // longer-lived one and leak it. Scan the whole deque.
        self.augmented.retain(|a| a.live_at(now));
    }

    /// The live augmentations at `now`, intersected with each other;
    /// empty when none are live. Expired augmentations are skipped
    /// even if `drop_expired` has not reaped them yet, so a gate that
    /// runs between ticks sees the same envelope the client does —
    /// not a stale one lingering up to a tick past its `valid_until`.
    pub fn effective_at(&self, now: DateTime<Utc>) -> VecBounds {
        let mut out: Option<VecBounds> = None;
        for a in &self.augmented {
            if a.live_at(now) {
                out = Some(match out {
                    None => a.bounds.clone(),
                    Some(acc) => acc.intersect(&a.bounds),
                });
            }
        }
        out.unwrap_or_default()
    }

    /// Whether any augmentation is still live at `now`. The companion
    /// [`Self::effective_at`] needs: an empty result there means "no
    /// constraint" only when nothing is live — with live
    /// augmentations that exclude each other it means the opposite,
    /// "nothing is legal". Nobody can tell those apart from the
    /// returned bands alone.
    pub fn has_live_augmentations(&self, now: DateTime<Utc>) -> bool {
        self.augmented.iter().any(|a| a.live_at(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_or_park_keeps_zero_parks_on_empty_and_clamps() {
        let band = VecBounds::single(1000.0, 5000.0);
        assert_eq!(band.clamp_or_park(0.0), 0.0);
        assert_eq!(band.clamp_or_park(3000.0), 3000.0);
        assert_eq!(band.clamp_or_park(8000.0), 5000.0);
        assert_eq!(band.clamp_or_park(200.0), 1000.0);
        assert_eq!(VecBounds::default().clamp_or_park(3000.0), 0.0);
    }

    /// Each shape rule rejects with its own message; a well-formed
    /// band, open edges included, passes.
    #[test]
    fn augmentation_shape_checks() {
        let err = |b: VecBounds| b.check_augmentation_shape().unwrap_err();
        assert!(err(VecBounds::default()).contains("contains no bounds"));
        assert!(err(VecBounds::single(f32::NAN, 1.0)).contains("non-finite edge"));
        assert!(err(VecBounds::single(1.0, -1.0)).contains("is inverted"));
        let open = VecBounds(vec![Bounds {
            lower: None,
            upper: Some(1.0),
        }]);
        assert!(open.check_augmentation_shape().is_ok());
    }

    /// `or_zero_band` normalizes an empty band list to a single
    /// `(0.0, 0.0)` band, and leaves a non-empty `VecBounds`
    /// (including a genuinely empty-content single band, like a
    /// caps-only kVA rim, which already has one entry) untouched.
    #[test]
    fn or_zero_band_normalizes_empty_and_leaves_non_empty_alone() {
        let normalized_empty = VecBounds::default().or_zero_band();
        assert_eq!(normalized_empty.0.len(), 1);
        assert_eq!(
            (normalized_empty.0[0].lower, normalized_empty.0[0].upper),
            (Some(0.0), Some(0.0))
        );

        let two_band = VecBounds(vec![
            Bounds {
                lower: Some(-30.0),
                upper: Some(-10.0),
            },
            Bounds {
                lower: Some(10.0),
                upper: Some(30.0),
            },
        ]);
        let normalized = two_band.clone().or_zero_band();
        assert_eq!(normalized.0.len(), 2, "non-empty input passes through");
        assert_eq!(normalized.0[0].lower, two_band.0[0].lower);
    }

    /// A band with neither edge set means "no bound on either side"
    /// (the proto documents absent floats exactly that way), so
    /// intersecting two of them keeps an unbounded band. Only a
    /// genuinely disjoint pair produces an empty result — the two
    /// used to share the `{None, None}` sentinel, and a pair of
    /// fully-unbounded augmentations emptied a Q axis's envelope.
    #[test]
    fn intersect_keeps_unbounded_bands_and_drops_only_disjoint_ones() {
        let unbounded = VecBounds::new(vec![Bounds {
            lower: None,
            upper: None,
        }]);
        let both = unbounded.intersect(&unbounded);
        assert_eq!(both.0.len(), 1, "unbounded ∩ unbounded is unbounded");
        assert_eq!((both.0[0].lower, both.0[0].upper), (None, None));

        let low = VecBounds::single(-4000.0, -3000.0);
        let high = VecBounds::single(500.0, 2000.0);
        assert!(
            low.intersect(&high).0.is_empty(),
            "a disjoint pair still empties the intersection"
        );
    }

    /// A multi-band child contributes its hull to the sum, not just
    /// its first band — a `[[-10,-5],[5,10]]` inverter summed with a
    /// `[0,2]` sibling gates on `[-10, 12]`, so a setpoint reachable
    /// via the second band is not rejected.
    #[test]
    fn sum_single_takes_the_hull_of_a_multi_band_child() {
        let multi = VecBounds::new(vec![
            Bounds {
                lower: Some(-10.0),
                upper: Some(-5.0),
            },
            Bounds {
                lower: Some(5.0),
                upper: Some(10.0),
            },
        ]);
        let single = VecBounds::single(0.0, 2.0);
        let sum = VecBounds::sum_single([multi, single]);
        assert_eq!(sum.0.len(), 1);
        assert_eq!((sum.0[0].lower, sum.0[0].upper), (Some(-10.0), Some(12.0)));
    }

    /// One absent edge in ANY band makes that side of the child's
    /// hull unbounded, so it contributes nothing to the sum — the
    /// other band's finite edge must NOT be summed in its place —
    /// that would tighten the gate beyond the contribute-nothing
    /// conservatism an unbounded side already deliberately gets.
    #[test]
    fn sum_single_half_open_band_unbounds_that_side_of_the_hull() {
        let half_open = VecBounds::new(vec![
            Bounds {
                lower: None,
                upper: Some(-100.0),
            },
            Bounds {
                lower: Some(500.0),
                upper: Some(1000.0),
            },
        ]);
        let single = VecBounds::single(0.0, 2.0);
        let sum = VecBounds::sum_single([half_open, single]);
        assert_eq!(sum.0.len(), 1);
        // Lower: the half-open child skips (unbounded below — the
        // buggy form summed the 500); upper: hull max 1000 + 2.
        assert_eq!((sum.0[0].lower, sum.0[0].upper), (Some(0.0), Some(1002.0)));
    }

    #[test]
    fn contains_and_clamp() {
        let vb = VecBounds::new(vec![
            Bounds {
                lower: Some(-30.0),
                upper: Some(-10.0),
            },
            Bounds {
                lower: Some(10.0),
                upper: Some(30.0),
            },
        ]);
        assert!(vb.contains(-20.0));
        assert!(!vb.contains(0.0));
        assert_eq!(vb.clamp(-20.0), -20.0);
        // 0 is closer to -10 than to 10 → -10
        assert_eq!(vb.clamp(0.0), -10.0);
        assert_eq!(vb.clamp(100.0), 30.0);
    }

    #[test]
    fn effective_at_skips_expired_augmentation_before_reaping() {
        // A tight augment loop (e.g. a GCP limiter) can push a fresh
        // augmentation in the sub-tick window after an old one's TTL
        // lapses but before `drop_expired` reaps it. `effective_at` must
        // already ignore the lapsed entry so the validation gate sees the
        // real envelope, not a stale one lingering up to a tick.
        let mut cb = ComponentBounds::default();
        let t0 = Utc::now();
        cb.add_augmentation(t0, VecBounds::single(-30.0, 0.0), Duration::from_secs(5));

        // Still live a second in.
        let live = cb.effective_at(t0 + chrono::Duration::seconds(1));
        assert_eq!((live.0[0].lower, live.0[0].upper), (Some(-30.0), Some(0.0)));

        // A second past its valid_until, with drop_expired NOT called
        // (the deque still holds it): the augmentation is ignored, so
        // a fresh augmentation disjoint from the lapsed one (e.g.
        // [50, 100]) is not rejected as disjoint.
        let after = t0 + chrono::Duration::seconds(6);
        assert!(cb.effective_at(after).0.is_empty());
        assert!(!cb.has_live_augmentations(after));
    }

    /// With no live augmentations the effective bounds are empty
    /// (unconstrained), with one live augmentation they equal it, and
    /// with two they intersect.
    #[test]
    fn effective_at_intersects_live_augmentations_alone() {
        let mut cb = ComponentBounds::default();
        let t0 = Utc::now();
        assert!(cb.effective_at(t0).0.is_empty());

        cb.add_augmentation(
            t0,
            VecBounds::single(-1_000.0, 1_000.0),
            Duration::from_secs(60),
        );
        let eff = cb.effective_at(t0);
        assert_eq!(
            (eff.0[0].lower, eff.0[0].upper),
            (Some(-1_000.0), Some(1_000.0))
        );

        // A second, tighter live augmentation intersects in.
        cb.add_augmentation(
            t0,
            VecBounds::single(-200.0, 500.0),
            Duration::from_secs(60),
        );
        let eff = cb.effective_at(t0);
        assert_eq!(
            (eff.0[0].lower, eff.0[0].upper),
            (Some(-200.0), Some(500.0))
        );

        // Once both expire, the envelope is unconstrained again.
        let eff = cb.effective_at(t0 + chrono::Duration::seconds(120));
        assert!(eff.0.is_empty());
    }

    /// An empty `effective_at` is ambiguous on its own: it means
    /// "nothing live, so no constraint" OR "live augmentations that
    /// exclude each other, so nothing is legal".
    /// `has_live_augmentations` is what tells the two apart — callers
    /// (`GatewayAxis::validation_envelope`) must fold the second case
    /// in as a real, if degenerate, constraint instead of skipping
    /// it.
    #[test]
    fn has_live_augmentations_separates_unconstrained_from_mutually_disjoint() {
        let mut cb = ComponentBounds::default();
        let t0 = Utc::now();
        assert!(!cb.has_live_augmentations(t0), "nothing armed yet");

        cb.add_augmentation(
            t0,
            VecBounds::single(-4_000.0, -3_000.0),
            Duration::from_secs(60),
        );
        cb.add_augmentation(
            t0,
            VecBounds::single(-500.0, 500.0),
            Duration::from_secs(60),
        );
        assert!(
            cb.effective_at(t0).0.is_empty(),
            "the two augmentations exclude each other"
        );
        assert!(
            cb.has_live_augmentations(t0),
            "…but they ARE live: empty here means 'nothing is legal'"
        );

        // Once both lapse the emptiness means "unconstrained" again.
        let later = t0 + chrono::Duration::seconds(120);
        assert!(cb.effective_at(later).0.is_empty());
        assert!(!cb.has_live_augmentations(later));
    }
}
