//! `Gateway::step`: one physics tick of the API rules. Expire request
//! lifetimes and augmentations at `now`, then for every gateway-owned
//! axis: trip it when its component is unhealthy, else target, clamp
//! and ramp it, and hand the result to the component.

use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};

use super::{Gateway, GatewayState, log_expired};
use crate::sim::{SimulatedComponent, gateway_axis::AdvanceCtx, runtime::Health};
use crate::timeout_tracker::SetpointAxis;

/// One healthy owned axis between its target (step 1) and the command
/// handed to its component.
struct Planned {
    id: u64,
    axis: SetpointAxis,
    component: Arc<dyn SimulatedComponent>,
    target: Option<f32>,
}

impl Gateway<'_> {
    /// One physics tick of the API rules, run by
    /// `MicrogridSite::tick_once` before the components tick. Holds
    /// the gateway lock from start to end.
    pub fn step(&self, now: DateTime<Utc>, dt: Duration) {
        let mut st = self.gw.state.lock();
        let expired = self.expire_locked(&st, now);
        for ax in st.axes.values_mut() {
            ax.drop_expired(now);
        }
        let planned = self.plan_locked(&st, now);
        for p in &planned {
            let base = self.base_of(p.component.as_ref(), p.axis);
            let physical = p.component.physical_band(p.axis, dt);
            let ctx = AdvanceCtx {
                base: &base,
                physical: physical.as_ref(),
                share: None,
            };
            let out = st.axes[&(p.id, p.axis)].advance(p.target, now, dt, &ctx);
            p.component.set_command(p.axis, out);
        }
        drop(st);
        log_expired(&expired);
    }

    /// Trip every owned axis whose component is unhealthy — the ramp
    /// snaps to 0, the command and its lifetime go unless the
    /// component keeps them, and 0 is handed — and target every other.
    fn plan_locked(&self, st: &GatewayState, now: DateTime<Utc>) -> Vec<Planned> {
        let mut planned = Vec::new();
        for (&(id, axis), ax) in &st.axes {
            let Some(c) = self.site.get(id) else {
                continue;
            };
            if self.site.runtime_of(id).health != Health::Ok {
                let keep = c.keeps_command_through_fault(axis);
                ax.trip(keep);
                if !keep {
                    st.lifetimes.remove(id, axis);
                }
                c.set_command(axis, 0.0);
                continue;
            }
            let target = ax.target(now, c.idle_value(axis));
            planned.push(Planned {
                id,
                axis,
                component: c,
                target,
            });
        }
        planned
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use super::super::{
        Mode,
        test_stubs::{Hw, put, sim_site},
    };
    use crate::sim::{
        MicrogridSite, bounds::VecBounds, component::GatewaySettings, runtime::Health,
    };
    use crate::timeout_tracker::SetpointAxis::Active;

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

    /// An expired lifetime ramps the axis to its park value.
    #[test]
    fn expiry_ramps_to_the_park_value() {
        let (site, clock) = sim_site();
        let hw = put(
            &site,
            Arc::new(Hw {
                park: -50.0,
                ..Hw::new(1)
            }),
        );
        site.gateway()
            .set_active_power(
                1,
                site.run_generation(),
                600.0,
                Duration::from_secs(1),
                Mode::Reject,
            )
            .unwrap();
        clock.advance(DT);
        site.tick_once(site.now(), DT);
        assert_eq!(hw.last(), Some(600.0));
        clock.advance(Duration::from_secs(1));
        site.tick_once(site.now(), DT);
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
        let (site, clock) = sim_site();
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
        clock.advance(Duration::from_secs(2));
        site.tick_once(site.now(), DT);
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
}
