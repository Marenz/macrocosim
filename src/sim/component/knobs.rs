//! Inputs that belong to one kind of component (meter, solar
//! inverter, steam boiler, EV charger), set by Lisp commands,
//! scenarios and the HTTP drive op. The reactive caps the inverters
//! share are `ReactiveLimits`.

use crate::sim::{
    component::{ReactiveReading, ScalarReading},
    dynamic_scalar::DynamicScalar,
    ev_presets::{ConnectedEv, EvInfo},
};

/// A meter's driven sources: the active and reactive power it
/// publishes in place of measuring its children.
pub trait MeterDrive: Send + Sync {
    /// Override the active-power value the meter publishes with a
    /// constant. Used by `(set-meter-power id W)` when called with a
    /// numeric argument.
    fn set_active_power_override(&self, p: f32);

    /// Replace the meter's `:power-w` source with a Lisp expression
    /// that the scheduler's `refresh_inputs` pass re-resolves each
    /// tick. Used by `(set-meter-power id (lambda () …))` and by
    /// the UI when a user types a Lisp form into the `:power-w` input.
    fn set_active_power_source(&self, scalar: DynamicScalar);

    /// Drop the meter's active-power override, returning it to
    /// measuring its children's aggregate — the way back from
    /// [`Self::set_active_power_override`] /
    /// [`Self::set_active_power_source`]. Also drops the
    /// construction-time `:power-w` kwarg for this axis so a
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
    /// construction-time `:reactive-power-var` / `:power-factor` kwarg
    /// for this axis too.
    fn clear_reactive_power_source(&self);

    /// The meter's active-power source knob, as configured — a live
    /// value plus, for a dynamic (lambda / symbol) source, the
    /// printed Lisp expression driving it (`None` for a plain
    /// constant). This is the `:power-w` input side, not the Q
    /// envelope. `None` while the meter is measuring its children.
    fn meter_power_reading(&self) -> Option<ScalarReading>;

    /// The meter's reactive-power source knob — either a direct VAr
    /// value (mirrors [`Self::meter_power_reading`]'s shape) or a
    /// power-factor derivation from the meter's own live P. `None`
    /// while the meter is summing its children.
    fn meter_reactive_reading(&self) -> Option<ReactiveReading>;
}

/// The solar inverter's sunlight knob: the cloud-cover percentage
/// that caps its output. One built without `:sunlight%` follows the
/// site's weather until something drives it.
pub trait SunlightDrive: Send + Sync {
    /// Drive the sunlight percentage with a constant. Used by
    /// `(set-solar-sunlight id PCT)` with a number and by the HTTP
    /// drive op. Collapses any prior source — a Lisp expression or
    /// the weather — until [`Self::clear_sunlight_source`].
    fn set_sunlight_pct(&self, pct: f32);

    /// Drive the sunlight percentage with a Lisp expression that
    /// `refresh_inputs` re-resolves each tick. Used by
    /// `(set-solar-sunlight id (lambda () …))` and by a `:sunlight%`
    /// bound to a lambda or symbol at construction.
    fn set_sunlight_source(&self, scalar: DynamicScalar);

    /// Drop whatever drives the sunlight percentage — a constant or
    /// a Lisp expression — and go back to following the site's
    /// weather: the way back from [`Self::set_sunlight_pct`] and
    /// [`Self::set_sunlight_source`]. A cleared inverter renders
    /// without `:sunlight%`, so a save/reload follows the weather
    /// too.
    fn clear_sunlight_source(&self);

    /// The live percentage, with the printed Lisp source of a dynamic
    /// source, or the `"weather"` marker while following the weather
    /// (`None` for a constant).
    fn sunlight_reading(&self) -> ScalarReading;
}

/// The steam boiler's driven inputs: its steam demand and its
/// pressure state.
pub trait SteamDrive: Send + Sync {
    /// Drive the steam demand (kg/h) with a constant. Collapses any
    /// prior dynamic source, like
    /// [`SunlightDrive::set_sunlight_pct`].
    fn set_steam_demand_kg_h(&self, kg_h: f32);

    /// Drive the steam demand with a Lisp expression that
    /// `refresh_inputs` re-resolves each tick.
    fn set_steam_demand_source(&self, scalar: DynamicScalar);

    /// Overwrite the pressure state (bar).
    fn set_pressure_bar(&self, bar: f32);

    /// The live demand (kg/h), with the printed Lisp source of a
    /// dynamic source (`None` for a constant).
    fn demand_reading(&self) -> ScalarReading;

    /// The live pressure (bar), for the inspector knob; `expr` is
    /// always `None`.
    fn pressure_reading(&self) -> ScalarReading;

    /// The thermostat target (bar), for chart annotation.
    fn pressure_target_bar(&self) -> f32;
}

/// An EV charger's car port. The car is runtime state, driven by
/// `plug-ev` / `unplug-ev` and read by `ev-info`; it is never a
/// construction kwarg, so the managed file never renders it.
pub trait EvPort: Send + Sync {
    /// Plug `ev` in. Errors when a car is already plugged in.
    fn plug_ev(&self, ev: ConnectedEv) -> Result<(), String>;

    /// Unplug the connected car. `false` when there was none.
    fn unplug_ev(&self) -> bool;

    /// The connected car and what the charger is doing with it, or
    /// `None` for an empty charger.
    fn ev_info(&self) -> Option<EvInfo>;

    /// Teleport the plugged car's state of charge to `pct` (clamped
    /// to 0..=100; a non-finite value is ignored). `false` when no
    /// car is plugged in, so the caller can say so.
    fn set_ev_soc_pct(&self, pct: f32) -> bool;
}
