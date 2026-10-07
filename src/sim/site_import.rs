//! Import of microgrid API site exports: the JSON files a site's
//! `ListElectricalComponents` / `ListElectricalComponentConnections`
//! calls produce. Parsing is ported from component-graph-web's
//! import module, extended to keep the physics the simulator can
//! use — rated power bounds, battery capacity, SoC bounds, and the
//! grid's rated fuse current — instead of discarding them.
//!
//! The import does not build components directly. It emits one
//! `(progn (make-* …) … (connect …) …)` form that the import
//! handler evals against the new microgrid — the exact same path a
//! UI edit takes, so the persist pass regenerates the microgrid's
//! managed file from the site the form just built. That rewritten
//! file is what makes the import durable.
//!
//! Field sources, per the `frequenz-api-common` protos vendored in
//! `submodules/`:
//! - `categorySpecificInfo.inverter.type` → battery vs solar make fn
//! - `categorySpecificInfo.gridConnectionPoint.ratedFuseCurrent`
//!   → `:rated-fuse-current-a`
//! - `metricConfigBounds[METRIC_AC_POWER_ACTIVE]` → `:rated-lower-w` /
//!   `:rated-upper-w` (batteries prefer `METRIC_DC_POWER` — their
//!   rated range is a DC quantity)
//! - `metricConfigBounds[METRIC_BATTERY_CAPACITY]` → `:capacity-wh` (Wh)
//!   — batteries only
//! - `metricConfigBounds[METRIC_BATTERY_SOC_PCT]` → `:soc-lower-pct` /
//!   `:soc-upper-pct` — batteries only; an EV charger's pack belongs to
//!   the car plugged into it, so these are dropped there
//! - `operationalMode` → `:operational-mode` (config; the simulator
//!   derives the runtime knobs from it)
//! - `categorySpecificInfo.battery.type` (chemistry) is dropped: the
//!   simulator's battery carries no chemistry yet.
//!
//! Wind turbines, power transformers and breakers import as marker
//! components (see [`crate::sim::marker`]): present in the topology,
//! no physics. Steam boilers carry their electric rating —
//! `metricConfigBounds[METRIC_AC_POWER_ACTIVE]` → `:rated-lower-w` /
//! `:rated-upper-w`, same as an EV charger.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};

/// The `components.json` shape: `{"electricalComponents": [...]}`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentsFile {
    electrical_components: Vec<ApiComponent>,
}

/// A component id: the protobuf JSON encoding writes uint64 as a
/// string, but re-processed exports often carry plain numbers —
/// accept both.
#[derive(Deserialize)]
#[serde(untagged)]
enum ApiId {
    Number(u64),
    Text(String),
}

impl ApiId {
    fn parse(&self) -> Result<u64, String> {
        match self {
            ApiId::Number(n) => Ok(*n),
            ApiId::Text(t) => t.parse().map_err(|_| format!("bad component id: {t}")),
        }
    }
}

impl std::fmt::Display for ApiId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiId::Number(n) => write!(f, "{n}"),
            ApiId::Text(t) => write!(f, "{t}"),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiComponent {
    id: ApiId,
    #[serde(default)]
    name: Option<String>,
    /// E.g. "ELECTRICAL_COMPONENT_CATEGORY_METER".
    category: String,
    #[serde(default)]
    category_specific_info: Option<CategoryInfo>,
    /// Per-metric configured bounds; carries the rated power range,
    /// battery capacity and SoC range in current exports.
    #[serde(default)]
    metric_config_bounds: Vec<MetricConfigBounds>,
    /// E.g. "ELECTRICAL_COMPONENT_OPERATIONAL_MODE_INACTIVE".
    #[serde(default)]
    operational_mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CategoryInfo {
    #[serde(default)]
    inverter: Option<TypedInfo>,
    #[serde(default)]
    grid_connection_point: Option<GridInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TypedInfo {
    /// E.g. "INVERTER_TYPE_PV".
    #[serde(default)]
    r#type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GridInfo {
    #[serde(default)]
    rated_fuse_current: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetricConfigBounds {
    #[serde(default)]
    metric: Option<String>,
    #[serde(default)]
    config_bounds: Option<Bounds>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bounds {
    #[serde(default)]
    lower: Option<f64>,
    #[serde(default)]
    upper: Option<f64>,
}

/// The `connections.json` shape:
/// `{"electricalComponentConnections": [...]}`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionsFile {
    electrical_component_connections: Vec<ApiConnection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiConnection {
    source_electrical_component_id: ApiId,
    destination_electrical_component_id: ApiId,
}

/// One component lifted out of the export: the make function that
/// builds its simulated twin plus the kwargs the export pins.
#[derive(Debug)]
pub struct ImportedComponent {
    pub id: u64,
    make_fn: &'static str,
    /// Rendered kwarg pairs, e.g. `(":capacity-wh", "40000")`. Values
    /// are already Lisp-syntax; names come from a fixed set.
    kwargs: Vec<(&'static str, String)>,
}

/// A parsed site export, ready to render as overrides-file forms.
#[derive(Debug)]
pub struct SiteImport {
    pub components: Vec<ImportedComponent>,
    pub connections: Vec<(u64, u64)>,
}

/// Strips the first matching prefix, so both the current
/// `ELECTRICAL_COMPONENT_*` tokens and the older `COMPONENT_*` ones
/// are accepted.
fn strip_prefixes<'a>(token: &'a str, prefixes: &[&str]) -> &'a str {
    for prefix in prefixes {
        if let Some(rest) = token.strip_prefix(prefix) {
            return rest;
        }
    }
    token
}

use crate::lisp::escape_lisp_string as esc;
use crate::lisp::lisp_float;

/// The configured bounds for one metric, by its token suffix.
fn bounds_for<'a>(c: &'a ApiComponent, suffix: &str) -> Option<&'a Bounds> {
    c.metric_config_bounds.iter().find_map(|b| {
        let token = b.metric.as_deref()?;
        (strip_prefixes(token, &["METRIC_"]) == suffix)
            .then_some(b.config_bounds.as_ref())
            .flatten()
    })
}

/// The `:rated-lower-w` / `:rated-upper-w` pair from the first metric in
/// `metrics` the export configures bounds for. AC components pass
/// only `AC_POWER_ACTIVE`; a battery prefers `DC_POWER` (its rated
/// range is a DC quantity) with the AC bounds as fallback.
fn rated_kwargs(c: &ApiComponent, out: &mut Vec<(&'static str, String)>, metrics: &[&str]) {
    let Some(b) = metrics.iter().find_map(|m| bounds_for(c, m)) else {
        return;
    };
    if let Some(l) = b.lower {
        out.push((":rated-lower-w", lisp_float(l)));
    }
    if let Some(u) = b.upper {
        out.push((":rated-upper-w", lisp_float(u)));
    }
}

/// The export's operational mode, kept as what it is: a config
/// parameter (`:operational-mode`). The simulator derives the
/// runtime knobs from it — the import never sets those directly.
/// UNSPECIFIED is the default and is omitted. Unknown tokens are an
/// error, not a silent default.
fn mode_kwargs(c: &ApiComponent, out: &mut Vec<(&'static str, String)>) -> Result<(), String> {
    let Some(mode) = c.operational_mode.as_deref() else {
        return Ok(());
    };
    let suffix = strip_prefixes(
        mode,
        &[
            "ELECTRICAL_COMPONENT_OPERATIONAL_MODE_",
            "COMPONENT_OPERATIONAL_MODE_",
        ],
    );
    // The API token is the mode's Lisp symbol in SCREAMING_SNAKE_CASE.
    let mode: crate::sim::OperationalMode = suffix
        .to_lowercase()
        .replace('_', "-")
        .parse()
        .map_err(|_| format!("unknown operational mode: {suffix}"))?;
    if mode == crate::sim::OperationalMode::Unspecified {
        return Ok(());
    }
    out.push((":operational-mode", format!("'{mode}")));
    Ok(())
}

/// Battery-shaped storage kwargs: capacity (Wh) and the SoC range.
/// Batteries and EV chargers both take them.
fn storage_kwargs(c: &ApiComponent, out: &mut Vec<(&'static str, String)>) {
    if let Some(cap) = bounds_for(c, "BATTERY_CAPACITY").and_then(|b| b.upper.or(b.lower)) {
        out.push((":capacity-wh", lisp_float(cap)));
    }
    if let Some(b) = bounds_for(c, "BATTERY_SOC_PCT") {
        if let Some(l) = b.lower {
            out.push((":soc-lower-pct", lisp_float(l)));
        }
        if let Some(u) = b.upper {
            out.push((":soc-upper-pct", lisp_float(u)));
        }
    }
}

/// Lifts one export component into the make function + kwargs of its
/// simulated twin. Unsupported categories are a hard error: silently
/// skipping one would import a topology that looks complete but has
/// holes where the export had components.
fn lift(c: &ApiComponent) -> Result<ImportedComponent, String> {
    let id = c.id.parse()?;
    let suffix = strip_prefixes(
        &c.category,
        &["ELECTRICAL_COMPONENT_CATEGORY_", "COMPONENT_CATEGORY_"],
    );
    let mut kwargs: Vec<(&'static str, String)> = Vec::new();
    if let Some(name) = c.name.as_deref().filter(|n| !n.is_empty()) {
        kwargs.push((":name", format!("\"{}\"", esc(name))));
    }
    let make_fn = match suffix {
        "GRID_CONNECTION_POINT" | "GRID" => {
            if let Some(fuse) = c
                .category_specific_info
                .as_ref()
                .and_then(|i| i.grid_connection_point.as_ref())
                .and_then(|g| g.rated_fuse_current)
            {
                kwargs.push((":rated-fuse-current-a", fuse.to_string()));
            }
            rated_kwargs(c, &mut kwargs, &["AC_POWER_ACTIVE"]);
            "make-grid-connection-point"
        }
        "METER" => "make-meter",
        "INVERTER" => {
            let inverter_type = c
                .category_specific_info
                .as_ref()
                .and_then(|i| i.inverter.as_ref())
                .and_then(|i| i.r#type.as_deref())
                .map(|t| strip_prefixes(t, &["INVERTER_TYPE_"]))
                .unwrap_or("UNSPECIFIED");
            rated_kwargs(c, &mut kwargs, &["AC_POWER_ACTIVE"]);
            match inverter_type {
                "BATTERY" => "make-battery-inverter",
                "PV" | "SOLAR" => "make-solar-inverter",
                other => {
                    // The simulator has to pick a concrete inverter
                    // model; UNSPECIFIED and HYBRID have none.
                    return Err(format!(
                        "component {id}: cannot simulate an inverter of type {other} \
                         (only battery and PV inverters exist here)"
                    ));
                }
            }
        }
        "BATTERY" => {
            storage_kwargs(c, &mut kwargs);
            rated_kwargs(c, &mut kwargs, &["DC_POWER", "AC_POWER_ACTIVE"]);
            "make-battery"
        }
        // No `storage_kwargs`: the charger has no pack of its own —
        // the car plugged into it does — so `%make-ev-charger`
        // rejects `:capacity-wh` / `:soc-lower-pct` / `:soc-upper-pct`, and an
        // export carrying them would abort the import's whole form.
        "EV_CHARGER" => {
            rated_kwargs(c, &mut kwargs, &["AC_POWER_ACTIVE"]);
            "make-ev-charger"
        }
        "STEAM_BOILER" => {
            rated_kwargs(c, &mut kwargs, &["AC_POWER_ACTIVE"]);
            "make-steam-boiler"
        }
        // CHP and the marker categories have no physics of their
        // own: they complete the topology and classify the meters
        // around them; power is set on the neighboring meter.
        "CHP" => "make-chp",
        "WIND_TURBINE" => "make-wind-turbine",
        "POWER_TRANSFORMER" => "make-power-transformer",
        "BREAKER" => "make-breaker",
        other => {
            return Err(format!("component {id}: cannot simulate category {other}"));
        }
    };
    mode_kwargs(c, &mut kwargs).map_err(|e| format!("component {id}: {e}"))?;
    Ok(ImportedComponent {
        id,
        make_fn,
        kwargs,
    })
}

/// Parses the two uploaded files into make/connect material. Errors
/// name the component so a 4000-line export stays debuggable.
pub fn parse(
    components: ComponentsFile,
    connections: Option<ConnectionsFile>,
) -> Result<SiteImport, String> {
    if components.electrical_components.is_empty() {
        return Err("the export has no components".to_string());
    }
    let mut seen_ids = std::collections::BTreeSet::new();
    let components = components
        .electrical_components
        .iter()
        .map(|c| {
            let lifted = lift(c)?;
            if !seen_ids.insert(lifted.id) {
                return Err(format!("component id {} appears twice", lifted.id));
            }
            Ok(lifted)
        })
        .collect::<Result<Vec<_>, String>>()?;
    // Exports sometimes repeat a connection row; keep the first copy.
    let mut edges = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for c in connections
        .map(|f| f.electrical_component_connections)
        .unwrap_or_default()
    {
        let edge = (
            c.source_electrical_component_id.parse()?,
            c.destination_electrical_component_id.parse()?,
        );
        if !seen_ids.contains(&edge.0) || !seen_ids.contains(&edge.1) {
            return Err(format!(
                "connection {} → {} references a component the export does not carry",
                edge.0, edge.1
            ));
        }
        if seen.insert(edge) {
            edges.push(edge);
        }
    }
    // Cycles must fail the whole request here: the site-side `connect`
    // refuses them one edge at a time, which would abort the import
    // mid-eval with the microgrid already created and half-built.
    for &(src, dst) in &edges {
        if src == dst {
            return Err(format!("connection {src} → {dst} is a self-edge"));
        }
    }
    if let Some((src, dst)) = cycle_edge(&edges) {
        return Err(format!(
            "the connections contain a cycle (reachable through {src} → {dst})"
        ));
    }
    Ok(SiteImport {
        components,
        connections: edges,
    })
}

/// Kahn topological sort over the connection edges; on a cycle,
/// returns an edge between two of the unresolvable nodes so the
/// error can name where to look.
fn cycle_edge(edges: &[(u64, u64)]) -> Option<(u64, u64)> {
    use std::collections::{BTreeMap, VecDeque};
    let mut indegree: BTreeMap<u64, usize> = BTreeMap::new();
    let mut out: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for &(src, dst) in edges {
        indegree.entry(src).or_default();
        *indegree.entry(dst).or_default() += 1;
        out.entry(src).or_default().push(dst);
    }
    let mut queue: VecDeque<u64> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    while let Some(n) = queue.pop_front() {
        for &m in out.get(&n).into_iter().flatten() {
            let d = indegree.get_mut(&m).expect("edge target has an indegree");
            *d -= 1;
            if *d == 0 {
                queue.push_back(m);
            }
        }
        indegree.remove(&n);
    }
    edges
        .iter()
        .copied()
        .find(|(s, d)| indegree.contains_key(s) && indegree.contains_key(d))
}

/// Depth-first post-order visit over the parent → child edges: every
/// child of `id` is emitted before `id` itself. `seen` stops a
/// component reached from more than one parent, or through more than
/// one path, from being pushed twice — and, because it is also
/// checked on entry, from being *descended into* twice. That entry
/// check is what keeps the walk linear: a component is marked only
/// once its whole subtree has been emitted, so meeting a marked id
/// again means that subtree is already in `ordered` and re-walking it
/// would only rediscover the same nodes. Without it a chain of
/// diamonds re-walks every shared subtree once per path into it,
/// which is exponential in the chain's length (see
/// `forms_walk_shared_subtrees_once`). `parse` has already rejected
/// cycles (see `import_rejects_self_edges_and_cycles`), so the entry
/// check is a performance guard, not the recursion's base case.
///
/// `calls` counts every entry, including the ones the memo turns
/// straight back — that count is what the test asserts on, since the
/// memo changes only the walk's cost, never its output.
fn visit_post_order(
    id: u64,
    children: &HashMap<u64, Vec<u64>>,
    seen: &mut HashSet<u64>,
    ordered: &mut Vec<u64>,
    calls: &mut u64,
) {
    *calls += 1;
    if seen.contains(&id) {
        return;
    }
    if let Some(kids) = children.get(&id) {
        for &child in kids {
            visit_post_order(child, children, seen, ordered, calls);
        }
    }
    if seen.insert(id) {
        ordered.push(id);
    }
}

impl SiteImport {
    /// The highest component id in the import — the enterprise id
    /// allocator must move past it so later auto-assigned ids can't
    /// collide with imported ones.
    pub fn max_id(&self) -> u64 {
        self.components.iter().map(|c| c.id).max().unwrap_or(0)
    }

    /// The order the components must be emitted in, and the number
    /// of `visit_post_order` calls it took to work that out.
    ///
    /// Children before parents, as an authored config registers them:
    /// Lisp evaluates :successors before the surrounding make-*, so an
    /// import must not tick an inverter before the battery it pushes
    /// into. A post-order walk over the parent → child edges gives
    /// that order; cycles were refused at parse time. A component in
    /// no edge is its own whole subtree, so it is emitted at the point
    /// the outer export-order loop reaches it — interleaved with the
    /// connected components, not collected at the end.
    ///
    /// The call count is here for `forms_walk_shared_subtrees_once`.
    /// Losing the entry memo does not change the order this returns,
    /// only how long it takes: on a chain of 25 diamonds the walk
    /// still finishes, in minutes, with exactly the same answer. So a
    /// test that reads the output alone stays green on the regressed
    /// walk, and the count is the one cheap thing that does not.
    fn ordered_ids_counting(&self) -> (Vec<u64>, u64) {
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        for &(from, to) in &self.connections {
            children.entry(from).or_default().push(to);
        }
        let mut ordered = Vec::with_capacity(self.components.len());
        let mut seen = HashSet::new();
        let mut calls = 0;
        for c in &self.components {
            visit_post_order(c.id, &children, &mut seen, &mut ordered, &mut calls);
        }
        (ordered, calls)
    }

    /// Renders one atomic form: `(progn (make-* …) … (connect …) …)`.
    /// Evaluated against the new microgrid through the same eval path
    /// UI edits take, so the persist pass regenerates that
    /// microgrid's managed file around the components this form built
    /// — the form itself is never stored, the resulting structure is.
    pub fn forms(&self) -> String {
        // Children before parents — see `ordered_ids_counting`, which
        // owns the walk and the reasoning behind its order.
        let (ordered_ids, _calls) = self.ordered_ids_counting();
        let by_id: HashMap<u64, &ImportedComponent> =
            self.components.iter().map(|c| (c.id, c)).collect();

        let mut out = String::from("(progn\n");
        for id in &ordered_ids {
            let c = by_id
                .get(id)
                .expect("edge endpoints exist: parse rejects dangling edges");
            out.push_str(&format!("  ({} :id {}", c.make_fn, c.id));
            for (k, v) in &c.kwargs {
                out.push_str(&format!(" {k} {v}"));
            }
            out.push_str(")\n");
        }
        for (from, to) in &self.connections {
            out.push_str(&format!("  (connect {from} {to})\n"));
        }
        out.push(')');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small made-up site in the API export format (generic data
    /// only — real exports are proprietary and must not appear here).
    fn generic_components() -> ComponentsFile {
        serde_json::from_str(
            r#"{
              "electricalComponents": [
                {"id": "1", "name": "grid", "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT",
                 "categorySpecificInfo": {"gridConnectionPoint": {"ratedFuseCurrent": 100}}},
                {"id": "2", "name": "main meter", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"},
                {"id": "3", "name": "inverter A", "category": "ELECTRICAL_COMPONENT_CATEGORY_INVERTER",
                 "categorySpecificInfo": {"inverter": {"type": "INVERTER_TYPE_BATTERY"}},
                 "metricConfigBounds": [
                   {"metric": "METRIC_AC_POWER_ACTIVE", "configBounds": {"lower": -30000, "upper": 30000}}
                 ]},
                {"id": "4", "name": "battery A", "category": "ELECTRICAL_COMPONENT_CATEGORY_BATTERY",
                 "categorySpecificInfo": {"battery": {"type": "BATTERY_TYPE_LI_ION"}},
                 "metricConfigBounds": [
                   {"metric": "METRIC_BATTERY_CAPACITY", "configBounds": {"upper": 40000}},
                   {"metric": "METRIC_BATTERY_SOC_PCT", "configBounds": {"lower": 5, "upper": 95}}
                 ]},
                {"id": "5", "category": "ELECTRICAL_COMPONENT_CATEGORY_INVERTER",
                 "categorySpecificInfo": {"inverter": {"type": "INVERTER_TYPE_PV"}}}
              ]
            }"#,
        )
        .unwrap()
    }

    fn generic_connections() -> ConnectionsFile {
        serde_json::from_str(
            r#"{
              "electricalComponentConnections": [
                {"sourceElectricalComponentId": "1", "destinationElectricalComponentId": "2"},
                {"sourceElectricalComponentId": "2", "destinationElectricalComponentId": "3"},
                {"sourceElectricalComponentId": "3", "destinationElectricalComponentId": "4"},
                {"sourceElectricalComponentId": "2", "destinationElectricalComponentId": "5"}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn import_keeps_capacity_soc_and_rated_bounds() {
        let import = parse(generic_components(), Some(generic_connections())).unwrap();
        assert_eq!(import.components.len(), 5);
        assert_eq!(import.connections.len(), 4);
        assert_eq!(import.max_id(), 5);
        let forms = import.forms();
        assert!(forms.contains(
            "(make-grid-connection-point :id 1 :name \"grid\" :rated-fuse-current-a 100)"
        ));
        assert!(forms.contains(
            "(make-battery-inverter :id 3 :name \"inverter A\" :rated-lower-w -30000.0 :rated-upper-w 30000.0)"
        ));
        assert!(forms.contains(
            "(make-battery :id 4 :name \"battery A\" :capacity-wh 40000.0 :soc-lower-pct 5.0 :soc-upper-pct 95.0)"
        ));
        assert!(forms.contains("(make-solar-inverter :id 5)"));
        assert!(forms.contains("(connect 3 4)"));
    }

    #[test]
    fn import_rejects_unsupported_categories_and_untyped_inverters() {
        let hvac: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_HVAC"}
            ]}"#,
        )
        .unwrap();
        let err = parse(hvac, None).unwrap_err();
        assert!(err.contains("component 1"));
        assert!(err.contains("cannot simulate"));

        let untyped: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "7", "category": "ELECTRICAL_COMPONENT_CATEGORY_INVERTER"}
            ]}"#,
        )
        .unwrap();
        let err = parse(untyped, None).unwrap_err();
        assert!(err.contains("component 7"));
        assert!(err.contains("UNSPECIFIED"));
    }

    #[test]
    fn import_rejects_self_edges_and_cycles() {
        let cyclic: ConnectionsFile = serde_json::from_str(
            r#"{"electricalComponentConnections": [
                {"sourceElectricalComponentId": "1", "destinationElectricalComponentId": "2"},
                {"sourceElectricalComponentId": "2", "destinationElectricalComponentId": "3"},
                {"sourceElectricalComponentId": "3", "destinationElectricalComponentId": "1"}
            ]}"#,
        )
        .unwrap();
        let err = parse(generic_components(), Some(cyclic)).unwrap_err();
        assert!(err.contains("cycle"), "{err}");

        let self_edge: ConnectionsFile = serde_json::from_str(
            r#"{"electricalComponentConnections": [
                {"sourceElectricalComponentId": "2", "destinationElectricalComponentId": "2"}
            ]}"#,
        )
        .unwrap();
        let err = parse(generic_components(), Some(self_edge)).unwrap_err();
        assert!(err.contains("self-edge"), "{err}");
    }

    /// Wind turbines, power transformers and breakers import as
    /// marker components; steam boilers carry their electric rating.
    #[test]
    fn import_lifts_marker_categories() {
        let file: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_WIND_TURBINE"},
                {"id": "2", "category": "ELECTRICAL_COMPONENT_CATEGORY_STEAM_BOILER",
                 "metricConfigBounds": [
                   {"metric": "METRIC_AC_POWER_ACTIVE", "configBounds": {"lower": 0, "upper": 500000}}
                 ]},
                {"id": "3", "category": "ELECTRICAL_COMPONENT_CATEGORY_POWER_TRANSFORMER"},
                {"id": "4", "category": "ELECTRICAL_COMPONENT_CATEGORY_BREAKER"}
            ]}"#,
        )
        .unwrap();
        let forms = parse(file, None).unwrap().forms();
        assert!(forms.contains("(make-wind-turbine :id 1)"));
        assert!(
            forms.contains("(make-steam-boiler :id 2 :rated-lower-w 0.0 :rated-upper-w 500000.0)")
        );
        assert!(forms.contains("(make-power-transformer :id 3)"));
        assert!(forms.contains("(make-breaker :id 4)"));
    }

    /// A battery's rated range is its DC power bounds; the AC bounds
    /// are only a fallback.
    #[test]
    fn import_prefers_dc_power_bounds_for_batteries() {
        let file: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_BATTERY",
                 "metricConfigBounds": [
                   {"metric": "METRIC_AC_POWER_ACTIVE", "configBounds": {"lower": -1, "upper": 1}},
                   {"metric": "METRIC_DC_POWER", "configBounds": {"lower": -50000, "upper": 50000}}
                 ]}
            ]}"#,
        )
        .unwrap();
        let forms = parse(file, None).unwrap().forms();
        assert!(
            forms.contains("(make-battery :id 1 :rated-lower-w -50000.0 :rated-upper-w 50000.0)")
        );
    }

    /// A charger has no pack of its own — the car it charges does —
    /// so `%make-ev-charger` rejects `:capacity-wh` / `:soc-lower-pct` /
    /// `:soc-upper-pct`. An export whose charger carries the battery
    /// bounds (exports do, for chargers that report an SoC) must
    /// therefore drop them rather than emit a form that aborts the
    /// import's whole `(progn …)`.
    #[test]
    fn import_drops_pack_bounds_from_ev_chargers() {
        let file: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_EV_CHARGER",
                 "metricConfigBounds": [
                   {"metric": "METRIC_AC_POWER_ACTIVE", "configBounds": {"lower": 0, "upper": 22000}},
                   {"metric": "METRIC_BATTERY_CAPACITY", "configBounds": {"upper": 40000}},
                   {"metric": "METRIC_BATTERY_SOC_PCT", "configBounds": {"lower": 5, "upper": 95}}
                 ]}
            ]}"#,
        )
        .unwrap();
        let forms = parse(file, None).unwrap().forms();
        assert!(
            forms.contains("(make-ev-charger :id 1 :rated-lower-w 0.0 :rated-upper-w 22000.0)"),
            "{forms}"
        );
        assert!(!forms.contains(":capacity-wh"), "{forms}");
        assert!(!forms.contains(":soc-lower-pct"), "{forms}");
        assert!(!forms.contains(":soc-upper-pct"), "{forms}");
    }

    /// The operational mode is a config parameter and stays one:
    /// it renders as `:operational-mode`, never as runtime knobs.
    #[test]
    fn import_keeps_operational_modes_as_config() {
        let file: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER",
                 "operationalMode": "ELECTRICAL_COMPONENT_OPERATIONAL_MODE_INACTIVE"},
                {"id": "2", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER",
                 "operationalMode": "ELECTRICAL_COMPONENT_OPERATIONAL_MODE_TELEMETRY_ONLY"},
                {"id": "3", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER",
                 "operationalMode": "ELECTRICAL_COMPONENT_OPERATIONAL_MODE_CONTROL_AND_TELEMETRY"},
                {"id": "4", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER",
                 "operationalMode": "ELECTRICAL_COMPONENT_OPERATIONAL_MODE_UNSPECIFIED"}
            ]}"#,
        )
        .unwrap();
        let forms = parse(file, None).unwrap().forms();
        assert!(forms.contains("(make-meter :id 1 :operational-mode 'inactive)"));
        assert!(forms.contains("(make-meter :id 2 :operational-mode 'telemetry-only)"));
        assert!(forms.contains("(make-meter :id 3 :operational-mode 'control-and-telemetry)"));
        assert!(forms.contains("(make-meter :id 4)"));
        assert!(!forms.contains(":telemetry-mode"));
        assert!(!forms.contains(":command-mode"));
    }

    /// The older `COMPONENT_*` token prefixes and plain-number ids
    /// are accepted too.
    #[test]
    fn import_accepts_old_prefixes_and_numeric_ids() {
        let file: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": 1, "category": "COMPONENT_CATEGORY_GRID_CONNECTION_POINT"},
                {"id": 2, "category": "COMPONENT_CATEGORY_METER"}
            ]}"#,
        )
        .unwrap();
        let connections: ConnectionsFile = serde_json::from_str(
            r#"{"electricalComponentConnections": [
                {"sourceElectricalComponentId": 1, "destinationElectricalComponentId": 2},
                {"sourceElectricalComponentId": 1, "destinationElectricalComponentId": 2}
            ]}"#,
        )
        .unwrap();
        let import = parse(file, Some(connections)).unwrap();
        assert_eq!(import.components[0].make_fn, "make-grid-connection-point");
        // The repeated connection row is kept once.
        assert_eq!(import.connections, vec![(1, 2)]);
    }

    #[test]
    fn import_rejects_duplicate_ids_and_dangling_edges() {
        let dup: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"},
                {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"}
            ]}"#,
        )
        .unwrap();
        assert!(parse(dup, None).unwrap_err().contains("appears twice"));

        let dangling: ConnectionsFile = serde_json::from_str(
            r#"{"electricalComponentConnections": [
                {"sourceElectricalComponentId": "1", "destinationElectricalComponentId": "99"}
            ]}"#,
        )
        .unwrap();
        let err = parse(generic_components(), Some(dangling)).unwrap_err();
        assert!(err.contains("99"));
    }

    /// Forms come out children first, as an authored config registers
    /// them: a battery's make precedes its inverter's, whatever the
    /// export order, so the tick order matches.
    #[test]
    fn forms_emit_children_before_parents() {
        let import = parse(generic_components(), Some(generic_connections())).unwrap();
        let out = import.forms();
        // Every component here carries at least one kwarg (a name),
        // so ":id N" is always followed by a space before the next
        // kwarg — that space, not just ":id 4", is the delimiter,
        // since ":id 4" alone would also match ":id 40".
        let pos = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing in {out}"))
        };
        assert!(
            pos(":id 4 ") < pos(":id 3 "),
            "battery 4 before inverter 3:\n{out}"
        );
        assert!(
            pos(":id 3 ") < pos(":id 2 "),
            "inverter 3 before meter 2:\n{out}"
        );
        assert!(
            pos(":id 2 ") < pos(":id 1 "),
            "meter 2 before grid 1:\n{out}"
        );
        // connections still follow every make
        assert!(pos("(connect") > pos(":id 1 "));
    }

    /// A diamond (1→2, 1→3, 2→4, 3→4) plus an edge-less component 5
    /// listed between 2 and 3 in export order: 4 has two parents (2
    /// and 3) and must still be emitted exactly once, and 5 — reached
    /// by no edge at all — keeps its export-order place at the end,
    /// after every edge-connected component.
    #[test]
    fn forms_emit_each_component_once_and_keep_unconnected_ones_in_export_order() {
        let components: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "name": "grid", "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT"},
                {"id": "2", "name": "m2", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"},
                {"id": "5", "name": "m5", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"},
                {"id": "3", "name": "m3", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"},
                {"id": "4", "name": "m4", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"}
            ]}"#,
        )
        .unwrap();
        let connections: ConnectionsFile = serde_json::from_str(
            r#"{"electricalComponentConnections": [
                {"sourceElectricalComponentId": "1", "destinationElectricalComponentId": "2"},
                {"sourceElectricalComponentId": "1", "destinationElectricalComponentId": "3"},
                {"sourceElectricalComponentId": "2", "destinationElectricalComponentId": "4"},
                {"sourceElectricalComponentId": "3", "destinationElectricalComponentId": "4"}
            ]}"#,
        )
        .unwrap();
        let out = parse(components, Some(connections)).unwrap().forms();
        // Every component here also carries a name kwarg, so ":id N "
        // (with the trailing space) is again the safe delimiter.
        let pos = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing in {out}"))
        };
        assert_eq!(
            out.matches(":id 4 ").count(),
            1,
            "component 4 emitted exactly once:\n{out}"
        );
        assert!(pos(":id 4 ") < pos(":id 2 "), "4 before 2:\n{out}");
        assert!(pos(":id 4 ") < pos(":id 3 "), "4 before 3:\n{out}");
        assert!(pos(":id 2 ") < pos(":id 1 "), "2 before 1:\n{out}");
        assert!(pos(":id 3 ") < pos(":id 1 "), "3 before 1:\n{out}");
        assert!(
            pos(":id 1 ") < pos(":id 5 "),
            "unconnected 5 after 1:\n{out}"
        );
    }

    /// 25 chained diamonds: node k fans out to `a_k` and `b_k`, both
    /// of which point at node k+1. Every node below the root is a
    /// shared subtree, so a walk that only marks `seen` on the way
    /// back up re-descends node 25 once per root-to-leaf path — 2^25
    /// of them, minutes of spinning while the import lock is held.
    ///
    /// Counted, not timed. The regressed walk reaches the SAME answer,
    /// just after exponentially many calls — about three minutes on
    /// this fixture — so asserting on the emitted order alone would
    /// pass on it and merely take a while doing so. The assertion
    /// below is on the call count instead: a memoized post-order walk
    /// enters once per component plus once per edge, so
    /// `2 * (nodes + edges)` leaves that room and no room at all for
    /// re-descending a shared subtree.
    #[test]
    fn forms_walk_shared_subtrees_once() {
        const DIAMONDS: u64 = 25;
        let node = |k: u64| 10 + k;
        let fork = |k: u64, side: u64| 100 + 2 * k + side;
        let meter = |id: u64| {
            format!(
                r#"{{"id": "{id}", "name": "m{id}", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"}}"#
            )
        };

        // Export order puts the root first, so the very first walk is
        // the one that has to cross every diamond.
        let mut comps = vec![format!(
            r#"{{"id": "{}", "name": "grid", "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT"}}"#,
            node(0)
        )];
        let mut edges = Vec::new();
        for k in 0..DIAMONDS {
            comps.push(meter(fork(k, 0)));
            comps.push(meter(fork(k, 1)));
            comps.push(meter(node(k + 1)));
            for side in 0..2 {
                for (from, to) in [(node(k), fork(k, side)), (fork(k, side), node(k + 1))] {
                    edges.push(format!(
                        r#"{{"sourceElectricalComponentId": "{from}", "destinationElectricalComponentId": "{to}"}}"#
                    ));
                }
            }
        }
        assert_eq!(comps.len(), 76, "26 chain nodes plus two forks per diamond");

        let components: ComponentsFile = serde_json::from_str(&format!(
            r#"{{"electricalComponents": [{}]}}"#,
            comps.join(",")
        ))
        .unwrap();
        let connections: ConnectionsFile = serde_json::from_str(&format!(
            r#"{{"electricalComponentConnections": [{}]}}"#,
            edges.join(",")
        ))
        .unwrap();

        let import = parse(components, Some(connections)).unwrap();
        let (_, calls) = import.ordered_ids_counting();
        let nodes = import.components.len() as u64;
        let edges = import.connections.len() as u64;
        assert!(
            calls <= 2 * (nodes + edges),
            "the walk entered {calls} times for {nodes} nodes and {edges} edges; \
             a memoized post-order walk enters about {} times",
            nodes + edges
        );

        let out = import.forms();
        // Every component carries a name kwarg, so ":id N " with the
        // trailing space is again the safe delimiter.
        let pos = |needle: &str| {
            out.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing in {out}"))
        };
        let once = |id: u64| {
            assert_eq!(
                out.matches(&format!(":id {id} ")).count(),
                1,
                "component {id} emitted exactly once"
            );
        };
        for k in 0..=DIAMONDS {
            once(node(k));
        }
        for k in 0..DIAMONDS {
            once(fork(k, 0));
            once(fork(k, 1));
            for side in 0..2 {
                assert!(
                    pos(&format!(":id {} ", node(k + 1))) < pos(&format!(":id {} ", fork(k, side))),
                    "shared child {} before its parent {}",
                    node(k + 1),
                    fork(k, side)
                );
            }
        }
    }

    /// Names with quote characters cannot break out of their string
    /// literal in the generated Lisp.
    #[test]
    fn import_escapes_names() {
        let file: ComponentsFile = serde_json::from_str(
            r#"{"electricalComponents": [
                {"id": "1", "name": "we \" like ) quotes", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"}
            ]}"#,
        )
        .unwrap();
        let forms = parse(file, None).unwrap().forms();
        assert!(forms.contains(r#":name "we \" like ) quotes""#));
    }
}
