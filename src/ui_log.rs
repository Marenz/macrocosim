//! Capture log lines so the SPA can render them above the REPL.
//!
//! `LogTap` plugs into `simplelog`'s `CombinedLogger` alongside the
//! existing terminal logger — every emitted record both stays in the
//! ring-buffer (for /api/logs backfill) and fans out on a tokio
//! broadcast channel (for /ws/events live push).
//!
//! Wired through a process-wide `OnceLock` because the global `log`
//! crate registry only accepts one logger via `set_boxed_logger`,
//! and we want the WS handler in the UI server to subscribe to the
//! same instance the binary set up at startup. Tests that don't
//! initialise the tap see `LOG_TAP.get()` return `None`; the WS
//! handler degrades gracefully.

use std::{
    collections::VecDeque,
    sync::{Arc, OnceLock},
};

use chrono::Utc;
use parking_lot::Mutex;
use serde::Serialize;
use simplelog::SharedLogger;
use tokio::sync::broadcast;

/// Process-wide log tap. Set by `bin/macrocosim.rs` at startup;
/// `Option`-shaped so library tests that don't go through main can
/// run without a panic at first log call.
pub static LOG_TAP: OnceLock<LogTap> = OnceLock::new();

/// One log record, in the wire shape the SPA consumes.
#[derive(Clone, Debug, Serialize)]
pub struct LogEvent {
    /// Epoch milliseconds; serialized as RFC 3339 `ts`.
    #[serde(
        rename = "ts",
        serialize_with = "crate::timefmt::serialize_millis_as_rfc3339"
    )]
    pub ts_ms: i64,
    pub level: String,
    pub target: String,
    pub message: String,
}

// No Default derive: a default (cap 0) buffer silently keeps only
// the newest event; construct via `new(cap)`.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogEvent>>>,
    cap: usize,
}

impl LogBuffer {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap))),
            cap,
        }
    }

    fn push(&self, ev: LogEvent) {
        let mut g = self.inner.lock();
        if g.len() >= self.cap {
            g.pop_front();
        }
        g.push_back(ev);
    }

    pub fn snapshot(&self) -> Vec<LogEvent> {
        self.inner.lock().iter().cloned().collect()
    }
}

/// Combined ring buffer + broadcast bus. Cheap to clone (Arc'd
/// internals); the `simplelog` plumbing takes one clone, the WS
/// handler subscribes via another.
#[derive(Clone)]
pub struct LogTap {
    buffer: LogBuffer,
    bus: broadcast::Sender<LogEvent>,
    level: log::LevelFilter,
    /// Target-prefix list that gets dropped before the record
    /// reaches the buffer / bus. Mirrors `simplelog`'s
    /// `add_filter_ignore_str` on the terminal logger so a noisy
    /// upstream module (e.g. frequenz-microgrid's
    /// `ComponentTelemetryTracker` per-tick spam) is suppressed in
    /// the SPA log panel + /api/logs backfill too — not just the
    /// terminal output.
    ignore_targets: Arc<Vec<String>>,
}

impl LogTap {
    /// `cap` events kept in the backfill buffer; `level` is the
    /// minimum record level to capture; `ignore_targets` drops any
    /// record whose `target` starts with one of the listed
    /// prefixes (compared with `str::starts_with`, so a module-path
    /// prefix matches every submodule).
    pub fn new(cap: usize, level: log::LevelFilter, ignore_targets: Vec<String>) -> Self {
        Self {
            buffer: LogBuffer::new(cap),
            bus: broadcast::channel(256).0,
            level,
            ignore_targets: Arc::new(ignore_targets),
        }
    }

    pub fn snapshot(&self) -> Vec<LogEvent> {
        self.buffer.snapshot()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogEvent> {
        self.bus.subscribe()
    }

    fn is_ignored(&self, target: &str) -> bool {
        self.ignore_targets
            .iter()
            .any(|prefix| target.starts_with(prefix.as_str()))
    }
}

impl log::Log for LogTap {
    fn enabled(&self, md: &log::Metadata) -> bool {
        md.level() <= self.level && !self.is_ignored(md.target())
    }
    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ev = LogEvent {
            ts_ms: Utc::now().timestamp_millis(),
            level: record.level().to_string(),
            target: record.target().to_string(),
            message: format!("{}", record.args()),
        };
        self.buffer.push(ev.clone());
        // Send errors mean nobody's subscribed; that's fine.
        let _ = self.bus.send(ev);
    }
    fn flush(&self) {}
}

impl SharedLogger for LogTap {
    fn level(&self) -> log::LevelFilter {
        self.level
    }
    fn config(&self) -> Option<&simplelog::Config> {
        None
    }
    fn as_log(self: Box<Self>) -> Box<dyn log::Log> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_record_carries_an_rfc3339_ts() {
        let ev = LogEvent {
            ts_ms: 1_791_288_000_000,
            level: "info".into(),
            target: "t".into(),
            message: "m".into(),
        };
        let v = serde_json::to_value(ev).unwrap();
        assert_eq!(v["ts"], "2026-10-06T12:00:00.000Z");
    }

    #[test]
    fn ring_evicts_oldest() {
        let b = LogBuffer::new(2);
        for i in 0..3 {
            b.push(LogEvent {
                ts_ms: i,
                level: "info".into(),
                target: "t".into(),
                message: format!("{i}"),
            });
        }
        let snap = b.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].message, "1");
        assert_eq!(snap[1].message, "2");
    }

    #[tokio::test]
    async fn tap_broadcasts_to_subscribers() {
        let tap = LogTap::new(10, log::LevelFilter::Info, Vec::new());
        let mut rx = tap.subscribe();
        log::Log::log(
            &tap,
            &log::Record::builder()
                .args(format_args!("hi"))
                .level(log::Level::Info)
                .target("test")
                .build(),
        );
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.message, "hi");
        assert_eq!(tap.snapshot().len(), 1);
    }

    /// Records whose target starts with an ignored prefix are
    /// dropped before the buffer / bus see them — same surface the
    /// terminal logger applies via `add_filter_ignore_str`.
    #[test]
    fn ignored_target_prefix_is_dropped() {
        let tap = LogTap::new(
            10,
            log::LevelFilter::Info,
            vec!["frequenz_microgrid::microgrid::telemetry_tracker".to_owned()],
        );
        log::Log::log(
            &tap,
            &log::Record::builder()
                .args(format_args!("noisy"))
                .level(log::Level::Error)
                .target(
                    "frequenz_microgrid::microgrid::telemetry_tracker::component_telemetry_tracker",
                )
                .build(),
        );
        log::Log::log(
            &tap,
            &log::Record::builder()
                .args(format_args!("keep me"))
                .level(log::Level::Info)
                .target("macrocosim::ui")
                .build(),
        );
        let snap = tap.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].message, "keep me");
    }
}
