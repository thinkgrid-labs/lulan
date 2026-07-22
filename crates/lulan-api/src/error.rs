use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lulan_engine::inventory::StoreError;
use serde_json::json;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(&'static str),
    Forbidden(&'static str),
    NotFound(String),
    Conflict(String),
    TooManyRequests,
    ServiceUnavailable(&'static str),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, msg.to_string()),
            ApiError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.to_string()),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate limit exceeded".to_string(),
            ),
            ApiError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg.to_string()),
            ApiError::Internal(err) => {
                tracing::error!(error = ?err, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::UnknownStop(_) | StoreError::StopsOutOfOrder { .. } => {
                ApiError::BadRequest(err.to_string())
            }
            StoreError::Span(_) => ApiError::BadRequest(err.to_string()),
            // The request was well formed; the departure just isn't for
            // sale any more — the same shape as losing a seat race.
            StoreError::TripNotSellable { .. } | StoreError::TripDeparted { .. } => {
                ApiError::Conflict(err.to_string())
            }
            StoreError::Db(db) => ApiError::Internal(db.into()),
        }
    }
}
