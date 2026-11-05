//! Pre-authentication body caps for the management and admin surface.
//!
//! `RequestBodyLimit` stops reading as soon as the declared or streamed body
//! exceeds the configured limit, so oversized requests never reach an
//! authentication backend. The wrapper below restores the standard JSON error
//! contract for the layer's otherwise empty 413 response.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use tower_http::limit::RequestBodyLimitLayer;

use crate::{ApiError, ApiErrorType};

/// Maximum body size for dashboard, admin and superadmin control routes.
pub const MANAGEMENT_BODY_LIMIT: usize = 1024 * 1024;

/// Caps request bodies for every route already registered on `router`.
pub fn limit_body<S>(router: Router<S>, limit: usize, message: &'static str) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .layer(RequestBodyLimitLayer::new(limit))
        .layer(from_fn_with_state(
            BodyLimitErrorState { message },
            body_limit_error,
        ))
}

#[derive(Clone, Copy)]
struct BodyLimitErrorState {
    message: &'static str,
}

async fn body_limit_error(
    State(state): State<BodyLimitErrorState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::new(ApiErrorType::ParameterLimitExceeded, state.message)
            .with_status(StatusCode::PAYLOAD_TOO_LARGE)
            .into_response();
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::header::CONTENT_LENGTH;
    use axum::routing::post;
    use tower::ServiceExt;

    use super::*;

    async fn echo(body: axum::body::Bytes) -> String {
        body.len().to_string()
    }

    #[tokio::test]
    async fn declared_oversized_bodies_are_rejected_with_the_api_error_contract() {
        let router = limit_body(
            Router::new().route("/limited", post(echo)),
            16,
            "Test body exceeds the 16 byte limit",
        );

        let response = router
            .oneshot(
                Request::post("/limited")
                    .header(CONTENT_LENGTH, "17")
                    .body(Body::from("x".repeat(17)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "parameter-limit-exceeded");
        assert_eq!(body["message"], "Test body exceeds the 16 byte limit");
    }

    #[tokio::test]
    async fn bodies_up_to_the_limit_reach_the_handler() {
        let router = limit_body(
            Router::new().route("/limited", post(echo)),
            16,
            "Test body exceeds the 16 byte limit",
        );

        let response = router
            .oneshot(
                Request::post("/limited")
                    .body(Body::from("x".repeat(16)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(&body[..], b"16");
    }
}
