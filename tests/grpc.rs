//! gRPC integration tests. Each test spawns a fresh `TestServer`,
//! connects via the generated `MicrogridClient`, and exercises the
//! RPC surface end-to-end.

mod common;

use common::TestServer;
use macrocosim::proto::common::metrics::{Bounds, Metric};
use macrocosim::proto::microgrid::microgrid_client::MicrogridClient;
use macrocosim::proto::microgrid::{
    AugmentElectricalComponentBoundsRequest, ListElectricalComponentConnectionsRequest,
    ListElectricalComponentsRequest, PowerType, ReceiveElectricalComponentTelemetryStreamRequest,
    ReceiveElectricalComponentTelemetryStreamResponse, SetElectricalComponentPowerRequest,
    SetElectricalComponentPowerRequestStatus, SetElectricalComponentPowerResponse,
};

/// Pull the AC active-power value (W) out of a telemetry response, if present.
fn active_power_w(resp: &ReceiveElectricalComponentTelemetryStreamResponse) -> Option<f32> {
    use macrocosim::proto::common::metrics::{Metric, metric_value_variant::MetricValueVariant};
    let t = resp.telemetry.as_ref()?;
    t.metric_samples.iter().find_map(|s| {
        if s.metric != Metric::AcPowerActive as i32 {
            return None;
        }
        match s.value.as_ref()?.metric_value_variant.as_ref()? {
            MetricValueVariant::SimpleMetric(v) => Some(v.value),
            _ => None,
        }
    })
}

/// Pull the `AC_POWER_REACTIVE` sample's bounds out of a telemetry
/// response. `None` when the component published no reactive sample.
fn reactive_sample_bounds(
    resp: &ReceiveElectricalComponentTelemetryStreamResponse,
) -> Option<Vec<Bounds>> {
    let t = resp.telemetry.as_ref()?;
    t.metric_samples
        .iter()
        .find(|s| s.metric == Metric::AcPowerReactive as i32)
        .map(|s| s.bounds.clone())
}

/// Subscribe to `id` and return the first reactive sample that
/// carries bounds. Panics if none arrives within 5 s.
async fn first_reactive_bounds(
    c: &mut MicrogridClient<tonic::transport::Channel>,
    id: u64,
) -> Vec<Bounds> {
    let mut stream = c
        .receive_electrical_component_telemetry_stream(
            ReceiveElectricalComponentTelemetryStreamRequest {
                electrical_component_id: id,
                filter: None,
            },
        )
        .await
        .expect("subscribe")
        .into_inner();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Ok(Some(msg)) = stream.message().await {
            match reactive_sample_bounds(&msg) {
                Some(b) if !b.is_empty() => return Some(b),
                _ => continue,
            }
        }
        None
    })
    .await
    .expect("telemetry stream timed out")
    .expect("a reactive sample carrying bounds")
}

/// Subscribe to `id` and block until its AC active power reaches
/// `at_least` W. Panics if it doesn't within 5 s. Lets a test wait for
/// a commanded setpoint to reach the physics loop's published value
/// instead of guessing at a sleep.
async fn wait_for_active_power(
    c: &mut MicrogridClient<tonic::transport::Channel>,
    id: u64,
    at_least: f32,
) {
    let mut stream = c
        .receive_electrical_component_telemetry_stream(
            ReceiveElectricalComponentTelemetryStreamRequest {
                electrical_component_id: id,
                filter: None,
            },
        )
        .await
        .expect("subscribe")
        .into_inner();
    let reached = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Ok(Some(msg)) = stream.message().await {
            if active_power_w(&msg).is_some_and(|p| p >= at_least) {
                return true;
            }
        }
        false
    })
    .await
    .expect("telemetry stream timed out");
    assert!(reached, "component {id} never reached {at_least} W");
}

/// Drain a SetPower response stream: an ACCEPTED acknowledgement
/// carrying the expiry, then a SUCCESS carrying the same expiry, then
/// end of stream.
async fn expect_accepted_then_success(
    mut stream: tonic::Streaming<SetElectricalComponentPowerResponse>,
) {
    let first = stream
        .message()
        .await
        .expect("stream poll")
        .expect("an ACCEPTED status first");
    assert_eq!(
        first.status,
        SetElectricalComponentPowerRequestStatus::Accepted as i32,
    );
    assert!(
        first.valid_until_time.is_some(),
        "ACCEPTED carries the expiry"
    );
    let second = stream
        .message()
        .await
        .expect("stream poll")
        .expect("a SUCCESS status second");
    assert_eq!(
        second.status,
        SetElectricalComponentPowerRequestStatus::Success as i32,
    );
    assert_eq!(second.valid_until_time, first.valid_until_time);
    assert!(
        stream.message().await.expect("stream poll").is_none(),
        "the stream closes after the final status"
    );
}

const TINY_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-battery-inverter
                                      :id 4
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower -5000.0
                                             :rated-upper  5000.0)))))))
"#;

async fn connect(s: &TestServer) -> MicrogridClient<tonic::transport::Channel> {
    MicrogridClient::connect(s.grpc_url.clone())
        .await
        .expect("grpc connect")
}

#[tokio::test(flavor = "multi_thread")]
async fn list_components_returns_topology() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .list_electrical_components(ListElectricalComponentsRequest::default())
        .await
        .expect("list ok")
        .into_inner();
    let ids: Vec<u64> = resp.electrical_components.iter().map(|c| c.id).collect();
    assert!(ids.contains(&1));
    assert!(ids.contains(&2));
    assert!(ids.contains(&3));
    assert!(ids.contains(&4));
}

#[tokio::test(flavor = "multi_thread")]
async fn list_connections_returns_edges() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .list_electrical_component_connections(ListElectricalComponentConnectionsRequest::default())
        .await
        .expect("list ok")
        .into_inner();
    // grid → meter, meter → inverter, inverter → battery.
    assert_eq!(resp.electrical_component_connections.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn set_power_streams_accepted_then_success() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4, // the battery inverter
            power: 1000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect("set-power ok");
    expect_accepted_then_success(resp.into_inner()).await;
}

const ERRORED_INVERTER_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-battery-inverter
                                      :id 4 :health 'error
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower -5000.0
                                             :rated-upper  5000.0)))))))
"#;

/// Inverter rated ±5 kW but its battery only ±1 kW, so the combined
/// envelope the gateway must enforce is ±1 kW — narrower than the
/// inverter's own bounds.
const NARROW_BATTERY_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-battery-inverter
                                      :id 4
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower -1000.0
                                             :rated-upper  1000.0)))))))
"#;

/// A boiler at its default 8 bar target with no steam demand: it needs
/// no electricity, so its per-tick derate band is `[0, 0]` — the
/// narrowest live envelope any component in these tests advertises.
const BOILER_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-steam-boiler :id 5)))))
"#;

/// An errored inverter refuses every command — both setpoints and bounds
/// augmentations (the latter were previously accepted unconditionally).
#[tokio::test(flavor = "multi_thread")]
async fn errored_component_rejects_power_and_bounds() {
    let s = TestServer::start(ERRORED_INVERTER_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let power_err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 0.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("setpoint to an errored device should be rejected");
    // Erroring the device couples its command mode to Error → Unavailable.
    assert_eq!(power_err.code(), tonic::Code::Unavailable);

    let bounds_err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(-1000.0),
                upper: Some(1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("bounds augmentation to an errored device should be rejected");
    assert_eq!(bounds_err.code(), tonic::Code::Unavailable);
}

/// A degraded component streams its health as the state code: the
/// `:health 'error` inverter reports ERROR in Normal telemetry mode,
/// so it is distinguishable from a healthy parked device — which
/// reports the power-derived state instead. Pins the health override
/// in the stream loop; without it both would read as idle.
#[tokio::test(flavor = "multi_thread")]
async fn health_drives_the_streamed_state_code() {
    use macrocosim::proto::common::microgrid::electrical_components::ElectricalComponentStateCode;

    fn states(resp: &ReceiveElectricalComponentTelemetryStreamResponse) -> Vec<i32> {
        resp.telemetry
            .as_ref()
            .map(|t| {
                t.state_snapshots
                    .iter()
                    .flat_map(|s| s.states.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    async fn first_sample(
        c: &mut MicrogridClient<tonic::transport::Channel>,
        id: u64,
    ) -> ReceiveElectricalComponentTelemetryStreamResponse {
        let mut stream = c
            .receive_electrical_component_telemetry_stream(
                ReceiveElectricalComponentTelemetryStreamRequest {
                    electrical_component_id: id,
                    filter: None,
                },
            )
            .await
            .expect("subscribe")
            .into_inner();
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.message())
            .await
            .expect("a sample within 5s")
            .expect("stream poll")
            .expect("one sample")
    }

    let s = TestServer::start(ERRORED_INVERTER_TOPOLOGY).await;
    let mut c = connect(&s).await;

    let error_code = ElectricalComponentStateCode::Error as i32;
    assert_eq!(
        states(&first_sample(&mut c, 4).await),
        vec![error_code],
        "the errored inverter must stream ERROR, not a power-derived state"
    );

    let battery_states = states(&first_sample(&mut c, 3).await);
    assert!(
        !battery_states.is_empty() && !battery_states.contains(&error_code),
        "the healthy battery keeps its power-derived state, got {battery_states:?}"
    );
}

/// An out-of-range `request_lifetime` is a protocol error that must be
/// rejected *before* the setpoint is applied — otherwise the component
/// runs at the commanded power with no expiry timer while the client
/// sees an error.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_range_lifetime_rejects_without_actuating() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 3000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(5), // below the 10 s minimum
        })
        .await
        .expect_err("sub-minimum lifetime should be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // The inverter must not have actuated: stream it and confirm it stays
    // at 0 W rather than ramping to the rejected 3 kW (its ramp is instant).
    let resp = c
        .receive_electrical_component_telemetry_stream(
            ReceiveElectricalComponentTelemetryStreamRequest {
                electrical_component_id: 4,
                filter: None,
            },
        )
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();
    let checked = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut seen = 0;
        while let Ok(Some(msg)) = stream.message().await {
            if let Some(p) = active_power_w(&msg) {
                assert!(
                    p.abs() < 1.0,
                    "inverter actuated despite a rejected request: {p} W"
                );
                seen += 1;
                if seen >= 3 {
                    break;
                }
            }
        }
        seen
    })
    .await
    .expect("telemetry stream timed out");
    assert!(checked >= 3, "expected ≥3 power samples, got {checked}");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_power_outside_envelope_is_rejected() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    // Inverter rated bounds are ±5 kW; +10 kW is outside.
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 10_000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("expected rejection");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("envelope") || err.message().contains("bounds"),
        "expected envelope/bounds in message, got {:?}",
        err.message()
    );
}

/// A setpoint inside the inverter's own bounds but outside its battery's
/// (narrower) bounds is rejected against the *intersection* — not
/// silently saturated. The complement of the inverter-only-bounds test.
#[tokio::test(flavor = "multi_thread")]
async fn set_power_outside_battery_inverter_intersection_is_rejected() {
    let s = TestServer::start(NARROW_BATTERY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    // +3 kW: within the inverter's ±5 kW, outside the battery's ±1 kW.
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 3_000.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("expected rejection against the ±1 kW intersection");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("envelope"),
        "expected 'envelope' in message, got {:?}",
        err.message()
    );
    // Within the ±1 kW intersection is accepted.
    let resp = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 800.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect("800 W is within the intersection");
    expect_accepted_then_success(resp.into_inner()).await;
}

/// 0 W (the fail-safe park) must always be accepted, even when an
/// augmentation has narrowed the envelope to exclude it.
#[tokio::test(flavor = "multi_thread")]
async fn zero_power_is_always_allowed() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    // Narrow inverter 4 to discharge-only [-5 kW, -1 kW], excluding 0 W.
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 4,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(-5000.0),
            upper: Some(-1000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("augment ok");

    // A non-zero setpoint outside the augmented band is still rejected...
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 500.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("500 W is outside the augmented envelope");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    // ...but 0 W is accepted regardless.
    let resp = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 0.0,
            power_type: PowerType::Active as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect("0 W must be accepted");
    expect_accepted_then_success(resp.into_inner()).await;
}

/// An Augment request without a lifetime falls back to the proto's
/// documented 5 s default, not SetPower's 60 s.
#[tokio::test(flavor = "multi_thread")]
async fn augment_without_lifetime_expires_after_five_seconds() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let before = std::time::SystemTime::now();
    let resp = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4, // the battery inverter
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(-1000.0),
                upper: Some(1000.0),
            }],
            request_lifetime: None,
        })
        .await
        .expect("augment ok")
        .into_inner();
    let ts = resp.valid_until_time.expect("expiry");
    let until = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::new(ts.seconds as u64, ts.nanos as u32);
    let ttl = until.duration_since(before).expect("expiry after request");
    assert!(
        (4..=6).contains(&ttl.as_secs()),
        "expected a ~5 s fallback, got {ttl:?}"
    );
}

/// A malformed augmentation — inverted, or disjoint from the component's
/// bounds — must be rejected, not silently brick the component (every
/// setpoint then rejected while the running output goes unconstrained).
#[tokio::test(flavor = "multi_thread")]
async fn malformed_augmentation_is_rejected() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;

    let inverted = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(1000.0),
                upper: Some(-1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("inverted bounds must be rejected");
    assert_eq!(inverted.code(), tonic::Code::InvalidArgument);

    // Disjoint from the inverter's rated ±5 kW band.
    let disjoint = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(50_000.0),
                upper: Some(60_000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("disjoint bounds must be rejected");
    assert_eq!(disjoint.code(), tonic::Code::InvalidArgument);

    // A valid tightening still succeeds.
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 4,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(-2000.0),
            upper: Some(2000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("valid augmentation must be accepted");
}

/// A component with no augmentation storage on the requested axis
/// says so instead of acknowledging a cap that is never armed —
/// the same answer a setpoint gets from a component that takes none.
#[tokio::test(flavor = "multi_thread")]
async fn augment_on_a_component_without_storage_is_unimplemented() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    for (id, what) in [(3, "the battery"), (2, "the meter"), (1, "the grid")] {
        let err = c
            .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
                electrical_component_id: id,
                target_metric: Metric::AcPowerActive as i32,
                bounds: vec![Bounds {
                    lower: Some(-1000.0),
                    upper: Some(1000.0),
                }],
                request_lifetime: Some(30),
            })
            .await
            .err()
            .unwrap_or_else(|| panic!("{what} accepted an augmentation it cannot store"));
        assert_eq!(err.code(), tonic::Code::Unimplemented, "{what}: {err:?}");
        // The refusal names the component and the axis, so a client
        // driving several at once can tell which request bounced.
        assert!(
            err.message().contains(&id.to_string()) && err.message().contains("AC_POWER_ACTIVE"),
            "{what}: expected the id and metric named, got {:?}",
            err.message(),
        );
    }
}

/// The refusal does not depend on the band. A battery has no
/// `PowerAxis` to store an augmentation in, so a band disjoint from
/// the ±5 kW envelope it advertises and a band that overlaps it are
/// both answered UNIMPLEMENTED: neither would have armed anything, so
/// neither is a matter of the client picking better numbers.
#[tokio::test(flavor = "multi_thread")]
async fn an_axis_less_component_refuses_every_augmentation_band() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    for (lower, upper, what) in [
        (50_000.0, 60_000.0, "a band disjoint from the battery"),
        (-2000.0, 2000.0, "a band overlapping the battery"),
    ] {
        let err = c
            .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
                electrical_component_id: 3, // the battery: no PowerAxis
                target_metric: Metric::AcPowerActive as i32,
                bounds: vec![Bounds {
                    lower: Some(lower),
                    upper: Some(upper),
                }],
                request_lifetime: Some(30),
            })
            .await
            .err()
            .unwrap_or_else(|| panic!("{what} was accepted"));
        assert_eq!(err.code(), tonic::Code::Unimplemented, "{what}: {err:?}");
    }
}

/// A boiler sitting at target pressure with no steam demand needs no
/// electricity: its per-tick derate band is `[0, 0]`. An augmentation
/// asking for a strictly positive band is disjoint from that, and
/// ACKing it would park the boiler at 0 W for the whole TTL with the
/// client believing a 10–20 kW window was in force. The gate composes
/// the derate, so it's an InvalidArgument — and the message names the
/// `[0, 0]` envelope the client can actually command.
#[tokio::test(flavor = "multi_thread")]
async fn an_augmentation_disjoint_from_a_derate_is_rejected() {
    let s = TestServer::start(BOILER_TOPOLOGY).await;
    let mut c = connect(&s).await;

    let err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 5,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(10_000.0),
                upper: Some(20_000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("disjoint from the idle boiler's [0, 0] demand band");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("current envelope [0, 0]"),
        "expected the derated envelope named in the message, got {:?}",
        err.message(),
    );

    // A band that includes 0 overlaps the derate and is accepted.
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 5,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(0.0),
            upper: Some(20_000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("a band overlapping the derate is accepted");
}

/// Storage on one axis is not storage on the other. A steam boiler
/// has an active `PowerAxis` but no reactive one, so it stores an
/// active augmentation and refuses a reactive one as unimplemented —
/// the per-axis answer, not a per-component one.
#[tokio::test(flavor = "multi_thread")]
async fn a_component_with_only_an_active_axis_refuses_reactive_augmentation() {
    let s = TestServer::start(BOILER_TOPOLOGY).await;
    let mut c = connect(&s).await;

    let err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 5,
            target_metric: Metric::AcPowerReactive as i32,
            bounds: vec![Bounds {
                lower: Some(-1000.0),
                upper: Some(1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("the boiler has no reactive axis to store an augmentation in");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");

    // The same component's active axis still takes one. `[0, 20 kW]`
    // overlaps the idle boiler's `[0, 0]` demand band.
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 5,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(0.0),
            upper: Some(20_000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("the boiler's active axis stores the augmentation");
}

/// Two sequential augmentations with mutually disjoint bands: the
/// second is rejected atomically against the first's still-live
/// envelope (not a lock-free read taken before either applied), and
/// the component still accepts a setpoint inside the first band. The
/// gRPC-level twin of
/// `try_augment_rejects_a_band_disjoint_with_live_augmentations` in
/// axis.rs.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_disjoint_augmentation_is_rejected_and_the_first_stays_live() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;

    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 4,
        target_metric: Metric::AcPowerActive as i32,
        bounds: vec![Bounds {
            lower: Some(1000.0),
            upper: Some(3000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("the first augmentation is accepted");

    let err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerActive as i32,
            bounds: vec![Bounds {
                lower: Some(-3000.0),
                upper: Some(-1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("disjoint from the live [1000, 3000] augmentation");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    // The rejection names the component's CURRENT envelope — the live
    // [1000, 3000] augmentation, i.e. where a client should retry —
    // not the composed result, which is empty by construction here and
    // would render as a useless constant "[]".
    assert!(
        err.message().contains("current envelope"),
        "expected the current-envelope detail in the message, got {:?}",
        err.message(),
    );
    assert!(
        err.message().contains("1000") && err.message().contains("3000"),
        "the message must name a NON-EMPTY envelope (the live [1000, 3000] band), got {:?}",
        err.message(),
    );

    // The first augmentation is still in force: a setpoint inside it
    // is still accepted, proving the envelope never went empty.
    c.set_electrical_component_power(SetElectricalComponentPowerRequest {
        electrical_component_id: 4,
        power: 2000.0,
        power_type: PowerType::Active as i32,
        request_lifetime: Some(30),
    })
    .await
    .expect("2000 W is inside the first, still-live augmentation");
}

/// Same shape as `TINY_TOPOLOGY`, but the inverter carries a real
/// reactive envelope: no PF limit (the inherited default would pin Q
/// to 0 at idle) and a 5 kVA apparent-power cap, so its Q band at
/// P = 0 is ±5 kVAr.
const REACTIVE_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-battery-inverter
                                      :id 4
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :reactive-pf-limit 0
                                      :reactive-apparent-va 5000.0
                                      :successors
                                      (list (%make-battery
                                             :id 3
                                             :rated-lower -5000.0
                                             :rated-upper  5000.0)))))))
"#;

/// An `AC_POWER_REACTIVE` augmentation is accepted end-to-end: the
/// response carries the expiry the augmentation was armed with, and
/// the live telemetry stream reports the narrowed Q band.
#[tokio::test(flavor = "multi_thread")]
async fn reactive_augmentation_is_accepted_and_narrows_the_stream() {
    let s = TestServer::start(REACTIVE_TOPOLOGY).await;
    let mut c = connect(&s).await;

    // Baseline: the un-augmented band is the ±5 kVAr apparent cap.
    let wide = first_reactive_bounds(&mut c, 4).await;
    assert_eq!(wide.len(), 1, "expected one band, got {wide:?}");
    assert_eq!(wide[0].lower, Some(-5000.0));
    assert_eq!(wide[0].upper, Some(5000.0));

    let resp = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerReactive as i32,
            bounds: vec![Bounds {
                lower: Some(-1000.0),
                upper: Some(1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect("a reactive augmentation must be accepted")
        .into_inner();
    assert!(
        resp.valid_until_time.is_some(),
        "an accepted augmentation reports when it expires",
    );

    // The journal keeps the axis: a Q augmentation is logged under
    // its own kind, not the active route's `augment_bounds`.
    let logged = s
        .config
        .site()
        .setpoints_window(4, chrono::Utc::now() - chrono::Duration::minutes(1));
    let kinds: Vec<&str> = logged.iter().map(|e| e.kind.as_str()).collect();
    assert!(
        kinds.contains(&"augment_reactive_bounds"),
        "expected the reactive augment kind in the journal, got {kinds:?}",
    );
    assert!(
        !kinds.contains(&"augment_bounds"),
        "the active augment kind must not be used for a Q request, got {kinds:?}",
    );

    let narrowed = first_reactive_bounds(&mut c, 4).await;
    assert_eq!(narrowed.len(), 1, "expected one band, got {narrowed:?}");
    assert_eq!(narrowed[0].lower, Some(-1000.0));
    assert_eq!(narrowed[0].upper, Some(1000.0));

    // The gateway now rejects a setpoint the caps band alone allowed.
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 3000.0,
            power_type: PowerType::Reactive as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("3 kVAr is outside the augmented ±1 kVAr band");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

/// Zero Q headroom must not be a loophole in the augment gate.
///
/// The disjoint check runs against the component's LIVE Q envelope,
/// composed inside `PowerAxis::try_augment`. Telemetry normalizes a
/// genuinely empty envelope to a present `(0, 0)` band
/// (`VecBounds::or_zero_band`) so consumers see "zero headroom"
/// rather than an absent bound — but the axis never applies that
/// normalization, and if it did it would accept any augmentation
/// straddling zero, leaving two live, mutually disjoint augmentations
/// on the axis. An empty envelope is disjoint from everything.
#[tokio::test(flavor = "multi_thread")]
async fn a_disjoint_q_augmentation_is_rejected_at_zero_headroom() {
    let s = TestServer::start(REACTIVE_TOPOLOGY).await;
    let mut c = connect(&s).await;

    // 1. At P = 0 the caps band is ±5 kVAr, so a [-4, -3] kVAr
    //    augmentation overlaps it and is accepted.
    c.augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
        electrical_component_id: 4,
        target_metric: Metric::AcPowerReactive as i32,
        bounds: vec![Bounds {
            lower: Some(-4000.0),
            upper: Some(-3000.0),
        }],
        request_lifetime: Some(30),
    })
    .await
    .expect("an augmentation overlapping the idle caps band is accepted");

    // 2. Drive P to the 5 kVA rim. The caps band collapses to (0, 0),
    //    which no longer overlaps the live augmentation: the real Q
    //    envelope is now EMPTY.
    c.set_electrical_component_power(SetElectricalComponentPowerRequest {
        electrical_component_id: 4,
        power: 5000.0,
        power_type: PowerType::Active as i32,
        request_lifetime: Some(30),
    })
    .await
    .expect("5 kW is inside the inverter's rated band");
    wait_for_active_power(&mut c, 4, 4200.0).await;

    // 3. [-500, 500] straddles zero, so it overlaps the NORMALIZED
    //    (0, 0) band telemetry publishes — but it is disjoint from the
    //    live [-4000, -3000] augmentation, and accepting it would leave
    //    the axis with two live augmentations that exclude each other.
    let err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerReactive as i32,
            bounds: vec![Bounds {
                lower: Some(-500.0),
                upper: Some(500.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("an augmentation disjoint from the live Q envelope must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("disjoint"),
        "expected 'disjoint' in message, got {:?}",
        err.message(),
    );

    // 4. Nothing was stored: the axis still reports the honest zero
    //    headroom, and every nonzero Q setpoint is still refused.
    let band = first_reactive_bounds(&mut c, 4).await;
    assert_eq!(band.len(), 1, "expected one band, got {band:?}");
    assert_eq!((band[0].lower, band[0].upper), (Some(0.0), Some(0.0)));
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 400.0,
            power_type: PowerType::Reactive as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("no Q is legal with zero headroom");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

/// Same as `REACTIVE_TOPOLOGY`, but the battery inverter's child is a
/// solar inverter, which *does* report a Q band (1 kVA cap -> ±1
/// kVAr). No production topology nests a Q-reporting child under an
/// inverter yet; this is how the reactive gateway's intersect branch
/// gets reached end-to-end.
const NESTED_REACTIVE_TOPOLOGY: &str = r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-meter :id 2
                               :successors
                               (list (%make-battery-inverter
                                      :id 4
                                      :rated-lower -5000.0
                                      :rated-upper  5000.0
                                      :reactive-pf-limit 0
                                      :reactive-apparent-va 5000.0
                                      :successors
                                      (list (%make-solar-inverter
                                             :id 3
                                             :sunlight% 0
                                             :rated-lower -1000.0
                                             :rated-upper  0.0
                                             :reactive-pf-limit 0
                                             :reactive-apparent-va 1000.0)))))))
"#;

/// The SetPower gateway gates the reactive axis against the combined
/// Q envelope, mirroring the active axis — right down to the message,
/// which reports VAr rather than W.
#[tokio::test(flavor = "multi_thread")]
async fn set_reactive_power_outside_the_combined_envelope_is_rejected() {
    let s = TestServer::start(NESTED_REACTIVE_TOPOLOGY).await;
    let mut c = connect(&s).await;
    // 3 kVAr is inside the inverter's own ±5 kVAr but outside the
    // ±1 kVAr intersection with its Q-reporting child.
    let err = c
        .set_electrical_component_power(SetElectricalComponentPowerRequest {
            electrical_component_id: 4,
            power: 3000.0,
            power_type: PowerType::Reactive as i32,
            request_lifetime: Some(30),
        })
        .await
        .expect_err("expected rejection against the ±1 kVAr intersection");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("combined envelope") && err.message().contains("VAr"),
        "expected the combined-envelope / VAr wording, got {:?}",
        err.message(),
    );
    // Inside the intersection is accepted, and so is the 0 VAr park.
    for power in [800.0, 0.0] {
        let resp = c
            .set_electrical_component_power(SetElectricalComponentPowerRequest {
                electrical_component_id: 4,
                power,
                power_type: PowerType::Reactive as i32,
                request_lifetime: Some(30),
            })
            .await
            .unwrap_or_else(|e| panic!("{power} VAr must be accepted, got {e:?}"));
        expect_accepted_then_success(resp.into_inner()).await;
    }
}

/// Only the two AC power axes are augmentable. Anything else keeps
/// the invalid-argument rejection, and the message names the metric
/// that was asked for so a client can see what it got wrong.
#[tokio::test(flavor = "multi_thread")]
async fn augment_rejects_an_unsupported_metric_by_name() {
    let s = TestServer::start(REACTIVE_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::DcPower as i32,
            bounds: vec![Bounds {
                lower: Some(-1000.0),
                upper: Some(1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("DC_POWER bounds are not augmentable");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("DC_POWER"),
        "expected the metric named in the message, got {:?}",
        err.message(),
    );
}

/// A reactive augmentation on a component with no Q axis is refused,
/// like an active one on a component with no augmentation storage:
/// nothing would be armed, so an ACK would lie.
#[tokio::test(flavor = "multi_thread")]
async fn reactive_augmentation_on_a_q_less_component_is_unimplemented() {
    let s = TestServer::start(REACTIVE_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let err = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 3, // the battery: no reactive axis
            target_metric: Metric::AcPowerReactive as i32,
            bounds: vec![Bounds {
                lower: Some(-1000.0),
                upper: Some(1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("a Q augmentation on a Q-less component must be refused");
    assert_eq!(err.code(), tonic::Code::Unimplemented, "{err:?}");
}

/// The reactive route feeds client input into `PowerAxis::augment`
/// just like the active one, so it must inherit the same shape
/// checks. A NaN edge is rejected rather than stored as a de-facto
/// no-op — and the Q band stays exactly where it was.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_reactive_augmentation_is_rejected() {
    let s = TestServer::start(REACTIVE_TOPOLOGY).await;
    let mut c = connect(&s).await;

    let nan = c
        .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
            electrical_component_id: 4,
            target_metric: Metric::AcPowerReactive as i32,
            bounds: vec![Bounds {
                lower: Some(f32::NAN),
                upper: Some(1000.0),
            }],
            request_lifetime: Some(30),
        })
        .await
        .expect_err("a non-finite edge must be rejected");
    assert_eq!(nan.code(), tonic::Code::InvalidArgument);
    assert!(
        nan.message().contains("non-finite"),
        "expected 'non-finite' in message, got {:?}",
        nan.message(),
    );

    // Inverted, and disjoint from the ±5 kVAr caps band.
    for bounds in [
        Bounds {
            lower: Some(1000.0),
            upper: Some(-1000.0),
        },
        Bounds {
            lower: Some(20_000.0),
            upper: Some(30_000.0),
        },
    ] {
        let err = c
            .augment_electrical_component_bounds(AugmentElectricalComponentBoundsRequest {
                electrical_component_id: 4,
                target_metric: Metric::AcPowerReactive as i32,
                bounds: vec![bounds],
                request_lifetime: Some(30),
            })
            .await
            .expect_err("malformed reactive bounds must be rejected");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    // Nothing was stored: the band is still the full ±5 kVAr cap, not
    // a silently unconstrained or bricked axis.
    let band = first_reactive_bounds(&mut c, 4).await;
    assert_eq!(band.len(), 1, "expected one band, got {band:?}");
    assert_eq!(band[0].lower, Some(-5000.0));
    assert_eq!(band[0].upper, Some(5000.0));
}

#[tokio::test(flavor = "multi_thread")]
async fn telemetry_stream_emits_samples_for_a_component() {
    let s = TestServer::start(TINY_TOPOLOGY).await;
    let mut c = connect(&s).await;
    let resp = c
        .receive_electrical_component_telemetry_stream(
            ReceiveElectricalComponentTelemetryStreamRequest {
                electrical_component_id: 2, // main meter
                filter: None,
            },
        )
        .await
        .expect("subscribe");
    let mut stream = resp.into_inner();
    let mut got = 0usize;
    let take = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Ok(Some(msg)) = stream.message().await {
            if msg.telemetry.is_some() {
                got += 1;
                if got >= 2 {
                    break;
                }
            }
        }
    })
    .await;
    assert!(take.is_ok(), "stream timed out before 2 samples");
    assert!(got >= 2, "expected ≥2 samples, got {got}");
}

/// A battery's DC power sample carries the SoC-throttled bounds: at
/// 85 % with the default 90 % `:soc-upper` and 10 % margin, the
/// charge side is tapered and the discharge side is rated.
#[tokio::test(flavor = "multi_thread")]
async fn battery_telemetry_carries_the_throttled_bounds() {
    use macrocosim::sim::decay::{SocProtect, soc_protected_bounds};
    let s = TestServer::start(
        r#"
(%make-grid-connection-point :id 1
            :successors
            (list (%make-battery-inverter
                   :id 4
                   :rated-lower -5000.0
                   :rated-upper  5000.0
                   :successors
                   (list (%make-battery
                          :id 3
                          :initial-soc 85.0
                          :rated-lower -5000.0
                          :rated-upper  5000.0)))))
"#,
    )
    .await;
    let mut c = connect(&s).await;
    let mut stream = c
        .receive_electrical_component_telemetry_stream(
            ReceiveElectricalComponentTelemetryStreamRequest {
                electrical_component_id: 3,
                filter: None,
            },
        )
        .await
        .expect("subscribe")
        .into_inner();
    let bounds = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Ok(Some(msg)) = stream.message().await {
            let dc = msg.telemetry.as_ref().and_then(|t| {
                t.metric_samples
                    .iter()
                    .find(|s| s.metric == Metric::DcPower as i32)
            });
            if let Some(dc) = dc
                && !dc.bounds.is_empty()
            {
                return Some(dc.bounds.clone());
            }
        }
        None
    })
    .await
    .expect("telemetry stream timed out")
    .expect("a DC power sample carrying bounds");
    let (lo, hi) = soc_protected_bounds(-5_000.0, 5_000.0, 85.0, SocProtect::new(10.0, 90.0, 10.0));
    assert!(hi > 0.0 && hi < 5_000.0, "the test needs a taper, got {hi}");
    assert_eq!(bounds.len(), 1);
    assert_eq!(bounds[0].lower, Some(lo));
    assert_eq!(bounds[0].upper, Some(hi));
}
