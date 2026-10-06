//! HTTP-side integration tests. Each test spawns a fresh
//! `TestServer`, hits the live `/api/*` surface with reqwest, and
//! asserts on the JSON shape — covering the boot path that the
//! in-source `tests` module's axum-oneshot pattern doesn't.

mod common;

use std::time::Duration;

use chrono::DateTime;
use common::{TestServer, eval_or_panic};
use serde_json::Value;

const TINY_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-battery :id 3)))))
"#;

/// A grid → meter → EV charger the two EV tests both drive: id 6 is
/// the charger, id 2 the meter that takes no car.
const EV_TOPOLOGY: &str = "(%make-grid-connection-point :id 1) (%make-meter :id 2) \
                           (%make-ev-charger :id 6) (connect 1 2) (connect 2 6)";

async fn json(client: &reqwest::Client, url: String) -> Value {
    client
        .get(&url)
        .send()
        .await
        .unwrap_or_else(|e| panic!("GET {url}: {e}"))
        .json::<Value>()
        .await
        .unwrap_or_else(|e| panic!("parse {url}: {e}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn topology_endpoint_serves_components_and_connections() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let topo = json(&reqwest::Client::new(), s.mg_url("topology")).await;
    let components = topo["components"].as_array().expect("components array");
    let ids: Vec<i64> = components
        .iter()
        .map(|c| c["id"].as_i64().unwrap())
        .collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    let connections = topo["connections"].as_array().expect("connections array");
    assert_eq!(connections.len(), 2, "expected grid→meter, meter→battery");
}

#[tokio::test(flavor = "multi_thread")]
async fn eval_endpoint_round_trips_world_state() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/api/eval", s.ui_url))
        .body("(rename-component 2 \"main\")")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert!(body["value"].is_string(), "{body}");

    // Read back via the topology route to confirm the world updated.
    let topo = json(&client, s.mg_url("topology")).await;
    let renamed = topo["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == 2)
        .unwrap();
    assert_eq!(renamed["name"], "main");
}

#[tokio::test(flavor = "multi_thread")]
async fn scenario_endpoints_round_trip_via_eval() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    // Pre-start: lifecycle is empty.
    let pre = json(&client, s.mg_url("scenario")).await;
    assert!(pre["name"].is_null());

    // Start + record an event via /api/eval.
    for body in [
        "(scenario-start \"smoke\")",
        "(scenario-event 'note \"hi\")",
    ] {
        let r = client
            .post(format!("{}/api/eval", s.ui_url))
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(
            r.status().is_success(),
            "eval {body} failed: {:?}",
            r.status()
        );
    }

    let summary = json(&client, s.mg_url("scenario")).await;
    assert_eq!(summary["name"], "smoke");
    assert_eq!(summary["event_count"], 1);

    let events = json(&client, s.mg_url("scenario/events")).await;
    let arr = events["events"].as_array().unwrap();
    assert_eq!(arr[0]["kind"], "note");

    // The report is keyed on the grid formula streams now, so it
    // carries no meter id at all — the retired main-meter concept.
    let report = json(&client, s.mg_url("scenario/report")).await;
    assert!(
        report.get("main_meter_id").is_none(),
        "main_meter_id should be retired from the report: {report}"
    );
    assert!(
        report.get("peak_grid_w").is_some(),
        "report should carry the grid peak: {report}"
    );
    for key in ["peak_grid_var", "site_pf_at_reactive_peak"] {
        assert!(
            report.get(key).is_some(),
            "report should carry {key}: {report}"
        );
    }
    assert!(
        report["grid_window_averages"].is_array(),
        "report should carry grid_window_averages as an array: {report}"
    );
}

/// A microgrid created over HTTP gets a managed file, and every
/// structural eval against it rewrites that file — the whole
/// save-on-edit path through the live server.
#[tokio::test(flavor = "multi_thread")]
async fn structural_evals_rewrite_the_managed_microgrid_file() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    let created: Value = client
        .post(format!("{}/api/microgrids", s.ui_url))
        .json(&serde_json::json!({"name": "saved"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_u64().unwrap();
    let path = s.config.state_dir().join(format!("microgrids/{id}.lisp"));
    assert!(path.exists(), "create writes {}", path.display());

    for body in [
        "(%make-grid-connection-point :id 4001)",
        "(rename-component 4001 \"main\")",
    ] {
        client
            .post(format!("{}/api/mg/{id}/eval", s.ui_url))
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("(%make-grid-connection-point :id 4001"),
        "{saved}"
    );
    assert!(saved.contains(":name \"main\""), "{saved}");
    // A poke is not structure: it leaves the file alone.
    let before = std::fs::read_to_string(&path).unwrap();
    client
        .post(format!("{}/api/mg/{id}/eval", s.ui_url))
        .body("(set-component-health 4001 'error)")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(before, std::fs::read_to_string(&path).unwrap());
}

/// The whole imported-site flow over HTTP: import a site export,
/// then ask the new microgrid for its battery formula. The import
/// populates the site through the per-mg eval path, so the formula
/// engine sees the imported topology immediately.
#[tokio::test(flavor = "multi_thread")]
async fn site_import_creates_microgrid_with_working_formulas() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "name": "imported site",
        "components": {"electricalComponents": [
            {"id": "9101", "name": "grid",
             "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT",
             "categorySpecificInfo": {"gridConnectionPoint": {"ratedFuseCurrent": 125}}},
            {"id": "9102", "name": "main meter",
             "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"},
            {"id": "9103", "name": "bat inverter",
             "category": "ELECTRICAL_COMPONENT_CATEGORY_INVERTER",
             "categorySpecificInfo": {"inverter": {"type": "INVERTER_TYPE_BATTERY"}},
             "metricConfigBounds": [
                {"metric": "METRIC_AC_POWER_ACTIVE",
                 "configBounds": {"lower": -30000, "upper": 30000}}]},
            {"id": "9104", "name": "battery",
             "category": "ELECTRICAL_COMPONENT_CATEGORY_BATTERY",
             "metricConfigBounds": [
                {"metric": "METRIC_BATTERY_CAPACITY", "configBounds": {"upper": 40000}},
                {"metric": "METRIC_BATTERY_SOC_PCT",
                 "configBounds": {"lower": 5, "upper": 95}}]}
        ]},
        "connections": {"electricalComponentConnections": [
            {"sourceElectricalComponentId": "9101", "destinationElectricalComponentId": "9102"},
            {"sourceElectricalComponentId": "9102", "destinationElectricalComponentId": "9103"},
            {"sourceElectricalComponentId": "9103", "destinationElectricalComponentId": "9104"}
        ]}
    });
    let resp: serde_json::Value = client
        .post(format!("{}/api/microgrids/import", s.ui_url))
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["components"], 4);
    let id = resp["id"].as_u64().unwrap();
    // No `mid` in the body — the pre-dialog behaviour, kept: the
    // server allocates the lowest free id, one above the bootstrap
    // microgrid this fixture registers.
    assert_eq!(id, 2201, "a no-mid import must still auto-assign: {resp}");

    let topo = json(&client, format!("{}/api/mg/{id}/topology", s.ui_url)).await;
    assert_eq!(topo["components"].as_array().unwrap().len(), 4);

    let formula = json(
        &client,
        format!("{}/api/mg/{id}/formula?metric=battery", s.ui_url),
    )
    .await;
    assert!(formula["formula"].is_string(), "body: {formula}");
    assert!(formula["formula"].as_str().unwrap().contains("#9103"));
    assert!(formula.get("explanation").is_none());

    // The microgrid's file carries the export's physical
    // parameters, so they come back at every boot.
    let saved = std::fs::read_to_string(s.config.state_dir().join(format!("microgrids/{id}.lisp")))
        .unwrap();
    assert!(saved.contains(":capacity-wh 40000.0"), "{saved}");
    assert!(saved.contains(":rated-fuse-current-a 125"), "{saved}");
}

/// A one-component site export — the smallest thing the importer
/// accepts, enough to watch where the new microgrid's id lands.
/// Component ids are enterprise-unique, so every call needs its own.
fn tiny_export(name: &str, mid: Option<u64>, component_id: u64) -> Value {
    let mut body = serde_json::json!({
        "name": name,
        "components": {"electricalComponents": [
            {"id": component_id.to_string(), "name": "grid",
             "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT"}
        ]}
    });
    if let Some(mid) = mid {
        body["id"] = serde_json::json!(mid);
    }
    body
}

/// The import dialog asks the user for the microgrid id, so the
/// import body carries an `id` — and the microgrid registers under
/// exactly that one, not the next free one.
#[tokio::test(flavor = "multi_thread")]
async fn site_import_claims_the_requested_microgrid_id() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    // Well clear of the lowest free id (2201), so landing on it can
    // only be the requested id being honoured.
    let wanted = 2317;
    let resp: Value = client
        .post(format!("{}/api/microgrids/import", s.ui_url))
        .json(&tiny_export("chosen id", Some(wanted), 9201))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["id"], wanted, "{resp}");

    let mgs = json(&client, format!("{}/api/microgrids", s.ui_url)).await;
    let entry = mgs
        .as_array()
        .expect("microgrids array")
        .iter()
        .find(|m| m["id"] == wanted)
        .unwrap_or_else(|| panic!("microgrid {wanted} not registered: {mgs}"));
    assert_eq!(entry["name"], "chosen id", "{entry}");
}

/// A requested id that is already taken is refused, and the refusal
/// names the id so the UI can put it in a toast. Nothing is created:
/// the collision is caught before the file is written.
#[tokio::test(flavor = "multi_thread")]
async fn site_import_refuses_a_taken_microgrid_id() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    let wanted = 2318;
    client
        .post(format!("{}/api/microgrids/import", s.ui_url))
        .json(&tiny_export("first", Some(wanted), 9301))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let resp = client
        .post(format!("{}/api/microgrids/import", s.ui_url))
        .json(&tiny_export("second", Some(wanted), 9302))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CONFLICT,
        "a taken id must be refused"
    );
    let body: Value = resp.json().await.unwrap();
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains(&wanted.to_string()),
        "the refusal must name the id: {error}"
    );

    // The refused import left nothing behind — no second microgrid,
    // and the file for the id it asked for is still the FIRST
    // import's, untouched (the refusal happens before any write).
    let mgs = json(&client, format!("{}/api/microgrids", s.ui_url)).await;
    assert_eq!(
        mgs.as_array()
            .expect("microgrids array")
            .iter()
            .filter(|m| m["id"] == wanted)
            .count(),
        1,
        "{mgs}"
    );
    let path = s
        .config
        .state_dir()
        .join(format!("microgrids/{wanted}.lisp"));
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("9301") && !saved.contains("9302"),
        "the refused import must not have touched {}: {saved}",
        path.display()
    );

    // And no file was written under any OTHER id on the way out —
    // the refusal is not a silent fallback to the next free one.
    let mut files: Vec<String> = std::fs::read_dir(s.config.state_dir().join("microgrids"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files, vec![format!("{wanted}.lisp")], "stray file written");
}

/// SP1's final review deferred the formula-convergence E2E to SP3
/// (this task) because it needs the loopback stream Task 1 added:
/// `grid_reactive_power`, fed by the upstream grid `AcPowerReactive`
/// formula. That formula spans every AC component under the grid —
/// including the EV charger, the concrete case of a component whose
/// formula term is present but whose Q slot never streams from the
/// device's own physics. SP3's mutation-check (temporarily flipping
/// the EV's `reactive_power_var` to `None`) plus a trace of the
/// vendored `frequenz-microgrid` crate sources found that
/// convergence does *not* actually depend on that sample: the crate
/// creates a resampler for every component the formula references
/// eagerly (before any data arrives), so a component that never
/// streams the metric simply resolves to `None` in its slot, and
/// the formula's own `COALESCE(..., 0.0)` absorbs it — the EV's
/// `Some(0.0)` is honest per-component telemetry, not a convergence
/// unblock. This test still proves the `grid_reactive_power` stream
/// end to end over that topology.
#[tokio::test(flavor = "multi_thread")]
async fn grid_reactive_formula_converges_over_a_site_with_an_ev_charger() {
    let topology = r#"
(%make-grid-connection-point :id 1
    :successors
    (list (%make-meter :id 2
                        :successors
                        (list (%make-battery-inverter :id 3
                                                        :successors
                                                        (list (%make-battery :id 4)))))
          (%make-meter :id 5
                       :successors
                       (list (%make-ev-charger :id 6)))))
"#;
    let s = TestServer::start(topology).await;
    let client = reqwest::Client::new();

    let mgs = json(&client, format!("{}/api/microgrids", s.ui_url)).await;
    let id = mgs
        .as_array()
        .expect("microgrids array")
        .first()
        .expect("one microgrid")["id"]
        .as_u64()
        .expect("microgrid id");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let snapshot = loop {
        let body = json(&client, format!("{}/api/mg/{id}/metrics/latest", s.ui_url)).await;
        let converged = body["grid_reactive_power"]["value"]
            .as_f64()
            .is_some_and(|v| v.is_finite());
        if converged || tokio::time::Instant::now() >= deadline {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let entry = &snapshot["grid_reactive_power"];
    let value = entry["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("grid_reactive_power never converged: {snapshot}"));
    assert!(value.is_finite(), "value not finite: {snapshot}");
    assert_eq!(entry["quantity"], "ReactivePower", "{snapshot}");
    assert_eq!(entry["unit"], "VAr", "{snapshot}");

    // Reactive energy (varh) integration is out of scope for this
    // stream — energy_stream_for never maps grid_reactive_power to a
    // companion, so no such key should ever appear.
    assert!(
        snapshot.get("grid_reactive_energy").is_none(),
        "unexpected grid_reactive_energy stream: {snapshot}"
    );

    // No PV in this site, and the stream appears anyway: the
    // empty-category formula renders to the literal "0.0" (see
    // category.rs:182-184 upstream) rather than failing to build, so
    // pv_reactive_power is deterministically pinned at exactly 0.
    // What the other categories do on an empty site is untested here.
    let pv_entry = &snapshot["pv_reactive_power"];
    let pv_value = pv_entry["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("pv_reactive_power never converged: {snapshot}"));
    assert_eq!(pv_value, 0.0, "{snapshot}");
    assert_eq!(pv_entry["quantity"], "ReactivePower", "{snapshot}");
    assert_eq!(pv_entry["unit"], "VAr", "{snapshot}");
}

/// The metrics panel's Reactive card charts per-source Q: grid, PV,
/// battery — the same logical-meter formulas as the power streams,
/// metric AcPowerReactive. This proves both new streams end to end
/// over a site that has both categories, and that neither grows a
/// varh companion (reactive-energy integration stays out of scope,
/// as with grid Q).
#[tokio::test(flavor = "multi_thread")]
async fn per_source_reactive_streams_converge() {
    let topology = r#"
(%make-grid-connection-point :id 1
    :successors
    (list (%make-meter :id 2
                        :successors
                        (list (%make-battery-inverter :id 3
                                                        :successors
                                                        (list (%make-battery :id 4)))))
          (%make-meter :id 5
                       :successors
                       (list (%make-solar-inverter :id 6)))))
"#;
    let s = TestServer::start(topology).await;
    let client = reqwest::Client::new();

    let mgs = json(&client, format!("{}/api/microgrids", s.ui_url)).await;
    let id = mgs
        .as_array()
        .expect("microgrids array")
        .first()
        .expect("one microgrid")["id"]
        .as_u64()
        .expect("microgrid id");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let snapshot = loop {
        let body = json(&client, format!("{}/api/mg/{id}/metrics/latest", s.ui_url)).await;
        let converged = ["pv_reactive_power", "battery_reactive_power"]
            .iter()
            .all(|s| body[*s]["value"].as_f64().is_some_and(|v| v.is_finite()));
        if converged || tokio::time::Instant::now() >= deadline {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    for stream in ["pv_reactive_power", "battery_reactive_power"] {
        let entry = &snapshot[stream];
        entry["value"]
            .as_f64()
            .filter(|v| v.is_finite())
            .unwrap_or_else(|| panic!("{stream} never converged: {snapshot}"));
        assert_eq!(entry["quantity"], "ReactivePower", "{stream}: {snapshot}");
        assert_eq!(entry["unit"], "VAr", "{stream}: {snapshot}");
    }
    assert!(
        snapshot.get("pv_reactive_energy").is_none()
            && snapshot.get("battery_reactive_energy").is_none(),
        "unexpected varh stream: {snapshot}"
    );
}

/// grid_frequency streams via the logical meter's Frequency formula
/// (a COALESCE over the PCC's meters) — usable since
/// frequenz-microgrid 0.6.0 wired the Frequency sender arm. The
/// topology here hangs TWO meters under the grid: the shape where
/// the old main-meter workaround returned None and the frequency
/// stream silently died. The topology payload also no longer
/// carries the retired main_meter_id flag.
#[tokio::test(flavor = "multi_thread")]
async fn grid_frequency_streams_on_a_multi_feeder_site() {
    let topology = r#"
(%make-grid-connection-point :id 1
    :successors
    (list (%make-meter :id 2
                        :successors
                        (list (%make-solar-inverter :id 3)))
          (%make-meter :id 4
                       :successors
                       (list (%make-battery-inverter :id 5
                                                       :successors
                                                       (list (%make-battery :id 6)))))))
"#;
    let s = TestServer::start(topology).await;
    let client = reqwest::Client::new();

    let mgs = json(&client, format!("{}/api/microgrids", s.ui_url)).await;
    let id = mgs
        .as_array()
        .expect("microgrids array")
        .first()
        .expect("one microgrid")["id"]
        .as_u64()
        .expect("microgrid id");

    let topo = json(&client, format!("{}/api/mg/{id}/topology", s.ui_url)).await;
    assert!(
        topo.get("main_meter_id").is_none(),
        "main_meter_id should be retired from the payload: {topo}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let snapshot = loop {
        let body = json(&client, format!("{}/api/mg/{id}/metrics/latest", s.ui_url)).await;
        let converged = body["grid_frequency"]["value"]
            .as_f64()
            .is_some_and(|v| v.is_finite());
        if converged || tokio::time::Instant::now() >= deadline {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let entry = &snapshot["grid_frequency"];
    let hz = entry["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("grid_frequency never converged: {snapshot}"));
    // The OU frequency model mean-reverts around 50 Hz; anything in a
    // generous grid band proves real data, not a fabricated zero.
    assert!(
        (45.0..=55.0).contains(&hz),
        "implausible Hz {hz}: {snapshot}"
    );
    assert_eq!(entry["quantity"], "Frequency", "{snapshot}");
    assert_eq!(entry["unit"], "Hz", "{snapshot}");
    assert!(
        snapshot.get("grid_frequency_energy").is_none(),
        "unexpected energy companion: {snapshot}"
    );
}

/// `GET`/`POST /api/mg/{mg}/weather`: 404 with no weather
/// configured, a partial POST creates it (config fields apply like
/// `(set-weather)`, same defaults as `WeatherConfig::default`), a
/// follow-up `GET` sees it, `pass_cloud` arms an event on top of a
/// config change, and a malformed field is a 400 that changes
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn weather_http_round_trip() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();

    let resp = client.get(s.mg_url("weather")).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    let body: Value = resp.json().await.unwrap();
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("no weather configured"), "{error}");

    // A cloud-only POST on a weatherless site is a 400, the same
    // answer `(pass-cloud …)` gives — NOT a silent "install a whole
    // default sky so the one cloud has somewhere to land".
    let orphan = client
        .post(s.mg_url("weather"))
        .json(&serde_json::json!({
            "pass_cloud": {"depth_pct": 50.0, "duration_s": 600.0}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(orphan.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = orphan.json().await.unwrap();
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("no weather on this site"), "{error}");

    // …and it really installed nothing: the site is still weatherless.
    let resp = client.get(s.mg_url("weather")).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // The other path: a POST carrying a CONFIG field still creates
    // weather on a weatherless site, exactly as `(set-weather …)` does.
    let created: Value = client
        .post(s.mg_url("weather"))
        .json(&serde_json::json!({"sunrise": "05:00"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["sunrise"], "05:00", "{created}");
    // Untouched fields keep WeatherConfig::default's values.
    assert_eq!(created["sunset"], "20:00", "{created}");
    assert_eq!(created["peak_pct"], 100.0, "{created}");

    let got = json(&client, s.mg_url("weather")).await;
    assert_eq!(got["sunrise"], "05:00", "{got}");

    let cloud: Value = client
        .post(s.mg_url("weather"))
        .json(&serde_json::json!({
            "pass_cloud": {"depth_pct": 50.0, "duration_s": 600.0}
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cloud["events"].as_array().unwrap().len(), 1, "{cloud}");
    // `now` is the snapshot's own instant, stamped by the same clock as
    // the event ends — it is what the panel filters its cloud list
    // against, so a just-fired cloud has to still be ahead of it.
    let now = DateTime::parse_from_rfc3339(cloud["now"].as_str().unwrap()).unwrap();
    let end = DateTime::parse_from_rfc3339(cloud["events"][0]["end"].as_str().unwrap()).unwrap();
    assert!(end > now, "{cloud}");
    // The config change from the prior request survived — a partial
    // update, not a reset.
    assert_eq!(cloud["sunrise"], "05:00", "{cloud}");

    let bad = client
        .post(s.mg_url("weather"))
        .json(&serde_json::json!({"sunrise": "nope"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    // The rejected request changed nothing — still one armed event,
    // still the same sunrise.
    let after = json(&client, s.mg_url("weather")).await;
    assert_eq!(after["events"].as_array().unwrap().len(), 1, "{after}");
    assert_eq!(after["sunrise"], "05:00", "{after}");
}

const SOLAR_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
    :successors
    (list (%make-solar-inverter :id 2 :rated-lower-w -10000.0 :rated-upper-w 0.0)
          (%make-meter :id 3)))
"#;

/// `POST /api/mg/{mg}/component/{id}/drive` with `clear_sunlight:
/// true` drops a prior `sunlight_pct` poke and returns the inverter
/// to following the site's weather — the read-back knob shows the
/// "weather" marker `sunlight_reading` gives a `Follow` source. On a
/// non-solar component (a meter) the same field is a 400, not a
/// silent no-op.
#[tokio::test(flavor = "multi_thread")]
async fn drive_clear_sunlight_returns_to_weather_and_rejects_non_solar() {
    let s = TestServer::start(SOLAR_TOPOLOGY).await;
    let client = reqwest::Client::new();

    client
        .post(s.mg_url("component/2/drive"))
        .json(&serde_json::json!({"sunlight_pct": 42.0}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let comp = json(&client, s.mg_url("component/2")).await;
    let knobs = comp["knobs"].as_array().unwrap();
    let poked = knobs
        .iter()
        .find(|k| k["knob"] == "solar-sunlight")
        .unwrap_or_else(|| panic!("no solar-sunlight knob: {comp}"));
    assert_eq!(poked["value"], 42.0, "{comp}");
    assert!(poked["expr"].is_null(), "a poke has no source text: {comp}");

    client
        .post(s.mg_url("component/2/drive"))
        .json(&serde_json::json!({"clear_sunlight": true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let comp = json(&client, s.mg_url("component/2")).await;
    let cleared = comp["knobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["knob"] == "solar-sunlight")
        .unwrap_or_else(|| panic!("no solar-sunlight knob: {comp}"));
    assert_eq!(cleared["expr"], "weather", "{comp}");

    // clear_sunlight on a meter (not a solar inverter) is a 4xx
    // rejection, not a silent no-op.
    let resp = client
        .post(s.mg_url("component/3/drive"))
        .json(&serde_json::json!({"clear_sunlight": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"], "component 3 does not take clear_sunlight (not a solar inverter)",
        "{body}"
    );
}

/// Each drive field sent to a component of the wrong kind is a 400
/// whose body names the component, the field and the kind it needs.
/// TINY_TOPOLOGY's meter 2 takes the non-meter fields and battery 3
/// the meter ones. `soc_pct` is pinned by
/// `drive_soc_on_an_empty_charger_names_the_missing_ev`.
#[tokio::test(flavor = "multi_thread")]
async fn drive_rejects_each_field_on_the_wrong_component_kind() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let client = reqwest::Client::new();
    let cases = [
        (
            3,
            serde_json::json!({"power_w": 1000.0}),
            "component 3 does not take power_w (not a meter)",
        ),
        (
            3,
            serde_json::json!({"reactive_power_var": 500.0}),
            "component 3 does not take reactive_power_var (not a meter)",
        ),
        (
            3,
            serde_json::json!({"power_factor": 0.9}),
            "component 3 does not take power_factor (not a meter)",
        ),
        (
            3,
            serde_json::json!({"clear_power": true}),
            "component 3 does not take clear_power (not a meter)",
        ),
        (
            3,
            serde_json::json!({"clear_reactive": true}),
            "component 3 does not take clear_reactive (not a meter)",
        ),
        (
            2,
            serde_json::json!({"sunlight_pct": 40.0}),
            "component 2 does not take sunlight_pct (not a solar inverter)",
        ),
        (
            2,
            serde_json::json!({"clear_sunlight": true}),
            "component 2 does not take clear_sunlight (not a solar inverter)",
        ),
        (
            2,
            serde_json::json!({"steam_demand_kg_per_s": 40.0}),
            "component 2 does not take steam_demand_kg_per_s (not a steam boiler)",
        ),
        (
            2,
            serde_json::json!({"pressure_bar": 9.0}),
            "component 2 does not take pressure_bar (not a steam boiler)",
        ),
    ];
    for (id, req, error) in cases {
        let resp = client
            .post(s.mg_url(&format!("component/{id}/drive")))
            .json(&req)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST, "{req}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], error, "{req}: {body}");
    }
}

/// The steam boiler pool rides the loopback like the battery pool:
/// an aggregate power stream, a bounds envelope flattened into two
/// point streams, and a cumulative energy companion. Meter 5 fronts
/// the boiler so the pool formula reads the metered line; the
/// boiler's own reading is only the fallback.
#[tokio::test(flavor = "multi_thread")]
async fn steam_boiler_pool_streams_reach_the_loopback_snapshot() {
    let topology = r#"
(%make-grid-connection-point :id 1
    :successors
    (list (%make-meter :id 2
                        :successors
                        (list (%make-meter :id 5
                                           :successors
                                           (list (%make-steam-boiler :id 6)))))))
"#;
    let s = TestServer::start(topology).await;
    let client = reqwest::Client::new();

    let mgs = json(&client, format!("{}/api/microgrids", s.ui_url)).await;
    let id = mgs
        .as_array()
        .expect("microgrids array")
        .first()
        .expect("one microgrid")["id"]
        .as_u64()
        .expect("microgrid id");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let snapshot = loop {
        let body = json(&client, format!("{}/api/mg/{id}/metrics/latest", s.ui_url)).await;
        let converged = ["steam_boiler_pool_power", "steam_boiler_pool_bounds_upper"]
            .iter()
            .all(|k| body[k]["value"].as_f64().is_some_and(|v| v.is_finite()));
        if converged || tokio::time::Instant::now() >= deadline {
            break body;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let power = &snapshot["steam_boiler_pool_power"];
    let value = power["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("steam_boiler_pool_power never converged: {snapshot}"));
    assert!(value.is_finite(), "value not finite: {snapshot}");
    assert_eq!(power["quantity"], "Power", "{snapshot}");
    assert_eq!(power["unit"], "W", "{snapshot}");

    let lower = snapshot["steam_boiler_pool_bounds_lower"]["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("no lower bound: {snapshot}"));
    let upper = snapshot["steam_boiler_pool_bounds_upper"]["value"]
        .as_f64()
        .unwrap_or_else(|| panic!("no upper bound: {snapshot}"));
    assert!(lower <= upper, "inverted envelope: {snapshot}");

    assert_eq!(
        snapshot["steam_boiler_pool_energy"]["unit"], "Wh",
        "energy companion missing: {snapshot}"
    );
}

/// A runtime poke such as `(set-meter-power …)` fires TopologyChanged
/// like every eval; the aggregate history behind the metrics charts
/// must survive it end to end. (Before the loopback checked the site
/// shape, the rebuild this triggered wiped the ring.) Whether the
/// poke rebuilds at all is pinned by the loopback's own unit test
/// `runtime_pokes_do_not_change_the_site_shape`.
#[tokio::test(flavor = "multi_thread")]
async fn a_meter_power_override_keeps_the_aggregate_history() {
    // The component graph wants the battery behind an inverter, or the
    // loopback's graph build never succeeds.
    let topology = r#"
(%make-grid-connection-point :id 1
    :successors
    (list (%make-meter :id 2
                       :successors
                       (list (%make-battery-inverter :id 3
                                                     :successors
                                                     (list (%make-battery :id 4)))))))
"#;
    let s = TestServer::start(topology).await;
    let client = reqwest::Client::new();
    let mgs = json(&client, format!("{}/api/microgrids", s.ui_url)).await;
    let id = mgs.as_array().expect("microgrids array")[0]["id"]
        .as_u64()
        .expect("microgrid id");
    let history_url = format!("{}/api/mg/{id}/metrics/history", s.ui_url);
    let grid_power_len = |body: &Value| body["grid_power"].as_array().map_or(0, |a| a.len());

    // Let the loopback connect and the grid_power ring fill a little.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let before = loop {
        let n = grid_power_len(&json(&client, history_url.clone()).await);
        if n >= 4 || tokio::time::Instant::now() >= deadline {
            break n;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(
        before >= 4,
        "grid_power history never filled: {before} samples"
    );

    let resp = client
        .post(format!("{}/api/eval", s.ui_url))
        .body("(set-meter-power 2 1000.0)")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: Value = resp.json().await.unwrap();
    assert!(body["value"].is_string(), "{body}");

    // Past the supervisor's 300 ms debounce and long enough for a
    // rebuild to have wiped the ring and refilled only a sample or two.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let after = grid_power_len(&json(&client, history_url.clone()).await);
    assert!(
        after >= before,
        "grid_power history restarted across the override: {before} -> {after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ev_route_reports_plug_state() {
    let s = TestServer::start(EV_TOPOLOGY).await;
    let client = reqwest::Client::new();
    let base = format!("{}/api/mg/2200", s.ui_url);

    let empty = json(&client, format!("{base}/component/6/ev")).await;
    assert_eq!(empty["plugged"], false, "{empty}");
    // The catalog rides along even on an empty charger, so the
    // inspector's dropdown never hardcodes it.
    assert!(
        empty["presets"]
            .as_array()
            .unwrap()
            .contains(&Value::from("city")),
        "{empty}"
    );

    eval_or_panic(&client, &s, "(plug-ev 6 'city :soc-pct 25)").await;

    let plugged = json(&client, format!("{base}/component/6/ev")).await;
    assert_eq!(plugged["plugged"], true, "{plugged}");
    assert_eq!(plugged["preset"], "city");
    assert_eq!(plugged["phases"], 1);
    assert!((plugged["soc_pct"].as_f64().unwrap() - 25.0).abs() < 0.01);
    assert!(plugged["state"].is_string());

    let not_charger = client
        .get(format!("{base}/component/2/ev"))
        .send()
        .await
        .unwrap();
    assert_eq!(not_charger.status(), 400);
    assert_eq!(
        not_charger.json::<Value>().await.unwrap()["error"],
        "component 2 is not an EV charger"
    );
    let missing = client
        .get(format!("{base}/component/99/ev"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

/// `soc_pct` on a charger drives the plugged CAR's SoC, so an empty
/// charger is a rejection — and one that says what is actually
/// missing rather than "not a battery", which is true of every
/// charger, plugged or not.
#[tokio::test(flavor = "multi_thread")]
async fn drive_soc_on_an_empty_charger_names_the_missing_ev() {
    let s = TestServer::start(EV_TOPOLOGY).await;
    let client = reqwest::Client::new();
    let base = format!("{}/api/mg/2200", s.ui_url);

    let resp = client
        .post(s.mg_url("component/6/drive"))
        .json(&serde_json::json!({"soc_pct": 50.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "charger 6 has no EV plugged in", "{body}");

    // A component with no pack at all still gets the old message.
    let resp = client
        .post(s.mg_url("component/2/drive"))
        .json(&serde_json::json!({"soc_pct": 50.0}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"], "component 2 does not take soc_pct (not a battery)",
        "{body}"
    );

    // With a car plugged in the same request lands on the car.
    eval_or_panic(&client, &s, "(plug-ev 6 'city :soc-pct 25)").await;
    client
        .post(s.mg_url("component/6/drive"))
        .json(&serde_json::json!({"soc_pct": 50.0}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let plugged = json(&client, format!("{base}/component/6/ev")).await;
    assert!(
        (plugged["soc_pct"].as_f64().unwrap() - 50.0).abs() < 0.01,
        "{plugged}"
    );
}

/// …and because that request writes the CAR, it takes the plug knob's
/// snapshot first, exactly as `set-battery-soc` does in Lisp: a
/// scenario that teleports a charger's SoC has that undone by its own
/// teardown, along with the plug itself. (A battery's SoC still has no
/// snapshot on any door — see scenarios/README.md.)
#[tokio::test(flavor = "multi_thread")]
async fn drive_soc_on_a_charger_is_undone_by_scenario_teardown() {
    let s = TestServer::start(EV_TOPOLOGY).await;
    let client = reqwest::Client::new();
    let base = format!("{}/api/mg/2200", s.ui_url);

    eval_or_panic(&client, &s, "(plug-ev 6 'city :soc-pct 25)").await;
    eval_or_panic(&client, &s, "(scenario-start \"ev\")").await;
    client
        .post(s.mg_url("component/6/drive"))
        .json(&serde_json::json!({"soc_pct": 60.0}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mid = json(&client, format!("{base}/component/6/ev")).await;
    assert!(
        (mid["soc_pct"].as_f64().unwrap() - 60.0).abs() < 0.5,
        "{mid}"
    );

    eval_or_panic(&client, &s, "(scenario-stop)").await;
    let after = json(&client, format!("{base}/component/6/ev")).await;
    assert_eq!(
        after["plugged"], true,
        "the car the run found is back: {after}"
    );
    assert!(
        (after["soc_pct"].as_f64().unwrap() - 25.0).abs() < 0.5,
        "teardown must put the car back at the SoC it found: {after}"
    );
}

/// A microgrid registered by a REPL eval after start gets a runtime
/// through the listener, and the list shows it running.
#[tokio::test(flavor = "multi_thread")]
async fn a_repl_registered_microgrid_is_running() {
    let server = TestServer::start("nil").await;
    let client = reqwest::Client::new();
    eval_or_panic(
        &client,
        &server,
        "(make-microgrid :id 71 :grpc-port 8971 :topology (lambda () nil))",
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let list = json(&client, format!("{}/api/microgrids", server.ui_url)).await;
        let status = list
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == 71)
            .and_then(|e| e["runtime"]["status"].as_str().map(str::to_string));
        if status.as_deref() == Some("running") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "71 never ran: {list}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
