//! Per-microgrid dispatch endpoints. List / create / pause-resume /
//! delete views over the shared `DispatchStore` — the same store the
//! `MicrogridDispatchService` gRPC server mutates, so all write paths
//! share construction + validation.

use axum::{extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};

use crate::lisp::Config;
use crate::ui::api::{ApiError, Json, Mg, Path};

/// JSON shape for one dispatch in the per-microgrid Dispatches view.
/// Timestamps are epoch-millis so the SPA formats them client-side via
/// its TZ toggle, like every other UI timestamp. `target` / `recurrence`
/// are pre-rendered human strings — the SPA only displays them.
#[derive(Serialize)]
pub(in crate::ui) struct DispatchView {
    id: u64,
    #[serde(rename = "type")]
    type_: String,
    active: bool,
    dry_run: bool,
    start_ms: Option<i64>,
    duration_s: Option<u32>,
    end_ms: Option<i64>,
    create_ms: Option<i64>,
    update_ms: Option<i64>,
    target: String,
    recurrence: Option<String>,
    payload: serde_json::Value,
}

/// Per-microgrid dispatch list, read straight from the shared
/// `DispatchStore` (no gRPC round-trip). Newest-created first — ids
/// are monotonic, so descending id order matches. Returns `[]` for a
/// registered microgrid with no dispatches.
pub(in crate::ui) async fn dispatches(
    State(config): State<Config>,
    mg: Mg,
) -> Json<Vec<DispatchView>> {
    let views = config
        .dispatches()
        .list_mg(mg.id)
        .iter()
        .rev()
        .map(dispatch_to_view)
        .collect();
    Json(views)
}

/// Body for `POST /api/mg/{mg}/dispatches`. `target` is the same
/// human syntax the dispatch CLI takes (category names or numeric
/// ids); `payload` is free JSON (must be an object). With no
/// `start_ms` the dispatch starts immediately. `recurrence` is
/// optional — omitted (or `freq: "once"`) creates a one-off.
#[derive(Deserialize)]
pub(in crate::ui) struct DispatchCreateReq {
    #[serde(rename = "type")]
    type_: String,
    target: String,
    #[serde(default)]
    duration_s: Option<u32>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    dry_run: Option<bool>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    #[serde(default)]
    start_ms: Option<i64>,
    #[serde(default)]
    recurrence: Option<RecurrenceReq>,
}

/// Recurrence shape the create form submits: a frequency name plus
/// "every N" interval. Maps onto the proto `RecurrenceRule`; the
/// by-minute / by-weekday refinements the proto also carries aren't
/// exposed here — the dispatch CLI covers those.
#[derive(Deserialize)]
pub(in crate::ui) struct RecurrenceReq {
    freq: String,
    #[serde(default)]
    interval: Option<u32>,
}

/// Map the form's frequency name onto the proto enum. `"once"` (or
/// empty) means no recurrence rule at all — distinct from
/// `FREQUENCY_UNSPECIFIED`, which the store also treats as
/// non-recurring but would still carry a pointless rule object.
/// Names derive from the proto enum's own `FREQUENCY_*` spelling
/// (the inverse of `recurrence_to_string`), so a frequency added
/// upstream is accepted here without touching this function.
fn recurrence_from_req(
    req: Option<&RecurrenceReq>,
) -> Result<Option<crate::proto::dispatch::RecurrenceRule>, String> {
    use crate::proto::dispatch::recurrence_rule::Frequency;
    let Some(req) = req else { return Ok(None) };
    let lowered = req.freq.to_lowercase();
    if lowered.is_empty() || lowered == "once" {
        return Ok(None);
    }
    let freq = Frequency::from_str_name(&format!("FREQUENCY_{}", lowered.to_uppercase()))
        .filter(|f| *f != Frequency::Unspecified)
        .ok_or_else(|| {
            format!(
                "unknown recurrence frequency {:?}; expected once or one of the \
                 frequencies the dispatch API defines (hourly, daily, weekly, …)",
                req.freq
            )
        })?;
    Ok(Some(crate::proto::dispatch::RecurrenceRule {
        freq: freq as i32,
        interval: req.interval.unwrap_or(1).max(1),
        ..Default::default()
    }))
}

/// Create a dispatch from the UI. Parses the human target / payload,
/// then goes through the same `DispatchStore::create` the gRPC server
/// uses, so the construction rules are identical.
pub(in crate::ui) async fn dispatch_create(
    State(config): State<Config>,
    mg: Mg,
    Json(req): Json<DispatchCreateReq>,
) -> Result<(StatusCode, Json<DispatchView>), ApiError> {
    let target = crate::sim::dispatch::parse_target(&req.target).map_err(ApiError::bad_request)?;
    let payload = match req.payload {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            Some(crate::sim::dispatch::json_to_struct(&value).map_err(ApiError::bad_request)?)
        }
    };
    let recurrence = recurrence_from_req(req.recurrence.as_ref()).map_err(ApiError::bad_request)?;
    let start_immediately = req.start_ms.is_none();
    let start_time = req.start_ms.map(|ms| prost_types::Timestamp {
        seconds: ms.div_euclid(1000),
        nanos: (ms.rem_euclid(1000) * 1_000_000) as i32,
    });
    let data = crate::proto::dispatch::DispatchData {
        r#type: req.type_,
        start_time,
        duration: req.duration_s,
        target: Some(target),
        is_active: req.active.unwrap_or(true),
        is_dry_run: req.dry_run.unwrap_or(false),
        payload,
        recurrence,
    };
    let dispatch = config
        .dispatches()
        .create(mg.id, data, start_immediately)
        .map_err(dispatch_err_to_http)?;
    Ok((StatusCode::CREATED, Json(dispatch_to_view(&dispatch))))
}

/// Body for `POST /api/mg/{mg}/dispatches/{did}/active` — pause
/// (`false`) or resume (`true`).
#[derive(Deserialize)]
pub(in crate::ui) struct DispatchSetActiveReq {
    active: bool,
}

pub(in crate::ui) async fn dispatch_set_active(
    State(config): State<Config>,
    mg: Mg,
    Path((_, dispatch_id)): Path<(u64, u64)>,
    Json(req): Json<DispatchSetActiveReq>,
) -> Result<Json<DispatchView>, ApiError> {
    let dispatch = config
        .dispatches()
        .set_active(mg.id, dispatch_id, req.active)
        .map_err(dispatch_err_to_http)?;
    Ok(Json(dispatch_to_view(&dispatch)))
}

pub(in crate::ui) async fn dispatch_delete(
    State(config): State<Config>,
    mg: Mg,
    Path((_, dispatch_id)): Path<(u64, u64)>,
) -> Result<StatusCode, ApiError> {
    let mg_id = mg.id;
    config
        .dispatches()
        .remove(mg_id, dispatch_id)
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "dispatch {dispatch_id} not found for microgrid {mg_id}"
            ))
        })?;
    Ok(StatusCode::NO_CONTENT)
}

fn dispatch_err_to_http(err: crate::sim::dispatch::DispatchError) -> ApiError {
    use crate::sim::dispatch::DispatchError;
    let code = match err {
        DispatchError::MissingStartTime => StatusCode::BAD_REQUEST,
        DispatchError::NotFound => StatusCode::NOT_FOUND,
    };
    ApiError::new(code, err.to_string())
}

fn dispatch_to_view(d: &crate::proto::dispatch::Dispatch) -> DispatchView {
    let data = d.data.clone().unwrap_or_default();
    let meta = d.metadata.unwrap_or_default();
    DispatchView {
        id: meta.dispatch_id,
        type_: data.r#type,
        active: data.is_active,
        dry_run: data.is_dry_run,
        start_ms: data.start_time.as_ref().map(ts_to_ms),
        duration_s: data.duration,
        end_ms: meta.end_time.as_ref().map(ts_to_ms),
        create_ms: meta.create_time.as_ref().map(ts_to_ms),
        update_ms: meta.update_time.as_ref().map(ts_to_ms),
        target: crate::sim::dispatch::target_to_string(data.target.as_ref()),
        recurrence: recurrence_to_string(data.recurrence.as_ref()),
        payload: data
            .payload
            .as_ref()
            .map(crate::sim::dispatch::struct_to_json)
            .unwrap_or(serde_json::Value::Null),
    }
}

fn ts_to_ms(ts: &prost_types::Timestamp) -> i64 {
    // i128 + clamp: an extreme start_time stored via gRPC (seconds near
    // i64::MAX) must not overflow `seconds * 1000` when the UI lists it.
    let ms = ts.seconds as i128 * 1000 + (ts.nanos as i128) / 1_000_000;
    ms.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Compact recurrence summary, e.g. `daily ×2`. `None` for a
/// non-recurring dispatch (no rule, or an unspecified frequency).
fn recurrence_to_string(rule: Option<&crate::proto::dispatch::RecurrenceRule>) -> Option<String> {
    use crate::proto::dispatch::recurrence_rule::Frequency;
    let rule = rule?;
    let freq = Frequency::try_from(rule.freq).ok()?;
    if freq == Frequency::Unspecified {
        return None;
    }
    let label = freq
        .as_str_name()
        .strip_prefix("FREQUENCY_")
        .unwrap_or(freq.as_str_name())
        .to_lowercase();
    let interval = rule.interval.max(1);
    Some(format!("{label} ×{interval}"))
}
