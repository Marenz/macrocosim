//! The runtime caps on a component's reactive envelope.

use crate::sim::reactive::ReactiveCapability;

/// The power-factor and apparent-power (kVA) caps that shape a
/// component's reactive envelope, read and set at runtime.
pub trait ReactiveLimits: Send + Sync {
    /// The Q axis's capability shape (PF cap, kVA cap, both, or
    /// neither) — the data behind the gateway's live Q envelope
    /// (`site.bounds_of`, via `ReactiveCapability::q_band_at`).
    /// "Static" relative to that envelope means P-independent, not
    /// fixed forever: the caps this returns are the CURRENT
    /// runtime-set PF/kVA limits (mutable via
    /// `set-reactive-pf-limit` / `set-reactive-apparent-va`), not a
    /// construction-time nameplate. `make_component_proto` uses this
    /// (via `ReactiveCapability::hull`) to advertise the reactive
    /// config bound instead of a live-P sample.
    fn reactive_capability(&self) -> ReactiveCapability;

    /// Replace the PF cap on the reactive envelope. `None` disables
    /// the PF constraint. Mirrors the SunSpec / IEEE 1547-2018 PF
    /// setpoint surface a real EMS pushes via Modbus.
    fn set_reactive_pf_limit(&self, pf: Option<f32>);

    /// Replace the apparent-power (kVA) cap on the reactive
    /// envelope. `None` disables the kVA constraint.
    fn set_reactive_apparent_va(&self, va: Option<f32>);
}
