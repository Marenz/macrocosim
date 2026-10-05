//! The scenario and UI knob families: the inputs a scenario, a Lisp
//! command or the inspector drives on one kind of component.

use crate::sim::{
    component::{ReactiveReading, ScalarReading},
    dynamic_scalar::DynamicScalar,
};

/// A meter's driven sources: the active and reactive power it
/// publishes in place of measuring its children.
pub trait MeterDrive: Send + Sync {
    /// Override the active-power value the meter publishes with a
    /// constant. Used by `(set-meter-power id W)` when called with a
    /// numeric argument.
    fn set_active_power_override(&self, p: f32);

    /// Replace the meter's `:power` source with a Lisp expression
    /// that the scheduler's `refresh_inputs` pass re-resolves each
    /// tick. Used by `(set-meter-power id (lambda () …))` and by
    /// the UI when a user types a Lisp form into the `:power` input.
    fn set_active_power_source(&self, scalar: DynamicScalar);

    /// Drop the meter's active-power override, returning it to
    /// measuring its children's aggregate — the way back from
    /// [`Self::set_active_power_override`] /
    /// [`Self::set_active_power_source`]. Also drops the
    /// construction-time `:power` kwarg for this axis so a
    /// save/reload agrees with the measuring live state instead of
    /// resurrecting the cleared override.
    fn clear_active_power_source(&self);

    /// Override the reactive-power value the meter publishes with a
    /// constant. The Q twin of [`Self::set_active_power_override`].
    fn set_reactive_power_override(&self, vars: f32);

    /// Replace the meter's reactive-power source with a Lisp
    /// expression re-resolved each tick. The Q twin of
    /// [`Self::set_active_power_source`].
    fn set_reactive_power_source(&self, scalar: DynamicScalar);

    /// Replace the meter's reactive-power source with a power-factor
    /// derivation that tracks the meter's own live active power.
    ///
    /// Does NOT validate `pf`: every caller must enforce
    /// `pf ∈ (0.0, 1.0]` itself before calling (both
    /// `set-meter-power-factor` in Lisp and the HTTP drive op do),
    /// because an out-of-range factor lands silently otherwise.
    fn set_power_factor(&self, pf: f32, leading: bool);

    /// Drop the meter's reactive-power override — whichever of
    /// `Var` / `PowerFactor` is set — returning it to summing its
    /// children's Q. The Q twin of
    /// [`Self::clear_active_power_source`]: it drops the
    /// construction-time `:reactive-power` / `:power-factor` kwarg
    /// for this axis too.
    fn clear_reactive_power_source(&self);

    /// The meter's active-power source knob, as configured — a live
    /// value plus, for a dynamic (lambda / symbol) source, the
    /// printed Lisp expression driving it (`None` for a plain
    /// constant). This is the `:power` input side, not the Q
    /// envelope. `None` while the meter is measuring its children.
    fn meter_power_reading(&self) -> Option<ScalarReading>;

    /// The meter's reactive-power source knob — either a direct VAr
    /// value (mirrors [`Self::meter_power_reading`]'s shape) or a
    /// power-factor derivation from the meter's own live P. `None`
    /// while the meter is summing its children.
    fn meter_reactive_reading(&self) -> Option<ReactiveReading>;
}
