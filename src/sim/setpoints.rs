//! Per-component log of incoming setpoint requests + their outcome —
//! the data behind the UI's control inspector. A real production
//! microgrid sees control apps issuing constant SetActivePower /
//! SetReactivePower / AugmentBounds requests; an engineer evaluating
//! such a control app wants to see, for any given moment, *what was
//! requested* and *what the sim did with it*. This module keeps a
//! bounded ring per component holding exactly that.
//!
//! Wired into `MicrogridSite` and populated by the gRPC server's
//! setpoint handlers. Pure data structures here.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetpointKind {
    ActivePower,
    ReactivePower,
    /// An *active*-power bounds augmentation. Keeps its original
    /// `augment_bounds` wire value — the reactive route got a new
    /// variant rather than a shared one, so nothing an existing
    /// consumer already reads changes meaning.
    AugmentBounds,
    /// A *reactive*-power bounds augmentation. The two augment routes
    /// journal distinct kinds so `component/{cid}/setpoints`, the
    /// event bus and the setpoints CSV keep the axis the request was
    /// aimed at.
    AugmentReactiveBounds,
}

impl SetpointKind {
    /// Stable snake_case label, matching the Serialize rename — used
    /// by the event bus and the scenario setpoints CSV.
    pub fn as_str(self) -> &'static str {
        match self {
            SetpointKind::ActivePower => "active_power",
            SetpointKind::ReactivePower => "reactive_power",
            SetpointKind::AugmentBounds => "augment_bounds",
            SetpointKind::AugmentReactiveBounds => "augment_reactive_bounds",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SetpointOutcome {
    /// Request applied as asked. `effective_value` is the value the
    /// component now tracks toward (== requested for active/reactive
    /// power; absent for augment-bounds).
    Accepted { effective_value: Option<f32> },
    /// Request rejected before reaching the component (out-of-bounds,
    /// component unhealthy, etc.). `reason` is the gRPC error message
    /// the client would see.
    Rejected { reason: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct SetpointEvent {
    /// Wall-clock timestamp of the inbound request.
    pub ts: DateTime<Utc>,
    pub kind: SetpointKind,
    /// Requested value (W for active, VAR for reactive). For
    /// augment-bounds `value` is unused / 0; the bounds shape itself
    /// isn't logged at this granularity for v1.
    pub value: f32,
    /// Resolved request lifetime in seconds — how long the setpoint /
    /// augmentation holds before expiry resets it. The *resolved*
    /// value (request's own lifetime, or the configured default when
    /// absent), so a replay sees what actually took effect. `None`
    /// when the request never reached lifetime resolution (e.g.
    /// rejected for an out-of-range lifetime).
    pub ttl_s: Option<u64>,
    pub outcome: SetpointOutcome,
}

/// Bounded ring of recent setpoint events for one component.
#[derive(Debug)]
pub struct SetpointLog {
    capacity: usize,
    ring: VecDeque<SetpointEvent>,
}

impl SetpointLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ring: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, event: SetpointEvent) {
        // `>=` not `==`: a capacity of 0 would never satisfy `== 0` after
        // the first push, growing the ring without bound.
        if self.ring.len() >= self.capacity {
            self.ring.pop_front();
        }
        self.ring.push_back(event);
    }

    pub fn iter_window(&self, since: DateTime<Utc>) -> impl Iterator<Item = &SetpointEvent> {
        // Ring is monotonic-time-ordered (push at the tail with
        // ts == "now"), same shape as History — partition_point
        // finds the first event at-or-after `since` in O(log n).
        let cut = self.ring.partition_point(|e| e.ts < since);
        self.ring.iter().skip(cut)
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &SetpointEvent> {
        self.ring.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn ev(secs: i64, kind: SetpointKind, value: f32, accepted: bool) -> SetpointEvent {
        SetpointEvent {
            ts: t(secs),
            kind,
            value,
            ttl_s: Some(60),
            outcome: if accepted {
                SetpointOutcome::Accepted {
                    effective_value: Some(value),
                }
            } else {
                SetpointOutcome::Rejected {
                    reason: "out of bounds".into(),
                }
            },
        }
    }

    #[test]
    fn push_evicts_oldest_when_full() {
        let mut log = SetpointLog::new(3);
        for i in 1..=4 {
            log.push(ev(i, SetpointKind::ActivePower, i as f32 * 100.0, true));
        }
        assert_eq!(log.len(), 3);
        let values: Vec<_> = log.iter().map(|e| e.value).collect();
        assert_eq!(values, vec![200.0, 300.0, 400.0]);
    }

    #[test]
    fn iter_window_is_inclusive_of_since() {
        let mut log = SetpointLog::new(10);
        for i in 1..=5 {
            log.push(ev(i, SetpointKind::ActivePower, i as f32, true));
        }
        let recent: Vec<_> = log.iter_window(t(3)).map(|e| e.value).collect();
        assert_eq!(recent, vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn outcomes_serialize_with_kind_tag() {
        let accepted = SetpointOutcome::Accepted {
            effective_value: Some(2500.0),
        };
        let rejected = SetpointOutcome::Rejected {
            reason: "ouch".into(),
        };
        assert!(
            serde_json::to_string(&accepted)
                .unwrap()
                .contains("\"kind\":\"accepted\"")
        );
        assert!(
            serde_json::to_string(&rejected)
                .unwrap()
                .contains("\"kind\":\"rejected\"")
        );
    }
}
