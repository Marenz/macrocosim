//! The HTTP API's shared shapes: the one error type every route
//! fails with, extractor wrappers that turn axum's own rejections
//! into it, and the JSON fallbacks for unknown paths and methods.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection, StringRejection};
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::{Method, StatusCode, Uri, request::Parts};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A failed request: a status and a message, sent as
/// `{"error": message}` plus any extra fields.
#[derive(Debug)]
pub(in crate::ui) struct ApiError {
    status: StatusCode,
    error: String,
    extra: Map<String, Value>,
}

impl ApiError {
    pub(in crate::ui) fn new(status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            status,
            error: error.into(),
            extra: Map::new(),
        }
    }

    pub(in crate::ui) fn bad_request(error: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, error)
    }

    pub(in crate::ui) fn not_found(error: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, error)
    }

    pub(in crate::ui) fn conflict(error: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, error)
    }

    pub(in crate::ui) fn internal(error: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error)
    }

    /// The 404 for a microgrid id the registry does not hold.
    pub(in crate::ui) fn not_registered(mg: u64) -> Self {
        Self::not_found(format!("microgrid {mg} not registered"))
    }

    /// The 404 for a component id the microgrid does not hold.
    pub(in crate::ui) fn no_component(id: u64) -> Self {
        Self::not_found(format!("component {id} not found"))
    }

    /// Add a field beside `error`.
    pub(in crate::ui) fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_string(), value.into());
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = self.extra;
        body.insert("error".to_string(), Value::String(self.error));
        (self.status, axum::Json(Value::Object(body))).into_response()
    }
}

/// `axum::Json` whose rejection is an `ApiError` that keeps axum's
/// status.
pub(in crate::ui) struct Json<T>(pub T);

impl<S, T> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(v)| Json(v))
            .map_err(|e: JsonRejection| ApiError::new(e.status(), e.body_text()))
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// `axum::extract::Query` whose rejection is an `ApiError` that
/// keeps axum's status.
pub(in crate::ui) struct Query<T>(pub T);

impl<S, T> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(v)| Query(v))
            .map_err(|e: QueryRejection| ApiError::new(e.status(), e.body_text()))
    }
}

/// `axum::extract::Path` whose rejection is an `ApiError` that keeps
/// axum's status.
pub(in crate::ui) struct Path<T>(pub T);

impl<S, T> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(v)| Path(v))
            .map_err(|e: PathRejection| ApiError::new(e.status(), e.body_text()))
    }
}

/// The `{id}` segment of a `/component/{id}` route.
#[derive(Deserialize)]
pub(in crate::ui) struct ComponentPath {
    pub id: u64,
}

/// A UTF-8 request body, as axum's `String` extractor reads it, whose
/// rejection is an `ApiError` that keeps axum's status.
pub(in crate::ui) struct Text(pub String);

impl<S> FromRequest<S> for Text
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        String::from_request(req, state)
            .await
            .map(Text)
            .map_err(|e: StringRejection| ApiError::new(e.status(), e.body_text()))
    }
}

/// `axum::extract::ws::WebSocketUpgrade` whose rejection is an
/// `ApiError` that keeps axum's status.
pub(in crate::ui) struct WsUpgrade(pub axum::extract::ws::WebSocketUpgrade);

impl<S> FromRequestParts<S> for WsUpgrade
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        axum::extract::ws::WebSocketUpgrade::from_request_parts(parts, state)
            .await
            .map(WsUpgrade)
            .map_err(|e: WebSocketUpgradeRejection| ApiError::new(e.status(), e.body_text()))
    }
}

/// Fallback for a path no route matches.
pub(in crate::ui) async fn no_route(method: Method, uri: Uri) -> ApiError {
    ApiError::not_found(format!("no route for {method} {}", uri.path()))
}

/// Fallback for a known path asked with a method it does not take.
pub(in crate::ui) async fn method_not_allowed(method: Method, uri: Uri) -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        format!("no route for {method} {}", uri.path()),
    )
}

/// The microgrid a `/api/mg/{mg}/...` route names, resolved from the
/// registry. An unregistered id is the shared 404; a non-numeric one
/// is a 400.
pub(in crate::ui) struct Mg {
    pub id: u64,
    pub site: crate::sim::MicrogridSite,
}

impl FromRequestParts<crate::lisp::Config> for Mg {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        config: &crate::lisp::Config,
    ) -> Result<Self, ApiError> {
        let params = axum::extract::RawPathParams::from_request_parts(parts, config)
            .await
            .map_err(|e| ApiError::bad_request(e.body_text()))?;
        let raw = params
            .iter()
            .find_map(|(k, v)| (k == "mg").then_some(v))
            .ok_or_else(|| ApiError::internal("route has no {mg} segment"))?;
        let id: u64 = raw
            .parse()
            .map_err(|_| ApiError::bad_request(format!("invalid microgrid id: {raw:?}")))?;
        let site = config
            .microgrids()
            .lock()
            .get(&id)
            .map(|e| e.site.clone())
            .ok_or_else(|| ApiError::not_registered(id))?;
        Ok(Mg { id, site })
    }
}
