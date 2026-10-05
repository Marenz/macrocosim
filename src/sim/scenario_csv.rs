//! Per-component CSV sinks for scenario recording.
//!
//! `(scenario-record-csv DIR)` walks every registered component and
//! opens CSV files per id under `DIR`:
//!
//! - `<id>-<category>.csv` — telemetry, every component. The header
//!   is identical across categories — a uniform set of telemetry
//!   columns with empty cells where a component doesn't publish that
//!   field — so a downstream loader doesn't need per-category
//!   dispatch. Rows are written from inside
//!   `MicrogridSite::record_history_snapshot`, so the cadence matches
//!   the existing history sampler (1 Hz).
//! - `<id>-setpoints.csv` — the control inputs the component
//!   *received*: one row per SetActivePower / SetReactivePower /
//!   AugmentBounds request (value + resolved TTL + arrival time +
//!   outcome). Event-driven from `MicrogridSite::log_setpoint`, not
//!   sampled. Only opened for components with an active-power
//!   envelope — those are the ones a control app commands.
//! - `<id>-bounds.csv` — the effective active-power envelope over
//!   time, sampled at the same 1 Hz pass as telemetry. Together with
//!   the setpoints file this lets a scenario replay-assert "the app
//!   held a stable cap" vs "it oscillated" without an external log
//!   scrape. Same envelope-bearing component set as setpoints.
//! - `<id>-reactive-bounds.csv` — the Q twin of `<id>-bounds.csv`:
//!   the live reactive-power envelope, sampled at the same pass, for
//!   components that report one (`bounds_of(id, Reactive)` is `Some`)
//!   — a different component set than the active-bounds files, since
//!   an inverter has a Q axis but the battery behind it doesn't.
//!
//! Each sink is a `BufWriter<File>`; `(scenario-stop-csv)` and
//! `(scenario-stop)` drop the writers, which flushes and closes
//! the underlying files.

use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use chrono::{DateTime, Utc};

use crate::sim::bounds::VecBounds;
use crate::sim::component::{Category, Telemetry};
use crate::sim::setpoints::{SetpointEvent, SetpointOutcome};

const CSV_HEADER: &str = "ts_iso,active_power_w,reactive_power_var,dc_power_w,soc_pct\n";
const SETPOINTS_CSV_HEADER: &str = "ts_iso,kind,value,ttl_s,accepted,effective_value,reason\n";
const BOUNDS_CSV_HEADER: &str = "ts_iso,lower_w,upper_w,bands\n";
const REACTIVE_BOUNDS_CSV_HEADER: &str = "ts_iso,lower_var,upper_var,bands\n";

/// One CSV sink — owns the file handle until dropped.
pub(crate) struct CsvSink {
    writer: BufWriter<File>,
}

impl CsvSink {
    fn create(dir: &Path, file_name: String, header: &str) -> std::io::Result<Self> {
        let mut writer = BufWriter::new(File::create(dir.join(file_name))?);
        writer.write_all(header.as_bytes())?;
        Ok(Self { writer })
    }

    /// Open the telemetry sink `dir/<id>-<category>.csv`, write the
    /// header, return the sink. Errors if the directory doesn't
    /// exist or isn't writable.
    pub(crate) fn open(dir: &Path, id: u64, category: Category) -> std::io::Result<Self> {
        Self::create(dir, format!("{id}-{}.csv", category.as_str()), CSV_HEADER)
    }

    /// Open the received-setpoints sink `dir/<id>-setpoints.csv`.
    pub(crate) fn open_setpoints(dir: &Path, id: u64) -> std::io::Result<Self> {
        Self::create(dir, format!("{id}-setpoints.csv"), SETPOINTS_CSV_HEADER)
    }

    /// Open the effective-bounds timeline sink `dir/<id>-bounds.csv`.
    pub(crate) fn open_bounds(dir: &Path, id: u64) -> std::io::Result<Self> {
        Self::create(dir, format!("{id}-bounds.csv"), BOUNDS_CSV_HEADER)
    }

    /// Open the effective-reactive-bounds timeline sink
    /// `dir/<id>-reactive-bounds.csv` — the Q twin of `open_bounds`.
    /// Same row shape (`write_bounds_row` is unit-agnostic), different
    /// header and file name.
    pub(crate) fn open_reactive_bounds(dir: &Path, id: u64) -> std::io::Result<Self> {
        Self::create(
            dir,
            format!("{id}-reactive-bounds.csv"),
            REACTIVE_BOUNDS_CSV_HEADER,
        )
    }

    /// Append one row from a telemetry snapshot. Empty cells for
    /// fields the component doesn't publish.
    pub(crate) fn write_row(&mut self, ts: DateTime<Utc>, snap: &Telemetry) -> std::io::Result<()> {
        writeln!(
            self.writer,
            "{},{},{},{},{}",
            ts.to_rfc3339(),
            cell(snap.active_power_w),
            cell(snap.reactive_power_var),
            cell(snap.dc_power_w),
            cell(snap.soc_pct),
        )
    }

    /// Append one received-setpoint row. `effective_value` is empty
    /// for rejections and bounds augmentations; `reason` is empty
    /// for accepted requests (and CSV-quoted otherwise — gRPC error
    /// messages contain commas).
    pub(crate) fn write_setpoint_row(&mut self, ev: &SetpointEvent) -> std::io::Result<()> {
        let (accepted, effective_value, reason) = match &ev.outcome {
            SetpointOutcome::Accepted { effective_value } => (true, *effective_value, ""),
            SetpointOutcome::Rejected { reason } => (false, None, reason.as_str()),
        };
        writeln!(
            self.writer,
            "{},{},{},{},{},{},{}",
            ev.ts.to_rfc3339(),
            ev.kind.as_str(),
            ev.value,
            ev.ttl_s.map(|s| s.to_string()).unwrap_or_default(),
            accepted,
            cell(effective_value),
            csv_quote(reason),
        )
    }

    /// Append one effective-bounds row. `lower_w` / `upper_w` are the
    /// envelope extremes (empty when that side is unbounded or the
    /// set is empty); `bands` holds the full multi-band shape as
    /// `lower:upper` pairs joined with `|`, `*` for an open side.
    pub(crate) fn write_bounds_row(
        &mut self,
        ts: DateTime<Utc>,
        bounds: &VecBounds,
    ) -> std::io::Result<()> {
        fn side(v: Option<f32>) -> String {
            v.map(|x| x.to_string()).unwrap_or_else(|| "*".into())
        }
        let bands = bounds
            .0
            .iter()
            .map(|b| format!("{}:{}", side(b.lower), side(b.upper)))
            .collect::<Vec<_>>()
            .join("|");
        writeln!(
            self.writer,
            "{},{},{},{}",
            ts.to_rfc3339(),
            cell(bounds.0.first().and_then(|b| b.lower)),
            cell(bounds.0.last().and_then(|b| b.upper)),
            bands,
        )
    }
}

fn cell(v: Option<f32>) -> String {
    v.map(|x| x.to_string()).unwrap_or_default()
}

/// Minimal CSV field quoting: empty and plain fields pass through;
/// anything holding a comma, quote, or newline is wrapped in double
/// quotes with inner quotes doubled (RFC 4180).
fn csv_quote(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Bundle of active CSV sinks keyed by component id. Mutated under
/// `MicrogridSite::scenario_csv` lock (and its setpoints / bounds
/// siblings); the lock pattern is "open for the lifetime of the
/// scenario, drop on stop".
pub(crate) type CsvSinks = HashMap<u64, CsvSink>;
