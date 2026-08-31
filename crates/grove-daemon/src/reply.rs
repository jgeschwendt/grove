//! The adapter from [`grove_api::Envelope`] to an axum response.
//!
//! Every enveloped byte this daemon writes goes through here, guards and fallbacks
//! included: the envelope is the contract's one serializer, and a handler that reached
//! for `Json(json!({…}))` would be a second one. The status code travels *beside* the
//! envelope rather than inside it — HTTP already carries it, and v1 kept the two in
//! one hand-built literal per call site.
//!
//! One route is deliberately not enveloped: `GET /api/events` is an SSE stream whose
//! frames are the tagged `Event` objects themselves (`stream::events`). A stream has no
//! single body to wrap, and wrapping each frame would make a subscriber unwrap twice
//! for a status that never varies.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use grove_api::{ApiError, Envelope, ErrorCode};
use serde::Serialize;
use serde_json::Value;

/// A route's answer: an HTTP status paired with the envelope body.
pub struct Reply<T> {
    status: StatusCode,
    body: Envelope<T>,
}

impl<T> Reply<T> {
    /// `200 OK` carrying `data`.
    pub const fn ok(data: T) -> Self {
        Self {
            status: StatusCode::OK,
            body: Envelope::ok(data),
        }
    }

    /// A failure envelope under `status`.
    #[must_use]
    pub const fn fail(status: StatusCode, error: ApiError) -> Self {
        Self {
            status,
            body: Envelope::Err(error),
        }
    }
}

impl<T: Serialize> IntoResponse for Reply<T> {
    fn into_response(self) -> Response {
        // Serialized by hand rather than through `Json` so a serialization failure
        // is still an envelope: `Json`'s own error path writes a bare text body,
        // which would be the one response on this surface that isn't the contract.
        match serde_json::to_vec(&self.body) {
            Ok(bytes) => (
                self.status,
                [(header::CONTENT_TYPE, "application/json")],
                bytes,
            )
                .into_response(),
            Err(e) => error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::new(
                    ErrorCode::Error,
                    format!("response serialization failed: {e}"),
                ),
            ),
        }
    }
}

/// A failure response with no payload type in hand — the guards and the router
/// fallbacks, which answer before any route's `data` type is known.
#[must_use]
pub fn error_response(status: StatusCode, error: ApiError) -> Response {
    Reply::<Value>::fail(status, error).into_response()
}

#[cfg(test)]
mod tests {
    use super::{Reply, error_response};
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use grove_api::routes::{HealthData, HealthStatus};
    use grove_api::{ApiError, ErrorCode};
    use serde_json::{Value, json};

    async fn body_of(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn a_success_reply_is_the_envelope_at_200() {
        let reply = Reply::ok(HealthData {
            status: HealthStatus::Ready,
            version: "0.1.0".into(),
            home: "/home/.grove".into(),
        });
        let response = reply.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
        assert_eq!(
            body_of(response).await,
            json!({"ok": true, "data": {"status": "ready", "version": "0.1.0", "home": "/home/.grove"}})
        );
    }

    #[tokio::test]
    async fn a_failure_reply_carries_the_code_message_and_data() {
        let response = error_response(
            StatusCode::NOT_FOUND,
            ApiError::new(ErrorCode::NotFound, "root not declared")
                .with_data(json!({"slug": "o/r"})),
        );
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_of(response).await,
            json!({"ok": false, "error": {
                "code": "not_found",
                "message": "root not declared",
                "data": {"slug": "o/r"}
            }})
        );
    }
}
