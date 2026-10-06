//! Enterprise-scoped metadata setters: `(set-enterprise-id)`,
//! `(set-assets-socket-addr)`, `(set-dispatch-socket-addr)`,
//! `(set-default-request-lifetime-s)`,
//! `(set-default-augment-lifetime-s)`, and the `-ms` names that still
//! work.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tulisp::{Error, TulispContext};

use super::super::Metadata;
use super::super::renames::warn_renamed;

pub(super) fn register(ctx: &mut TulispContext, metadata: Arc<RwLock<Metadata>>) {
    let m = metadata.clone();
    ctx.defun("set-enterprise-id", move |id: i64| -> Result<bool, Error> {
        if id < 0 {
            // The proto id is unsigned; `as u64` would wrap a
            // negative into an 18-quintillion enterprise id.
            return Err(Error::invalid_argument(format!(
                "set-enterprise-id: id must be non-negative, got {id}"
            )));
        }
        m.write().enterprise_id = id as u64;
        Ok(true)
    });
    let m = metadata.clone();
    ctx.defun(
        "set-assets-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            m.write().assets_socket_addr = addr;
            Ok(true)
        },
    );
    let m = metadata.clone();
    ctx.defun(
        "set-dispatch-socket-addr",
        move |addr: String| -> Result<bool, Error> {
            m.write().dispatch_socket_addr = addr;
            Ok(true)
        },
    );
    for (name, ms_name, augment) in [
        (
            "set-default-request-lifetime-s",
            "set-default-request-lifetime-ms",
            false,
        ),
        (
            "set-default-augment-lifetime-s",
            "set-default-augment-lifetime-ms",
            true,
        ),
    ] {
        let store = move |md: &mut Metadata, d: Duration| {
            if augment {
                md.default_augment_lifetime = d;
            } else {
                md.default_request_lifetime = d;
            }
        };
        let m = metadata.clone();
        ctx.defun(name, move |secs: f64| -> Result<bool, Error> {
            store(&mut m.write(), crate::lisp::secs_duration(name, secs)?);
            Ok(true)
        });
        let m = metadata.clone();
        ctx.defun(ms_name, move |ms: i64| -> Result<bool, Error> {
            warn_renamed(ms_name, name, " (seconds)");
            store(&mut m.write(), Duration::from_millis(ms.max(0) as u64));
            Ok(true)
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::super::test_support::config_with;

    #[test]
    fn default_lifetimes_take_seconds_and_the_ms_names_still_work() {
        let (cfg, _dir) = config_with("(set-default-request-lifetime-s 45)");
        assert_eq!(
            cfg.metadata().default_request_lifetime,
            Duration::from_secs(45)
        );
        cfg.eval("(set-default-request-lifetime-ms 20000)").unwrap();
        assert_eq!(
            cfg.metadata().default_request_lifetime,
            Duration::from_secs(20)
        );
        cfg.eval("(set-default-augment-lifetime-s 1.5)").unwrap();
        assert_eq!(
            cfg.metadata().default_augment_lifetime,
            Duration::from_millis(1500)
        );
        assert!(cfg.eval("(set-default-augment-lifetime-s -1)").is_err());
    }
}
