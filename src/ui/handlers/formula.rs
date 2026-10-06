//! Formula endpoint behind the formula explorer panel.
//!
//! Unlike `/api/mg/{mg}/metrics/formulas` (which reads rendered
//! strings off the loopback client's logical meter), this endpoint
//! builds its own [`ComponentGraph`] straight from the site via
//! [`graph_adapter`], so it works even when the loopback slot is
//! empty, and it can honor per-request engine options. Sites are
//! small; a per-request build is cheap. The endpoint returns just the
//! rendered formula string; parsing and highlighting live client-side
//! in `formula-ast.js`.

use std::collections::BTreeSet;

use frequenz_microgrid_component_graph::{ComponentGraphConfig, ErrorKind, Formula};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::sim::graph_adapter;
use crate::ui::api::{ApiError, Json, Mg, Query};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::ui) struct FormulaQuery {
    metric: String,
    /// Comma-separated component ids for the id-taking metrics.
    #[serde(default)]
    component_ids: Option<String>,
    /// Engine options, mirroring `ComponentGraphConfig`. The UI keeps
    /// these client-side and passes them on every request.
    #[serde(default)]
    prefer_meters: bool,
    #[serde(default)]
    phantom_loads: bool,
    #[serde(default)]
    no_fallback: bool,
    #[serde(default)]
    allow_unconnected: bool,
    #[serde(default)]
    allow_validation_failures: bool,
    #[serde(default)]
    allow_unspecified_inverters: bool,
}

/// GET /api/mg/{mg}/formula?metric=grid[&component_ids=1,2][&prefer_meters=true…]
/// — the rendered formula string.
pub(in crate::ui) async fn formula(
    mg: Mg,
    Query(query): Query<FormulaQuery>,
) -> Result<Json<Value>, ApiError> {
    let site = mg.site;
    // spawn_blocking like the other CPU-bound handlers: on a large
    // imported site the graph validation + formula generation is real
    // CPU work, and running it inline would stall a tokio worker
    // that also serves the WS pump and telemetry forwarders.
    super::blocking(move || formula_body(site, query)).await?
}

fn formula_body(
    site: crate::sim::MicrogridSite,
    query: FormulaQuery,
) -> Result<Json<Value>, ApiError> {
    let graph_config = ComponentGraphConfig::builder()
        .prefer_meters_in_component_formulas(query.prefer_meters)
        .include_phantom_loads_in_consumer_formula(query.phantom_loads)
        .disable_fallback_components(query.no_fallback)
        .allow_unconnected_components(query.allow_unconnected)
        .allow_component_validation_failures(query.allow_validation_failures)
        .allow_unspecified_inverters(query.allow_unspecified_inverters)
        .build();
    let (nodes, edges) = graph_adapter::snapshot(&site);
    if nodes.is_empty() {
        return Err(ApiError::bad_request(
            "The microgrid has no components yet.",
        ));
    }
    let graph = match graph_adapter::build_from_with_config(nodes, edges, graph_config) {
        Ok(graph) => graph,
        Err(e) => {
            return Err(
                ApiError::bad_request(format!("Invalid graph: {e}")).with("kind", kind_of(&e))
            );
        }
    };
    let ids: Option<BTreeSet<u64>> = match query.component_ids.as_deref() {
        None | Some("") => None,
        Some(text) => match parse_ids(text) {
            Ok(set) => Some(set),
            Err(e) => return Err(ApiError::bad_request(e)),
        },
    };
    // The single-component metrics need exactly one id; anything else
    // is an error rather than a silent pick.
    let single_id = match ids.as_ref() {
        Some(set) if set.len() == 1 => set.first().copied(),
        _ => None,
    };
    let formula: Result<Formula, _> = match query.metric.as_str() {
        "grid" => graph.grid_formula(),
        "consumer" => graph.consumer_formula(),
        "producer" => graph.producer_formula(),
        "battery" => graph.battery_formula(ids),
        "pv" => graph.pv_formula(ids),
        "chp" => graph.chp_formula(ids),
        "wind_turbine" => graph.wind_turbine_formula(ids),
        "ev_charger" => graph.ev_charger_formula(ids),
        "steam_boiler" => graph.steam_boiler_formula(ids),
        "grid_coalesce" => graph.grid_coalesce_formula(),
        "battery_ac_coalesce" => graph.battery_ac_coalesce_formula(ids),
        "pv_ac_coalesce" => graph.pv_ac_coalesce_formula(ids),
        metric @ ("component" | "component_ac_coalesce") => match single_id {
            Some(id) if metric == "component" => graph.component_formula(id),
            Some(id) => graph.component_ac_coalesce_formula(id),
            None => {
                return Err(ApiError::bad_request(
                    "Select exactly one component for this metric.",
                ));
            }
        },
        other => {
            return Err(ApiError::bad_request(format!("Unknown metric: {other}")));
        }
    };
    match formula {
        Ok(formula) => Ok(Json(json!({
            "metric": query.metric,
            "formula": formula.to_string(),
        }))),
        Err(e) => Err(ApiError::bad_request(e.to_string()).with("kind", kind_of(&e))),
    }
}

/// Parses a comma-separated id list. A token that is not a number is
/// an error — dropping it silently would flip the result from "these
/// components" to a different selection.
fn parse_ids(ids: &str) -> Result<BTreeSet<u64>, String> {
    let mut set = BTreeSet::new();
    for token in ids.split(',').filter(|s| !s.trim().is_empty()) {
        let id = token
            .trim()
            .parse()
            .map_err(|_| format!("bad component id: {token}"))?;
        set.insert(id);
    }
    if set.is_empty() {
        return Err("no component ids given".to_string());
    }
    Ok(set)
}

/// Machine-readable error kind, so the UI can tell "your selection
/// has no such component" apart from "the graph is broken".
fn kind_of(e: &frequenz_microgrid_component_graph::Error) -> String {
    match e.kind() {
        ErrorKind::ComponentNotFound => "component_not_found",
        ErrorKind::InvalidComponent => "invalid_component",
        ErrorKind::InvalidConnection => "invalid_connection",
        ErrorKind::InvalidGraph => "invalid_graph",
        ErrorKind::ValidationErrors(_) => "validation_errors",
        ErrorKind::Internal => "internal",
        _ => "error",
    }
    .to_string()
}
