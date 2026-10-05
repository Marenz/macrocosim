//! EV charger — an AC charging point. The gateway drives the *limit*
//! the charger offers (its command, delayed by the device and clamped
//! to the rated band); the connected car, if any, decides what it
//! draws within that limit. See `ev_presets` for the car and the draw
//! law.

use std::{fmt, str::FromStr, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::sim::{
    Category, Controllable, MicrogridSite, SimulatedComponent, Telemetry,
    bounds::VecBounds,
    component::{GatewaySettings, KnobKind, KnobSnapshot},
    decay::sanitize_soc_pct,
    device_axis::DeviceAxis,
    ev_presets::{ConnectedEv, EvDrawState, EvInfo, offered_current_a},
    runtime::Health,
};
use crate::timeout_tracker::SetpointAxis;

/// What the charger offers when no command stands (or a TTL expired).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EvIdle {
    /// Offer nothing: the safe state the API's expiry semantics mean.
    #[default]
    Paused,
    /// Offer the full rating, as a charger with no EMS does.
    Full,
}

impl FromStr for EvIdle {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "paused" => Ok(EvIdle::Paused),
            "full" => Ok(EvIdle::Full),
            _ => Err(()),
        }
    }
}

impl fmt::Display for EvIdle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EvIdle::Paused => "paused",
            EvIdle::Full => "full",
        })
    }
}

#[derive(Clone, Debug)]
pub struct EvChargerConfig {
    pub rated_lower_w: f32,
    pub rated_upper_w: f32,
    /// 1 or 3: how many phases the charger is wired on.
    pub phases: u8,
    pub idle: EvIdle,
    pub command_delay: Duration,
    pub ramp_rate_w_per_s: f32,
    pub stream_jitter_pct: f32,
    /// Keep the standing command through a health fault and ramp back
    /// to it on recovery, like the solar inverter; off, the fault
    /// clears the command and recovery waits for a new one, like the
    /// battery inverter. A request lifetime that runs out while the
    /// charger is faulted still clears the command.
    pub resume_on_recovery: bool,
    /// Time a command takes to reach the offered limit once the
    /// charger has it; default 100 ms.
    pub device_delay: Duration,
}

impl EvChargerConfig {
    /// What the charger offers with no command standing: nothing, or
    /// its full rating.
    fn idle_w(&self) -> f32 {
        match self.idle {
            EvIdle::Paused => 0.0,
            EvIdle::Full => self.rated_upper_w,
        }
    }
}

impl Default for EvChargerConfig {
    fn default() -> Self {
        Self {
            rated_lower_w: 0.0,
            rated_upper_w: 22_000.0,
            phases: 3,
            idle: EvIdle::Paused,
            command_delay: Duration::from_millis(500),
            ramp_rate_w_per_s: f32::INFINITY,
            stream_jitter_pct: 0.0,
            resume_on_recovery: false,
            device_delay: crate::sim::inverter::DEFAULT_DEVICE_DELAY,
        }
    }
}

/// A charger config that answers a command the same tick: no delay,
/// no slew. The starting point for every charger test.
#[cfg(test)]
pub fn instant() -> EvChargerConfig {
    EvChargerConfig {
        command_delay: Duration::ZERO,
        ramp_rate_w_per_s: f32::INFINITY,
        device_delay: Duration::ZERO,
        ..Default::default()
    }
}

pub struct EvCharger {
    id: u64,
    name: String,
    interval: Duration,
    cfg: EvChargerConfig,
    state: Mutex<EvState>,
    /// The offered limit: the command handed in through
    /// `set_command`, delayed and clamped to the rated band.
    limit: DeviceAxis,
}

#[derive(Debug, Clone)]
struct EvState {
    ev: Option<ConnectedEv>,
    /// Power taken this tick.
    draw_w: f32,
    /// What the charger did with its car this tick; `Tripped` while
    /// faulted, `Paused` for an empty charger too (nothing to draw).
    last: EvDrawState,
}

impl EvCharger {
    pub fn new(id: u64, interval: Duration, cfg: EvChargerConfig) -> Self {
        Self {
            id,
            name: format!("ev-charger-{id}"),
            interval,
            limit: DeviceAxis::new(cfg.device_delay, 0.0),
            cfg,
            state: Mutex::new(EvState {
                ev: None,
                draw_w: 0.0,
                last: EvDrawState::Paused,
            }),
        }
    }
}

impl fmt::Display for EvCharger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for EvCharger {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::EvCharger
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
        let mut s = self.state.lock();
        let Some(ev) = s.ev.as_mut() else {
            return false;
        };
        if let Some(pct) = sanitize_soc_pct("EvCharger::set_soc_pct", pct) {
            ev.soc_pct = pct;
        }
        true
    }

    fn takes_soc_pct(&self) -> bool {
        self.state.lock().ev.is_some()
    }

    fn takes_ev(&self) -> bool {
        true
    }

    fn plug_ev(&self, ev: ConnectedEv) -> Result<(), String> {
        let mut s = self.state.lock();
        if s.ev.is_some() {
            return Err("an EV is already plugged in".to_string());
        }
        s.ev = Some(ev);
        // A fresh car starts from a clean slate, the way `unplug_ev`
        // and `restore_knob` leave one: otherwise the draw and state
        // of whatever was here before are reported until the next
        // tick overwrites them.
        s.draw_w = 0.0;
        s.last = EvDrawState::Paused;
        Ok(())
    }

    fn unplug_ev(&self) -> bool {
        let mut s = self.state.lock();
        let had = s.ev.take().is_some();
        s.draw_w = 0.0;
        s.last = EvDrawState::Paused;
        had
    }

    fn ev_info(&self) -> Option<EvInfo> {
        let s = self.state.lock();
        s.ev.clone().map(|ev| EvInfo { ev, state: s.last })
    }

    fn snapshot_knob(&self, kind: KnobKind) -> Option<KnobSnapshot> {
        match kind {
            KnobKind::Ev => Some(KnobSnapshot::Ev(self.state.lock().ev.clone())),
            _ => None,
        }
    }

    fn restore_knob(&self, snap: KnobSnapshot) -> bool {
        match snap {
            KnobSnapshot::Ev(ev) => {
                let mut s = self.state.lock();
                s.ev = ev;
                s.draw_w = 0.0;
                s.last = EvDrawState::Paused;
                true
            }
            _ => false,
        }
    }

    fn tick(&self, world: &MicrogridSite, now: DateTime<Utc>, dt: Duration) {
        // 1. Own-health gate: a faulted or standby charger is offline
        //    — no offer, nothing left in the delay line. Whether the
        //    command survives is the gateway's business.
        if world.runtime_of(self.id).health != Health::Ok {
            self.limit.trip();
            let mut s = self.state.lock();
            s.draw_w = 0.0;
            s.last = EvDrawState::Tripped;
            return;
        }

        // 2. The offer: the delayed command, clamped to the rating.
        let rated = VecBounds::single(self.cfg.rated_lower_w, self.cfg.rated_upper_w);
        let limit_w = self.limit.tick(now, Some(&rated));

        // 3. The car decides what it takes within the offer.
        let mut s = self.state.lock();
        let offered = offered_current_a(limit_w, self.cfg.phases);
        let (draw, last) = match (s.ev.as_mut(), offered) {
            (None, _) => (0.0, EvDrawState::Paused),
            (Some(ev), _) if ev.done() => (0.0, EvDrawState::Done),
            (Some(_), None) => (0.0, EvDrawState::Paused),
            (Some(ev), Some(amps)) => {
                let p = ev.draw_w(amps, self.cfg.phases);
                ev.integrate(p, dt);
                (
                    p,
                    if p > 0.0 {
                        EvDrawState::Charging
                    } else {
                        EvDrawState::Paused
                    },
                )
            }
        };
        s.draw_w = draw;
        s.last = last;
    }

    fn telemetry(&self, site: &MicrogridSite) -> Telemetry {
        let grid = site.grid_state();
        let s = self.state.lock();
        Telemetry {
            id: self.id,
            category: Some(Category::EvCharger),
            active_power_w: Some(s.draw_w),
            // A P-only AC component: Q is settled at 0, said explicitly
            // so the stream and the UI do not read it as missing.
            reactive_power_var: Some(0.0),
            // The car's SoC rides the internal telemetry for the UI,
            // history and CSV; proto_conv drops it from the gRPC stream.
            soc_pct: s.ev.as_ref().map(|ev| ev.soc_pct),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            frequency_hz: Some(grid.frequency_hz),
            component_state: Some(if s.draw_w > 0.0 { "charging" } else { "ready" }),
            cable_states: if s.ev.is_some() {
                &[
                    "ev-charging-cable-locked-at-station",
                    "ev-charging-cable-locked-at-ev",
                ]
            } else {
                &["ev-charging-cable-unplugged"]
            },
            ..Default::default()
        }
    }

    fn active_power_w(&self, _site: &MicrogridSite) -> Option<f32> {
        Some(self.state.lock().draw_w)
    }

    fn aggregate_power_w(&self, _world: &MicrogridSite) -> f32 {
        self.state.lock().draw_w
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        Some((self.cfg.rated_lower_w, self.cfg.rated_upper_w))
    }

    fn controllable(&self) -> Option<&dyn Controllable> {
        Some(self)
    }

    fn make_fn(&self) -> &'static str {
        "%make-ev-charger"
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        let lf = crate::lisp::lisp_float32;
        let mut kw = vec![
            (":rated-lower", lf(self.cfg.rated_lower_w)),
            (":rated-upper", lf(self.cfg.rated_upper_w)),
            (
                ":command-delay-ms",
                self.cfg.command_delay.as_millis().to_string(),
            ),
        ];
        if self.cfg.phases != 3 {
            kw.push((":phases", self.cfg.phases.to_string()));
        }
        if self.cfg.idle != EvIdle::Paused {
            kw.push((":idle", format!("'{}", self.cfg.idle)));
        }
        if self.cfg.ramp_rate_w_per_s.is_finite() {
            kw.push((":ramp-rate", lf(self.cfg.ramp_rate_w_per_s)));
        }
        kw.extend(crate::sim::inverter::device_delay_kw(self.cfg.device_delay));
        if self.interval != Duration::from_millis(1000) {
            kw.push((":interval", self.interval.as_millis().to_string()));
        }
        if self.cfg.stream_jitter_pct != 0.0 {
            kw.push((":stream-jitter-pct", lf(self.cfg.stream_jitter_pct)));
        }
        if self.cfg.resume_on_recovery {
            kw.push((":resume-on-recovery", "t".to_string()));
        }
        kw
    }
}

impl Controllable for EvCharger {
    fn has_axis(&self, axis: SetpointAxis) -> bool {
        axis == SetpointAxis::Active
    }

    fn set_command(&self, axis: SetpointAxis, value: f32) {
        if axis == SetpointAxis::Active {
            self.limit.set_command(value);
        }
    }

    /// The offered limit has no physical band: the car draws what it
    /// wants beneath it.
    fn physical_band(&self, _axis: SetpointAxis, _dt: Duration) -> Option<VecBounds> {
        None
    }

    /// With no command the charger offers nothing (`'paused`) or its
    /// full rating (`'full`).
    fn idle_value(&self, _axis: SetpointAxis) -> Option<f32> {
        Some(self.cfg.idle_w())
    }

    fn keeps_command_through_fault(&self, _axis: SetpointAxis) -> bool {
        self.cfg.resume_on_recovery
    }

    fn gateway_settings(&self) -> GatewaySettings {
        GatewaySettings {
            command_delay: self.cfg.command_delay,
            ramp_rate_w_per_s: self.cfg.ramp_rate_w_per_s,
            ..GatewaySettings::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::common::metrics::Bounds;
    use crate::sim::AugmentError;
    use crate::sim::ev_presets::{EvOverrides, preset, test_car};
    use crate::sim::gateway::{Applied, GatewayError, Mode};
    use crate::sim::runtime::Health;
    use crate::timeout_tracker::SetpointAxis;
    use std::sync::Arc;

    /// A 22 kW three-phase charger with no delay and no ramp, sited
    /// under id 7 so tests can drive its health.
    fn sited(cfg: EvChargerConfig) -> (MicrogridSite, Arc<dyn SimulatedComponent>) {
        let w = MicrogridSite::new();
        w.register(EvCharger::new(7, Duration::from_secs(1), cfg));
        let ev = w.get(7).unwrap();
        (w, ev)
    }

    /// `n` one-second ticks of the whole site.
    fn tick_n(w: &MicrogridSite, n: usize) {
        w.tick_n(n, Duration::from_secs(1));
    }

    /// Command charger 7 through the gateway.
    fn cmd(w: &MicrogridSite, watts: f32) -> Result<Applied, GatewayError> {
        w.gateway().command(7, SetpointAxis::Active, watts)
    }

    #[test]
    fn empty_charger_draws_nothing_and_still_advertises_its_rating() {
        let (w, ev) = sited(instant());
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 3);
        assert_eq!(ev.aggregate_power_w(&w), 0.0);
        let b = w.bounds_of(7, SetpointAxis::Active).unwrap();
        assert_eq!(b.0[0].upper, Some(22_000.0), "bounds are the charger's own");
        assert!(ev.ev_info().is_none());
    }

    #[test]
    fn plugged_car_draws_within_the_limit_on_its_own_phases() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("city", Some(30.0))).unwrap(); // 1 ph, 32 A
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        // 22 kW over the charger's three phases is 31.9 A; the car takes
        // that on its single phase: 7.33 kW.
        let p = ev.aggregate_power_w(&w);
        assert!(
            (p - 22_000.0 / 3.0).abs() < 1.0,
            "one phase of the offer, got {p}"
        );
        assert_eq!(ev.ev_info().unwrap().state, EvDrawState::Charging);
    }

    #[test]
    fn limit_under_six_amps_pauses() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        cmd(&w, 4_000.0).unwrap(); // 5.8 A on 3 phases
        tick_n(&w, 2);
        assert_eq!(ev.aggregate_power_w(&w), 0.0);
        assert_eq!(ev.ev_info().unwrap().state, EvDrawState::Paused);
        cmd(&w, 4_200.0).unwrap(); // 6.09 A
        tick_n(&w, 2);
        assert!(ev.aggregate_power_w(&w) > 4_100.0);
    }

    #[test]
    fn idle_paused_draws_nothing_and_idle_full_draws_the_cap() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        tick_n(&w, 2);
        assert_eq!(ev.aggregate_power_w(&w), 0.0, "no command, paused idle");

        let (w, ev) = sited(EvChargerConfig {
            idle: EvIdle::Full,
            ..instant()
        });
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        tick_n(&w, 2);
        let p = ev.aggregate_power_w(&w);
        assert!((p - 3.0 * 230.0 * 16.0).abs() < 1.0, "11 kW cap, got {p}");
    }

    #[test]
    fn target_soc_ends_the_session_and_unplug_clears_it() {
        let (w, ev) = sited(instant());
        ev.plug_ev(
            ConnectedEv::new(
                preset("phev").unwrap(),
                &EvOverrides {
                    soc_pct: Some(99.9),
                    target_soc_pct: Some(100.0),
                    ..Default::default()
                },
                Utc::now(),
            )
            .unwrap(),
        )
        .unwrap();
        cmd(&w, 22_000.0).unwrap();
        // ~4 A after the taper on a 13 kWh pack: 0.1 % takes ~50 s.
        tick_n(&w, 90);
        assert_eq!(ev.ev_info().unwrap().state, EvDrawState::Done);
        assert_eq!(ev.aggregate_power_w(&w), 0.0);
        assert!(ev.unplug_ev());
        assert!(ev.ev_info().is_none());
        assert!(!ev.unplug_ev(), "empty already");
    }

    /// A plug is a clean slate: whatever the charger last did with
    /// its previous car (here `Tripped`, left behind by a fault the
    /// charger has since recovered from) must not be reported as the
    /// new car's state in the window before the next tick.
    /// Killing mutation: drop the `draw_w`/`last` reset in `plug_ev`.
    #[test]
    fn plugging_clears_the_previous_session_state() {
        let (w, ev) = sited(instant());
        w.set_health(7, Health::Error).unwrap();
        tick_n(&w, 1);
        assert_eq!(ev.ev_info().map(|i| i.state), None, "nothing plugged yet");
        w.set_health(7, Health::Ok).unwrap();
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        assert_eq!(
            ev.ev_info().unwrap().state,
            EvDrawState::Paused,
            "a freshly plugged car is paused, not tripped",
        );
        assert_eq!(ev.aggregate_power_w(&w), 0.0);
    }

    #[test]
    fn plug_while_plugged_errors() {
        let (_w, ev) = sited(instant());
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        assert!(ev.plug_ev(test_car("van", Some(30.0))).is_err());
        assert_eq!(ev.ev_info().unwrap().ev.preset, "sedan");
    }

    #[test]
    fn health_trip_still_zeroes_and_clears_the_command() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        assert!(ev.aggregate_power_w(&w) > 10_000.0);
        w.set_health(7, Health::Error).unwrap();
        tick_n(&w, 1);
        assert_eq!(
            w.gateway().remaining_lifetime(7, SetpointAxis::Active),
            None,
            "the lifetime goes with the command"
        );
        assert_eq!(ev.aggregate_power_w(&w), 0.0);
        assert_eq!(ev.ev_info().unwrap().state, EvDrawState::Tripped);
        w.set_health(7, Health::Ok).unwrap();
        tick_n(&w, 3);
        assert_eq!(
            ev.aggregate_power_w(&w),
            0.0,
            "no command survives the trip"
        );
    }

    #[test]
    fn ttl_expiry_slews_the_limit_and_the_draw_follows() {
        let (w, ev) = sited(EvChargerConfig {
            ramp_rate_w_per_s: 1_000.0,
            ..instant()
        });
        ev.plug_ev(test_car("van", Some(30.0))).unwrap(); // 3 ph 32 A = 22 kW cap
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 25);
        assert!((ev.aggregate_power_w(&w) - 22_000.0).abs() < 100.0);
        w.gateway().reset(7, SetpointAxis::Active);
        tick_n(&w, 1);
        let p = ev.aggregate_power_w(&w);
        assert!(p > 20_000.0 && p < 22_000.0, "one tick of slew, got {p}");
    }

    #[test]
    fn energy_matches_the_integrated_draw() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 10);
        let info = ev.ev_info().unwrap();
        // First tick has no draw yet (limit promoted this tick), so nine
        // seconds at 11 040 W.
        let expected = 9.0 * 11_040.0 / 3600.0;
        assert!(
            (info.ev.energy_wh - expected).abs() < 5.0,
            "got {}",
            info.ev.energy_wh
        );
    }

    #[test]
    fn set_soc_moves_the_plugged_car_only() {
        let (_w, ev) = sited(instant());
        assert!(!ev.takes_soc_pct(), "no car, no SoC");
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        assert!(ev.takes_soc_pct());
        assert!(ev.set_soc_pct(55.0));
        assert_eq!(ev.ev_info().unwrap().ev.soc_pct, 55.0);
    }

    #[test]
    fn knob_snapshot_restores_the_plug_state() {
        let (_w, ev) = sited(instant());
        let before = ev.snapshot_knob(KnobKind::Ev);
        assert!(matches!(before, Some(KnobSnapshot::Ev(None))));
        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        assert!(ev.restore_knob(before.unwrap()));
        assert!(ev.ev_info().is_none(), "restore unplugs");
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        let plugged = ev.snapshot_knob(KnobKind::Ev).unwrap();
        assert!(ev.unplug_ev());
        assert!(ev.restore_knob(plugged));
        assert_eq!(ev.ev_info().unwrap().ev.preset, "van");
    }

    #[test]
    fn constructor_kwargs_round_trip() {
        let kw = |cfg: EvChargerConfig| {
            EvCharger::new(9, Duration::from_secs(1), cfg)
                .constructor_kwargs()
                .into_iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let d = kw(EvChargerConfig::default());
        assert!(d.contains(":rated-upper 22000"), "{d}");
        assert!(!d.contains(":phases"), "default phases omitted: {d}");
        assert!(!d.contains(":idle"), "default idle omitted: {d}");
        assert!(!d.contains(":capacity"), "the pack is gone: {d}");
        let s = kw(EvChargerConfig {
            phases: 1,
            idle: EvIdle::Full,
            ..Default::default()
        });
        assert!(s.contains(":phases 1"), "{s}");
        assert!(s.contains(":idle 'full"), "{s}");
    }

    /// With `:resume-on-recovery` the fault still zeroes the draw,
    /// but the armed command survives it and charging ramps back when
    /// health returns — the way the solar inverter keeps its
    /// curtailment, and unlike the default charger below. Killing
    /// mutation: `keeps_command_through_fault` → `false`.
    #[test]
    fn resume_on_recovery_keeps_the_command_through_the_fault() {
        let (w, ev) = sited(EvChargerConfig {
            resume_on_recovery: true,
            ..instant()
        });
        ev.plug_ev(test_car("van", Some(30.0))).unwrap(); // 3 ph 32 A: takes the whole offer
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        assert!((ev.aggregate_power_w(&w) - 22_000.0).abs() < 100.0);

        w.set_health(7, Health::Error).unwrap();
        tick_n(&w, 3);
        assert!(
            w.gateway()
                .remaining_lifetime(7, SetpointAxis::Active)
                .is_some(),
            "the lifetime stays with the command"
        );
        assert_eq!(ev.aggregate_power_w(&w), 0.0, "stays off while faulted");
        assert_eq!(ev.ev_info().unwrap().state, EvDrawState::Tripped);

        w.set_health(7, Health::Ok).unwrap();
        tick_n(&w, 2);
        let p = ev.aggregate_power_w(&w);
        assert!(
            (p - 22_000.0).abs() < 100.0,
            "resumes the armed command, got {p}"
        );
    }

    /// Standby is offline exactly like Error — the gate is "not Ok",
    /// not "is Error" — and, without `:resume-on-recovery`, waking up
    /// does not resurrect the pre-fault command: the charger waits for
    /// the controller to re-dispatch.
    /// Killing mutation: `!= Health::Ok` → `== Health::Error`.
    #[test]
    fn standby_trips_like_an_error_and_awaits_redispatch() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        assert!(ev.aggregate_power_w(&w) > 10_000.0);

        w.set_health(7, Health::Standby).unwrap();
        tick_n(&w, 1);
        assert_eq!(ev.aggregate_power_w(&w), 0.0, "standby is offline too");
        assert_eq!(ev.ev_info().unwrap().state, EvDrawState::Tripped);

        w.set_health(7, Health::Ok).unwrap();
        tick_n(&w, 3);
        assert_eq!(
            ev.aggregate_power_w(&w),
            0.0,
            "waking up awaits a re-dispatch",
        );
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        assert!(
            ev.aggregate_power_w(&w) > 10_000.0,
            "a new command resumes charging",
        );
    }

    /// An augmentation narrows what the charger OFFERS, and the car
    /// draws within the narrowed offer — so a TTL narrowing reaches
    /// the draw even though the car, not the gateway, decides it. It
    /// tightens the validation envelope in the same breath.
    #[test]
    fn augmentation_narrows_the_offer_and_the_draw_follows() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        assert!((ev.aggregate_power_w(&w) - 22_000.0).abs() < 100.0);

        w.gateway()
            .augment(
                7,
                w.run_generation(),
                SetpointAxis::Active,
                VecBounds(vec![Bounds {
                    lower: Some(0.0),
                    upper: Some(7_000.0),
                }]),
                Duration::from_secs(60),
            )
            .unwrap();
        let eff = w.bounds_of(7, SetpointAxis::Active).unwrap();
        assert_eq!(eff.0[0].upper, Some(7_000.0), "the offer is narrowed");

        // 7 kW over three phases is 10.1 A — above the 6 A floor, so
        // the car tracks the narrowed offer down rather than pausing.
        tick_n(&w, 2);
        let p = ev.aggregate_power_w(&w);
        assert!(
            (p - 7_000.0).abs() < 50.0,
            "draw follows the offer, got {p}"
        );

        // And the narrowing is the validation envelope too.
        assert!(matches!(
            cmd(&w, 10_000.0),
            Err(GatewayError::OutOfEnvelope(_))
        ));
    }

    /// A band with no overlap with rated is refused rather than
    /// stored: accepting it would compose an empty envelope and park
    /// the charger at 0 W for the augmentation's whole lifetime. The
    /// rejection changes nothing — rated and the live draw survive.
    /// Killing mutation: the gateway swallowing the axis's `Err`.
    #[test]
    fn an_augmentation_disjoint_from_rated_is_rejected() {
        let (w, ev) = sited(instant());
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        cmd(&w, 22_000.0).unwrap();
        assert!(matches!(
            w.gateway().augment(
                7,
                w.run_generation(),
                SetpointAxis::Active,
                VecBounds(vec![Bounds {
                    lower: Some(30_000.0),
                    upper: Some(40_000.0),
                }]),
                Duration::from_secs(60),
            ),
            Err(AugmentError::Disjoint(_))
        ));
        assert_eq!(
            w.bounds_of(7, SetpointAxis::Active).unwrap().0[0].upper,
            Some(22_000.0),
            "rated survives a rejected augmentation",
        );
        tick_n(&w, 2);
        assert!(
            (ev.aggregate_power_w(&w) - 22_000.0).abs() < 100.0,
            "and the car still charges",
        );
    }

    /// The charger is a single-axis (P-only) component: Q is never
    /// augmented, and a Q augmentation or command is refused. Killing
    /// mutation: `has_axis` → `true` for every axis.
    #[test]
    fn augmentation_active_reports_only_the_active_axis() {
        let (w, _ev) = sited(instant());
        assert!(!w.gateway().augmented(7, SetpointAxis::Active));
        assert!(!w.gateway().augmented(7, SetpointAxis::Reactive));

        w.gateway()
            .augment(
                7,
                w.run_generation(),
                SetpointAxis::Active,
                VecBounds(vec![Bounds {
                    lower: Some(0.0),
                    upper: Some(7_000.0),
                }]),
                Duration::from_secs(60),
            )
            .unwrap();
        assert!(w.gateway().augmented(7, SetpointAxis::Active));
        assert!(
            !w.gateway().augmented(7, SetpointAxis::Reactive),
            "the charger has no reactive axis to narrow",
        );
        assert!(matches!(
            w.gateway().augment(
                7,
                w.run_generation(),
                SetpointAxis::Reactive,
                VecBounds::single(-1_000.0, 1_000.0),
                Duration::from_secs(60),
            ),
            Err(AugmentError::Unsupported)
        ));
        assert!(matches!(
            w.gateway().command(7, SetpointAxis::Reactive, 0.0),
            Err(GatewayError::NoAxis { .. })
        ));
    }

    /// A P-only AC component still advertises an EXPLICIT zero Q, so
    /// the stream and the UI read "settled at 0" rather than "metric
    /// missing". The plug-derived fields follow the car: `cable_states`
    /// says whether anything is connected and `soc_pct` is the car's,
    /// absent on an empty charger, which has no pack of its own.
    /// Killing mutation: `reactive_power_var: Some(0.0)` → `None`.
    #[test]
    fn telemetry_advertises_zero_reactive_and_tracks_the_plug() {
        let (w, ev) = sited(instant());
        let t = ev.telemetry(&w);
        assert_eq!(t.reactive_power_var, Some(0.0));
        assert_eq!(t.cable_states, &["ev-charging-cable-unplugged"][..]);
        assert_eq!(t.soc_pct, None, "an empty charger has no SoC to report");

        ev.plug_ev(test_car("sedan", Some(30.0))).unwrap();
        let t = ev.telemetry(&w);
        assert_eq!(t.reactive_power_var, Some(0.0));
        assert_eq!(
            t.cable_states,
            &[
                "ev-charging-cable-locked-at-station",
                "ev-charging-cable-locked-at-ev"
            ][..]
        );
        assert_eq!(t.soc_pct, Some(30.0), "the car's SoC, not the charger's");
    }

    /// `component_state` is the proto state code the charger reports:
    /// "charging" exactly while power is flowing, "ready" otherwise —
    /// including a charger whose car is plugged in but paused, which
    /// is what distinguishes it from `cable_states`.
    /// Killing mutation: a constant `Some("ready")` (or keying it off
    /// `s.ev.is_some()` rather than `s.draw_w`).
    #[test]
    fn component_state_tracks_the_flow_not_the_plug() {
        let (w, ev) = sited(instant());
        assert_eq!(
            ev.telemetry(&w).component_state,
            Some("ready"),
            "an empty charger is ready, not charging",
        );

        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        tick_n(&w, 2);
        assert_eq!(
            ev.telemetry(&w).component_state,
            Some("ready"),
            "plugged but no command: paused, so still ready",
        );

        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        assert!(ev.aggregate_power_w(&w) > 0.0);
        assert_eq!(ev.telemetry(&w).component_state, Some("charging"));

        // Below the 6 A floor the car pauses, and the state goes back
        // to ready with the cable still locked.
        cmd(&w, 4_000.0).unwrap();
        tick_n(&w, 2);
        assert_eq!(ev.aggregate_power_w(&w), 0.0);
        assert_eq!(ev.telemetry(&w).component_state, Some("ready"));
        assert_eq!(
            ev.telemetry(&w).cable_states,
            &[
                "ev-charging-cable-locked-at-station",
                "ev-charging-cable-locked-at-ev"
            ][..],
            "paused is not unplugged",
        );
    }

    /// `:phases` is the charger's own wiring, and it enters the draw
    /// twice: it divides the limit into a per-phase current, and it
    /// caps how many of the car's phases can be used. A 22 kW rated
    /// charger wired on one phase can only ever deliver ~7.4 kW, even
    /// to a three-phase car and even commanded to its full rating.
    /// Killing mutation: hardcode 3 for either `self.cfg.phases` use
    /// in `tick`.
    #[test]
    fn a_one_phase_charger_delivers_one_phase() {
        let (w, ev) = sited(EvChargerConfig {
            phases: 1,
            ..instant()
        });
        ev.plug_ev(test_car("van", Some(30.0))).unwrap(); // a 3 ph, 32 A car
        // 7 360 W on one phase is 32 A — the car's cap, on the single
        // phase the charger has.
        cmd(&w, 7_360.0).unwrap();
        tick_n(&w, 2);
        let p = ev.aggregate_power_w(&w);
        assert!((p - 7_360.0).abs() < 1.0, "one phase at 32 A, got {p}");

        // Commanding the full 22 kW rating changes nothing: the offer
        // is 95 A on the one phase, but the car takes only its 32 A.
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 2);
        let p = ev.aggregate_power_w(&w);
        assert!(
            (p - 7_360.0).abs() < 1.0,
            "still one phase of 32 A, not 22 kW, got {p}"
        );
    }

    /// A charger idling at its full rating parks there when its
    /// command is reset, instead of at 0.
    #[test]
    fn a_full_idle_charger_parks_at_its_rating() {
        let (w, ev) = sited(EvChargerConfig {
            idle: EvIdle::Full,
            ..instant()
        });
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        cmd(&w, 7_000.0).unwrap();
        tick_n(&w, 2);
        assert!((ev.aggregate_power_w(&w) - 7_000.0).abs() < 50.0);
        w.gateway().reset(7, SetpointAxis::Active);
        tick_n(&w, 2);
        assert!((ev.aggregate_power_w(&w) - 22_000.0).abs() < 100.0);
    }

    /// Command charger 7 with a request lifetime of `secs`.
    fn cmd_for(w: &MicrogridSite, watts: f32, secs: u64) {
        w.gateway()
            .set_power(
                7,
                w.run_generation(),
                SetpointAxis::Active,
                watts,
                Duration::from_secs(secs),
                Mode::Reject,
            )
            .unwrap();
    }

    /// A lifetime that runs out on the site clock slews the offer
    /// back to idle at the ramp rate, and the draw follows it down.
    #[test]
    fn a_lapsed_lifetime_slews_the_limit_and_the_draw_follows() {
        let (w, ev) = sited(EvChargerConfig {
            ramp_rate_w_per_s: 1_000.0,
            ..instant()
        });
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        cmd_for(&w, 22_000.0, 30);
        tick_n(&w, 29);
        assert!((ev.aggregate_power_w(&w) - 22_000.0).abs() < 100.0);
        tick_n(&w, 3);
        assert_eq!(
            w.gateway().remaining_lifetime(7, SetpointAxis::Active),
            None
        );
        let p = ev.aggregate_power_w(&w);
        assert!(p > 18_000.0 && p < 21_500.0, "a few ticks of slew, got {p}");
        tick_n(&w, 30);
        assert_eq!(ev.aggregate_power_w(&w), 0.0, "parked at the idle 0");
    }

    /// A kept command still goes when its lifetime runs out while the
    /// charger is faulted: recovery then finds nothing to resume.
    #[test]
    fn a_lifetime_lapsing_in_a_fault_clears_a_kept_command() {
        let (w, ev) = sited(EvChargerConfig {
            resume_on_recovery: true,
            ..instant()
        });
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        cmd_for(&w, 22_000.0, 5);
        tick_n(&w, 2);
        assert!((ev.aggregate_power_w(&w) - 22_000.0).abs() < 100.0);
        w.set_health(7, Health::Error).unwrap();
        tick_n(&w, 10);
        w.set_health(7, Health::Ok).unwrap();
        tick_n(&w, 3);
        assert_eq!(ev.aggregate_power_w(&w), 0.0, "nothing to resume");
    }

    /// A command still in the device delay line when the charger
    /// trips does not replay on recovery. Killing mutation: drop
    /// `self.limit.trip()` from the health gate.
    #[test]
    fn a_trip_does_not_replay_a_pre_trip_command() {
        let (w, ev) = sited(EvChargerConfig {
            device_delay: Duration::from_secs(2),
            ..instant()
        });
        ev.plug_ev(test_car("van", Some(30.0))).unwrap();
        cmd(&w, 22_000.0).unwrap();
        tick_n(&w, 1);
        assert_eq!(ev.aggregate_power_w(&w), 0.0, "still in the delay line");
        w.set_health(7, Health::Error).unwrap();
        tick_n(&w, 1);
        w.set_health(7, Health::Ok).unwrap();
        for _ in 0..3 {
            tick_n(&w, 1);
            assert_eq!(ev.aggregate_power_w(&w), 0.0, "nothing replays");
        }
    }
}
