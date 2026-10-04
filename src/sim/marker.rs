//! Marker components: CHP, wind turbine, power transformer, breaker.
//! They carry no physics of their own — they exist so the topology is
//! complete and so the formula engine can classify the meters around
//! them (a meter feeding a wind turbine is a wind meter). Power is
//! set directly on the neighboring meter via `(set-meter-power …)`.
//! Whatever flows through a marker is its children's, so a meter
//! above a transformer or breaker still sums the branch below it.

use std::{fmt, time::Duration};

use chrono::{DateTime, Utc};

use crate::sim::{Category, MicrogridSite, SimulatedComponent, Telemetry};

pub struct Marker {
    id: u64,
    name: String,
    category: Category,
    stream_jitter_pct: f32,
}

impl Marker {
    pub fn new(id: u64, category: Category, stream_jitter_pct: f32) -> Self {
        let prefix = match category {
            Category::Chp => "chp",
            Category::WindTurbine => "wind",
            Category::PowerTransformer => "transformer",
            Category::Breaker => "breaker",
            // The constructor is only reached from the %make-*
            // marker forms; anything else is a programming error.
            other => unreachable!("Marker::new with non-marker category {other:?}"),
        };
        Self {
            id,
            name: format!("{prefix}-{id}"),
            category,
            stream_jitter_pct,
        }
    }
}

impl fmt::Display for Marker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl SimulatedComponent for Marker {
    fn id(&self) -> u64 {
        self.id
    }
    fn category(&self) -> Category {
        self.category
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn stream_interval(&self) -> Duration {
        Duration::from_secs(1)
    }
    fn stream_jitter_pct(&self) -> f32 {
        self.stream_jitter_pct
    }
    fn tick(&self, _w: &MicrogridSite, _n: DateTime<Utc>, _d: Duration) {}
    fn telemetry(&self, _w: &MicrogridSite) -> Telemetry {
        Telemetry {
            id: self.id,
            category: Some(self.category),
            ..Default::default()
        }
    }

    fn aggregate_power_w(&self, site: &MicrogridSite) -> f32 {
        site.sum_children(self.id, |c| c.aggregate_power_w(site))
    }

    fn aggregate_reactive_var(&self, site: &MicrogridSite) -> f32 {
        site.sum_children(self.id, |c| c.aggregate_reactive_var(site))
    }

    fn make_fn(&self) -> &'static str {
        match self.category {
            Category::Chp => "%make-chp",
            Category::WindTurbine => "%make-wind-turbine",
            Category::PowerTransformer => "%make-power-transformer",
            Category::Breaker => "%make-breaker",
            other => unreachable!("Marker::make_fn with non-marker category {other:?}"),
        }
    }

    fn constructor_kwargs(&self) -> Vec<(&'static str, String)> {
        let mut kw = Vec::new();
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

    /// `make_fn` maps every marker category to its `%make-*`
    /// primitive; `:stream-jitter-pct` renders only when non-zero.
    #[test]
    fn constructor_kwargs_and_make_fn_by_category() {
        let m = Marker::new(1, Category::Chp, 3.0);
        assert_eq!(m.make_fn(), "%make-chp");
        let s = m
            .constructor_kwargs()
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(s.contains(":stream-jitter-pct 3.0"));

        assert_eq!(
            Marker::new(2, Category::WindTurbine, 0.0).make_fn(),
            "%make-wind-turbine"
        );
        assert_eq!(
            Marker::new(4, Category::PowerTransformer, 0.0).make_fn(),
            "%make-power-transformer"
        );
        assert_eq!(
            Marker::new(5, Category::Breaker, 0.0).make_fn(),
            "%make-breaker"
        );
        assert!(
            Marker::new(2, Category::WindTurbine, 0.0)
                .constructor_kwargs()
                .is_empty()
        );
    }
}
