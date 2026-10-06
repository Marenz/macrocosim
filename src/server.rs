//! gRPC server: implements the Frequenz Microgrid API on top of
//! macrocosim's `MicrogridSite` + `Config`.
//!
//! Streaming telemetry is one tokio task per subscription; each task
//! samples its component at the component's own `stream_interval` and
//! forwards the protobuf message until the client disconnects.

use std::pin::Pin;
use std::time::{Duration, SystemTime};

use prost_types::Timestamp;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;

use crate::lisp::Config;
use crate::proto::common::metrics::Metric;

/// Per the Frequenz Microgrid API, request_lifetime must fit in an
/// RPC-specific window. SetElectricalComponentPower's minimum is
/// 10 s (long enough for a control loop to apply and settle);
/// AugmentElectricalComponentBounds allows down to 5 s. Both share
/// the 15 min cap on how long a forgotten request can park a
/// component away from its default. Out-of-range values are
/// rejected with InvalidArgument; absent values fall back to
/// `Metadata::default_request_lifetime` for SetElectricalComponentPower
/// or `Metadata::default_augment_lifetime` for
/// AugmentElectricalComponentBounds (both configurable from lisp).
const SET_POWER_LIFETIME_MIN_S: u64 = 10;
const AUGMENT_LIFETIME_MIN_S: u64 = 5;
const REQUEST_LIFETIME_MAX_S: u64 = 15 * 60;

fn resolve_lifetime(
    req_lifetime_s: Option<u64>,
    min_s: u64,
    fallback: Duration,
) -> Result<Duration, tonic::Status> {
    match req_lifetime_s {
        Some(s) => {
            if !(min_s..=REQUEST_LIFETIME_MAX_S).contains(&s) {
                return Err(tonic::Status::invalid_argument(format!(
                    "request_lifetime {s} s outside [{min_s}, \
                     {REQUEST_LIFETIME_MAX_S}] s",
                )));
            }
            Ok(Duration::from_secs(s))
        }
        None => Ok(fallback),
    }
}

use crate::proto::common::microgrid::electrical_components::ElectricalComponentConnection;
use crate::proto::common::microgrid::{Microgrid, MicrogridStatus};
use crate::proto::microgrid::{
    AckElectricalComponentErrorRequest, AugmentElectricalComponentBoundsRequest,
    AugmentElectricalComponentBoundsResponse, GetMicrogridResponse,
    ListElectricalComponentConnectionsRequest, ListElectricalComponentConnectionsResponse,
    ListElectricalComponentsRequest, ListElectricalComponentsResponse, ListSensorRequest,
    ListSensorsResponse, PowerType, PutElectricalComponentInStandbyRequest,
    ReceiveElectricalComponentTelemetryStreamRequest,
    ReceiveElectricalComponentTelemetryStreamResponse, ReceiveSensorTelemetryStreamRequest,
    ReceiveSensorTelemetryStreamResponse, SetElectricalComponentPowerRequest,
    SetElectricalComponentPowerRequestStatus, SetElectricalComponentPowerResponse,
    StartElectricalComponentRequest, StopElectricalComponentRequest, microgrid_server,
};
use crate::proto_conv::{make_component_proto, telemetry_to_proto};
use crate::sim::gateway::{GatewayError, Mode};
use crate::sim::runtime::{CommandMode, Health, TelemetryMode};
use crate::sim::setpoints::{SetpointEvent, SetpointKind, SetpointOutcome};
use crate::sim::{AugmentError, bounds::VecBounds};
use crate::timeout_tracker::SetpointAxis;

/// gRPC frontend for one microgrid. Each microgrid registered in
/// `Config::microgrids` spawns its own server bound to its
/// `MicrogridDef::grpc_port` and serving against its own
/// `MicrogridSite`. The legacy single-microgrid binary still uses
/// this — the registry just carries one entry (the default).
pub struct MicrogridServer {
    pub config: Config,
    /// Specific id of the microgrid this gRPC frontend represents.
    /// `get_microgrid` sources its response from here so each
    /// per-port server returns its own MicrogridInfo (matching the
    /// id the client picked when connecting to its port).
    pub microgrid_id: u64,
    /// Pinned site this server reads from. Captured at
    /// construction time rather than re-resolved via the registry
    /// on every RPC, so a topology rebuild on a *different*
    /// microgrid doesn't disturb this server's stream sessions.
    pub site: crate::sim::MicrogridSite,
}

impl MicrogridServer {
    pub fn new(config: Config, microgrid_id: u64, site: crate::sim::MicrogridSite) -> Self {
        Self {
            config,
            microgrid_id,
            site,
        }
    }

    /// Whether a component is *currently* inside its over-bound fault window.
    /// Each component faults for a single second roughly once a minute, with
    /// its fault-second spread across the cycle by a hash of its id so the
    /// faults don't bunch into the same handful of seconds. So at any instant
    /// at most a small, slowly-rotating subset of components is rejecting
    /// set-power — a few flaky devices flapping in and out of the blocked set,
    /// rather than a different one rejecting every single second.
    fn over_bound_faulty_now(component_id: u64) -> bool {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // ~once a minute, for one second, per component.
        const CYCLE: u64 = 60;
        const FAULT_WINDOW: u64 = 1;
        let phase = component_id.wrapping_mul(0x9E37_79B9_7F4A_7C15) % CYCLE;
        secs.wrapping_add(phase) % CYCLE < FAULT_WINDOW
    }

    /// Runtime fault gate shared by SetPower and AugmentBounds. A
    /// `Timeout` command mode hangs the request, `Error` replies
    /// `Unavailable`, and any non-`Ok` health refuses with
    /// `FailedPrecondition` — so an errored or standby device rejects
    /// both setpoint and bounds commands. Returns the command mode so
    /// setpoint callers can further special-case `OverBound` (which is
    /// setpoint-only — bounds commands carry no power value).
    async fn gate_runtime_faults(&self, id: u64) -> Result<CommandMode, tonic::Status> {
        let runtime = self.site.runtime_of(id);
        match runtime.command {
            CommandMode::Timeout => {
                std::future::pending::<()>().await;
                unreachable!()
            }
            CommandMode::Error => {
                return Err(tonic::Status::unavailable(format!(
                    "component {id} unreachable"
                )));
            }
            CommandMode::OverBound | CommandMode::Normal => {}
        }
        if runtime.health != Health::Ok {
            return Err(tonic::Status::failed_precondition(format!(
                "component {id} is in {:?} state; commands rejected",
                runtime.health
            )));
        }
        Ok(runtime.command)
    }

    /// The body of `set_electrical_component_power` minus the
    /// power-type validation up front. Split out so the wrapper can
    /// log the outcome of every code path (early-return rejection or
    /// success) at a single tail point.
    ///
    /// On success the returned stream carries two responses, ACCEPTED
    /// then SUCCESS, and then closes.
    async fn do_set_power(
        &self,
        req: SetElectricalComponentPowerRequest,
        power_type: PowerType,
    ) -> Result<
        tonic::Response<<Self as microgrid_server::Microgrid>::SetElectricalComponentPowerStream>,
        tonic::Status,
    > {
        let site = self.site.clone();
        // A non-finite power sails through every bounds gate (all
        // NaN comparisons are false) and through CommandDelay, and
        // would be acknowledged Success with a fresh TTL while the
        // ramp silently drops it — the previous setpoint keeps
        // running under a re-armed expiry. Protocol error instead.
        if !req.power.is_finite() {
            return Err(tonic::Status::invalid_argument(format!(
                "power must be finite, got {}",
                req.power
            )));
        }
        // The run this request belongs to: the gateway refuses it if
        // a reload resets the site while the fault gate below awaits.
        let generation = site.run_generation();
        site.get(req.electrical_component_id).ok_or_else(|| {
            tonic::Status::not_found(format!(
                "component {} not found",
                req.electrical_component_id
            ))
        })?;

        // Runtime fault simulation: command mode + health are checked
        // before any physics (shared with AugmentBounds).
        let command = self
            .gate_runtime_faults(req.electrical_component_id)
            .await?;
        // `OverBound` is a setpoint-only fault: the gateway advertised
        // bounds that accept this value but rejects it against a tighter
        // internal limit just below the request, returning
        // INVALID_ARGUMENT. Applies to both power types — a device
        // lying about its envelope lies on the reactive axis too.
        // Faulting is intermittent and rotating per component (see
        // `over_bound_faulty_now`), so the set of currently rejecting
        // components churns over time. Zero-power (fail-safe)
        // setpoints are still accepted.
        if matches!(command, CommandMode::OverBound)
            && req.power != 0.0
            && Self::over_bound_faulty_now(req.electrical_component_id)
        {
            let allowed = (req.power.abs() * 0.97).round();
            let label = match power_type {
                PowerType::Active if req.power >= 0.0 => "charge power",
                PowerType::Active => "discharge power",
                _ => "reactive power",
            };
            let unit = if matches!(power_type, PowerType::Active) {
                "W"
            } else {
                "VAr"
            };
            return Err(tonic::Status::invalid_argument(format!(
                "Requested {label} {} {unit} exceeds the maximum allowed {allowed} {unit}",
                req.power.abs().round()
            )));
        }

        let axis = match power_type {
            PowerType::Active => SetpointAxis::Active,
            PowerType::Reactive => SetpointAxis::Reactive,
            PowerType::Unspecified => unreachable!("rejected above"),
        };
        // Resolve the request lifetime before the gateway sees the
        // request: an out-of-range lifetime is a protocol error and
        // must not reach actuation.
        let duration = resolve_lifetime(
            req.request_lifetime,
            SET_POWER_LIFETIME_MIN_S,
            self.config.metadata().default_request_lifetime,
        )?;
        // The gateway gates the value against the setpoint envelope
        // (own bounds ∩ children's), applies it and arms its lifetime
        // under one lock. The response carries that deadline.
        let applied = site
            .gateway()
            .set_power(
                req.electrical_component_id,
                generation,
                axis,
                req.power,
                duration,
                Mode::Reject,
            )
            .map_err(gateway_error_to_status)?;
        let valid_until = Some(crate::proto_conv::datetime_to_ts(applied.deadline));
        let statuses = [
            SetElectricalComponentPowerRequestStatus::Accepted,
            SetElectricalComponentPowerRequestStatus::Success,
        ];
        Ok(tonic::Response::new(Box::pin(tokio_stream::iter(
            statuses.map(|status| {
                Ok(SetElectricalComponentPowerResponse {
                    valid_until_time: valid_until,
                    status: status as i32,
                })
            }),
        ))))
    }
}

#[tonic::async_trait]
impl microgrid_server::Microgrid for MicrogridServer {
    type ReceiveElectricalComponentTelemetryStreamStream = Pin<
        Box<
            dyn Stream<
                    Item = Result<ReceiveElectricalComponentTelemetryStreamResponse, tonic::Status>,
                > + Send,
        >,
    >;
    type ReceiveSensorTelemetryStreamStream = Pin<
        Box<dyn Stream<Item = Result<ReceiveSensorTelemetryStreamResponse, tonic::Status>> + Send>,
    >;
    type SetElectricalComponentPowerStream = Pin<
        Box<dyn Stream<Item = Result<SetElectricalComponentPowerResponse, tonic::Status>> + Send>,
    >;

    async fn get_microgrid(
        &self,
        _request: tonic::Request<()>,
    ) -> Result<tonic::Response<GetMicrogridResponse>, tonic::Status> {
        // The MicrogridInfo response reports *this server's*
        // microgrid id. The display name comes from the registry
        // entry so each per-port server returns the right label.
        let enterprise_id = self.config.metadata().enterprise_id;
        let registry_name = self
            .config
            .microgrids()
            .lock()
            .get(&self.microgrid_id)
            .map(|e| e.def.name.clone());
        let name = registry_name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Microgrid {}", self.microgrid_id));
        let now = Some(Timestamp::from(SystemTime::now()));
        Ok(tonic::Response::new(GetMicrogridResponse {
            microgrid: Some(Microgrid {
                id: self.microgrid_id,
                enterprise_id,
                name,
                status: MicrogridStatus::Active as i32,
                create_timestamp: now,
                ..Default::default()
            }),
        }))
    }

    async fn list_electrical_components(
        &self,
        request: tonic::Request<ListElectricalComponentsRequest>,
    ) -> Result<tonic::Response<ListElectricalComponentsResponse>, tonic::Status> {
        let req = request.into_inner();
        let comps: Vec<_> = self
            .site
            .components()
            .iter()
            .filter(|c| !c.is_hidden())
            .map(|c| make_component_proto(c.as_ref(), &self.site, self.microgrid_id))
            .filter(|c| {
                (req.electrical_component_ids.is_empty()
                    || req.electrical_component_ids.contains(&c.id))
                    && (req.electrical_component_categories.is_empty()
                        || req.electrical_component_categories.contains(&c.category))
            })
            .collect();
        Ok(tonic::Response::new(ListElectricalComponentsResponse {
            electrical_components: comps,
        }))
    }

    async fn list_electrical_component_connections(
        &self,
        request: tonic::Request<ListElectricalComponentConnectionsRequest>,
    ) -> Result<tonic::Response<ListElectricalComponentConnectionsResponse>, tonic::Status> {
        let req = request.into_inner();
        let conns: Vec<_> = self
            .site
            .connections()
            .into_iter()
            .map(|(from, to)| ElectricalComponentConnection {
                source_electrical_component_id: from,
                destination_electrical_component_id: to,
                ..Default::default()
            })
            .filter(|c| {
                (req.source_electrical_component_ids.is_empty()
                    || req
                        .source_electrical_component_ids
                        .contains(&c.source_electrical_component_id))
                    && (req.destination_electrical_component_ids.is_empty()
                        || req
                            .destination_electrical_component_ids
                            .contains(&c.destination_electrical_component_id))
            })
            .collect();
        Ok(tonic::Response::new(
            ListElectricalComponentConnectionsResponse {
                electrical_component_connections: conns,
            },
        ))
    }

    async fn set_electrical_component_power(
        &self,
        request: tonic::Request<SetElectricalComponentPowerRequest>,
    ) -> Result<tonic::Response<Self::SetElectricalComponentPowerStream>, tonic::Status> {
        let req = request.into_inner();
        let power_type = PowerType::try_from(req.power_type)
            .map_err(|_| tonic::Status::invalid_argument("invalid power type"))?;
        if matches!(power_type, PowerType::Unspecified) {
            return Err(tonic::Status::invalid_argument(
                "Power type cannot be UNSPECIFIED.",
            ));
        }

        // Capture the inputs the control inspector cares about
        // before we move `req` into the inner work. Logging happens
        // at the tail so every code path (early return + success)
        // emits exactly one event per request.
        let id = req.electrical_component_id;
        let value = req.power;
        let kind = match power_type {
            PowerType::Active => SetpointKind::ActivePower,
            PowerType::Reactive => SetpointKind::ReactivePower,
            PowerType::Unspecified => unreachable!("rejected above"),
        };
        // Re-resolve the lifetime for the log entry: `do_set_power`
        // consumes `req`, and its own resolution is interleaved with
        // actuation. `None` when the lifetime itself is out of range —
        // the request is then rejected before any TTL takes effect.
        let ttl_s = resolve_lifetime(
            req.request_lifetime,
            SET_POWER_LIFETIME_MIN_S,
            self.config.metadata().default_request_lifetime,
        )
        .ok()
        .map(|d| d.as_secs());
        let site = self.site.clone();
        // A Timeout-fault component parks the request indefinitely
        // (gate_runtime_faults pends until the client cancels), so the
        // tail logging below would never run — yet the stuck-component
        // case is exactly what the setpoint inspector exists to
        // surface. Log the attempt up front for those. Skip non-finite
        // values: do_set_power rejects them BEFORE the fault gate, so
        // they never park and the tail log below records the real
        // rejection — a pre-log here would be a phantom entry.
        if req.power.is_finite()
            && site.get(id).is_some()
            && site.runtime_of(id).command == CommandMode::Timeout
        {
            site.log_setpoint(
                id,
                SetpointEvent {
                    ts: chrono::Utc::now(),
                    kind,
                    value,
                    ttl_s,
                    outcome: SetpointOutcome::Rejected {
                        reason: "command channel hung (timeout fault); request parked \
                                 until the client deadline"
                            .to_string(),
                    },
                },
            );
        }
        let known = site.get(id).is_some();
        let response = self.do_set_power(req, power_type).await;

        let outcome = match &response {
            Ok(_) => SetpointOutcome::Accepted {
                effective_value: Some(value),
            },
            Err(s) => SetpointOutcome::Rejected {
                reason: s.message().to_string(),
            },
        };
        // Only log for registered components — log_setpoint mints a
        // per-id ring, so logging a not-found rejection would let a
        // client with random ids grow the map without bound (and
        // show phantom components in the UI). Matches the augment
        // path's `known` guard. Checked again after the apply: a
        // component registered while the command was in flight
        // (site import, UI eval) must still get its audit record.
        if known || site.get(id).is_some() {
            site.log_setpoint(
                id,
                SetpointEvent {
                    ts: chrono::Utc::now(),
                    kind,
                    value,
                    ttl_s,
                    outcome,
                },
            );
        }
        response
    }

    async fn receive_electrical_component_telemetry_stream(
        &self,
        request: tonic::Request<ReceiveElectricalComponentTelemetryStreamRequest>,
    ) -> Result<tonic::Response<Self::ReceiveElectricalComponentTelemetryStreamStream>, tonic::Status>
    {
        let req = request.into_inner();
        let id = req.electrical_component_id;
        let site = self.site.clone();

        let component = site
            .get(id)
            .ok_or_else(|| tonic::Status::not_found(format!("component {id} not found")))?;

        // A component whose data channel is absent: it exists in the
        // graph (so clients discover and subscribe to it) but the
        // telemetry pipeline has no channel behind it, so every stream
        // request is rejected with NOT_FOUND and a streaming client
        // retries the subscription indefinitely.
        if site.runtime_of(id).telemetry == TelemetryMode::NotFound {
            return Err(tonic::Status::not_found(
                r#"{ "kind": "NotFound", "message": "No data channel found for component", "source": "" }"#,
            ));
        }

        let interval = component.stream_interval();
        let jitter_pct = component.stream_jitter_pct().clamp(0.0, 100.0);

        // Optional metric allowlist. Per the proto:
        //   - filter absent           → all metrics (None below)
        //   - filter present, empty   → InvalidArgument
        //   - filter present, non-empty → only those metrics
        let metric_filter: Option<std::collections::HashSet<i32>> = match req.filter {
            None => None,
            Some(f) if f.metrics.is_empty() => {
                return Err(tonic::Status::invalid_argument(
                    "ComponentTelemetryStreamFilter.metrics must contain at least one metric",
                ));
            }
            Some(f) => Some(f.metrics.into_iter().collect()),
        };

        let (tx, rx) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            // SmallRng is `Send`; seed from thread_rng once at task
            // start (which is fine because spawning the task is sync).
            use rand::{Rng, SeedableRng, rngs::SmallRng};
            let mut rng = SmallRng::from_entropy();
            // Anchor the schedule on a moving target timestamp rather
            // than on `now` after the work finishes — otherwise the
            // tx.send / telemetry overhead accumulates each iteration
            // and the stream slowly drifts out beyond `interval` per
            // step. Pattern lifted from microsim's server loop.
            let mut next_due = SystemTime::now();
            // Capture the cancel epoch at task start. Each iteration
            // compares; a mismatch means `cancel_all_streams()` was
            // called, so we send the client a gRPC `CANCELLED` status
            // and exit — a server-initiated graceful stream cancel.
            let start_epoch = site.stream_cancel_epoch();
            loop {
                if site.stream_cancel_epoch() != start_epoch {
                    log::debug!("stream({id}): cancelled by epoch bump");
                    let _ = tx
                        .send(Err(tonic::Status::cancelled("Channel is closed")))
                        .await;
                    break;
                }
                // Re-read the runtime each iteration so a mid-stream
                // `(set-component-telemetry-mode)` flip takes effect
                // on the next sample boundary.
                let runtime = site.runtime_of(id);
                match runtime.telemetry {
                    TelemetryMode::Closed => {
                        log::debug!("stream({id}): closed by runtime mode");
                        break;
                    }
                    TelemetryMode::Silent => {
                        // Skip the send; still sleep for `step` so
                        // we cooperate with the client and re-check
                        // the mode at the next interval.
                    }
                    TelemetryMode::ErrorEmpty => {
                        // Send a sample with no metrics and just an
                        // ERROR state code.
                        let msg = crate::proto_conv::error_empty_to_proto(id);
                        if tx.send(Ok(msg)).await.is_err() {
                            log::debug!("stream({id}): client disconnected");
                            break;
                        }
                    }
                    TelemetryMode::NotFound => {
                        // Flipped to NotFound after the stream opened:
                        // terminate it with NOT_FOUND so the client
                        // reconnects and then hits the open-time
                        // rejection above, entering the retry loop.
                        let _ = tx
                            .send(Err(tonic::Status::not_found(
                                r#"{ "kind": "NotFound", "message": "No data channel found for component", "source": "" }"#,
                            )))
                            .await;
                        break;
                    }
                    TelemetryMode::Normal => {
                        let mut snapshot = site.telemetry_of(component.as_ref());
                        // Health override: a degraded device reports
                        // ERROR/STANDBY in its state code, regardless
                        // of what the physics layer thinks the
                        // component is doing this tick.
                        if let Some(label) = runtime.health.state_label() {
                            snapshot.component_state = Some(label);
                        }
                        let msg = telemetry_to_proto(
                            component.as_ref(),
                            &snapshot,
                            metric_filter.as_ref(),
                            site.sample_lag_ms(),
                        );
                        if tx.send(Ok(msg)).await.is_err() {
                            log::debug!("stream({id}): client disconnected");
                            break;
                        }
                    }
                }
                let factor: f32 = if jitter_pct > 0.0 {
                    let j = jitter_pct / 100.0;
                    1.0 + rng.gen_range(-j..=j)
                } else {
                    1.0
                };
                let step = Duration::from_secs_f32((interval.as_secs_f32() * factor).max(0.001));
                let target = next_due + step;
                let now = SystemTime::now();
                let dur = target.duration_since(now).unwrap_or(Duration::ZERO);
                // Wake on the dwell elapsing OR the client dropping.
                // Without this, a Silent stream that the client drops
                // would leak this task forever — tx.send only fires
                // (and only fails) in Normal mode.
                tokio::select! {
                    _ = tokio::time::sleep(dur) => {}
                    _ = tx.closed() => {
                        log::debug!("stream({id}): client disconnected (during dwell)");
                        break;
                    }
                }
                next_due = target.max(now);
            }
        });

        Ok(tonic::Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn augment_electrical_component_bounds(
        &self,
        request: tonic::Request<AugmentElectricalComponentBoundsRequest>,
    ) -> Result<tonic::Response<AugmentElectricalComponentBoundsResponse>, tonic::Status> {
        let req = request.into_inner();
        let target_metric = Metric::try_from(req.target_metric).map_err(|_| {
            tonic::Status::invalid_argument(format!("invalid metric type: {}", req.target_metric))
        })?;
        // Only the two AC power axes carry augmentable bounds. Every
        // other metric is a protocol error, and the message names the
        // metric that was asked for. The two axes journal distinct
        // kinds so `component/{id}/setpoints`, the event bus and the
        // setpoints CSV can tell a P augmentation from a Q one.
        let (axis, journal_kind) = match target_metric {
            Metric::AcPowerActive => (SetpointAxis::Active, SetpointKind::AugmentBounds),
            Metric::AcPowerReactive => {
                (SetpointAxis::Reactive, SetpointKind::AugmentReactiveBounds)
            }
            other => {
                return Err(tonic::Status::invalid_argument(format!(
                    "Unsupported metric type: {}. Only AC_POWER_ACTIVE and \
                     AC_POWER_REACTIVE are supported.",
                    other.as_str_name()
                )));
            }
        };

        let lifetime = resolve_lifetime(
            req.request_lifetime,
            AUGMENT_LIFETIME_MIN_S,
            self.config.metadata().default_augment_lifetime,
        )?;
        let now = chrono::Utc::now();
        let id = req.electrical_component_id;

        let site = self.site.clone();
        let generation = site.run_generation();
        let known = site.get(id).is_some();
        // Timeout-fault components park the request inside
        // gate_runtime_faults — log the attempt up front, same
        // rationale as the SetPower path.
        if known && site.runtime_of(id).command == CommandMode::Timeout {
            site.log_setpoint(
                id,
                SetpointEvent {
                    ts: now,
                    kind: journal_kind,
                    value: 0.0,
                    ttl_s: Some(lifetime.as_secs()),
                    outcome: SetpointOutcome::Rejected {
                        reason: "command channel hung (timeout fault); request parked \
                                 until the client deadline"
                            .to_string(),
                    },
                },
            );
        }
        let response = if known {
            // Reject the augmentation for an errored/standby/unreachable
            // device, same as a setpoint command.
            match self.gate_runtime_faults(id).await {
                // The gateway checks the shape, then checks the live
                // envelope and stores the augmentation under one
                // lock.
                Ok(_) => match site.gateway().augment(
                    id,
                    generation,
                    axis,
                    VecBounds::new(req.bounds),
                    lifetime,
                ) {
                    Ok(deadline) => Ok(tonic::Response::new(
                        AugmentElectricalComponentBoundsResponse {
                            valid_until_time: Some(crate::proto_conv::datetime_to_ts(deadline)),
                        },
                    )),
                    Err(AugmentError::Malformed(m)) => Err(tonic::Status::invalid_argument(m)),
                    // The component stores no augmentation on this
                    // axis, so there is nothing an ACK could promise.
                    // Same answer the setpoint path gives a component
                    // that takes no setpoint on this axis.
                    Err(AugmentError::Unsupported) => Err(tonic::Status::unimplemented(format!(
                        "component {id} stores no augmentation for {}",
                        target_metric.as_str_name()
                    ))),
                    Err(e @ AugmentError::Disjoint(_)) => {
                        Err(tonic::Status::invalid_argument(e.to_string()))
                    }
                    Err(e @ AugmentError::NotFound(_)) => {
                        Err(tonic::Status::not_found(e.to_string()))
                    }
                    Err(e @ AugmentError::SiteReset) => {
                        Err(tonic::Status::failed_precondition(e.to_string()))
                    }
                },
                Err(status) => Err(status),
            }
        } else {
            Err(tonic::Status::not_found(format!(
                "component {id} not found"
            )))
        };

        let outcome = match &response {
            Ok(_) => SetpointOutcome::Accepted {
                effective_value: None,
            },
            Err(s) => SetpointOutcome::Rejected {
                reason: s.message().to_string(),
            },
        };
        // Don't auto-create a zombie SetpointLog ring for an id that
        // was never registered — SetPower returns before logging on
        // not-found; match it.
        if known {
            site.log_setpoint(
                id,
                SetpointEvent {
                    ts: now,
                    kind: journal_kind,
                    value: 0.0,
                    ttl_s: Some(lifetime.as_secs()),
                    outcome,
                },
            );
        }
        response
    }

    // --- Unimplemented optional surface --------------------------------------

    async fn list_sensors(
        &self,
        _request: tonic::Request<ListSensorRequest>,
    ) -> Result<tonic::Response<ListSensorsResponse>, tonic::Status> {
        // macrocosim does not model sensors; report an empty inventory.
        Ok(tonic::Response::new(ListSensorsResponse {
            sensors: Vec::new(),
        }))
    }
    async fn receive_sensor_telemetry_stream(
        &self,
        _request: tonic::Request<ReceiveSensorTelemetryStreamRequest>,
    ) -> Result<tonic::Response<Self::ReceiveSensorTelemetryStreamStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("sensors are not modeled"))
    }
    async fn start_electrical_component(
        &self,
        _request: tonic::Request<StartElectricalComponentRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("start not yet implemented"))
    }
    async fn put_electrical_component_in_standby(
        &self,
        _request: tonic::Request<PutElectricalComponentInStandbyRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("standby not yet implemented"))
    }
    async fn stop_electrical_component(
        &self,
        _request: tonic::Request<StopElectricalComponentRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("stop not yet implemented"))
    }
    async fn ack_electrical_component_error(
        &self,
        _request: tonic::Request<AckElectricalComponentErrorRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "ack-error not yet implemented",
        ))
    }
}

fn gateway_error_to_status(err: GatewayError) -> tonic::Status {
    match err {
        GatewayError::NotFound(_) => tonic::Status::not_found(err.to_string()),
        GatewayError::NoAxis { .. } => tonic::Status::unimplemented(err.to_string()),
        GatewayError::NonFinite { .. } => tonic::Status::invalid_argument(err.to_string()),
        GatewayError::OutOfEnvelope(_) | GatewayError::SiteReset => {
            tonic::Status::failed_precondition(err.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each gateway refusal maps to today's status code.
    #[test]
    fn gateway_errors_map_to_todays_codes() {
        use tonic::Code;
        let code = |e| gateway_error_to_status(e).code();
        assert_eq!(code(GatewayError::NotFound(1)), Code::NotFound);
        assert_eq!(
            code(GatewayError::NoAxis {
                id: 1,
                axis: SetpointAxis::Active
            }),
            Code::Unimplemented
        );
        assert_eq!(
            code(GatewayError::NonFinite { value: f32::NAN }),
            Code::InvalidArgument
        );
        assert_eq!(
            code(GatewayError::OutOfEnvelope("x envelope".into())),
            Code::FailedPrecondition
        );
        let reset = gateway_error_to_status(GatewayError::SiteReset);
        assert_eq!(reset.code(), Code::FailedPrecondition);
        assert!(reset.message().contains("site was reset"));
    }
}
