//! Scalar component inputs that may be a constant or a Lisp expression.
//!
//! A meter's `:power-w` and a solar inverter's `:sunlight-pct` are scalar
//! inputs that scenario scripts often want to drive declaratively:
//!
//! ```lisp
//! (make-meter :power-w (lambda () (csv-lookup curve (now-seconds))))
//! (make-meter :power-w 'consumer-power)              ; deref a global
//! ```
//!
//! `DynamicScalar` is the storage shape that lets a component carry
//! such an input without violating the architectural rule that
//! `tick()` must not call back into the interpreter. The MicrogridSite
//! scheduler holds the interpreter lock once per tick, calls
//! [`SimulatedComponent::refresh_inputs`], which re-resolves the
//! source and stores the resulting `f32` in an atomic. `tick()`
//! then reads the atomic — pure Rust, no Lisp.
//!
//! [`SimulatedComponent::refresh_inputs`]: crate::sim::component::SimulatedComponent::refresh_inputs

use std::sync::atomic::{AtomicU32, Ordering};

use tulisp::{Error, TulispContext, TulispObject};

/// A scalar input that is either a constant or a Lisp expression.
pub struct DynamicScalar {
    cached: AtomicU32,
    /// The zero-argument function each refresh calls.
    source: Option<TulispObject>,
    /// Printed Lisp form of `source`, captured at construction so
    /// read-back never touches the `TulispObject` off the
    /// interpreter lock. `None` for constants.
    source_text: Option<String>,
}

/// Clones the cached value into a fresh atomic (loaded from the
/// current bits, `Acquire` pairing with `set`'s `Release`) alongside
/// the source and its printed text. The cached value and the atomic
/// itself are independent after the clone: a `set` on one never
/// touches the other. The source it holds is a different story —
/// the `TulispObject` inside is itself `Clone`, an `Arc` bump under
/// tulisp's sync feature, so a lambda's or symbol's underlying Lisp
/// cell is deliberately SHARED between the original and the clone;
/// that's what makes a lambda's closed-over state and a symbol's
/// live binding still resolve correctly through either copy. Scenario
/// teardown relies on the cached-value independence: a snapshot's
/// cache has to be a standalone copy, not a second handle the
/// scenario's own refreshes silently update out from under it.
impl Clone for DynamicScalar {
    fn clone(&self) -> Self {
        Self {
            cached: AtomicU32::new(self.cached.load(Ordering::Acquire)),
            source: self.source.clone(),
            source_text: self.source_text.clone(),
        }
    }
}

impl DynamicScalar {
    /// A pure constant. `refresh` is a no-op.
    pub fn constant(v: f32) -> Self {
        Self {
            cached: AtomicU32::new(v.to_bits()),
            source: None,
            source_text: None,
        }
    }

    /// A Lisp-driven value: EXPR is compiled once, as the body of a
    /// lambda, and the lambda is called on each refresh. Use this for
    /// symbol-form sources (`'consumer-power`) or arbitrary Lisp
    /// expressions whose evaluation yields a number. Fails if EXPR
    /// does not compile.
    pub fn from_eval(
        ctx: &mut TulispContext,
        expr: TulispObject,
        fallback: f32,
    ) -> Result<Self, Error> {
        let source_text = Some(expr.to_string());
        let lambda: TulispObject = [ctx.intern("lambda"), TulispObject::nil(), expr]
            .into_iter()
            .collect();
        let callable = ctx.eval(&lambda)?;
        Ok(Self {
            cached: AtomicU32::new(fallback.to_bits()),
            source: Some(callable),
            source_text,
        })
    }

    /// A Lisp-driven value resolved by [`TulispContext::funcall`]
    /// each refresh. Use this when `callable` is a lambda value or
    /// any other zero-arg callable handed in directly as a plist
    /// value.
    pub fn from_funcall(callable: TulispObject, fallback: f32) -> Self {
        let source_text = Some(callable.to_string());
        Self {
            cached: AtomicU32::new(fallback.to_bits()),
            source: Some(callable),
            source_text,
        }
    }

    /// Build the right variant by inspecting `obj`'s shape:
    ///
    /// - `nil` → `None`.
    /// - number → [`Self::constant`].
    /// - symbol, cons, string → [`Self::from_eval`] (a symbol derefs;
    ///   a cons is compiled once and runs on each refresh).
    /// - anything else (`Lambda` / `CompiledDefun` / opaque Rust
    ///   handle) → [`Self::from_funcall`].
    ///
    /// Pass a lambda value *unquoted* in the plist —
    /// `:power-w (lambda () …)` — so the plist evaluator hands back the
    /// compiled function rather than the literal list.
    pub fn from_lisp(
        ctx: &mut TulispContext,
        obj: &TulispObject,
        fallback: f32,
    ) -> Result<Option<Self>, Error> {
        if obj.null() {
            return Ok(None);
        }
        if obj.numberp() {
            return Ok(f64::try_from(obj).ok().map(|n| Self::constant(n as f32)));
        }
        if obj.symbolp() || obj.consp() || obj.stringp() {
            return Self::from_eval(ctx, obj.clone(), fallback).map(Some);
        }
        Ok(Some(Self::from_funcall(obj.clone(), fallback)))
    }

    /// Read the cached resolved value. Cheap; never blocks. Acquire
    /// pairs with `set`'s Release so the physics-tick reader observes
    /// the refresh thread's latest store without relying implicitly
    /// on surrounding locks for visibility.
    pub fn get(&self) -> f32 {
        f32::from_bits(self.cached.load(Ordering::Acquire))
    }

    /// Overwrite the cached value. Used by `(set-meter-power id W)`-
    /// style external setters and by tests.
    pub fn set(&self, v: f32) {
        self.cached.store(v.to_bits(), Ordering::Release);
    }

    /// True if the source is a Lisp expression rather than a static
    /// constant. Components use this to skip dynamic-source bookkeeping
    /// for the common numeric case.
    pub fn is_dynamic(&self) -> bool {
        self.source.is_some()
    }

    /// The printed Lisp form of the source expression, captured once
    /// at construction. `None` for constants. Safe to call off the
    /// interpreter lock — unlike `source`, this never touches a
    /// `TulispObject`.
    pub fn source_text(&self) -> Option<String> {
        self.source_text.clone()
    }

    /// Re-resolve the source and update the cached value. No-op for
    /// constants. Errors and non-finite results (`NaN`, `±∞`) log
    /// and keep the prior cached value — a scenario shouldn't
    /// corrupt downstream telemetry if a curve transiently returns
    /// garbage.
    pub fn refresh(&self, ctx: &mut TulispContext) {
        let Some(src) = &self.source else { return };
        let label = self.source_text.as_deref().unwrap_or_default();
        let result = ctx.funcall(src, ());
        match result {
            Ok(obj) => match f64::try_from(&obj) {
                Ok(v) if v.is_finite() => self.set(v as f32),
                Ok(v) => log::warn!(
                    "DynamicScalar refresh: non-finite result {} from {}; keeping prior value",
                    v,
                    label,
                ),
                Err(e) => log::warn!(
                    "DynamicScalar refresh: non-numeric result from {}: {}",
                    label,
                    e.with_file_names(ctx)
                ),
            },
            Err(e) => log::warn!("DynamicScalar refresh error in {label}: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tulisp::TulispContext;

    #[test]
    fn constant_get_returns_initial_value() {
        let s = DynamicScalar::constant(123.5);
        assert_eq!(s.get(), 123.5);
    }

    #[test]
    fn constant_refresh_is_noop() {
        let s = DynamicScalar::constant(7.0);
        let mut ctx = TulispContext::new();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 7.0);
    }

    #[test]
    fn set_overrides_cached() {
        let s = DynamicScalar::constant(0.0);
        s.set(99.5);
        assert_eq!(s.get(), 99.5);
    }

    #[test]
    fn from_eval_runs_arithmetic_each_refresh() {
        let mut ctx = TulispContext::new();
        // `'(* 3 14.0)` returns the quoted list itself; refresh evals
        // it to 42.0.
        let src = ctx.eval_string("'(* 3 14.0)").unwrap();
        let s = DynamicScalar::from_eval(&mut ctx, src, 0.0).unwrap();
        assert_eq!(s.get(), 0.0);
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 42.0);
    }

    // The expression is compiled once, when the value is made: a
    // macro redefined afterwards does not change it.
    #[test]
    fn from_eval_compiles_once() {
        let mut ctx = TulispContext::new();
        ctx.eval_string("(defmacro level () 1.0)").unwrap();
        let src = ctx.eval_string("'(level)").unwrap();
        let s = DynamicScalar::from_eval(&mut ctx, src, 0.0).unwrap();
        ctx.eval_string("(defmacro level () 2.0)").unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 1.0);
    }

    #[test]
    fn from_eval_keeps_fallback_on_non_finite_result() {
        let mut ctx = TulispContext::new();
        // 1.0 / 0.0 = +Inf in tulisp's float arithmetic; the
        // refresh path should reject it and keep the fallback
        // rather than poison the cache.
        let src = ctx.eval_string("'(/ 1.0 0.0)").unwrap();
        let s = DynamicScalar::from_eval(&mut ctx, src, 7.5).unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 7.5);
    }

    #[test]
    fn from_eval_keeps_fallback_on_non_numeric() {
        let mut ctx = TulispContext::new();
        let src = ctx.eval_string("'\"not a number\"").unwrap();
        let s = DynamicScalar::from_eval(&mut ctx, src, 1.5).unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 1.5);
    }

    #[test]
    fn from_eval_derefs_a_symbol() {
        let mut ctx = TulispContext::new();
        ctx.eval_string("(setq consumer-power 1500.0)").unwrap();
        let sym = ctx.eval_string("'consumer-power").unwrap();
        let s = DynamicScalar::from_eval(&mut ctx, sym, 0.0).unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 1500.0);
        // Mutate the bound variable; refresh picks up the new value.
        ctx.eval_string("(setq consumer-power 2750.0)").unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 2750.0);
    }

    #[test]
    fn from_funcall_invokes_a_lambda() {
        let mut ctx = TulispContext::new();
        let lambda = ctx.eval_string("(lambda () 17.5)").unwrap();
        let s = DynamicScalar::from_funcall(lambda, 0.0);
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 17.5);
    }

    #[test]
    fn from_lisp_dispatches_on_kind() {
        let mut ctx = TulispContext::new();

        // Numeric → constant.
        let n = ctx.eval_string("42.0").unwrap();
        let s = DynamicScalar::from_lisp(&mut ctx, &n, 0.0)
            .unwrap()
            .unwrap();
        assert!(!s.is_dynamic());
        assert_eq!(s.get(), 42.0);

        // Lambda value (CompiledDefun after eval) → funcall.
        let l = ctx.eval_string("(lambda () 9.5)").unwrap();
        let s = DynamicScalar::from_lisp(&mut ctx, &l, 0.0)
            .unwrap()
            .unwrap();
        assert!(s.is_dynamic());
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 9.5);

        // Symbol → eval (deref) on refresh.
        ctx.eval_string("(setq pv-cap 8000.0)").unwrap();
        let sym = ctx.eval_string("'pv-cap").unwrap();
        let s = DynamicScalar::from_lisp(&mut ctx, &sym, 0.0)
            .unwrap()
            .unwrap();
        assert!(s.is_dynamic());
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 8000.0);

        // Cons cell (arbitrary Lisp expression) → eval on refresh.
        let expr = ctx.eval_string("'(* 2 21)").unwrap();
        let s = DynamicScalar::from_lisp(&mut ctx, &expr, 0.0)
            .unwrap()
            .unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 42.0);

        // nil → None.
        let nil = ctx.eval_string("nil").unwrap();
        assert!(
            DynamicScalar::from_lisp(&mut ctx, &nil, 0.0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn constant_has_no_source_text() {
        let s = DynamicScalar::constant(42.0);
        assert_eq!(s.source_text(), None);
    }

    /// A clone is an independent copy: it starts with the same
    /// cached value and source text, and mutating one (`set`, or a
    /// `refresh` that resolves a different result) does not leak
    /// into the other.
    #[test]
    fn clone_is_independent_of_the_original() {
        let mut ctx = TulispContext::new();
        ctx.eval_string("(setq clone-src 10.0)").unwrap();
        let sym = ctx.eval_string("'clone-src").unwrap();
        let s = DynamicScalar::from_eval(&mut ctx, sym, 0.0).unwrap();
        s.refresh(&mut ctx);
        assert_eq!(s.get(), 10.0);

        let cloned = s.clone();
        assert_eq!(cloned.get(), 10.0);
        assert_eq!(cloned.source_text(), s.source_text());
        assert!(cloned.is_dynamic());

        // Poking the original's cache doesn't touch the clone.
        s.set(999.0);
        assert_eq!(cloned.get(), 10.0);

        // Re-resolving the clone independently picks up the live
        // binding without disturbing the original's cache.
        ctx.eval_string("(setq clone-src 20.0)").unwrap();
        cloned.refresh(&mut ctx);
        assert_eq!(cloned.get(), 20.0);
        assert_eq!(s.get(), 999.0);
    }

    #[test]
    fn from_lisp_captures_printed_source() {
        let mut ctx = TulispContext::new();
        // Quoted so `from_lisp` sees a cons (unevaluated lambda form)
        // and routes through `from_eval`, preserving the printed
        // source rather than the opaque `CompiledDefun` a compiled
        // lambda value would print as.
        let obj = ctx.eval_string("'(lambda () 5)").unwrap();
        let s = DynamicScalar::from_lisp(&mut ctx, &obj, 0.0)
            .unwrap()
            .unwrap();
        let text = s.source_text().expect("dynamic scalar has source text");
        assert!(text.contains("lambda"), "got: {text}");
    }
}
