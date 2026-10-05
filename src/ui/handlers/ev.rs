//! `GET /api/mg/{mg}/ev/{id}` — the simulator's private view of the
//! car plugged into a charger, for the inspector's EV card and the
//! Python client. Not the gRPC API: that sees only the charger.
//!
//! The JSON keys are the snake_case twins of `ev-info`'s plist keys,
//! plus `plugged`; three of the twins do not line up: `capacity_wh`
//! for `:capacity-kwh` (in watt-hours), `soc_pct` for `:soc`,
//! `target_soc_pct` for `:target-soc`.
//!
//! `presets` — the catalog — rides along whether or not a car is
//! plugged in, so the inspector builds its dropdown from the server's
//! list instead of a copy of it.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Serialize;

use super::resolve_site;
use crate::lisp::Config;
use crate::sim::ev_presets::PRESETS;

#[derive(Serialize, Default)]
pub(in crate::ui) struct EvResponse {
    plugged: bool,
    /// The catalog a `plug-ev` may name, sent plugged or not so the
    /// inspector's preset dropdown never hardcodes it.
    presets: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preset: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    soc_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_soc_pct: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phases: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_current_a: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capacity_wh: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    energy_wh: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugged_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
}

pub(in crate::ui) async fn ev_for_mg(
    State(config): State<Config>,
    Path((mg_id, id)): Path<(u64, u64)>,
) -> Result<Json<EvResponse>, (StatusCode, String)> {
    let site = resolve_site(&config, mg_id)?;
    let c = site
        .get(id)
        .ok_or((StatusCode::NOT_FOUND, format!("component {id} not found")))?;
    let Some(port) = c.ev_port() else {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("component {id} is not an EV charger"),
        ));
    };
    let presets = PRESETS.iter().map(|p| p.name).collect();
    Ok(Json(match port.ev_info() {
        None => EvResponse {
            plugged: false,
            presets,
            ..Default::default()
        },
        Some(i) => EvResponse {
            plugged: true,
            presets,
            preset: Some(i.ev.preset),
            soc_pct: Some(i.ev.soc_pct),
            target_soc_pct: Some(i.ev.target_soc_pct),
            phases: Some(i.ev.phases),
            max_current_a: Some(i.ev.max_current_a),
            capacity_wh: Some(i.ev.capacity_wh),
            energy_wh: Some(i.ev.energy_wh),
            plugged_at: Some(i.ev.plugged_at.to_rfc3339()),
            state: Some(i.state.as_str()),
        },
    }))
}
