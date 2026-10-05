//! MicrogridSite-level event stream — fans out telemetry samples and topology
//! version bumps to UI subscribers (the WebSocket endpoint, future
//! command-line monitors, anything else that wants live notifications).
//!
//! `tokio::sync::broadcast` is multi-producer / multi-consumer with a
//! bounded ring; if a subscriber falls behind by more than the ring
//! capacity it gets a `RecvError::Lagged` and skips ahead. That's the
//! right tradeoff for a UI: the chart is stale anyway when the tab
//! has been backgrounded for an hour, no value in queueing every
//! missed sample.

use serde::Serialize;

/// Capacity of the broadcast ring. Sized to absorb a full second of
/// telemetry samples (~10 metrics × ~50 components) plus headroom
/// for eval bursts. Subscribers that fall further behind than this
/// will see `Lagged` errors and re-sync.
pub const EVENT_BUS_CAPACITY: usize = 4096;

/// A single broadcast event the UI can react to. The discriminator
/// is `kind`; per-variant fields are inlined alongside it (serde
/// `tag` + `flatten`-equivalent via per-variant struct shape).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SiteEvent {
    /// Mutation occurred (eval, reload, …). Subscribers should
    /// refetch /api/mg/{mg}/topology if they care about structure or
    /// metadata changes. Cheap signal — sent on every accepted eval
    /// regardless of whether the eval actually mutated state.
    TopologyChanged { version: u64 },
    /// Single telemetry sample, emitted by the history sampler.
    Sample {
        id: u64,
        metric: &'static str,
        ts_ms: i64,
        value: f32,
    },
    /// Control-app setpoint event — fires for every gRPC SetActive /
    /// SetReactive / AugmentBounds the server processed (regardless
    /// of accept / reject). UI inspector appends to the live list.
    /// Field is `setpoint_kind` (not `kind`) to avoid colliding with
    /// the parent enum's serde `tag = "kind"` discriminator.
    Setpoint {
        id: u64,
        ts_ms: i64,
        /// Lowercase token: "active_power" / "reactive_power" /
        /// "augment_bounds" (active axis) / "augment_reactive_bounds".
        setpoint_kind: &'static str,
        value: f32,
        accepted: bool,
        /// Only set when `accepted == false` — the gRPC error message
        /// the client received.
        reason: Option<String>,
    },
    /// One captured log record. Fanned out from `ui_log::LogTap` via
    /// the WS handler — the handler subscribes to LOG_TAP separately
    /// and re-emits as this variant so the SPA's single WS stream
    /// covers everything.
    Log {
        ts_ms: i64,
        level: String,
        target: String,
        message: String,
    },
    /// Reload (or the initial load) raised a lisp error. The site
    /// has been reset to its post-reset (empty) state by `reload`
    /// before this fires, so a UI subscriber knows to show a
    /// banner "config invalid since `ts_ms` — fix and save to
    /// recover" rather than "everything got deleted".
    ConfigError { ts_ms: i64, message: String },
    /// One sample from an aggregated metric stream that the loopback
    /// Microgrid client exposes — grid_power, battery_pool_power,
    /// pv_power, consumer_power, producer_power, etc. (see
    /// `ui::spawn_microgrid_loopback` for the set of streams).
    /// `value` is the f32 magnitude in the base `unit`; `None` means
    /// the formula has no current value (e.g. the source category
    /// has no live samples in the configured `LogicalMeterConfig`
    /// resampling window). The SPA's Dashboard tiles pick by
    /// `stream` and apply auto-scale on `unit`.
    MicrogridSample {
        stream: &'static str,
        /// Quantity type name — `"Power"` / `"Voltage"` / `"Frequency"` /
        /// `"Percentage"` / etc. Matches `frequenz_microgrid::quantity`'s
        /// type names. Lets the SPA group same-quantity tiles onto a
        /// shared visual baseline without parsing the unit string.
        quantity: &'static str,
        /// Base unit string — `"W"` / `"VAR"` / `"V"` / `"Hz"` / `"%"`.
        unit: &'static str,
        ts_ms: i64,
        value: Option<f32>,
    },
    /// A dispatch was created / updated / deleted in the enterprise
    /// dispatch store. The SPA's per-microgrid Dispatches view
    /// refetches `/api/mg/{id}/dispatches` when the carried `mg_id`
    /// (set on the `WireEvent` wrapper) matches the microgrid it's
    /// showing. Emitted directly by the WS event pump from its
    /// `DispatchStore` subscription — like the `Log` variant — rather
    /// than from a per-site event bus, since the dispatch store is
    /// enterprise-wide and lives on `Config`, not on a `MicrogridSite`.
    DispatchChanged {
        dispatch_id: u64,
        /// `"created"` / `"updated"` / `"deleted"`.
        change: &'static str,
    },
    /// A runtime knob changed — REPL, scenario, typed control API, or the
    /// web UI; all writes funnel through the defuns or the control
    /// handlers, and both emit this. The inspector refreshes its
    /// edit-in-place inputs from it.
    KnobChanged {
        id: u64,
        ts_ms: i64,
        /// One of: "meter-power" / "meter-reactive-power" /
        /// "meter-power-factor" / "solar-sunlight" / "boiler-demand" /
        /// "boiler-pressure" / "ev" / "reactive-pf-limit" /
        /// "reactive-apparent-va".
        knob: &'static str,
        /// New value; None when the knob was cleared (pf-limit /
        /// apparent-va accept clearing).
        value: Option<f32>,
        /// Printed Lisp source when the write installed an expression.
        expr: Option<String>,
        /// meter-power-factor only.
        leading: Option<bool>,
    },
}
