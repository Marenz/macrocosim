//! `(set-component-health)`, `(set-component-telemetry-mode)`,
//! `(set-component-command-mode)` — flip a component's runtime
//! enum at REPL / scenario time so fault simulation is scriptable.
//! Plus the site-wide stream knobs `(cancel-all-streams)` and
//! `(set-sample-lag-s)`, and its `-ms` name that still works.

use tulisp::TulispContext;

use crate::sim::microgrids::SharedSiteRouter;

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    use crate::sim::runtime::{CommandMode, Health, TelemetryMode};

    // All four setters below reject an unregistered id, matching
    // their siblings (set-meter-power, set-active-power, ...). The
    // site-level setters do that rejection; each defun only prefixes
    // its own name onto their message.
    let r = router.clone();
    ctx.defun(
        "set-component-health",
        move |id: i64, h: Health| -> Result<bool, tulisp::Error> {
            let w = r.site();
            w.set_health(id as u64, h).map_err(|e| {
                tulisp::Error::invalid_argument(format!("set-component-health: {e}"))
            })?;
            Ok(true)
        },
    );

    let r = router.clone();
    ctx.defun(
        "set-component-telemetry-mode",
        move |id: i64, m: TelemetryMode| -> Result<bool, tulisp::Error> {
            let w = r.site();
            w.set_telemetry_mode(id as u64, m).map_err(|e| {
                tulisp::Error::invalid_argument(format!("set-component-telemetry-mode: {e}"))
            })?;
            Ok(true)
        },
    );

    let r = router.clone();
    ctx.defun(
        "set-component-command-mode",
        move |id: i64, m: CommandMode| -> Result<bool, tulisp::Error> {
            let w = r.site();
            w.set_command_mode(id as u64, m).map_err(|e| {
                tulisp::Error::invalid_argument(format!("set-component-command-mode: {e}"))
            })?;
            Ok(true)
        },
    );

    // Not a mode but a parameter of one: how an `over-bound`
    // component rejects. With a limit it rejects every request above
    // that magnitude, constantly; without one it keeps the rotating
    // fault window. Omit WATTS to clear it again.
    let r = router.clone();
    ctx.defun(
        "set-component-over-bound-limit",
        move |id: i64, watts: Option<f64>| -> Result<bool, tulisp::Error> {
            let limit = watts
                .map(crate::sim::runtime::validate_over_bound_limit_w)
                .transpose()
                .map_err(|e| {
                    tulisp::Error::invalid_argument(format!("set-component-over-bound-limit: {e}"))
                })?;
            r.site()
                .set_over_bound_limit(id as u64, limit)
                .map_err(|e| {
                    tulisp::Error::invalid_argument(format!("set-component-over-bound-limit: {e}"))
                })?;
            Ok(true)
        },
    );

    // Unlike the runtime knobs above, the operational mode is a
    // CONFIG parameter — the declared capability of the component.
    // Setting it re-derives the runtime knobs (no telemetry means a
    // silent stream, no control means an erroring command channel)
    // and, being structural, is written back into the microgrid's
    // managed file (`:operational-mode` in the generated block).
    let r = router.clone();
    ctx.defun(
        "set-component-operational-mode",
        move |id: i64, m: crate::sim::component::OperationalMode| -> Result<bool, tulisp::Error> {
            let w = r.site();
            w.set_operational_mode(id as u64, m)
                .map_err(tulisp::Error::invalid_argument)?;
            Ok(true)
        },
    );

    let r = router.clone();
    ctx.defun("cancel-all-streams", move || -> bool {
        // Server-side graceful cancel of every active stream. Each
        // streaming task sees the epoch bump on its next iteration and
        // exits, sending the client an EOF/CANCELLED. Clients reconnect
        // and resume on fresh streams.
        r.site().cancel_all_streams();
        true
    });

    let r2 = router.clone();
    ctx.defun(
        "set-sample-lag-s",
        move |secs: f64| -> Result<bool, tulisp::Error> {
            // Shift every outgoing telemetry sample's timestamp into
            // the past by SECS seconds. Models a server that delivers
            // samples with a fixed timestamp lag, e.g. to test how a
            // downstream resampler copes with stale data.
            let d = crate::lisp::secs_duration("set-sample-lag-s", secs)?;
            r2.site().set_sample_lag_ms(d.as_millis() as u64);
            Ok(true)
        },
    );

    let r = router;
    ctx.defun("set-sample-lag-ms", move |ms: i64| -> bool {
        super::super::renames::warn_renamed("set-sample-lag-ms", "set-sample-lag-s", " (seconds)");
        r.site().set_sample_lag_ms(ms.max(0) as u64);
        true
    });
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;

    /// `set-sample-lag-s` takes seconds and refuses a negative value;
    /// the `-ms` name still takes milliseconds.
    #[test]
    fn sample_lag_takes_seconds() {
        let (cfg, _dir) = config_with("");
        cfg.eval("(set-sample-lag-s 0.05)").unwrap();
        assert_eq!(cfg.site().sample_lag_ms(), 50);
        let err = cfg.eval("(set-sample-lag-s -1)").unwrap_err();
        assert!(err.contains("set-sample-lag-s"), "{err}");
        assert_eq!(cfg.site().sample_lag_ms(), 50);
        cfg.eval("(set-sample-lag-ms 20)").unwrap();
        assert_eq!(cfg.site().sample_lag_ms(), 20);
    }
}
