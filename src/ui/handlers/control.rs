//! Typed JSON control endpoints — component stimuli without Lisp.
//!
//! `POST /api/mg/{mg}/component/{id}/status` and
//! `POST /api/mg/{mg}/component/{id}/drive` give programmatic clients
//! a structured way to inject faults and drive the environment.
//! Validation errors come back as HTTP 400 with a JSON error body,
//! and an unknown component or microgrid is 404; success is 204 with
//! no body. Eval remains the escape hatch for dynamic (lambda /
//! symbol) drive sources and everything else Lisp.

use axum::http::StatusCode;
use serde::Deserialize;

use crate::sim::component::KnobKind;
use crate::sim::microgrid_site::{MicrogridSite, SocRefusal};
use crate::sim::runtime::{CommandMode, Health, TelemetryMode};
use crate::ui::api::{ApiError, ComponentPath, Json, Mg, Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::ui) struct StatusRequest {
    /// New health state (`ok` / `error` / `standby`), if changing.
    health: Option<String>,
    /// New command-channel mode (`normal` / `timeout` / `error` /
    /// `over-bound`), if changing.
    command_mode: Option<String>,
    /// New telemetry mode (`normal` / `silent` / `closed` /
    /// `error-empty` / `not-found`), if changing.
    telemetry_mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::ui) struct DriveRequest {
    /// Constant active-power override for a meter (watts), if driving.
    power_w: Option<f64>,
    /// Sunlight percentage for a PV inverter, if driving.
    sunlight_pct: Option<f64>,
    /// Teleport a battery's state of charge to this percentage.
    soc_pct: Option<f64>,
    /// Constant reactive-power override for a meter (VArs), if driving.
    reactive_var: Option<f64>,
    /// Hold a meter's reactive power at this power factor (cos phi,
    /// `0.0 < power_factor <= 1.0`), tracking its own live active power.
    power_factor: Option<f64>,
    /// With `power_factor`, capacitive (leading) instead of the
    /// default inductive (lagging). Meaningless without `power_factor`.
    leading: Option<bool>,
    /// Steam demand for a steam boiler (kg/h), if driving.
    steam_demand_kg_h: Option<f64>,
    /// Constant pressure override for a steam boiler (bar), if driving.
    pressure_bar: Option<f64>,
    /// Drop a meter's active-power override, returning it to
    /// measuring its children. Mutually exclusive with `power_w` in
    /// the same request. Absent and `false` mean the same thing —
    /// "don't clear" — so this is a plain bool, not an `Option`.
    #[serde(default)]
    clear_power: bool,
    /// Drop a meter's reactive-power override (Var or PowerFactor),
    /// returning it to summing children's Q. Mutually exclusive with
    /// `reactive_var` / `power_factor` in the same request. Plain
    /// bool for the same reason as `clear_power`.
    #[serde(default)]
    clear_reactive: bool,
    /// Drop whatever is driving a solar inverter's sunlight% (a
    /// poke or a Lisp source), returning it to following the site's
    /// weather. The HTTP twin of `(clear-solar-sunlight id)`.
    /// Mutually exclusive with `sunlight_pct` in the same request.
    /// Plain bool for the same reason as `clear_power`.
    #[serde(default)]
    clear_sunlight: bool,
}

fn apply_status(
    site: &MicrogridSite,
    id: u64,
    req: &StatusRequest,
) -> Result<StatusCode, ApiError> {
    if site.get(id).is_none() {
        return Err(ApiError::no_component(id));
    }
    // Parse and validate everything first, apply after: a request
    // with one bad field changes nothing (no half-applied status).
    let health: Option<Health> = parse_enum(&req.health, "health")?;
    let command: Option<CommandMode> = parse_enum(&req.command_mode, "command_mode")?;
    let telemetry: Option<TelemetryMode> = parse_enum(&req.telemetry_mode, "telemetry_mode")?;
    // The operational mode forbids some knob values (e.g.
    // telemetry=normal on an inactive component). Check before any
    // setter runs, so a rejected request leaves the state untouched.
    let mode = site.operational_mode(id);
    if telemetry == Some(TelemetryMode::Normal) && !mode.provides_telemetry() {
        return Err(ApiError::bad_request(format!(
            "component {id} has operational mode {mode}, which streams no telemetry"
        )));
    }
    if command == Some(CommandMode::Normal) && !mode.accepts_control() {
        return Err(ApiError::bad_request(format!(
            "component {id} has operational mode {mode}, which accepts no commands"
        )));
    }
    // Health wins for an errored device: `health=error` forces the
    // command channel to Error, and an explicit `command_mode=normal`
    // in the same request must not re-open it. The Lisp constructors
    // enforce the same rule (see apply_initial_modes in lisp/make.rs).
    if health == Some(Health::Error) && command == Some(CommandMode::Normal) {
        return Err(ApiError::bad_request(format!(
            "component {id}: health=error forbids command_mode=normal in the same request"
        )));
    }
    // NOTE: the mode checks above and the setters below take no
    // common lock, so a concurrent (set-component-operational-mode
    // ...) eval can still fail a setter after set_health applied.
    // The window is a few instructions wide; closing it needs an
    // atomic multi-knob setter on the site (tracked in todo.org).
    if let Some(h) = health {
        site.set_health(id, h).map_err(ApiError::bad_request)?;
    }
    if let Some(m) = command {
        site.set_command_mode(id, m)
            .map_err(ApiError::bad_request)?;
    }
    if let Some(m) = telemetry {
        site.set_telemetry_mode(id, m)
            .map_err(ApiError::bad_request)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

fn parse_enum<T: std::str::FromStr>(
    value: &Option<String>,
    field: &str,
) -> Result<Option<T>, ApiError> {
    match value {
        None => Ok(None),
        Some(s) => s
            .parse::<T>()
            .map(Some)
            .map_err(|_| ApiError::bad_request(format!("invalid {field}: {s:?}"))),
    }
}

/// The answer to a refused SoC write. A charger with no car gets
/// `no_car_status`: a 400 when validation finds it, a 409 when the
/// car left between validation and the write. A component with no
/// SoC is always a 400.
fn soc_rejection(id: u64, no_car_status: StatusCode, refusal: SocRefusal) -> ApiError {
    match refusal {
        SocRefusal::NoCar => {
            ApiError::new(no_car_status, format!("charger {id} has no EV plugged in"))
        }
        SocRefusal::NoStorage => ApiError::bad_request(format!(
            "component {id} does not take soc_pct (not a battery)"
        )),
    }
}

fn apply_drive(site: &MicrogridSite, id: u64, req: &DriveRequest) -> Result<StatusCode, ApiError> {
    let Some(component) = site.get(id) else {
        return Err(ApiError::no_component(id));
    };
    // Validate every field first, apply after (same contract as
    // apply_status): a request with one inapplicable field changes
    // nothing. An inapplicable stimulus is a 400, never a silent no-op.
    // The meter drive is looked up once: the checks below reject a
    // meter field on a non-meter, and the apply phase drives through
    // this same reference.
    let meter = component.meter_drive();
    if req.power_w.is_some() && meter.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take power_w (not a meter)"
        )));
    }
    // The apply phase writes the sunlight fields through this same
    // reference, so it changes nothing this check did not allow.
    let sunlight = component.sunlight_drive();
    if req.sunlight_pct.is_some() && sunlight.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take sunlight_pct (not a solar inverter)"
        )));
    }
    // A charger takes soc_pct exactly while a car is plugged in —
    // the SoC is the car's.
    if req.soc_pct.is_some()
        && let Some(refusal) = SocRefusal::of(&*component)
    {
        return Err(soc_rejection(id, StatusCode::BAD_REQUEST, refusal));
    }
    // reactive_var and power_factor both drive the meter's Q, through
    // the same `MeterDrive` methods `set-meter-reactive-power` /
    // `set-meter-power-factor` call in Lisp.
    if req.reactive_var.is_some() && meter.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take reactive_var (not a meter)"
        )));
    }
    if req.power_factor.is_some() && meter.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take power_factor (not a meter)"
        )));
    }
    // `reactive_var` and `power_factor` set the same slot, so a request
    // carrying both would apply one and then overwrite it — a silent
    // no-op for the loser. Same mutual exclusion `%make-meter` enforces.
    if req.reactive_var.is_some() && req.power_factor.is_some() {
        return Err(ApiError::bad_request(format!(
            "component {id}: reactive_var and power_factor are mutually \
                 exclusive; send one or the other"
        )));
    }
    // `leading` only means something alongside `power_factor` — same
    // shape as the health/command_mode cross-field check above.
    if req.leading.is_some() && req.power_factor.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id}: leading requires power_factor in the same request"
        )));
    }
    // Like `sunlight`, the apply phase writes the steam fields
    // through this same reference.
    let steam = component.steam_drive();
    if req.steam_demand_kg_h.is_some() && steam.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take steam_demand_kg_h (not a steam boiler)"
        )));
    }
    if req.pressure_bar.is_some() && steam.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take pressure_bar (not a steam boiler)"
        )));
    }
    // Each clear is mutually exclusive with the value it would
    // immediately undo — clearing and setting the same axis in one
    // request is ambiguous, not a defined "set then clear" ordering.
    if req.clear_power && meter.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take clear_power (not a meter)"
        )));
    }
    if req.clear_reactive && meter.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take clear_reactive (not a meter)"
        )));
    }
    if req.clear_sunlight && sunlight.is_none() {
        return Err(ApiError::bad_request(format!(
            "component {id} does not take clear_sunlight (not a solar inverter)"
        )));
    }
    if req.clear_sunlight && req.sunlight_pct.is_some() {
        return Err(ApiError::bad_request(format!(
            "component {id}: clear_sunlight and sunlight_pct are mutually exclusive; \
                 send one or the other"
        )));
    }
    if req.clear_power && req.power_w.is_some() {
        return Err(ApiError::bad_request(format!(
            "component {id}: clear_power and power_w are mutually exclusive; \
                 send one or the other"
        )));
    }
    if req.clear_reactive && (req.reactive_var.is_some() || req.power_factor.is_some()) {
        return Err(ApiError::bad_request(format!(
            "component {id}: clear_reactive and reactive_var/power_factor are \
                 mutually exclusive; send one or the other"
        )));
    }
    // Value sanity, same validate-first contract. The f64→f32 cast
    // turns any JSON number beyond f32 range into ±inf, and the meter
    // override installs whatever it's given — an inf/NaN would poison
    // the energy integrator and every aggregate upstream. Battery /
    // ramp guard their own doors; the meter path has no guard below.
    for (field, v) in [
        ("power_w", req.power_w),
        ("sunlight_pct", req.sunlight_pct),
        ("soc_pct", req.soc_pct),
        ("reactive_var", req.reactive_var),
        ("steam_demand_kg_h", req.steam_demand_kg_h),
        ("pressure_bar", req.pressure_bar),
    ] {
        if let Some(v) = v
            && !(v as f32).is_finite()
        {
            return Err(ApiError::bad_request(format!(
                "{field} must be a finite number, got {v}"
            )));
        }
    }
    // `MeterDrive::set_power_factor` does no range validation of its
    // own — this door and `set-meter-power-factor` in Lisp are the
    // only places that enforce it, before the value reaches the
    // meter.
    if let Some(pf) = req.power_factor
        && !(pf > 0.0 && pf <= 1.0)
    {
        return Err(ApiError::bad_request(format!(
            "power_factor must be in (0.0, 1.0], got {pf}"
        )));
    }
    // Every mutation below is preceded by a `scenario_snapshot_knob`
    // for the knob it touches — the same first-snapshot-wins capture
    // the Lisp setters in `src/lisp/defuns/load_drivers.rs` take, with
    // the same kind mapping. Self-gating (a no-op outside a running
    // scenario), and it is what makes the uniform-restore policy true
    // for EVERY door: a poke through this route during a run is put
    // back by `(scenario-stop)` like any other, and — the case that
    // actually bites — a first-touch poke here can no longer be
    // mistaken for the pre-scenario baseline by a LATER scenario
    // drive of the same knob.
    if req.clear_power
        && let Some(m) = meter
    {
        site.scenario_snapshot_knob(id, KnobKind::MeterPower);
        m.clear_active_power_source();
        site.note_knob_changed(id, "meter-power", None, None, None);
    }
    if req.clear_reactive
        && let Some(m) = meter
    {
        site.scenario_snapshot_knob(id, KnobKind::MeterReactive);
        m.clear_reactive_power_source();
        // Two tokens, one slot: the inspector's power-factor input is
        // a knob of its own ("meter-power-factor"), separate from
        // "meter-reactive-power" — a PowerFactor-shaped clear must
        // blank both or the PF input keeps showing a stale number
        // until the next full snapshot.
        site.note_knob_changed(id, "meter-reactive-power", None, None, None);
        site.note_knob_changed(id, "meter-power-factor", None, None, None);
    }
    if req.clear_sunlight
        && let Some(sun) = sunlight
    {
        site.scenario_snapshot_knob(id, KnobKind::Sunlight);
        sun.clear_sunlight_source();
        // Unlike clear_power/clear_reactive, the cleared slot is not
        // "nothing" — a `Follow` source has a live percentage of its
        // own (the seeded full sun until the first tick resolves the
        // sky), so the inspector gets that value rather than a
        // blanked input. Mirrors `(clear-solar-sunlight)` in
        // load_drivers.rs.
        let now_pct = sun.sunlight_reading().value;
        site.note_knob_changed(
            id,
            "solar-sunlight",
            Some(now_pct),
            Some("weather".into()),
            None,
        );
    }
    // Each setter below emits KnobChanged on the same success path
    // as its Lisp defun (src/lisp/defuns/load_drivers.rs) —
    // `soc_pct` isn't part of the knob vocabulary the inspector
    // reads back, so a SoC write gets no broadcast.
    if let Some(watts) = req.power_w
        && let Some(m) = meter
    {
        site.scenario_snapshot_knob(id, KnobKind::MeterPower);
        m.set_active_power_override(watts as f32);
        site.note_knob_changed(id, "meter-power", Some(watts as f32), None, None);
    }
    if let Some(pct) = req.sunlight_pct
        && let Some(sun) = sunlight
    {
        site.scenario_snapshot_knob(id, KnobKind::Sunlight);
        sun.set_sunlight_pct(pct as f32);
        site.note_knob_changed(id, "solar-sunlight", Some(pct as f32), None, None);
    }
    // Validation already ran the same check, so a refusal here is an
    // unplug that raced this request.
    if let Some(pct) = req.soc_pct
        && let Err(refusal) = site.set_soc_pct(&*component, pct as f32)
    {
        return Err(soc_rejection(id, StatusCode::CONFLICT, refusal));
    }
    if let Some(vars) = req.reactive_var
        && let Some(m) = meter
    {
        site.scenario_snapshot_knob(id, KnobKind::MeterReactive);
        m.set_reactive_power_override(vars as f32);
        site.note_knob_changed(id, "meter-reactive-power", Some(vars as f32), None, None);
    }
    if let Some(pf) = req.power_factor
        && let Some(m) = meter
    {
        site.scenario_snapshot_knob(id, KnobKind::MeterReactive);
        let leading = req.leading.unwrap_or(false);
        m.set_power_factor(pf as f32, leading);
        site.note_knob_changed(
            id,
            "meter-power-factor",
            Some(pf as f32),
            None,
            Some(leading),
        );
    }
    if let Some(kg_h) = req.steam_demand_kg_h
        && let Some(boiler) = steam
    {
        site.scenario_snapshot_knob(id, KnobKind::BoilerDemand);
        boiler.set_steam_demand_kg_h(kg_h as f32);
        site.note_knob_changed(id, "boiler-demand", Some(kg_h as f32), None, None);
    }
    if let Some(bar) = req.pressure_bar
        && let Some(boiler) = steam
    {
        boiler.set_pressure_bar(bar as f32);
        site.note_knob_changed(id, "boiler-pressure", Some(bar as f32), None, None);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::ui) async fn component_status(
    mg: Mg,
    Path(ComponentPath { id }): Path<ComponentPath>,
    Json(req): Json<StatusRequest>,
) -> Result<StatusCode, ApiError> {
    apply_status(&mg.site, id, &req)
}

pub(in crate::ui) async fn component_drive(
    mg: Mg,
    Path(ComponentPath { id }): Path<ComponentPath>,
    Json(req): Json<DriveRequest>,
) -> Result<StatusCode, ApiError> {
    apply_drive(&mg.site, id, &req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::meter::Meter;
    use std::time::Duration;

    /// A drive request with one inapplicable field changes nothing:
    /// the rejection must come before any field is applied.
    #[test]
    fn rejected_drive_applies_nothing() {
        let site = MicrogridSite::new();
        site.register(Meter::new(
            5,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));

        let req = DriveRequest {
            power_w: Some(5_000.0),
            sunlight_pct: None,
            soc_pct: Some(50.0), // not a battery → the whole request rejects
            reactive_var: None,
            power_factor: None,
            leading: None,
            steam_demand_kg_h: None,
            pressure_bar: None,
            clear_power: false,
            clear_reactive: false,
            clear_sunlight: false,
        };
        assert!(apply_drive(&site, 5, &req).is_err());

        // The meter's power override must not have been installed.
        let meter = site.get(5).unwrap();
        assert!(meter.aggregate_power_w(&site).abs() < 1e-6);
    }

    fn register_boiler(site: &MicrogridSite, id: u64) {
        site.register(crate::sim::steam_boiler::SteamBoiler::new(
            id,
            Duration::from_secs(1),
            crate::sim::steam_boiler::SteamBoilerConfig::default(),
        ));
    }

    /// `steam_demand_kg_h` on a steam boiler applies immediately — no
    /// tick needed, `demand_reading` reads the source directly.
    #[test]
    fn drive_accepts_steam_demand_on_boiler() {
        let site = MicrogridSite::new();
        register_boiler(&site, 6);

        let req = DriveRequest {
            power_w: None,
            sunlight_pct: None,
            soc_pct: None,
            reactive_var: None,
            power_factor: None,
            leading: None,
            steam_demand_kg_h: Some(40.0),
            pressure_bar: None,
            clear_power: false,
            clear_reactive: false,
            clear_sunlight: false,
        };
        assert!(apply_drive(&site, 6, &req).is_ok());

        let boiler = site.get(6).unwrap();
        let r = boiler
            .steam_drive()
            .expect("demand reading")
            .demand_reading();
        assert!((r.value - 40.0).abs() < 1e-6, "{}", r.value);
    }

    /// `pressure_bar` on a steam boiler moves the pressure state.
    #[test]
    fn drive_accepts_pressure_on_boiler() {
        let site = MicrogridSite::new();
        register_boiler(&site, 6);

        let req = DriveRequest {
            power_w: None,
            sunlight_pct: None,
            soc_pct: None,
            reactive_var: None,
            power_factor: None,
            leading: None,
            steam_demand_kg_h: None,
            pressure_bar: Some(9.0),
            clear_power: false,
            clear_reactive: false,
            clear_sunlight: false,
        };
        assert!(apply_drive(&site, 6, &req).is_ok());

        let boiler = site.get(6).unwrap();
        let r = boiler
            .steam_drive()
            .expect("pressure reading")
            .pressure_reading();
        assert!((r.value - 9.0).abs() < 1e-6, "{}", r.value);
    }

    /// Either boiler field on a non-boiler component is a 4xx
    /// rejection, not a silent no-op.
    #[test]
    fn drive_rejects_boiler_fields_on_non_boiler() {
        let site = MicrogridSite::new();
        site.register(Meter::new(
            5,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));

        let req = DriveRequest {
            power_w: None,
            sunlight_pct: None,
            soc_pct: None,
            reactive_var: None,
            power_factor: None,
            leading: None,
            steam_demand_kg_h: Some(40.0),
            pressure_bar: None,
            clear_power: false,
            clear_reactive: false,
            clear_sunlight: false,
        };
        assert!(apply_drive(&site, 5, &req).is_err());

        let req = DriveRequest {
            power_w: None,
            sunlight_pct: None,
            soc_pct: None,
            reactive_var: None,
            power_factor: None,
            leading: None,
            steam_demand_kg_h: None,
            pressure_bar: Some(9.0),
            clear_power: false,
            clear_reactive: false,
            clear_sunlight: false,
        };
        assert!(apply_drive(&site, 5, &req).is_err());
    }

    /// A JSON number that's finite as f64 but overflows to infinity
    /// on the f64→f32 cast (see the finiteness array's comment) is
    /// rejected before it ever reaches the boiler's demand source.
    #[test]
    fn drive_rejects_non_finite_steam_demand() {
        let site = MicrogridSite::new();
        register_boiler(&site, 6);

        let req: DriveRequest = serde_json::from_str(r#"{"steam_demand_kg_h": 1e40}"#).unwrap();
        assert!(apply_drive(&site, 6, &req).is_err());
    }

    fn register_meter(site: &MicrogridSite, id: u64) {
        site.register(Meter::new(
            id,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));
    }

    fn register_battery(site: &MicrogridSite, id: u64) {
        site.register(crate::sim::battery::Battery::new(
            id,
            Duration::from_secs(1),
            crate::sim::battery::BatteryConfig::default(),
        ));
    }

    /// `{"clear_power": true}` on an overridden meter succeeds and the
    /// override is gone — `meter_power_reading` reads back `None`.
    #[test]
    fn drive_clear_power_restores_measuring() {
        let site = MicrogridSite::new();
        register_meter(&site, 5);
        let req: DriveRequest = serde_json::from_str(r#"{"power_w": 5000.0}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());
        let meter = site.get(5).unwrap();
        assert!(meter.meter_drive().unwrap().meter_power_reading().is_some());

        let req: DriveRequest = serde_json::from_str(r#"{"clear_power": true}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());
        assert!(meter.meter_drive().unwrap().meter_power_reading().is_none());
    }

    /// `clear_power` and `power_w` in the same request is a 4xx
    /// mutual-exclusion rejection — the whole request applies nothing,
    /// including the pre-existing override.
    #[test]
    fn drive_rejects_clear_power_with_power_w() {
        let site = MicrogridSite::new();
        register_meter(&site, 5);
        let req: DriveRequest = serde_json::from_str(r#"{"power_w": 5000.0}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());

        let req: DriveRequest =
            serde_json::from_str(r#"{"clear_power": true, "power_w": 5.0}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_err());

        // Nothing applied: the prior override survives untouched.
        let meter = site.get(5).unwrap();
        assert!((meter.aggregate_power_w(&site) - 5000.0).abs() < 1e-3);
    }

    /// `clear_power` on a battery (not a meter) is a 4xx rejection,
    /// not a silent no-op.
    #[test]
    fn drive_rejects_clear_power_on_non_meter() {
        let site = MicrogridSite::new();
        register_battery(&site, 4);
        let req: DriveRequest = serde_json::from_str(r#"{"clear_power": true}"#).unwrap();
        assert!(apply_drive(&site, 4, &req).is_err());
    }

    /// On a solar inverter (no meter drive) a request combining
    /// `sunlight_pct` with `clear_power` must reject AND leave
    /// `sunlight_pct` untouched: the meter check runs in the
    /// validation pass, before any setter.
    #[test]
    fn drive_clear_power_on_a_non_meter_leaves_other_fields_untouched() {
        use crate::sim::inverter::solar_inverter::{SolarInverter, SolarInverterConfig};

        let site = MicrogridSite::new();
        let cfg = SolarInverterConfig {
            sunlight_pct: Some(100.0),
            ..Default::default()
        };
        site.register(SolarInverter::new(7, Duration::from_secs(1), cfg));

        let req: DriveRequest =
            serde_json::from_str(r#"{"sunlight_pct": 10.0, "clear_power": true}"#).unwrap();
        assert!(apply_drive(&site, 7, &req).is_err());

        let inv = site.get(7).unwrap();
        let r = inv
            .sunlight_drive()
            .expect("sunlight reading")
            .sunlight_reading();
        assert!((r.value - 100.0).abs() < 1e-6, "{}", r.value);
    }

    /// `clear_reactive` mirrors `clear_power` for the Q axis: restores
    /// measuring, and rejects alongside `reactive_var` / `power_factor`
    /// in the same request.
    #[test]
    fn drive_clear_reactive_restores_measuring() {
        let site = MicrogridSite::new();
        register_meter(&site, 5);
        let req: DriveRequest = serde_json::from_str(r#"{"reactive_var": 500.0}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());
        let meter = site.get(5).unwrap();
        assert!(
            meter
                .meter_drive()
                .unwrap()
                .meter_reactive_reading()
                .is_some()
        );

        let req: DriveRequest = serde_json::from_str(r#"{"clear_reactive": true}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());
        assert!(
            meter
                .meter_drive()
                .unwrap()
                .meter_reactive_reading()
                .is_none()
        );
    }

    /// `clear_reactive` together with `reactive_var` or `power_factor`
    /// is a 4xx mutual-exclusion rejection.
    #[test]
    fn drive_rejects_clear_reactive_with_reactive_fields() {
        let site = MicrogridSite::new();
        register_meter(&site, 5);

        let req: DriveRequest =
            serde_json::from_str(r#"{"clear_reactive": true, "reactive_var": 5.0}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_err());

        let req: DriveRequest =
            serde_json::from_str(r#"{"clear_reactive": true, "power_factor": 0.9}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_err());
    }

    /// `clear_reactive` on a battery (not a meter) is a 4xx rejection —
    /// the Q twin of `drive_rejects_clear_power_on_non_meter`.
    #[test]
    fn drive_rejects_clear_reactive_on_non_meter() {
        let site = MicrogridSite::new();
        register_battery(&site, 4);
        let req: DriveRequest = serde_json::from_str(r#"{"clear_reactive": true}"#).unwrap();
        assert!(apply_drive(&site, 4, &req).is_err());
    }

    /// `clear_reactive` over HTTP broadcasts on BOTH knob tokens, same
    /// as the Lisp `(clear-meter-reactive)` defun — the inspector's PF
    /// input is a separate knob from `meter-reactive-power` and must
    /// not keep showing a stale number after the clear.
    #[test]
    fn drive_clear_reactive_broadcasts_both_knob_tokens() {
        let site = MicrogridSite::new();
        register_meter(&site, 5);
        let req: DriveRequest =
            serde_json::from_str(r#"{"power_factor": 0.8, "leading": true}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());

        let mut rx = site.subscribe_events();
        let req: DriveRequest = serde_json::from_str(r#"{"clear_reactive": true}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());

        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                crate::sim::events::SiteEvent::KnobChanged {
                    id: 5,
                    knob: "meter-reactive-power",
                    value: None,
                    expr: None,
                    ..
                }
            )),
            "no meter-reactive-power KnobChanged on the bus; saw: {seen:?}"
        );
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                crate::sim::events::SiteEvent::KnobChanged {
                    id: 5,
                    knob: "meter-power-factor",
                    value: None,
                    expr: None,
                    ..
                }
            )),
            "no meter-power-factor KnobChanged on the bus; saw: {seen:?}"
        );
    }

    /// A charger whose car leaves between the drive's validation and
    /// its write: `ev_info` still reports a car, then
    /// `set_ev_soc_pct` finds none. Stands in for an unplug racing
    /// the request, which a real charger cannot be made to lose on
    /// cue.
    struct CarLeavesMidRequest;

    impl std::fmt::Display for CarLeavesMidRequest {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("car-leaves-mid-request")
        }
    }

    impl crate::sim::component::SimulatedComponent for CarLeavesMidRequest {
        fn id(&self) -> u64 {
            6
        }
        fn category(&self) -> crate::sim::Category {
            crate::sim::Category::EvCharger
        }
        fn name(&self) -> &str {
            "car-leaves-mid-request"
        }
        fn stream_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn tick(&self, _: &MicrogridSite, _: chrono::DateTime<chrono::Utc>, _: Duration) {}
        fn telemetry(&self, _: &MicrogridSite) -> crate::sim::Telemetry {
            crate::sim::Telemetry::default()
        }
        fn ev_port(&self) -> Option<&dyn crate::sim::component::EvPort> {
            Some(self)
        }
        fn make_fn(&self) -> &'static str {
            "%make-test-stub"
        }
        fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
            Vec::new()
        }
    }

    impl crate::sim::component::EvPort for CarLeavesMidRequest {
        fn plug_ev(&self, _ev: crate::sim::ev_presets::ConnectedEv) -> Result<(), String> {
            Err("test stub".into())
        }
        fn unplug_ev(&self) -> bool {
            false
        }
        fn ev_info(&self) -> Option<crate::sim::ev_presets::EvInfo> {
            Some(crate::sim::ev_presets::EvInfo {
                ev: crate::sim::ev_presets::test_car("sedan", None),
                state: crate::sim::ev_presets::EvDrawState::Paused,
            })
        }
        fn set_ev_soc_pct(&self, _pct: f32) -> bool {
            false
        }
    }

    /// The one rejection the apply phase can still give: the car left
    /// after validation passed, so the SoC write is a 409 naming the
    /// charger.
    #[tokio::test]
    async fn drive_soc_on_a_charger_whose_car_left_is_a_conflict() {
        use axum::response::IntoResponse;
        let site = MicrogridSite::new();
        site.register(CarLeavesMidRequest);
        let req: DriveRequest = serde_json::from_str(r#"{"soc_pct": 50.0}"#).unwrap();
        let Err(err) = apply_drive(&site, 6, &req) else {
            panic!("a SoC write the charger refused must not be a 200");
        };
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "charger 6 has no EV plugged in");
    }

    /// The typed drive route snapshots exactly like the Lisp
    /// setters do, so scenario teardown covers it too: a poke made
    /// through `POST component/{id}/drive` while a scenario runs
    /// is put back at `(scenario-stop)`. Without the snapshot, a poke
    /// on a knob the scenario never touched would survive teardown —
    /// and, worse, a FIRST touch through this door would go on to be
    /// captured as the "pre-scenario" baseline by a later scenario
    /// drive, so stop would restore the poke instead of the meter's
    /// own constructed value.
    #[test]
    fn a_drive_poke_during_a_scenario_is_restored_at_stop() {
        let site = MicrogridSite::new();
        site.register(Meter::new(
            5,
            Duration::from_secs(1),
            Some(crate::sim::dynamic_scalar::DynamicScalar::constant(1234.0)),
            None,
            0.0,
            false,
        ));
        let now = chrono::Utc::now();
        let meter = site.get(5).unwrap();
        assert_eq!(
            meter
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            1234.0
        );

        site.scenario_start("drive-poke".into(), now);
        let req: DriveRequest = serde_json::from_str(r#"{"power_w": 7777.0}"#).unwrap();
        assert!(apply_drive(&site, 5, &req).is_ok());
        assert_eq!(
            meter
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            7777.0
        );

        site.scenario_stop(now, None);
        assert_eq!(
            meter
                .meter_drive()
                .unwrap()
                .meter_power_reading()
                .unwrap()
                .value,
            1234.0,
            "a drive-route poke during a scenario must be restored by its stop"
        );
    }
}
