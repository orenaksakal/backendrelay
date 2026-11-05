use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::{ApiError, ApiErrorType};

pub const DASHBOARD_GATEWAY_HEADER: &str = "x-instant-dashboard-gateway-token";

/// Origin credential held only by the API and the private gateway. Deliberately
/// not Debug/Serialize: this is never a dashboard-user credential or public config.
#[derive(Clone)]
pub struct DashboardGatewayToken(Arc<[u8]>);

impl DashboardGatewayToken {
    /// Accepts a 32–128 character header-safe secret. A configured but invalid
    /// value must abort startup, not silently disable the origin guard.
    #[must_use]
    pub fn new(value: String) -> Option<Self> {
        if !(32..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return None;
        }
        Some(Self(Arc::from(value.into_bytes())))
    }
}

/// Applies to the assembled management route group, not raw URI prefixes.
/// Runtime/Admin SDK routes and public webhook verification remain unchanged.
pub(crate) fn protect_dashboard_routes(
    router: Router,
    token: Option<DashboardGatewayToken>,
) -> Router {
    match token {
        Some(token) => router.route_layer(from_fn_with_state(token, require_gateway)),
        None => router,
    }
}

async fn require_gateway(
    State(token): State<DashboardGatewayToken>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut values = request.headers().get_all(DASHBOARD_GATEWAY_HEADER).iter();
    let authorized = values
        .next()
        .is_some_and(|value| bool::from(token.0.as_ref().ct_eq(value.as_bytes())))
        && values.next().is_none();
    // Neither downstream dashboard handlers nor their tracking/logging should
    // ever receive the gateway credential, even on a successfully admitted call.
    request.headers_mut().remove(DASHBOARD_GATEWAY_HEADER);
    if !authorized {
        let mut response = ApiError::new(
            ApiErrorType::PermissionDenied,
            "Dashboard management is available only through the private gateway.",
        )
        .with_status(StatusCode::FORBIDDEN)
        .into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        return response;
    }
    // Normal user/session/app authorization is still performed downstream.
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderMap, Request};
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "unit-test-only-origin-gateway-secret";

    #[test]
    fn invalid_gateway_credentials_cannot_disable_the_guard() {
        for value in [
            "",
            "short",
            "white space is not a valid gateway credential",
            "💻",
        ] {
            assert!(DashboardGatewayToken::new(value.to_owned()).is_none());
        }
        assert!(DashboardGatewayToken::new("a".repeat(129)).is_none());
        assert!(DashboardGatewayToken::new(TOKEN.to_owned()).is_some());
    }

    #[tokio::test]
    async fn gateway_token_is_required_stripped_and_does_not_replace_bearer_auth() {
        let router = protect_dashboard_routes(
            Router::new().route(
                "/dash/me",
                get(|headers: HeaderMap| async move {
                    assert!(!headers.contains_key(DASHBOARD_GATEWAY_HEADER));
                    if headers.get(header::AUTHORIZATION).is_some() {
                        StatusCode::OK
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }),
            ),
            DashboardGatewayToken::new(TOKEN.to_owned()),
        );
        for value in [
            None,
            Some("spoofed"),
            Some("unit-test-only-origin-gateway-secreu"),
        ] {
            let mut request = Request::builder()
                .uri("/dash/me")
                .header("Tailscale-User-Login", "owner@example.com")
                .header(header::AUTHORIZATION, "Bearer user-session");
            if let Some(value) = value {
                request = request.header(DASHBOARD_GATEWAY_HEADER, value);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            assert!(!String::from_utf8_lossy(&body).contains(TOKEN));
        }
        let duplicate = Request::builder()
            .uri("/dash/me")
            .header(DASHBOARD_GATEWAY_HEADER, TOKEN)
            .header(DASHBOARD_GATEWAY_HEADER, TOKEN)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.clone().oneshot(duplicate).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let admitted = Request::builder()
            .uri("/dash/me")
            .header(DASHBOARD_GATEWAY_HEADER, TOKEN)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.clone().oneshot(admitted).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let signed_in = Request::builder()
            .uri("/dash/me")
            .header(DASHBOARD_GATEWAY_HEADER, TOKEN)
            .header(header::AUTHORIZATION, "Bearer user-session")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(signed_in).await.unwrap().status(),
            StatusCode::OK
        );
    }
}
