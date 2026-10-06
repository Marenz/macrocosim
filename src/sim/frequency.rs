//! Enterprise-wide grid frequency model. One Ornstein-Uhlenbeck
//! process per process, shared across every microgrid in the
//! registry — microgrids tied to the same AC grid all see the
//! same frequency by physics, so it lives outside per-MicrogridSite
//! state — the registry-shared driver writes once per step and
//! every site reads from the same slot via `grid_state()`.
//!
//! The driver task spawned by `spawn_driver` steps the state
//! forward every `STEP_MS` ms with the discrete Euler-Maruyama
//! update of the OU SDE:
//!
//!   dF = -k * (F - F_nominal) * dt + σ * sqrt(dt) * N(0, 1)
//!
//! `k` (mean reversion rate, 1/s) sets the correlation time
//! (~1/k seconds); `σ` (Hz/sqrt(s)) sets the noise floor — the
//! equilibrium standard deviation is σ / sqrt(2k). Defaults:
//! k = 0.05, σ = 0.015 → ~47 mHz std dev around 50 Hz with a
//! ~20-second correlation window. Same order of magnitude real
//! grid operators see at the dispatch dashboard.
//!
//! Scenarios that need a specific frequency event (UFLS dip,
//! generator-trip overshoot + recovery, etc.) call
//! `(override-frequency-model …)` to swap in different OU parameters
//! (e.g. pull toward a lower nominal); the driver keeps integrating
//! with those until `(clear-frequency-override)` restores the base
//! model.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use rand::SeedableRng;
use rand::rngs::SmallRng;

pub const NOMINAL_HZ: f32 = 50.0;
pub const DEFAULT_MEAN_REV_RATE: f32 = 0.05;
pub const DEFAULT_SIGMA: f32 = 0.015;
/// Driver step cadence. 200 ms is fast enough that components
/// reading `frequency_hz` once per `physics_tick_ms` (100 ms) see
/// a fresh value each tick, but slow enough that the OU integration
/// doesn't dominate the runtime cost.
const STEP_MS: u64 = 200;

/// The three knobs the OU step reads on each tick. Wrapped in its
/// own struct so the base + override slots share a shape: a
/// scenario can `(override-frequency-model :nominal-hz 49.5)` to pull
/// frequency toward 49.5 with the same dynamics as the base, then
/// later layer on `(override-frequency-model :sigma 0.05)` without
/// touching the override nominal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrequencyModel {
    pub nominal_hz: f32,
    pub mean_rev_rate: f32,
    pub sigma: f32,
}

impl FrequencyModel {
    pub const DEFAULT: Self = Self {
        nominal_hz: NOMINAL_HZ,
        mean_rev_rate: DEFAULT_MEAN_REV_RATE,
        sigma: DEFAULT_SIGMA,
    };
}

#[derive(Clone, Debug)]
pub struct FrequencyState {
    /// The shape the OU driver follows by default. Configurable from
    /// lisp via `(set-frequency-model …)`; sticks across scenarios.
    pub base: FrequencyModel,
    /// Scenario-driven override. When `Some`, the driver uses these
    /// params in place of the base for each step (drift toward the
    /// override's nominal, noise at the override's sigma, reversion
    /// at the override's rate). `(clear-frequency-override)` drops it.
    pub override_model: Option<FrequencyModel>,
    pub current_hz: f32,
}

impl FrequencyState {
    pub fn new() -> Self {
        Self {
            base: FrequencyModel::DEFAULT,
            override_model: None,
            current_hz: FrequencyModel::DEFAULT.nominal_hz,
        }
    }

    /// Active set of params: override if set, else base.
    pub fn active_model(&self) -> FrequencyModel {
        self.override_model.unwrap_or(self.base)
    }

    /// One Euler-Maruyama step of the OU process driven by the
    /// active model. Override is just a different set of params —
    /// the driver keeps integrating either way.
    pub fn step(&mut self, dt: f32, rng: &mut SmallRng) {
        let m = self.active_model();
        // Self-heal a non-finite state: `+=` would keep it NaN/Inf
        // forever, and the slot is shared by every microgrid. The
        // lisp setter rejects non-finite inputs, so this is a
        // second line of defense against any other corruption.
        if !self.current_hz.is_finite() {
            self.current_hz = m.nominal_hz;
        }
        let drift = -m.mean_rev_rate * (self.current_hz - m.nominal_hz) * dt;
        let noise = m.sigma * dt.sqrt() * normal_sample(rng);
        self.current_hz += drift + noise;
    }

    /// Component-facing read.
    pub fn read_hz(&self) -> f32 {
        self.current_hz
    }
}

impl Default for FrequencyState {
    fn default() -> Self {
        Self::new()
    }
}

/// Box-Muller. Cheap enough at 5 Hz to skip the extra-half-sample
/// caching trick.
fn normal_sample(rng: &mut SmallRng) -> f32 {
    use rand::Rng;
    // gen_range with a positive lower bound avoids ln(0) → -inf.
    let u1: f32 = rng.gen_range(1e-10_f32..1.0);
    let u2: f32 = rng.r#gen();
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
}

pub type SharedFrequency = Arc<RwLock<FrequencyState>>;

pub fn new_shared() -> SharedFrequency {
    Arc::new(RwLock::new(FrequencyState::new()))
}

/// Start the driver loop. Spawns one tokio task that ticks at
/// `STEP_MS` and updates the shared state. Idempotent at the
/// caller's discretion — calling twice would spawn two drivers,
/// which is a bug (Config::new is the sole call site).
pub fn spawn_driver(state: SharedFrequency) {
    let dt_s = (STEP_MS as f32) / 1000.0;
    tokio::spawn(async move {
        let mut rng = SmallRng::from_entropy();
        let mut interval = tokio::time::interval(Duration::from_millis(STEP_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            state.write().step(dt_s, &mut rng);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OU mean reversion: starting far from nominal, the state
    /// asymptotically pulls back toward nominal. We pick σ = 0 so
    /// the test is deterministic (pure exponential decay, no
    /// noise). After 5 correlation times (1/k = 20 s → 100 s) the
    /// initial 500 mHz offset has decayed by e^-5 ≈ 0.007 → ~3 mHz
    /// remaining, comfortably under 10 mHz.
    #[test]
    fn ou_drifts_back_to_nominal() {
        let mut s = FrequencyState::new();
        s.base.sigma = 0.0;
        s.current_hz = 49.5; // 500 mHz low
        let mut rng = SmallRng::seed_from_u64(0);
        for _ in 0..500 {
            s.step(0.2, &mut rng);
        }
        let gap = (s.current_hz - NOMINAL_HZ).abs();
        assert!(gap < 0.01, "expected within 10 mHz after 100 s, got {gap}");
    }

    /// Override switches the dynamics, not the value: drift pulls
    /// toward the override's nominal at the override's rate. With
    /// σ = 0 the test is deterministic.
    #[test]
    fn override_pulls_toward_override_nominal() {
        let mut s = FrequencyState::new();
        s.current_hz = 50.0;
        s.override_model = Some(FrequencyModel {
            nominal_hz: 49.0,
            mean_rev_rate: 0.05,
            sigma: 0.0,
        });
        let mut rng = SmallRng::seed_from_u64(0);
        for _ in 0..500 {
            s.step(0.2, &mut rng);
        }
        let gap = (s.current_hz - 49.0).abs();
        assert!(
            gap < 0.01,
            "expected to drift to 49.0 ±10 mHz, got {}",
            s.current_hz
        );
    }

    /// Clearing the override hands the dynamics back to the base.
    #[test]
    fn clearing_override_returns_to_base_dynamics() {
        let mut s = FrequencyState::new();
        s.base.sigma = 0.0;
        s.override_model = Some(FrequencyModel {
            nominal_hz: 49.0,
            mean_rev_rate: 1.0,
            sigma: 0.0,
        });
        s.current_hz = 49.0; // pinned to override
        s.override_model = None; // release
        let mut rng = SmallRng::seed_from_u64(0);
        for _ in 0..500 {
            s.step(0.2, &mut rng);
        }
        // Base pulls back to 50.0.
        let gap = (s.current_hz - NOMINAL_HZ).abs();
        assert!(
            gap < 0.01,
            "expected drift back to 50.0, got {}",
            s.current_hz
        );
    }

    /// Box-Muller stays bounded for typical seeds. Sanity check: the
    /// noise term shouldn't blow up to absurd values at 1 sigma.
    #[test]
    fn step_noise_is_modest() {
        let mut rng = SmallRng::seed_from_u64(42);
        let mut s = FrequencyState::new();
        let mut max_dev = 0.0f32;
        for _ in 0..1000 {
            s.step(0.2, &mut rng);
            let d = (s.current_hz - NOMINAL_HZ).abs();
            if d > max_dev {
                max_dev = d;
            }
        }
        // With σ=0.015 + reversion, 1000 steps shouldn't escape
        // ±500 mHz under normal conditions. (Equilibrium std dev
        // is ≈ 47 mHz, so 10× that is generous.)
        assert!(max_dev < 0.5, "frequency strayed by {max_dev} Hz");
    }
}
