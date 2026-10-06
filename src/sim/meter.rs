use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use tulisp::TulispContext;

use crate::sim::{
    Category, MicrogridSite, SimulatedComponent, Telemetry,
    component::{KnobKind, KnobSnapshot, MeterDrive, ReactiveReading, ScalarReading},
    dynamic_scalar::DynamicScalar,
};

/// A meter's reactive-power (Q) source — the VAr twin of the
/// `:power-w` active-power source. Either a direct VAr value (constant,
/// lambda, or symbol) or a derivation from the meter's own live P via
/// a power factor.
#[derive(Clone)]
pub enum ReactiveSource {
    /// VArs directly: constant, lambda, or symbol.
    Var(DynamicScalar),
    /// Derive Q from this meter's live P: `P·tan(acos(pf))`,
    /// negated when leading, so lagging keeps Q on P's own sign
    /// (the passive-sign convention the UI labels by). `pf` is
    /// true cos φ in `(0, 1]`.
    PowerFactor { pf: f32, leading: bool },
}

/// The reactive source's construction-time freeze — the Q twin of
/// `constructed_power`. `None` when the meter was built with no
/// reactive source, or with a dynamic (lambda / symbol) `Var`.
// `pub`, not `pub(crate)`: `KnobSnapshot::MeterReactive` (a variant of
// the `pub` `KnobSnapshot` enum, since it appears in the `pub trait
// SimulatedComponent`'s `snapshot_knob` / `restore_knob` signatures)
// carries this type, and an enum's variant fields can't be less
// visible than the enum itself — the `pub trait` signature forces
// `pub` here regardless of what the `meter` module itself does with
// its own visibility. This crate is unpublished (it has a `lib.rs`,
// but nothing outside this workspace depends on it), so there's no
// external downstream to leak the type to; the module-privacy trick
// that keeps `DynamicScalar` (declared in a `pub(crate) mod
// dynamic_scalar`) off this crate's public API doesn't apply here,
// since `sim/mod.rs` declares `pub mod meter`.
#[derive(Clone, Copy)]
pub enum ConstructedReactive {
    Var(f32),
    PowerFactor { pf: f32, leading: bool },
}

/// A power meter sums its successors' active and reactive power, then
/// voltage-splits the totals across the three phases. If the parent
/// registered with explicit `:power-w` — a constant, a lambda, or a
/// symbol — that value is used verbatim instead, modelling a
/// headless consumer / CHP load. Lisp timers can also push a value
/// in via `(set-meter-power id W)`, which collapses the source back
/// to a constant. The reactive axis mirrors this: an explicit
/// `:reactive-power-var` or `:power-factor` source overrides the
/// aggregate-from-successors path independently of the active axis,
/// so a `:power-w` override no longer forces Q to zero (todo #537).
pub struct Meter {
    id: u64,
    name: String,
    interval: Duration,
    /// Override the aggregate-from-successors path with an explicit
    /// active-power source — either a constant or a Lisp expression
    /// re-resolved each tick. RwLock so `(set-meter-power)` can
    /// replace the slot without contending against the per-tick
    /// aggregation read.
    power_source: RwLock<Option<DynamicScalar>>,
    /// Override the aggregate-from-successors path with an explicit
    /// reactive-power source — either a direct VAr value or a
    /// power-factor derivation from this meter's own live P. RwLock
    /// for the same reason as `power_source`.
    reactive_source: RwLock<Option<ReactiveSource>>,
    stream_jitter_pct: f32,
    /// Excluded from gRPC component / connection listings, but still
    /// aggregated by parent meters via MicrogridSite::get. Used for synthetic
    /// loads / generators that present as a power flow without being
    /// a discrete addressable component.
    hidden: bool,
    /// The `:power-w` value this meter was constructed with, when it
    /// was a plain constant — `None` for a dynamic (lambda / symbol)
    /// source or no source at all. Only construction-time state:
    /// later pokes via `set_active_power_override` /
    /// `set_fixed_power` never touch this, so the microgrid-file
    /// renderer keeps writing the original construction kwarg
    /// instead of resurrecting a runtime poke as if it were config.
    /// RwLock so `clear_active_power_source` can drop it too —
    /// "clear means cleared" extends to the constructed kwarg, not
    /// just the live slot, so a save/reload agrees with a cleared
    /// meter instead of resurrecting the override.
    constructed_power: RwLock<Option<f32>>,
    /// The `:reactive-power-var` / `:power-factor` this meter was
    /// constructed with — the Q twin of `constructed_power`. See
    /// there for why this is construction-only state, and why it's
    /// an RwLock (`clear_reactive_power_source` drops it too).
    constructed_reactive: RwLock<Option<ConstructedReactive>>,
}

impl Meter {
    pub fn new(
        id: u64,
        interval: Duration,
        power_source: Option<DynamicScalar>,
        reactive_source: Option<ReactiveSource>,
        stream_jitter_pct: f32,
        hidden: bool,
    ) -> Self {
        let constructed_power = power_source
            .as_ref()
            .filter(|s| !s.is_dynamic())
            .map(|s| s.get());
        let constructed_reactive = match &reactive_source {
            Some(ReactiveSource::Var(s)) if !s.is_dynamic() => {
                Some(ConstructedReactive::Var(s.get()))
            }
            Some(ReactiveSource::Var(_)) => None,
            Some(ReactiveSource::PowerFactor { pf, leading }) => {
                Some(ConstructedReactive::PowerFactor {
                    pf: *pf,
                    leading: *leading,
                })
            }
            None => None,
        };
        Self {
            id,
            name: format!("meter-{id}"),
            interval,
            power_source: RwLock::new(power_source),
            reactive_source: RwLock::new(reactive_source),
            stream_jitter_pct,
            hidden,
            constructed_power: RwLock::new(constructed_power),
            constructed_reactive: RwLock::new(constructed_reactive),
        }
    }

    fn aggregate_active(&self, site: &MicrogridSite) -> f32 {
        if let Some(scalar) = self.power_source.read().as_ref() {
            return scalar.get();
        }
        site.sum_children(self.id, |c| c.aggregate_power_w(site))
    }

    fn aggregate_reactive(&self, site: &MicrogridSite) -> f32 {
        self.aggregate_reactive_with(site, || self.aggregate_active(site))
    }

    /// [`Self::aggregate_reactive`] with the active power supplied
    /// lazily: only a PF source derives Q from P, so the Var and
    /// child-sum arms never pay for the active walk, while `telemetry`
    /// passes the `total_p` it already read and gets a P/Q pair taken
    /// from one read.
    fn aggregate_reactive_with(&self, site: &MicrogridSite, p: impl FnOnce() -> f32) -> f32 {
        match self.reactive_source.read().as_ref() {
            Some(ReactiveSource::Var(scalar)) => scalar.get(),
            Some(ReactiveSource::PowerFactor { pf, leading }) => derive_pf_q(p(), *pf, *leading),
            None => site.sum_children(self.id, |c| c.aggregate_reactive_var(site)),
        }
    }

    /// Replace the power source with a fresh constant. Used by
    /// `(set-meter-power)` to drive consumer / load curves from a
    /// Lisp timer; collapses any prior dynamic source so subsequent
    /// refreshes are no-ops.
    pub fn set_fixed_power(&self, watts: f32) {
        *self.power_source.write() = Some(DynamicScalar::constant(watts));
    }

    /// Replace the reactive source with a fresh constant VAr value.
    /// The Q twin of `set_fixed_power`.
    pub fn set_fixed_reactive_power(&self, vars: f32) {
        *self.reactive_source.write() = Some(ReactiveSource::Var(DynamicScalar::constant(vars)));
    }

    /// Replace the reactive source with a power-factor derivation
    /// that tracks this meter's own live active power on every read.
    pub fn set_power_factor_source(&self, pf: f32, leading: bool) {
        *self.reactive_source.write() = Some(ReactiveSource::PowerFactor { pf, leading });
    }
}

/// Derive reactive power from active power and a power factor:
/// `P·tan(acos(pf))`, negated when leading. Signed P keeps a lagging
/// Q on the same sign as the power flow — an exporting meter (P < 0)
/// configured lagging yields -Q, so the UI's sign-pair rule
/// (same signs = lagging) labels it the way it was configured.
/// `pf` is true cos φ in
/// `(0, 1]`; the clamp is a guard against `pf > 1` (whose `acos` is
/// NaN). The lower bound is inert in f32 — `acos(f32::MIN_POSITIVE)`
/// equals `acos(0.0)` bit for bit — so a `pf` of 0 still yields a
/// nonsense Q. Construction validates the actual range; the clamp is
/// only a backstop.
fn derive_pf_q(p: f32, pf: f32, leading: bool) -> f32 {
    p * pf.clamp(f32::MIN_POSITIVE, 1.0).acos().tan() * if leading { -1.0 } else { 1.0 }
}

impl fmt::Display for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Meter {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Meter
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        self.interval
    }
    fn stream_jitter_pct(&self) -> f32 {
        self.stream_jitter_pct
    }
    fn refresh_inputs(&self, ctx: &mut TulispContext) {
        if let Some(scalar) = self.power_source.read().as_ref() {
            scalar.refresh(ctx);
        }
        // Only the Var shape carries a Lisp-resolvable scalar — a PF
        // source derives from live P on every read and needs no
        // refresh of its own.
        if let Some(ReactiveSource::Var(scalar)) = self.reactive_source.read().as_ref() {
            scalar.refresh(ctx);
        }
    }
    fn tick(&self, _world: &MicrogridSite, _now: DateTime<Utc>, _dt: Duration) {}

    fn telemetry(&self, site: &MicrogridSite) -> Telemetry {
        let grid = site.grid_state();
        let total_p = self.aggregate_active(site);
        let total_q = self.aggregate_reactive_with(site, || total_p);

        let pp = split_per_phase(total_p, grid.voltage_per_phase);
        let qq = split_per_phase(total_q, grid.voltage_per_phase);
        let (i1, i2, i3) = per_phase_apparent_current(pp, qq, grid.voltage_per_phase);

        Telemetry {
            id: self.id,
            category: Some(Category::Meter),
            active_power_w: Some(total_p),
            reactive_power_var: Some(total_q),
            per_phase_active_w: Some(pp),
            per_phase_reactive_var: Some(qq),
            per_phase_voltage_v: Some(grid.voltage_per_phase),
            per_phase_current_a: Some((i1, i2, i3)),
            frequency_hz: Some(grid.frequency_hz),
            component_state: Some("ready"),
            ..Default::default()
        }
    }

    fn active_power_w(&self, site: &MicrogridSite) -> Option<f32> {
        Some(self.aggregate_active(site))
    }

    fn aggregate_power_w(&self, site: &MicrogridSite) -> f32 {
        self.aggregate_active(site)
    }

    fn aggregate_reactive_var(&self, site: &MicrogridSite) -> f32 {
        self.aggregate_reactive(site)
    }

    fn meter_drive(&self) -> Option<&dyn MeterDrive> {
        Some(self)
    }

    fn is_hidden(&self) -> bool {
        self.hidden
    }

    fn snapshot_knob(&self, kind: KnobKind) -> Option<KnobSnapshot> {
        match kind {
            KnobKind::MeterPower => {
                // Same paired-guard order `clear_active_power_source`
                // writes under (constructed then source), so a
                // concurrent poke can't be observed torn between the
                // two fields of the snapshot.
                let constructed = self.constructed_power.read();
                let source = self.power_source.read();
                Some(KnobSnapshot::MeterActive {
                    source: source.clone(),
                    constructed: *constructed,
                })
            }
            KnobKind::MeterReactive => {
                let constructed = self.constructed_reactive.read();
                let source = self.reactive_source.read();
                Some(KnobSnapshot::MeterReactive {
                    source: source.clone(),
                    constructed: *constructed,
                })
            }
            KnobKind::Sunlight | KnobKind::BoilerDemand | KnobKind::Ev => None,
        }
    }

    fn restore_knob(&self, snap: KnobSnapshot) -> bool {
        match snap {
            KnobSnapshot::MeterActive {
                source,
                constructed,
            } => {
                // Write both under the guards held together, matching
                // `clear_active_power_source`'s ABBA-safe pairing.
                let mut c = self.constructed_power.write();
                let mut s = self.power_source.write();
                *c = constructed;
                *s = source;
                true
            }
            KnobSnapshot::MeterReactive {
                source,
                constructed,
            } => {
                let mut c = self.constructed_reactive.write();
                let mut s = self.reactive_source.write();
                *c = constructed;
                *s = source;
                true
            }
            _ => false,
        }
    }

    fn make_fn(&self) -> &'static str {
        "%make-meter"
    }

    fn has_unrenderable_source(&self) -> bool {
        // `constructed_power` / `constructed_reactive` hold the
        // constant `:power-w` / `:reactive-power-var` / `:power-factor`
        // this meter was BUILT with, so an unset one with a live
        // source means the value came from somewhere the renderer
        // cannot write: a lambda / symbol binding, or a runtime poke
        // (`set-meter-power`, `set_fixed_reactive_power`,
        // `set_power_factor_source`) over a meter constructed without
        // that kwarg.
        (self.constructed_power.read().is_none() && self.power_source.read().is_some())
            || (self.constructed_reactive.read().is_none() && self.reactive_source.read().is_some())
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        let mut kw = Vec::new();
        if self.interval != Duration::from_millis(1000) {
            kw.push((
                ":interval-s",
                crate::lisp::lisp_float(self.interval.as_secs_f64()),
            ));
        }
        if let Some(p) = self.constructed_power.read().filter(|p| p.is_finite()) {
            kw.push((":power-w", crate::lisp::lisp_float32(p)));
        }
        // Hoisted out of the match scrutinee: a place-expression match
        // on `*self.constructed_reactive.read()` would hold the read
        // guard for the whole match (Rust's temporary-lifetime
        // extension covers the arms too), which is a future-deadlock
        // hazard the moment an arm ever needs to touch the lock again.
        let ctor = *self.constructed_reactive.read();
        match ctor {
            Some(ConstructedReactive::Var(v)) if v.is_finite() => {
                kw.push((":reactive-power-var", crate::lisp::lisp_float32(v)));
            }
            Some(ConstructedReactive::PowerFactor { pf, leading }) => {
                kw.push((":power-factor", crate::lisp::lisp_float32(pf)));
                if leading {
                    kw.push((":leading", "t".to_string()));
                }
            }
            _ => {}
        }
        if self.hidden {
            kw.push((":hidden", "t".to_string()));
        }
        if self.stream_jitter_pct != 0.0 {
            kw.push((
                ":stream-jitter-pct",
                crate::lisp::lisp_float32(self.stream_jitter_pct),
            ));
        }
        kw
    }
}

impl MeterDrive for Meter {
    fn set_active_power_override(&self, p: f32) {
        self.set_fixed_power(p);
    }

    fn set_active_power_source(&self, scalar: DynamicScalar) {
        *self.power_source.write() = Some(scalar);
    }

    fn clear_active_power_source(&self) {
        // Hold both write guards together, acquired in the same order
        // `has_unrenderable_source` reads them (constructed before
        // source) — two separate statement-scoped acquisitions here
        // would open a window where a concurrent save could observe
        // `power_source` already cleared but `constructed_power` not
        // yet, tearing "cleared means cleared"; matching the read
        // order also keeps this ABBA-safe against that read pair.
        let mut constructed = self.constructed_power.write();
        let mut source = self.power_source.write();
        *constructed = None;
        *source = None;
    }

    fn set_reactive_power_override(&self, vars: f32) {
        self.set_fixed_reactive_power(vars);
    }

    fn set_reactive_power_source(&self, scalar: DynamicScalar) {
        *self.reactive_source.write() = Some(ReactiveSource::Var(scalar));
    }

    fn set_power_factor(&self, pf: f32, leading: bool) {
        self.set_power_factor_source(pf, leading);
    }

    fn clear_reactive_power_source(&self) {
        // As in `clear_active_power_source`: both guards held
        // together, acquired constructed-then-source to match
        // `has_unrenderable_source`'s read order.
        let mut constructed = self.constructed_reactive.write();
        let mut source = self.reactive_source.write();
        *constructed = None;
        *source = None;
    }

    fn meter_power_reading(&self) -> Option<ScalarReading> {
        self.power_source.read().as_ref().map(|s| ScalarReading {
            value: s.get(),
            expr: s.source_text(),
        })
    }

    fn meter_reactive_reading(&self) -> Option<ReactiveReading> {
        self.reactive_source.read().as_ref().map(|r| match r {
            ReactiveSource::Var(s) => ReactiveReading::Var(ScalarReading {
                value: s.get(),
                expr: s.source_text(),
            }),
            ReactiveSource::PowerFactor { pf, leading } => ReactiveReading::PowerFactor {
                pf: *pf,
                leading: *leading,
            },
        })
    }
}

/// Voltage-weighted per-phase split of a single total. Mirrors a real
/// 3-phase meter's reading on a balanced load: phase i gets
/// `total × V_i / (V1 + V2 + V3)`. Returns zeros if all voltages are
/// zero (avoids NaN).
pub fn split_per_phase(total_w: f32, voltage: (f32, f32, f32)) -> (f32, f32, f32) {
    let sum = voltage.0 + voltage.1 + voltage.2;
    if sum == 0.0 {
        return (0.0, 0.0, 0.0);
    }
    (
        total_w * voltage.0 / sum,
        total_w * voltage.1 / sum,
        total_w * voltage.2 / sum,
    )
}

/// Per-phase apparent current = `√(P² + Q²) / V` in each phase.
pub fn per_phase_apparent_current(
    p: (f32, f32, f32),
    q: (f32, f32, f32),
    v: (f32, f32, f32),
) -> (f32, f32, f32) {
    fn one(p: f32, q: f32, v: f32) -> f32 {
        if v == 0.0 {
            0.0
        } else {
            (p * p + q * q).sqrt() / v
        }
    }
    (one(p.0, q.0, v.0), one(p.1, q.1, v.1), one(p.2, q.2, v.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimulatedComponent;

    /// Stub component that returns a fixed P / Q for testing how
    /// meters aggregate their children. Doesn't model any physics.
    struct FixedFlow {
        id: u64,
        p: f32,
        q: f32,
    }
    impl std::fmt::Display for FixedFlow {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "fixed-{}", self.id)
        }
    }
    impl SimulatedComponent for FixedFlow {
        fn id(&self) -> u64 {
            self.id
        }
        fn category(&self) -> Category {
            Category::Inverter
        }
        fn name(&self) -> &str {
            "fixed"
        }
        fn stream_interval(&self) -> Duration {
            Duration::from_secs(1)
        }
        fn tick(&self, _: &MicrogridSite, _: chrono::DateTime<chrono::Utc>, _: Duration) {}
        fn telemetry(&self, _: &MicrogridSite) -> Telemetry {
            Telemetry::default()
        }
        fn aggregate_power_w(&self, _: &MicrogridSite) -> f32 {
            self.p
        }
        fn aggregate_reactive_var(&self, _: &MicrogridSite) -> f32 {
            self.q
        }
        fn make_fn(&self) -> &'static str {
            "%make-test-stub"
        }
        fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
            Vec::new()
        }
    }

    /// A duplicated (parent, child) edge — `connect` rejects only
    /// cycles, so evaluating `(connect 2 100)` twice stores two
    /// identical edges — must not change the meter's sum: each copy
    /// contributes value/2, landing on the child's actual power.
    #[test]
    fn duplicate_edge_does_not_double_count() {
        let w = MicrogridSite::new();
        let inverter = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 10_000.0,
            q: 0.0,
        });
        w.register_arc(inverter);
        let meter = Meter::new(2, Duration::from_secs(1), None, None, 0.0, false);
        w.register(meter);
        assert!(w.connect(2, 100));
        assert!(w.connect(2, 100));
        let m = w.get(2).unwrap();
        assert!((m.aggregate_power_w(&w) - 10_000.0).abs() < 1e-3);
    }

    /// A transformer and a breaker between two meters carry the power
    /// below them: the top meter reads the inverter's flow (P and Q)
    /// through both, not zero for the branch.
    #[test]
    fn markers_pass_their_childrens_flow_through() {
        let w = MicrogridSite::new();
        w.register_arc(std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 10_000.0,
            q: 2_000.0,
        }));
        w.register(Meter::new(
            3,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));
        w.register(crate::sim::Marker::new(20, Category::Breaker, 0.0));
        w.register(crate::sim::Marker::new(21, Category::PowerTransformer, 0.0));
        w.register(Meter::new(
            2,
            Duration::from_secs(1),
            None,
            None,
            0.0,
            false,
        ));
        assert!(w.connect(2, 21));
        assert!(w.connect(21, 20));
        assert!(w.connect(20, 3));
        assert!(w.connect(3, 100));
        let top = w.get(2).unwrap();
        assert!((top.aggregate_power_w(&w) - 10_000.0).abs() < 1e-3);
        assert!((top.aggregate_reactive_var(&w) - 2_000.0).abs() < 1e-3);
    }

    /// 1 inverter, 2 parallel meters, 1 top meter:
    ///
    ///                  top (id 2)
    ///                  ╱     ╲
    ///         meter_a (10)  meter_b (11)
    ///                  ╲     ╱
    ///                inverter (100)
    ///
    /// inverter publishes 10 kW. Each parallel meter should see half
    /// (5 kW); the top meter aggregates both halves and lands on the
    /// inverter's actual flow (10 kW), not 20 kW.
    #[test]
    fn parallel_paths_share_one_inverter() {
        let w = MicrogridSite::new();

        // Register an inverter that publishes 10 kW active, 0 VAR.
        let inverter = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 10_000.0,
            q: 0.0,
        });
        w.register_arc(inverter);

        // Two parallel meters that each list the inverter as their
        // only successor — connect both edges so parent_count(100) = 2.
        let meter_a = Meter::new(10, Duration::from_secs(1), None, None, 0.0, false);
        let meter_b = Meter::new(11, Duration::from_secs(1), None, None, 0.0, false);
        w.register(meter_a);
        w.register(meter_b);
        w.connect(10, 100);
        w.connect(11, 100);

        // Top meter aggregates both parallel meters.
        let top = Meter::new(2, Duration::from_secs(1), None, None, 0.0, false);
        w.register(top);
        w.connect(2, 10);
        w.connect(2, 11);

        let m_a = w.get(10).unwrap();
        let m_b = w.get(11).unwrap();
        let m_top = w.get(2).unwrap();

        assert!((m_a.aggregate_power_w(&w) - 5_000.0).abs() < 1e-3);
        assert!((m_b.aggregate_power_w(&w) - 5_000.0).abs() < 1e-3);
        assert!((m_top.aggregate_power_w(&w) - 10_000.0).abs() < 1e-3);
    }

    /// `(disconnect …)` must take aggregation effect even for
    /// children that were wired at make-time. Pre-fix the meter
    /// cached its full successor list and ignored disconnects.
    #[test]
    fn disconnect_after_make_drops_child() {
        let w = MicrogridSite::new();
        let inverter = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 2_000.0,
            q: 0.0,
        });
        w.register_arc(inverter);
        // Visible meter with the inverter as a make-time child —
        // connections is the single source of truth so the
        // connect/disconnect dance flows through it directly.
        let m = Meter::new(2, Duration::from_secs(1), None, None, 0.0, false);
        w.register(m);
        w.connect(2, 100);
        let m = w.get(2).unwrap();
        assert!((m.aggregate_power_w(&w) - 2_000.0).abs() < 1e-3);
        assert!(w.disconnect(2, 100));
        assert_eq!(m.aggregate_power_w(&w), 0.0);
    }

    /// Children connected via post-make `(connect …)` (eg.
    /// the UI's copy / paste flow) must aggregate too — the meter's
    /// internal successor list isn't the only source of truth.
    #[test]
    fn connect_after_make_aggregates() {
        let w = MicrogridSite::new();

        // Inverter publishes 2 kW; meter starts with no successors.
        let inverter = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 2_000.0,
            q: 0.0,
        });
        w.register_arc(inverter);
        let m = Meter::new(2, Duration::from_secs(1), None, None, 0.0, false);
        w.register(m);

        // Pre-connect: nothing under the meter.
        let m = w.get(2).unwrap();
        assert_eq!(m.aggregate_power_w(&w), 0.0);

        // Post-connect: aggregation picks up the new edge.
        w.connect(2, 100);
        assert!((m.aggregate_power_w(&w) - 2_000.0).abs() < 1e-3);
    }

    /// A constant `:power-w` DynamicScalar bypasses children-aggregation
    /// and reads through directly. set_fixed_power replaces the slot
    /// with a fresh constant.
    #[test]
    fn constant_power_source_reads_through() {
        let w = MicrogridSite::new();
        let m = Meter::new(
            2,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(2750.0)),
            None,
            0.0,
            false,
        );
        w.register(m);
        let m = w.get(2).unwrap();
        assert!((m.aggregate_power_w(&w) - 2750.0).abs() < 1e-3);

        // set-meter-power collapses the source to a fresh constant
        // even if it had been dynamic.
        m.meter_drive().unwrap().set_active_power_override(4100.0);
        assert!((m.aggregate_power_w(&w) - 4100.0).abs() < 1e-3);
    }

    /// A lambda-bound `:power-w` resolves through `refresh_inputs` and
    /// the new value lands in subsequent `aggregate_power_w` reads.
    #[test]
    fn lambda_power_source_refreshes_each_tick() {
        let mut ctx = tulisp::TulispContext::new();
        let lambda = ctx.eval_string("(lambda () 1234.5)").unwrap();
        let scalar = DynamicScalar::from_lisp(&lambda, 0.0).expect("lambda → dynamic");
        assert!(scalar.is_dynamic());

        let w = MicrogridSite::new();
        let m = Meter::new(2, Duration::from_secs(1), Some(scalar), None, 0.0, false);
        w.register(m);
        let m = w.get(2).unwrap();

        // Pre-refresh: the fallback (0.0) is what the cache holds.
        assert_eq!(m.aggregate_power_w(&w), 0.0);

        // Refresh once — the lambda resolves to 1234.5.
        m.refresh_inputs(&mut ctx);
        assert!((m.aggregate_power_w(&w) - 1234.5).abs() < 1e-3);
    }

    /// A hidden child aggregates into its visible parent just like a
    /// non-hidden one. With the unified graph, hidden edges land in
    /// `connections` like any other edge — the visibility filter only
    /// kicks in at the `connections()` / `hidden_connections()`
    /// boundary that drives gRPC and the UI.
    #[test]
    fn hidden_child_aggregates_into_visible_parent() {
        let w = MicrogridSite::new();
        // Hidden meter consumer with a constant 1500 W draw.
        let hidden_meter = Meter::new(
            9000,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(1500.0)),
            None,
            0.0,
            true,
        );
        w.register(hidden_meter);

        let parent = Meter::new(2, Duration::from_secs(1), None, None, 0.0, false);
        w.register(parent);
        w.connect(2, 9000);

        let parent = w.get(2).unwrap();
        assert!((parent.aggregate_power_w(&w) - 1500.0).abs() < 1e-3);
        // The edge is in the graph but not surfaced through the
        // gRPC-facing `connections()` view.
        assert!(w.connections().is_empty());
        assert_eq!(w.hidden_connections(), vec![(2, 9000)]);
    }

    /// `constructed_power` captures only the constant `:power-w` a
    /// meter was built with — later pokes via
    /// `set_active_power_override` must not leak into it. A meter
    /// with no power source at all renders no `:power-w` kwarg.
    #[test]
    fn meter_records_constructed_power_constant_only() {
        // Constant power → kwarg present; poked value must NOT change it.
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            DynamicScalar::from_lisp(&1875.0f64.into(), 0.0),
            None,
            0.0,
            false,
        );
        let kw = |m: &Meter| {
            m.constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kw(&m).contains(":power-w 1875.0"));
        m.set_active_power_override(9999.0);
        assert!(
            kw(&m).contains(":power-w 1875.0"),
            "pokes are not construction"
        );
        // No power source → no :power-w kwarg; hidden renders.
        let h = Meter::new(2, Duration::from_secs(1), None, None, 0.0, true);
        assert!(!kw(&h).contains(":power-w"));
        assert!(kw(&h).contains(":hidden t"));
    }

    /// A `:power-w` override no longer zeroes Q (todo #537 fix): a
    /// meter with BOTH a `:power-w` override and a `Var` reactive
    /// source reports the Var value, not 0. A meter with a `:power-w`
    /// override but NO reactive source still sums its children's Q —
    /// the two axes are independent.
    #[test]
    fn reactive_var_source_and_children_sum() {
        let w = MicrogridSite::new();

        // Own reactive source wins outright — the P override doesn't
        // touch it.
        let m_with_reactive = Meter::new(
            1,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(5_000.0)),
            Some(ReactiveSource::Var(DynamicScalar::constant(750.0))),
            0.0,
            false,
        );
        w.register(m_with_reactive);
        let m_with_reactive = w.get(1).unwrap();
        assert!((m_with_reactive.aggregate_reactive_var(&w) - 750.0).abs() < 1e-3);

        // No reactive source → sums children's Q, even though this
        // meter has a P override (the pre-fix behaviour returned 0
        // here).
        let child = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 0.0,
            q: 1_200.0,
        });
        w.register_arc(child);
        let m_no_reactive = Meter::new(
            2,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(5_000.0)),
            None,
            0.0,
            false,
        );
        w.register(m_no_reactive);
        w.connect(2, 100);
        let m_no_reactive = w.get(2).unwrap();
        assert!((m_no_reactive.aggregate_reactive_var(&w) - 1_200.0).abs() < 1e-3);
    }

    /// A `PowerFactor` reactive source derives Q from this meter's
    /// OWN live P on every read: `pf 0.8` lagging on `:power-w 8000` →
    /// `Q ≈ 8000·tan(acos(0.8)) = 6000`; leading negates it; and
    /// moving the P override moves Q on the next read.
    #[test]
    fn power_factor_source_tracks_live_p() {
        let w = MicrogridSite::new();
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(8_000.0)),
            Some(ReactiveSource::PowerFactor {
                pf: 0.8,
                leading: false,
            }),
            0.0,
            false,
        );
        w.register(m);
        let m = w.get(1).unwrap();
        assert!((m.aggregate_reactive_var(&w) - 6_000.0).abs() < 1.0);

        // Leading flips the sign.
        m.meter_drive().unwrap().set_power_factor(0.8, true);
        assert!((m.aggregate_reactive_var(&w) - -6_000.0).abs() < 1.0);

        // Moving the P override moves Q on the next read — the PF
        // source has no cached value of its own.
        m.meter_drive().unwrap().set_active_power_override(4_000.0);
        assert!((m.aggregate_reactive_var(&w) - -3_000.0).abs() < 1.0);

        // Signed P: an exporting meter keeps a lagging Q on the
        // export's own sign, so the UI's sign-pair rule (same signs
        // = lagging) labels it as configured.
        m.meter_drive().unwrap().set_power_factor(0.8, false);
        m.meter_drive().unwrap().set_active_power_override(-8_000.0);
        assert!((m.aggregate_reactive_var(&w) - -6_000.0).abs() < 1.0);
        m.meter_drive().unwrap().set_power_factor(0.8, true);
        assert!((m.aggregate_reactive_var(&w) - 6_000.0).abs() < 1.0);
    }

    /// `constructed_reactive` freezes the SAME way `constructed_power`
    /// does: a `:reactive-power-var` kwarg survives into `constructor_kwargs`,
    /// and a later runtime poke (`set_fixed_reactive_power`) doesn't
    /// leak into it. A meter built with no reactive source, then
    /// given a runtime PF source, becomes unrenderable — the same
    /// rule that already applies to a runtime `:power-w` poke.
    #[test]
    fn constructed_reactive_freezes_for_rendering() {
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            None,
            Some(ReactiveSource::Var(DynamicScalar::constant(500.0))),
            0.0,
            false,
        );
        let kw = |m: &Meter| {
            m.constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kw(&m).contains(":reactive-power-var 500.0"));
        m.set_fixed_reactive_power(999.0);
        assert!(
            kw(&m).contains(":reactive-power-var 500.0"),
            "pokes are not construction"
        );

        // Built plain, then a runtime PF source lands — unrenderable.
        let h = Meter::new(2, Duration::from_secs(1), None, None, 0.0, false);
        assert!(!h.has_unrenderable_source());
        h.set_power_factor_source(0.8, false);
        assert!(h.has_unrenderable_source());
    }

    /// `clear_active_power_source` empties the active override and the
    /// meter goes back to measuring its children's aggregate — the
    /// one-way trip fixed by this change.
    #[test]
    fn clear_active_power_source_restores_children_sum() {
        let w = MicrogridSite::new();
        let child = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 3_000.0,
            q: 0.0,
        });
        w.register_arc(child);
        let m = Meter::new(
            2,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(9_000.0)),
            None,
            0.0,
            false,
        );
        w.register(m);
        w.connect(2, 100);
        let m = w.get(2).unwrap();

        // Overridden: reads the constant, not the child sum.
        assert!((m.aggregate_power_w(&w) - 9_000.0).abs() < 1e-3);

        m.meter_drive().unwrap().clear_active_power_source();
        assert!((m.aggregate_power_w(&w) - 3_000.0).abs() < 1e-3);
        assert!(m.meter_drive().unwrap().meter_power_reading().is_none());
    }

    /// `clear_reactive_power_source` clears a `Var` reactive override
    /// back to summing children's Q — the Q twin of the active-axis
    /// test above.
    #[test]
    fn clear_reactive_power_source_restores_children_sum_for_var() {
        let w = MicrogridSite::new();
        let child = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 0.0,
            q: 1_100.0,
        });
        w.register_arc(child);
        let m = Meter::new(
            2,
            Duration::from_secs(1),
            None,
            Some(ReactiveSource::Var(DynamicScalar::constant(750.0))),
            0.0,
            false,
        );
        w.register(m);
        w.connect(2, 100);
        let m = w.get(2).unwrap();

        assert!((m.aggregate_reactive_var(&w) - 750.0).abs() < 1e-3);
        m.meter_drive().unwrap().clear_reactive_power_source();
        assert!((m.aggregate_reactive_var(&w) - 1_100.0).abs() < 1e-3);
        assert!(m.meter_drive().unwrap().meter_reactive_reading().is_none());
    }

    /// Same as above but for a `PowerFactor` reactive source — the
    /// clear must drop the whole `ReactiveSource` enum, not just a
    /// `Var` variant, so a PF-derived meter also goes back to
    /// measuring.
    #[test]
    fn clear_reactive_power_source_restores_children_sum_for_power_factor() {
        let w = MicrogridSite::new();
        let child = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 0.0,
            q: 2_200.0,
        });
        w.register_arc(child);
        let m = Meter::new(
            2,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(8_000.0)),
            Some(ReactiveSource::PowerFactor {
                pf: 0.8,
                leading: false,
            }),
            0.0,
            false,
        );
        w.register(m);
        w.connect(2, 100);
        let m = w.get(2).unwrap();

        assert!((m.aggregate_reactive_var(&w) - 6_000.0).abs() < 1.0);
        m.meter_drive().unwrap().clear_reactive_power_source();
        assert!((m.aggregate_reactive_var(&w) - 2_200.0).abs() < 1e-3);
        assert!(m.meter_drive().unwrap().meter_reactive_reading().is_none());
    }

    /// A meter CONSTRUCTED with `:power-w` then cleared emits no
    /// `:power-w` kwarg on the next render, and `has_unrenderable_source`
    /// stays false — "clear means cleared" extends to the construction
    /// kwarg, so a save/reload agrees with the live (measuring) state
    /// instead of resurrecting the override.
    #[test]
    fn clear_active_power_source_drops_constructed_kwarg() {
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(1875.0)),
            None,
            0.0,
            false,
        );
        let kw = |m: &Meter| {
            m.constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kw(&m).contains(":power-w 1875.0"));
        m.clear_active_power_source();
        assert!(!kw(&m).contains(":power-w"), "{}", kw(&m));
        assert!(!m.has_unrenderable_source());
    }

    /// Same round-trip for the reactive axis, constructed with
    /// `:reactive-power-var`.
    #[test]
    fn clear_reactive_power_source_drops_constructed_kwarg() {
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            None,
            Some(ReactiveSource::Var(DynamicScalar::constant(500.0))),
            0.0,
            false,
        );
        let kw = |m: &Meter| {
            m.constructor_kwargs()
                .iter()
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert!(kw(&m).contains(":reactive-power-var 500.0"));
        m.clear_reactive_power_source();
        assert!(!kw(&m).contains(":reactive-power-var"), "{}", kw(&m));
        assert!(!m.has_unrenderable_source());
    }

    /// Clearing a never-overridden meter is a no-op: it goes on
    /// measuring its children.
    #[test]
    fn clear_on_never_overridden_meter_is_a_noop() {
        let w = MicrogridSite::new();
        let child = std::sync::Arc::new(FixedFlow {
            id: 100,
            p: 500.0,
            q: 50.0,
        });
        w.register_arc(child);
        let m = Meter::new(2, Duration::from_secs(1), None, None, 0.0, false);
        w.register(m);
        w.connect(2, 100);
        let m = w.get(2).unwrap();

        m.meter_drive().unwrap().clear_active_power_source();
        m.meter_drive().unwrap().clear_reactive_power_source();
        assert!((m.aggregate_power_w(&w) - 500.0).abs() < 1e-3);
        assert!((m.aggregate_reactive_var(&w) - 50.0).abs() < 1e-3);
    }

    /// A meter constructed with `:power-w`, then driven by a scenario
    /// installing a dynamic source: `restore_knob` must bring back
    /// BOTH the live constant source AND the `:power-w` constructor
    /// kwarg — restore is not `clear_active_power_source`, which
    /// would drop the kwarg for good.
    #[test]
    fn snapshot_restore_round_trip_meter_active_constructed_then_driven() {
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(1875.0)),
            None,
            0.0,
            false,
        );
        let snap = m.snapshot_knob(KnobKind::MeterPower).unwrap();

        // A scenario displaces it with a dynamic source.
        let mut ctx = tulisp::TulispContext::new();
        let lambda = ctx.eval_string("(lambda () 42.0)").unwrap();
        m.set_active_power_source(DynamicScalar::from_lisp(&lambda, 0.0).unwrap());
        assert!(m.meter_power_reading().unwrap().expr.is_some());

        assert!(m.restore_knob(snap));
        assert_eq!(m.meter_power_reading().unwrap().value, 1875.0);
        assert!(m.meter_power_reading().unwrap().expr.is_none());
        assert!(!m.has_unrenderable_source());
        let kw = m
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(kw.contains(":power-w 1875.0"), "{kw}");
    }

    /// A meter with NO reactive override snapshots as empty; after a
    /// scenario fakes one in, restore puts it back to measuring —
    /// and, crucially, never touches the unrelated active axis's own
    /// constructed `:power-w` kwarg.
    #[test]
    fn snapshot_restore_round_trip_meter_reactive_empty_baseline() {
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            Some(DynamicScalar::constant(1875.0)),
            None,
            0.0,
            false,
        );
        let snap = m.snapshot_knob(KnobKind::MeterReactive).unwrap();
        assert!(m.meter_reactive_reading().is_none());

        m.set_fixed_reactive_power(500.0);
        assert!(m.meter_reactive_reading().is_some());

        assert!(m.restore_knob(snap));
        assert!(
            m.meter_reactive_reading().is_none(),
            "reactive axis must go back to measuring"
        );
        let kw = m
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            kw.contains(":power-w 1875.0"),
            "restoring the reactive axis must not disturb the active axis's kwarg: {kw}"
        );
    }

    /// Baseline `Var`, scenario overrides to `PowerFactor`, restore
    /// brings back `Var` — the whole `ReactiveSource` enum round-trips,
    /// not just the numeric value.
    #[test]
    fn snapshot_restore_round_trip_meter_reactive_pf_to_var() {
        let m = Meter::new(
            1,
            Duration::from_secs(1),
            None,
            Some(ReactiveSource::Var(DynamicScalar::constant(500.0))),
            0.0,
            false,
        );
        let snap = m.snapshot_knob(KnobKind::MeterReactive).unwrap();

        m.set_power_factor(0.8, true);
        match m.meter_reactive_reading().unwrap() {
            ReactiveReading::PowerFactor { .. } => {}
            ReactiveReading::Var(_) => panic!("expected PowerFactor after the scenario override"),
        }

        assert!(m.restore_knob(snap));
        match m.meter_reactive_reading().unwrap() {
            ReactiveReading::Var(r) => assert_eq!(r.value, 500.0),
            ReactiveReading::PowerFactor { .. } => panic!("expected Var after restore"),
        }
    }
}
