pub mod battery_inverter;
pub mod solar_inverter;

pub use battery_inverter::BatteryInverter;
pub use solar_inverter::SolarInverter;

use std::time::Duration;

use crate::sim::{
    Category, MicrogridSite, Telemetry,
    component::power_state,
    meter::{per_phase_apparent_current, split_per_phase},
    reactive::ReactiveCapability,
};

/// The device delay a component gets when `:device-delay-s` is not
/// given; it is not written into a managed file.
pub(crate) const DEFAULT_DEVICE_DELAY: Duration = Duration::from_millis(100);

/// The `:device-delay-s` kwarg for `d`, or `None` when `d` is the
/// default and so stays out of the rendered constructor.
pub(crate) fn device_delay_kw(d: Duration) -> Option<(&'static str, String)> {
    (d != DEFAULT_DEVICE_DELAY)
        .then(|| (":device-delay-s", crate::lisp::lisp_float(d.as_secs_f64())))
}

/// Telemetry shared by both inverters: P/Q with per-phase splits,
/// grid voltage/frequency and apparent currents. The bounds come from
/// the gateway (`MicrogridSite::telemetry_of`). The inverters differ
/// only in where `p` comes from (measured AC output vs device
/// output).
pub(crate) fn inverter_telemetry(id: u64, site: &MicrogridSite, p: f32, q: f32) -> Telemetry {
    let grid = site.grid_state();
    let pp = split_per_phase(p, grid.voltage_per_phase);
    let qpp = split_per_phase(q, grid.voltage_per_phase);
    Telemetry {
        id,
        category: Some(Category::Inverter),
        active_power_w: Some(p),
        reactive_power_var: Some(q),
        per_phase_active_w: Some(pp),
        per_phase_reactive_var: Some(qpp),
        per_phase_voltage_v: Some(grid.voltage_per_phase),
        per_phase_current_a: Some(per_phase_apparent_current(pp, qpp, grid.voltage_per_phase)),
        frequency_hz: Some(grid.frequency_hz),
        component_state: Some(power_state(p)),
        ..Default::default()
    }
}

/// The fields `BatteryInverter` and `SolarInverter` share for
/// [`common_inverter_kwargs`]. A plain field bag (rather than either
/// config struct) since `BatteryInverterConfig` and
/// `SolarInverterConfig` are unrelated types.
pub(crate) struct CommonInverterCfg {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub interval: Duration,
    pub stream_jitter_pct: f32,
    pub reactive: ReactiveCapability,
    pub reactive_command_delay: Duration,
    pub reactive_ramp_rate_var_per_s: f32,
    pub device_delay: Duration,
}

/// Construction kwargs shared by `BatteryInverter` and
/// `SolarInverter`: rated bounds, command-delay, ramp rate, interval,
/// jitter, and the reactive envelope. `SolarInverter` appends its own
/// `:sunlight%` kwarg on top of this.
pub(crate) fn common_inverter_kwargs(cfg: CommonInverterCfg) -> Vec<(&'static str, String)> {
    let lf = crate::lisp::lisp_float32;
    let mut kw = vec![
        (":rated-lower", lf(cfg.rated_lower_w)),
        (":rated-upper", lf(cfg.rated_upper_w)),
        (
            ":command-delay-s",
            crate::lisp::lisp_float(cfg.command_delay.as_secs_f64()),
        ),
    ];
    if cfg.ramp_rate_w_per_s.is_finite() {
        kw.push((":ramp-rate", lf(cfg.ramp_rate_w_per_s)));
    }
    if cfg.interval != Duration::from_millis(1000) {
        kw.push((
            ":interval-s",
            crate::lisp::lisp_float(cfg.interval.as_secs_f64()),
        ));
    }
    if cfg.stream_jitter_pct != 0.0 {
        kw.push((":stream-jitter-pct", lf(cfg.stream_jitter_pct)));
    }
    // Disabled reactive caps must pin as literal `0`, not be omitted —
    // omitting would resurrect the 0.35 PF default at load time.
    kw.push((
        ":reactive-pf-limit",
        cfg.reactive.pf_limit.map(lf).unwrap_or_else(|| "0".into()),
    ));
    kw.push((
        ":reactive-apparent-va",
        cfg.reactive
            .apparent_va
            .map(lf)
            .unwrap_or_else(|| "0".into()),
    ));
    kw.push((
        ":reactive-command-delay-s",
        crate::lisp::lisp_float(cfg.reactive_command_delay.as_secs_f64()),
    ));
    if cfg.reactive_ramp_rate_var_per_s.is_finite() {
        kw.push((":reactive-ramp-rate", lf(cfg.reactive_ramp_rate_var_per_s)));
    }
    kw.extend(device_delay_kw(cfg.device_delay));
    kw
}
