use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, Controllable, MicrogridSite, ReactiveLimits, SimulatedComponent, Telemetry,
    bounds::VecBounds, component::GatewaySettings, device_axis::DeviceAxis,
    reactive::ReactiveCapability, runtime::Health,
};
use crate::timeout_tracker::SetpointAxis;

#[derive(Clone, Debug)]
pub struct BatteryInverterConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    /// The gateway's delay before it acts on an active command.
    pub command_delay: Duration,
    /// W/s; use `f32::INFINITY` to disable ramping.
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
    /// Q envelope. Default microsim-compatible PF cap of 0.35.
    pub reactive: ReactiveCapability,
    /// The gateway's delay before it acts on a reactive command;
    /// default 100 ms.
    pub reactive_command_delay: Duration,
    /// Reactive slew rate (VAR/s). Sized to give an open-loop
    /// response time around 5 s when traversing a ~10 kVAR window —
    /// IEEE 1547-2018's Performance Category B default OLRT for
    /// Volt/VAR control. Use `f32::INFINITY` to disable.
    pub reactive_ramp_rate_var_per_s: f32,
    /// Time a command takes to reach the output once the inverter has
    /// it, on both axes; default 100 ms.
    pub device_delay: Duration,
}

impl Default for BatteryInverterConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: -30_000.0,
            rated_upper_w: 30_000.0,
            command_delay: Duration::ZERO,
            ramp_rate_w_per_s: f32::INFINITY,
            stream_jitter_pct: 0.0,
            reactive: ReactiveCapability::microsim_default(),
            reactive_command_delay: Duration::from_millis(100),
            reactive_ramp_rate_var_per_s: 2000.0,
            device_delay: super::DEFAULT_DEVICE_DELAY,
        }
    }
}

pub struct BatteryInverter {
    id: u64,
    name: String,
    interval: Duration,
    cfg: BatteryInverterConfig,
    /// Active output: the command handed in through `set_command`,
    /// delayed and clamped to the rated band. It is what the inverter
    /// pushes onto the DC bus.
    active: DeviceAxis,
    /// Reactive output, clamped to the capability at the live P.
    reactive: DeviceAxis,
    /// The live PF / kVA capability; `set-reactive-pf-limit` and
    /// `set-reactive-apparent-va` change it at runtime.
    caps: Mutex<ReactiveCapability>,
    /// The AC-side value telemetry publishes: the push each healthy
    /// child accepted, by its accept ratio; 0 when the inverter is
    /// tripped or no healthy child took the push.
    measured_w: Mutex<f32>,
    /// The reactive value telemetry and parent meters read: the
    /// reactive output; 0 when the inverter is tripped or no healthy
    /// child took the push.
    measured_var: Mutex<f32>,
}

impl BatteryInverter {
    pub fn new(id: u64, interval: Duration, cfg: BatteryInverterConfig) -> Self {
        Self {
            id,
            name: format!("inv-bat-{id}"),
            interval,
            active: DeviceAxis::new(cfg.device_delay, 0.0),
            reactive: DeviceAxis::new(cfg.device_delay, 0.0),
            caps: Mutex::new(cfg.reactive),
            cfg,
            measured_w: Mutex::new(0.0),
            measured_var: Mutex::new(0.0),
        }
    }

    fn rated(&self) -> VecBounds {
        VecBounds::single(self.cfg.rated_lower_w, self.cfg.rated_upper_w)
    }

    /// The Q band the capability allows at active power `p`.
    fn q_band_at(&self, p: f32) -> VecBounds {
        self.caps.lock().q_band_at(p)
    }
}

impl fmt::Display for BatteryInverter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for BatteryInverter {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Inverter
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }

    fn tick(&self, site: &MicrogridSite, now: DateTime<Utc>, _dt: Duration) {
        // Own-health gate: a faulted or standby inverter is
        // electrically offline — its IGBTs stop switching, so both
        // outputs snap to 0 and nothing still in a delay line comes
        // out later. (The gateway clears the command, so recovery
        // waits for a new one.)
        if site.runtime_of(self.id).health != Health::Ok {
            self.active.trip();
            self.reactive.trip();
            *self.measured_w.lock() = 0.0;
            *self.measured_var.lock() = 0.0;
            return;
        }

        let commanded_p = self.active.tick(now, Some(&self.rated()));
        // Q is clamped at the PREVIOUS tick's published P: the AC
        // power the children actually accepted, not what was just
        // commanded.
        let p_live = *self.measured_w.lock();
        let commanded_q = self.reactive.tick(now, Some(&self.q_band_at(p_live)));

        // Distribute equal shares among the healthy DC sinks. Failed
        // batteries are skipped, so the survivors absorb the full
        // push; a child that takes no DC push (a meter mis-wired
        // under the inverter) neither inflates the divisor nor counts
        // as having accepted a share. Each child accumulates pushes
        // over the tick, so N inverters on one bus settle to the
        // clamped sum.
        let healthy = site.healthy_dc_children(self.id);
        if healthy.is_empty() {
            // No child accepted the push → no AC output. The device
            // output stays at its command, so delivery resumes the
            // moment a child comes back.
            *self.measured_w.lock() = 0.0;
            *self.measured_var.lock() = 0.0;
        } else {
            let p_share = commanded_p / healthy.len() as f32;
            // Publish what the children accepted of our push, by each
            // child's clip ratio from its previous tick (children
            // tick first): the published value lags one tick. Q never
            // reaches a battery; it ends here, on the AC side.
            let mut accepted_p = 0.0;
            for store in healthy.iter().filter_map(|c| c.dc_storage()) {
                store.set_dc_power(p_share);
                accepted_p += p_share * store.dc_accept_ratio();
            }
            *self.measured_w.lock() = accepted_p;
            *self.measured_var.lock() = commanded_q;
        }
    }

    fn telemetry(&self, site: &MicrogridSite) -> Telemetry {
        // The measured AC output, not the commanded value: the two
        // differ when a battery clips downstream.
        super::inverter_telemetry(
            self.id,
            site,
            *self.measured_w.lock(),
            *self.measured_var.lock(),
        )
    }

    fn active_power_w(&self, _site: &MicrogridSite) -> Option<f32> {
        Some(*self.measured_w.lock())
    }

    fn controllable(&self) -> Option<&dyn Controllable> {
        Some(self)
    }

    fn reactive_limits(&self) -> Option<&dyn ReactiveLimits> {
        Some(self)
    }

    fn aggregate_power_w(&self, _world: &MicrogridSite) -> f32 {
        *self.measured_w.lock()
    }

    fn aggregate_reactive_var(&self, _world: &MicrogridSite) -> f32 {
        *self.measured_var.lock()
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn subtype(&self) -> Option<&'static str> {
        Some("battery")
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.cfg.stream_jitter_pct
    }

    fn make_fn(&self) -> &'static str {
        "%make-battery-inverter"
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        super::common_inverter_kwargs(super::CommonInverterCfg {
            rated_lower_w: self.cfg.rated_lower_w,
            rated_upper_w: self.cfg.rated_upper_w,
            command_delay: self.cfg.command_delay,
            ramp_rate_w_per_s: self.cfg.ramp_rate_w_per_s,
            interval: self.interval,
            stream_jitter_pct: self.cfg.stream_jitter_pct,
            reactive: self.cfg.reactive,
            reactive_command_delay: self.cfg.reactive_command_delay,
            reactive_ramp_rate_var_per_s: self.cfg.reactive_ramp_rate_var_per_s,
            device_delay: self.cfg.device_delay,
        })
    }
}

impl ReactiveLimits for BatteryInverter {
    fn reactive_capability(&self) -> ReactiveCapability {
        *self.caps.lock()
    }

    fn set_reactive_pf_limit(&self, pf: Option<f32>) {
        self.caps.lock().pf_limit = pf;
    }

    fn set_reactive_apparent_va(&self, va: Option<f32>) {
        self.caps.lock().apparent_va = va;
    }
}

impl Controllable for BatteryInverter {
    fn has_axis(&self, _axis: SetpointAxis) -> bool {
        true
    }

    fn set_command(&self, axis: SetpointAxis, value: f32) {
        match axis {
            SetpointAxis::Active => self.active.set_command(value),
            SetpointAxis::Reactive => self.reactive.set_command(value),
        }
    }

    fn physical_band(&self, axis: SetpointAxis, _dt: Duration) -> Option<VecBounds> {
        match axis {
            SetpointAxis::Active => None,
            SetpointAxis::Reactive => {
                let p = *self.measured_w.lock();
                Some(self.q_band_at(p))
            }
        }
    }

    fn gateway_settings(&self) -> GatewaySettings {
        GatewaySettings {
            command_delay: self.cfg.command_delay,
            ramp_rate_w_per_s: self.cfg.ramp_rate_w_per_s,
            reactive_command_delay: self.cfg.reactive_command_delay,
            reactive_ramp_rate_var_per_s: self.cfg.reactive_ramp_rate_var_per_s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::{Battery, battery::BatteryConfig};

    const DT: Duration = Duration::from_millis(100);

    /// ±10 kW, no delays, no ramps, a pure 10 kVA Q cap.
    fn instant_cfg() -> BatteryInverterConfig {
        BatteryInverterConfig {
            rated_lower_w: -10_000.0,
            rated_upper_w: 10_000.0,
            command_delay: Duration::ZERO,
            ramp_rate_w_per_s: f32::INFINITY,
            reactive: ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(10_000.0),
            },
            reactive_command_delay: Duration::ZERO,
            reactive_ramp_rate_var_per_s: f32::INFINITY,
            device_delay: Duration::ZERO,
            ..Default::default()
        }
    }

    fn battery(id: u64, w: f32, soc: f32) -> Battery {
        Battery::new(
            id,
            Duration::from_secs(1),
            BatteryConfig {
                rated_lower_w: -w,
                rated_upper_w: w,
                capacity_wh: 100_000.0,
                initial_soc_pct: soc,
                soc_protect_margin_pct: 0.0,
                ..Default::default()
            },
        )
    }

    /// Battery 100 (±10 kW) under inverter 200 (`instant_cfg`).
    fn setup_inverter_with_battery() -> (MicrogridSite, u64, u64) {
        let w = MicrogridSite::new();
        w.register(battery(100, 10_000.0, 50.0));
        w.register(BatteryInverter::new(
            200,
            Duration::from_secs(1),
            instant_cfg(),
        ));
        w.connect(200, 100);
        (w, 100, 200)
    }

    /// `n` inverters (200, 201, …) sharing battery 100 (±`bat_w`).
    fn setup_shared_battery(n: usize, bat_w: f32, soc: f32) -> (MicrogridSite, u64, Vec<u64>) {
        let w = MicrogridSite::new();
        w.register(battery(100, bat_w, soc));
        let mut invs = Vec::new();
        for i in 0..n {
            let id = 200 + i as u64;
            w.register(BatteryInverter::new(
                id,
                Duration::from_secs(1),
                instant_cfg(),
            ));
            w.connect(id, 100);
            invs.push(id);
        }
        (w, 100, invs)
    }

    /// Hardware only: every component ticks in registration order and
    /// no gateway step runs, so a command written with `set_command`
    /// stands.
    fn hardware_ticks(w: &MicrogridSite, rounds: usize) {
        let mut now = Utc::now();
        for _ in 0..rounds {
            now += chrono::Duration::milliseconds(100);
            w.tick_hardware(now, DT);
        }
    }

    /// The inverter publishes what the battery accepted, not what it
    /// was commanded: a 5 kW push into a battery that clips at 3 kW
    /// reads 3 kW on the inverter too.
    #[test]
    fn reported_power_follows_the_battery_clip() {
        let (w, bat, invs) = setup_shared_battery(1, 3_000.0, 50.0);
        let inv = w.get(invs[0]).unwrap();
        inv.controllable()
            .unwrap()
            .set_command(SetpointAxis::Active, 5_000.0);
        hardware_ticks(&w, 2);
        let accepted = w.get(bat).unwrap().aggregate_power_w(&w);
        assert!(
            (accepted - 3_000.0).abs() < 1.0,
            "battery clip, got {accepted}"
        );
        let reported = inv.aggregate_power_w(&w);
        assert!(
            (reported - 3_000.0).abs() < 1.0,
            "expected 3 kW reported, got {reported}"
        );
        assert_eq!(inv.telemetry(&w).active_power_w, Some(reported));
    }

    /// Two inverters on one battery: the one that commands nothing
    /// reports nothing; the other carries the whole clip.
    #[test]
    fn idle_sibling_reports_zero_and_the_other_the_clip() {
        let (w, _bat, invs) = setup_shared_battery(2, 3_000.0, 50.0);
        w.get(invs[0])
            .unwrap()
            .controllable()
            .unwrap()
            .set_command(SetpointAxis::Active, 4_000.0);
        hardware_ticks(&w, 2);
        let a = w.get(invs[0]).unwrap().aggregate_power_w(&w);
        let b = w.get(invs[1]).unwrap().aggregate_power_w(&w);
        assert!((a - 3_000.0).abs() < 1.0, "commanding inverter, got {a}");
        assert!(b.abs() < 1.0, "idle inverter, got {b}");
    }

    /// Pushes of 4 kW and 2 kW into a 3 kW battery share the clip in
    /// proportion: 2 kW and 1 kW.
    #[test]
    fn shared_clip_is_split_in_proportion_to_the_push() {
        let (w, bat, invs) = setup_shared_battery(2, 3_000.0, 50.0);
        w.get(invs[0])
            .unwrap()
            .controllable()
            .unwrap()
            .set_command(SetpointAxis::Active, 4_000.0);
        w.get(invs[1])
            .unwrap()
            .controllable()
            .unwrap()
            .set_command(SetpointAxis::Active, 2_000.0);
        hardware_ticks(&w, 2);
        let a = w.get(invs[0]).unwrap().aggregate_power_w(&w);
        let b = w.get(invs[1]).unwrap().aggregate_power_w(&w);
        assert!((a - 2_000.0).abs() < 1.0, "4 kW pusher, got {a}");
        assert!((b - 1_000.0).abs() < 1.0, "2 kW pusher, got {b}");
        let total = w.get(bat).unwrap().aggregate_power_w(&w);
        assert!(
            (a + b - total).abs() < 1.0,
            "shares must sum to the battery's {total}"
        );
    }

    /// A setpoint refused because a live augmentation narrowed the
    /// envelope names the augmented bounds, not the rated ones.
    #[test]
    fn out_of_bounds_error_reports_augmented_envelope() {
        let (w, _bat, inv) = setup_inverter_with_battery();
        let gw = w.gateway();
        gw.augment(
            inv,
            w.run_generation(),
            SetpointAxis::Active,
            VecBounds::single(-5_000.0, 5_000.0),
            Duration::from_secs(60),
        )
        .unwrap();
        let err = gw
            .command(inv, SetpointAxis::Active, 8_000.0)
            .expect_err("8 kW exceeds the augmented envelope");
        assert!(err.to_string().contains("[-5000, 5000]"), "{err}");
    }

    /// An augmentation arriving after a setpoint armed pulls the
    /// running output in, and the command resumes when it lapses.
    #[test]
    fn late_augmentation_re_clamps_an_armed_setpoint() {
        let (w, _bat, id) = setup_inverter_with_battery();
        let inv = w.get(id).unwrap();
        let t0 = w.now();
        w.gateway()
            .command(id, SetpointAxis::Active, 8_000.0)
            .unwrap();
        w.tick_once(t0, DT);
        assert!((inv.aggregate_power_w(&w) - 8_000.0).abs() < 1.0);
        w.gateway()
            .augment(
                id,
                w.run_generation(),
                SetpointAxis::Active,
                VecBounds::single(-5_000.0, 5_000.0),
                Duration::from_millis(500),
            )
            .unwrap();
        w.tick_once(t0 + chrono::Duration::milliseconds(100), DT);
        assert!(
            (inv.aggregate_power_w(&w) - 5_000.0).abs() < 1.0,
            "expected the armed 8 kW clamped to 5 kW, got {}",
            inv.aggregate_power_w(&w),
        );
        w.tick_once(t0 + chrono::Duration::seconds(5), DT);
        assert!((inv.aggregate_power_w(&w) - 8_000.0).abs() < 1.0);
    }

    /// Resetting one axis leaves the other axis's command running.
    #[test]
    fn axis_reset_leaves_the_other_axis_running() {
        let (w, _bat, id) = setup_inverter_with_battery();
        let inv = w.get(id).unwrap();
        let gw = w.gateway();
        gw.command(id, SetpointAxis::Active, 4_000.0).unwrap();
        gw.command(id, SetpointAxis::Reactive, 1_000.0).unwrap();
        w.tick_n(2, DT);
        assert!((inv.aggregate_power_w(&w) - 4_000.0).abs() < 1.0);
        assert!((inv.aggregate_reactive_var(&w) - 1_000.0).abs() < 1.0);

        gw.reset(id, SetpointAxis::Reactive);
        w.tick_n(1, DT);
        assert!(
            (inv.aggregate_power_w(&w) - 4_000.0).abs() < 1.0,
            "P survives"
        );
        assert!(inv.aggregate_reactive_var(&w).abs() < 1.0, "Q parked");

        gw.command(id, SetpointAxis::Reactive, 800.0).unwrap();
        w.tick_n(1, DT);
        gw.reset(id, SetpointAxis::Active);
        w.tick_n(1, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0, "P parked");
        assert!(
            (inv.aggregate_reactive_var(&w) - 800.0).abs() < 1.0,
            "Q survives"
        );
    }

    /// With every battery unhealthy the inverter publishes 0 on both
    /// axes, and resumes at once when one comes back.
    #[test]
    fn no_healthy_children_means_zero_published() {
        let (w, bat_id, id) = setup_inverter_with_battery();
        let inv = w.get(id).unwrap();
        w.gateway()
            .command(id, SetpointAxis::Active, 3_000.0)
            .unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - 3_000.0).abs() < 1.0);
        w.set_health(bat_id, Health::Error).unwrap();
        w.tick_n(1, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0);
        assert!(inv.aggregate_reactive_var(&w).abs() < 1.0);
        w.set_health(bat_id, Health::Ok).unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - 3_000.0).abs() < 1.0);
    }

    /// A child that takes no DC push (a mis-wired meter) neither
    /// dilutes the share nor counts as having accepted one.
    #[test]
    fn non_dc_children_do_not_dilute_the_share() {
        use crate::sim::Meter;
        let (w, bat_id, id) = setup_inverter_with_battery();
        w.register(Meter::new(
            300,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));
        w.connect(id, 300);
        let inv = w.get(id).unwrap();
        w.gateway()
            .command(id, SetpointAxis::Active, 3_000.0)
            .unwrap();
        w.gateway()
            .command(id, SetpointAxis::Reactive, 500.0)
            .unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - 3_000.0).abs() < 1.0);
        assert!((inv.aggregate_reactive_var(&w) - 500.0).abs() < 1.0);
        w.set_health(bat_id, Health::Error).unwrap();
        w.tick_n(1, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0);
        assert!(inv.aggregate_reactive_var(&w).abs() < 1.0);
    }

    /// A faulted inverter trips offline, publishes 0 on both axes,
    /// loses its command and its lifetime, and stays at 0 after
    /// recovery until re-dispatched.
    #[test]
    fn errored_inverter_trips_offline_and_awaits_redispatch() {
        let (w, _bat, id) = setup_inverter_with_battery();
        let inv = w.get(id).unwrap();
        w.gateway()
            .command(id, SetpointAxis::Active, 3_000.0)
            .unwrap();
        w.gateway()
            .command(id, SetpointAxis::Reactive, 1_000.0)
            .unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - 3_000.0).abs() < 1.0);
        assert!((inv.aggregate_reactive_var(&w) - 1_000.0).abs() < 1.0);
        w.set_health(id, Health::Error).unwrap();
        w.tick_n(1, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0);
        assert!(inv.aggregate_reactive_var(&w).abs() < 1.0, "Q tripped");
        let q = inv.telemetry(&w).reactive_power_var.unwrap();
        assert!(q.abs() < 1.0, "reported Q tripped, got {q}");
        assert_eq!(
            w.gateway().remaining_lifetime(id, SetpointAxis::Active),
            None
        );
        w.set_health(id, Health::Ok).unwrap();
        w.tick_n(1, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0, "awaits re-dispatch");
        w.gateway()
            .command(id, SetpointAxis::Active, 2_000.0)
            .unwrap();
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - 2_000.0).abs() < 1.0);
    }

    /// A command still in the device delay line when the inverter
    /// trips does not replay on recovery.
    #[test]
    fn a_trip_does_not_replay_a_pre_trip_command() {
        let w = MicrogridSite::new();
        w.register(battery(100, 10_000.0, 50.0));
        w.register(BatteryInverter::new(
            200,
            Duration::from_secs(1),
            BatteryInverterConfig {
                device_delay: Duration::from_millis(100),
                ..instant_cfg()
            },
        ));
        w.connect(200, 100);
        let inv = w.get(200).unwrap();
        w.gateway()
            .command(200, SetpointAxis::Active, 3_000.0)
            .unwrap();
        w.tick_n(1, DT);
        assert!(
            inv.aggregate_power_w(&w).abs() < 1.0,
            "still in the delay line"
        );
        w.set_health(200, Health::Error).unwrap();
        w.tick_n(1, DT);
        w.set_health(200, Health::Ok).unwrap();
        w.tick_n(3, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0, "nothing replays");
    }

    /// Gateway delay and device delay each apply once: 200 ms of
    /// gateway delay and 100 ms of device delay at a 100 ms tick put
    /// the command on the output at the fourth tick, not earlier and
    /// not later.
    #[test]
    fn a_command_reaches_the_output_after_the_gateway_and_device_delays() {
        let w = MicrogridSite::new();
        w.register(battery(100, 10_000.0, 50.0));
        w.register(BatteryInverter::new(
            200,
            Duration::from_secs(1),
            BatteryInverterConfig {
                command_delay: Duration::from_millis(200),
                device_delay: Duration::from_millis(100),
                ..instant_cfg()
            },
        ));
        w.connect(200, 100);
        let inv = w.get(200).unwrap();
        w.gateway()
            .command(200, SetpointAxis::Active, 3_000.0)
            .unwrap();
        w.tick_n(3, DT);
        assert!(inv.aggregate_power_w(&w).abs() < 1.0, "too early");
        w.tick_n(1, DT);
        assert!((inv.aggregate_power_w(&w) - 3_000.0).abs() < 1.0, "on time");
    }

    /// A disabled PF cap pins as literal `0`; infinite ramps are
    /// omitted.
    #[test]
    fn constructor_kwargs_pin_reactive_disabled_as_zero() {
        let mut cfg = BatteryInverterConfig::default();
        cfg.reactive.pf_limit = None;
        let inv = BatteryInverter::new(3, Duration::from_secs(1), cfg);
        assert_eq!(inv.make_fn(), "%make-battery-inverter");
        let s = inv
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            s.contains(":reactive-pf-limit 0"),
            "None must pin as 0, got {s}"
        );
        assert!(
            !s.contains(":ramp-rate-w-per-s"),
            "infinite ramp is omitted"
        );
        assert!(s.contains(":command-delay-s 0.0"));
    }

    /// f32 config renders without widened-f64 noise.
    #[test]
    fn constructor_kwargs_render_f32_without_widening_noise() {
        let mut cfg = BatteryInverterConfig::default();
        cfg.reactive.pf_limit = Some(0.35);
        let inv = BatteryInverter::new(3, Duration::from_secs(1), cfg);
        let s = inv
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(s.contains(":reactive-pf-limit 0.35"), "{s}");
        assert!(!s.contains("0.3499"), "no widened-f64 tail: {s}");
    }

    /// P at the apparent-power rim alone gives a present (0, 0) band.
    #[test]
    fn apparent_power_rim_alone_is_a_present_zero_band_not_an_empty_one() {
        let inv = BatteryInverter::new(1, Duration::from_secs(1), instant_cfg());
        let env = inv.q_band_at(10_000.0);
        assert_eq!(env.0.len(), 1, "caps alone never produce an empty band");
        assert_eq!((env.0[0].lower, env.0[0].upper), (Some(0.0), Some(0.0)));
    }

    /// A Q augmentation that fit the caps band at idle P is disjoint
    /// from it once P reaches the kVA rim: the envelope is genuinely
    /// empty, and every reader sees a present (0, 0) band.
    #[test]
    fn zero_headroom_from_a_disjoint_q_augmentation_publishes_a_present_zero_band() {
        let w = MicrogridSite::new();
        w.register(battery(100, 5_000.0, 50.0));
        w.register(BatteryInverter::new(
            200,
            Duration::from_secs(1),
            BatteryInverterConfig {
                rated_lower_w: -5_000.0,
                rated_upper_w: 5_000.0,
                reactive: ReactiveCapability {
                    pf_limit: None,
                    apparent_va: Some(5_000.0),
                },
                ..instant_cfg()
            },
        ));
        w.connect(200, 100);
        let gw = w.gateway();
        gw.augment(
            200,
            w.run_generation(),
            SetpointAxis::Reactive,
            VecBounds::single(-4_000.0, -3_000.0),
            Duration::from_secs(60),
        )
        .unwrap();
        gw.command(200, SetpointAxis::Active, 5_000.0).unwrap();
        w.tick_n(3, DT);
        assert!((w.get(200).unwrap().aggregate_power_w(&w) - 5_000.0).abs() < 1.0);
        let band = w.bounds_of(200, SetpointAxis::Reactive).unwrap();
        assert_eq!(
            (band.0.len(), band.0[0].lower, band.0[0].upper),
            (1, Some(0.0), Some(0.0))
        );
        let t = w.telemetry_of(w.get(200).unwrap().as_ref());
        assert_eq!(t.reactive_power_bounds.unwrap().to_string(), "[0, 0]");
        assert!(gw.command(200, SetpointAxis::Reactive, 400.0).is_err());
    }
}
