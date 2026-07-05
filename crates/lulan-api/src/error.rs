use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lulan_engine::inventory::StoreError;
use serde_json::json;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    ServiceUnavailable(&'static str),
    Internal(anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
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
            StoreError::Db(db) => ApiError::Internal(db.into()),
        }
    }
}
