use super::*;
use crate::lisp::Config;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use std::io::Write;
use tower::ServiceExt;

/// Boots a `Config` against a freshly-written tiny config file
/// holding `body`, so the live tulisp ctx + MicrogridSite are wired up the
/// same way the binary wires them. Returns the Config; caller
/// composes a router with it.
///
/// Each call gets its own unique subdirectory under `temp_dir()`
/// so concurrent test runs don't stomp each other's config.lisp
/// (cargo runs the lib test suite multi-threaded by default).
async fn config_with(body: &str) -> Config {
    config_with_dir(body).await.0
}

/// [`config_with`] plus the state directory it booted in — for tests
/// that read the files macrocosim writes (managed microgrid files,
/// `enterprise.lisp`) or boot a second `Config` on the same dir.
async fn config_with_dir(body: &str) -> (Config, std::path::PathBuf) {
    // tulisp-async's executor needs a tokio runtime in scope; we
    // already have one via #[tokio::test], so Config::new works.
    let mut p = std::env::temp_dir();
    p.push(format!(
        "macrocosim-ui-{}-{}",
        std::process::id(),
        // Counter — even if SystemTime resolves the same nanos for
        // two near-simultaneous tests, the AtomicU64 disambiguates.
        UNIQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&p).unwrap();
    let path = p.join("config.lisp");
    let wrapped = wrap_test_body(body);
    write!(std::fs::File::create(&path).unwrap(), "{wrapped}").unwrap();
    let cfg = Config::new(path.to_str().unwrap()).expect("config eval");
    (cfg, p)
}

/// Wrap a test body in `(make-microgrid …)` if the body doesn't already
/// register one. Tests that care about the microgrid's id supply
/// their own `(make-microgrid …)` form; everything else gets the
/// fixed default id 2200.
fn wrap_test_body(body: &str) -> String {
    if body.contains("make-microgrid") {
        return body.to_string();
    }
    let inner = if body.trim().is_empty() {
        "nil".to_string()
    } else {
        body.to_string()
    };
    format!("(make-microgrid :id 2200 :grpc-port 8800 :topology (lambda () {inner}))")
}

static UNIQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One-shot a request and return (status, body). axum's `oneshot`
/// avoids binding a real port. The runtimes are inert, so nothing is
/// started and every loopback slot is empty: `metrics/status`
/// answers "not connected" without a real gRPC server. Tests that
/// want a populated handle would have to spin up the gRPC server
/// too.
async fn call(config: Config, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    call_with(config, crate::runtime::MicrogridRuntimes::inert(), req).await
}

/// [`call`] with a runtimes handle the test keeps across requests.
async fn call_with(
    config: Config,
    runtimes: crate::runtime::MicrogridRuntimes,
    req: Request<Body>,
) -> (StatusCode, Vec<u8>) {
    let resp = router(config, runtimes).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, bytes.to_vec())
}

fn get(path: &str) -> Request<Body> {
    Request::builder().uri(path).body(Body::empty()).unwrap()
}

fn post(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The `error` field of a JSON error body; panics with the raw body
/// when it is not one.
fn error_of(body: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(body)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(body)));
    v["error"]
        .as_str()
        .unwrap_or_else(|| panic!("no error field: {v}"))
        .to_string()
}

#[tokio::test]
async fn index_serves_embedded_shell() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/")).await;
    assert_eq!(status, StatusCode::OK);
    let s = String::from_utf8_lossy(&body);
    assert!(s.contains("<title>macrocosim</title>"));
    assert!(s.contains("/assets/app.js"));
}

#[tokio::test]
async fn scripts_listing_walks_the_state_dir_and_rejects_escapes() {
    let cfg = config_with("").await;
    let root = cfg.state_dir().to_path_buf();
    std::fs::create_dir_all(root.join("examples")).unwrap();
    std::fs::write(root.join("examples/demo.lisp"), "nil").unwrap();
    std::fs::write(root.join("notes.txt"), "not lisp").unwrap();

    let (status, body) = call(cfg.clone(), get("/api/scripts")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["parent"].is_null());
    assert!(
        v["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d == "examples")
    );
    // config.lisp is listed, the .txt is not.
    let files = v["files"].as_array().unwrap();
    assert!(files.iter().any(|f| f == "config.lisp"));
    assert!(!files.iter().any(|f| f == "notes.txt"));

    let (status, body) = call(cfg.clone(), get("/api/scripts?dir=examples")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["parent"], "");
    assert!(
        v["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "demo.lisp")
    );

    let (status, _) = call(cfg.clone(), get("/api/scripts?dir=..")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(cfg, get("/api/scripts?dir=%2Fetc")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn asset_route_serves_embedded_files() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/assets/app.js")).await;
    assert_eq!(status, StatusCode::OK);
    // Phrase from app.js — anchors the test against actually
    // serving the right file rather than just any 200.
    assert!(String::from_utf8_lossy(&body).contains("vis-network"));
}

#[tokio::test]
async fn asset_route_serves_vendored_lib() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/assets/vendor/vis-network.min.js")).await;
    assert_eq!(status, StatusCode::OK);
    // The bundle names itself throughout; "vis" alone would match
    // almost any JS file ("visibility", "provision", ...).
    assert!(String::from_utf8_lossy(&body).contains("vis-network"));
}

#[tokio::test]
async fn asset_route_404s_unknown_path() {
    let cfg = config_with("").await;
    let (status, _) = call(cfg, get("/assets/does-not-exist.js")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn topology_endpoint_emits_components_and_connections() {
    let cfg = config_with(
        r#"(%make-grid-connection-point :id 1
             :successors
             (list (%make-meter :id 2
                     :successors
                     (list (%make-battery :id 3)))))"#,
    )
    .await;

    let (status, body) = call(cfg, get("/api/mg/2200/topology")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(parsed["components"].as_array().unwrap().len(), 3);
    assert_eq!(parsed["connections"].as_array().unwrap().len(), 2);
}

/// Both evals answer 200 {"value"} on success and 400 {"error"} on
/// an evaluation error; neither body carries `ok`.
#[tokio::test]
async fn eval_answers_with_status_codes() {
    let cfg = config_with("").await;
    for path in ["/api/eval", "/api/mg/2200/eval"] {
        let (status, body) = call(cfg.clone(), post(path, "(+ 1 2)")).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v, serde_json::json!({"value": "3"}), "{path}");

        let (status, body) = call(cfg.clone(), post(path, "(no-such-fn)")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert!(error_of(&body).contains("no-such-fn"), "{path}");
    }
}

/// A scoped eval for an unregistered microgrid is the shared 404.
#[tokio::test]
async fn scoped_eval_for_an_unregistered_microgrid_is_404() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, post("/api/mg/9999/eval", "(+ 1 2)")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_of(&body), "microgrid 9999 not registered");
}

/// Every per-microgrid route answers the shared 404 for an
/// unregistered microgrid, and 400 for a non-numeric one.
#[tokio::test]
async fn every_microgrid_route_shares_the_unregistered_404() {
    let cfg = config_with("").await;
    let gets = [
        "topology",
        "formula?metric=grid",
        "weather",
        "metrics/status",
        "metrics/latest",
        "metrics/history",
        "metrics/formulas",
        "component/1",
        "component/1/history?metric=active_power",
        "component/1/setpoints",
        "component/1/ev",
        "undo",
        "snapshots",
        "dispatches",
    ];
    for suffix in gets {
        let (status, body) = call(cfg.clone(), get(&format!("/api/mg/9999/{suffix}"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "GET {suffix}");
        assert_eq!(
            error_of(&body),
            "microgrid 9999 not registered",
            "GET {suffix}"
        );
    }
    let posts = [
        ("eval", "(+ 1 2)"),
        ("component/1/status", "{}"),
        ("component/1/drive", "{}"),
        ("weather", "{}"),
        ("adopt", ""),
        ("undo", ""),
        ("redo", ""),
        ("snapshots", r#"{"name":"x"}"#),
        ("snapshots/load", r#"{"name":"x"}"#),
        ("dispatches", "{}"),
        ("dispatches/1/active", "{}"),
    ];
    for (suffix, body) in posts {
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/mg/9999/{suffix}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, resp) = call(cfg.clone(), req).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "POST {suffix}");
        assert_eq!(
            error_of(&resp),
            "microgrid 9999 not registered",
            "POST {suffix}"
        );
    }
    let (status, resp) = call(cfg.clone(), delete_req("/api/mg/9999/dispatches/1")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "DELETE dispatches/1");
    assert_eq!(
        error_of(&resp),
        "microgrid 9999 not registered",
        "DELETE dispatches/1"
    );
    let (status, body) = call(cfg, get("/api/mg/abc/topology")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(&body), "invalid microgrid id: \"abc\"");
}

/// The unscoped per-microgrid routes are gone.
#[tokio::test]
async fn unscoped_microgrid_routes_are_gone() {
    let cfg = config_with("").await;
    for path in [
        "/api/topology",
        "/api/weather",
        "/api/history?id=1&metric=active_power",
        "/api/setpoints?id=1",
        "/api/component?id=1",
        "/api/microgrid/status",
        "/api/microgrid/latest",
        "/api/microgrid/history",
        "/api/microgrid/formulas",
        "/api/mg/2200/microgrid/status",
        "/api/mg/2200/component?id=1",
        "/api/mg/2200/history?id=1&metric=active_power",
        "/api/mg/2200/setpoints?id=1",
        "/api/mg/2200/ev/1",
    ] {
        let (status, body) = call(cfg.clone(), get(path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(error_of(&body).starts_with("no route for GET"), "{path}");
    }
    for path in ["/api/microgrids/create", "/api/mg/2200/snapshots/save"] {
        let (status, body) = call(cfg.clone(), post_json(path, "{}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "POST {path}");
        assert!(error_of(&body).starts_with("no route for POST"), "{path}");
    }
}

/// A registered microgrid whose runtime never started reports
/// "not connected", not "not registered".
#[tokio::test]
async fn metrics_for_a_microgrid_without_a_loopback_are_not_connected() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/mg/2200/metrics/status")).await;
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["connected"], false);
}

/// Success bodies carry no `ok`; actions with nothing to return are
/// 204; metrics without a client report "not connected".
#[tokio::test]
async fn success_bodies_are_plain() {
    let cfg = config_with("(%make-grid-connection-point :id 1)").await;

    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/mg/2200/component/1/status")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"health":"ok"}"#))
        .unwrap();
    let (status, body) = call(cfg.clone(), req).await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(body.is_empty());

    let (status, body) = call(cfg.clone(), get("/api/mg/2200/undo")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.get("ok").is_none(), "{v}");
    assert!(v.get("undo_depth").is_some(), "{v}");

    let (status, body) = call(cfg.clone(), get("/api/mg/2200/metrics/status")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"connected": false, "component_count": null})
    );

    let (status, body) = call(cfg.clone(), get("/api/mg/2200/metrics/latest")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({})
    );

    let (status, body) = call(cfg, get("/api/mg/2200/metrics/formulas")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_of(&body), "metrics client not connected");
}

/// Creating a microgrid is a POST to the collection, answering 201.
#[tokio::test]
async fn creating_a_microgrid_answers_201() {
    let cfg = config_with("").await;
    let (status, body) = call(
        cfg,
        post_json("/api/microgrids", r#"{"name":"b","id":2301}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], 2301);
}

/// The whole-site eval can create a microgrid: site-level Lisp still
/// has a door once the unscoped routes are gone.
#[tokio::test]
async fn site_eval_creates_a_microgrid() {
    let cfg = config_with("").await;
    let (status, _) = call(
        cfg.clone(),
        post(
            "/api/eval",
            "(make-microgrid :id 2300 :grpc-port 8820 :topology (lambda () nil))",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(cfg.microgrids().lock().contains_key(&2300));
}

/// A formula error is a 400; one with a kind keeps it beside
/// `error`.
#[tokio::test]
async fn a_formula_error_is_400_with_its_kind() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/mg/2200/formula?metric=grid")).await;
    // The default test microgrid has no components.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(&body), "The microgrid has no components yet.");

    let cfg = config_with(FORMULA_TOPOLOGY).await;
    let (status, body) = call(cfg, get("/api/mg/2200/formula?metric=battery&ids=99")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["kind"], "component_not_found");
    assert!(v.get("ok").is_none(), "{v}");
}

#[tokio::test]
async fn format_endpoint_pretty_prints_lisp() {
    let cfg = config_with("").await;
    let (status, body) = call(
        cfg,
        post("/api/format?width=20", "(when (< x 5)(inc x)(princ x))"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Width 20 forces (when …) to break header-then-body, with each
    // body form on its own line at +2.
    assert_eq!(
        String::from_utf8_lossy(&body),
        "(when (< x 5)\n  (inc x)\n  (princ x))\n"
    );
}

#[tokio::test]
async fn format_endpoint_returns_400_on_parse_error() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, post("/api/format", "(unbalanced")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!error_of(&body).is_empty());
}

#[tokio::test]
async fn history_endpoint_returns_recent_samples() {
    // Build a site with a battery, then drive the sampler twice
    // synchronously so the rings have content to query. Battery
    // publishes soc_pct in its telemetry; that's what we query.
    let cfg = config_with("(%make-battery :id 1000)").await;
    let site = cfg.site();
    let now = chrono::Utc::now();
    site.record_history_snapshot(now - chrono::Duration::seconds(2));
    site.record_history_snapshot(now - chrono::Duration::seconds(1));

    let (status, body) = call(
        cfg,
        get("/api/mg/2200/component/1000/history?metric=soc_pct&window_s=10"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["id"], 1000);
    assert_eq!(parsed["metric"], "soc_pct");
    let samples = parsed["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 2);
    // Each sample is [ts_ms, value]
    assert!(samples[0][0].as_i64().unwrap() < samples[1][0].as_i64().unwrap());
}

#[tokio::test]
async fn history_endpoint_rejects_unknown_metric() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/mg/2200/component/1/history?metric=foo")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_of(&body).contains("unknown metric"));
}

#[tokio::test]
async fn history_endpoint_returns_empty_for_unknown_component() {
    let cfg = config_with("").await;
    let (status, body) = call(
        cfg,
        get("/api/mg/2200/component/999/history?metric=active_power_w"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["samples"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn eval_endpoint_mutates_world() {
    // Confirm an /api/eval call that registers a component shows
    // up in the topology endpoint immediately afterwards. This is
    // the load-bearing claim of the "Lisp eval as the unifying
    // mutation API" design.
    let cfg = config_with("").await;
    let (status, _) = call(
        cfg.clone(),
        post("/api/eval", "(%make-grid-connection-point :id 42)"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = call(cfg, get("/api/mg/2200/topology")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let components = parsed["components"].as_array().unwrap();
    assert!(components.iter().any(|c| c["id"] == 42));
}

#[tokio::test]
async fn scenario_endpoints_round_trip_lifecycle_and_events() {
    let cfg = config_with("").await;

    // Pre-start: name is null, count is 0.
    let (_, body) = call(cfg.clone(), get("/api/scenario")).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["name"].is_null());
    assert_eq!(v["event_count"], 0);

    // Start + record two events.
    call(
        cfg.clone(),
        post("/api/eval", "(scenario-start \"warmup\")"),
    )
    .await;
    call(
        cfg.clone(),
        post("/api/eval", "(scenario-event 'outage \"bat-1003\")"),
    )
    .await;
    call(
        cfg.clone(),
        post("/api/eval", "(scenario-event \"note\" \"hi\")"),
    )
    .await;

    // Summary reflects the events.
    let (status, body) = call(cfg.clone(), get("/api/scenario")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["name"], "warmup");
    assert_eq!(v["event_count"], 2);
    assert_eq!(v["next_event_id"], 2);

    // /api/scenario/events with default since=0 returns both.
    let (status, body) = call(cfg.clone(), get("/api/scenario/events")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "outage");
    assert_eq!(events[1]["kind"], "note");

    // since=1 cursor returns only id 1 onward.
    let (_, body) = call(cfg.clone(), get("/api/scenario/events?since=1")).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = v["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], 1);
}

#[tokio::test]
async fn scenario_report_endpoint_returns_grid_peak() {
    let cfg = config_with(
        "(%make-grid-connection-point
           :id 1
           :successors (list (%make-meter :id 2)))
         (scenario-start \"smoke\")",
    )
    .await;
    // The peak rides the loopback's grid_power formula stream. This
    // fixture serves HTTP without a microgrid loopback, so the sample
    // is handed to the site hook directly — the same call the
    // forwarder makes.
    cfg.site().record_grid_power_sample(4500.0, Utc::now());

    let (status, body) = call(cfg, get("/api/scenario/report")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // No meter id: the main-meter concept is retired from the report.
    assert!(v.get("main_meter_id").is_none(), "{v}");
    let peak = v["peak_grid_w"].as_f64().unwrap();
    assert!((peak - 4500.0).abs() < 1e-3, "got peak {peak}");
}

fn seed_dispatch(
    store: &crate::sim::dispatch::SharedDispatchStore,
    mg: u64,
    id: u64,
    type_: &str,
    active: bool,
) {
    use crate::proto::dispatch as dpb;
    store.insert(
        mg,
        dpb::Dispatch {
            metadata: Some(dpb::DispatchMetadata {
                dispatch_id: id,
                ..Default::default()
            }),
            data: Some(dpb::DispatchData {
                r#type: type_.to_string(),
                is_active: active,
                ..Default::default()
            }),
        },
    );
}

#[tokio::test]
async fn dispatches_endpoint_lists_microgrid_dispatches_newest_first() {
    let cfg = config_with("").await;
    let store = cfg.dispatches();
    seed_dispatch(&store, 2200, 1, "ALPHA", true);
    seed_dispatch(&store, 2200, 2, "PEAK_SHAVE", false);
    // A dispatch for another microgrid must not leak into 2200's list.
    seed_dispatch(&store, 999, 3, "OTHER", true);

    let (status, body) = call(cfg, get("/api/mg/2200/dispatches")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Newest (highest id) first.
    assert_eq!(arr[0]["id"], 2);
    assert_eq!(arr[0]["type"], "PEAK_SHAVE");
    assert_eq!(arr[0]["active"], false);
    assert_eq!(arr[1]["id"], 1);
    assert_eq!(arr[1]["type"], "ALPHA");
    assert_eq!(arr[1]["active"], true);
}

#[tokio::test]
async fn dispatches_endpoint_empty_for_microgrid_without_dispatches() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/mg/2200/dispatches")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v.as_array().unwrap().is_empty());
}

fn post_json(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn delete_req(path: &str) -> Request<Body> {
    Request::builder()
        .method(Method::DELETE)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

fn active_dispatch(type_: &str) -> crate::proto::dispatch::DispatchData {
    crate::proto::dispatch::DispatchData {
        r#type: type_.to_string(),
        is_active: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn dispatch_create_endpoint_stores_and_returns_view() {
    let cfg = config_with("").await;
    let (status, body) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/dispatches",
            r#"{"type":"ALPHA","target":"BATTERY","payload":{"target_power_w":5000}}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["type"], "ALPHA");
    assert_eq!(v["active"], true);
    assert_eq!(v["target"], "BATTERY");
    assert_eq!(v["payload"]["target_power_w"], 5000.0);
    // start_immediately default => a start time was stamped.
    assert!(v["start_ms"].is_i64());
    assert_eq!(cfg.dispatches().list_mg(2200).len(), 1);
}

#[tokio::test]
async fn dispatch_create_endpoint_accepts_recurrence() {
    let cfg = config_with("").await;
    let (status, body) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/dispatches",
            r#"{"type":"ALPHA","target":"battery","duration_s":3600,
                "recurrence":{"freq":"daily","interval":2}}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["recurrence"], "daily ×2");
    // Recurring dispatches have no single predetermined end, even
    // with a per-occurrence duration set.
    assert!(v["end_ms"].is_null());

    // freq "once" is the explicit no-recurrence spelling.
    let (status, body) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/dispatches",
            r#"{"type":"ALPHA","target":"battery","recurrence":{"freq":"once"}}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["recurrence"].is_null());

    // Unknown frequency names are a client bug — reject loudly.
    let (status, _) = call(
        cfg,
        post_json(
            "/api/mg/2200/dispatches",
            r#"{"type":"ALPHA","target":"battery","recurrence":{"freq":"fortnightly"}}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dispatch_create_endpoint_rejects_bad_target() {
    let cfg = config_with("").await;
    let (status, _) = call(
        cfg,
        post_json(
            "/api/mg/2200/dispatches",
            r#"{"type":"X","target":"not-a-category"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn dispatch_active_endpoint_pauses_and_resumes() {
    let cfg = config_with("").await;
    let id = cfg
        .dispatches()
        .create(2200, active_dispatch("X"), true)
        .unwrap()
        .metadata
        .unwrap()
        .dispatch_id;

    let (status, body) = call(
        cfg.clone(),
        post_json(
            &format!("/api/mg/2200/dispatches/{id}/active"),
            r#"{"active":false}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["active"], false);
    assert!(
        !cfg.dispatches()
            .get(2200, id)
            .unwrap()
            .data
            .unwrap()
            .is_active
    );

    // Resume.
    let (status, _) = call(
        cfg.clone(),
        post_json(
            &format!("/api/mg/2200/dispatches/{id}/active"),
            r#"{"active":true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        cfg.dispatches()
            .get(2200, id)
            .unwrap()
            .data
            .unwrap()
            .is_active
    );
}

#[tokio::test]
async fn dispatch_delete_endpoint_removes_then_404s() {
    let cfg = config_with("").await;
    let id = cfg
        .dispatches()
        .create(2200, active_dispatch("X"), true)
        .unwrap()
        .metadata
        .unwrap()
        .dispatch_id;

    let (status, _) = call(
        cfg.clone(),
        delete_req(&format!("/api/mg/2200/dispatches/{id}")),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(cfg.dispatches().get(2200, id).is_none());

    // Deleting again is a 404.
    let (status, _) = call(cfg, delete_req(&format!("/api/mg/2200/dispatches/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The typed control endpoints mutate the site without Lisp: a drive
/// lands a meter's constant power override, and every rejection is a
/// structured HTTP error (400/404 + JSON), not an `ok: false` payload.
#[tokio::test]
async fn control_drive_sets_meter_power() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, _) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/7/drive", r#"{"power_w": 1234.5}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    cfg.refresh_once();
    let m = cfg.site().get(7).unwrap();
    assert!((m.aggregate_power_w(&cfg.site()) - 1234.5).abs() < 1e-3);

    // An unknown component is a 404 with the reason in the body.
    let (status, body) = call(
        cfg,
        post_json("/api/mg/2200/component/999/drive", r#"{"power_w": 1.0}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("999"));
}

/// The drive op's reactive twins mirror `power_w`: `reactive_var` lands
/// a constant Q override, `power_factor` (+ optional `leading`) holds Q
/// at a power factor tracking live P. A non-meter and an out-of-range
/// power_factor are both 400s.
#[tokio::test]
async fn drive_op_accepts_reactive_var_and_power_factor() {
    let cfg = config_with(
        "(%make-meter :id 7 :power 8000.0)
                            (%make-solar-inverter :id 8)",
    )
    .await;

    // reactive_var: constant Q override.
    let (status, _) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/7/drive",
            r#"{"reactive_var": 500.0}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let m = cfg.site().get(7).unwrap();
    assert!((m.aggregate_reactive_var(&cfg.site()) - 500.0).abs() < 1e-3);

    // power_factor + leading: Q derives from live P, sign flipped.
    let (status, _) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/7/drive",
            r#"{"power_factor": 0.8, "leading": true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!((m.aggregate_reactive_var(&cfg.site()) - -6_000.0).abs() < 1.0);

    // A non-meter rejects both new fields.
    let (status, body) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/8/drive",
            r#"{"reactive_var": 100.0}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("reactive_var"));

    let (status, body) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/8/drive", r#"{"power_factor": 0.8}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("power_factor"));

    // power_factor out of (0.0, 1.0] is a 400 naming the range.
    let (status, body) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/7/drive", r#"{"power_factor": 1.5}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("(0.0, 1.0]"));

    // leading without power_factor is an invalid request too.
    let (status, body) = call(
        cfg,
        post_json("/api/mg/2200/component/7/drive", r#"{"leading": true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("leading"));
}

/// `reactive_var` and `power_factor` drive the same Q slot, so sending
/// both is a 400 — not a 200 where the second silently overwrites the
/// first. Validate-first: the meter's existing Q override is untouched.
#[tokio::test]
async fn drive_op_rejects_reactive_var_with_power_factor() {
    let cfg = config_with("(%make-meter :id 7 :power 8000.0)").await;

    // Land a Q override first, so a silent overwrite would be visible.
    let (status, _) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/7/drive",
            r#"{"reactive_var": 500.0}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/7/drive",
            r#"{"reactive_var": 100.0, "power_factor": 0.8}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let err = parsed["error"].as_str().unwrap();
    assert!(err.contains("reactive_var"), "{err}");
    assert!(err.contains("power_factor"), "{err}");

    // Nothing applied: the earlier override still stands.
    let m = cfg.site().get(7).unwrap();
    assert!((m.aggregate_reactive_var(&cfg.site()) - 500.0).abs() < 1e-3);
}

/// Status changes parse-then-apply: a valid health lands on the
/// component's runtime, a bad value is a 400 and changes nothing.
#[tokio::test]
async fn control_status_flips_health_and_rejects_bad_values() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, _) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/7/status", r#"{"health": "error"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        cfg.site().runtime_of(7).health,
        crate::sim::runtime::Health::Error
    );

    // A bad enum value: 400, and the health is untouched.
    let (status, body) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/7/status", r#"{"health": "broken"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("health"));
    assert_eq!(
        cfg.site().runtime_of(7).health,
        crate::sim::runtime::Health::Error
    );

    // A request with one bad field applies nothing (parse-then-apply).
    let (status, _) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/7/status",
            r#"{"health": "ok", "command_mode": "nonsense"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        cfg.site().runtime_of(7).health,
        crate::sim::runtime::Health::Error
    );
}

/// `health=error` forces the command channel shut; an explicit
/// `command_mode=normal` in the same request must not re-open it.
#[tokio::test]
async fn control_status_health_error_forbids_command_normal() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, body) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/7/status",
            r#"{"health": "error", "command_mode": "normal"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("health=error"));
    // Nothing applied: health is still the default.
    assert_eq!(
        cfg.site().runtime_of(7).health,
        crate::sim::runtime::Health::Ok
    );
}

/// The per-microgrid variants resolve the mg first: an unregistered
/// microgrid is a 404 before the component is even looked at.
#[tokio::test]
async fn control_for_mg_requires_a_registered_microgrid() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, body) = call(
        cfg,
        post_json("/api/mg/33/component/7/drive", r#"{"power_w": 1.0}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("microgrid 33"));
}

/// Driving a battery's SoC teleports its state: the test can arrange a
/// nearly-empty or nearly-full pool without simulating the charge.
#[tokio::test]
async fn control_drive_sets_battery_soc() {
    let cfg = config_with("(%make-battery :id 4 :initial-soc 60.0)").await;
    let (status, _) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/4/drive", r#"{"soc_pct": 11.5}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let site = cfg.site();
    let soc = site.get(4).unwrap().telemetry(&site).soc_pct.unwrap();
    assert!((soc - 11.5).abs() < 1e-3, "{soc}");

    // Out-of-range values clamp instead of corrupting the state.
    let (status, _) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/4/drive", r#"{"soc_pct": 250.0}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let soc = site.get(4).unwrap().telemetry(&site).soc_pct.unwrap();
    assert!((soc - 100.0).abs() < 1e-3, "{soc}");
}

/// A drive stimulus that does not apply to the component's category is
/// a 400 with the reason — never a silent 200 no-op. The matching
/// stimulus on the right category still lands (sunlight covered here).
#[tokio::test]
async fn control_drive_rejects_wrong_category() {
    let cfg = config_with(
        "(%make-meter :id 7)
         (%make-solar-inverter :id 8)",
    )
    .await;

    // Sunlight on a meter: rejected.
    let (status, body) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/7/drive",
            r#"{"sunlight_pct": 80.0}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["error"].as_str().unwrap().contains("sunlight_pct"));

    // SoC on a meter: rejected.
    let (status, _) = call(
        cfg.clone(),
        post_json("/api/mg/2200/component/7/drive", r#"{"soc_pct": 50.0}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Sunlight on the solar inverter: applies.
    let (status, _) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/8/drive",
            r#"{"sunlight_pct": 25.0}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // An unknown field name is a client error too
    // (deny_unknown_fields -> axum's 422 naming the field), not a
    // silently ignored typo.
    let (status, body) = call(
        cfg,
        post_json("/api/mg/2200/component/7/drive", r#"{"powr_w": 1.0}"#),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(error_of(&body).contains("powr_w"), "{}", error_of(&body));
}

/// Battery topology used by the formula tests:
/// grid → meter → battery-inverter → battery, mg id 2200.
const FORMULA_TOPOLOGY: &str = r#"(%make-grid-connection-point :id 1
     :successors
     (list (%make-meter :id 2
             :successors
             (list (%make-battery-inverter :id 3
                     :successors
                     (list (%make-battery :id 4)))))))"#;

#[tokio::test]
async fn formula_endpoint_returns_formula() {
    let cfg = config_with(FORMULA_TOPOLOGY).await;
    let (status, body) = call(cfg, get("/api/mg/2200/formula?metric=battery")).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["metric"], "battery");
    assert!(parsed.get("ok").is_none(), "body: {parsed}");
    // Just the rendered string now — parsing/highlighting live
    // client-side in formula-ast.js, so ast/explanation/commented no
    // longer ride along.
    assert!(parsed["formula"].as_str().unwrap().contains('#'));
    assert!(parsed.get("ast").is_none());
    assert!(parsed.get("explanation").is_none());
    assert!(parsed.get("commented").is_none());
}

#[tokio::test]
async fn formula_endpoint_rejects_unknown_metric_and_bad_ids() {
    let cfg = config_with(FORMULA_TOPOLOGY).await;
    let (status, body) = call(cfg.clone(), get("/api/mg/2200/formula?metric=bogus")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_of(&body).contains("bogus"));

    let (status, body) = call(cfg, get("/api/mg/2200/formula?metric=battery&ids=1,x")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("bad component id")
    );
}

#[tokio::test]
async fn formula_endpoint_reports_error_kind_for_missing_component() {
    let cfg = config_with(FORMULA_TOPOLOGY).await;
    let (status, body) = call(cfg, get("/api/mg/2200/formula?metric=battery&ids=99")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["kind"], "component_not_found");
}

#[tokio::test]
async fn formula_endpoint_honors_allow_unconnected() {
    // The same battery topology, plus a meter nobody connects to.
    let cfg = config_with(
        r#"(progn
             (%make-meter :id 9)
             (%make-grid-connection-point :id 1
               :successors
               (list (%make-meter :id 2
                       :successors
                       (list (%make-battery-inverter :id 3
                               :successors
                               (list (%make-battery :id 4))))))))"#,
    )
    .await;
    // Default config: the unconnected meter makes the graph invalid.
    let (status, body) = call(cfg.clone(), get("/api/mg/2200/formula?metric=battery")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body:?}");
    // With the flag the graph builds and the formula comes back.
    let (status, body) = call(
        cfg,
        get("/api/mg/2200/formula?metric=battery&allow_unconnected=true"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(parsed["formula"].as_str().unwrap().contains('#'));
}

#[tokio::test]
async fn formula_endpoint_404s_unknown_microgrid() {
    let cfg = config_with(FORMULA_TOPOLOGY).await;
    let (status, _) = call(cfg, get("/api/mg/9999/formula?metric=grid")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn microgrids_import_creates_entry_and_managed_file() {
    let cfg = config_with("(%make-grid-connection-point :id 1)").await;
    let body = r#"{
      "name": "imported site",
      "components": {"electricalComponents": [
        {"id": "10", "name": "grid", "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT",
         "categorySpecificInfo": {"gridConnectionPoint": {"ratedFuseCurrent": 125}}},
        {"id": "11", "category": "ELECTRICAL_COMPONENT_CATEGORY_METER"},
        {"id": "12", "category": "ELECTRICAL_COMPONENT_CATEGORY_INVERTER",
         "categorySpecificInfo": {"inverter": {"type": "INVERTER_TYPE_BATTERY"}}},
        {"id": "13", "category": "ELECTRICAL_COMPONENT_CATEGORY_BATTERY",
         "metricConfigBounds": [
           {"metric": "METRIC_BATTERY_CAPACITY", "configBounds": {"upper": 40000}}
         ]}
      ]},
      "connections": {"electricalComponentConnections": [
        {"sourceElectricalComponentId": "10", "destinationElectricalComponentId": "11"},
        {"sourceElectricalComponentId": "11", "destinationElectricalComponentId": "12"},
        {"sourceElectricalComponentId": "12", "destinationElectricalComponentId": "13"}
      ]}
    }"#;
    let (status, resp) = call(cfg.clone(), post_json("/api/microgrids/import", body)).await;
    let parsed: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    assert_eq!(status, StatusCode::OK, "body: {parsed}");
    assert_eq!(parsed["components"], 4);
    assert_eq!(parsed["connections"], 3);
    let id = parsed["id"].as_u64().unwrap();

    // The import populates the new site through the per-mg eval
    // path, so the components exist right away…
    let (status, topo) = call(cfg.clone(), get(&format!("/api/mg/{id}/topology"))).await;
    assert_eq!(status, StatusCode::OK);
    let topo: serde_json::Value = serde_json::from_slice(&topo).unwrap();
    assert_eq!(topo["components"].as_array().unwrap().len(), 4);
    assert_eq!(topo["connections"].as_array().unwrap().len(), 3);
    // …and the eval regenerated the managed file, with the export's
    // physical parameters, so the next boot loads them back.
    let saved = std::fs::read_to_string(cfg.microgrids_dir().join(format!("{id}.lisp"))).unwrap();
    assert!(saved.contains(":rated-fuse-current 125"), "{saved}");
    assert!(saved.contains("(%make-battery :id 13"), "{saved}");
    assert!(saved.contains(":capacity 40000.0"), "{saved}");
    assert!(saved.contains("(connect 12 13)"), "{saved}");

    // The registry lists it.
    let (_, list) = call(cfg, get("/api/microgrids")).await;
    let list: serde_json::Value = serde_json::from_slice(&list).unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"].as_u64() == Some(id) && m["name"] == "imported site")
    );
}

/// The whole persistence contract end to end: a microgrid created
/// from the UI, populated through the per-mg eval path, comes back
/// with the same ids and values in a brand-new `Config` booted on
/// the same state directory — no journal, no manual save.
///
/// Auto-allocated ids are part of that contract: a component created
/// without an explicit `:id` must come back under the id it was
/// given, not under a freshly minted one. The generated block pins
/// every id explicitly for exactly this reason.
#[tokio::test]
async fn ui_created_microgrid_survives_a_restart() {
    let (config, dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    // Create via the endpoint, add components via scoped eval.
    let (st, body) = call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"persist me"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let id = created["id"].as_u64().unwrap();
    let (st, _) = call(
        config.clone(),
        post(
            &format!("/api/mg/{id}/eval"),
            "(%make-grid-connection-point :id 300 :successors (list (%make-meter :id 301 :power 250.0)))",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // One more component, this time with NO explicit :id — the
    // allocator picks one, and that pick has to survive the restart.
    let (st, _) = call(
        config.clone(),
        post(&format!("/api/mg/{id}/eval"), "(%make-meter :power 75.0)"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let auto_id = {
        let reg = config.microgrids();
        let r = reg.lock();
        let mut ids: Vec<u64> = r[&id].site.components().iter().map(|c| c.id()).collect();
        ids.retain(|i| *i != 300 && *i != 301);
        assert_eq!(ids.len(), 1, "exactly one auto-allocated component");
        ids[0]
    };

    // "Restart": a brand-new Config on the same state dir, loading the file.
    let file = dir.join(format!("microgrids/{id}.lisp"));
    let cfg2 = Config::new_with(&[file.to_string_lossy().into_owned()], Some(dir.clone())).unwrap();
    let reg = cfg2.microgrids();
    let r = reg.lock();
    let e = r.get(&id).expect("microgrid survives the restart");
    assert_eq!(e.def.name, "persist me");
    assert!(
        e.site.get(300).is_some() && e.site.get(301).is_some(),
        "identical component ids"
    );
    assert!((e.site.get(301).unwrap().aggregate_power_w(&e.site) - 250.0).abs() < 1e-3);
    assert!(
        e.site.get(auto_id).is_some(),
        "the auto-allocated id {auto_id} must not be re-minted on replay",
    );
}

#[tokio::test]
async fn microgrids_import_rejects_id_collisions_atomically() {
    // Component id 1 already exists in the config's microgrid.
    let cfg = config_with("(%make-grid-connection-point :id 1)").await;
    let body = r#"{
      "name": "colliding site",
      "components": {"electricalComponents": [
        {"id": "1", "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT"}
      ]}
    }"#;
    let (status, resp) = call(cfg.clone(), post_json("/api/microgrids/import", body)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(error_of(&resp).contains("enterprise-unique"));
    // Nothing was created.
    let (_, list) = call(cfg, get("/api/microgrids")).await;
    let list: serde_json::Value = serde_json::from_slice(&list).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn microgrids_import_serializes_racing_imports() {
    // Two imports carrying the same component id race. The import
    // lock runs them one at a time, so the loser's collision scan
    // sees the winner's components and returns 409 — component ids
    // stay enterprise-unique, with no silent duplicate.
    let cfg = config_with("(%make-grid-connection-point :id 1)").await;
    let body = |name: &str| {
        format!(
            r#"{{
      "name": "{name}",
      "components": {{"electricalComponents": [
        {{"id": "40", "category": "ELECTRICAL_COMPONENT_CATEGORY_GRID_CONNECTION_POINT"}}
      ]}}
    }}"#
        )
    };
    let (a, b) = tokio::join!(
        call(
            cfg.clone(),
            post_json("/api/microgrids/import", &body("site a"))
        ),
        call(
            cfg.clone(),
            post_json("/api/microgrids/import", &body("site b"))
        ),
    );
    let statuses = [a.0, b.0];
    assert!(statuses.contains(&StatusCode::OK), "{statuses:?}");
    assert!(statuses.contains(&StatusCode::CONFLICT), "{statuses:?}");
    // Exactly one import landed next to the config's own microgrid.
    let (_, list) = call(cfg, get("/api/microgrids")).await;
    let list: serde_json::Value = serde_json::from_slice(&list).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn microgrids_import_rejects_unsupported_category() {
    let cfg = config_with("(%make-grid-connection-point :id 1)").await;
    let body = r#"{
      "name": "hvac site",
      "components": {"electricalComponents": [
        {"id": "10", "category": "ELECTRICAL_COMPONENT_CATEGORY_HVAC"}
      ]}
    }"#;
    let (status, resp) = call(cfg, post_json("/api/microgrids/import", body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_of(&resp).contains("cannot simulate"));
}

/// set-component-operational-mode is a CONFIG change: the runtime
/// knobs derive from it and the site enforces them.
#[tokio::test]
async fn operational_mode_eval_derives_and_is_enforced() {
    let cfg = config_with("(%make-grid-connection-point :id 1)").await;
    let (status, body) = call(
        cfg.clone(),
        post(
            "/api/eval",
            "(set-component-operational-mode 1 'control-only)",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");

    // Derived: the topology snapshot shows the mode and the silenced
    // stream.
    let (_, body) = call(cfg.clone(), get("/api/mg/2200/topology")).await;
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let c = &parsed["components"][0];
    assert_eq!(c["operational_mode"], "control-only");
    assert_eq!(c["telemetry_mode"], "silent");
    // The capability booleans the inspect panel keys its knob
    // gating off — derived server-side so the rule stays in Rust.
    assert_eq!(c["provides_telemetry"], false);
    assert_eq!(c["accepts_control"], true);

    // Enforced: poking the stream back to normal is rejected while
    // the mode forbids telemetry.
    let (status, body) = call(
        cfg,
        post("/api/eval", "(set-component-telemetry-mode 1 'normal)"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_of(&body).contains("streams no telemetry"));
}

/// The create lock is held until a create's load ends, not until its
/// handler ends: a client that goes away drops the handler future
/// mid-load, and the load runs on in the blocking pool.
#[tokio::test]
async fn create_lock_outlives_a_dropped_handler_until_the_load_ends() {
    let config =
        config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let cfg = config.clone();
    let handler = tokio::spawn(async move {
        super::handlers::blocking_under_create_lock(&cfg, move |_| {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv();
        })
        .await
    });
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());
    assert!(
        config.create_lock().try_lock().is_err(),
        "the lock must stay held while the load runs on"
    );
    release_tx.send(()).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        config.create_lock().lock_owned(),
    )
    .await
    .expect("the lock frees once the load ends");
}

/// Loading a file whose microgrid id is already live is refused with
/// a 409 that names the collision and suggests a free id — the load
/// picker turns that into a "load as N" button, which lands the same
/// file a second time under the free id.
#[tokio::test]
async fn load_endpoint_offers_load_as_on_collision() {
    let (config, dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let text = crate::lisp::microgrid_file::compose(
        "(make-microgrid :id 9 :name \"dup\" :grpc-port 8890\n  :topology\n  (lambda ()\n    nil))",
        "",
    );
    std::fs::write(dir.join("dup.lisp"), &text).unwrap();
    let (st, body) = call(
        config.clone(),
        post_json("/api/load", r#"{"path":"dup.lisp"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"], "microgrid 9 is already loaded");
    assert_eq!(v["collision_id"], 9);
    assert_eq!(v["managed"], true);
    let suggested = v["suggested_id"].as_u64().unwrap();
    let (st, _) = call(
        config.clone(),
        post_json(
            "/api/load-as",
            &format!(r#"{{"path":"dup.lisp","id":{suggested}}}"#),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(config.microgrids().lock().contains_key(&suggested));
}

/// A load-as whose generated block registered but whose script
/// section then failed is a COMMITTED partial: the copy is live and
/// its file is kept. Reporting that as a 409 would read as the
/// collision code the caller just answered and invite a retry, which
/// then hits "target exists" or mints a second copy. It is a 200
/// carrying the id and a warning instead.
#[tokio::test]
async fn load_as_reports_a_committed_partial_as_a_warning_not_a_conflict() {
    let (config, dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let text = crate::lisp::microgrid_file::compose(
        "(make-microgrid :id 40 :name \"p\" :grpc-port 8840\n  :topology\n  \
         (lambda ()\n    (%make-meter :id 410)))",
        "(set-meter-power 999999 1.0)\n",
    );
    std::fs::write(dir.join("partial.lisp"), &text).unwrap();
    let (st, body) = call(
        config.clone(),
        post_json("/api/load-as", r#"{"path":"partial.lisp","id":41}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], 41, "the caller still learns the id");
    let warning = v["warning"].as_str().expect("a warning is carried");
    assert!(warning.contains("999999"), "{warning}");
    assert!(
        warning.contains("script section"),
        "the warning says what half failed: {warning}"
    );
    assert!(config.microgrids().lock().contains_key(&41));
    assert!(dir.join("microgrids/41.lisp").exists(), "the copy is kept");
}

/// A load-as claims an id and a port and writes a file, the same
/// window a create has, so it waits on the create lock: a create in
/// flight finishes before the copy picks its port.
#[tokio::test]
async fn load_as_waits_for_a_create_in_flight() {
    let (config, dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let text = crate::lisp::microgrid_file::compose(
        "(make-microgrid :id 44 :name \"w\" :grpc-port 8844\n  :topology\n  (lambda ()\n    nil))",
        "",
    );
    std::fs::write(dir.join("wait.lisp"), &text).unwrap();
    let create_lock = config.create_lock();
    let held = create_lock.lock().await;
    let pending = tokio::spawn(call(
        config.clone(),
        post_json("/api/load-as", r#"{"path":"wait.lisp","id":45}"#),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !config.microgrids().lock().contains_key(&45),
        "the copy must not load while a create holds the lock"
    );
    drop(held);
    let (st, body) = pending.await.unwrap();
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(config.microgrids().lock().contains_key(&45));
}

/// Loading a snapshot as a new microgrid runs the same load-as, so it
/// waits on the create lock too.
#[tokio::test]
async fn snapshot_load_as_waits_for_a_create_in_flight() {
    let (config, _dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"s","id":46}"#),
    )
    .await;
    let (st, _) = call(
        config.clone(),
        post_json("/api/mg/46/snapshots", r#"{"name":"one"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let create_lock = config.create_lock();
    let held = create_lock.lock().await;
    let pending = tokio::spawn(call(
        config.clone(),
        post_json("/api/mg/46/snapshots/load", r#"{"name":"one","as_id":47}"#),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !config.microgrids().lock().contains_key(&47),
        "the copy must not load while a create holds the lock"
    );
    drop(held);
    let (st, body) = pending.await.unwrap();
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(config.microgrids().lock().contains_key(&47));
}

/// The committed-partial 200 must come from load_as KNOWING it
/// copied and registered, not from asking the registry afterwards
/// who backs the target. Retrying a load-as that already succeeded
/// fails early — before anything is copied — and the target file it
/// would have written is exactly the one the previous, successful
/// call left backing a live microgrid. A registry query cannot tell
/// those apart and would answer a copied-nothing call with
/// "the copy loaded but its script section failed".
#[tokio::test]
async fn a_repeated_load_as_is_an_error_not_a_fabricated_warning() {
    let (config, dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let text = crate::lisp::microgrid_file::compose(
        "(make-microgrid :id 42 :name \"ok\" :grpc-port 8842\n  :topology\n  \
         (lambda ()\n    (%make-meter :id 420)))",
        "",
    );
    std::fs::write(dir.join("good.lisp"), &text).unwrap();
    let body = r#"{"path":"good.lisp","id":43}"#;
    let (st, _) = call(config.clone(), post_json("/api/load-as", body)).await;
    assert_eq!(st, StatusCode::OK, "the first copy lands cleanly");

    // Same id again: nothing is copied, so this is a plain refusal.
    let (st, body) = call(config.clone(), post_json("/api/load-as", body)).await;
    assert!(
        st.is_client_error(),
        "a call that copied nothing must not report success: {st} {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        !error_of(&body).contains("script section"),
        "and must not claim a script section failed: {}",
        error_of(&body)
    );
}

/// Undo walks back one structural edit of a managed microgrid — the
/// file is rewritten from the previous generated block and reloaded —
/// and redo walks forward again.
#[tokio::test]
async fn undo_reverts_the_last_structural_edit() {
    let (config, _dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let (st, body) = call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"u","id":30}"#),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    call(
        config.clone(),
        post("/api/mg/30/eval", "(%make-meter :id 500)"),
    )
    .await;
    call(
        config.clone(),
        post("/api/mg/30/eval", "(%make-meter :id 501)"),
    )
    .await;
    let (st, _) = call(config.clone(), post("/api/mg/30/undo", "")).await;
    assert_eq!(st, StatusCode::OK);
    let site = config.microgrids().lock().get(&30).unwrap().site.clone();
    assert!(
        site.get(500).is_some() && site.get(501).is_none(),
        "one step undone"
    );
    let (st, _) = call(config.clone(), post("/api/mg/30/redo", "")).await;
    assert_eq!(st, StatusCode::OK);
    let site = config.microgrids().lock().get(&30).unwrap().site.clone();
    assert!(site.get(501).is_some(), "redo restores");
}

/// Snapshots are per microgrid: they live under `snapshots/{id}/`,
/// copy that microgrid's own file, and restore it in place. The
/// ambient (whole-world) snapshot endpoints are gone.
#[tokio::test]
async fn snapshots_are_per_microgrid() {
    let (config, dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"s","id":31}"#),
    )
    .await;
    call(
        config.clone(),
        post("/api/mg/31/eval", "(%make-meter :id 600)"),
    )
    .await;
    let (st, body) = call(
        config.clone(),
        post_json("/api/mg/31/snapshots", r#"{"name":"one"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v["path"].is_string(), "{v}");
    assert!(v.get("ok").is_none(), "{v}");
    assert!(dir.join("snapshots/31/one.lisp").exists());
    call(
        config.clone(),
        post("/api/mg/31/eval", "(remove-component 600)"),
    )
    .await;
    let (st, body) = call(
        config.clone(),
        post_json("/api/mg/31/snapshots/load", r#"{"name":"one"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], 31, "{v}");
    assert!(v.get("ok").is_none(), "{v}");
    let site = config.microgrids().lock().get(&31).unwrap().site.clone();
    assert!(site.get(600).is_some(), "restore brings the meter back");
    // The ambient endpoint is gone.
    let (st, _) = call(config.clone(), get("/api/snapshots")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // Loading the same snapshot `as_id` lands it BESIDE the original.
    // It runs through load_as, so it inherits the fresh component
    // ids: microgrid 31 still holds meter 600, and the copy holds an
    // equivalent meter under an id of its own.
    let (st, body) = call(
        config.clone(),
        post_json("/api/mg/31/snapshots/load", r#"{"name":"one","as_id":32}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let registry = config.microgrids();
    let reg = registry.lock();
    assert!(
        reg.get(&31).unwrap().site.get(600).is_some(),
        "the original keeps its component"
    );
    let copied: Vec<u64> = reg
        .get(&32)
        .expect("the copy registered")
        .site
        .components()
        .iter()
        .map(|c| c.id())
        .collect();
    assert_eq!(copied.len(), 1, "the copy carries the snapshot's topology");
    assert_ne!(copied[0], 600, "under a component id of its own");
}

/// Adopt takes a hand-written single-microgrid file over: the live
/// structure is written as a generated block and the original form is
/// commented out, so later structural edits regenerate the file.
#[tokio::test]
async fn adopt_makes_an_unmanaged_single_mg_file_managed() {
    let (config, dir) = config_with_dir(
        "(make-microgrid :id 9 :grpc-port 8800 :topology \
                                         (lambda () (%make-meter :id 700 :power 100.0)))",
    )
    .await;
    let (st, body) = call(config.clone(), post("/api/mg/9/adopt", "")).await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let text = std::fs::read_to_string(dir.join("config.lisp")).unwrap();
    assert!(text.starts_with(";;; macrocosim:generated"));
    assert!(
        text.contains(";; (make-microgrid"),
        "original form commented out: {text}"
    );
    // Managed now: a structural edit rewrites the file.
    call(
        config.clone(),
        post("/api/mg/9/eval", "(%make-meter :id 701)"),
    )
    .await;
    let text = std::fs::read_to_string(dir.join("config.lisp")).unwrap();
    assert!(text.contains("(%make-meter :id 701"));
}

/// Two creates racing must end up as two microgrids, not one: the
/// create lock keeps each one's id + port claim valid until the file
/// it wrote has been loaded and its entry is in the registry.
#[tokio::test]
async fn concurrent_creates_get_distinct_microgrids() {
    let (config, _dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let (a, b) = tokio::join!(
        call(
            config.clone(),
            post_json("/api/microgrids", r#"{"name":"a"}"#)
        ),
        call(
            config.clone(),
            post_json("/api/microgrids", r#"{"name":"b"}"#)
        ),
    );
    assert_eq!(
        a.0,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&a.1)
    );
    assert_eq!(
        b.0,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&b.1)
    );
    let id_of = |body: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(body).unwrap()["id"]
            .as_u64()
            .unwrap()
    };
    assert_ne!(id_of(&a.1), id_of(&b.1), "each create gets its own id");
    assert_eq!(config.microgrids().lock().len(), 3);
}

/// An explicit port on the assets or dispatch server's port is
/// refused, as another microgrid's would be.
#[tokio::test]
async fn create_refuses_a_reserved_port() {
    let config = config_with("nil").await;
    let ports = [config.assets_socket_addr(), config.dispatch_socket_addr()]
        .map(|a| a.parse::<std::net::SocketAddr>().unwrap().port());
    for port in ports {
        let (st, body) = call(
            config.clone(),
            post_json(
                "/api/microgrids",
                &format!(r#"{{"name":"r","grpc_port":{port}}}"#),
            ),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        let error = error_of(&body);
        assert!(error.contains("reserved"), "unexpected error: {error}");
    }
}

/// Create takes an explicit id and port, and refuses either when it
/// is already claimed — the create dialog turns that into an inline
/// error rather than quietly picking something else.
#[tokio::test]
async fn create_refuses_a_taken_id_or_port() {
    let (config, _dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    let (st, body) = call(
        config.clone(),
        post_json(
            "/api/microgrids",
            r#"{"name":"pinned","id":40,"grpc_port":8899}"#,
        ),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], 40);
    assert_eq!(v["grpc_port"], 8899);
    assert_eq!(v["managed"], true);

    let (st, _) = call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"again","id":40}"#),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "id 40 is taken");
    let (st, _) = call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"again","grpc_port":8899}"#),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "port 8899 is taken");
}

/// With inert runtimes, nothing is started and the list says so.
#[tokio::test]
async fn the_list_reports_no_runtime_under_inert_runtimes() {
    let config = config_with("nil").await;
    let (st, body) = call(config, get("/api/microgrids")).await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(v[0].as_object().unwrap().contains_key("runtime"), "{v}");
    assert!(v[0]["runtime"].is_null(), "{v}");
}

/// Runtimes that really bind, on `[::1]`.
fn live_runtimes(config: &Config, ephemeral_ports: bool) -> crate::runtime::MicrogridRuntimes {
    crate::runtime::MicrogridRuntimes::new(
        config.clone(),
        crate::runtime::RuntimeOptions {
            ephemeral_ports,
            bind_host: std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        },
    )
    .unwrap()
}

/// Microgrid `id`'s entry in `/api/microgrids`.
async fn listed(
    config: Config,
    runtimes: crate::runtime::MicrogridRuntimes,
    id: u64,
) -> serde_json::Value {
    let (_, list) = call_with(config, runtimes, get("/api/microgrids")).await;
    let list: serde_json::Value = serde_json::from_slice(&list).unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .cloned()
        .unwrap()
}

/// A created microgrid reports the address its server bound, in the
/// create answer and in the list.
#[tokio::test]
async fn create_reports_the_bound_address() {
    let config = config_with("nil").await;
    let runtimes = live_runtimes(&config, true);
    let (st, body) = call_with(
        config.clone(),
        runtimes.clone(),
        post_json("/api/microgrids", r#"{"name":"r","id":61}"#),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["runtime"]["status"], "running");
    let addr = v["runtime"]["grpc_addr"].as_str().unwrap().to_string();
    assert!(
        !addr.ends_with(&format!(":{}", v["grpc_port"])),
        "ephemeral, not configured: {addr}"
    );
    let entry = listed(config, runtimes, 61).await;
    assert_eq!(entry["runtime"]["grpc_addr"], addr.as_str());
}

/// A create whose port is held answers 201 with the failure, and the
/// list shows it failed.
#[tokio::test]
async fn create_on_a_held_port_reports_a_failed_runtime() {
    let holder = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)).unwrap();
    let port = holder.local_addr().unwrap().port();
    let config = config_with("nil").await;
    let runtimes = live_runtimes(&config, false);
    let (st, body) = call_with(
        config.clone(),
        runtimes.clone(),
        post_json(
            "/api/microgrids",
            &format!(r#"{{"name":"h","id":62,"grpc_port":{port}}}"#),
        ),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["runtime"]["status"], "failed");
    let entry = listed(config, runtimes, 62).await;
    assert_eq!(entry["runtime"]["status"], "failed");
    drop(holder);
}

/// Adopt rewrites a whole file from ONE microgrid's live state, so a
/// file declaring two microgrids is refused instead of losing one.
#[tokio::test]
async fn adopt_refuses_a_file_declaring_two_microgrids() {
    let (config, _dir) = config_with_dir(
        "(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))\n\
         (make-microgrid :id 10 :grpc-port 8810 :topology (lambda () nil))",
    )
    .await;
    let (st, body) = call(config.clone(), post("/api/mg/9/adopt", "")).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert!(
        error_of(&body).contains("split the file first"),
        "{}",
        error_of(&body)
    );
    assert!(!config.microgrids().lock().get(&9).unwrap().managed);
}

/// Undo depths are readable without taking a step, and both stacks
/// stay empty for a microgrid nothing has edited.
#[tokio::test]
async fn undo_depths_track_edits() {
    let (config, _dir) =
        config_with_dir("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"d","id":32}"#),
    )
    .await;
    let (st, body) = call(config.clone(), get("/api/mg/32/undo")).await;
    assert_eq!(st, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["undo_depth"], 0);
    // Nothing to undo yet.
    let (st, _) = call(config.clone(), post("/api/mg/32/undo", "")).await;
    assert_eq!(st, StatusCode::CONFLICT);
    call(
        config.clone(),
        post("/api/mg/32/eval", "(%make-meter :id 800)"),
    )
    .await;
    let (_, body) = call(config.clone(), get("/api/mg/32/undo")).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["undo_depth"], 1);
    assert_eq!(v["redo_depth"], 0);
    // An unmanaged microgrid has no history to walk.
    let (st, _) = call(config.clone(), post("/api/mg/9/undo", "")).await;
    assert_eq!(st, StatusCode::CONFLICT);
}

// ─── same-origin guard + body limits ──────────────────────────────

/// Browser-shaped request: `Host` plus, optionally, `Origin` — the
/// header pair the same-origin guard keys on. Requests built by the
/// plain `get`/`post` helpers carry neither, which is the
/// non-browser client shape the guard waves through.
fn browser_post(path: &str, host: &str, origin: Option<&str>, body: &str) -> Request<Body> {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", host);
    if let Some(o) = origin {
        b = b.header("origin", o);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn origin_guard_passes_same_origin_browser_requests() {
    let config = config_with("").await;
    let (st, _) = call(
        config.clone(),
        browser_post(
            "/api/eval",
            "localhost:8801",
            Some("http://localhost:8801"),
            "(+ 1 2)",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // Bracketed IPv6 authority — the `http://[::1]:8801` shape.
    let (st, _) = call(
        config.clone(),
        browser_post(
            "/api/eval",
            "[::1]:8801",
            Some("http://[::1]:8801"),
            "(+ 1 2)",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // Loopback spellings browsers resolve without DNS: *.localhost,
    // trailing-dot FQDNs, and non-.1 addresses in 127.0.0.0/8.
    for host in ["app.localhost:8801", "localhost.:8801", "127.0.0.5:8801"] {
        let origin = format!("http://{host}");
        let (st, _) = call(
            config.clone(),
            browser_post("/api/eval", host, Some(&origin), "(+ 1 2)"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{host}");
    }
}

#[tokio::test]
async fn origin_guard_rejects_foreign_origin_and_rebound_host() {
    let config = config_with("").await;
    // Cross-origin POST — text/plain needs no CORS preflight, and
    // executing it would run attacker Lisp, so the guard must reject
    // before routing.
    let (st, _) = call(
        config.clone(),
        browser_post(
            "/api/eval",
            "localhost:8801",
            Some("http://evil.example:8801"),
            "(+ 1 2)",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // `Origin: null` (sandboxed iframe, file://) is not same-origin.
    let (st, _) = call(
        config.clone(),
        browser_post("/api/eval", "localhost:8801", Some("null"), "(+ 1 2)"),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // DNS rebinding: same-origin from the browser's point of view,
    // but the foreign hostname survives in `Host`.
    let (st, _) = call(
        config,
        browser_post("/api/eval", "evil.example:8801", None, "(+ 1 2)"),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn eval_body_is_capped_by_the_default_limit() {
    // Pin axum's stock 2 MB `DefaultBodyLimit` on /api/eval so a
    // future extractor or layer reshuffle can't silently drop the
    // cap on the code-execution endpoint.
    let config = config_with("").await;
    let (st, body) = call(config, post("/api/eval", &"x".repeat(3 * 1024 * 1024))).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!error_of(&body).is_empty());
}

/// A body that is not UTF-8 is rejected with axum's status and a
/// JSON error.
#[tokio::test]
async fn a_non_utf8_eval_body_is_a_json_error() {
    let config = config_with("").await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/eval")
        .body(Body::from(vec![0xff, 0xfe, 0xfd]))
        .unwrap();
    let (st, body) = call(config, req).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(!error_of(&body).is_empty());
}

#[tokio::test]
async fn setpoints_resolve_per_microgrid() {
    use crate::sim::setpoints::{SetpointEvent, SetpointKind, SetpointOutcome};
    // Two microgrids: 9 and 31.
    let config =
        config_with("(make-microgrid :id 9 :grpc-port 8800 :topology (lambda () nil))").await;
    call(
        config.clone(),
        post_json("/api/microgrids", r#"{"name":"s","id":31}"#),
    )
    .await;
    // Plant one event per microgrid, each under its own component id,
    // so the assertions can tell WHICH site each endpoint answered
    // from — an existence check alone would pass even if every route
    // read the same site.
    let ev = || SetpointEvent {
        ts: Utc::now(),
        kind: SetpointKind::ActivePower,
        value: 1234.0,
        ttl_s: Some(60),
        outcome: SetpointOutcome::Accepted {
            effective_value: Some(1234.0),
        },
    };
    let site_of = |mg: u64| {
        config
            .microgrids()
            .lock()
            .get(&mg)
            .unwrap_or_else(|| panic!("microgrid {mg} registered"))
            .site
            .clone()
    };
    site_of(31).log_setpoint(600, ev());
    site_of(9).log_setpoint(500, ev());
    let events_at = |path: &str| {
        let config = config.clone();
        let path = path.to_string();
        async move {
            let (st, body) = call(config, get(&path)).await;
            assert_eq!(st, StatusCode::OK, "{path}");
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
            v["events"].as_array().unwrap().len()
        }
    };
    assert_eq!(events_at("/api/mg/31/component/600/setpoints").await, 1);
    assert_eq!(events_at("/api/mg/9/component/600/setpoints").await, 0);
    assert_eq!(events_at("/api/mg/9/component/500/setpoints").await, 1);
    assert_eq!(events_at("/api/mg/31/component/500/setpoints").await, 0);
    let (st, _) = call(config, get("/api/mg/9999/component/600/setpoints")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// A meter's `meter-power` knob reads back the live constant
/// override, and a `set-meter-power-factor` call swaps its Q source
/// from `meter-reactive-power` to `meter-power-factor` (with
/// `leading`) — the two are mutually exclusive readings of the same
/// underlying reactive source.
#[tokio::test]
async fn component_snapshot_reads_meter_knobs_and_envelope() {
    let cfg = config_with("(%make-meter :id 7)").await;
    call(cfg.clone(), post("/api/eval", "(set-meter-power 7 1500)")).await;
    call(
        cfg.clone(),
        post("/api/eval", "(set-meter-power-factor 7 0.9 t)"),
    )
    .await;
    let (status, body) = call(cfg, get("/api/mg/2200/component/7")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], 7);
    let knobs = v["knobs"].as_array().unwrap();
    let power = knobs.iter().find(|k| k["knob"] == "meter-power").unwrap();
    assert_eq!(power["value"], 1500.0);
    assert!(power["expr"].is_null());
    let pf = knobs
        .iter()
        .find(|k| k["knob"] == "meter-power-factor")
        .unwrap();
    assert_eq!(pf["value"], 0.9);
    assert_eq!(pf["leading"], true);
    // The meter never had a direct VAr source configured, so the
    // knob list shouldn't carry a stale `meter-reactive-power` entry
    // alongside the power-factor one.
    assert!(!knobs.iter().any(|k| k["knob"] == "meter-reactive-power"));
}

/// A quoted or symbol source prints its Lisp form readably in `expr`.
#[tokio::test]
async fn component_snapshot_prints_expression_sources() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, _) = call(
        cfg.clone(),
        post("/api/eval", "(set-meter-power 7 '(lambda () 25))"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    cfg.refresh_once();
    let (_s, body) = call(cfg, get("/api/mg/2200/component/7")).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let power = v["knobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["knob"] == "meter-power")
        .unwrap()
        .clone();
    assert!(power["expr"].as_str().unwrap().contains("lambda"));
}

/// An unquoted `(lambda …)` literal evaluates to tulisp's opaque
/// `CompiledDefun` closure before `set-meter-power` ever sees it —
/// its `source_text()` is the literal, useless string "CompiledDefun".
/// The handler ships that raw opaque string unfiltered (no server-
/// side normalization — see `handlers/component.rs`); it's the
/// client's `knobDisplay` (`ui-assets/inspect.js`) that detects the
/// `CompiledDefun` marker and swaps in a placeholder, the one place
/// both the snapshot and WS paths funnel through. The live resolved
/// `value` still reflects the lambda's result either way.
#[tokio::test]
async fn component_snapshot_ships_raw_compiled_defun_expr() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, _) = call(
        cfg.clone(),
        post("/api/eval", "(set-meter-power 7 (lambda () 25))"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    cfg.refresh_once();
    let (_s, body) = call(cfg, get("/api/mg/2200/component/7")).await;
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let power = v["knobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["knob"] == "meter-power")
        .unwrap()
        .clone();
    assert!(
        power["expr"].as_str().unwrap().starts_with("CompiledDefun"),
        "expr: {:?}",
        power["expr"]
    );
    assert_eq!(power["value"], 25.0);
}

#[tokio::test]
async fn component_snapshot_404s_unknown_ids() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, _b) = call(cfg.clone(), get("/api/mg/2200/component/99")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _b) = call(cfg, get("/api/mg/9999/component/7")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A solar inverter's knob set is sunlight + both reactive caps
/// (mirroring the client's `KNOBS_BY_CATEGORY` solar rule), and the
/// unset `reactive-apparent-va` cap still gets an entry with
/// `value: null` so the client renders the input. `envelope.reactive`
/// needs a downstream component reporting a Q band to populate (see
/// `Gateway::child_envelope`) — a battery inverter wired as a
/// (topologically nonsensical, but type-legal) child gives it one
/// without dragging in a whole battery rig.
#[tokio::test]
async fn component_snapshot_inverter_knobs_and_reactive_envelope() {
    let cfg = config_with(
        "(%make-solar-inverter :id 4)
         (%make-battery-inverter :id 5)
         (connect 4 5)",
    )
    .await;
    call(cfg.clone(), post("/api/eval", "(set-solar-sunlight 4 63)")).await;
    let (status, _) = call(
        cfg.clone(),
        post("/api/eval", "(set-reactive-pf-limit 4 0.95)"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(cfg, get("/api/mg/2200/component/4")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let knobs = v["knobs"].as_array().unwrap();
    let sunlight = knobs
        .iter()
        .find(|k| k["knob"] == "solar-sunlight")
        .unwrap();
    assert_eq!(sunlight["value"], 63.0);
    let pf_limit = knobs
        .iter()
        .find(|k| k["knob"] == "reactive-pf-limit")
        .unwrap();
    assert_eq!(pf_limit["value"], 0.95);
    let apparent_va = knobs
        .iter()
        .find(|k| k["knob"] == "reactive-apparent-va")
        .unwrap();
    assert!(apparent_va["value"].is_null());
    assert!(!v["envelope"]["reactive"].is_null());
}

/// A battery inverter's own reactive capability must populate
/// `envelope.reactive` even when its only child is a battery, which
/// (correctly) exposes no Q bounds of its own — Q terminates at the
/// inverter. The gateway's setpoint envelope falls back to the
/// inverter's own band when no child reports one; returning nothing
/// there would clobber the WS-fed graduation the inspector already
/// draws to null on every snapshot re-fetch (every accepted setpoint)
/// — the once-a-second flicker this test guards against.
#[tokio::test]
async fn component_snapshot_reactive_envelope_falls_back_to_own_bounds() {
    let cfg = config_with(
        "(%make-battery-inverter :id 3
           :successors (list (%make-battery :id 4)))",
    )
    .await;
    let (status, _) = call(
        cfg.clone(),
        // Clear the microsim-default PF cap (±35% of |P|) first — at
        // this inverter's idle P = 0 that cap alone pins Q to (0, 0),
        // masking the fallback this test targets. The kVA cap alone
        // gives the full ±VA band at P = 0.
        post("/api/eval", "(set-reactive-pf-limit 3 0)"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        cfg.clone(),
        post("/api/eval", "(set-reactive-apparent-va 3 3000.0)"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(cfg, get("/api/mg/2200/component/3")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let reactive = &v["envelope"]["reactive"];
    assert!(!reactive.is_null(), "{v}");
    let bounds = reactive.as_array().unwrap();
    let lo = bounds[0].as_f64().unwrap();
    let hi = bounds[1].as_f64().unwrap();
    assert!(
        lo < 0.0 && 0.0 < hi,
        "expected lo < 0 < hi, got ({lo}, {hi})"
    );
}

/// `setpoints[axis].remaining_ms` reflects the live `TimeoutTracker`
/// deadline armed by `(set-active-power … LIFETIME-MS)`, bounded by
/// the lifetime just requested. The value/axis themselves come from
/// the separate setpoint-event log (`log_setpoint`), which
/// `(set-active-power)` doesn't populate on its own — planted here
/// the same way `setpoints_resolve_per_microgrid` does.
#[tokio::test]
async fn component_snapshot_reports_remaining_ms_for_a_timed_setpoint() {
    use crate::sim::setpoints::{SetpointEvent, SetpointKind, SetpointOutcome};
    let cfg = config_with("(%make-solar-inverter :id 4)").await;
    let lifetime_ms: u64 = 5000;
    let (status, _) = call(
        cfg.clone(),
        post("/api/eval", "(set-active-power 4 -5000 5000)"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    cfg.legacy_site().log_setpoint(
        4,
        SetpointEvent {
            ts: Utc::now(),
            kind: SetpointKind::ActivePower,
            value: -5000.0,
            ttl_s: Some(5),
            outcome: SetpointOutcome::Accepted {
                effective_value: Some(-5000.0),
            },
        },
    );
    let (status, body) = call(cfg, get("/api/mg/2200/component/4")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sp = v["setpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["axis"] == "active")
        .unwrap();
    assert_eq!(sp["value"], -5000.0);
    let remaining = sp["remaining_ms"].as_u64().unwrap();
    assert!(
        remaining > 0 && remaining <= lifetime_ms,
        "remaining={remaining}"
    );
}

/// A meter driven directly via `(set-meter-reactive-power id V)` (as
/// opposed to `component_snapshot_reads_meter_knobs_and_envelope`'s
/// power-factor-derived reading) exercises the `ReactiveReading::Var`
/// read-back arm in `knobs_for` — the `meter-reactive-power` knob
/// shows up with the constant value just set, no `expr`.
#[tokio::test]
async fn component_snapshot_reads_reactive_var_knob() {
    let cfg = config_with("(%make-meter :id 7)").await;
    let (status, _) = call(
        cfg.clone(),
        post("/api/eval", "(set-meter-reactive-power 7 1250)"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(cfg, get("/api/mg/2200/component/7")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let reactive = v["knobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["knob"] == "meter-reactive-power")
        .unwrap();
    assert_eq!(reactive["value"], 1250.0);
    assert!(reactive["expr"].is_null());
}

/// The typed control API's drive endpoint is a second door onto the
/// same setters the Lisp defuns use (src/lisp/defuns/load_drivers.rs)
/// — it must broadcast `KnobChanged` on the same success path, so a
/// live UI inspector tab refreshes its edit-in-place input regardless
/// of which door the write came through. Same event-bus assertion
/// shape as `set_meter_power_broadcasts_knob_changed` in
/// `lisp/defuns/load_drivers.rs`, driven over HTTP instead of `eval`.
#[tokio::test]
async fn control_drive_broadcasts_knob_changed() {
    use crate::sim::events::SiteEvent;

    let cfg = config_with("(%make-meter :id 7)").await;
    let mut rx = cfg.site().subscribe_events();
    let (status, _) = call(
        cfg,
        post_json("/api/mg/2200/component/7/drive", r#"{"power_w": 1234.5}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let mut seen = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        seen.push(ev);
    }
    assert!(
        seen.iter().any(|ev| matches!(
            ev,
            SiteEvent::KnobChanged { id: 7, knob: "meter-power", value: Some(v), expr: None, .. }
                if (*v - 1234.5).abs() < 1e-6
        )),
        "no matching KnobChanged on the bus; saw: {seen:?}"
    );
}

/// `clear_sunlight: true` on the typed drive door must broadcast the
/// "weather" marker in `expr`, not `None` — otherwise the instant
/// after a clear, the inspector renders a bare, markerless number
/// (the opposite of the `Follow` mode the clear just installed) until
/// the component is re-selected. Mirrors
/// `clear_solar_sunlight_returns_to_following_weather` in
/// `lisp/defuns/load_drivers.rs`, driven over HTTP instead of `eval`.
#[tokio::test]
async fn control_drive_clear_sunlight_broadcasts_weather_marker() {
    use crate::sim::events::SiteEvent;

    let cfg = config_with("(%make-solar-inverter :id 8 :sunlight% 40)").await;
    call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/8/drive",
            r#"{"sunlight_pct": 10.0}"#,
        ),
    )
    .await;

    let mut rx = cfg.site().subscribe_events();
    let (status, _) = call(
        cfg,
        post_json(
            "/api/mg/2200/component/8/drive",
            r#"{"clear_sunlight": true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let mut seen = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        seen.push(ev);
    }
    assert!(
        seen.iter().any(|ev| matches!(
            ev,
            SiteEvent::KnobChanged {
                id: 8,
                knob: "solar-sunlight",
                expr: Some(e),
                ..
            } if e == "weather"
        )),
        "no matching KnobChanged with the weather marker on the bus; saw: {seen:?}"
    );
}

/// The HTTP twin of
/// `scenario_stop_restores_a_constructed_sunlight_kwarg_a_clear_dropped`
/// in `lisp/defuns/load_drivers.rs`: `clear_sunlight` over the drive
/// door is a user-intent verb, so inside a running scenario it must
/// still snapshot the knob BEFORE clearing it. The clear is the run's
/// first touch of that knob, so the Manual 40 the inverter was built
/// with can only come back from the snapshot the clear itself took.
#[tokio::test]
async fn drive_clear_sunlight_inside_a_scenario_restores_on_stop() {
    let cfg = config_with("(%make-solar-inverter :id 8 :sunlight% 40)").await;
    let knob = |body: Vec<u8>| -> serde_json::Value {
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        v["knobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["knob"] == "solar-sunlight")
            .unwrap_or_else(|| panic!("no solar-sunlight knob: {v}"))
            .clone()
    };

    let (_, body) = call(cfg.clone(), get("/api/mg/2200/component/8")).await;
    let before = knob(body);
    assert_eq!(before["value"], 40.0, "{before}");
    assert!(before["expr"].is_null(), "constructed Manual: {before}");

    call(
        cfg.clone(),
        post("/api/eval", "(scenario-start \"clear-sun\")"),
    )
    .await;
    let (status, _) = call(
        cfg.clone(),
        post_json(
            "/api/mg/2200/component/8/drive",
            r#"{"clear_sunlight": true}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(cfg.clone(), get("/api/mg/2200/component/8")).await;
    let during = knob(body);
    assert_eq!(
        during["expr"], "weather",
        "the clear must really clear while the scenario runs: {during}"
    );

    call(cfg.clone(), post("/api/eval", "(scenario-stop)")).await;
    let (_, body) = call(cfg, get("/api/mg/2200/component/8")).await;
    let after = knob(body);
    assert_eq!(after["value"], 40.0, "{after}");
    assert!(
        after["expr"].is_null(),
        "back to the constructed Manual 40, not left following: {after}"
    );
}

/// A steam boiler's inspector snapshot lists its demand and pressure
/// knobs and its thermostat target.
#[tokio::test]
async fn component_snapshot_boiler_knobs_and_pressure_target() {
    let cfg = config_with("(%make-steam-boiler :id 6 :target-bar 8.0)").await;
    let (status, _) = call(cfg.clone(), post("/api/eval", "(set-boiler-demand 6 40)")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(cfg.clone(), post("/api/eval", "(set-boiler-pressure 6 9)")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(cfg, get("/api/mg/2200/component/6")).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let knobs = v["knobs"].as_array().unwrap();
    let names: Vec<&str> = knobs.iter().map(|k| k["knob"].as_str().unwrap()).collect();
    assert_eq!(names, ["boiler-demand", "boiler-pressure"]);
    assert_eq!(knobs[0]["value"], 40.0);
    assert_eq!(knobs[1]["value"], 9.0);
    assert_eq!(v["pressure_target_bar"], 8.0);
}

/// Each component in a mixed topology lists exactly the knobs of the
/// capabilities it has, in order, and one with no knob capability
/// lists none.
#[tokio::test]
async fn component_snapshot_lists_exactly_the_knobs_each_kind_has() {
    let cfg = config_with(
        "(%make-grid-connection-point :id 1
           :successors
           (list (%make-meter :id 2 :power 1000.0 :reactive-power 200.0)
                 (%make-meter :id 3 :power 1000.0 :power-factor 0.9)
                 (%make-solar-inverter :id 4)
                 (%make-battery-inverter :id 5
                   :successors (list (%make-battery :id 6)))
                 (%make-ev-charger :id 7)))",
    )
    .await;
    let want: [(u64, &[&str]); 7] = [
        (1, &[]),
        (2, &["meter-power", "meter-reactive-power"]),
        (3, &["meter-power", "meter-power-factor"]),
        (
            4,
            &[
                "solar-sunlight",
                "reactive-pf-limit",
                "reactive-apparent-va",
            ],
        ),
        (5, &["reactive-pf-limit", "reactive-apparent-va"]),
        (6, &[]),
        (7, &[]),
    ];
    for (id, names) in want {
        let (status, body) = call(cfg.clone(), get(&format!("/api/mg/2200/component/{id}"))).await;
        assert_eq!(status, StatusCode::OK, "component {id}");
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let got: Vec<&str> = v["knobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k["knob"].as_str().unwrap())
            .collect();
        assert_eq!(got, names, "component {id}");
    }
}

/// An unknown path answers the JSON 404, naming the method and path.
#[tokio::test]
async fn an_unknown_path_is_a_json_404() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_of(&body), "no route for GET /api/nope");
}

/// A wrong method on a known path answers the JSON 405.
#[tokio::test]
async fn a_wrong_method_is_a_json_405() {
    let cfg = config_with("").await;
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/microgrids/import")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(cfg, req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(error_of(&body), "no route for GET /api/microgrids/import");
}

/// A malformed JSON body is a 400 whose error carries axum's
/// rejection text.
#[tokio::test]
async fn a_malformed_json_body_is_a_json_400() {
    let cfg = config_with("").await;
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/mg/2200/component/1/drive")
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let (status, body) = call(cfg, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!error_of(&body).is_empty());
}

/// A missing required query parameter is a JSON 400.
#[tokio::test]
async fn a_missing_query_parameter_is_a_json_400() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/mg/2200/formula")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_of(&body).contains("metric"), "{}", error_of(&body));
}

/// The origin guard rejects with JSON and no trailing newline.
#[tokio::test]
async fn the_origin_guard_rejects_with_json() {
    let cfg = config_with("").await;
    let req = Request::builder()
        .uri("/api/microgrids")
        .header("host", "evil.example")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(cfg, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_of(&body), "non-loopback Host rejected");
}

/// A handler error keeps its message and comes back as JSON.
#[tokio::test]
async fn a_handler_error_is_json_with_its_old_text() {
    let cfg = config_with("").await;
    let (status, body) = call(cfg, get("/api/scripts?dir=..")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_of(&body), "invalid dir");
}
