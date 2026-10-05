use std::{fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use tulisp::TulispContext;

use crate::sim::{
    bounds::VecBounds,
    dynamic_scalar::DynamicScalar,
    inverter::solar_inverter::SunlightSource,
    meter::{ConstructedReactive, ReactiveSource},
    microgrid_site::MicrogridSite,
};

mod controllable;

pub use controllable::{Controllable, GatewaySettings};

/// High-level kind of a component, mirroring the proto category enum but
/// kept Rust-side so non-gRPC code does not need to depend on protobuf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Grid,
    Meter,
    Inverter,
    Battery,
    EvCharger,
    Chp,
    WindTurbine,
    SteamBoiler,
    PowerTransformer,
    Breaker,
}

impl Category {
    /// Canonical lowercase slug — the one vocabulary shared by the
    /// topology API and the scenario CSV filenames.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Grid => "grid",
            Category::Meter => "meter",
            Category::Inverter => "inverter",
            Category::Battery => "battery",
            Category::EvCharger => "ev-charger",
            Category::Chp => "chp",
            Category::WindTurbine => "wind-turbine",
            Category::SteamBoiler => "steam-boiler",
            Category::PowerTransformer => "power-transformer",
            Category::Breaker => "breaker",
        }
    }
}

/// Proto state label from the sign of P: positive = charging,
/// negative = discharging (a PV inverter delivering power reads as
/// discharging too — the proto has no "generating" code), zero =
/// ready. Shared by the battery and both inverters.
pub(crate) fn power_state(p: f32) -> &'static str {
    if p > 0.0 {
        "charging"
    } else if p < 0.0 {
        "discharging"
    } else {
        "ready"
    }
}

/// A component's declared capability — a microgrid CONFIG parameter,
/// not a runtime state. The runtime fault knobs (telemetry mode,
/// command mode, health) depend on it: an `Inactive` component can
/// never stream telemetry, whatever the knobs say. The formula
/// engine reads this mode to decide whether a component can be a
/// measurement source.
///
/// A deliberate twin of the graph crate's `OperationalMode`
/// (lifted 1:1 in `graph_adapter::lift_mode`): the local type
/// carries the Lisp `FromStr`/`Display`/plist impls the foreign
/// type can't. Keep `provides_telemetry` in sync with the graph
/// crate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OperationalMode {
    /// Not explicitly set; treated as full capability.
    #[default]
    Unspecified,
    /// Not operational: no telemetry, no control.
    Inactive,
    /// Streams telemetry, rejects control commands.
    TelemetryOnly,
    /// Accepts control commands, streams no telemetry.
    ControlOnly,
    /// Full capability, explicitly declared.
    ControlAndTelemetry,
}

impl OperationalMode {
    /// Whether a component in this mode streams telemetry.
    pub fn provides_telemetry(self) -> bool {
        matches!(
            self,
            Self::Unspecified | Self::TelemetryOnly | Self::ControlAndTelemetry
        )
    }

    /// Whether a component in this mode accepts control commands.
    pub fn accepts_control(self) -> bool {
        matches!(
            self,
            Self::Unspecified | Self::ControlOnly | Self::ControlAndTelemetry
        )
    }
}

impl std::str::FromStr for OperationalMode {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "unspecified" => Ok(Self::Unspecified),
            "inactive" => Ok(Self::Inactive),
            "telemetry-only" => Ok(Self::TelemetryOnly),
            "control-only" => Ok(Self::ControlOnly),
            "control-and-telemetry" => Ok(Self::ControlAndTelemetry),
            _ => Err(()),
        }
    }
}

impl fmt::Display for OperationalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unspecified => "unspecified",
            Self::Inactive => "inactive",
            Self::TelemetryOnly => "telemetry-only",
            Self::ControlOnly => "control-only",
            Self::ControlAndTelemetry => "control-and-telemetry",
        })
    }
}

/// Why an augmentation was not stored.
#[derive(Debug)]
pub enum AugmentError {
    /// No bands, a non-finite edge, or an inverted band.
    Malformed(String),
    /// The component has no augmentation storage on that axis (the
    /// gateway answers UNIMPLEMENTED, as for a setpoint it takes none
    /// of).
    Unsupported,
    /// The proposal does not overlap the component's current envelope,
    /// carried here: what a client could still command, not the empty
    /// composed result.
    Disjoint(VecBounds),
    /// No component with this id is registered.
    NotFound(u64),
    /// The site was reset between the request's lookup and its
    /// arrival at the gateway.
    SiteReset,
}

impl fmt::Display for AugmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "{m}"),
            Self::Unsupported => write!(f, "stores no augmentation on this axis"),
            Self::Disjoint(env) => write!(
                f,
                "augmentation is disjoint from the component's current envelope {env}; \
                 no valid setpoint would remain"
            ),
            Self::NotFound(id) => write!(f, "component {id} not found"),
            Self::SiteReset => write!(f, "{}", crate::sim::gateway::SITE_RESET),
        }
    }
}

/// Per-tick snapshot a component emits for the gRPC telemetry
/// stream and the UI's history sampler. All numeric fields are
/// SI units (W, VAR, V, A, %, Wh).
///
/// Optional fields stay `None` for component types that do not expose
/// them — a meter has no SoC; a battery has no AC voltage; etc.
#[derive(Debug, Default, Clone)]
pub struct Telemetry {
    pub id: u64,
    pub category: Option<Category>,

    pub active_power_w: Option<f32>,
    pub reactive_power_var: Option<f32>,

    pub per_phase_active_w: Option<(f32, f32, f32)>,
    pub per_phase_reactive_var: Option<(f32, f32, f32)>,
    pub per_phase_voltage_v: Option<(f32, f32, f32)>,
    pub per_phase_current_a: Option<(f32, f32, f32)>,

    pub frequency_hz: Option<f32>,

    pub soc_pct: Option<f32>,
    /// Steam pressure (bar); steam boilers only.
    pub pressure_bar: Option<f32>,
    pub soc_lower_pct: Option<f32>,
    pub soc_upper_pct: Option<f32>,
    pub capacity_wh: Option<f32>,
    pub dc_voltage_v: Option<f32>,
    pub dc_current_a: Option<f32>,
    pub dc_power_w: Option<f32>,

    pub active_power_bounds: Option<VecBounds>,
    /// Live reactive-power envelope at the current P — caps band ∩
    /// live Q augmentations, possibly multi-band. Overlaid from the
    /// gateway by `MicrogridSite::telemetry_of`.
    pub reactive_power_bounds: Option<VecBounds>,

    pub component_state: Option<&'static str>,
    pub relay_state: Option<&'static str>,
    /// One state code per cable end: a plugged car is locked at the
    /// station and at the car.
    pub cable_states: &'static [&'static str],
}

impl Telemetry {
    /// Read a single metric off this snapshot. `None` when the
    /// component doesn't publish it. The active-bounds metrics
    /// report the *envelope extremes* — first segment's lower, last
    /// segment's upper — unlike the chart history, which collapses a
    /// multi-segment `VecBounds` to its first segment: an assertion
    /// against "the upper bound" means the outermost reachable
    /// value, not the edge of an arbitrary inner segment.
    ///
    /// The reactive-bounds metrics follow the same rule.
    pub fn metric_value(&self, metric: crate::sim::history::Metric) -> Option<f32> {
        use crate::sim::history::Metric;
        match metric {
            Metric::ActivePowerW => self.active_power_w,
            Metric::ReactivePowerVar => self.reactive_power_var,
            Metric::FrequencyHz => self.frequency_hz,
            Metric::SocPct => self.soc_pct,
            Metric::PressureBar => self.pressure_bar,
            Metric::DcPowerW => self.dc_power_w,
            // Cumulative — integrated on the physics tick and held in the
            // site's per-component energy accumulator, not on the
            // instantaneous snapshot. Read it via
            // `MicrogridSite::component_energy_wh`.
            Metric::EnergyWh => None,
            Metric::ActivePowerLowerBoundW => self
                .active_power_bounds
                .as_ref()
                .and_then(|b| b.0.first())
                .and_then(|b| b.lower),
            Metric::ActivePowerUpperBoundW => self
                .active_power_bounds
                .as_ref()
                .and_then(|b| b.0.last())
                .and_then(|b| b.upper),
            Metric::ReactivePowerLowerBoundVar => self
                .reactive_power_bounds
                .as_ref()
                .and_then(|b| b.0.first())
                .and_then(|b| b.lower),
            Metric::ReactivePowerUpperBoundVar => self
                .reactive_power_bounds
                .as_ref()
                .and_then(|b| b.0.last())
                .and_then(|b| b.upper),
        }
    }
}

/// A single-axis knob read-back: the live resolved value plus, for a
/// dynamic (lambda / symbol) source, the printed Lisp expression
/// driving it. `expr` is `None` for a plain constant — see
/// `DynamicScalar::source_text`, which this wraps. Cheap and
/// lock-safe by construction: the source string is captured once at
/// construction time, so reading it here never touches the
/// interpreter.
#[derive(Clone, Debug)]
pub struct ScalarReading {
    pub value: f32,
    pub expr: Option<String>,
}

/// A meter's reactive-power knob read-back — the Q twin of
/// `ScalarReading`, widened to also cover the power-factor
/// derivation shape (`ReactiveSource::PowerFactor` has no scalar
/// source of its own to read back, just the pf/leading pair it was
/// configured with).
#[derive(Clone, Debug)]
pub enum ReactiveReading {
    Var(ScalarReading),
    PowerFactor { pf: f32, leading: bool },
}

/// Which driven knob a scenario snapshot call targets — the
/// vocabulary [`SimulatedComponent::snapshot_knob`] keys on, the key
/// type `MicrogridSite`'s per-scenario baseline map indexes by
/// alongside the component id, and what teardown dispatches its
/// `KnobChanged` broadcast on. Restore needs no `KnobKind`: a
/// [`KnobSnapshot`] already names its own knob by variant.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum KnobKind {
    MeterPower,
    MeterReactive,
    Sunlight,
    BoilerDemand,
    /// EV charger plug state (the connected car, or none).
    Ev,
}

/// A captured knob value, ready to be written straight back into a
/// component's slot(s) by [`SimulatedComponent::restore_knob`].
/// Holds the live Rust objects (`DynamicScalar` / `ReactiveSource`),
/// not their printed text: a lambda's closed-over state and a
/// symbol's live binding can't be reconstructed by re-parsing a
/// string, and restoring one has to hand back the exact object that
/// was installed before the scenario touched it.
///
/// The meter variants carry BOTH the live source and the
/// construction-time kwarg, captured as one paired snapshot: restore
/// must write both, or a scenario-era `clear_active_power_source` /
/// `clear_reactive_power_source` (which also drops the construction
/// kwarg — "clear means cleared") would permanently lose the
/// `:power` / `:reactive-power` a component was originally built
/// with.
pub enum KnobSnapshot {
    /// Solar inverter sunlight %: the source slot is never optional
    /// on this component (it's always at least a constant), so no
    /// `Option` here — but the payload carries the whole
    /// [`SunlightSource`], `Follow` included, not just the driven
    /// scalar. That is what lets restore land the exact pre-scenario
    /// state: an inverter that was tracking the weather goes back to
    /// tracking the weather, rather than being frozen at whatever
    /// constant the sky happened to be at snapshot time. Distinct
    /// from [`Self::BoilerDemand`] so the type — not a `kind`
    /// argument travelling alongside it — is what keeps a boiler's
    /// snapshot from being written into an inverter's slot.
    Sunlight(SunlightSource),
    /// Steam boiler steam demand (kg/h): same shape as
    /// [`Self::Sunlight`], different knob.
    BoilerDemand(DynamicScalar),
    /// Meter active-power axis: the live override (`None` when the
    /// meter was measuring its children) plus the `:power` kwarg it
    /// was constructed with, if any.
    MeterActive {
        source: Option<DynamicScalar>,
        constructed: Option<f32>,
    },
    /// Meter reactive-power axis: the live override — `Var` or
    /// `PowerFactor`, or `None` when the meter was measuring — plus
    /// the `:reactive-power` / `:power-factor` kwarg it was
    /// constructed with, if any.
    MeterReactive {
        source: Option<ReactiveSource>,
        constructed: Option<ConstructedReactive>,
    },
    /// EV charger plug state: the car that was connected (a copy) or
    /// `None` for an empty charger, so restore puts the exact car back
    /// or unplugs whatever a scenario plugged.
    Ev(Option<crate::sim::ev_presets::ConnectedEv>),
}

/// The single trait every simulated component implements.
///
/// Reading order:
///   - **Identity**: id, category, name, subtype, is_hidden.
///   - **Lifecycle**: stream_interval, stream_jitter_pct, tick, telemetry.
///   - **Capability groups**: controllable ([`Controllable`]). Each
///     accessor answers `Some` on the components that have the group.
///   - **Hardware facts the gateway reads**: soc_window, soc_pct. The
///     Microgrid API rules themselves live in `sim::gateway`, never
///     here.
///   - **Stimuli**: set_active_power_override, set_soc_pct, plug_ev,
///     …
///   - **Bounds**: rated_active_bounds, reactive_capability,
///     rated_fuse_current.
///   - **Aggregation** (parent → child): aggregate_power_w,
///     aggregate_reactive_var.
///   - **Inverter → child wiring**: set_dc_power (active only — Q
///     terminates at the inverter and never reaches a DC-side child).
///   - **Runtime knobs**: set_reactive_pf_limit, set_reactive_apparent_va.
///
/// Every method except the required ones (`id`, `category`, `name`,
/// `stream_interval`, `tick`, `telemetry`, `make_fn`,
/// `constructor_kwargs`) has a default — components implement only
/// the surface they need.
pub trait SimulatedComponent: Send + Sync + fmt::Display {
    // ── identity ─────────────────────────────────────────────────────

    fn id(&self) -> u64;
    fn category(&self) -> Category;
    fn name(&self) -> &str;

    /// Free-form subtype label (e.g. `"solar"`, `"li-ion"`, `"ac"`).
    /// Drives the `InverterType` / `BatteryType` / `EvChargerType`
    /// proto enums in `make_component_proto`. Free-form so the trait
    /// doesn't depend on proto types — `proto_conv` matches on known
    /// strings and falls back to "unspecified".
    fn subtype(&self) -> Option<&'static str> {
        None
    }

    /// Hidden components are still registered (so a parent meter can
    /// look them up and aggregate their power) but excluded from the
    /// gRPC `ListElectricalComponents` / `ListConnections` responses
    /// and from `macroctl tree`. Used for synthetic load / generator
    /// meters that should appear as a power flow without being a
    /// discrete addressable component.
    fn is_hidden(&self) -> bool {
        false
    }

    // ── lifecycle ────────────────────────────────────────────────────

    /// Telemetry stream interval requested by the component. The
    /// physics tick may run more often; gRPC streams sample at this
    /// cadence (subject to `stream_jitter_pct`).
    fn stream_interval(&self) -> Duration;

    /// Per-emit jitter applied to the stream interval, in percent
    /// (0..100). Each subscriber's task picks a uniform random
    /// multiplier in `1.0 ± pct/100` for every sleep so multi-stream
    /// clients see streams drifting independently. Default 0.
    fn stream_jitter_pct(&self) -> f32 {
        0.0
    }

    /// Refresh externally-driven inputs from Lisp. The MicrogridSite
    /// scheduler holds the interpreter lock and calls this on every
    /// component, in registration order, *before* the tick pass.
    /// Components carrying a [`DynamicScalar`] (lambda- or symbol-
    /// bound `:power`, `:sunlight%`, …) re-evaluate it here and
    /// stash the resolved scalar in an atomic that `tick` then reads.
    /// Default no-op.
    ///
    /// Must not register defuns or otherwise mutate global state —
    /// the lock is held for every component in turn and the loop's
    /// total cost is bounded by the slowest implementor.
    ///
    /// [`DynamicScalar`]: crate::sim::dynamic_scalar::DynamicScalar
    fn refresh_inputs(&self, _ctx: &mut TulispContext) {}

    /// Advance internal state by `dt`. Called once per physics tick
    /// from `MicrogridSite::tick_once` in registration order (children before
    /// parents). Components that aggregate from successors read them
    /// here via `site.get(child_id)`. Must not call back into the
    /// Lisp interpreter — see [`Self::refresh_inputs`] for that.
    fn tick(&self, site: &MicrogridSite, now: DateTime<Utc>, dt: Duration);

    /// Snapshot the component's observable state for streaming. Pure
    /// — should not mutate. `site` is for components that read AC
    /// environment (per-phase voltage, frequency) at sample time.
    fn telemetry(&self, site: &MicrogridSite) -> Telemetry;

    /// The AC active power `telemetry()` would report, without
    /// building the full snapshot — the per-tick energy integrator
    /// needs only this field, at 10 Hz, for every component. The
    /// default derives it from `telemetry()`; overrides must read
    /// the same source their `telemetry()` reads.
    fn active_power_w(&self, site: &MicrogridSite) -> Option<f32> {
        self.telemetry(site).active_power_w
    }

    // ── capability groups ────────────────────────────────────────────

    /// The gateway-driven side of this component: `Some` when it
    /// takes a command on at least one axis. The gateway owns axes
    /// only for a component that answers `Some`.
    fn controllable(&self) -> Option<&dyn Controllable> {
        None
    }

    // ── hardware facts the gateway reads ─────────────────────────────

    /// A battery's usable SoC window (`:soc-lower`, `:soc-upper`,
    /// `:soc-protect-margin`); `None` for everything else.
    fn soc_window(&self) -> Option<crate::sim::decay::SocProtect> {
        None
    }

    /// A battery's state of charge in %; `None` for everything else.
    fn soc_pct(&self) -> Option<f32> {
        None
    }

    /// Override the active-power value a meter publishes with a
    /// constant. Used by `(set-meter-power id W)` when called with a
    /// numeric argument. Returns whether the component supports the
    /// stimulus (the typed control API rejects a `false`); the default
    /// is an unsupported no-op.
    fn set_active_power_override(&self, _p: f32) -> bool {
        false
    }

    /// Whether [`Self::set_active_power_override`] applies to this
    /// component. The typed control API checks every field of a drive
    /// request with these predicates before applying any of them, so a
    /// rejected request changes nothing.
    fn takes_active_power_override(&self) -> bool {
        false
    }

    /// Teleport a battery's state of charge to `pct` (clamped to
    /// 0..=100). Lets a test arrange a precondition (a nearly-empty or
    /// nearly-full pool) without simulating hours of charging. Returns
    /// whether the component carries charge (the typed control API
    /// rejects a `false`); the default is an unsupported no-op.
    fn set_soc_pct(&self, _pct: f32) -> bool {
        false
    }

    /// Whether [`Self::set_soc_pct`] applies to this component. See
    /// [`Self::takes_active_power_override`] for why the predicates
    /// exist.
    fn takes_soc_pct(&self) -> bool {
        false
    }

    /// Whether this component is an EV charger a car can be plugged
    /// into. The strict doors (`plug-ev`, the EV HTTP route) check it.
    fn takes_ev(&self) -> bool {
        false
    }

    /// Plug `ev` in. Errors when a car is already connected or the
    /// component takes no EV.
    fn plug_ev(&self, _ev: crate::sim::ev_presets::ConnectedEv) -> Result<(), String> {
        Err("this component takes no EV".to_string())
    }

    /// Unplug the connected car. `false` when there was none (or the
    /// component takes no EV).
    fn unplug_ev(&self) -> bool {
        false
    }

    /// The connected car and what the charger is doing with it, or
    /// `None` for an empty charger / a non-charger.
    fn ev_info(&self) -> Option<crate::sim::ev_presets::EvInfo> {
        None
    }

    /// Steam boiler: overwrite the pressure state (bar). `false`
    /// for components without a pressure notion.
    fn set_pressure_bar(&self, _bar: f32) -> bool {
        false
    }
    fn takes_pressure_bar(&self) -> bool {
        false
    }

    /// Replace the meter's `:power` source with a Lisp expression
    /// that the scheduler's `refresh_inputs` pass re-resolves each
    /// tick. Used by `(set-meter-power id (lambda () …))` and by
    /// the UI when a user types a Lisp form into the `:power` input.
    /// Default no-op for non-meter components.
    fn set_active_power_source(&self, _scalar: DynamicScalar) {}

    /// Drop the meter's active-power override, returning it to
    /// measuring its children's aggregate — the one-way trip
    /// `set_active_power_override` / `set_active_power_source` never
    /// had a way back from. Also drops the construction-time
    /// `:power` kwarg for this axis so a save/reload agrees with the
    /// now-measuring live state instead of resurrecting the cleared
    /// override. Returns whether the component supports the
    /// stimulus (a meter always does, even with nothing to clear);
    /// the default is an unsupported no-op.
    fn clear_active_power_source(&self) -> bool {
        false
    }

    /// Override the reactive-power value a meter publishes with a
    /// constant. The Q twin of [`Self::set_active_power_override`].
    /// Returns whether the component supports the stimulus; the
    /// default is an unsupported no-op.
    fn set_reactive_power_override(&self, _vars: f32) -> bool {
        false
    }

    /// Whether [`Self::set_reactive_power_override`] applies to this
    /// component. See [`Self::takes_active_power_override`] for why
    /// the predicates exist.
    fn takes_reactive_power_override(&self) -> bool {
        false
    }

    /// Replace the meter's reactive-power source with a Lisp
    /// expression re-resolved each tick. The Q twin of
    /// [`Self::set_active_power_source`]. Default no-op for
    /// non-meter components.
    fn set_reactive_power_source(&self, _scalar: DynamicScalar) {}

    /// Replace the meter's reactive-power source with a power-factor
    /// derivation that tracks the meter's own live active power.
    /// Returns whether the component supports the stimulus; the
    /// default is an unsupported no-op.
    ///
    /// This trait door does NOT validate `pf`: every caller must
    /// enforce `pf ∈ (0.0, 1.0]` itself before calling (both
    /// `set-meter-power-factor` in Lisp and the HTTP drive op already
    /// do), because an out-of-range factor lands silently otherwise.
    fn set_power_factor(&self, _pf: f32, _leading: bool) -> bool {
        false
    }

    /// Drop the meter's reactive-power override — whichever of
    /// `Var` / `PowerFactor` is currently set — returning it to
    /// summing its children's Q. The Q twin of
    /// `clear_active_power_source`: same "clear means cleared" rule,
    /// dropping the construction-time `:reactive-power` /
    /// `:power-factor` kwarg for this axis too. Returns whether the
    /// component supports the stimulus; the default is an
    /// unsupported no-op.
    fn clear_reactive_power_source(&self) -> bool {
        false
    }

    /// Update the live cloud-cover percentage on a solar inverter.
    /// Used by `(set-solar-sunlight id PCT)` with a numeric
    /// argument. Default no-op for non-solar components.
    /// Returns whether the component models sunlight (the typed
    /// control API rejects a `false`); the default is an unsupported
    /// no-op.
    fn set_sunlight_pct(&self, _pct: f32) -> bool {
        false
    }

    /// Whether [`Self::set_sunlight_pct`] applies to this component.
    /// See [`Self::takes_active_power_override`] for why the
    /// predicates exist.
    fn takes_sunlight_pct(&self) -> bool {
        false
    }

    /// Replace the solar inverter's `:sunlight%` source with a Lisp
    /// expression. PV analogue of [`Self::set_active_power_source`];
    /// used by `(set-solar-sunlight id (lambda () …))`. Default
    /// no-op for non-solar components.
    fn set_sunlight_source(&self, _scalar: DynamicScalar) {}

    /// Drop whatever was driving the solar inverter's `:sunlight%` —
    /// a constant poke or a Lisp expression — and return it to
    /// following the site's weather. The PV analogue of
    /// [`Self::clear_active_power_source`]: the one-way trip
    /// `set_sunlight_pct` / `set_sunlight_source` otherwise has no
    /// way back. Returns whether the component supports the stimulus
    /// (a solar inverter always does, even with nothing to clear);
    /// the default is an unsupported no-op.
    fn clear_sunlight_source(&self) -> bool {
        false
    }

    /// Steam boiler: constant steam demand in kg/h. Collapses any
    /// prior dynamic source, like `set_sunlight_pct`.
    fn set_steam_demand_kg_h(&self, _kg_h: f32) -> bool {
        false
    }
    fn takes_steam_demand(&self) -> bool {
        false
    }
    /// Steam boiler: install a Lisp-driven demand source that
    /// `refresh_inputs` re-resolves each tick.
    fn set_steam_demand_source(&self, _scalar: DynamicScalar) {}

    // ── scenario teardown (snapshot / restore) ───────────────────────

    /// Capture this component's `kind` knob so a later
    /// [`Self::restore_knob`] can put it back exactly as it was.
    /// Called by `MicrogridSite`'s scenario baseline map the first
    /// time a scenario is about to displace a knob it hasn't already
    /// snapshotted — so the very first capture, taken before the
    /// scenario's own drive, is what teardown restores to, no matter
    /// how many times the scenario re-drives the same knob
    /// afterward. `None` when this component has no such knob (the
    /// default) — the caller treats that as "nothing to snapshot",
    /// not an error.
    fn snapshot_knob(&self, _kind: KnobKind) -> Option<KnobSnapshot> {
        None
    }

    /// Write a previously captured [`KnobSnapshot`] straight back
    /// into the component's slot(s). Mechanical, unlike
    /// [`Self::clear_active_power_source`] /
    /// [`Self::clear_reactive_power_source`] — those are user-intent
    /// verbs that also drop the construction-time kwarg, which would
    /// permanently erase a `:power` a scenario merely overrode for
    /// its own duration.
    ///
    /// `snap`'s variant names the knob on its own — there is one per
    /// [`KnobKind`], so a boiler-demand snapshot handed to a solar
    /// inverter simply doesn't match its arm. Returns `false` when
    /// `snap` names nothing this component owns (including "this
    /// component has no such knob at all", the default).
    fn restore_knob(&self, _snap: KnobSnapshot) -> bool {
        false
    }

    // ── bounds telemetry ─────────────────────────────────────────────

    /// Static rated active-power bounds (W). Used by
    /// `ListElectricalComponents` to populate `metric_config_bounds`.
    /// Doesn't change at runtime.
    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        None
    }

    /// The Q axis's capability shape (PF cap, kVA cap, both, or
    /// neither) — the data behind the gateway's live Q envelope
    /// (`site.bounds_of`, via `ReactiveCapability::q_band_at`).
    /// `None` for components with no Q axis at all. "Static" relative
    /// to that envelope means P-independent, not fixed forever: the
    /// caps this returns are the CURRENT runtime-set PF/kVA limits
    /// (mutable via `set-reactive-pf-limit` /
    /// `set-reactive-apparent-va`), not a construction-time
    /// nameplate. `make_component_proto` uses this (via
    /// `ReactiveCapability::hull`) to advertise the reactive config
    /// bound instead of a live-P sample.
    fn reactive_capability(&self) -> Option<crate::sim::reactive::ReactiveCapability> {
        None
    }

    /// Rated fuse current at the grid connection point.
    fn rated_fuse_current(&self) -> Option<u32> {
        None
    }

    // ── knob read-back (inspector snapshot) ──────────────────────────

    /// The meter's active-power source knob, as currently configured
    /// — a live value plus, for a dynamic (lambda / symbol) source,
    /// the printed Lisp expression driving it (`None` for a plain
    /// constant). Distinct from `reactive_capability()`'s PF-limit /
    /// kVA-cap read-back: this is the `:power` input side, not the Q
    /// envelope. `None` for components with no active-power source
    /// knob at all (only `Meter` has one).
    fn meter_power_reading(&self) -> Option<ScalarReading> {
        None
    }

    /// The meter's reactive-power source knob — either a direct VAr
    /// value (mirrors `meter_power_reading`'s shape) or a
    /// power-factor derivation from the meter's own live P. `None`
    /// for components with no reactive source knob configured.
    fn meter_reactive_reading(&self) -> Option<ReactiveReading> {
        None
    }

    /// The PV inverter's cloud-cover knob — always present once a
    /// solar inverter exists (it defaults to a constant), so `None`
    /// here just means "not a solar inverter".
    fn sunlight_reading(&self) -> Option<ScalarReading> {
        None
    }

    /// Resolved demand + source text for the inspector knob.
    fn demand_reading(&self) -> Option<ScalarReading> {
        None
    }
    /// Live pressure for the inspector knob prefill (expr always None).
    fn pressure_reading(&self) -> Option<ScalarReading> {
        None
    }
    /// The boiler's thermostat target, for chart annotation.
    fn pressure_target_bar(&self) -> Option<f32> {
        None
    }

    // ── aggregation (parent reads from child) ────────────────────────

    /// Total real power flowing at this component. Parents (meters,
    /// inverters) sum this across their successors. `site` lets
    /// nesting components recurse — a nested meter calls into its
    /// inverter, which reads from its batteries.
    fn aggregate_power_w(&self, _world: &MicrogridSite) -> f32 {
        0.0
    }

    /// Total reactive power flowing at this component.
    fn aggregate_reactive_var(&self, _world: &MicrogridSite) -> f32 {
        0.0
    }

    // ── inverter → child push (DC bus) ───────────────────────────────

    /// Whether this component sits on a DC bus and takes
    /// [`Self::set_dc_power`] pushes. An inverter only counts
    /// children answering `true` when it splits its commanded power —
    /// a mis-wired AC child (say a meter connected under an inverter
    /// from the UI) must not inflate the divisor and silently swallow
    /// a share the inverter then reports as delivered. Default
    /// `false`; batteries override.
    fn takes_dc_power(&self) -> bool {
        false
    }

    /// Push DC active power onto a child. Inverters call this on each
    /// of their batteries every tick. Default no-op.
    fn set_dc_power(&self, _p: f32) {}

    /// Share of last tick's pushed DC power this child accepted, in
    /// [0, 1]: `accepted / pushed`. A parent multiplies its own push
    /// by this to report what actually flowed, so a battery clipping
    /// at its hardware limits pulls every inverter on its bus down in
    /// proportion. One tick stale by construction: on the tick a
    /// parent changes its push, its report still uses the ratio of
    /// the previous mix. 1.0 for children that never clip (the
    /// default).
    fn dc_accept_ratio(&self) -> f32 {
        1.0
    }

    // ── runtime reactive-capability knobs ────────────────────────────

    /// Replace the PF cap on the reactive envelope at runtime.
    /// `None` disables the PF constraint. Mirrors the SunSpec /
    /// IEEE 1547-2018 PF setpoint surface a real EMS pushes via
    /// Modbus.
    fn set_reactive_pf_limit(&self, _pf: Option<f32>) {}

    /// Replace the apparent-power (kVA) cap on the reactive envelope
    /// at runtime. `None` disables the kVA constraint.
    fn set_reactive_apparent_va(&self, _va: Option<f32>) {}

    // ── microgrid-file rendering ──────────────────────────────────────

    /// The `%make-*` primitive that rebuilds this component on load.
    fn make_fn(&self) -> &'static str;

    /// Does this component carry an input value the generated block
    /// cannot write down? Two shapes qualify: a `:power` /
    /// `:sunlight%` bound to a lambda or symbol, which only means
    /// something while the interpreter is running, and a value poked
    /// in at runtime over a component that was built without that
    /// kwarg — the renderer writes construction arguments, not
    /// pokes, so either way the value is dropped. Adopt warns about
    /// these; they have to be set again from the script section.
    fn has_unrenderable_source(&self) -> bool {
        false
    }

    /// Construction kwargs as lisp-syntax (key, value) pairs, excluding
    /// `:id`, `:name`, `:successors` and runtime-mode kwargs — the
    /// microgrid-file renderer supplies those. Values follow the file
    /// format rules: floats via `lisp_float`, non-finite values omitted,
    /// disabled reactive caps pinned as `0`.
    fn constructor_kwargs(&self) -> Vec<(&'static str, String)>;
}

/// Cloneable handle that we hand to Lisp via `Shared<dyn TulispAny>`.
/// Wrapping in a newtype lets us hang `Display`, `Clone`, conversion
/// trait impls, and a stable `TypeId` off it.
#[derive(Clone)]
pub struct ComponentHandle(pub Arc<dyn SimulatedComponent>);

impl ComponentHandle {
    pub fn new<C: SimulatedComponent + 'static>(c: C) -> Self {
        Self(Arc::new(c))
    }

    pub fn from_arc(c: Arc<dyn SimulatedComponent>) -> Self {
        Self(c)
    }

    pub fn id(&self) -> u64 {
        self.0.id()
    }

    pub fn is_hidden(&self) -> bool {
        self.0.is_hidden()
    }
}

impl fmt::Display for ComponentHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} #{}>", self.0.name(), self.0.id())
    }
}

/// First auto-allocated component ID. Microsim picks 1000 so explicit
/// IDs (1, 2, …) on roots/main-meters don't collide; macrocosim
/// matches the convention so test fixtures stay portable.
pub const FIRST_AUTO_ID: u64 = 1000;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{SimulatedComponent, Telemetry};
    use crate::proto::common::metrics::Bounds;
    use crate::sim::bounds::VecBounds;
    use crate::sim::gateway::test_stubs::{Hw, Pq};
    use crate::sim::history::Metric;
    use crate::sim::{
        Battery, BatteryInverter, Category, EvCharger, Grid, Marker, Meter, SolarInverter,
        SteamBoiler,
    };

    /// Whether a component answers `Some` from one group accessor.
    type Has = fn(&dyn SimulatedComponent) -> bool;

    /// Every capability-group accessor on `SimulatedComponent`, by
    /// group name.
    const GROUPS: &[(&str, Has)] = &[("controllable", |c| c.controllable().is_some())];

    /// The names of the groups `c` has, in `GROUPS` order.
    fn groups_of(c: &dyn SimulatedComponent) -> Vec<&'static str> {
        GROUPS
            .iter()
            .filter(|(_, has)| has(c))
            .map(|(name, _)| *name)
            .collect()
    }

    /// Each component answers `Some` from the accessors of exactly
    /// the groups it implements. A row lists its groups in `GROUPS`
    /// order.
    #[test]
    fn capability_table() {
        let sec = Duration::from_secs(1);
        let rows: Vec<(Box<dyn SimulatedComponent>, &[&str])> = vec![
            (Box::new(Grid::new(1, 0, None, 0.0)), &[]),
            (Box::new(Meter::new(2, sec, None, None, 0.0, false)), &[]),
            (Box::new(Marker::new(3, Category::Chp, 0.0)), &[]),
            (Box::new(Battery::new(4, sec, Default::default())), &[]),
            (
                Box::new(BatteryInverter::new(5, sec, Default::default())),
                &["controllable"],
            ),
            (
                Box::new(SolarInverter::new(6, sec, Default::default())),
                &["controllable"],
            ),
            (
                Box::new(EvCharger::new(7, sec, Default::default())),
                &["controllable"],
            ),
            (
                Box::new(SteamBoiler::new(8, sec, Default::default())),
                &["controllable"],
            ),
            (Box::new(Hw::new(9)), &["controllable"]),
            (Box::new(Pq::new()), &["controllable"]),
        ];
        for (c, want) in &rows {
            assert_eq!(groups_of(c.as_ref()), *want, "{c}");
        }
    }

    /// Bounds metrics read the envelope extremes: a two-segment
    /// VecBounds (disjoint augmentation) reports the first segment's
    /// lower and the LAST segment's upper, not the first's.
    #[test]
    fn metric_value_bounds_use_envelope_extremes() {
        let snap = Telemetry {
            active_power_w: Some(2500.0),
            active_power_bounds: Some(VecBounds(vec![
                Bounds {
                    lower: Some(-10000.0),
                    upper: Some(-2000.0),
                },
                Bounds {
                    lower: Some(2000.0),
                    upper: Some(10000.0),
                },
            ])),
            ..Default::default()
        };
        assert_eq!(snap.metric_value(Metric::ActivePowerW), Some(2500.0));
        assert_eq!(
            snap.metric_value(Metric::ActivePowerLowerBoundW),
            Some(-10000.0)
        );
        assert_eq!(
            snap.metric_value(Metric::ActivePowerUpperBoundW),
            Some(10000.0)
        );
        // Unpublished metrics read None.
        assert_eq!(snap.metric_value(Metric::SocPct), None);
        assert_eq!(snap.metric_value(Metric::ReactivePowerLowerBoundVar), None);
    }

    /// Like the active-bounds arms, the reactive-bounds arms report
    /// the envelope extremes — a two-band Q envelope (split by a live
    /// Q augmentation) reports the outermost reachable edges, not one
    /// inner band's.
    #[test]
    fn metric_value_reactive_bounds_report_envelope_extremes() {
        let snap = Telemetry {
            reactive_power_var: Some(500.0),
            reactive_power_bounds: Some(VecBounds(vec![
                Bounds {
                    lower: Some(-2000.0),
                    upper: Some(-500.0),
                },
                Bounds {
                    lower: Some(500.0),
                    upper: Some(2000.0),
                },
            ])),
            ..Default::default()
        };
        assert_eq!(
            snap.metric_value(Metric::ReactivePowerLowerBoundVar),
            Some(-2000.0)
        );
        assert_eq!(
            snap.metric_value(Metric::ReactivePowerUpperBoundVar),
            Some(2000.0)
        );
    }
}
