//! End-to-end scenario integration. Drives a battery through a
//! known charge/discharge cycle, snapshots the report, and asserts
//! the peak / integral / SoC numbers match the analytical
//! expectation within tolerance. Exercises the same boot path the
//! sample examples/scenario-driving.lisp uses.

mod common;

use std::time::Duration;

use common::{TestServer, eval_or_panic};
use serde_json::Value;

const TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter
                   :id 2
                   :successors
                   (list (%make-battery-inverter
                          :id 4
                          :rated-lower-w -10000.0
                          :rated-upper-w  10000.0
                          :successors
                          (list (%make-battery
                                 :id 3
                                 :capacity-wh 100000.0
                                 :rated-lower-w -10000.0
                                 :rated-upper-w  10000.0)))))))
"#;

async fn report(client: &reqwest::Client, s: &TestServer) -> Value {
    client
        .get(s.mg_url("scenario/report"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn topology(client: &reqwest::Client, s: &TestServer) -> Value {
    client
        .get(s.mg_url("topology"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Poll the scenario report until `peak_grid_w` reaches `want`
/// within `tol`, then return it. The peak now rides the microgrid
/// loopback's `grid_power` formula stream, which resamples at ~1 Hz
/// off the gRPC telemetry — so a value driven through `/api/eval`
/// lands a beat later than the caller's manual snapshot, and the
/// assertion has to wait for it rather than read once. The peak is
/// monotonic within a run, so waiting can only ever turn a
/// not-yet-arrived value into the expected one.
async fn wait_for_peak(client: &reqwest::Client, s: &TestServer, want: f64, tol: f64) -> f64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let peak = report(client, s).await["peak_grid_w"].as_f64().unwrap();
        if (peak - want).abs() <= tol || tokio::time::Instant::now() >= deadline {
            return peak;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `(set-meter-power id (lambda () N))` round-trips through the
/// HTTP /api/eval boundary, the polymorphic Lisp setter installs
/// a DynamicScalar source on the meter, and the next physics tick
/// exposes the resolved value via the scenario report's grid peak
/// once the loopback resamples it. Mirrors what the dashboard does when a
/// scenario edits a curve from the side panel.
#[tokio::test(flavor = "multi_thread")]
async fn lambda_meter_power_resolves_through_http_eval() {
    let s = TestServer::start(TOPOLOGY).await;
    let client = reqwest::Client::new();

    eval_or_panic(&client, &s, "(scenario-start \"lambda\")").await;
    // Symbol form (the global mutates between snapshots — the
    // integration test asserts the deref happens each tick).
    eval_or_panic(&client, &s, "(setq curve 1234.5)").await;
    eval_or_panic(&client, &s, "(set-meter-power 2 'curve)").await;

    let mut now = chrono::Utc::now();
    s.config.refresh_once();
    s.config
        .site()
        .tick_once(now, std::time::Duration::from_millis(100));
    s.config.site().record_history_snapshot(now);

    let topo = topology(&client, &s).await;
    // Telemetry isn't on the topology route; assert via the report's
    // grid peak instead.
    let peak = wait_for_peak(&client, &s, 1234.5, 1.0).await;
    assert!(
        (peak - 1234.5).abs() < 1.0,
        "expected ~1234.5 W peak via symbol curve, got {peak} (topo {topo})",
    );

    // Mutate the global; the next snapshot picks up the new value.
    eval_or_panic(&client, &s, "(setq curve 4321.0)").await;
    now += chrono::Duration::seconds(1);
    s.config.refresh_once();
    s.config
        .site()
        .tick_once(now, std::time::Duration::from_millis(100));
    s.config.site().record_history_snapshot(now);
    let peak = wait_for_peak(&client, &s, 4321.0, 1.0).await;
    assert!(
        (peak - 4321.0).abs() < 1.0,
        "expected ~4321.0 W peak after symbol mutation, got {peak}",
    );

    // Lambda form: replace the source with a thunk and confirm
    // the next snapshot resolves it.
    eval_or_panic(&client, &s, "(set-meter-power 2 (lambda () 9999.0))").await;
    now += chrono::Duration::seconds(1);
    s.config.refresh_once();
    s.config
        .site()
        .tick_once(now, std::time::Duration::from_millis(100));
    s.config.site().record_history_snapshot(now);
    let peak = wait_for_peak(&client, &s, 9999.0, 1.0).await;
    assert!(
        (peak - 9999.0).abs() < 1.0,
        "expected ~9999 W peak via lambda, got {peak}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn driver_run_aggregates_peak_charge_and_soc_stats() {
    let s = TestServer::start(TOPOLOGY).await;
    let client = reqwest::Client::new();

    // Start the scenario then push a known charge setpoint.
    for body in [
        "(scenario-start \"smoke\")",
        "(set-active-power 4 3600.0 60000)",
    ] {
        client
            .post(format!("{}/api/eval", s.ui_url))
            .body(body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    // Drive the simulation deterministically: tick + snapshot at
    // explicit timestamps. 10 sim-seconds at +3600 W = 10 Wh charged
    // into the battery. Two short ticks carry the command across the
    // gateway and device delays before the measured window starts.
    let mut now = chrono::Utc::now();
    for _ in 0..2 {
        now += chrono::Duration::milliseconds(100);
        s.config.site().tick_once(now, Duration::from_millis(100));
    }
    s.config.site().record_history_snapshot(now);
    now += chrono::Duration::seconds(10);
    s.config.site().tick_once(now, Duration::from_secs(10));
    s.config.site().record_history_snapshot(now);

    let r = report(&client, &s).await;

    // Battery energy charged ≈ 10 Wh. Tolerance for the seed
    // sample dt at scenario_start.
    let charged = r["total_battery_charged_wh"].as_f64().unwrap();
    assert!(
        (8.0..=12.0).contains(&charged),
        "expected ~10 Wh charged, got {charged}",
    );
    let discharged = r["total_battery_discharged_wh"].as_f64().unwrap();
    assert_eq!(
        discharged, 0.0,
        "no discharge expected on a charge-only run, got {discharged}",
    );

    // SoC stats reflect the single battery's current SoC. Default
    // initial-soc on a battery is 50 % per BatteryConfig::default,
    // and 10 Wh into a 100000 Wh capacity is +0.01 % — still ≈ 50.
    let soc = &r["soc_stats"];
    assert!(!soc.is_null(), "soc_stats missing");
    let mean = soc["mean_pct"].as_f64().unwrap();
    assert!(
        (45.0..=55.0).contains(&mean),
        "expected mean SoC ≈ 50, got {mean}",
    );

    // per_battery and per_pv shapes.
    let per_battery = r["per_battery"].as_array().unwrap();
    assert_eq!(per_battery.len(), 1);
    assert_eq!(per_battery[0]["id"], 3);

    // Peak through the grid connection point — the inverter is
    // publishing 3600 W up the stack. Unlike everything above it, this
    // is not read off `r`: the peak rides the loopback's ~1 Hz
    // grid_power stream and can lag the already-captured snapshot, so
    // it is polled fresh. Asserted last only so a failure in the
    // deterministic energy/SoC checks surfaces without first burning
    // the poll deadline — `r` is frozen and cannot drift while we wait.
    let peak = wait_for_peak(&client, &s, 3600.0, 400.0).await;
    assert!(
        (3000.0..=4000.0).contains(&peak),
        "expected ~3600 W peak, got {peak}",
    );

    // Stop freezes elapsed.
    client
        .post(format!("{}/api/eval", s.ui_url))
        .body("(scenario-stop)")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let frozen = report(&client, &s).await;
    let frozen_elapsed = frozen["scenario_elapsed_s"].as_f64().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let later = report(&client, &s).await;
    assert_eq!(
        frozen_elapsed,
        later["scenario_elapsed_s"].as_f64().unwrap(),
        "elapsed should freeze after scenario-stop",
    );
}

const BOILER_TOPOLOGY: &str = r#"(%make-steam-boiler :id 9)"#;

/// `(drive-boiler-kg-per-s ID SOURCE)` compiles through `scenario--drive` to
/// `set-boiler-demand-kg-per-s`, the same path `drive-solar` takes to
/// `set-solar-sunlight` — asserts the compiled item lands on the
/// boiler's demand knob, read back via `demand_reading()`.
#[tokio::test(flavor = "multi_thread")]
async fn drive_boiler_sets_demand_via_scenario_compile() {
    let s = TestServer::start(BOILER_TOPOLOGY).await;
    let client = reqwest::Client::new();

    eval_or_panic(&client, &s, "(scenario-start \"boiler-drive\")").await;
    eval_or_panic(
        &client,
        &s,
        "(scenario--drive (drive-boiler-kg-per-s 9 0.5))",
    )
    .await;

    let r = s
        .config
        .site()
        .get(9)
        .expect("boiler component")
        .steam_drive()
        .expect("demand reading")
        .demand_reading();
    assert_eq!(r.value, 0.5, "drive-boiler should set constant demand");
}

/// The dedicated `POST /api/scenarios/stop` HTTP door must restore
/// driven knobs exactly like `(scenario-stop)` does from Lisp — both
/// paths go through the same `scenario-stop` Rust defun
/// (`scenarios_stop`'s handler evals `"(scenario-stop)"`), but this
/// pins the actual REST endpoint the Scenarios UI's Stop button
/// calls, guarding against a future refactor that routes it straight
/// to `MicrogridSite::scenario_stop` and skips the knob-restore this
/// defun wires in. Only knob-restore is asserted here — the same
/// refactor would also skip timer-cancellation, but that half is
/// pinned separately (no agent/timer is involved in this test) by
/// `boot::tests::scenario_stop_cancels_the_agent_timer_so_it_stops_firing`,
/// which exercises the defun directly.
#[tokio::test(flavor = "multi_thread")]
async fn http_scenarios_stop_endpoint_restores_a_driven_knob() {
    let s = TestServer::start(TOPOLOGY).await;
    let client = reqwest::Client::new();

    eval_or_panic(&client, &s, "(scenario-start \"http-stop\")").await;
    eval_or_panic(&client, &s, "(set-meter-power 2 9000.0)").await;
    assert_eq!(
        s.config
            .site()
            .get(2)
            .unwrap()
            .meter_drive()
            .unwrap()
            .meter_power_reading()
            .unwrap()
            .value,
        9000.0,
        "sanity: the drive landed before we stop"
    );

    let resp = client
        .post(format!("{}/api/scenarios/stop", s.ui_url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::NO_CONTENT,
        "POST /api/scenarios/stop"
    );

    assert!(
        s.config
            .site()
            .get(2)
            .unwrap()
            .meter_drive()
            .unwrap()
            .meter_power_reading()
            .is_none(),
        "the dedicated /api/scenarios/stop door must restore driven knobs, same as (scenario-stop)"
    );
}
