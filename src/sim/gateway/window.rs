//! The SoC window: each battery's throttled bounds, and each battery
//! inverter's share of the room they leave.

use std::collections::HashMap;

use crate::{
    proto::common::metrics::Bounds,
    sim::{
        bounds::VecBounds,
        decay::{SocProtect, soc_protected_bounds},
    },
};

/// One battery's SoC window: its usable band and taper, and the
/// throttled bounds they give at the battery's last SoC.
pub(super) struct BatteryWindow {
    protect: SocProtect,
    rated: (f32, f32),
    throttled: (f32, f32),
}

impl BatteryWindow {
    pub(super) fn new(protect: SocProtect, rated: (f32, f32), soc: f32) -> Self {
        let mut w = Self {
            protect,
            rated,
            throttled: rated,
        };
        w.refresh(soc);
        w
    }

    /// Recompute the throttled bounds at `soc` (taper base 1.2, floor
    /// 0.3, closing at the window's limits).
    pub(super) fn refresh(&mut self, soc: f32) {
        self.throttled = soc_protected_bounds(self.rated.0, self.rated.1, soc, self.protect);
    }

    /// The throttled bounds.
    pub(super) fn bounds(&self) -> VecBounds {
        VecBounds::single(self.throttled.0, self.throttled.1)
    }
}

/// One inverter's push into one battery on one side this tick: the
/// farther from 0 of its clamped target and its ramp output on that
/// side, split equally across its healthy batteries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Push {
    pub inverter: u64,
    pub battery: u64,
    pub watts: f32,
}

/// One inverter's pushes of one sign: how many, the one nearest 0
/// after scaling, and whether any of them was scaled.
#[derive(Clone, Copy, Default)]
struct Side {
    n: u16,
    least: Option<f32>,
    binds: bool,
}

impl Side {
    /// The most the inverter can put out on this side without
    /// overloading any of its batteries: it splits its output
    /// equally, so `n` times its push nearest 0.
    fn limit(&self) -> Option<f32> {
        self.binds
            .then(|| f32::from(self.n) * self.least.unwrap_or(0.0))
    }
}

/// The window share of every inverter whose pushes a battery's room
/// binds. Per battery and per sign, same-sign pushes that sum past
/// the battery's throttled bound on that side are scaled down in
/// proportion; pushes inside it, and pushes of 0, are left alone.
/// Opposite signs are never scaled against each other. A share limits
/// only a binding side: the inverter splits its output equally across
/// its batteries, so the limit is the number of its pushes of that
/// sign times the one nearest 0 after scaling. An inverter nothing
/// binds gets no share. Every share contains 0: a side's limit is
/// never past 0. `throttled` gives a battery's `(lower, upper)`;
/// `None` binds nothing.
pub(super) fn shares(
    pushes: &[Push],
    throttled: impl Fn(u64) -> Option<(f32, f32)>,
) -> HashMap<u64, VecBounds> {
    let mut by_battery: HashMap<u64, Vec<Push>> = HashMap::new();
    for p in pushes {
        by_battery.entry(p.battery).or_default().push(*p);
    }
    // Per inverter: (discharge, charge).
    let mut sides: HashMap<u64, (Side, Side)> = HashMap::new();
    for (battery, ps) in by_battery {
        let (k_neg, k_pos) = match throttled(battery) {
            Some((lo, hi)) => {
                let neg: f32 = ps.iter().map(|p| p.watts.min(0.0)).sum();
                let pos: f32 = ps.iter().map(|p| p.watts.max(0.0)).sum();
                (scale(neg, lo.min(0.0)), scale(pos, hi.max(0.0)))
            }
            None => (None, None),
        };
        for p in ps.iter().filter(|p| p.watts != 0.0) {
            let (neg, pos) = sides.entry(p.inverter).or_default();
            let (side, k) = if p.watts > 0.0 {
                (pos, k_pos)
            } else {
                (neg, k_neg)
            };
            let scaled = p.watts * k.unwrap_or(1.0);
            side.n += 1;
            side.least = Some(
                side.least
                    .filter(|l| l.abs() <= scaled.abs())
                    .unwrap_or(scaled),
            );
            side.binds |= k.is_some();
        }
    }
    sides
        .into_iter()
        .filter(|(_, (neg, pos))| neg.binds || pos.binds)
        .map(|(inv, (neg, pos))| {
            let band = Bounds {
                lower: neg.limit(),
                upper: pos.limit(),
            };
            (inv, VecBounds::new(vec![band]))
        })
        .collect()
}

/// The factor that brings a same-sign `sum` back to `limit` (same
/// sign or 0); `None` when it is already inside.
fn scale(sum: f32, limit: f32) -> Option<f32> {
    (sum.abs() > limit.abs()).then(|| limit / sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(inverter: u64, battery: u64, watts: f32) -> Push {
        Push {
            inverter,
            battery,
            watts,
        }
    }

    fn band(shares: &HashMap<u64, VecBounds>, inv: u64) -> String {
        shares[&inv].to_string()
    }

    /// Same-sign pushes past the bound are scaled in proportion, and
    /// each share limits only that side.
    #[test]
    fn same_sign_pushes_past_the_bound_are_scaled_in_proportion() {
        let s = shares(&[push(1, 9, 4_000.0), push(2, 9, 2_000.0)], |_| {
            Some((-3_000.0, 3_000.0))
        });
        assert_eq!(band(&s, 1), "[*, 2000]");
        assert_eq!(band(&s, 2), "[*, 1000]");
        let d = shares(&[push(1, 9, -4_000.0), push(2, 9, -2_000.0)], |_| {
            Some((-3_000.0, 3_000.0))
        });
        assert_eq!(band(&d, 1), "[-2000, *]");
        assert_eq!(band(&d, 2), "[-1000, *]");
    }

    /// Opposite signs are never netted or scaled against each other:
    /// each sign is held against its own side.
    #[test]
    fn opposite_signs_are_not_scaled_against_each_other() {
        let s = shares(&[push(1, 9, 4_000.0), push(2, 9, -4_000.0)], |_| {
            Some((-5_000.0, 3_000.0))
        });
        assert_eq!(band(&s, 1), "[*, 3000]");
        assert!(!s.contains_key(&2), "the discharge side has room");
        let both = shares(&[push(1, 9, 4_000.0), push(2, 9, -4_000.0)], |_| {
            Some((-2_000.0, 3_000.0))
        });
        assert_eq!(band(&both, 1), "[*, 3000]");
        assert_eq!(band(&both, 2), "[-2000, *]");
    }

    /// Pushes inside the room get no window term at all; no room on a
    /// side closes that side.
    #[test]
    fn pushes_inside_the_bound_get_no_term_and_zero_room_closes_the_side() {
        let s = shares(&[push(1, 9, 1_000.0), push(2, 9, 2_000.0)], |_| {
            Some((-3_000.0, 3_000.0))
        });
        assert!(s.is_empty(), "nothing binds: {s:?}");
        let full = shares(&[push(1, 9, 1_000.0)], |_| Some((-3_000.0, 0.0)));
        assert_eq!(band(&full, 1), "[*, 0]");
    }

    /// An inverter pushing 0 next to one that fills the room gets no
    /// window term, so it can still start.
    #[test]
    fn a_zero_push_gets_no_term() {
        let s = shares(&[push(1, 9, 4_000.0), push(2, 9, 0.0)], |_| {
            Some((-3_000.0, 3_000.0))
        });
        assert_eq!(band(&s, 1), "[*, 3000]");
        assert!(!s.contains_key(&2));
    }

    /// An inverter on two batteries splits its output equally, so the
    /// battery with the least room sets its share for both; a battery
    /// without bounds scales nothing.
    #[test]
    fn an_inverter_on_two_batteries_is_held_to_its_tightest_battery() {
        let s = shares(&[push(1, 8, 3_000.0), push(1, 9, 3_000.0)], |b| {
            (b == 8).then_some((-1_000.0, 1_000.0))
        });
        assert_eq!(band(&s, 1), "[*, 2000]");
        let d = shares(&[push(1, 8, -3_000.0), push(1, 9, -3_000.0)], |b| {
            Some(if b == 8 {
                (-1_000.0, 1_000.0)
            } else {
                (-2_000.0, 1_000.0)
            })
        });
        assert_eq!(band(&d, 1), "[-2000, *]");
        let open = shares(&[push(1, 8, 3_000.0)], |_| None);
        assert!(open.is_empty());
    }

    /// The window throttles with today's taper and closes at the
    /// limits.
    #[test]
    fn the_window_throttles_with_todays_taper() {
        use crate::sim::decay::SocProtect;
        let mut w = BatteryWindow::new(
            SocProtect::new(10.0, 90.0, 10.0),
            (-30_000.0, 30_000.0),
            50.0,
        );
        assert_eq!(w.bounds().to_string(), "[-30000, 30000]");
        w.refresh(85.0);
        let hi = w.bounds().0[0].upper.unwrap();
        assert!(hi > 0.0 && hi < 30_000.0, "tapered, got {hi}");
        w.refresh(90.0);
        assert_eq!(w.bounds().0[0].upper, Some(0.0));
        w.refresh(10.0);
        assert_eq!(w.bounds().0[0].lower, Some(0.0));
    }
}
