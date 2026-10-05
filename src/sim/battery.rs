use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, MicrogridSite, SimulatedComponent, Telemetry,
    decay::{SocProtect, integrate_soc_pct, sanitize_soc_pct},
};

/// Tunables exposed via `(make-battery :soc-protect-margin 10.0 …)`.
#[derive(Clone, Debug)]
pub struct BatteryConfig {
    pub capacity_wh: f32,
    pub initial_soc_pct: f32,
    pub soc_lower_pct: f32,
    pub soc_upper_pct: f32,
    pub voltage_v: f32,
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    /// Width (in % points) of the band inside the gateway's SoC
    /// window where the throttled bound tapers toward zero. With
    /// margin = 10 and `soc_upper_pct = 90`, the charge bound starts
    /// decaying at SoC=80% and reaches 0 at SoC=90%. Same on the
    /// discharge side near `soc_lower_pct`. `0.0` disables the taper;
    /// the window still closes at its limits.
    pub soc_protect_margin_pct: f32,
    pub stream_jitter_pct: f32,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            capacity_wh: 92_000.0,
            initial_soc_pct: 50.0,
            soc_lower_pct: 10.0,
            soc_upper_pct: 90.0,
            voltage_v: 800.0,
            rated_lower_w: -30_000.0,
            rated_upper_w: 30_000.0,
            soc_protect_margin_pct: 10.0,
            stream_jitter_pct: 0.0,
        }
    }
}

pub struct Battery {
    id: u64,
    name: String,
    interval: Duration,
    cfg: BatteryConfig,
    state: Mutex<BatteryState>,
}

#[derive(Debug, Clone)]
struct BatteryState {
    /// Settled active DC power in W (last-tick's clamped accumulator).
    /// The only power the battery knows about — a battery has no AC
    /// side, so there is nothing here for reactive power to be.
    power_w: f32,
    /// Per-tick accumulator for inverter pushes. `set_dc_power` adds
    /// here; `tick()` drains it and clamps the sum. This is what makes
    /// N inverters pushing the same battery sum correctly instead of
    /// having the last writer win.
    pending_p: f32,
    /// `power_w / pushed total` from the last tick — how much of what
    /// the inverters pushed the hardware limits let through. 1.0 when
    /// nothing was pushed. Read back by the inverters for their own
    /// published power (see `SimulatedComponent::dc_accept_ratio`).
    accept_ratio: f32,
    /// State of charge in % [0, 100], updated each tick from
    /// `power_w * dt` and clamped at the boundaries.
    soc_pct: f32,
}

impl Battery {
    pub fn new(id: u64, interval: Duration, cfg: BatteryConfig) -> Self {
        let init_soc = cfg.initial_soc_pct;
        Self {
            id,
            name: format!("bat-{id}"),
            interval,
            cfg,
            state: Mutex::new(BatteryState {
                power_w: 0.0,
                pending_p: 0.0,
                accept_ratio: 1.0,
                soc_pct: init_soc,
            }),
        }
    }

    /// The hardware limits at `soc`: the rated band, with charging
    /// refused at a full pack and discharging at an empty one.
    fn hardware_bounds(&self, soc: f32) -> (f32, f32) {
        let lower = if soc <= 0.0 {
            0.0
        } else {
            self.cfg.rated_lower_w
        };
        let upper = if soc >= 100.0 {
            0.0
        } else {
            self.cfg.rated_upper_w
        };
        (lower, upper)
    }
}

impl fmt::Display for Battery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Battery {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Battery
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }
    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn set_soc_pct(&self, pct: f32) -> bool {
        // The gateway re-derives the throttled bounds from the new
        // value on its next step.
        if let Some(pct) = sanitize_soc_pct("Battery::set_soc_pct", pct) {
            self.state.lock().soc_pct = pct;
        }
        true
    }

    fn takes_soc_pct(&self) -> bool {
        true
    }

    fn tick(&self, _world: &MicrogridSite, _now: DateTime<Utc>, dt: Duration) {
        let mut s = self.state.lock();

        // 1. Drain the per-tick accumulator and clamp the summed push
        //    against the hardware limits: with N inverters sharing
        //    the bus, the clamp applies to the total, not the last
        //    writer. min/max keeps a NaN config bound from panicking.
        let (lower, upper) = self.hardware_bounds(s.soc_pct);
        let total_p = s.pending_p;
        s.pending_p = 0.0;
        s.power_w = total_p.min(upper).max(lower);

        // 2. The accept ratio. Inside sane limits (lower ≤ 0 ≤ upper)
        //    the clip keeps the sign and never grows the magnitude,
        //    so the ratio is already in [0, 1]; the clamp only guards
        //    a config whose limits exclude zero.
        s.accept_ratio = if total_p != 0.0 && s.power_w.is_finite() {
            (s.power_w / total_p).clamp(0.0, 1.0)
        } else {
            1.0
        };

        // 3. SoC update from settled P — the rectangular P·dt step in
        //    decay::integrate_soc_pct. Related to the per-component
        //    `EnergyWh` history metric (see history.rs `push_snapshot`), but
        //    a different quadrature: rectangular over this one fixed physics
        //    tick's `dt` here, versus trapezoidal over the variable sample
        //    gap there. Deliberately not the shared `EnergyAccum` — the
        //    per-tick rectangle is what clamps SoC each step.
        s.soc_pct = integrate_soc_pct(s.soc_pct, s.power_w, dt, self.cfg.capacity_wh);
    }

    fn telemetry(&self, _world: &MicrogridSite) -> Telemetry {
        let s = self.state.lock().clone();
        // DC side, plain and simple: a battery only ever sees active
        // power (there is no reactive component on a DC bus), so
        // dc_power_w is just the settled `power_w` and dc_current_a
        // is Ohm's-law from that and the pack voltage.
        Telemetry {
            id: self.id,
            category: Some(Category::Battery),
            soc_pct: Some(s.soc_pct),
            soc_lower_pct: Some(self.cfg.soc_lower_pct),
            soc_upper_pct: Some(self.cfg.soc_upper_pct),
            capacity_wh: Some(self.cfg.capacity_wh),
            dc_voltage_v: Some(self.cfg.voltage_v),
            dc_power_w: Some(s.power_w),
            dc_current_a: Some(if self.cfg.voltage_v != 0.0 {
                s.power_w / self.cfg.voltage_v
            } else {
                0.0
            }),
            component_state: Some(crate::sim::component::power_state(s.power_w)),
            relay_state: Some("relay-closed"),
            ..Default::default()
        }
    }

    /// Battery's contribution to its parent meter — a battery only
    /// ever moves active power (a DC bus has no reactive axis), so
    /// this is the same `power_w` that `telemetry().dc_power_w`
    /// reports. Q is entirely the inverter's business: it terminates
    /// there and never reaches the battery.
    fn aggregate_power_w(&self, _world: &MicrogridSite) -> f32 {
        self.state.lock().power_w
    }

    fn takes_dc_power(&self) -> bool {
        true
    }

    /// Add an inverter's active push to this tick's accumulator. The
    /// actual `power_w` value is the *total* across all parents after
    /// `tick()` clamps the accumulated sum to the hardware limits.
    fn set_dc_power(&self, p: f32) {
        self.state.lock().pending_p += p;
    }

    fn dc_accept_ratio(&self) -> f32 {
        self.state.lock().accept_ratio
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn soc_window(&self) -> Option<SocProtect> {
        Some(SocProtect::new(
            self.cfg.soc_lower_pct,
            self.cfg.soc_upper_pct,
            self.cfg.soc_protect_margin_pct,
        ))
    }

    fn soc_pct(&self) -> Option<f32> {
        Some(self.state.lock().soc_pct)
    }

    fn make_fn(&self) -> &'static str {
        "%make-battery"
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        let lf = crate::lisp::lisp_float32;
        let mut kw = vec![
            (":capacity", lf(self.cfg.capacity_wh)),
            (":initial-soc", lf(self.cfg.initial_soc_pct)),
            (":soc-lower", lf(self.cfg.soc_lower_pct)),
            (":soc-upper", lf(self.cfg.soc_upper_pct)),
            (":voltage", lf(self.cfg.voltage_v)),
            (":rated-lower", lf(self.cfg.rated_lower_w)),
            (":rated-upper", lf(self.cfg.rated_upper_w)),
            (":soc-protect-margin", lf(self.cfg.soc_protect_margin_pct)),
        ];
        if self.interval != Duration::from_millis(1000) {
            kw.push((":interval", self.interval.as_millis().to_string()));
        }
        if self.cfg.stream_jitter_pct != 0.0 {
            kw.push((":stream-jitter-pct", lf(self.cfg.stream_jitter_pct)));
        }
        kw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 4 pin: a Q setpoint at the inverter must never reach the
    /// battery's DC telemetry. Before this change the battery folded
    /// any pushed Q into a signed-apparent `dc_power_w` (sqrt(P^2+Q^2)
    /// with the sign of P); after this change the inverter keeps Q on
    /// its own AC-side axis and only ever pushes active power, so
    /// `dc_power_w` is exactly the accepted P share and `dc_current_a`
    /// is derived from that alone.
    #[test]
    fn dc_power_is_pure_active_even_when_the_inverter_runs_q() {
        use crate::sim::inverter::battery_inverter::{BatteryInverter, BatteryInverterConfig};

        let w = MicrogridSite::new();
        let bat = Battery::new(
            1,
            Duration::from_secs(1),
            BatteryConfig {
                rated_lower_w: -10_000.0,
                rated_upper_w: 10_000.0,
                capacity_wh: 100_000.0,
                soc_protect_margin_pct: 0.0,
                ..Default::default()
            },
        );
        w.register(bat);
        let inv = BatteryInverter::new(
            2,
            Duration::from_secs(1),
            BatteryInverterConfig {
                rated_lower_w: -10_000.0,
                rated_upper_w: 10_000.0,
                command_delay: Duration::ZERO,
                ramp_rate_w_per_s: f32::INFINITY,
                // kVA cap instead of the default PF cap: a PF-capped
                // envelope is 0 at P=0, so the Q setpoint below would
                // be rejected before the active setpoint has a chance
                // to ramp in. This test cares about the P/Q split, not
                // the envelope shape.
                reactive: crate::sim::reactive::ReactiveCapability {
                    pf_limit: None,
                    apparent_va: Some(10_000.0),
                },
                reactive_command_delay: Duration::ZERO,
                reactive_ramp_rate_var_per_s: f32::INFINITY,
                device_delay: Duration::ZERO,
                ..Default::default()
            },
        );
        w.register(inv);
        w.connect(2, 1);

        let inv_ref = w.get(2).unwrap();
        use crate::timeout_tracker::SetpointAxis;
        let ctl = inv_ref.controllable().unwrap();
        ctl.set_command(SetpointAxis::Active, 4_000.0);
        ctl.set_command(SetpointAxis::Reactive, 1_000.0);

        let dt = Duration::from_millis(100);
        for _ in 0..2 {
            w.tick_hardware(Utc::now(), dt);
        }

        let bat_ref = w.get(1).unwrap();
        let t = bat_ref.telemetry(&w);
        let dc_power_w = t.dc_power_w.expect("battery publishes dc_power_w");
        let dc_current_a = t.dc_current_a.expect("battery publishes dc_current_a");
        let accepted_p = bat_ref.aggregate_power_w(&w);

        assert!(
            (dc_power_w - accepted_p).abs() < 1.0,
            "dc_power_w must equal the accepted P share exactly, got {dc_power_w} vs accepted {accepted_p}"
        );
        assert!(
            (dc_current_a - dc_power_w / 800.0).abs() < 0.01,
            "dc_current_a must be dc_power_w / voltage, got {dc_current_a}"
        );
    }

    /// Every construction kwarg round-trips into the rendered form,
    /// and `:interval` renders only when it departs from the 1000 ms
    /// default.
    #[test]
    fn constructor_kwargs_round_trip_battery() {
        let cfg = BatteryConfig {
            capacity_wh: 50_000.0,
            initial_soc_pct: 20.0,
            ..Default::default()
        };
        let b = Battery::new(7, Duration::from_millis(500), cfg);
        assert_eq!(b.make_fn(), "%make-battery");
        let kw = b.constructor_kwargs();
        let s = kw
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(s.contains(":capacity 50000.0"));
        assert!(s.contains(":initial-soc 20.0"));
        assert!(s.contains(":interval 500"));
        assert!(s.contains(":rated-lower -30000.0"));
    }

    /// The pack takes up to its rating anywhere between empty and
    /// full, refuses charge at 100 % and discharge at 0 %.
    #[test]
    fn the_hardware_refuses_charge_at_full_and_discharge_at_empty() {
        let w = MicrogridSite::new();
        let tick = |b: &Battery, p: f32| {
            b.set_dc_power(p);
            b.tick(&w, Utc::now(), Duration::from_millis(100));
            b.aggregate_power_w(&w)
        };
        let full = Battery::new(
            1,
            Duration::from_secs(1),
            BatteryConfig {
                initial_soc_pct: 100.0,
                ..Default::default()
            },
        );
        assert_eq!(tick(&full, 5_000.0), 0.0);
        assert_eq!(tick(&full, -5_000.0), -5_000.0);
        let empty = Battery::new(
            2,
            Duration::from_secs(1),
            BatteryConfig {
                initial_soc_pct: 0.0,
                ..Default::default()
            },
        );
        assert_eq!(tick(&empty, -5_000.0), 0.0);
        assert_eq!(tick(&empty, 5_000.0), 5_000.0);
        let near_top = Battery::new(
            3,
            Duration::from_secs(1),
            BatteryConfig {
                initial_soc_pct: 95.0,
                ..Default::default()
            },
        );
        assert_eq!(tick(&near_top, 40_000.0), 30_000.0, "rated, not the window");
    }
}
