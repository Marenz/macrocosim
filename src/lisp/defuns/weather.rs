//! Site weather doors: `(make-weather …)` / `(set-weather …)` install
//! and retune the site's [`Weather`] singleton, `(pass-cloud …)`
//! scripts one deterministic cloud over the array, and
//! `(weather-status)` reads the sky back as an alist.

use std::time::Duration;

use tulisp::{Error, TulispContext, TulispObject};

use crate::lisp::renames::Renamed;
use crate::lisp::value::LispValue;
use crate::sim::microgrids::SharedSiteRouter;
use crate::sim::sim_clock::parse_time_of_day;
use crate::sim::weather::{self as weather, Weather, WeatherConfig, WeatherDoor, WeatherPatch};

tulisp::AsPlist! {
    /// Plist payload shared by `(make-weather …)` and
    /// `(set-weather …)`. Every key is optional: `make-weather`
    /// applies them over [`WeatherConfig::default`], `set-weather`
    /// over whatever the site already has.
    pub struct WeatherArgs {
        /// Time of day (UTC) the clear-sky curve turns on — either
        /// an `"HH:MM"` string or a bare number of seconds since
        /// midnight, matching `(parse-time-of-day)`.
        sunrise: Option<LispValue> {= None},
        /// Time of day (UTC) the clear-sky curve turns off. Same
        /// two spellings as `:sunrise`.
        sunset: Option<LispValue> {= None},
        /// Clear-sky output at solar noon, percent.
        peak_pct<":peak-pct">: Option<f64> {= None},
        /// Mean gap between ambient clouds, seconds. Zero disables
        /// the ambient generator, leaving only scripted
        /// `(pass-cloud)` events; a negative gap, or a positive one
        /// under a second, is an error.
        cloud_mean_gap_s<":cloud-mean-gap-s">: Option<f64> {= None},
        /// Ambient cloud depth, percent — a number (fixed) or a
        /// two-element `(lo hi)` list drawn from uniformly.
        cloud_depth_pct<":cloud-depth-pct">: Option<LispValue> {= None},
        /// Ambient cloud total duration, seconds. Number or `(lo hi)`.
        cloud_duration_s<":cloud-duration-s">: Option<LispValue> {= None},
        /// Ambient cloud ramp-in/ramp-out time, seconds. Number or
        /// `(lo hi)`.
        cloud_ramp_s<":cloud-ramp-s">: Option<LispValue> {= None},
        /// Ambient generator seed. Passing it to `(set-weather)`
        /// re-seeds the generator (see the defun's docs).
        seed: Option<i64> {= None},
    }
}

/// A `(lo hi)` range kwarg: a plain number means a fixed value
/// (`(v, v)`), a two-element list means the uniform range. Mirrors
/// `:sunlight-pct`'s raw-value dispatch in `make.rs`.
fn range_arg(kw: &str, v: &LispValue) -> Result<(f32, f32), Error> {
    let raw = v.as_inner();
    if raw.numberp() {
        let x = f64::try_from(raw)? as f32;
        return Ok((x, x));
    }
    if raw.consp() {
        // A malformed list — `'(20)`, whose cadr is nil — fails the
        // number conversion with tulisp's own type error, which names
        // neither the kwarg nor the shape expected. Re-wrap so the
        // author is told which key they got wrong.
        let elem = |o: TulispObject| {
            f64::try_from(&o).map(|v| v as f32).map_err(|_| {
                Error::invalid_argument(format!(
                    "{kw}: expected a two-element (lo hi) list of numbers — got {raw}"
                ))
            })
        };
        return Ok((elem(raw.car()?)?, elem(raw.cadr()?)?));
    }
    Err(Error::invalid_argument(format!(
        "{kw}: expected a number or a (lo hi) list — got {raw}"
    )))
}

/// The ambient cloud rate, events per hour, for a mean gap in seconds.
/// Zero is "no ambient clouds"; the weather model takes the rate.
fn cloud_rate_per_h(form: &str, gap_s: f64) -> Result<f32, Error> {
    crate::sim::weather::validate::cloud_rate_for_gap(gap_s)
        .map_err(|e| Error::invalid_argument(format!("{form}: :cloud-mean-gap-s {e}")))
}

/// A time-of-day kwarg: an `"HH:MM"` string, or a bare number of
/// seconds since midnight. The number spelling matches what
/// `(parse-time-of-day)` already accepts, so a scenario can compute
/// one instead of formatting a string.
fn time_of_day_arg(kw: &str, v: &LispValue) -> Result<Duration, Error> {
    let raw = v.as_inner();
    if raw.numberp() {
        let secs = f64::try_from(raw)?;
        if !(0.0..86_400.0).contains(&secs) {
            return Err(Error::invalid_argument(format!(
                "{kw}: seconds since midnight must be in [0, 86400), got {secs}"
            )));
        }
        return Ok(Duration::from_secs_f64(secs));
    }
    let s = String::try_from(raw.clone())?;
    parse_time_of_day(&s).ok_or_else(|| {
        Error::invalid_argument(format!("{kw}: malformed time {s:?} — expected \"HH:MM\""))
    })
}

/// Parse the plist's Lisp spellings into a [`WeatherPatch`]. Only
/// the wire shapes are this door's business — `"HH:MM"` or bare
/// seconds for a time, a number or a `(lo hi)` list for a range. The
/// fold order, the numeric validation and the sunrise/sunset pair
/// check all belong to the patch, shared with the HTTP weather route.
fn patch_args(a: &WeatherArgs, form: &str) -> Result<WeatherPatch, Error> {
    let range = |kw, v: &Option<LispValue>| v.as_ref().map(|v| range_arg(kw, v)).transpose();
    Ok(WeatherPatch {
        sunrise: a
            .sunrise
            .as_ref()
            .map(|v| time_of_day_arg(":sunrise", v))
            .transpose()?,
        sunset: a
            .sunset
            .as_ref()
            .map(|v| time_of_day_arg(":sunset", v))
            .transpose()?,
        peak_pct: a.peak_pct.map(|v| v as f32),
        cloud_rate_per_h: a
            .cloud_mean_gap_s
            .map(|g| cloud_rate_per_h(form, g))
            .transpose()?,
        cloud_depth: range(":cloud-depth-pct", &a.cloud_depth_pct)?,
        cloud_duration: range(":cloud-duration-s", &a.cloud_duration_s)?,
        cloud_ramp: range(":cloud-ramp-s", &a.cloud_ramp_s)?,
        seed: a.seed.map(|v| v as u64),
    })
}

pub(super) fn register(ctx: &mut TulispContext, router: SharedSiteRouter) {
    // Install a fresh Weather on the site, replacing whatever was
    // there (events and RNG stream included). Every key optional —
    // the bare `(make-weather)` gives the default 06:00–20:00
    // clear-sky day with no ambient clouds.
    let r = router.clone();
    ctx.defun(
        "make-weather",
        move |args: tulisp::Plist<Renamed<WeatherArgs>>| -> Result<bool, Error> {
            let a = args.into_inner().0;
            let mut cfg = WeatherConfig::default();
            patch_args(&a, "make-weather")?
                .apply_to(&mut cfg, WeatherDoor::Lisp("make-weather"))
                .map_err(Error::invalid_argument)?;
            r.site().set_weather(Some(Weather::new(cfg)));
            Ok(true)
        },
    );

    // PARTIAL update of the site's existing weather config: only the
    // keys passed are touched, and the event list and anchor survive.
    // With no weather installed yet it starts from the defaults, so
    // `(set-weather :cloud-mean-gap-s 600)` on a fresh site is a valid way in.
    //
    // `:seed` is the exception: an RNG can't be re-seeded in place
    // without disturbing the stream, so passing it rebuilds the
    // Weather outright — a deliberate reset of the ambient event
    // stream, which is exactly what "re-seed" means for a scenario
    // that wants reproducibility from here on.
    let r = router.clone();
    ctx.defun(
        "set-weather",
        move |args: tulisp::Plist<Renamed<WeatherArgs>>| -> Result<bool, Error> {
            let a = args.into_inner().0;
            patch_args(&a, "set-weather")?
                .install(&r.site(), WeatherDoor::Lisp("set-weather"))
                .map_err(Error::invalid_argument)?;
            Ok(true)
        },
    );

    // Script one deterministic cloud starting at the weather's
    // current anchor. Plain positional args, not a plist — this is
    // the one weather door a scenario cue fires repeatedly, and
    // `(pass-cloud 80 600)` reads better in a cue list than a plist
    // would.
    let r = router.clone();
    ctx.defun(
        "pass-cloud",
        move |depth_pct: f64, duration_s: f64, ramp_s: Option<f64>| -> Result<bool, Error> {
            let (depth_pct, duration, ramp) =
                weather::validate::pass_cloud_args(depth_pct, duration_s, ramp_s.unwrap_or(0.0))
                    .map_err(|e| Error::invalid_argument(format!("pass-cloud: {e}")))?;
            r.site()
                .with_weather(|w| w.pass_cloud(depth_pct, duration, ramp))
                .ok_or_else(|| {
                    Error::invalid_argument(
                        "pass-cloud: no weather on this site — call (make-weather …) first"
                            .to_string(),
                    )
                })?;
            Ok(true)
        },
    );

    // Read the sky back as
    // `((sunlight-pct . N) (clear-sky-pct . N) (events . N))`,
    // evaluated at the weather's own anchor — the last `now` a tick
    // handed it, so the answer matches what the inverters just saw
    // rather than a wall clock that may be hours off in a stepped
    // run. `nil` when the site has no weather: this is a query, and a
    // panel polling it shouldn't have to catch an error to learn the
    // sky isn't modelled.
    let r = router;
    ctx.defun(
        "weather-status",
        move |ctx: &mut TulispContext| -> Result<TulispObject, Error> {
            let Some((pct, clear, events)) = r.site().with_weather(|w| {
                // Un-anchored (nothing has ticked yet) falls back to
                // the wall clock, which is what `pass_cloud` uses for
                // its own start in that state — so the two agree.
                let at = w.anchor().unwrap_or_else(chrono::Utc::now);
                (w.pct_at(at), w.clear_sky_pct(at), w.events().len() as i64)
            }) else {
                return Ok(TulispObject::nil());
            };
            // Two decimals: the underlying f32 widened to f64 prints
            // as 99.99999237060547, which is noise in an alist a
            // human reads and a scenario asserts on.
            let round2 = |v: f32| (v as f64 * 100.0).round() / 100.0;
            let mut cell = |k: &str, v: TulispObject| TulispObject::cons(ctx.intern(k), v);
            Ok(vec![
                cell("sunlight-pct", round2(pct).into()),
                cell("clear-sky-pct", round2(clear).into()),
                cell("events", events.into()),
            ]
            .into_iter()
            .collect())
        },
    );
}

#[cfg(test)]
mod tests {
    use super::super::super::test_support::config_with;
    use chrono::{TimeZone, Utc};
    use std::time::Duration;

    /// make-weather + weather-status round trip, pass-cloud arms an
    /// event, and the malformed / inverted time forms error cleanly.
    #[test]
    fn weather_doors_round_trip() {
        let (cfg, _dir) = config_with("");
        cfg.eval(r#"(make-weather :sunrise "06:00" :sunset "20:00")"#)
            .unwrap();
        // Nothing has ticked, so the reading is un-anchored — assert
        // the alist shape, not the value.
        let st = cfg.eval("(weather-status)").unwrap();
        assert!(st.contains("pct"), "alist shape, got {st}");
        cfg.eval("(pass-cloud 50 600)").unwrap();
        let st = cfg.eval("(weather-status)").unwrap();
        assert!(st.contains("(events . 1)"), "one event armed, got {st}");

        // Malformed and inverted windows error — and the message
        // names the door the author actually typed, not the sibling
        // that shares the plist folding.
        assert!(cfg.eval(r#"(set-weather :sunrise "6h30")"#).is_err());
        let err = cfg
            .eval(r#"(set-weather :sunrise "21:00" :sunset "06:00")"#)
            .unwrap_err();
        assert!(err.contains("set-weather:"), "{err}");
        let err = cfg
            .eval(r#"(make-weather :sunrise "21:00" :sunset "06:00")"#)
            .unwrap_err();
        assert!(err.contains("make-weather:"), "{err}");
        // …and a rejected form leaves the live weather alone.
        let st = cfg.eval("(weather-status)").unwrap();
        assert!(st.contains("(events . 1)"), "still armed, got {st}");
    }

    /// The weather keywords carry their unit, the clouds are spaced by
    /// a mean gap in seconds, and `(weather-status)` names its percents.
    #[test]
    fn weather_takes_unit_keywords_and_reports_them() {
        let (cfg, _dir) = config_with("");
        cfg.eval(
            "(make-weather :peak-pct 80.0 :cloud-mean-gap-s 1200 :cloud-depth-pct 40
                           :cloud-duration-s 120 :cloud-ramp-s 30)",
        )
        .unwrap();
        let st = cfg.eval("(weather-status)").unwrap();
        assert!(st.contains("sunlight-pct"), "{st}");
        assert!(st.contains("clear-sky-pct"), "{st}");
        assert!(!st.contains("(pct"), "{st}");
        cfg.site()
            .with_weather(|w| {
                let c = w.config();
                assert_eq!(c.peak_pct, 80.0);
                assert_eq!(c.cloud_rate_per_h, Some(3.0));
                assert_eq!(c.cloud_depth, (40.0, 40.0));
                assert_eq!(c.cloud_duration, (120.0, 120.0));
                assert_eq!(c.cloud_ramp, (30.0, 30.0));
            })
            .unwrap();
    }

    /// The old cloud rate (events per hour) and `:peak%` still load.
    #[test]
    fn old_weather_keywords_convert() {
        let (old, _a) = config_with("");
        old.eval("(make-weather :peak% 80.0 :cloud-rate 3 :cloud-depth 40 :cloud-duration 120 :cloud-ramp 30)")
            .unwrap();
        let (new, _b) = config_with("");
        new.eval(
            "(make-weather :peak-pct 80.0 :cloud-mean-gap-s 1200 :cloud-depth-pct 40
                           :cloud-duration-s 120 :cloud-ramp-s 30)",
        )
        .unwrap();
        assert_eq!(
            old.eval("(weather-status)").unwrap(),
            new.eval("(weather-status)").unwrap()
        );
        let rate = |c: &crate::lisp::Config| {
            c.site()
                .with_weather(|w| w.config().cloud_rate_per_h)
                .unwrap()
        };
        assert_eq!(rate(&old), rate(&new));
        assert_eq!(rate(&old), Some(3.0));
    }

    /// A zero mean gap is "no ambient clouds", from either spelling.
    #[test]
    fn a_zero_cloud_mean_gap_means_no_ambient_clouds() {
        let (cfg, _dir) = config_with("");
        cfg.eval("(make-weather :cloud-mean-gap-s 1200)").unwrap();
        let rate = || {
            cfg.site()
                .with_weather(|w| w.config().cloud_rate_per_h)
                .unwrap()
        };
        assert_eq!(rate(), Some(3.0));
        cfg.eval("(set-weather :cloud-mean-gap-s 0)").unwrap();
        assert_eq!(rate(), None);
        cfg.eval("(set-weather :cloud-mean-gap-s 1200)").unwrap();
        cfg.eval("(set-weather :cloud-rate 0)").unwrap();
        assert_eq!(rate(), None);
    }

    /// `(weather-status)` on a site with no weather is `nil`, not an
    /// error — a polling panel shouldn't need a handler for "the sky
    /// isn't modelled here".
    #[test]
    fn weather_status_is_nil_without_weather() {
        let (cfg, _dir) = config_with("");
        assert_eq!(cfg.eval("(weather-status)").unwrap(), "nil");
        // pass-cloud, being a mutator, does error.
        let err = cfg.eval("(pass-cloud 50 600)").unwrap_err();
        assert!(err.contains("no weather"), "{err}");
    }

    /// `(set-weather)` is a PARTIAL update: it keeps the keys it
    /// wasn't given (and the armed events), while `:seed` rebuilds
    /// the generator. Range keys take a number or a `(lo hi)` list.
    #[test]
    fn set_weather_updates_partially_and_reseeds() {
        let (cfg, _dir) = config_with("");
        cfg.eval(r#"(make-weather :sunrise "05:00" :sunset "21:00" :peak-pct 80)"#)
            .unwrap();
        cfg.eval("(pass-cloud 30 600)").unwrap();
        cfg.eval("(set-weather :cloud-mean-gap-s 600 :cloud-depth-pct '(20 70) :cloud-ramp-s 15)")
            .unwrap();
        let site = cfg.site();
        site.with_weather(|w| {
            let c = w.config();
            assert_eq!(c.sunrise, Duration::from_secs(5 * 3600), "kept :sunrise");
            assert_eq!(c.peak_pct, 80.0, "kept :peak-pct");
            assert_eq!(c.cloud_rate_per_h, Some(6.0));
            assert_eq!(c.cloud_depth, (20.0, 70.0), "(lo hi) list");
            assert_eq!(c.cloud_ramp, (15.0, 15.0), "a bare number is a fixed value");
        })
        .unwrap();
        assert!(
            cfg.eval("(weather-status)")
                .unwrap()
                .contains("(events . 1)"),
            "a partial update keeps the armed events"
        );

        // :seed rebuilds the generator — a deliberate reset.
        cfg.eval("(set-weather :seed 7)").unwrap();
        site.with_weather(|w| assert_eq!(w.config().seed, Some(7)))
            .unwrap();
        assert!(
            cfg.eval("(weather-status)")
                .unwrap()
                .contains("(events . 0)"),
            "re-seeding resets the event stream"
        );

        // Out-of-range depth is rejected.
        let err = cfg
            .eval("(set-weather :cloud-depth-pct '(0 140))")
            .unwrap_err();
        assert!(err.contains("[0, 100]"), "{err}");
    }

    /// Every numeric kwarg is checked at the door rather than left to
    /// produce silently wrong physics: a negative `:peak-pct` inverts
    /// the clear-sky arch into parked-at-0 non-generation, a negative
    /// `:cloud-mean-gap-s` is refused (only 0 means "no clouds"), and
    /// a negative duration/ramp panics `Duration::from_secs_f32`
    /// downstream. A malformed range list names its own kwarg.
    #[test]
    fn numeric_kwargs_are_validated_at_the_door() {
        let (cfg, _dir) = config_with("");
        cfg.eval("(make-weather)").unwrap();
        for (form, needle) in [
            ("(set-weather :peak-pct -10)", ":peak-pct"),
            ("(make-weather :peak-pct -0.5)", ":peak-pct"),
            ("(set-weather :cloud-mean-gap-s -1)", ":cloud-mean-gap-s"),
            (
                "(set-weather :cloud-duration-s '(-60 600))",
                ":cloud-duration-s",
            ),
            ("(set-weather :cloud-ramp-s -5)", ":cloud-ramp-s"),
            ("(set-weather :cloud-duration-s '(20))", ":cloud-duration-s"),
            (
                "(set-weather :cloud-depth-pct \"heavy\")",
                ":cloud-depth-pct",
            ),
        ] {
            let err = cfg.eval(form).unwrap_err();
            assert!(err.contains(needle), "{form} → {err}");
        }
        // The live weather survived every rejection.
        cfg.site()
            .with_weather(|w| {
                assert_eq!(w.config().peak_pct, 100.0);
                assert_eq!(w.config().cloud_rate_per_h, None);
            })
            .unwrap();
        // 0 stays the "no ambient clouds" spelling.
        cfg.eval("(set-weather :cloud-mean-gap-s 0)").unwrap();
        cfg.site()
            .with_weather(|w| assert_eq!(w.config().cloud_rate_per_h, None))
            .unwrap();
    }

    /// The exact repro from the final review: a magnitude the sign-only
    /// checks above would have waved through (`1e30` is finite and
    /// non-negative) is rejected at the door instead of surviving to
    /// panic `Duration::from_secs_f32` inside `Weather::advance` on the
    /// physics task. A `:cloud-mean-gap-s` under 1 s is refused too: it
    /// is a typo, and a vanishing one would make `advance` add clouds
    /// without end.
    #[test]
    fn absurd_magnitude_cloud_config_is_rejected_at_the_door() {
        let (cfg, _dir) = config_with("");
        cfg.eval("(make-weather)").unwrap();
        for (form, needle) in [
            (
                "(set-weather :cloud-mean-gap-s 3600 :cloud-duration-s '(1e30 1e30))",
                ":cloud-duration-s",
            ),
            ("(set-weather :cloud-ramp-s '(1e30 1e30))", ":cloud-ramp-s"),
            ("(set-weather :cloud-mean-gap-s 1e-30)", ":cloud-mean-gap-s"),
            ("(set-weather :cloud-mean-gap-s 0.5)", ":cloud-mean-gap-s"),
        ] {
            let err = cfg.eval(form).unwrap_err();
            assert!(err.contains(needle), "{form} → {err}");
        }
        // The live weather survived every rejection — no ambient
        // generator was ever armed with the absurd config.
        cfg.site()
            .with_weather(|w| assert_eq!(w.config().cloud_rate_per_h, None))
            .unwrap();
    }

    /// `:sunrise` / `:sunset` also take a bare number of seconds
    /// since midnight, the same second spelling `(parse-time-of-day)`
    /// accepts.
    #[test]
    fn time_of_day_takes_seconds_too() {
        let (cfg, _dir) = config_with("");
        cfg.eval("(make-weather :sunrise 21600 :sunset (* 20 3600))")
            .unwrap();
        cfg.site()
            .with_weather(|w| {
                assert_eq!(w.config().sunrise, Duration::from_secs(21_600));
                assert_eq!(w.config().sunset, Duration::from_secs(72_000));
            })
            .unwrap();
        assert!(
            cfg.eval("(make-weather :sunrise 90000)").is_err(),
            "seconds past a day are rejected"
        );
    }

    /// A follow-mode inverter's output dips through a scripted cloud
    /// in a stepped run: weather → inverter → telemetry end to end.
    /// Driven straight off `tick_once` (the same door the headless
    /// stepped runner turns) so the assertion doesn't depend on a
    /// scenario harness.
    #[test]
    fn stepped_run_sweeps_a_scripted_cloud() {
        let (cfg, _dir) = config_with(
            r#"(%make-solar-inverter :id 3 :rated-lower-w -10000.0 :rated-upper-w 0.0)"#,
        );
        cfg.eval(r#"(make-weather :sunrise "00:00" :sunset "23:59" :peak-pct 100)"#)
            .unwrap();
        let site = cfg.site();
        let dt = Duration::from_secs(60);
        let noon = Utc.with_ymd_and_hms(2020, 1, 1, 12, 0, 0).unwrap();
        // Step up to solar noon in dt-sized steps, exactly as the
        // stepped runner does.
        let mut t = noon - chrono::Duration::minutes(10);
        while t <= noon {
            site.tick_once(t, dt);
            t += chrono::Duration::seconds(60);
        }
        let before = site
            .get(3)
            .unwrap()
            .telemetry(&site)
            .active_power_w
            .expect("active power");
        assert!(before < -9_000.0, "clear-sky noon production, got {before}");

        cfg.eval("(pass-cloud 80 600 0)").unwrap();
        for _ in 0..12 {
            t += chrono::Duration::seconds(10);
            site.tick_once(t, Duration::from_secs(10));
        }
        let during = site
            .get(3)
            .unwrap()
            .telemetry(&site)
            .active_power_w
            .expect("active power");
        assert!(
            during > before * 0.5,
            "80% cloud must cut production (negative W): before {before}, during {during}"
        );
    }
}
