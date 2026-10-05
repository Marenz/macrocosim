//! `DcStorage`: the battery side of the DC bus.

use crate::sim::decay::SocProtect;

/// A store of charge on a DC bus. A battery inverter splits its push
/// across the `DcStorage` children it finds healthy and reads back
/// how much each one took; the gateway throttles the bounds from the
/// SoC window and the SoC.
pub trait DcStorage: Send + Sync {
    /// Add an inverter's active push to this tick's accumulator; the
    /// component settles the sum on its own tick. Active power only:
    /// Q terminates at the inverter and never reaches a DC bus.
    fn set_dc_power(&self, p: f32);

    /// Share of last tick's pushed DC power this component accepted,
    /// in [0, 1]: `accepted / pushed`. A parent multiplies its own
    /// push by this to report what actually flowed, so a battery
    /// clipping at its hardware limits pulls every inverter on its
    /// bus down in proportion. One tick stale by construction: on the
    /// tick a parent changes its push, its report still uses the
    /// ratio of the previous mix.
    fn dc_accept_ratio(&self) -> f32;

    /// The state of charge in %.
    fn soc_pct(&self) -> f32;

    /// The usable SoC window (`:soc-lower`, `:soc-upper`,
    /// `:soc-protect-margin`) the gateway throttles the bounds by.
    fn soc_window(&self) -> SocProtect;

    /// Teleport the state of charge to `pct` (clamped to 0..=100; a
    /// non-finite value is ignored). Lets a test arrange a
    /// precondition (a nearly-empty or nearly-full pool) without
    /// simulating hours of charging.
    fn set_soc_pct(&self, pct: f32);
}
