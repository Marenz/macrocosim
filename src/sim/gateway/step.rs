//! `Gateway::step`: one physics tick of the API rules. Expire request
//! lifetimes and augmentations at `now`, refresh each battery's SoC
//! window from its SoC, then for every gateway-owned axis: trip it
//! when its component is unhealthy, else target it, share out the
//! batteries' room between the inverters pushing into them, clamp and
//! ramp each axis, and hand the result to its component.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};

use super::{Gateway, GatewayState, log_expired, window};
use crate::sim::{
    SimulatedComponent, bounds::VecBounds, gateway_axis::AdvanceCtx, runtime::Health,
};
use crate::timeout_tracker::SetpointAxis;

/// One healthy owned axis between its target (step 1) and the command
/// handed to its component.
struct Planned {
    id: u64,
    axis: SetpointAxis,
    component: Arc<dyn SimulatedComponent>,
    target: Option<f32>,
    /// The validation base and the physical band, both read before
    /// any healthy axis is commanded, so a reactive axis sees the P
    /// of the last tick whatever the order of the axes. (A tripped
    /// axis is handed its 0 during planning; that touches only the
    /// device's pending command, which no later read depends on.)
    base: VecBounds,
    physical: Option<VecBounds>,
}

impl Gateway<'_> {
    /// One physics tick of the API rules, run by
    /// `MicrogridSite::tick_once` before the components tick. Holds
    /// the gateway lock from start to end.
    pub fn step(&self, now: DateTime<Utc>, dt: Duration) {
        let mut st = self.gw.state.lock();
        let expired = self.expire_locked(&mut st, now);
        for ax in st.axes.values_mut() {
            ax.drop_expired(now);
        }
        for (id, w) in st.batteries.iter_mut() {
            if let Some(soc) = self.site.get(*id).and_then(|c| c.soc_pct()) {
                w.refresh(soc);
            }
        }
        let planned = self.plan_locked(&mut st, now, dt);
        let pushes = self.window_pushes(&st, &planned, now);
        let shares = window::shares(&pushes, |battery| {
            self.bounds_of_locked(&st, battery, SetpointAxis::Active)
                .and_then(|b| b.outer_edges())
        });
        for p in &planned {
            let share = match p.axis {
                SetpointAxis::Active => shares.get(&p.id),
                SetpointAxis::Reactive => None,
            };
            let ctx = AdvanceCtx {
                base: &p.base,
                physical: p.physical.as_ref(),
                share,
            };
            let ax = st
                .axes
                .get_mut(&(p.id, p.axis))
                .expect("a planned axis is in the map");
            let out = ax.advance(p.target, now, dt, &ctx);
            p.component.set_command(p.axis, out);
        }
        drop(st);
        log_expired(&expired);
    }

    /// Trip every owned axis whose component is unhealthy — the ramp
    /// snaps to 0, the command and its lifetime go unless the
    /// component keeps them, and 0 is handed — and target every
    /// other, reading its validation base and physical band.
    fn plan_locked(&self, st: &mut GatewayState, now: DateTime<Utc>, dt: Duration) -> Vec<Planned> {
        let GatewayState {
            axes, lifetimes, ..
        } = st;
        let mut planned = Vec::new();
        for (&(id, axis), ax) in axes.iter_mut() {
            let Some(c) = self.site.get(id) else {
                continue;
            };
            if self.site.runtime_of(id).health != Health::Ok {
                let keep = c.keeps_command_through_fault(axis);
                ax.trip(keep);
                if !keep {
                    lifetimes.remove(id, axis);
                }
                c.set_command(axis, 0.0);
                continue;
            }
            let target = ax.target(now, c.idle_value(axis));
            let base = self.base_of(c.as_ref(), axis);
            let physical = c.physical_band(axis, dt);
            planned.push(Planned {
                id,
                axis,
                component: c,
                target,
                base,
                physical,
            });
        }
        planned
    }

    /// Each planned active axis's pushes, split equally across its
    /// component's healthy DC children. On each side the push is the
    /// farther from 0 of the clamped target and last tick's ramp
    /// output, so an output still ramping down toward a lower target
    /// stays inside the room. The clamped target is the step-1 target
    /// clamped to the axis's validation envelope ∩ physical band,
    /// what the ramp aims for before the share, or the ramp target
    /// while the axis holds. A side where both are 0 or on the other
    /// sign pushes nothing; an axis with no healthy DC child pushes
    /// nothing. An axis the share then parks at 0 still pushes its
    /// clamped target, so its neighbours' shares can leave part of
    /// the room unused, never more than the room.
    fn window_pushes(
        &self,
        st: &GatewayState,
        planned: &[Planned],
        now: DateTime<Utc>,
    ) -> Vec<window::Push> {
        let mut pushes = Vec::new();
        for p in planned.iter().filter(|p| p.axis == SetpointAxis::Active) {
            let batteries: Vec<u64> = self
                .site
                .healthy_dc_children(p.id)
                .iter()
                .map(|c| c.id())
                .collect();
            if batteries.is_empty() {
                continue;
            }
            let ax = &st.axes[&(p.id, p.axis)];
            let target = ax.clamped_target(p.target, &p.base, p.physical.as_ref(), now);
            let actual = ax.actual();
            let n = batteries.len() as f32;
            let sides = [target.max(actual).max(0.0), target.min(actual).min(0.0)];
            for side in sides.into_iter().filter(|w| *w != 0.0) {
                pushes.extend(batteries.iter().map(|&battery| window::Push {
                    inverter: p.id,
                    battery,
                    watts: side / n,
                }));
            }
        }
        pushes
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::super::{
        Mode,
        test_stubs::{Hw, instant_inverter, nearly_full_pack, put, ramping_inverter},
    };
    use chrono::{DateTime, Utc};
    use parking_lot::Mutex;

    use crate::sim::{
        Battery, BatteryInverter, Category, MicrogridSite, SimulatedComponent, Telemetry,
        battery::BatteryConfig,
        bounds::VecBounds,
        component::GatewaySettings,
        decay::{SocProtect, soc_protected_bounds},
        inverter::battery_inverter::BatteryInverterConfig,
        reactive::ReactiveCapability,
        runtime::Health,
    };
    use crate::timeout_tracker::SetpointAxis::{self, Active, Reactive};

    const DT: Duration = Duration::from_millis(100);

    fn ramped(rate: f32) -> GatewaySettings {
        GatewaySettings {
            ramp_rate_w_per_s: rate,
            ..GatewaySettings::default()
        }
    }

    #[test]
    fn step_hands_the_ramp_output_to_the_component() {
        let site = MicrogridSite::new();
        let hw = put(&site, Arc::new(Hw::new(1)));
        site.gateway().command(1, Active, 600.0).unwrap();
        site.tick_n(1, DT);
        assert_eq!(hw.last(), Some(600.0));
    }

    /// An expired lifetime ramps the axis to its idle value.
    #[test]
    fn expiry_ramps_to_the_idle_value() {
        let site = MicrogridSite::new();
        let hw = put(
            &site,
            Arc::new(Hw {
                idle: Some(-50.0),
                ..Hw::new(1)
            }),
        );
        site.gateway()
            .set_power(
                1,
                site.run_generation(),
                Active,
                600.0,
                Duration::from_secs(1),
                Mode::Reject,
            )
            .unwrap();
        site.tick_n(1, DT);
        assert_eq!(hw.last(), Some(600.0));
        site.tick_n(10, DT);
        assert_eq!(hw.last(), Some(-50.0));
        assert_eq!(site.gateway().remaining_lifetime(1, Active), None);
    }

    /// A trip hands 0 and clears the command and its lifetime; the
    /// recovered axis waits for a new command.
    #[test]
    fn a_trip_clears_the_command_and_its_lifetime() {
        let site = MicrogridSite::new();
        let hw = put(&site, Arc::new(Hw::new(1)));
        site.gateway().command(1, Active, 600.0).unwrap();
        site.tick_n(1, DT);
        site.set_health(1, Health::Error).unwrap();
        site.tick_n(1, DT);
        assert_eq!(hw.last(), Some(0.0));
        assert_eq!(site.gateway().remaining_lifetime(1, Active), None);
        site.set_health(1, Health::Ok).unwrap();
        site.tick_n(2, DT);
        assert_eq!(hw.last(), Some(0.0), "no command survives the trip");
    }

    /// A component that keeps its command through a fault keeps the
    /// lifetime too, and the axis climbs back at the ramp rate.
    #[test]
    fn a_kept_command_survives_a_trip_and_is_ramped_back_to() {
        let site = MicrogridSite::new();
        let hw = put(
            &site,
            Arc::new(Hw {
                keeps: true,
                settings: ramped(2_000.0),
                ..Hw::new(1)
            }),
        );
        site.gateway().command(1, Active, 600.0).unwrap();
        site.tick_n(5, DT);
        assert_eq!(hw.last(), Some(600.0));
        site.set_health(1, Health::Error).unwrap();
        site.tick_n(1, DT);
        assert_eq!(hw.last(), Some(0.0));
        assert!(site.gateway().remaining_lifetime(1, Active).is_some());
        site.set_health(1, Health::Ok).unwrap();
        site.tick_n(1, DT);
        assert_eq!(hw.last(), Some(200.0), "climbs back at 2 kW/s");
    }

    /// The ramp starts at the component's initial value and tracks
    /// its idle value with no command standing.
    #[test]
    fn the_ramp_starts_at_the_initial_value_and_tracks_idle() {
        let site = MicrogridSite::new();
        let hw = put(
            &site,
            Arc::new(Hw {
                initial: -300.0,
                idle: Some(-300.0),
                settings: ramped(100.0),
                ..Hw::new(1)
            }),
        );
        site.tick_n(1, DT);
        assert_eq!(hw.last(), Some(-300.0), "no slew up from zero");
    }

    /// The physical band snaps the handed command down.
    #[test]
    fn the_physical_band_snaps_the_command_down() {
        let site = MicrogridSite::new();
        let hw = put(&site, Arc::new(Hw::new(1)));
        *hw.physical.lock() = Some(VecBounds::single(0.0, 200.0));
        site.gateway().command(1, Active, 600.0).unwrap();
        site.tick_n(1, DT);
        assert_eq!(hw.last(), Some(200.0));
    }

    /// The step reaps an owned axis's lapsed augmentations, so none
    /// is left stored to weigh on later checks and reads.
    #[test]
    fn step_reaps_lapsed_augmentations() {
        let site = MicrogridSite::new();
        put(&site, Arc::new(Hw::new(1)));
        let t0 = site.now();
        site.gateway()
            .augment(
                1,
                site.run_generation(),
                Active,
                VecBounds::single(0.0, 300.0),
                Duration::from_secs(1),
            )
            .unwrap();
        site.tick_n(20, DT);
        let gw = site.gateway();
        let st = gw.gw.state.lock();
        let rated = VecBounds::single(-1000.0, 1000.0);
        assert_eq!(
            st.axes[&(1, Active)]
                .validation_envelope(&rated, t0)
                .to_string(),
            "[-1000, 1000]",
            "the lapsed augmentation is gone even at its own time"
        );
    }

    /// A two-axis component whose measured P follows its last active
    /// command at once, with a pure 1 kVA reactive cap that is also
    /// its reactive physical band at that P.
    struct Pq {
        p: Mutex<f32>,
        q: Mutex<Option<f32>>,
    }

    impl std::fmt::Display for Pq {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "pq")
        }
    }

    impl SimulatedComponent for Pq {
        fn id(&self) -> u64 {
            1
        }
        fn category(&self) -> Category {
            Category::Inverter
        }
        fn name(&self) -> &str {
            "pq"
        }
        fn stream_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn tick(&self, _: &MicrogridSite, _: DateTime<Utc>, _: Duration) {}
        fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
            Telemetry::default()
        }
        fn active_power_w(&self, _: &MicrogridSite) -> Option<f32> {
            Some(*self.p.lock())
        }
        fn rated_active_bounds(&self) -> Option<(f32, f32)> {
            Some((-1000.0, 1000.0))
        }
        fn reactive_capability(&self) -> Option<ReactiveCapability> {
            Some(ReactiveCapability {
                pf_limit: None,
                apparent_va: Some(1000.0),
            })
        }
        fn has_axis(&self, _: SetpointAxis) -> bool {
            true
        }
        fn set_command(&self, axis: SetpointAxis, value: f32) {
            match axis {
                Active => *self.p.lock() = value,
                Reactive => *self.q.lock() = Some(value),
            }
        }
        fn physical_band(&self, axis: SetpointAxis, _: Duration) -> Option<VecBounds> {
            let cap = self.reactive_capability()?;
            (axis == Reactive).then(|| cap.q_band_at(*self.p.lock()))
        }
        fn make_fn(&self) -> &'static str {
            "%make-test-pq"
        }
        fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
            Vec::new()
        }
    }

    /// A reactive axis is clamped against the capability and the
    /// physical band at the P measured before the step, whichever
    /// axis is handed its command first: the active command moving P
    /// to the kVA rim in the same step does not squeeze Q to 0.
    #[test]
    fn the_reactive_base_and_band_are_read_before_any_axis_is_commanded() {
        // Fresh sites, so the axes' map order varies between runs.
        for i in 0..32 {
            let site = MicrogridSite::new();
            let pq = put(
                &site,
                Arc::new(Pq {
                    p: Mutex::new(0.0),
                    q: Mutex::new(None),
                }),
            );
            let gw = site.gateway();
            gw.command(1, Active, 1000.0).unwrap();
            gw.command(1, Reactive, 500.0).unwrap();
            site.tick_n(1, DT);
            assert_eq!(*pq.p.lock(), 1000.0, "iteration {i}");
            assert_eq!(*pq.q.lock(), Some(500.0), "iteration {i}");
        }
    }

    /// A running charge command tapers to nothing at `:soc-upper`
    /// instead of running the battery past it; the command stands,
    /// and the battery never has to clip what it is pushed.
    #[test]
    fn a_running_setpoint_holds_inside_the_soc_window() {
        let site = MicrogridSite::new();
        site.register(nearly_full_pack(1));
        site.register(instant_inverter(2));
        site.connect(2, 1);
        site.gateway().command(2, Active, 3_600.0).unwrap();
        // 3.6 kW into 1 kWh is 0.01 % per 100 ms tick: 50 ticks to
        // the window edge.
        site.tick_n(200, DT);
        let bat = site.get(1).unwrap();
        let soc = bat.telemetry(&site).soc_pct.unwrap();
        assert!(soc <= 90.03, "the window holds within one delay, got {soc}");
        assert!(soc >= 89.9, "it charged up to the window, got {soc}");
        assert!(
            (bat.dc_accept_ratio() - 1.0).abs() < 1e-6,
            "nothing to clip"
        );
        assert!(site.get(2).unwrap().aggregate_power_w(&site).abs() < 1.0);
        assert!(
            site.gateway().remaining_lifetime(2, Active).is_some(),
            "the command stands"
        );
    }

    /// With the default 100 ms device delay in the path, the window
    /// is exceeded by at most the tick the last command inside it
    /// runs for plus one device delay: 3.6 kW for 100 ms into 1 kWh
    /// is 0.01 % each.
    #[test]
    fn the_window_holds_within_one_device_delay() {
        let site = MicrogridSite::new();
        site.register(nearly_full_pack(1));
        site.register(BatteryInverter::new(
            2,
            Duration::from_secs(1),
            BatteryInverterConfig {
                rated_lower_w: -10_000.0,
                rated_upper_w: 10_000.0,
                ..Default::default()
            },
        ));
        site.connect(2, 1);
        site.gateway().command(2, Active, 3_600.0).unwrap();
        let peak = peak_soc(&site, 200);
        assert!(peak >= 89.99, "it charged up to the window, got {peak}");
        assert!(peak <= 90.0 + 0.02 + 1e-3, "past the allowance, got {peak}");
        assert!(site.get(2).unwrap().aggregate_power_w(&site).abs() < 1.0);
    }

    /// Two inverters on one battery share its shrinking room in
    /// proportion to their pushes, and the second, starting from 0,
    /// still gets its part.
    #[test]
    fn two_inverters_share_the_room_in_proportion() {
        let site = MicrogridSite::new();
        site.register(Battery::new(
            1,
            Duration::from_secs(1),
            BatteryConfig {
                capacity_wh: 1_000_000.0,
                initial_soc_pct: 85.0,
                soc_upper_pct: 90.0,
                soc_protect_margin_pct: 10.0,
                rated_lower_w: -10_000.0,
                rated_upper_w: 10_000.0,
                ..Default::default()
            },
        ));
        site.register(instant_inverter(2));
        site.register(instant_inverter(3));
        site.connect(2, 1);
        site.connect(3, 1);
        site.gateway().command(2, Active, 2_400.0).unwrap();
        site.tick_n(3, DT);
        let bat = site.get(1).unwrap();
        assert!(bat.set_soc_pct(89.0));
        site.tick_n(3, DT);
        site.gateway().command(3, Active, 1_200.0).unwrap();
        site.tick_n(5, DT);

        let soc = bat.telemetry(&site).soc_pct.unwrap();
        let room =
            soc_protected_bounds(-10_000.0, 10_000.0, soc, SocProtect::new(10.0, 90.0, 10.0)).1;
        assert!(
            room < 3_600.0,
            "the test needs the room to be short, got {room}"
        );
        let a = site.get(2).unwrap().aggregate_power_w(&site);
        let b = site.get(3).unwrap().aggregate_power_w(&site);
        assert!((a / b - 2.0).abs() < 0.01, "in proportion: {a} / {b}");
        assert!(
            (a + b - room).abs() < 5.0,
            "together they fill the room {room}: {a} + {b}"
        );
        assert!(
            b > 1_000.0,
            "the inverter that started at 0 got its part: {b}"
        );
        assert!(
            (bat.dc_accept_ratio() - 1.0).abs() < 1e-3,
            "nothing to clip"
        );
    }

    /// While its battery has room, a battery inverter's ramp is left
    /// alone: a lower setpoint, a sign flip and a lifetime expiry are
    /// all ramped at the ramp rate, not jumped to.
    #[test]
    fn a_battery_with_room_leaves_the_ramp_alone() {
        let site = MicrogridSite::new();
        site.register(Battery::new(
            1,
            Duration::from_secs(1),
            BatteryConfig {
                rated_lower_w: -10_000.0,
                rated_upper_w: 10_000.0,
                ..Default::default()
            },
        ));
        site.register(ramping_inverter(2));
        site.connect(2, 1);
        let inv = site.get(2).unwrap();
        let out = || inv.aggregate_power_w(&site);
        let gw = site.gateway();
        let run = site.run_generation();
        let set = |w: f32, lifetime_s: u64| {
            gw.set_power(
                2,
                run,
                Active,
                w,
                Duration::from_secs(lifetime_s),
                Mode::Reject,
            )
            .unwrap();
        };
        set(3_000.0, 60);
        site.tick_n(40, DT);
        assert!((out() - 3_000.0).abs() < 1.0, "settled, got {}", out());

        set(1_000.0, 60);
        site.tick_n(1, DT);
        assert!((out() - 2_900.0).abs() < 1.0, "ramps down, got {}", out());
        site.tick_n(25, DT);
        assert!((out() - 1_000.0).abs() < 1.0, "settled, got {}", out());

        set(-1_000.0, 5);
        site.tick_n(1, DT);
        assert!((out() - 900.0).abs() < 1.0, "ramps across 0, got {}", out());
        site.tick_n(25, DT);
        assert!((out() + 1_000.0).abs() < 1.0, "settled, got {}", out());

        // The 5 s lifetime runs out within these ticks; the axis then
        // ramps toward 0 instead of jumping there.
        let mut last = out();
        for _ in 0..30 {
            site.tick_n(1, DT);
            let now = out();
            assert!(
                (now - last).abs() <= 101.0,
                "at most 100 W per tick: {last} -> {now}"
            );
            last = now;
        }
        assert_eq!(gw.remaining_lifetime(2, Active), None, "expired");
        assert!(last > -1_000.0 && last < 0.0, "on its way to 0, got {last}");
    }

    /// A 1 MWh battery at `soc` %, ±5 kW, tapering over the 10 %
    /// below a 90 % `:soc-upper`.
    fn tapering_battery(id: u64, soc: f32) -> Battery {
        Battery::new(
            id,
            Duration::from_secs(1),
            BatteryConfig {
                capacity_wh: 1_000_000.0,
                initial_soc_pct: soc,
                soc_upper_pct: 90.0,
                soc_protect_margin_pct: 10.0,
                rated_lower_w: -5_000.0,
                rated_upper_w: 5_000.0,
                ..Default::default()
            },
        )
    }

    /// The charge room `tapering_battery` leaves at its current SoC.
    fn charge_room(site: &MicrogridSite, battery: u64) -> f32 {
        let soc = site.get(battery).unwrap().telemetry(site).soc_pct.unwrap();
        soc_protected_bounds(-5_000.0, 5_000.0, soc, SocProtect::new(10.0, 90.0, 10.0)).1
    }

    /// An `instant_inverter` (2) over two `tapering_battery`s: 1 at
    /// 89 %, short of room, and 3 at 50 %.
    fn two_tapering_batteries() -> MicrogridSite {
        let site = MicrogridSite::new();
        site.register(tapering_battery(1, 89.0));
        site.register(tapering_battery(3, 50.0));
        site.register(instant_inverter(2));
        site.connect(2, 1);
        site.connect(2, 3);
        site
    }

    /// An inverter on two batteries splits its output equally, so the
    /// nearly full one sets its output: neither battery clips.
    #[test]
    fn an_inverter_on_two_batteries_overloads_neither() {
        let site = two_tapering_batteries();
        site.gateway().command(2, Active, 6_000.0).unwrap();
        site.tick_n(10, DT);

        let room = charge_room(&site, 1);
        assert!(room < 3_000.0, "the test needs battery 1 short, got {room}");
        for b in [1, 3] {
            let ratio = site.get(b).unwrap().dc_accept_ratio();
            assert!((ratio - 1.0).abs() < 1e-3, "battery {b} clips: {ratio}");
        }
        let out = site.get(2).unwrap().aggregate_power_w(&site);
        assert!(
            (out - 2.0 * room).abs() < 5.0,
            "twice the tight room {room}, got {out}"
        );
    }

    /// When a battery's room narrows under a ramping inverter, the
    /// output is cut to the room at once, not ramped down to it.
    #[test]
    fn a_narrowing_room_cuts_a_ramping_output_at_once() {
        let site = MicrogridSite::new();
        site.register(tapering_battery(1, 50.0));
        site.register(ramping_inverter(2));
        site.connect(2, 1);
        let inv = site.get(2).unwrap();
        site.gateway().command(2, Active, 3_000.0).unwrap();
        site.tick_n(40, DT);
        assert!((inv.aggregate_power_w(&site) - 3_000.0).abs() < 1.0);

        let bat = site.get(1).unwrap();
        assert!(bat.set_soc_pct(89.5));
        // The gateway reads the new SoC on its next step and cuts the
        // output in that same step; at 1 kW/s the ramp alone could
        // not have come down by more than 100 W.
        site.tick_n(1, DT);
        let room = charge_room(&site, 1);
        assert!(room < 2_000.0, "the test needs the room short, got {room}");
        let out = inv.aggregate_power_w(&site);
        assert!(
            (out - room).abs() < 5.0,
            "cut to the room {room}, got {out}"
        );
        assert!(
            (bat.dc_accept_ratio() - 1.0).abs() < 1e-3,
            "the battery no longer clips"
        );
    }

    /// A `nearly_full_pack` (1) under a `ramping_inverter` (2)
    /// settled at `power_w`, its SoC then put to `soc_pct`. At ±5 kW
    /// and 89.9 % or 10.1 % that is about seven ticks short of the
    /// window's edge.
    fn settled_at_the_edge(power_w: f32, soc_pct: f32) -> MicrogridSite {
        let site = MicrogridSite::new();
        site.register(nearly_full_pack(1));
        site.register(ramping_inverter(2));
        site.connect(2, 1);
        let bat = site.get(1).unwrap();
        assert!(bat.set_soc_pct(50.0));
        site.gateway().command(2, Active, power_w).unwrap();
        site.tick_n(60, DT);
        let out = site.get(2).unwrap().aggregate_power_w(&site);
        assert!((out - power_w).abs() < 1.0, "settled, got {out}");
        assert!(bat.set_soc_pct(soc_pct));
        site
    }

    /// Battery 1's SoC after each of the next `ticks` ticks.
    fn soc_trace(site: &MicrogridSite, ticks: usize) -> Vec<f32> {
        let bat = site.get(1).unwrap();
        (0..ticks)
            .map(|_| {
                site.tick_n(1, DT);
                bat.telemetry(site).soc_pct.unwrap()
            })
            .collect()
    }

    /// The peak SoC of battery 1 over the next `ticks` ticks.
    fn peak_soc(site: &MicrogridSite, ticks: usize) -> f32 {
        soc_trace(site, ticks).into_iter().fold(0.0, f32::max)
    }

    /// A 0 command on a charging inverter still holds the window
    /// while the output ramps down to it: 5 kW ramping down at 1 kW/s
    /// would put 0.35 % into the 1 kWh pack.
    #[test]
    fn a_zero_command_holds_the_window_while_the_output_ramps_down() {
        let site = settled_at_the_edge(5_000.0, 89.9);
        site.gateway().command(2, Active, 0.0).unwrap();
        let peak = peak_soc(&site, 80);
        assert!(peak <= 90.02, "past the window, got {peak}");
    }

    /// The discharge side of
    /// `a_zero_command_holds_the_window_while_the_output_ramps_down`:
    /// a 0 command on a discharging inverter holds the window at
    /// `:soc-lower` while the output ramps up to 0.
    #[test]
    fn a_zero_command_holds_the_lower_window_while_the_output_ramps_up() {
        let site = settled_at_the_edge(-5_000.0, 10.1);
        site.gateway().command(2, Active, 0.0).unwrap();
        let lowest = soc_trace(&site, 80).into_iter().fold(100.0, f32::min);
        assert!(lowest >= 9.98, "past the window, got {lowest}");
    }

    /// A command across 0 on a charging inverter holds the window
    /// while the output ramps down through 0: the push on the charge
    /// side comes from the ramp output, not the discharge target.
    #[test]
    fn a_reversed_command_holds_the_window_while_the_output_ramps_down() {
        let site = settled_at_the_edge(5_000.0, 89.9);
        site.gateway().command(2, Active, -5_000.0).unwrap();
        let peak = peak_soc(&site, 80);
        assert!(peak <= 90.02, "past the window, got {peak}");
    }

    /// An augmentation that leaves 0 out pulls a small command up to
    /// its edge; the share judges that pulled-up target, so a battery
    /// short of room parks the output at 0 on every tick instead of
    /// letting the edge through on every other tick.
    #[test]
    fn an_augmentation_pulling_the_target_up_still_holds_the_window() {
        for cmd in [200.0_f32, -200.0] {
            let site = MicrogridSite::new();
            site.register(tapering_battery(1, 89.0));
            site.register(instant_inverter(2));
            site.connect(2, 1);
            let gw = site.gateway();
            gw.command(2, Active, cmd).unwrap();
            site.tick_n(3, DT);
            gw.augment(
                2,
                site.run_generation(),
                Active,
                VecBounds::single(3_000.0, 5_000.0),
                Duration::from_secs(60),
            )
            .unwrap();
            let inv = site.get(2).unwrap();
            let mut outs = Vec::new();
            for _ in 0..20 {
                site.tick_n(1, DT);
                let room = charge_room(&site, 1);
                assert!(room < 3_000.0, "the test needs the room short, got {room}");
                outs.push(inv.aggregate_power_w(&site));
            }
            let room = charge_room(&site, 1);
            assert!(
                outs.iter().all(|o| *o <= room + 1.0),
                "{cmd}: past the room {room}: {outs:?}"
            );
            assert!(
                outs.windows(2).all(|w| w[0] == w[1]),
                "{cmd}: alternates: {outs:?}"
            );
        }
    }

    /// A lifetime that runs out near `:soc-upper` sends the axis to
    /// its idle value; the window still holds the output on the way.
    #[test]
    fn an_expiry_near_the_window_edge_holds_the_window() {
        let site = settled_at_the_edge(5_000.0, 89.9);
        site.gateway()
            .set_power(
                2,
                site.run_generation(),
                Active,
                5_000.0,
                Duration::from_millis(300),
                Mode::Reject,
            )
            .unwrap();
        let peak = peak_soc(&site, 80);
        assert_eq!(site.gateway().remaining_lifetime(2, Active), None);
        assert!(peak <= 90.02, "past the window, got {peak}");
    }

    /// A lowered command whose target lies inside a tapering room,
    /// while the output is still above that room, is cut to the room
    /// on the next tick instead of ramping down through it.
    #[test]
    fn a_lowered_command_inside_the_room_cuts_the_output_to_it() {
        let site = MicrogridSite::new();
        site.register(tapering_battery(1, 50.0));
        site.register(ramping_inverter(2));
        site.connect(2, 1);
        let inv = site.get(2).unwrap();
        site.gateway().command(2, Active, 4_000.0).unwrap();
        site.tick_n(50, DT);
        assert!((inv.aggregate_power_w(&site) - 4_000.0).abs() < 1.0);

        assert!(site.get(1).unwrap().set_soc_pct(89.7));
        site.gateway().command(2, Active, 200.0).unwrap();
        site.tick_n(1, DT);
        let room = charge_room(&site, 1);
        assert!(room < 3_000.0, "the test needs the room short, got {room}");
        let out = inv.aggregate_power_w(&site);
        assert!(out <= room + 5.0, "cut to the room {room}, got {out}");
    }

    /// An inverter over two batteries, one of them faulted, pushes
    /// everything into the healthy one; its window share follows, so
    /// the healthy battery stays within its throttled bound.
    #[test]
    fn a_faulted_battery_leaves_the_whole_room_to_the_healthy_one() {
        let site = two_tapering_batteries();
        site.set_health(3, Health::Error).unwrap();
        site.gateway().command(2, Active, 6_000.0).unwrap();
        site.tick_n(10, DT);

        let room = charge_room(&site, 1);
        assert!(room < 3_000.0, "the test needs battery 1 short, got {room}");
        let bat = site.get(1).unwrap();
        let took = bat.aggregate_power_w(&site);
        assert!(took <= room + 5.0, "over the room {room}, got {took}");
        assert!(took > room - 5.0, "it fills the room {room}, got {took}");
        assert!(
            (bat.dc_accept_ratio() - 1.0).abs() < 1e-3,
            "the healthy battery clips"
        );
    }

    /// With no gateway in the path, the hardware charges a battery
    /// all the way to 100 % and stops there.
    #[test]
    fn the_hardware_alone_charges_a_battery_to_full() {
        let site = MicrogridSite::new();
        site.register(Battery::new(
            1,
            Duration::from_secs(1),
            BatteryConfig {
                capacity_wh: 1_000.0,
                initial_soc_pct: 95.0,
                ..Default::default()
            },
        ));
        site.register(instant_inverter(2));
        site.connect(2, 1);
        let inv = site.get(2).unwrap();
        let mut now = site.now();
        for _ in 0..1_000 {
            now += chrono::Duration::milliseconds(100);
            inv.set_command(Active, 3_600.0);
            site.tick_hardware(now, DT);
        }
        let bat = site.get(1).unwrap();
        assert!((bat.telemetry(&site).soc_pct.unwrap() - 100.0).abs() < 1e-3);
        assert!(
            bat.aggregate_power_w(&site).abs() < 1.0,
            "a full pack takes nothing"
        );
    }

    /// A window share that closes the charge band of a two-band
    /// augmentation parks the charge command at 0: it never turns it
    /// into a discharge on the augmentation's other band.
    #[test]
    fn a_share_below_a_gap_parks_at_zero_not_across() {
        use crate::proto::common::metrics::Bounds;
        let site = MicrogridSite::new();
        site.register(tapering_battery(1, 50.0));
        site.register(instant_inverter(2));
        site.register(instant_inverter(3));
        site.connect(2, 1);
        site.connect(3, 1);
        let gw = site.gateway();
        let two_bands = VecBounds::new(vec![
            Bounds {
                lower: Some(-500.0),
                upper: Some(-300.0),
            },
            Bounds {
                lower: Some(2_000.0),
                upper: Some(10_000.0),
            },
        ]);
        gw.augment(
            2,
            site.run_generation(),
            Active,
            two_bands,
            Duration::from_secs(600),
        )
        .unwrap();
        gw.command(2, Active, 2_400.0).unwrap();
        gw.command(3, Active, 2_500.0).unwrap();
        site.tick_n(5, DT);
        assert!(site.get(1).unwrap().set_soc_pct(89.0));
        let mut outs = Vec::new();
        for _ in 0..20 {
            site.tick_n(1, DT);
            let room = charge_room(&site, 1);
            assert!(
                room < 2_000.0,
                "the test needs the share below the gap, got {room}"
            );
            outs.push(site.get(2).unwrap().aggregate_power_w(&site));
        }
        for a in &outs {
            assert!(*a >= 0.0, "inverter 2 crossed to discharge: {outs:?}");
        }
    }
}
