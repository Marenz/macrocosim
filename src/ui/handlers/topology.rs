//! Topology snapshot endpoints. `/api/topology` (legacy) returns
//! the first registered microgrid — the bootstrap site when none
//! is registered — and `/api/mg/{id}/topology` the per-microgrid
//! view.

use axum::extract::State;
use serde::Serialize;

use crate::lisp::Config;

use super::resolve_site;
use crate::ui::api::{ApiError, Json, Path};

#[derive(Serialize)]
pub(in crate::ui) struct TopologySnapshot {
    components: Vec<ComponentSummary>,
    /// Visible parent → child edges, matching the gRPC
    /// `ListConnections` semantic.
    connections: Vec<(u64, u64)>,
    /// Parent → child edges where the child is hidden. Surfaced
    /// separately so the UI can render them dashed without
    /// polluting the gRPC graph. Aggregator components
    /// (Meter, BatteryInverter) cache their hidden children for
    /// aggregation; we read those here.
    hidden_connections: Vec<(u64, u64)>,
    /// Latest graph-validator outcome. `None` = the graph crate
    /// accepted the topology; `Some(msg)` = it rejected with the
    /// human-readable error string. The pulse-bar graph pill
    /// flips between ✓ and ⚠ on this field.
    graph_status: Option<String>,
}

#[derive(Serialize)]
struct ComponentSummary {
    id: u64,
    name: String,
    /// Lowercase string form of [`Category`] (e.g. "grid", "battery").
    /// Stable wire shape — the UI keys icon / colour selection off it.
    category: &'static str,
    /// Subtype label like "battery" / "pv" for inverters; `None` for
    /// component categories that don't subdivide further.
    subtype: Option<&'static str>,
    hidden: bool,
    /// Current runtime knob settings — Display impls on the enums map
    /// to the same lowercase tokens the corresponding setter defuns
    /// accept, so the UI's dropdowns can round-trip via /api/eval
    /// without a string table.
    health: String,
    telemetry_mode: String,
    command_mode: String,
    /// The config-level declared capability (drives the formula
    /// engine); the runtime modes above depend on it.
    operational_mode: String,
    /// Whether the operational mode streams telemetry. Sent as a
    /// boolean so the UI never re-derives it from the mode string —
    /// Rust stays the single source of that rule.
    provides_telemetry: bool,
    /// Whether the operational mode accepts control commands — the
    /// same single-source rule as `provides_telemetry`.
    accepts_control: bool,
}

pub(in crate::ui) async fn topology(State(config): State<Config>) -> Json<TopologySnapshot> {
    Json(topology_snapshot(&config.legacy_site()))
}

pub(in crate::ui) async fn topology_for_mg(
    State(config): State<Config>,
    Path(mg_id): Path<u64>,
) -> Result<Json<TopologySnapshot>, ApiError> {
    let site = resolve_site(&config, mg_id)?;
    Ok(Json(topology_snapshot(&site)))
}

fn topology_snapshot(site: &crate::sim::MicrogridSite) -> TopologySnapshot {
    let components = site
        .components()
        .iter()
        .map(|c| {
            let runtime = site.runtime_of(c.id());
            let mode = site.operational_mode(c.id());
            ComponentSummary {
                id: c.id(),
                name: site
                    .display_name(c.id())
                    .unwrap_or_else(|| c.name().to_string()),
                category: c.category().as_str(),
                subtype: c.subtype(),
                hidden: c.is_hidden(),
                health: runtime.health.to_string(),
                telemetry_mode: runtime.telemetry.to_string(),
                command_mode: runtime.command.to_string(),
                operational_mode: mode.to_string(),
                provides_telemetry: mode.provides_telemetry(),
                accepts_control: mode.accepts_control(),
            }
        })
        .collect();
    TopologySnapshot {
        components,
        connections: site.connections(),
        hidden_connections: site.hidden_connections(),
        // Computed per request from THIS site — a process-global
        // status can't answer for the microgrid actually being
        // viewed. Sites are small; the graph build is cheap.
        graph_status: crate::sim::graph_adapter::validation_error(site),
    }
}
