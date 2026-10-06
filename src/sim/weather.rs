//! Site-wide weather: a parametric clear-sky day plus discrete cloud
//! events layered on top.
//!
//! ```text
//! site Weather:  clear_sky(t) x cloud_attenuation(t)  ->  site sunlight%
//! ```
//!
//! Cloud cover comes from two producers that both just push
//! [`CloudEvent`]s onto the same list: an ambient Poisson-arrival
//! generator (driven by [`Weather::advance`], when
//! `cloud_rate_per_h` is configured) and a scripted door
//! ([`Weather::pass_cloud`]) for scenarios and the weather panel.
//! [`Weather::pct_at`] multiplies every event's transmission
//! together, so overlapping clouds compound.
//!
//! Evaluation is a pure function of `(config, event list)` — times
//! are UTC throughout, and everything below is stated in UTC.

use std::time::Duration;

use chrono::{DateTime, Timelike, Utc};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::sim::{MicrogridSite, sim_clock::parse_time_of_day};

/// How long an expired [`CloudEvent`] is kept in the list after it
/// ends, so a lagged reader (an inverter reading `pct_at(now -
/// lag)`) still finds it. Also the bound `advance` measures a forward
/// clock jump against: `advance` is called once per tick (100 ms
/// live; dt-sized steps in a stepped scenario run), so any single
/// call spanning more than an hour is a clock anomaly (suspended VM,
/// wall-clock correction), not real elapsed simulation time — instead
/// of grinding through a potentially huge ambient backlog one arrival
/// at a time, `advance` just re-anchors to `now`.
fn retention_margin() -> chrono::Duration {
    chrono::Duration::hours(1)
}

/// `Duration::from_secs_f32` panics on a negative, NaN, or overflowing
/// input. The doors validate their inputs (`validate::secs_range`,
/// `validate::cloud_rate`) before they ever reach here, but `Weather`
/// itself has no way to enforce that a caller went through a door —
/// direct construction (tests, a future embedder) can hand it anything
/// — so every conversion on the `advance` hot path saturates to `cap`
/// instead of trusting the door. A degraded cloud is a wrong number;
/// a panicked physics task is the whole site going dark.
fn saturating_secs_to_duration(v: f32, cap: Duration) -> Duration {
    Duration::try_from_secs_f32(v).unwrap_or(cap)
}

/// Config for one [`Weather`] instance: the clear-sky window and the
/// ambient cloud generator's rates and ranges.
#[derive(Clone, Debug)]
pub struct WeatherConfig {
    /// Time of day (UTC) the clear-sky curve turns on.
    pub sunrise: Duration,
    /// Time of day (UTC) the clear-sky curve turns off.
    pub sunset: Duration,
    /// Clear-sky output at solar noon, in percent.
    pub peak_pct: f32,
    /// Ambient cloud arrival rate, events per hour. `None` disables
    /// the ambient generator — only scripted [`Weather::pass_cloud`]
    /// events appear.
    pub cloud_rate_per_h: Option<f32>,
    /// Uniform (min, max) range an ambient cloud's depth is drawn
    /// from, in percent.
    pub cloud_depth: (f32, f32),
    /// Uniform (min, max) range an ambient cloud's total duration is
    /// drawn from, in seconds.
    pub cloud_duration: (f32, f32),
    /// Uniform (min, max) range an ambient cloud's ramp-in/ramp-out
    /// time is drawn from, in seconds.
    pub cloud_ramp: (f32, f32),
    /// Seed for the ambient generator's RNG. `Some` makes the event
    /// stream reproducible run to run; `None` seeds from entropy.
    pub seed: Option<u64>,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            sunrise: parse_time_of_day("06:00").expect("valid literal"),
            sunset: parse_time_of_day("20:00").expect("valid literal"),
            peak_pct: 100.0,
            cloud_rate_per_h: None,
            cloud_depth: (20.0, 70.0),
            cloud_duration: (60.0, 600.0),
            cloud_ramp: (10.0, 60.0),
            seed: None,
        }
    }
}

/// One discrete cloud passing overhead. Its attenuation envelope is a
/// trapezoid: ramps linearly up to `depth_pct` over `ramp_in`, holds
/// for `plateau`, then ramps back down over `ramp_out`. Outside
/// `[start, end())` it contributes no attenuation at all.
#[derive(Clone, Debug)]
pub struct CloudEvent {
    /// When the cloud begins arriving (the start of `ramp_in`), UTC.
    pub start: DateTime<Utc>,
    /// How long attenuation takes to ramp linearly from zero up to
    /// `depth_pct`, right after `start`.
    pub ramp_in: Duration,
    /// How long attenuation holds steady at `depth_pct`, between
    /// `ramp_in` and `ramp_out`.
    pub plateau: Duration,
    /// How long attenuation takes to ramp linearly back down to zero,
    /// right after the plateau. The event ends (see [`Self::end`])
    /// once this finishes.
    pub ramp_out: Duration,
    /// Attenuation depth at the plateau, in percent (0..=100).
    pub depth_pct: f32,
}

impl CloudEvent {
    /// When the event's attenuation returns to zero.
    pub fn end(&self) -> DateTime<Utc> {
        let span = self.ramp_in + self.plateau + self.ramp_out;
        self.start + chrono::Duration::from_std(span).unwrap_or(chrono::Duration::zero())
    }

    /// Fractional attenuation (`0..=depth_pct/100`) at `t`.
    pub fn attenuation_at(&self, t: DateTime<Utc>) -> f32 {
        if t < self.start || t >= self.end() {
            return 0.0;
        }
        let depth = (self.depth_pct / 100.0).clamp(0.0, 1.0);
        let elapsed = (t - self.start).to_std().unwrap_or(Duration::ZERO);
        if elapsed < self.ramp_in {
            let frac = elapsed.as_secs_f32() / self.ramp_in.as_secs_f32().max(f32::EPSILON);
            depth * frac
        } else if elapsed < self.ramp_in + self.plateau {
            depth
        } else {
            let into_ramp_out = elapsed - (self.ramp_in + self.plateau);
            let frac = into_ramp_out.as_secs_f32() / self.ramp_out.as_secs_f32().max(f32::EPSILON);
            depth * (1.0 - frac)
        }
    }
}

/// Site-wide sunlight model: a clear-sky curve modulated by whatever
/// cloud events are currently in the list. Not `Clone` — it owns an
/// RNG stream that shouldn't be duplicated.
pub struct Weather {
    cfg: WeatherConfig,
    events: Vec<CloudEvent>,
    rng: SmallRng,
    /// The last `now` passed to `advance` (or set implicitly by
    /// `pass_cloud`'s fallback) — the reference point new scripted
    /// events start from, and the watermark `advance` measures clock
    /// steps against.
    anchor: Option<DateTime<Utc>>,
    /// When the ambient generator's next arrival lands, once primed.
    next_arrival: Option<DateTime<Utc>>,
}

impl Weather {
    pub fn new(cfg: WeatherConfig) -> Self {
        let rng = match cfg.seed {
            Some(seed) => SmallRng::seed_from_u64(seed),
            None => SmallRng::from_entropy(),
        };
        Self {
            cfg,
            events: Vec::new(),
            rng,
            anchor: None,
            next_arrival: None,
        }
    }

    pub fn config(&self) -> &WeatherConfig {
        &self.cfg
    }

    pub fn config_mut(&mut self) -> &mut WeatherConfig {
        &mut self.cfg
    }

    pub fn events(&self) -> &[CloudEvent] {
        &self.events
    }

    pub fn anchor(&self) -> Option<DateTime<Utc>> {
        self.anchor
    }

    /// Clear-sky output at `t`: zero outside `[sunrise, sunset]`,
    /// else a sine arch peaking at `peak_pct` at solar noon (the
    /// window's midpoint).
    pub fn clear_sky_pct(&self, t: DateTime<Utc>) -> f32 {
        let secs = t.time().num_seconds_from_midnight() as u64;
        let sunrise = self.cfg.sunrise.as_secs();
        let sunset = self.cfg.sunset.as_secs();
        if secs < sunrise || secs > sunset || sunset <= sunrise {
            return 0.0;
        }
        let day_len = (sunset - sunrise) as f32;
        let frac = (secs - sunrise) as f32 / day_len;
        self.cfg.peak_pct * (std::f32::consts::PI * frac).sin()
    }

    /// Sunlight at `t`: clear-sky attenuated by every currently
    /// tracked cloud event, multiplied together (`Π (1 -
    /// attenuation_i(t))`), so overlapping clouds compound.
    pub fn pct_at(&self, t: DateTime<Utc>) -> f32 {
        let transmission: f32 = self
            .events
            .iter()
            .map(|e| 1.0 - e.attenuation_at(t))
            .product();
        self.clear_sky_pct(t) * transmission
    }

    /// Draw one exponential inter-arrival duration for a Poisson
    /// process at `rate` events/hour.
    fn exp_sample(rng: &mut SmallRng, rate: f32) -> Duration {
        let u: f32 = rng.gen_range(1e-6..1.0f32);
        // Capped at the re-anchor bound: a gap that long is already
        // past what `advance` will ever walk one arrival at a time
        // (it re-anchors instead), so there is no behavioral
        // difference between this and the true, unrepresentable gap —
        // just the difference between saturating and panicking.
        saturating_secs_to_duration(
            -u.ln() / rate * 3600.0,
            retention_margin()
                .to_std()
                .unwrap_or(Duration::from_secs(3600)),
        )
    }

    /// Draw a value uniformly from `(lo, hi)` (order-independent).
    fn uniform(rng: &mut SmallRng, range: (f32, f32)) -> f32 {
        let (lo, hi) = (range.0.min(range.1), range.0.max(range.1));
        if hi <= lo { lo } else { rng.gen_range(lo..hi) }
    }

    /// Materialize ambient cloud events (if `cloud_rate_per_h` is
    /// configured) up to `now`, then prune events that expired more
    /// than an hour ago.
    ///
    /// Tolerates clock steps: a backward `now` (an NTP correction)
    /// is a no-op that leaves the anchor untouched. A forward jump
    /// past [`retention_margin`] (a suspended VM resuming) re-anchors
    /// to `now` instead of grinding through a potentially huge
    /// ambient backlog one arrival at a time.
    pub fn advance(&mut self, now: DateTime<Utc>) {
        let mut base = self.anchor.unwrap_or(now);
        if let Some(a) = self.anchor {
            if now < a {
                return;
            }
            if now - a > retention_margin() {
                self.next_arrival = None;
                base = now;
            }
        }
        if let Some(rate) = self.cfg.cloud_rate_per_h
            && rate > 0.0
        {
            if self.next_arrival.is_none() {
                self.next_arrival = Some(base + Self::exp_sample(&mut self.rng, rate));
            }
            while let Some(arrival) = self.next_arrival {
                if arrival > now {
                    break;
                }
                let depth = Self::uniform(&mut self.rng, self.cfg.cloud_depth).clamp(0.0, 100.0);
                let duration_s = Self::uniform(&mut self.rng, self.cfg.cloud_duration).max(0.0);
                let ramp_s = Self::uniform(&mut self.rng, self.cfg.cloud_ramp).max(0.0);
                // Capped at a day: a cloud longer than that is already
                // outside anything `validate::secs_range` will admit
                // through a door, so a direct (unvalidated) caller
                // just gets clamped physics instead of a killed tick.
                let duration = saturating_secs_to_duration(duration_s, Duration::from_secs(86_400));
                let ramp = saturating_secs_to_duration(ramp_s, Duration::from_secs(86_400));
                let plateau = duration.saturating_sub(ramp.saturating_mul(2));
                self.events.push(CloudEvent {
                    start: arrival,
                    ramp_in: ramp,
                    plateau,
                    ramp_out: ramp,
                    depth_pct: depth,
                });
                self.next_arrival = Some(arrival + Self::exp_sample(&mut self.rng, rate));
            }
        } else {
            // The ambient generator is off, so its arrival clock stops
            // with it. Keeping a stale `next_arrival` while the anchor
            // kept advancing would make the first `advance` after a
            // re-enable grind the ENTIRE disabled gap through the loop
            // above one arrival at a time, materializing a backlog of
            // clouds that all started in the past. Clearing it
            // re-anchors on the next enabled advance instead — the
            // same thing the >1 h leap above does, for the same reason.
            self.next_arrival = None;
        }
        self.anchor = Some(now);
        let margin = retention_margin();
        self.events.retain(|e| e.end() + margin >= now);
    }

    /// Insert one deterministic, scripted cloud event starting at
    /// the current anchor (or `Utc::now()` if `advance` has never
    /// been called). Used by scenarios and the weather panel — a
    /// scripted cloud is an ordinary event that expires on its own.
    pub fn pass_cloud(&mut self, depth_pct: f32, duration: Duration, ramp: Duration) {
        let start = self.anchor.unwrap_or_else(Utc::now);
        let plateau = duration.saturating_sub(ramp.saturating_mul(2));
        self.events.push(CloudEvent {
            start,
            ramp_in: ramp,
            plateau,
            ramp_out: ramp,
            depth_pct: depth_pct.clamp(0.0, 100.0),
        });
    }
}

/// Pure numeric validators shared by the Lisp weather doors
/// (`src/lisp/defuns/weather.rs`) and the HTTP weather routes
/// (`src/ui/handlers/weather.rs`), so both surfaces enforce
/// identical rules even though each phrases its error text around
/// its own field vocabulary (`:cloud-mean-gap-s` vs `cloud_rate_per_h`).
/// Every fn here is a pure check — no `WeatherConfig`, no site, no
/// error type tied to either caller's framework.
pub mod validate {
    use std::time::Duration;

    /// A cloud longer than a day is a config error, not a real cloud
    /// — and without a cap here, a magnitude like `1e30` would sail
    /// through the sign-only check below and later overflow
    /// `Duration::from_secs_f32` inside `Weather::advance`, on the
    /// physics task, with nothing catching it.
    pub const MAX_SECS: f32 = 86_400.0;

    /// Both ends of a `(lo, hi)` seconds range must be finite and
    /// within `[0, MAX_SECS]` — a NaN would poison every `uniform`
    /// draw, a negative would panic `Duration::from_secs_f32`
    /// downstream, and an absurd magnitude (e.g. `1e30`) would too.
    pub fn secs_range(range: (f32, f32)) -> Result<(), String> {
        if !range.0.is_finite()
            || !range.1.is_finite()
            || range.0 < 0.0
            || range.1 < 0.0
            || range.0 > MAX_SECS
            || range.1 > MAX_SECS
        {
            return Err(format!(
                "must be a non-negative number of seconds no greater than {MAX_SECS} (one day), got ({}, {})",
                range.0, range.1
            ));
        }
        Ok(())
    }

    /// The longest lag a `Follow` solar inverter may read the sky at,
    /// in seconds — the same hour `Weather::advance`'s
    /// `retention_margin` keeps an expired [`CloudEvent`] around for.
    /// Past that the lagged sample lands in pruned history, where
    /// every cloud has already been dropped from the list, so
    /// `pct_at` returns unattenuated clear sky: the array silently
    /// never clouds over, with nothing anywhere to say why.
    ///
    /// [`CloudEvent`]: super::CloudEvent
    pub const MAX_LAG_S: f64 = 3_600.0;

    /// A `Follow` inverter's weather lag: finite, non-negative, and
    /// no longer than the cloud history [`MAX_LAG_S`] bounds. Returns
    /// the `Duration` the caller stores, so the fallible conversion
    /// happens once, here — `Duration::from_secs_f64` PANICS on a
    /// non-finite or overflowing value, which would abort a whole
    /// config load over one typo'd kwarg.
    pub fn weather_lag_s(v: f64) -> Result<Duration, String> {
        if !(v.is_finite() && v >= 0.0) {
            return Err(format!("must be a non-negative number of seconds, got {v}"));
        }
        if v > MAX_LAG_S {
            return Err(format!(
                "must be no more than {MAX_LAG_S} s: cloud events are pruned an hour after they \
                 end, so a longer lag reads a sky with no clouds left in it; got {v}"
            ));
        }
        Duration::try_from_secs_f64(v).map_err(|e| format!("is not a usable duration: {e}"))
    }

    /// A depth percentage range: a negative depth would brighten the
    /// array and >100 would drive `pct_at` negative.
    pub fn depth_range(range: (f32, f32)) -> Result<(), String> {
        if !(0.0..=100.0).contains(&range.0) || !(0.0..=100.0).contains(&range.1) {
            return Err(format!(
                "must be within [0, 100], got ({}, {})",
                range.0, range.1
            ));
        }
        Ok(())
    }

    /// A negative peak inverts the clear-sky arch, which drives
    /// `min_avail` positive — parking every following array at 0
    /// with nothing in telemetry to say why.
    pub fn peak_pct(v: f32) -> Result<(), String> {
        if !(v.is_finite() && v >= 0.0) {
            return Err(format!("must be a non-negative percentage, got {v}"));
        }
        Ok(())
    }

    /// More than one cloud a second is not a sky, it's a typo — and
    /// without an upper bound, a tiny positive rate near the other
    /// end (e.g. `1e-30`) blows up `exp_sample`'s `-ln(u) / rate` the
    /// same way an absurd `:cloud-duration-s` blows up the trapezoid.
    pub const MAX_PER_H: f32 = 3_600.0;

    /// Ambient cloud arrival rate: 0 is the natural "no ambient
    /// clouds" spelling, but a NEGATIVE rate is a mistake, not a
    /// second spelling of "off". Returns the config slot's own
    /// `Option<f32>` shape: `Some` above zero, `None` at zero.
    pub fn cloud_rate(v: f32) -> Result<Option<f32>, String> {
        if !(v.is_finite() && v >= 0.0) {
            return Err(format!("must be a non-negative rate (0 disables), got {v}"));
        }
        if v > MAX_PER_H {
            return Err(format!(
                "must be no more than {MAX_PER_H} (one cloud a second), got {v}"
            ));
        }
        Ok(if v > 0.0 { Some(v) } else { None })
    }

    /// Checked after both land, so a form/request moving the whole
    /// window is judged on the pair it produces rather than the half
    /// set first.
    pub fn sunrise_before_sunset(sunrise: Duration, sunset: Duration) -> Result<(), String> {
        if sunrise >= sunset {
            return Err(format!(
                "sunrise must be before sunset, got {}s and {}s",
                sunrise.as_secs(),
                sunset.as_secs()
            ));
        }
        Ok(())
    }

    /// One scripted cloud's args: depth in range, duration/ramp
    /// non-negative and representable as a `Duration`. Shared by
    /// `(pass-cloud)` and the HTTP `pass_cloud` sub-object.
    pub fn pass_cloud_args(
        depth_pct: f64,
        duration_s: f64,
        ramp_s: f64,
    ) -> Result<(f32, Duration, Duration), String> {
        if !(0.0..=100.0).contains(&depth_pct) {
            return Err(format!("depth must be within [0, 100], got {depth_pct}"));
        }
        if duration_s < 0.0 || ramp_s < 0.0 {
            return Err(format!(
                "duration and ramp must be non-negative, got {duration_s} and {ramp_s}"
            ));
        }
        let duration =
            Duration::try_from_secs_f64(duration_s).map_err(|e| format!("bad duration: {e}"))?;
        let ramp = Duration::try_from_secs_f64(ramp_s).map_err(|e| format!("bad ramp: {e}"))?;
        Ok((depth_pct as f32, duration, ramp))
    }
}

/// Which door a [`WeatherPatch`] arrived through. It changes nothing
/// about what is checked — only how a rejection is worded, since each
/// surface names the same field in its own vocabulary (`:peak-pct` in a
/// Lisp form, `peak_pct` in a JSON body) and a Lisp error also names
/// the defun the author actually typed.
#[derive(Clone, Copy)]
pub enum WeatherDoor<'a> {
    /// A Lisp form, carrying its own name — `"make-weather"` or
    /// `"set-weather"`.
    Lisp(&'a str),
    /// The HTTP weather route.
    Http,
}

impl WeatherDoor<'_> {
    /// How this door introduces an error about one field: the Lisp
    /// kwarg or the JSON field name, followed by the message.
    ///
    /// `with_form` is why there are two spellings rather than one:
    /// the Lisp doors put the form name in front of the checks that
    /// read as being about the form as a whole (the window, `:peak-pct`,
    /// `:cloud-mean-gap-s`) but not in front of the range kwargs. That
    /// split is historical, and is kept so no existing message moves.
    fn label(&self, kw: &str, json: &str, with_form: bool) -> String {
        match self {
            Self::Lisp(form) if with_form => format!("{form}: {kw}"),
            Self::Lisp(_) => kw.to_string(),
            Self::Http => format!("{json}:"),
        }
    }
}

/// One update to a [`WeatherConfig`]: every field optional, already
/// parsed out of whatever wire shape it arrived in.
///
/// Both weather doors — the Lisp `(make-weather)` / `(set-weather)`
/// forms in `src/lisp/defuns/weather.rs` and `POST
/// /api/mg/{mg}/weather` in `src/ui/handlers/weather.rs` — carry the
/// same fields, fold them in the same order, run the same checks, and
/// share the same "a rejected request changes nothing" contract. So
/// each door only parses its own spelling (an `"HH:MM"` string or
/// bare seconds and a number-or-`(lo hi)` range from Lisp; JSON
/// strings and `[lo, hi]` pairs over HTTP) into this struct.
/// Everything past that point — the fold order, the validation, the
/// sunrise/sunset pair check and the create-or-update install
/// decision — lives here, written once.
#[derive(Default)]
pub struct WeatherPatch {
    /// Time of day (UTC) the clear-sky curve turns on.
    pub sunrise: Option<Duration>,
    /// Time of day (UTC) the clear-sky curve turns off.
    pub sunset: Option<Duration>,
    /// Clear-sky output at solar noon, percent.
    pub peak_pct: Option<f32>,
    /// Ambient cloud arrival rate as the caller wrote it, before
    /// [`validate::cloud_rate`] turns it into the config's own
    /// `Option` shape (where zero means "off").
    pub cloud_rate_per_h: Option<f32>,
    /// Ambient cloud depth `(lo, hi)`, percent.
    pub cloud_depth: Option<(f32, f32)>,
    /// Ambient cloud total duration `(lo, hi)`, seconds.
    pub cloud_duration: Option<(f32, f32)>,
    /// Ambient cloud ramp-in/ramp-out `(lo, hi)`, seconds.
    pub cloud_ramp: Option<(f32, f32)>,
    /// Ambient generator seed. Setting it REBUILDS the weather — see
    /// [`Self::install`]. The HTTP door has no field for it.
    pub seed: Option<u64>,
}

impl WeatherPatch {
    /// Fold this patch into `cfg`, validating as it goes. `cfg` is
    /// the caller's working copy, so a rejected patch leaves the
    /// site's live weather untouched. Errors are worded in `door`'s
    /// own field vocabulary.
    pub fn apply_to(&self, cfg: &mut WeatherConfig, door: WeatherDoor<'_>) -> Result<(), String> {
        if let Some(v) = self.sunrise {
            cfg.sunrise = v;
        }
        if let Some(v) = self.sunset {
            cfg.sunset = v;
        }
        // Checked after both land, so a patch moving the whole window
        // ("06:00"→"21:00" over a 05:00–20:00 config) is judged on the
        // pair it produces rather than on the half it happens to set
        // first.
        let at = door.label(":sunrise/:sunset —", "sunrise/sunset", true);
        validate::sunrise_before_sunset(cfg.sunrise, cfg.sunset)
            .map_err(|e| format!("{at} {e}"))?;
        if let Some(v) = self.peak_pct {
            // A negative peak inverts the clear-sky arch, which drives
            // `min_avail` positive — the band collapses and every
            // following array parks at 0 instead of generating, with
            // nothing in the telemetry to say why.
            let at = door.label(":peak-pct", "peak_pct", true);
            validate::peak_pct(v).map_err(|e| format!("{at} {e}"))?;
            cfg.peak_pct = v;
        }
        if let Some(v) = self.cloud_rate_per_h {
            // 0 is the natural "no ambient clouds" spelling from Lisp,
            // where `None` has no keyword of its own — but a NEGATIVE
            // rate is a mistake, not a second spelling of "off", so it
            // says so rather than silently disabling the generator.
            let at = door.label(":cloud-mean-gap-s", "cloud_rate_per_h", true);
            cfg.cloud_rate_per_h = validate::cloud_rate(v).map_err(|e| format!("{at} {e}"))?;
        }
        if let Some(range) = self.cloud_depth {
            let at = door.label(":cloud-depth-pct", "cloud_depth", false);
            validate::depth_range(range).map_err(|e| format!("{at} {e}"))?;
            cfg.cloud_depth = range;
        }
        if let Some(range) = self.cloud_duration {
            let at = door.label(":cloud-duration-s", "cloud_duration", false);
            validate::secs_range(range).map_err(|e| format!("{at} {e}"))?;
            cfg.cloud_duration = range;
        }
        if let Some(range) = self.cloud_ramp {
            let at = door.label(":cloud-ramp-s", "cloud_ramp", false);
            validate::secs_range(range).map_err(|e| format!("{at} {e}"))?;
            cfg.cloud_ramp = range;
        }
        if let Some(seed) = self.seed {
            cfg.seed = Some(seed);
        }
        Ok(())
    }

    /// Fold this patch into the site's weather and install the
    /// result. A PARTIAL update of what is already there — only the
    /// fields the patch carries move, and the event list and anchor
    /// survive — or a fresh [`Weather`] over
    /// [`WeatherConfig::default`] when the site has no weather yet,
    /// which is what makes `(set-weather :cloud-mean-gap-s 600)` (and its
    /// HTTP twin) a valid way in on a fresh site.
    ///
    /// A seed is the exception: an RNG cannot be re-seeded in place
    /// without disturbing the stream, so passing one rebuilds the
    /// `Weather` outright — a deliberate reset of the ambient event
    /// stream, which is exactly what "re-seed" means to a scenario
    /// that wants reproducibility from here on.
    ///
    /// Nothing is written until validation passes, so a rejected
    /// patch leaves the live weather exactly as it was.
    ///
    /// This form reads the site itself, which is what the Lisp doors
    /// want — nothing has looked yet when `(set-weather …)` runs. A
    /// caller that has ALREADY looked must hand that reading to
    /// [`Self::install_over`] instead of letting this take a second,
    /// possibly different one.
    pub fn install(&self, site: &MicrogridSite, door: WeatherDoor<'_>) -> Result<(), String> {
        let existing = site.with_weather(|w| w.config().clone());
        self.install_over(site, door, existing)
    }

    /// [`Self::install`] against a reading the caller already took:
    /// `existing` is that caller's own look at the site's weather
    /// config, `None` meaning it found none. It decides create versus
    /// update AND supplies the base the patch folds over, so both
    /// come from the same observation.
    ///
    /// Passing the reading in rather than taking a fresh one is the
    /// whole point. The HTTP door has to look before it installs, to
    /// turn away a cloud-only body; if a concurrent `reset()` — a hot
    /// reload, or `(reset-microgrid)` — clears the slot in between,
    /// installing against what the caller SAW takes the update arm,
    /// `with_weather` no-ops on the now-empty slot, and the request
    /// lands nowhere. That is correct: a reset's clear is scoped to
    /// the run, and the request it interrupted reports the conflict
    /// rather than undoing it. Re-probing here would find no weather,
    /// take the create arm, and resurrect the sky the reset had just
    /// cleared.
    ///
    /// A seed re-seeds by rebuilding the `Weather` outright, but that
    /// rebuild obeys the same rule: on the update arm it goes in
    /// through `with_weather`, replacing a live sky in place and
    /// no-opping on a slot a reset has emptied. Only the create arm —
    /// the caller genuinely saw no weather — installs into an empty
    /// slot.
    pub fn install_over(
        &self,
        site: &MicrogridSite,
        door: WeatherDoor<'_>,
        existing: Option<WeatherConfig>,
    ) -> Result<(), String> {
        let existed = existing.is_some();
        let mut cfg = existing.unwrap_or_default();
        self.apply_to(&mut cfg, door)?;
        if !existed {
            site.set_weather(Some(Weather::new(cfg)));
        } else if self.seed.is_some() {
            // An RNG cannot be re-seeded in place without disturbing
            // the stream, so this replaces the whole `Weather` — but
            // through the occupied slot, so a reset that landed since
            // the caller's reading is not undone by it.
            site.with_weather(|w| *w = Weather::new(cfg));
        } else {
            site.with_weather(|w| *w.config_mut() = cfg);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, h, m, 0).unwrap()
    }

    /// Clear sky: zero outside daylight, peak at solar noon, sine
    /// in between (sin(π/4) ≈ 0.7071 a quarter-day in).
    #[test]
    fn clear_sky_is_a_sine_between_sunrise_and_sunset() {
        let w = Weather::new(WeatherConfig::default()); // 06:00–20:00, peak 100
        assert_eq!(w.clear_sky_pct(at(3, 0)), 0.0);
        assert_eq!(w.clear_sky_pct(at(22, 0)), 0.0);
        assert_eq!(w.clear_sky_pct(at(6, 0)), 0.0);
        assert!(
            (w.clear_sky_pct(at(13, 0)) - 100.0).abs() < 0.01,
            "solar noon"
        );
        let quarter = w.clear_sky_pct(at(9, 30)); // 3.5 h of 14 h in
        assert!((quarter - 100.0 * (std::f32::consts::PI * 0.25).sin()).abs() < 0.01);
    }

    /// A cloud's attenuation ramps linearly to depth, holds, ramps out.
    #[test]
    fn cloud_event_is_a_trapezoid() {
        let e = CloudEvent {
            start: at(12, 0),
            ramp_in: Duration::from_secs(60),
            plateau: Duration::from_secs(120),
            ramp_out: Duration::from_secs(60),
            depth_pct: 50.0,
        };
        assert_eq!(e.attenuation_at(at(11, 59)), 0.0);
        assert!((e.attenuation_at(at(12, 0) + chrono::Duration::seconds(30)) - 0.25).abs() < 1e-4);
        assert!((e.attenuation_at(at(12, 2)) - 0.5).abs() < 1e-4); // plateau
        assert_eq!(
            e.attenuation_at(e.end() + chrono::Duration::seconds(1)),
            0.0
        );
    }

    /// Overlapping clouds multiply transmissions: 50% and 40% deep
    /// clouds together leave 0.5 × 0.6 = 30% of clear sky.
    #[test]
    fn overlapping_clouds_multiply() {
        let mut w = Weather::new(WeatherConfig::default());
        w.advance(at(13, 0));
        w.pass_cloud(50.0, Duration::from_secs(600), Duration::ZERO);
        w.pass_cloud(40.0, Duration::from_secs(600), Duration::ZERO);
        let clear = w.clear_sky_pct(at(13, 1));
        assert!((w.pct_at(at(13, 1)) - clear * 0.5 * 0.6).abs() < 0.01);
    }

    /// Same seed ⇒ same ambient event stream; different seeds diverge.
    /// Steps in 5-minute increments — matching real usage, where
    /// `advance` is called once per tick, not in one giant jump.
    #[test]
    fn seeded_generator_is_reproducible() {
        let mk = |seed| {
            let mut w = Weather::new(WeatherConfig {
                cloud_rate_per_h: Some(30.0),
                seed,
                ..Default::default()
            });
            let mut t = at(10, 0);
            let end = at(14, 0);
            while t <= end {
                w.advance(t);
                t += chrono::Duration::minutes(5);
            }
            w.events()
                .iter()
                .map(|e| (e.start, e.depth_pct))
                .collect::<Vec<_>>()
        };
        assert_eq!(mk(Some(7)), mk(Some(7)));
        assert!(!mk(Some(7)).is_empty(), "4 h at 30/h must produce events");
        assert_ne!(mk(Some(7)), mk(Some(8)));
    }

    /// Clock tolerance: a backward `now` generates nothing and keeps
    /// the anchor; a forward leap past the bound re-anchors instead
    /// of materializing hours of events.
    #[test]
    fn generator_tolerates_clock_steps() {
        let mut w = Weather::new(WeatherConfig {
            cloud_rate_per_h: Some(60.0),
            seed: Some(1),
            ..Default::default()
        });
        w.advance(at(12, 0));
        let anchor = w.anchor();
        w.advance(at(11, 0)); // NTP step backward
        assert_eq!(w.anchor(), anchor, "backward now must not move the anchor");
        let before = w.events().len();
        w.advance(at(12, 0) + chrono::Duration::hours(6)); // suspended VM resumes
        let gained = w.events().len() - before;
        assert_eq!(gained, 0, "a >1 h leap re-anchors instead of backfilling");
    }

    /// Turning `cloud_rate_per_h` off stops the arrival clock with
    /// it. Without that, `next_arrival` stays pinned at the moment
    /// the rate went off while the anchor keeps advancing, and the
    /// first `advance` after a re-enable walks the whole disabled gap
    /// one arrival at a time — a day off at 30/h is ~720 loop
    /// iterations, one cloud each, all started back in the gap (most
    /// then immediately pruned, so the only visible trace is a burst
    /// of retroactive attenuation).
    #[test]
    fn disabling_the_cloud_rate_re_anchors_the_next_arrival() {
        let mut w = Weather::new(WeatherConfig {
            cloud_rate_per_h: Some(30.0),
            seed: Some(3),
            ..Default::default()
        });
        let start = at(10, 0);
        w.advance(start);
        w.advance(start + chrono::Duration::minutes(10));

        // Rate off, then a simulated day of ordinary ticks — each one
        // well inside the re-anchor bound, so nothing else clears
        // `next_arrival` on the way through.
        w.config_mut().cloud_rate_per_h = None;
        let gap_end = start + chrono::Duration::days(1);
        let mut t = start + chrono::Duration::minutes(15);
        while t <= gap_end {
            w.advance(t);
            t += chrono::Duration::minutes(5);
        }
        assert!(
            w.events().is_empty(),
            "a disabled rate produces no clouds at all"
        );

        // Re-enabled: one advance re-anchors rather than backfilling.
        w.config_mut().cloud_rate_per_h = Some(30.0);
        let resume = gap_end + chrono::Duration::minutes(1);
        w.advance(resume);
        assert!(
            w.events().len() <= 10,
            "re-enabling must not backfill the disabled gap, got {} events",
            w.events().len()
        );
        for e in w.events() {
            assert!(
                e.start >= gap_end,
                "no arrival may land back in the disabled gap: {} < {gap_end}",
                e.start
            );
        }
    }

    /// Expired events are pruned, but only past the 1 h lag margin.
    #[test]
    fn events_prune_past_the_lag_margin() {
        let mut w = Weather::new(WeatherConfig::default());
        w.advance(at(10, 0));
        w.pass_cloud(50.0, Duration::from_secs(60), Duration::ZERO);
        w.advance(at(10, 30));
        assert_eq!(w.events().len(), 1, "expired but inside the margin");
        w.advance(at(12, 0));
        assert!(w.events().is_empty(), "past the margin");
    }

    /// The create-or-update decision belongs to the caller's own
    /// reading of the site, not to a fresh probe inside the install:
    /// a `reset()` landing mid-request must not be undone by the very
    /// request it interrupted. This stands in for that race
    /// deterministically — install over an `existing` that says
    /// "there was weather" onto a site that no longer has any, which
    /// is exactly the state a concurrent `reset()` leaves behind. The
    /// update arm has to win: `with_weather` no-ops on the empty
    /// slot, nothing is written, and the HTTP door's missing snapshot
    /// becomes its 409. Re-probing inside `install_over` would take
    /// the create arm here and answer 200 over a resurrected sky.
    #[test]
    fn install_over_a_stale_reading_does_not_resurrect_cleared_weather() {
        let site = MicrogridSite::new();
        let patch = WeatherPatch {
            peak_pct: Some(80.0),
            ..Default::default()
        };

        // What the caller saw before the reset cleared the slot.
        let seen_before_the_reset = Some(WeatherConfig::default());
        patch
            .install_over(&site, WeatherDoor::Http, seen_before_the_reset.clone())
            .expect("a valid patch");
        assert!(
            site.with_weather(|_| ()).is_none(),
            "the reset's cleared weather must stay cleared",
        );

        // A seed rebuilds the `Weather` outright rather than editing
        // it in place, which is the one arm that could still reach
        // for `set_weather` and put a sky back. It must not: the
        // rebuild is an update of what the caller saw, so an emptied
        // slot swallows it too.
        let reseed = WeatherPatch {
            peak_pct: Some(80.0),
            seed: Some(7),
            ..Default::default()
        };
        reseed
            .install_over(&site, WeatherDoor::Http, seen_before_the_reset)
            .expect("a valid patch");
        assert!(
            site.with_weather(|_| ()).is_none(),
            "a re-seed must not resurrect the cleared weather either",
        );

        // The other half of the contract: asked to look for itself,
        // the self-probing form finds nothing and does create.
        patch
            .install(&site, WeatherDoor::Http)
            .expect("a valid patch");
        assert_eq!(
            site.with_weather(|w| w.config().peak_pct),
            Some(80.0),
            "a genuinely weatherless site still gets a fresh sky",
        );
    }

    /// Direct construction bypasses the doors — nothing stops a test
    /// or a future embedder from handing `Weather::new` a config the
    /// validators would have rejected. `advance` must degrade rather
    /// than panic even then: an absurd `cloud_duration` upper bound
    /// and a vanishing `cloud_rate_per_h` (which blows up
    /// `exp_sample`'s `-ln(u)/rate` the same way) must not kill the
    /// physics tick that calls this.
    #[test]
    fn advance_does_not_panic_on_absurd_config_bypassing_the_doors() {
        let mut w = Weather::new(WeatherConfig {
            cloud_rate_per_h: Some(1e-30),
            cloud_duration: (1e30, 1e30),
            cloud_ramp: (1e30, 1e30),
            seed: Some(1),
            ..Default::default()
        });
        let mut t = at(0, 0);
        let end = at(23, 0);
        while t <= end {
            w.advance(t); // must not panic
            t += chrono::Duration::minutes(30);
        }
    }
}
