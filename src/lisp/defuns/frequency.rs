//! Grid-frequency defuns: read the live value, write a one-shot
//! override, or retune the OU process driving the shared frequency
//! state.

use tulisp::{Error, TulispContext};

use crate::lisp::renames::Renamed;

tulisp::AsPlist! {
    /// Plist payload for `(set-frequency-model …)`. Every field
    /// optional — only the keys the caller passes are touched.
    pub struct FrequencyModelArgs {
        /// Mean the OU process pulls toward (Hz).
        nominal: Option<f64> {= None},
        /// Mean reversion rate (1/s). Correlation time of the
        /// noisy fluctuations is roughly `1 / mean-rev-rate`.
        mean_rev_rate<":mean-rev-rate">: Option<f64> {= None},
        /// Noise intensity (Hz/sqrt(s)). Equilibrium standard
        /// deviation is `sigma / sqrt(2 * mean-rev-rate)`.
        sigma: Option<f64> {= None},
    }
}

/// Defuns the config + scenarios use to drive the shared grid
/// frequency:
///
/// - `(set-frequency F)` — one-shot write of the current value.
///   The OU driver overwrites on the next step (every 200 ms), so
///   this is useful for test fixtures or for setting an initial
///   condition the OU then evolves away from.
/// - `(set-frequency-model :nominal :mean-rev-rate :sigma)` —
///   tune the *base* driver parameters. Each key optional;
///   unspecified keys keep their current base values. Defaults
///   pick a noise floor (~47 mHz std dev) and correlation time
///   (~20 s) that look like a healthy synchronous grid.
/// - `(override-frequency-model :nominal :mean-rev-rate :sigma)`
///   — install an override on the OU dynamics. Driver keeps
///   integrating, but uses the override's params in place of the
///   base while it's set. Unspecified keys inherit from the
///   current active model (override if already set, else base) —
///   so `(override-frequency-model :nominal 49.5)` pulls toward
///   49.5 with the base dynamics, and a later
///   `(override-frequency-model :sigma 0.05)` widens noise
///   without disturbing the override nominal.
/// - `(clear-frequency-override)` — drop the override; the
///   driver returns to base dynamics from the current value.
/// - `(current-frequency)` — read the live value.
pub(in crate::lisp) fn register(
    ctx: &mut TulispContext,
    state: crate::sim::frequency::SharedFrequency,
) {
    use crate::sim::frequency::FrequencyModel;
    fn apply_overrides(model: &mut FrequencyModel, a: &FrequencyModelArgs) {
        if let Some(v) = a.nominal {
            model.nominal_hz = v as f32;
        }
        if let Some(v) = a.mean_rev_rate {
            model.mean_rev_rate = v.max(0.0) as f32;
        }
        if let Some(v) = a.sigma {
            model.sigma = v.max(0.0) as f32;
        }
    }

    let s = state.clone();
    ctx.defun("set-frequency", move |hz: f64| -> Result<bool, Error> {
        // A NaN/Inf here would poison the shared OU state permanently
        // (`step()` integrates `current_hz += …`, so non-finite never
        // recovers) — and the state is shared by EVERY microgrid.
        if !hz.is_finite() {
            return Err(Error::invalid_argument(format!(
                "set-frequency: hz must be finite, got {hz}"
            )));
        }
        s.write().current_hz = hz as f32;
        Ok(true)
    });

    let s = state.clone();
    ctx.defun(
        "set-frequency-model",
        move |args: tulisp::Plist<Renamed<FrequencyModelArgs>>| -> Result<bool, Error> {
            let a = args.into_inner().0;
            apply_overrides(&mut s.write().base, &a);
            Ok(true)
        },
    );

    let s = state.clone();
    ctx.defun(
        "override-frequency-model",
        move |args: tulisp::Plist<Renamed<FrequencyModelArgs>>| -> Result<bool, Error> {
            let a = args.into_inner().0;
            let mut g = s.write();
            // Missing keys inherit from the currently-active model:
            // the existing override if there is one (so repeated
            // calls layer), else the base (so the first call after a
            // clear picks up sensible defaults).
            let mut next = g.active_model();
            apply_overrides(&mut next, &a);
            g.override_model = Some(next);
            Ok(true)
        },
    );

    let s = state.clone();
    ctx.defun(
        "clear-frequency-override",
        move || -> Result<bool, Error> {
            s.write().override_model = None;
            Ok(true)
        },
    );

    let s = state;
    ctx.defun("current-frequency", move || -> Result<f64, Error> {
        Ok(s.read().read_hz() as f64)
    });
}
