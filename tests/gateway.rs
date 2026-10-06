//! End-to-end tests for the per-microgrid gateway on a headless
//! config: request lifetimes and augmentations expire on sim time,
//! the SoC window holds a running setpoint, trips follow each
//! component's rules, and a command crosses the gateway and device
//! delays once.

use std::time::Duration;

mod common;

use common::{TestServer, first_bounds};
use macrocosim::proto::common::metrics::{Bounds, Metric};
use macrocosim::proto::microgrid::microgrid_client::MicrogridClient;
use macrocosim::proto::microgrid::{
    AugmentElectricalComponentBoundsRequest, PowerType, SetElectricalComponentPowerRequest,
};

use macrocosim::lisp::Config;
use macrocosim::timeout_tracker::SetpointAxis;

/// A headless config built from `body` inside microgrid 9, and the
/// temp dir that holds it.
fn headless(body: &str) -> (Config, tempfile::TempDir) {
    let dir = tempfile::TempDir::with_prefix("macrocosim-gateway-").expect("temp dir");
    let path = dir.path().join("config.lisp");
    std::fs::write(
        &path,
        format!("(make-microgrid :id 9 :grpc-port 18990 :topology (lambda () {body}))"),
    )
    .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (cfg, _clock) = rt
        .block_on(async { Config::new_headless(path.to_str().unwrap()) })
        .expect("headless config builds");
    std::mem::forget(rt);
    (cfg, dir)
}

fn num(cfg: &Config, expr: &str) -> f64 {
    cfg.eval_silent(expr).unwrap().parse().unwrap()
}

fn soc(cfg: &Config, id: u64) -> f32 {
    let site = cfg.site();
    site.get(id).unwrap().telemetry(&site).soc_pct.unwrap()
}

const TICK: Duration = Duration::from_millis(100);

const INVERTER_AND_BATTERY: &str =
    "(%make-battery-inverter :id 3 :rated-lower-w -5000.0 :rated-upper-w 5000.0
   :successors (list (%make-battery :id 4 :rated-lower-w -5000.0 :rated-upper-w 5000.0)))";

/// A setpoint and an augmentation both lapse after their lifetime of
/// sim time, though almost no wall time passes.
#[test]
fn headless_setpoint_and_augmentation_expire_on_sim_time() {
    let (cfg, _dir) = headless(INVERTER_AND_BATTERY);
    cfg.eval_silent("(set-active-power 3 2000.0 10000)")
        .unwrap();
    cfg.sim_run(Duration::from_secs(5), TICK);
    assert!((num(&cfg, "(component-active-power 3)") - 2000.0).abs() < 1.0);
    cfg.sim_run(Duration::from_secs(6), TICK);
    assert!(
        num(&cfg, "(component-active-power 3)").abs() < 1.0,
        "expired on sim time"
    );

    cfg.eval_silent("(augment-active-bounds 3 '(-1000 1000) 10000)")
        .unwrap();
    assert!((num(&cfg, "(component-bound-upper 3)") - 1000.0).abs() < 1e-3);
    cfg.sim_run(Duration::from_secs(5), TICK);
    assert!(
        (num(&cfg, "(component-bound-upper 3)") - 1000.0).abs() < 1e-3,
        "live at 5 s"
    );
    cfg.sim_run(Duration::from_secs(6), TICK);
    assert!(
        (num(&cfg, "(component-bound-upper 3)") - 5000.0).abs() < 1e-3,
        "expired on sim time"
    );
}

const SMALL_PACK: &str =
    "(%make-battery-inverter :id 3 :rated-lower-w -5000.0 :rated-upper-w 5000.0
   :successors (list (%make-battery :id 4 :capacity-wh 1000.0 :initial-soc-pct 80.0
                                    :soc-upper-pct 90.0 :soc-protect-margin-pct 5.0
                                    :rated-lower-w -5000.0 :rated-upper-w 5000.0)))";

/// Charging toward `:soc-upper-pct`, the inverter's output tapers inside
/// the margin and stops at the window, within one device delay; the
/// command stands throughout.
#[test]
fn the_soc_window_tapers_and_holds_a_running_setpoint() {
    let (cfg, _dir) = headless(SMALL_PACK);
    cfg.eval_silent("(set-active-power 3 3600.0 900000)")
        .unwrap();
    // 3.6 kW into 1 kWh moves the SoC 0.1 % per second.
    for _ in 0..1_000 {
        if soc(&cfg, 4) >= 87.0 {
            break;
        }
        cfg.sim_step(TICK);
    }
    let p = num(&cfg, "(component-active-power 3)");
    assert!(
        p > 0.0 && p < 3_500.0,
        "tapering inside the margin, got {p}"
    );

    cfg.sim_run(Duration::from_secs(120), TICK);
    let s = soc(&cfg, 4);
    assert!(
        s <= 90.03,
        "the window holds within one device delay, got {s}"
    );
    assert!(s >= 89.9, "charged up to the window, got {s}");
    assert!(num(&cfg, "(component-active-power 3)").abs() < 1.0);
    let site = cfg.site();
    assert!(
        site.gateway()
            .remaining_lifetime(3, SetpointAxis::Active)
            .is_some()
    );
}

/// Driving the hardware directly, with no gateway in the path, the
/// same pack charges all the way to 100 % and stops there.
#[test]
fn the_hardware_alone_charges_to_full() {
    let (cfg, _dir) = headless(SMALL_PACK);
    let site = cfg.site();
    let inv = site.get(3).unwrap();
    let ctl = inv.controllable().unwrap();
    let mut now = site.now();
    for _ in 0..3_000 {
        now += chrono::Duration::milliseconds(100);
        ctl.set_command(SetpointAxis::Active, 3_600.0);
        for c in site.components().iter() {
            c.tick(&site, now, TICK);
        }
    }
    assert!((soc(&cfg, 4) - 100.0).abs() < 1e-3);
    assert!(site.get(4).unwrap().aggregate_power_w(&site).abs() < 1.0);
    assert!(
        inv.aggregate_power_w(&site).abs() < 1.0,
        "the inverter's accept ratio reaches 0 at the hard limit"
    );
}

/// Trips: the battery inverter loses its command; the PV inverter
/// keeps its curtailment and loses Q; the charger with
/// `:resume-on-recovery` keeps its command; nothing replays.
#[test]
fn trips_follow_each_components_rules() {
    let (cfg, _dir) = headless(
        "(%make-battery-inverter :id 3 :successors (list (%make-battery :id 4)))
         (%make-solar-inverter :id 5 :sunlight-pct 100.0 :rated-lower-w -10000.0 :rated-upper-w 0.0
                               :reactive-pf-limit 0 :reactive-apparent-va 10000.0
                               :reactive-command-delay-s 0)
         (%make-ev-charger :id 6 :command-delay-s 0 :resume-on-recovery t)",
    );
    for form in [
        "(set-active-power 3 3000.0 600000)",
        "(set-active-power 5 -4000.0 600000)",
        "(set-active-power 6 11000.0 600000)",
    ] {
        cfg.eval_silent(form).unwrap();
    }
    cfg.sim_run(Duration::from_secs(1), TICK);
    cfg.eval_silent("(set-reactive-power 5 2000.0 600000)")
        .unwrap();
    cfg.sim_run(Duration::from_secs(5), TICK);
    assert!((num(&cfg, "(component-active-power 3)") - 3000.0).abs() < 1.0);
    assert!((num(&cfg, "(component-active-power 5)") + 4000.0).abs() < 1.0);
    assert!((num(&cfg, "(component-reactive-power 5)") - 2000.0).abs() < 1.0);

    for id in [3, 5, 6] {
        cfg.eval_silent(&format!("(set-component-health {id} 'error)"))
            .unwrap();
    }
    cfg.sim_run(Duration::from_secs(1), TICK);
    assert!(num(&cfg, "(component-active-power 3)").abs() < 1.0);
    assert!(num(&cfg, "(component-active-power 5)").abs() < 1.0);
    assert!(num(&cfg, "(component-reactive-power 5)").abs() < 1.0);
    {
        let site = cfg.site();
        let gw = site.gateway();
        assert_eq!(gw.remaining_lifetime(3, SetpointAxis::Active), None);
        assert!(gw.remaining_lifetime(5, SetpointAxis::Active).is_some());
        assert_eq!(gw.remaining_lifetime(5, SetpointAxis::Reactive), None);
        assert!(gw.remaining_lifetime(6, SetpointAxis::Active).is_some());
    }

    for id in [3, 5, 6] {
        cfg.eval_silent(&format!("(set-component-health {id} 'ok)"))
            .unwrap();
    }
    cfg.sim_run(Duration::from_secs(2), TICK);
    assert!(
        num(&cfg, "(component-active-power 3)").abs() < 1.0,
        "no replay, awaits re-dispatch"
    );
    assert!(
        (num(&cfg, "(component-active-power 5)") + 4000.0).abs() < 1.0,
        "the curtailment resumes"
    );
    assert!(
        num(&cfg, "(component-reactive-power 5)").abs() < 1.0,
        "Q awaits re-dispatch"
    );
}

/// A command reaches the output after the gateway delay plus the
/// device delay, at any physics tick: with 300 ms + 200 ms it shows
/// on the 6th 100 ms tick and on the 4th 250 ms tick.
#[test]
fn a_command_crosses_the_gateway_and_device_delays_at_any_tick() {
    for (tick_ms, quiet) in [(100_u64, 5_usize), (250, 3)] {
        let (cfg, _dir) = headless(
            "(%make-battery-inverter :id 3 :command-delay-s 0.3 :device-delay-s 0.2
               :successors (list (%make-battery :id 4)))",
        );
        let dt = Duration::from_millis(tick_ms);
        cfg.eval_silent("(set-active-power 3 3000.0 600000)")
            .unwrap();
        for _ in 0..quiet {
            cfg.sim_step(dt);
        }
        assert!(
            num(&cfg, "(component-active-power 3)").abs() < 1.0,
            "{tick_ms} ms ticks: too early"
        );
        cfg.sim_step(dt);
        assert!(
            (num(&cfg, "(component-active-power 3)") - 3000.0).abs() < 1.0,
            "{tick_ms} ms ticks: on time"
        );
    }
}

/// With no gateway in the path, an empty pack discharges to 0 % and
/// the inverter's output stops there.
#[test]
fn the_hardware_alone_stops_discharging_at_empty() {
    let (cfg, _dir) =
        headless(&SMALL_PACK.replace(":initial-soc-pct 80.0", ":initial-soc-pct 20.0"));
    let site = cfg.site();
    let inv = site.get(3).unwrap();
    let ctl = inv.controllable().unwrap();
    let mut now = site.now();
    for _ in 0..3_000 {
        now += chrono::Duration::milliseconds(100);
        ctl.set_command(SetpointAxis::Active, -3_600.0);
        for c in site.components().iter() {
            c.tick(&site, now, TICK);
        }
    }
    assert!(soc(&cfg, 4).abs() < 1e-3);
    assert!(site.get(4).unwrap().aggregate_power_w(&site).abs() < 1.0);
    assert!(inv.aggregate_power_w(&site).abs() < 1.0);
}

/// PV output follows the sun: a drop in sunlight cuts the output
/// within one device delay, a return climbs at the gateway ramp rate.
#[test]
fn pv_output_follows_sunlight_cuts_at_once_and_climbs_at_the_ramp() {
    let (cfg, _dir) = headless(
        "(%make-solar-inverter :id 5 :sunlight-pct 100.0 :rated-lower-w -10000.0 :rated-upper-w 0.0
                               :array-peak-w 10000.0 :ramp-rate-w-per-s 1000.0)",
    );
    cfg.sim_run(Duration::from_secs(30), TICK);
    let full = num(&cfg, "(component-active-power 5)");
    assert!(
        (full + 10_000.0).abs() < 1.0,
        "steady in full sun, got {full}"
    );

    cfg.eval_silent("(set-solar-sunlight 5 50)").unwrap();
    cfg.sim_run(Duration::from_millis(300), TICK);
    let cut = num(&cfg, "(component-active-power 5)");
    assert!(
        (cut + 5_000.0).abs() < 1.0,
        "cut at once, not ramped, got {cut}"
    );

    cfg.eval_silent("(set-solar-sunlight 5 100)").unwrap();
    cfg.sim_run(Duration::from_millis(1_000), TICK);
    let mid = num(&cfg, "(component-active-power 5)");
    assert!(mid < -5_000.0 && mid > -10_000.0, "climbing, got {mid}");
    assert!(
        (mid + 6_000.0).abs() < 300.0,
        "about 1000 W/s after 1 s, got {mid}"
    );
    cfg.sim_run(Duration::from_secs(10), TICK);
    assert!((num(&cfg, "(component-active-power 5)") + 10_000.0).abs() < 1.0);
}

/// Narrowing the apparent-power limit at runtime shrinks the live Q
/// envelope and turns a Q request that used to fit into a rejection.
#[test]
fn changing_the_reactive_limit_at_runtime_moves_the_q_envelope() {
    let (cfg, _dir) = headless(
        "(%make-battery-inverter :id 3 :reactive-pf-limit 0 :reactive-apparent-va 10000.0
           :successors (list (%make-battery :id 4)))",
    );
    cfg.sim_run(Duration::from_secs(1), TICK);
    cfg.eval_silent("(set-reactive-power 3 8000.0 600000)")
        .unwrap();
    let before = cfg.site().bounds_of(3, SetpointAxis::Reactive).unwrap();

    cfg.eval_silent("(set-reactive-apparent-va 3 5000.0)")
        .unwrap();
    cfg.sim_run(Duration::from_secs(1), TICK);
    let after = cfg.site().bounds_of(3, SetpointAxis::Reactive).unwrap();
    assert_ne!(before.to_string(), after.to_string(), "envelope moved");
    let err = cfg
        .eval_silent("(set-reactive-power 3 8000.0 600000)")
        .expect_err("8 kVAr no longer fits")
        .to_string();
    assert!(
        err.contains("8000 VAr out of bounds"),
        "wrong rejection, envelope {after}: {err}"
    );
    cfg.eval_silent("(set-reactive-power 3 4000.0 600000)")
        .unwrap();
}

/// A reactive command already standing is pulled in when the apparent
/// power limit narrows under it, and resumes when the limit returns.
#[test]
fn a_standing_q_command_follows_a_narrowing_apparent_power_limit() {
    let (cfg, _dir) = headless(
        "(%make-battery-inverter :id 3 :reactive-pf-limit 0 :reactive-apparent-va 10000.0
           :successors (list (%make-battery :id 4)))",
    );
    cfg.sim_run(Duration::from_secs(1), TICK);
    cfg.eval_silent("(set-reactive-power 3 8000.0 600000)")
        .unwrap();
    // 100 ms delay, then 2000 VAr/s: 8 kVAr takes about 4 s.
    cfg.sim_run(Duration::from_secs(6), TICK);
    assert!((num(&cfg, "(component-reactive-power 3)") - 8_000.0).abs() < 1.0);

    cfg.eval_silent("(set-reactive-apparent-va 3 5000.0)")
        .unwrap();
    cfg.sim_run(Duration::from_secs(2), TICK);
    let q = num(&cfg, "(component-reactive-power 3)");
    assert!(
        q <= 5_001.0 && q > 4_000.0,
        "pulled in, not cleared, got {q}"
    );
    let site = cfg.site();
    assert!(
        site.gateway()
            .remaining_lifetime(3, SetpointAxis::Reactive)
            .is_some()
    );
    drop(site);

    cfg.eval_silent("(set-reactive-apparent-va 3 10000.0)")
        .unwrap();
    cfg.sim_run(Duration::from_secs(4), TICK);
    let q = num(&cfg, "(component-reactive-power 3)");
    assert!((q - 8_000.0).abs() < 1.0, "back to the command, got {q}");
}

const AGREEMENT_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
  :successors (list (%make-meter :id 2
    :successors (list
      (%make-battery-inverter :id 4 :rated-lower-w -10000.0 :rated-upper-w 10000.0
        :successors (list (%make-battery :id 3 :initial-soc-pct 87.5 :soc-upper-pct 90.0
                                         :soc-protect-margin-pct 5.0
                                         :rated-lower-w -10000.0 :rated-upper-w 10000.0)))
      (%make-steam-boiler :id 6 :demand-kg-per-s 0.027777778)))))
"#;

/// gRPC telemetry, `site.bounds_of`, Lisp `component-bound-upper`,
/// the history ring and the scenario bounds CSV report the same upper
/// edge for a throttled battery, an augmented inverter and a boiler.
#[tokio::test(flavor = "multi_thread")]
async fn bounds_reads_agree_across_every_consumer() {
    let s = TestServer::start(AGREEMENT_TOPOLOGY).await;
    let mut c = MicrogridClient::connect(s.grpc_url.clone()).await.unwrap();
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 4,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(-3000.0),
            upper: Some(3000.0),
        }],
        request_lifetime: Some(600),
    })
    .await
    .expect("augment ok");

    let csv_dir = s.config_path().parent().unwrap().join("csv");
    s.config
        .eval_silent(&format!("(scenario-record-csv \"{}\")", csv_dir.display()))
        .unwrap();
    let site = s.config.site();
    let now = chrono::Utc::now();
    site.record_history_snapshot(now);
    s.config.eval_silent("(scenario-stop-csv)").unwrap();

    for (id, metric) in [
        (3, Metric::DcPower),
        (4, Metric::AcPowerActive),
        (6, Metric::AcPowerActive),
    ] {
        let gateway = site
            .bounds_of(id, SetpointAxis::Active)
            .unwrap()
            .0
            .last()
            .unwrap()
            .upper
            .unwrap();
        let grpc = first_bounds(&mut c, id, metric)
            .await
            .last()
            .and_then(|b| b.upper)
            .expect("an upper edge");
        let lisp = num(&s.config, &format!("(component-bound-upper {id})")) as f32;
        let history = site
            .history_window(
                id,
                macrocosim::sim::history::Metric::ActivePowerUpperBoundW,
                now - chrono::Duration::seconds(1),
            )
            .unwrap()
            .last()
            .unwrap()
            .value;
        let csv = std::fs::read_to_string(csv_dir.join(format!("{id}-bounds.csv"))).unwrap();
        let csv: f32 = csv
            .lines()
            .last()
            .unwrap()
            .split(',')
            .nth(2)
            .unwrap()
            .parse()
            .unwrap();
        for (what, v) in [
            ("grpc", grpc),
            ("lisp", lisp),
            ("history", history),
            ("csv", csv),
        ] {
            assert!(
                (v - gateway).abs() < 1e-2,
                "component {id}: {what} {v} vs gateway {gateway}"
            );
        }
    }
    let battery = site.bounds_of(3, SetpointAxis::Active).unwrap().0[0]
        .upper
        .unwrap();
    assert!(
        battery > 0.0 && battery < 10_000.0,
        "the battery is throttled: {battery}"
    );
    assert_eq!(
        site.bounds_of(4, SetpointAxis::Active).unwrap().0[0].upper,
        Some(3000.0)
    );
    let boiler = site.bounds_of(6, SetpointAxis::Active).unwrap().0[0]
        .upper
        .unwrap();
    assert!(
        (boiler - 62_700.0).abs() < 1.0,
        "the boiler advertises its need: {boiler}"
    );
}

/// `valid_until` on a SetPower response is the gateway's deadline.
#[tokio::test(flavor = "multi_thread")]
async fn valid_until_is_the_gateways_deadline() {
    let s = TestServer::start(INVERTER_AND_BATTERY).await;
    let mut c = MicrogridClient::connect(s.grpc_url.clone()).await.unwrap();
    let mut stream = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 3,
            power: 1000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect("set-power ok")
        .into_inner();
    let ts = stream
        .message()
        .await
        .unwrap()
        .unwrap()
        .valid_until_time
        .expect("valid_until");
    let valid_until = chrono::DateTime::from_timestamp(ts.seconds, ts.nanos as u32).unwrap();
    let left = s
        .config
        .site()
        .gateway()
        .remaining_lifetime(3, SetpointAxis::Active)
        .expect("a live lifetime");
    let gateway_deadline = chrono::Utc::now() + chrono::Duration::from_std(left).unwrap();
    assert!(
        (valid_until - gateway_deadline).num_milliseconds().abs() < 1000,
        "{valid_until} vs {gateway_deadline}"
    );
}
