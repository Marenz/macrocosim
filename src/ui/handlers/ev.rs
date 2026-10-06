//! `GET /api/mg/{mg}/component/{id}/ev` — the simulator's private
//! view of the car plugged into a charger, for the inspector's EV
//! card and the Python client. Not the gRPC API: that sees only the
//! charger.
//!
//! The JSON keys are the snake_case twins of `ev-info`'s plist keys,
//! plus `plugged`.
//!
//! `presets` — the catalog — rides along whether or not a car is
//! plugged in, so the inspector builds its dropdown from the server's
//! list instead of a copy of it.

use serde::Serialize;

use crate::sim::ev_presets::PRESETS;
use crate::ui::api::{ApiError, ComponentPath, Json, Mg, Path};

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

pub(in crate::ui) async fn ev(
    mg: Mg,
    Path(ComponentPath { id }): Path<ComponentPath>,
) -> Result<Json<EvResponse>, ApiError> {
    let c = mg.site.get(id).ok_or_else(|| ApiError::no_component(id))?;
    let Some(port) = c.ev_port() else {
        return Err(ApiError::bad_request(format!(
            "component {id} is not an EV charger"
        )));
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
            plugged_at: Some(crate::timefmt::rfc3339(i.ev.plugged_at)),
            state: Some(i.state.as_str()),
        },
    }))
}
