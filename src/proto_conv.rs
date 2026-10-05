//! Convert macrocosim's Rust-side `Telemetry` and `Category` into the
//! proto messages that the Microgrid gRPC service emits.
//!
//! Lives in its own module so the server code stays focused on RPC
//! plumbing.

use std::collections::HashSet;

use prost_types::Timestamp;

use crate::{
    proto::common::{
        metrics::{
            Bounds, Metric, MetricSample, MetricValueVariant, SimpleMetricValue,
            metric_value_variant,
        },
        microgrid::electrical_components::{
            Battery, BatteryType, ElectricalComponent, ElectricalComponentCategory,
            ElectricalComponentCategorySpecificInfo, ElectricalComponentOperationalMode,
            ElectricalComponentStateCode, ElectricalComponentStateSnapshot,
            ElectricalComponentTelemetry, EvCharger, EvChargerType, GridConnectionPoint, Inverter,
            InverterType, MetricConfigBounds, electrical_component_category_specific_info::Kind,
        },
    },
    proto::microgrid::ReceiveElectricalComponentTelemetryStreamResponse,
    sim::{Category, MicrogridSite, OperationalMode, SimulatedComponent, Telemetry},
};

/// The one chrono→proto Timestamp conversion. The nanos cast is
/// subtle enough (`timestamp_subsec_nanos` is u32, proto wants i32)
/// that every hand-rolled copy is a drift hazard — the dispatch
/// store, the gRPC servers, and macroctl all funnel through here.
pub fn datetime_to_ts(dt: chrono::DateTime<chrono::Utc>) -> Timestamp {
    Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

/// Subscriber's metric allowlist. `None` means "all metrics"; `Some`
/// is the set of `Metric as i32` values the client asked for.
pub type MetricFilter<'a> = Option<&'a HashSet<i32>>;

#[inline]
fn allowed(filter: MetricFilter<'_>, metric: Metric) -> bool {
    match filter {
        None => true,
        Some(set) => set.contains(&(metric as i32)),
    }
}

pub fn category_to_proto(c: Category) -> ElectricalComponentCategory {
    match c {
        Category::Grid => ElectricalComponentCategory::GridConnectionPoint,
        Category::Meter => ElectricalComponentCategory::Meter,
        Category::Inverter => ElectricalComponentCategory::Inverter,
        Category::Battery => ElectricalComponentCategory::Battery,
        Category::EvCharger => ElectricalComponentCategory::EvCharger,
        Category::Chp => ElectricalComponentCategory::Chp,
        Category::WindTurbine => ElectricalComponentCategory::WindTurbine,
        Category::SteamBoiler => ElectricalComponentCategory::SteamBoiler,
        Category::PowerTransformer => ElectricalComponentCategory::PowerTransformer,
        Category::Breaker => ElectricalComponentCategory::Breaker,
    }
}

fn operational_mode_to_proto(m: OperationalMode) -> ElectricalComponentOperationalMode {
    match m {
        OperationalMode::Unspecified => ElectricalComponentOperationalMode::Unspecified,
        OperationalMode::Inactive => ElectricalComponentOperationalMode::Inactive,
        // "Telemtry" is the upstream proto's own spelling.
        OperationalMode::TelemetryOnly => ElectricalComponentOperationalMode::TelemtryOnly,
        OperationalMode::ControlOnly => ElectricalComponentOperationalMode::ControlOnly,
        OperationalMode::ControlAndTelemetry => {
            ElectricalComponentOperationalMode::ControlAndTelemetry
        }
    }
}

/// Build the static, type-defining `ElectricalComponent` for a
/// component (used by `ListElectricalComponents` and the assets
/// listing). `microgrid_id` comes from the caller — the per-port
/// Microgrid server knows its own id, the assets server takes it
/// from the request — so multi-microgrid clients can route by it.
/// `site` resolves the display-name override (`:name` /
/// `rename-component` land in the site's `name_overrides`, not on the
/// component) and the operational mode.
pub fn make_component_proto(
    c: &dyn SimulatedComponent,
    site: &MicrogridSite,
    microgrid_id: u64,
) -> ElectricalComponent {
    let cat = category_to_proto(c.category());
    let kind = match cat {
        ElectricalComponentCategory::Inverter => Some(Kind::Inverter(Inverter {
            r#type: match c.subtype() {
                Some("solar") | Some("pv") => InverterType::Pv as i32,
                Some("hybrid") => InverterType::Hybrid as i32,
                _ => InverterType::Battery as i32,
            },
        })),
        ElectricalComponentCategory::Battery => Some(Kind::Battery(Battery {
            // Alias set must stay in sync with graph_adapter's
            // `lift_category`, or the same component classifies
            // differently in gRPC listings vs. graph validation.
            r#type: match c.subtype() {
                Some("li-ion") | Some("liion") => BatteryType::LiIon as i32,
                Some("na-ion") | Some("naion") => BatteryType::NaIon as i32,
                _ => BatteryType::Unspecified as i32,
            },
        })),
        ElectricalComponentCategory::EvCharger => Some(Kind::EvCharger(EvCharger {
            r#type: match c.subtype() {
                Some("ac") => EvChargerType::Ac as i32,
                Some("dc") => EvChargerType::Dc as i32,
                Some("hybrid") => EvChargerType::Hybrid as i32,
                _ => EvChargerType::Unspecified as i32,
            },
        })),
        ElectricalComponentCategory::GridConnectionPoint => {
            Some(Kind::GridConnectionPoint(GridConnectionPoint {
                rated_fuse_current: c.rated_fuse_current().unwrap_or(0),
            }))
        }
        _ => None,
    };

    let mut bounds = Vec::new();
    if let Some((lower, upper)) = c.rated_active_bounds() {
        let metric = if cat == ElectricalComponentCategory::Battery {
            Metric::DcPower
        } else {
            Metric::AcPowerActive
        };
        bounds.push(MetricConfigBounds {
            metric: metric as i32,
            config_bounds: Some(Bounds {
                lower: Some(lower),
                upper: Some(upper),
            }),
        });
        // Reactive config bounds: the capability hull — the widest Q
        // reachable at any P in the rated range — not a live sample at
        // the current P. "Static" here means computed fresh from the
        // caps at read time, not fixed at construction: the caps
        // themselves are runtime-mutable (`set-reactive-pf-limit` /
        // `set-reactive-apparent-va`), so this hull tracks whatever PF
        // / kVA band is live right now, not a frozen nameplate value.
        // A component with no reactive caps (`reactive_limits() ==
        // None`) honestly advertises `(0.0, 0.0)` instead of a fake
        // ±p_max edge.
        if cat != ElectricalComponentCategory::Battery {
            let p_max = lower.abs().max(upper.abs());
            let (rlo, rhi) = c
                .reactive_limits()
                .map(|r| r.reactive_capability().hull(p_max))
                .unwrap_or((0.0, 0.0));
            bounds.push(MetricConfigBounds {
                metric: Metric::AcPowerReactive as i32,
                config_bounds: Some(Bounds {
                    lower: Some(rlo),
                    upper: Some(rhi),
                }),
            });
        }
    }

    ElectricalComponent {
        id: c.id(),
        name: site
            .display_name(c.id())
            .unwrap_or_else(|| c.name().to_string()),
        category: cat as i32,
        microgrid_id,
        operational_mode: operational_mode_to_proto(site.operational_mode(c.id())) as i32,
        category_specific_info: Some(ElectricalComponentCategorySpecificInfo { kind }),
        metric_config_bounds: bounds,
        ..Default::default()
    }
}

/// Build a streaming telemetry response for a component, optionally
/// limited to a subset of metrics chosen by the subscriber.
pub fn telemetry_to_proto(
    c: &dyn SimulatedComponent,
    t: &Telemetry,
    filter: MetricFilter<'_>,
    sample_lag_ms: u64,
) -> ReceiveElectricalComponentTelemetryStreamResponse {
    let mut wall_now = std::time::SystemTime::now();
    if sample_lag_ms > 0 {
        wall_now -= std::time::Duration::from_millis(sample_lag_ms);
    }
    let now = Some(Timestamp::from(wall_now));
    let cat = c.category();

    let mut samples = Vec::new();
    let mut states = Vec::new();

    if let Some(s) = t.frequency_hz
        && allowed(filter, Metric::AcFrequency)
    {
        samples.push(simple_sample(now, Metric::AcFrequency, s));
    }
    // Per-phase triples all convert the same way: one metric per
    // phase, filter-gated.
    let per_phase = [
        (
            t.per_phase_voltage_v,
            [
                Metric::AcVoltagePhase1N,
                Metric::AcVoltagePhase2N,
                Metric::AcVoltagePhase3N,
            ],
        ),
        (
            t.per_phase_current_a,
            [
                Metric::AcCurrentPhase1,
                Metric::AcCurrentPhase2,
                Metric::AcCurrentPhase3,
            ],
        ),
        (
            t.per_phase_active_w,
            [
                Metric::AcPowerActivePhase1,
                Metric::AcPowerActivePhase2,
                Metric::AcPowerActivePhase3,
            ],
        ),
        (
            t.per_phase_reactive_var,
            [
                Metric::AcPowerReactivePhase1,
                Metric::AcPowerReactivePhase2,
                Metric::AcPowerReactivePhase3,
            ],
        ),
    ];
    for (triple, metrics) in per_phase {
        let Some((v1, v2, v3)) = triple else { continue };
        for (metric, value) in metrics.into_iter().zip([v1, v2, v3]) {
            if allowed(filter, metric) {
                samples.push(simple_sample(now, metric, value));
            }
        }
    }
    if let Some(p) = t.active_power_w
        && allowed(filter, Metric::AcPowerActive)
    {
        let mut sample = simple_sample(now, Metric::AcPowerActive, p);
        if let Some(b) = &t.active_power_bounds {
            sample.bounds = b.0.clone();
        }
        samples.push(sample);
    }
    if let Some(q) = t.reactive_power_var
        && allowed(filter, Metric::AcPowerReactive)
    {
        let mut sample = simple_sample(now, Metric::AcPowerReactive, q);
        // Live stream does not collapse — every band goes out, same
        // as the active arm above.
        if let Some(b) = &t.reactive_power_bounds {
            sample.bounds = b.0.clone();
        }
        samples.push(sample);
    }

    // DC / battery-flavoured samples
    if let Some(cap) = t.capacity_wh
        && allowed(filter, Metric::BatteryCapacity)
    {
        samples.push(simple_sample(now, Metric::BatteryCapacity, cap));
    }
    // The car behind a charger is the simulator's business, not the
    // API's: its SoC rides the internal telemetry only.
    if let Some(soc) = t.soc_pct
        && cat != Category::EvCharger
        && allowed(filter, Metric::BatterySocPct)
    {
        let mut s = simple_sample(now, Metric::BatterySocPct, soc);
        if let (Some(l), Some(u)) = (t.soc_lower_pct, t.soc_upper_pct) {
            s.bounds = vec![Bounds {
                lower: Some(l),
                upper: Some(u),
            }];
        }
        samples.push(s);
    }
    if let Some(v) = t.dc_voltage_v
        && allowed(filter, Metric::DcVoltage)
    {
        samples.push(simple_sample(now, Metric::DcVoltage, v));
    }
    if let Some(i) = t.dc_current_a
        && allowed(filter, Metric::DcCurrent)
    {
        samples.push(simple_sample(now, Metric::DcCurrent, i));
    }
    if let Some(p) = t.dc_power_w
        && allowed(filter, Metric::DcPower)
    {
        let mut sample = simple_sample(now, Metric::DcPower, p);
        // Only attach bounds to DC for batteries — for AC components
        // they are attached above.
        if cat == Category::Battery
            && let Some(b) = &t.active_power_bounds
        {
            sample.bounds = b.0.clone();
        }
        samples.push(sample);
    }

    if let Some(s) = t.component_state
        && let Some(code) = parse_state(s)
    {
        states.push(code as i32);
    }
    if let Some(s) = t.relay_state
        && let Some(code) = parse_state(s)
    {
        states.push(code as i32);
    }
    states.extend(
        t.cable_states
            .iter()
            .copied()
            .filter_map(parse_state)
            .map(|code| code as i32),
    );

    // No state codes resolved → no snapshot at all; an empty snapshot
    // would make a metrics-only frame look like a state report.
    let state_snapshots = if states.is_empty() {
        Vec::new()
    } else {
        vec![ElectricalComponentStateSnapshot {
            origin_time: now,
            states,
            ..Default::default()
        }]
    };
    ReceiveElectricalComponentTelemetryStreamResponse {
        telemetry: Some(ElectricalComponentTelemetry {
            electrical_component_id: c.id(),
            metric_samples: samples,
            state_snapshots,
        }),
    }
}

/// Build a telemetry response with no metric samples and just an
/// ERROR state code. Used by `TelemetryMode::ErrorEmpty` to model a
/// device that streams only an error state with zero metrics.
pub fn error_empty_to_proto(
    component_id: u64,
) -> ReceiveElectricalComponentTelemetryStreamResponse {
    let now = Some(Timestamp::from(std::time::SystemTime::now()));
    ReceiveElectricalComponentTelemetryStreamResponse {
        telemetry: Some(ElectricalComponentTelemetry {
            electrical_component_id: component_id,
            metric_samples: Vec::new(),
            state_snapshots: vec![ElectricalComponentStateSnapshot {
                origin_time: now,
                states: vec![ElectricalComponentStateCode::Error as i32],
                ..Default::default()
            }],
        }),
    }
}

fn simple_sample(now: Option<Timestamp>, metric: Metric, value: f32) -> MetricSample {
    MetricSample {
        sample_time: now,
        metric: metric as i32,
        value: Some(MetricValueVariant {
            metric_value_variant: Some(metric_value_variant::MetricValueVariant::SimpleMetric(
                SimpleMetricValue { value },
            )),
        }),
        ..Default::default()
    }
}

fn parse_state(s: &str) -> Option<ElectricalComponentStateCode> {
    use std::str::FromStr;
    ElectricalComponentStateCode::from_str(s).ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        proto::common::metrics::Metric,
        proto_conv::{make_component_proto, telemetry_to_proto},
        sim::{
            Battery, BatteryInverter, EvCharger, Grid, MicrogridSite, SimulatedComponent,
            battery::BatteryConfig, ev_charger::EvChargerConfig,
            inverter::battery_inverter::BatteryInverterConfig, reactive::ReactiveCapability,
        },
    };

    /// Pull the AC_POWER_REACTIVE config-bounds pair `make_component_proto`
    /// advertises for a component, if it advertised one at all.
    fn reactive_config_bounds(comp: &dyn SimulatedComponent) -> Option<(f32, f32)> {
        let site = MicrogridSite::new();
        let proto = make_component_proto(comp, &site, 1);
        proto
            .metric_config_bounds
            .iter()
            .find(|b| b.metric == Metric::AcPowerReactive as i32)
            .map(|b| {
                let bounds = b.config_bounds.as_ref().expect("bounds present");
                (
                    bounds.lower.expect("lower set"),
                    bounds.upper.expect("upper set"),
                )
            })
    }

    /// A PF-only inverter (rated ±30 kW, k=0.35) advertises the PF
    /// hull ±10.5 kVAr as its static config bound EVEN WHILE IDLE
    /// (P=0) — the config bound is the capability hull over the whole
    /// rated P range, not a live sample at the current P (which for a
    /// PF-only cap is 0 at P=0, the pre-fix behavior this replaces).
    #[test]
    fn pf_only_inverter_advertises_pf_hull_while_idle() {
        let cfg = BatteryInverterConfig {
            rated_lower_w: -30_000.0,
            rated_upper_w: 30_000.0,
            reactive: ReactiveCapability {
                pf_limit: Some(0.35),
                apparent_va: None,
            },
            ..Default::default()
        };
        let inv = BatteryInverter::new(1, Duration::from_secs(1), cfg);
        let (lo, hi) =
            reactive_config_bounds(&inv).expect("reactive config bounds present for a Q axis");
        assert!((lo + 10_500.0).abs() < 1.0, "got lo={lo}");
        assert!((hi - 10_500.0).abs() < 1.0, "got hi={hi}");
    }

    /// A kVA-only inverter advertises ±S as its config bound — the
    /// hull is widest at P=0 for a kVA-only cap, so this one happens
    /// to coincide with the live value at idle, but it's the hull
    /// that's being advertised.
    #[test]
    fn kva_only_inverter_advertises_kva_hull() {
        let cfg = BatteryInverterConfig {
            rated_lower_w: -30_000.0,
            rated_upper_w: 30_000.0,
            reactive: ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(20_000.0),
            },
            ..Default::default()
        };
        let inv = BatteryInverter::new(1, Duration::from_secs(1), cfg);
        let (lo, hi) = reactive_config_bounds(&inv).expect("reactive config bounds present");
        assert!((lo + 20_000.0).abs() < 1.0, "got lo={lo}");
        assert!((hi - 20_000.0).abs() < 1.0, "got hi={hi}");
    }

    /// Both caps set: the config bound is the PF-line/kVA-circle
    /// crossing value from `ReactiveCapability::hull`, not either cap
    /// alone. k=1, s=5000, rated 10000 → crossing at P*=s/sqrt(2)
    /// (≤ rated) → hull = k*s/sqrt(2).
    #[test]
    fn both_caps_inverter_advertises_crossing_hull() {
        let cfg = BatteryInverterConfig {
            rated_lower_w: -10_000.0,
            rated_upper_w: 10_000.0,
            reactive: ReactiveCapability {
                pf_limit: Some(1.0),
                apparent_va: Some(5_000.0),
            },
            ..Default::default()
        };
        let inv = BatteryInverter::new(1, Duration::from_secs(1), cfg);
        let (lo, hi) = reactive_config_bounds(&inv).expect("reactive config bounds present");
        let expected = 5_000.0 / 2f32.sqrt();
        assert!((hi - expected).abs() < 1.0, "got hi={hi}");
        assert!((lo + expected).abs() < 1.0, "got lo={lo}");
    }

    /// Neither cap set (the `:reactive-pf-limit 0 :reactive-apparent-va
    /// 0` config) advertises ±p_rated — the neither-cap fallback cone
    /// from `ReactiveCapability::hull`.
    #[test]
    fn neither_cap_inverter_advertises_rated_p_as_hull() {
        let cfg = BatteryInverterConfig {
            rated_lower_w: -8_000.0,
            rated_upper_w: 8_000.0,
            reactive: ReactiveCapability {
                pf_limit: None,
                apparent_va: None,
            },
            ..Default::default()
        };
        let inv = BatteryInverter::new(1, Duration::from_secs(1), cfg);
        let (lo, hi) = reactive_config_bounds(&inv).expect("reactive config bounds present");
        assert!((lo + 8_000.0).abs() < 1.0, "got lo={lo}");
        assert!((hi - 8_000.0).abs() < 1.0, "got hi={hi}");
    }

    /// A component with no Q axis at all (EV charger) advertises an
    /// honest `(0.0, 0.0)` config bound — not the old `±p_max` fake
    /// fallback that made it look like it had reactive headroom it
    /// doesn't model.
    #[test]
    fn ev_charger_advertises_zero_reactive_config_bounds() {
        let ev = EvCharger::new(1, Duration::from_secs(1), EvChargerConfig::default());
        let (lo, hi) =
            reactive_config_bounds(&ev).expect("a config-bounds entry is present, honestly zero");
        assert_eq!((lo, hi), (0.0, 0.0));
    }

    /// Same honest-zero requirement for the grid connection point.
    #[test]
    fn grid_advertises_zero_reactive_config_bounds() {
        let grid = Grid::new(1, 100, Some((-50_000.0, 50_000.0)), 0.0);
        let (lo, hi) =
            reactive_config_bounds(&grid).expect("a config-bounds entry is present, honestly zero");
        assert_eq!((lo, hi), (0.0, 0.0));
    }

    /// A battery still advertises `DcPower` only — no `AcPowerReactive`
    /// entry at all, since batteries are skipped by the reactive arm
    /// regardless of Q-axis presence.
    #[test]
    fn battery_advertises_dc_power_only_no_reactive_entry() {
        let bat = Battery::new(1, Duration::from_secs(1), BatteryConfig::default());
        let site = MicrogridSite::new();
        let proto = make_component_proto(&bat, &site, 1);
        let metrics: Vec<i32> = proto
            .metric_config_bounds
            .iter()
            .map(|b| b.metric)
            .collect();
        assert!(metrics.contains(&(Metric::DcPower as i32)));
        assert!(!metrics.contains(&(Metric::AcPowerReactive as i32)));
    }

    /// A P-only AC component (the EV charger) streams its explicit
    /// zero Q as an `AcPowerReactive` sample rather than omitting the
    /// metric, so the WS feed and the UI read "settled at 0" instead
    /// of "no data". This pins the streaming half of that honesty;
    /// upstream convergence does not depend on it (the aggregation
    /// COALESCEs a missing slot to 0.0).
    #[test]
    fn ev_streaming_telemetry_emits_zero_reactive_sample() {
        let w = MicrogridSite::new();
        let ev = EvCharger::new(1, Duration::from_secs(1), EvChargerConfig::default());
        let t = ev.telemetry(&w);
        let resp = telemetry_to_proto(&ev, &t, None, 0);
        let telemetry = resp.telemetry.expect("telemetry present");
        let q_sample = telemetry
            .metric_samples
            .iter()
            .find(|s| s.metric == crate::proto::common::metrics::Metric::AcPowerReactive as i32)
            .expect("AcPowerReactive sample must be present for a P-only AC component");
        let value = match &q_sample.value.as_ref().unwrap().metric_value_variant {
            Some(crate::proto::common::metrics::metric_value_variant::MetricValueVariant::SimpleMetric(v)) => v.value,
            other => panic!("expected a simple metric value, got {other:?}"),
        };
        assert_eq!(value, 0.0);
    }

    /// A component whose reactive bounds carry two disjoint bands (a
    /// live Q augmentation splitting the caps band) streams BOTH
    /// bands in its `AcPowerReactive` sample — the gRPC live stream
    /// does not collapse, unlike the WS/history scalar (first band's
    /// edges) and `Telemetry::metric_value` (envelope extremes —
    /// first band's lower, last band's upper).
    #[test]
    fn two_band_reactive_bounds_stream_every_band() {
        let w = MicrogridSite::new();
        let ev = EvCharger::new(1, Duration::from_secs(1), EvChargerConfig::default());
        let mut t = ev.telemetry(&w);
        t.reactive_power_var = Some(0.0);
        t.reactive_power_bounds = Some(crate::sim::bounds::VecBounds(vec![
            crate::proto::common::metrics::Bounds {
                lower: Some(-2000.0),
                upper: Some(-500.0),
            },
            crate::proto::common::metrics::Bounds {
                lower: Some(500.0),
                upper: Some(2000.0),
            },
        ]));
        let resp = telemetry_to_proto(&ev, &t, None, 0);
        let telemetry = resp.telemetry.expect("telemetry present");
        let q_sample = telemetry
            .metric_samples
            .iter()
            .find(|s| s.metric == crate::proto::common::metrics::Metric::AcPowerReactive as i32)
            .expect("AcPowerReactive sample must be present");
        assert_eq!(
            q_sample.bounds.len(),
            2,
            "both bands must stream, got {:?}",
            q_sample.bounds
        );
        assert_eq!(q_sample.bounds[0].lower, Some(-2000.0));
        assert_eq!(q_sample.bounds[0].upper, Some(-500.0));
        assert_eq!(q_sample.bounds[1].lower, Some(500.0));
        assert_eq!(q_sample.bounds[1].upper, Some(2000.0));
    }

    /// A zero-headroom Q envelope normalizes to a PRESENT single
    /// `(0.0, 0.0)` band (see `VecBounds::or_zero_band`), so the live
    /// gRPC stream carries exactly one zero band rather than omitting
    /// `sample.bounds` entirely.
    #[test]
    fn zero_headroom_reactive_bounds_stream_a_single_zero_band() {
        let w = MicrogridSite::new();
        let ev = EvCharger::new(1, Duration::from_secs(1), EvChargerConfig::default());
        let mut t = ev.telemetry(&w);
        t.reactive_power_var = Some(0.0);
        t.reactive_power_bounds = Some(crate::sim::bounds::VecBounds::single(0.0, 0.0));
        let resp = telemetry_to_proto(&ev, &t, None, 0);
        let telemetry = resp.telemetry.expect("telemetry present");
        let q_sample = telemetry
            .metric_samples
            .iter()
            .find(|s| s.metric == crate::proto::common::metrics::Metric::AcPowerReactive as i32)
            .expect("AcPowerReactive sample must be present");
        assert_eq!(
            q_sample.bounds.len(),
            1,
            "zero headroom must still stream one band, got {:?}",
            q_sample.bounds
        );
        assert_eq!(q_sample.bounds[0].lower, Some(0.0));
        assert_eq!(q_sample.bounds[0].upper, Some(0.0));
    }

    fn ev_states(w: &MicrogridSite, ev: &dyn crate::sim::SimulatedComponent) -> Vec<i32> {
        let t = ev.telemetry(w);
        telemetry_to_proto(ev, &t, None, 0)
            .telemetry
            .unwrap()
            .state_snapshots
            .iter()
            .flat_map(|s| s.states.clone())
            .collect()
    }

    #[test]
    fn empty_charger_streams_unplugged_and_ready() {
        use crate::proto::common::microgrid::electrical_components::ElectricalComponentStateCode as C;
        let w = MicrogridSite::new();
        let ev = EvCharger::new(1, Duration::from_secs(1), EvChargerConfig::default());
        let states = ev_states(&w, &ev);
        assert!(states.contains(&(C::Ready as i32)), "{states:?}");
        assert!(
            states.contains(&(C::EvChargingCableUnplugged as i32)),
            "{states:?}"
        );
    }

    #[test]
    fn charging_car_streams_locked_and_charging_but_no_soc() {
        use crate::proto::common::microgrid::electrical_components::ElectricalComponentStateCode as C;
        use crate::sim::{ev_charger::instant, ev_presets::test_car};
        let w = MicrogridSite::new();
        w.register(EvCharger::new(1, Duration::from_secs(1), instant()));
        let ev = w.get(1).unwrap();
        ev.ev_port()
            .unwrap()
            .plug_ev(test_car("sedan", None))
            .unwrap();
        w.gateway()
            .command(1, crate::timeout_tracker::SetpointAxis::Active, 22_000.0)
            .unwrap();
        w.tick_n(2, Duration::from_secs(1));
        let states = ev_states(&w, ev.as_ref());
        assert!(states.contains(&(C::Charging as i32)), "{states:?}");
        assert!(
            states.contains(&(C::EvChargingCableLockedAtEv as i32)),
            "{states:?}"
        );
        assert!(
            states.contains(&(C::EvChargingCableLockedAtStation as i32)),
            "{states:?}"
        );
        let t = ev.telemetry(&w);
        assert!(
            t.soc_pct.is_some(),
            "internal telemetry keeps the SoC for the UI"
        );
        let metrics: Vec<i32> = telemetry_to_proto(ev.as_ref(), &t, None, 0)
            .telemetry
            .unwrap()
            .metric_samples
            .iter()
            .map(|s| s.metric)
            .collect();
        assert!(
            !metrics.contains(&(Metric::BatterySocPct as i32)),
            "the API must not see the car: {metrics:?}"
        );
        assert!(
            !metrics.contains(&(Metric::BatteryCapacity as i32)),
            "the API must not see the car's pack either: {metrics:?}"
        );
        assert!(
            metrics.contains(&(Metric::AcPowerActive as i32)),
            "{metrics:?}"
        );
    }
}
