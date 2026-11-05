//! Minimal shared web helpers for the archive service: a plain
//! `/health` endpoint. Kept local so this crate stays self-contained.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// `GET /health` -- plain `"ok"` body.
pub async fn health_ok() -> Response {
    (StatusCode::OK, "ok").into_response()
}
