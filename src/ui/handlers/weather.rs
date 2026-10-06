//! Site weather over HTTP: `GET`/`POST /api/mg/{mg}/weather` mirror
//! the Lisp `(make-weather)`, `(set-weather)`, `(pass-cloud)` and
//! `(weather-status)` doors (`src/lisp/defuns/weather.rs`) for the
//! weather panel, which has no Lisp console of its own.
//!
//! Unlike the typed drive door in `control.rs`, weather isn't
//! component-addressed — it's a property of the site — so it gets
//! its own route pair instead of a `DriveRequest` field. Validation
//! reuses the same [`crate::sim::weather::validate`] rules the Lisp
//! doors enforce, so a request that would be rejected in the console
//! is rejected here too, and vice versa.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sim::microgrid_site::MicrogridSite;
use crate::sim::sim_clock::parse_time_of_day;
use crate::sim::weather::{self as weather, WeatherPatch};
use crate::ui::api::{ApiError, Json, Mg};

type WeatherResult = Result<Json<WeatherResponse>, ApiError>;

/// `GET`/`POST weather` response — the config the panel can
/// edit, plus the live readout it repaints from. Times print back as
/// `"HH:MM"`, the same spelling the POST body accepts.
#[derive(Serialize)]
pub(in crate::ui) struct WeatherResponse {
    sunrise: String,
    sunset: String,
    peak_pct: f32,
    /// Mean seconds between ambient clouds; `None` when the site has
    /// no ambient clouds.
    cloud_mean_gap_s: Option<f32>,
    cloud_depth_pct: (f32, f32),
    cloud_duration_s: (f32, f32),
    cloud_ramp_s: (f32, f32),
    sunlight_pct: f32,
    clear_sky_pct: f32,
    /// The instant this snapshot was evaluated at — the same clock
    /// that stamped every `events` start/end below. The panel filters
    /// its "clouds still overhead" list against THIS rather than the
    /// browser's `Date.now()`: the two clocks are usually different
    /// machines (a host browser against a guest VM), and a browser
    /// running a few seconds ahead would drop a cloud that had only
    /// just been fired.
    #[serde(serialize_with = "crate::timefmt::serialize_rfc3339")]
    now: DateTime<Utc>,
    events: Vec<CloudEventResponse>,
}

#[derive(Serialize)]
pub(in crate::ui) struct CloudEventResponse {
    #[serde(serialize_with = "crate::timefmt::serialize_rfc3339")]
    start: DateTime<Utc>,
    #[serde(serialize_with = "crate::timefmt::serialize_rfc3339")]
    end: DateTime<Utc>,
    depth_pct: f32,
}

/// `"HH:MM"`, the inverse of `parse_time_of_day`.
fn fmt_hhmm(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

/// Read the site's weather back as the panel's JSON shape, evaluated
/// at the weather's own anchor (falling back to the wall clock, same
/// as `(weather-status)`) — `None` when no weather is configured.
fn snapshot(site: &crate::sim::MicrogridSite) -> Option<WeatherResponse> {
    site.with_weather(|w| {
        let at = w.anchor().unwrap_or_else(Utc::now);
        let cfg = w.config();
        WeatherResponse {
            sunrise: fmt_hhmm(cfg.sunrise),
            sunset: fmt_hhmm(cfg.sunset),
            peak_pct: cfg.peak_pct,
            cloud_mean_gap_s: cfg.cloud_rate_per_h.map(|rate| 3600.0 / rate),
            cloud_depth_pct: cfg.cloud_depth,
            cloud_duration_s: cfg.cloud_duration,
            cloud_ramp_s: cfg.cloud_ramp,
            sunlight_pct: w.pct_at(at),
            clear_sky_pct: w.clear_sky_pct(at),
            now: at,
            events: w
                .events()
                .iter()
                .map(|e| CloudEventResponse {
                    start: e.start,
                    end: e.end(),
                    depth_pct: e.depth_pct,
                })
                .collect(),
        }
    })
}

pub(in crate::ui) async fn weather_get(mg: Mg) -> WeatherResult {
    snapshot(&mg.site)
        .map(Json)
        .ok_or_else(|| ApiError::not_found("no weather configured"))
}

/// One scripted cloud, fired after any config change in the same
/// request. `ramp_s` defaults to 0, matching `(pass-cloud)`'s
/// optional third argument.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::ui) struct PassCloudRequest {
    depth_pct: f64,
    duration_s: f64,
    #[serde(default)]
    ramp_s: Option<f64>,
}

/// `POST weather` body. Every field optional — a partial update
/// over whatever the site already has, same contract as
/// `(set-weather)`. Times are `"HH:MM"` strings; the ranges are
/// plain `[lo, hi]` pairs (the HTTP door has no need for Lisp's
/// number-or-list shorthand).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::ui) struct WeatherPostRequest {
    sunrise: Option<String>,
    sunset: Option<String>,
    peak_pct: Option<f64>,
    cloud_mean_gap_s: Option<f64>,
    cloud_depth_pct: Option<(f32, f32)>,
    cloud_duration_s: Option<(f32, f32)>,
    cloud_ramp_s: Option<(f32, f32)>,
    pass_cloud: Option<PassCloudRequest>,
}

impl WeatherPostRequest {
    /// Whether the body carries any *config* field — the half that
    /// creates weather on a weatherless site, exactly as
    /// `(set-weather …)` does. A body holding nothing but
    /// `pass_cloud` carries none.
    fn has_config_field(&self) -> bool {
        self.sunrise.is_some()
            || self.sunset.is_some()
            || self.peak_pct.is_some()
            || self.cloud_mean_gap_s.is_some()
            || self.cloud_depth_pct.is_some()
            || self.cloud_duration_s.is_some()
            || self.cloud_ramp_s.is_some()
    }
}

/// `"HH:MM"` → a time of day, naming the JSON field it came from.
fn hhmm_field(field: &str, s: &str) -> Result<std::time::Duration, String> {
    parse_time_of_day(s)
        .ok_or_else(|| format!("{field}: malformed time {s:?} — expected \"HH:MM\""))
}

/// Parse the request body's JSON shapes into a [`WeatherPatch`]. Only
/// the wire shapes are this door's business — `"HH:MM"` strings and
/// plain `[lo, hi]` pairs (no need for Lisp's number-or-list
/// shorthand). The fold order, the numeric validation and the
/// sunrise/sunset pair check all belong to the patch, shared with the
/// `(make-weather)` / `(set-weather)` doors, so a request rejected in
/// the console is rejected here too and vice versa.
fn patch_of(req: &WeatherPostRequest) -> Result<WeatherPatch, String> {
    Ok(WeatherPatch {
        sunrise: req
            .sunrise
            .as_deref()
            .map(|s| hhmm_field("sunrise", s))
            .transpose()?,
        sunset: req
            .sunset
            .as_deref()
            .map(|s| hhmm_field("sunset", s))
            .transpose()?,
        peak_pct: req.peak_pct.map(|v| v as f32),
        cloud_rate_per_h: req
            .cloud_mean_gap_s
            .map(|gap_s| {
                weather::validate::cloud_rate_for_gap(gap_s)
                    .map_err(|e| format!("cloud_mean_gap_s: {e}"))
            })
            .transpose()?,
        cloud_depth: req.cloud_depth_pct,
        cloud_duration: req.cloud_duration_s,
        cloud_ramp: req.cloud_ramp_s,
        // No JSON field for it: re-seeding the ambient generator is a
        // console operation, not something the panel offers.
        seed: None,
    })
}

fn apply_weather(site: &MicrogridSite, req: &WeatherPostRequest) -> WeatherResult {
    // The site's weather config, read ONCE. It answers both questions
    // this handler has — whether the site models weather at all (a
    // cloud-only body has to be turned away rather than creating one
    // on the way past) and what the patch folds over — and the
    // install below is handed this same reading rather than taking
    // its own, so a `reset()` landing mid-request can't be half-seen.
    // Taking it here and passing it down is what makes the decision
    // testable against a reading that no longer matches the site.
    let existing = site.with_weather(|w| w.config().clone());
    apply_weather_over(site, req, existing)
}

/// The half of [`apply_weather`] that runs against the caller's own
/// reading: `existing` is the look taken above, and every decision
/// below — create versus update, and whether a cloud-only body is
/// turned away — is made on it rather than on a second probe. A
/// `reset()` landing between the read and here leaves `existing`
/// stale on purpose; the install then no-ops on the empty slot and
/// the missing snapshot becomes the 409.
fn apply_weather_over(
    site: &MicrogridSite,
    req: &WeatherPostRequest,
    existing: Option<weather::WeatherConfig>,
) -> WeatherResult {
    // Validate everything first, apply after — same contract as
    // apply_status/apply_drive in control.rs: a bad pass_cloud must
    // not leave a config change applied, and vice versa.
    let pass_cloud = req
        .pass_cloud
        .as_ref()
        .map(|p| {
            weather::validate::pass_cloud_args(p.depth_pct, p.duration_s, p.ramp_s.unwrap_or(0.0))
        })
        .transpose()
        .map_err(|e| ApiError::bad_request(format!("pass_cloud: {e}")))?;

    let had_weather = existing.is_some();

    // A cloud-only request on a weatherless site is the one shape
    // where this door would otherwise diverge from `(pass-cloud …)`,
    // which rejects it outright: falling through would install a
    // whole default sky as a side effect of asking for one cloud.
    // A body that also carries a config field still creates, same as
    // `(set-weather …)` on a fresh site.
    if !had_weather && pass_cloud.is_some() && !req.has_config_field() {
        return Err(ApiError::bad_request(
            "pass_cloud: no weather on this site — set a config field in the same request, or \
             call (make-weather …) first"
                .to_string(),
        ));
    }

    // Partial update over what the site has, or a fresh default sky
    // when it has none — the shared create-or-update rule, the same
    // one `(set-weather …)` goes through, decided on the reading
    // taken above.
    patch_of(req)
        .and_then(|patch| patch.install_over(site, weather::WeatherDoor::Http, existing))
        .map_err(ApiError::bad_request)?;

    if let Some((depth_pct, duration, ramp)) = pass_cloud {
        site.with_weather(|w| w.pass_cloud(depth_pct, duration, ramp));
    }

    // Race: this handler takes the site's weather lock four separate
    // times (read the config, write it back, arm the cloud, snapshot),
    // and a concurrent `reset()` — a hot reload, or
    // `(reset-microgrid)` — can clear the slot between any two of
    // them. `with_weather` no-ops silently once that happens, so the
    // config change and the scripted cloud both went nowhere; the
    // missing snapshot is simply where that becomes visible. Report
    // the conflict instead of panicking on an `expect`.
    snapshot(site)
        .map(Json)
        .ok_or_else(|| ApiError::conflict("weather was removed while applying"))
}

pub(in crate::ui) async fn weather_post(
    mg: Mg,
    Json(req): Json<WeatherPostRequest>,
) -> WeatherResult {
    apply_weather(&mg.site, &req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    /// The wiring itself, pinned: this door hands its ONE reading to
    /// `install_over` rather than letting the install re-probe. The
    /// difference only shows in the race window, so the whole
    /// end-to-end suite passes with the self-probing `install` back
    /// in place — nothing above the sim layer notices. This is the
    /// test that does: a reading saying "there was weather" against a
    /// site a `reset()` has since cleared. The update arm has to win,
    /// the site has to stay weatherless, and the answer has to be the
    /// 409 the missing snapshot produces. Re-probing would take the
    /// create arm, resurrect the sky the reset cleared, and answer
    /// 200 over it.
    #[test]
    fn a_stale_reading_lands_nowhere_and_answers_409() {
        let site = MicrogridSite::new();
        let req: WeatherPostRequest =
            serde_json::from_str(r#"{"peak_pct": 80.0}"#).expect("a valid body");

        // What the handler read before the reset emptied the slot.
        let seen_before_the_reset = Some(weather::WeatherConfig::default());
        let status = apply_weather_over(&site, &req, seen_before_the_reset)
            .err()
            .map(|e| e.into_response().status());
        assert_eq!(
            status,
            Some(StatusCode::CONFLICT),
            "a stale reading has to report the conflict, not a 200 or a 400",
        );
        assert!(
            site.with_weather(|_| ()).is_none(),
            "the reset's cleared weather must stay cleared",
        );

        // The other half: on a site that really has weather, the same
        // body applies and answers with the updated snapshot.
        site.set_weather(Some(weather::Weather::new(
            weather::WeatherConfig::default(),
        )));
        let existing = site.with_weather(|w| w.config().clone());
        let peak = apply_weather_over(&site, &req, existing)
            .ok()
            .map(|Json(body)| body.peak_pct);
        assert_eq!(
            peak,
            Some(80.0),
            "a live site still takes the update and answers with it",
        );
    }
}
