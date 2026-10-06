use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::sim::{Category, MicrogridSite, SimulatedComponent, Telemetry};

pub struct Grid {
    id: u64,
    name: String,
    pub rated_fuse_current: u32,
    pub rated_active_bounds: Option<(f32, f32)>,
    pub stream_jitter_pct: f32,
}

impl Grid {
    pub fn new(
        id: u64,
        rated_fuse_current: u32,
        rated_active_bounds: Option<(f32, f32)>,
        stream_jitter_pct: f32,
    ) -> Self {
        Self {
            id,
            name: format!("grid-{id}"),
            rated_fuse_current,
            rated_active_bounds,
            stream_jitter_pct,
        }
    }
}

impl fmt::Display for Grid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Grid {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        Category::Grid
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn tick(&self, _world: &MicrogridSite, _now: DateTime<Utc>, _dt: Duration) {}
    /// Grid is a topology root; per-phase voltage / frequency reads
    /// belong on the meter directly downstream of it (those are the
    /// fields a real control app subscribes to). Returning only
    /// id + category here keeps the stream lean and matches how
    /// microsim modelled the grid connection point.
    fn telemetry(&self, _world: &MicrogridSite) -> Telemetry {
        Telemetry {
            id: self.id,
            category: Some(Category::Grid),
            ..Default::default()
        }
    }

    fn rated_fuse_current(&self) -> Option<u32> {
        Some(self.rated_fuse_current)
    }

    fn rated_active_bounds(&self) -> Option<(f32, f32)> {
        self.rated_active_bounds
    }

    fn stream_jitter_pct(&self) -> f32 {
        self.stream_jitter_pct
    }

    fn make_fn(&self) -> &'static str {
        "%make-grid-connection-point"
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        let mut kw = Vec::new();
        if self.rated_fuse_current != 0 {
            kw.push((":rated-fuse-current-a", self.rated_fuse_current.to_string()));
        }
        if let Some((l, u)) = self.rated_active_bounds {
            kw.push((":rated-lower-w", crate::lisp::lisp_float32(l)));
            kw.push((":rated-upper-w", crate::lisp::lisp_float32(u)));
        }
        if self.stream_jitter_pct != 0.0 {
            kw.push((
                ":stream-jitter-pct",
                crate::lisp::lisp_float32(self.stream_jitter_pct),
            ));
        }
        kw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimulatedComponent;

    /// All-defaults grid renders no kwargs at all — the loader's
    /// own defaults reconstruct the same state.
    #[test]
    fn constructor_kwargs_empty_for_defaults() {
        let g = Grid::new(1, 0, None, 0.0);
        assert_eq!(g.make_fn(), "%make-grid-connection-point");
        assert!(g.constructor_kwargs().is_empty());
    }

    /// Every non-default field round-trips into its kwarg.
    #[test]
    fn constructor_kwargs_round_trip_grid() {
        let g = Grid::new(2, 200, Some((-50_000.0, 50_000.0)), 2.5);
        let kw = g.constructor_kwargs();
        let s = kw
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(s.contains(":rated-fuse-current-a 200"));
        assert!(s.contains(":rated-lower-w -50000.0"));
        assert!(s.contains(":rated-upper-w 50000.0"));
        assert!(s.contains(":stream-jitter-pct 2.5"));
    }
}
