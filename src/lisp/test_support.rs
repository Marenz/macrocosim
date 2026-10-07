//! Shared test fixtures for the lisp/ subtree. Every child module's
//! `#[cfg(test)] mod tests` block builds its `Config` instances
//! through `config_with`, which seeds a fresh temp dir + auto-
//! wraps the test body in a `(make-microgrid …)` form so callers
//! don't have to repeat the boilerplate.

use super::Config;
use crate::test_dir::TestDir;

/// Build a Config from a tiny config.lisp body in a fresh temp dir;
/// returns the Config + the dir so tests can mess with the
/// per-microgrid override path. Dropping the dir removes it.
pub(super) fn config_with(body: &str) -> (Config, TestDir) {
    let dir = TestDir::new("macrocosim-cfg-");
    let path = dir.join("config.lisp");
    let wrapped = wrap_test_body(body);
    std::fs::write(&path, wrapped).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cfg = rt
        .block_on(async { Config::new(path.to_str().unwrap()) })
        .expect("config eval");
    // Drop the runtime — Config keeps its own handles to whatever
    // tulisp-async spawned during init.
    std::mem::forget(rt);
    (cfg, dir)
}

/// Auto-wrap a test body in `(make-microgrid …)` if the body doesn't
/// already register one — every config must do so post-migration, but
/// most tests don't care about the wrapper and just want their forms
/// evaluated in a microgrid scope. Tests that exercise make-microgrid
/// itself, or that care about the microgrid's id, supply their own
/// form and the wrapper is skipped. Everything else gets the fixed
/// default id 2200.
pub(super) fn wrap_test_body(body: &str) -> String {
    if body.contains("make-microgrid") {
        return body.to_string();
    }
    let inner = if body.trim().is_empty() {
        "nil".to_string()
    } else {
        body.to_string()
    };
    format!("(make-microgrid :id 2200 :grpc-port 8800 :topology (lambda () {inner}))")
}

/// The message line of a Lisp error: tulisp appends the trace
/// (`<eval_string>:1.1-…: at …`) on the lines after it, and those
/// positions are not part of the message a door produces.
pub(super) fn err_line(cfg: &Config, src: &str) -> String {
    let err = cfg
        .eval(src)
        .expect_err(&format!("{src} should have errored"));
    err.lines().next().unwrap_or_default().to_string()
}

/// What a lenient setter must leave alone on a component of the
/// wrong kind, read through methods every component has: its
/// constructor kwargs, whether it holds a dynamic source, its power
/// and reactive aggregates, and its telemetry. The telemetry leaves
/// out the grid frequency, which moves on its own between reads.
fn fingerprint(cfg: &Config, id: u64) -> String {
    let site = cfg.site();
    let c = site.get(id).expect("component registered");
    format!(
        "{:?} {} {} {} {:?}",
        c.constructor_kwargs(),
        c.has_unrenderable_source(),
        c.aggregate_power_w(&site),
        c.aggregate_reactive_var(&site),
        crate::sim::Telemetry {
            frequency_hz: None,
            ..c.telemetry(&site)
        },
    )
}

/// Run a lenient setter against component `id` of the wrong kind:
/// it returns `t`, leaves the component as it was, and (when `knob`
/// is given) still broadcasts that `KnobChanged`.
pub(super) fn assert_lenient_noop(cfg: &Config, id: u64, src: &str, knob: Option<&str>) {
    let before = fingerprint(cfg, id);
    let mut rx = cfg.site().subscribe_events();
    assert_eq!(cfg.eval(src).as_deref(), Ok("t"), "{src}");
    assert_eq!(fingerprint(cfg, id), before, "{src} changed component {id}");
    if let Some(knob) = knob {
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            seen.push(ev);
        }
        assert!(
            seen.iter().any(|ev| matches!(
                ev,
                crate::sim::events::SiteEvent::KnobChanged { id: i, knob: k, .. }
                    if *i == id && *k == knob
            )),
            "{src}: no {knob} KnobChanged for {id}; saw: {seen:?}"
        );
    }
}
