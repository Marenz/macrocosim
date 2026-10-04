//! `/api/component` — the inspector's read-back snapshot for one
//! component: its runtime knobs (meter power/reactive/PF, solar
//! sunlight, reactive PF-limit/apparent-VA caps), the last accepted
//! setpoint per axis with its remaining request-lifetime, whether a
//! bounds augmentation is currently narrowing each axis, and the
//! setpoint envelope each axis is gated against. Distinct from
//! `/api/history` (time series) and `/api/setpoints` (the raw event
//! log) — this is "what does the component look like right now".

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::sim::Category;
use crate::sim::component::{ReactiveReading, ScalarReading};
use crate::sim::setpoints::{SetpointKind, SetpointOutcome};
use crate::timeout_tracker::SetpointAxis;

use super::resolve_site;

#[derive(Deserialize)]
pub(in crate::ui) struct ComponentQuery {
    id: u64,
}

#[derive(Serialize)]
pub(in crate::ui) struct ComponentStateResponse {
    id: u64,
    knobs: Vec<KnobState>,
    setpoints: Vec<ActiveSetpoint>,
    augmented: AxisFlags,
    envelope: Envelope,
    /// Thermostat pressure target (bar) for a steam boiler, for chart
    /// annotation. `None` for any other category.
    #[serde(skip_serializing_if = "Option::is_none")]
    pressure_target_bar: Option<f32>,
}

#[derive(Serialize)]
struct KnobState {
    /// Fixed token from the client's knob vocabulary — see
    /// `KNOBS_BY_CATEGORY` in `ui-assets/inspect.js`.
    knob: &'static str,
    /// `None` when the knob exists but has no value configured yet
    /// (an inverter's PF-limit / apparent-VA cap before either is
    /// set) — the client still renders the input, empty.
    value: Option<f32>,
    /// Printed Lisp source for a dynamic (lambda / symbol) reading.
    /// `None` for a plain constant. Shipped unfiltered, including
    /// tulisp's opaque `CompiledDefun` closure Display (an unquoted
    /// lambda literal evaluates to one before it ever reaches the
    /// component) — the client detects that marker and renders a
    /// placeholder instead of the meaningless literal string
    /// "CompiledDefun" (see `knobDisplay` in `ui-assets/inspect.js`),
    /// the same way it already had to for the WS `knob_changed` path.
    expr: Option<String>,
    /// Set only on `meter-power-factor`: whether the derived reactive
    /// power leads (capacitive) rather than lags (inductive).
    leading: Option<bool>,
}

#[derive(Serialize)]
struct ActiveSetpoint {
    axis: &'static str,
    value: f32,
    /// Time left before the request-lifetime timeout resets this
    /// axis, in ms. `None` for a persistent (untracked) setpoint —
    /// nothing in `TimeoutTracker` for this (id, axis).
    remaining_ms: Option<u64>,
}

#[derive(Serialize)]
struct AxisFlags {
    active: bool,
    reactive: bool,
}

#[derive(Serialize)]
struct Envelope {
    active: Option<(f32, f32)>,
    reactive: Option<(f32, f32)>,
}

pub(in crate::ui) async fn component(
    State(config): State<Config>,
    Query(q): Query<ComponentQuery>,
) -> Result<Json<ComponentStateResponse>, (StatusCode, String)> {
    component_state(&config.legacy_site(), q.id).map(Json)
}

pub(in crate::ui) async fn component_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
    Query(q): Query<ComponentQuery>,
) -> Result<Json<ComponentStateResponse>, (StatusCode, String)> {
    let site = resolve_site(&config, mg_id)?;
    component_state(&site, q.id).map(Json)
}

fn knob(
    knob: &'static str,
    value: Option<f32>,
    expr: Option<String>,
    leading: Option<bool>,
) -> KnobState {
    KnobState {
        knob,
        value,
        expr,
        leading,
    }
}

fn scalar_knob(name: &'static str, r: ScalarReading) -> KnobState {
    knob(name, Some(r.value), r.expr, None)
}

/// Knobs the component's category actually has — mirrors the
/// client's `KNOBS_BY_CATEGORY` plus its solar-sunlight rule: meters
/// get the three meter knobs, inverters get the reactive caps, and a
/// solar inverter additionally gets sunlight.
fn knobs_for(c: &dyn crate::sim::SimulatedComponent) -> Vec<KnobState> {
    let mut knobs = Vec::new();
    match c.category() {
        Category::Meter => {
            if let Some(r) = c.meter_power_reading() {
                knobs.push(scalar_knob("meter-power", r));
            }
            match c.meter_reactive_reading() {
                Some(ReactiveReading::Var(r)) => knobs.push(scalar_knob("meter-reactive-power", r)),
                Some(ReactiveReading::PowerFactor { pf, leading }) => {
                    knobs.push(knob("meter-power-factor", Some(pf), None, Some(leading)));
                }
                None => {}
            }
        }
        Category::Inverter => {
            if c.subtype() == Some("solar")
                && let Some(r) = c.sunlight_reading()
            {
                knobs.push(scalar_knob("solar-sunlight", r));
            }
            if let Some(cap) = c.reactive_capability() {
                knobs.push(knob("reactive-pf-limit", cap.pf_limit, None, None));
                knobs.push(knob("reactive-apparent-va", cap.apparent_va, None, None));
            }
        }
        Category::SteamBoiler => {
            if let Some(r) = c.demand_reading() {
                knobs.push(scalar_knob("boiler-demand", r));
            }
            if let Some(r) = c.pressure_reading() {
                knobs.push(scalar_knob("boiler-pressure", r));
            }
        }
        _ => {}
    }
    knobs
}

/// Last ACCEPTED event per axis within the trailing window, paired
/// with its remaining request-lifetime. Augment-bounds events don't
/// count — they narrow the envelope (see `augmented` below), they
/// aren't a power setpoint.
fn setpoints_for(site: &crate::sim::MicrogridSite, id: u64) -> Vec<ActiveSetpoint> {
    let since = Utc::now() - ChronoDuration::minutes(10);
    let events = site.setpoints_window(id, since);
    let mut last_active = None;
    let mut last_reactive = None;
    for ev in &events {
        if !matches!(ev.outcome, SetpointOutcome::Accepted { .. }) {
            continue;
        }
        match ev.kind {
            SetpointKind::ActivePower => last_active = Some(ev),
            SetpointKind::ReactivePower => last_reactive = Some(ev),
            SetpointKind::AugmentBounds | SetpointKind::AugmentReactiveBounds => {}
        }
    }
    [
        (last_active, "active", SetpointAxis::Active),
        (last_reactive, "reactive", SetpointAxis::Reactive),
    ]
    .into_iter()
    .filter_map(|(ev, axis, timeout_axis)| {
        ev.map(|ev| ActiveSetpoint {
            axis,
            value: ev.value,
            remaining_ms: site
                .gateway()
                .remaining_lifetime(id, timeout_axis)
                .map(|d| d.as_millis() as u64),
        })
    })
    .collect()
}

/// `VecBounds`'s envelope extremes — first segment's lower to last
/// segment's upper, matching how `Telemetry::metric_value` reads a
/// multi-segment bounds' outer edges.
fn envelope_tuple(bounds: Option<crate::sim::bounds::VecBounds>) -> Option<(f32, f32)> {
    let bounds = bounds?;
    let lower = bounds.0.first()?.lower?;
    let upper = bounds.0.last()?.upper?;
    Some((lower, upper))
}

fn component_state(
    site: &crate::sim::MicrogridSite,
    id: u64,
) -> Result<ComponentStateResponse, (StatusCode, String)> {
    let c = site
        .get(id)
        .ok_or((StatusCode::NOT_FOUND, format!("component {id} not found")))?;

    Ok(ComponentStateResponse {
        id,
        knobs: knobs_for(c.as_ref()),
        setpoints: setpoints_for(site, id),
        augmented: AxisFlags {
            active: site.gateway().augmented(id, SetpointAxis::Active),
            reactive: site.gateway().augmented(id, SetpointAxis::Reactive),
        },
        envelope: Envelope {
            // The gateway's setpoint envelope: own bounds ∩ the
            // children's, or own bounds alone when no child reports
            // any (the common case for Q, which ends at the inverter).
            active: envelope_tuple(site.gateway().setpoint_envelope(id, SetpointAxis::Active)),
            reactive: envelope_tuple(site.gateway().setpoint_envelope(id, SetpointAxis::Reactive)),
        },
        pressure_target_bar: c.pressure_target_bar(),
    })
}
