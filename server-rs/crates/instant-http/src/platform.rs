use std::future::Future;
use std::sync::Arc;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{ApiError, ApiErrorType};

/// Authenticated identity and WAL position addressed by a webhook replay request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookPayloadRequest {
    pub app_id: Uuid,
    pub webhook_id: Uuid,
    pub isn: String,
    pub bearer_token: String,
}

/// Stable payload replay response consumed by Instant webhook receivers.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WebhookPayloadResponse {
    pub data: Value,
    #[serde(rename = "idempotencyKey")]
    pub idempotency_key: Uuid,
}

/// Side-effect boundary for the non-product platform routes.
pub trait PlatformBackend: Send + Sync + 'static {
    fn webhook_jwks(&self) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn webhook_payload(
        &self,
        request: WebhookPayloadRequest,
    ) -> impl Future<Output = Result<WebhookPayloadResponse, ApiError>> + Send;
}

/// Routes for public webhook verification and replay.
pub fn platform_router<B>(backend: Arc<B>) -> Router
where
    B: PlatformBackend,
{
    Router::new()
        .route("/.well-known/webhooks/jwks.json", get(webhook_jwks::<B>))
        .route(
            "/webhooks/payload/{app_id}/{webhook_id}/{*isn}",
            get(webhook_payload::<B>),
        )
        .with_state(backend)
}

/// CSP demonstration page retained from the JVM server's operational test surface.
pub fn demo_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/demo/csp", get(csp_demo))
}

async fn webhook_jwks<B>(State(backend): State<Arc<B>>) -> Result<Json<Value>, ApiError>
where
    B: PlatformBackend,
{
    backend.webhook_jwks().await.map(Json)
}

async fn webhook_payload<B>(
    State(backend): State<Arc<B>>,
    path: Result<Path<(Uuid, Uuid, String)>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError>
where
    B: PlatformBackend,
{
    let Path((app_id, webhook_id, isn)) = path.map_err(|error| {
        ApiError::new(
            ApiErrorType::ParamMalformed,
            "Malformed webhook payload path",
        )
        .with_hint(serde_json::json!({"error": error.body_text()}))
    })?;
    let bearer_token = bearer_token(&headers)?;
    let payload = backend
        .webhook_payload(WebhookPayloadRequest {
            app_id,
            webhook_id,
            isn,
            bearer_token,
        })
        .await?;
    let mut response = Json(payload).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store, private".parse().unwrap());
    headers.insert(header::PRAGMA, "no-cache".parse().unwrap());
    headers.insert(header::EXPIRES, "0".parse().unwrap());
    headers.insert(header::VARY, "Authorization".parse().unwrap());
    Ok(response)
}

async fn csp_demo() -> Html<&'static str> {
    Html(CSP_DEMO_HTML)
}

fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| {
            ApiError::new(ApiErrorType::ParamMissing, "Missing Authorization header").unauthorized()
        })?
        .to_str()
        .map_err(|_| {
            ApiError::new(
                ApiErrorType::ParamMalformed,
                "Malformed Authorization header",
            )
            .unauthorized()
        })?;
    value
        .split_once(' ')
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("Bearer").then_some(token))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorType::ParamMalformed,
                "Expected Bearer authorization",
            )
            .unauthorized()
        })
}

const CSP_DEMO_HTML: &str = r#"<!DOCTYPE html><html lang="en"><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1.0"><title>Test redirect</title></head><body><p>Whitelisted script:</p><div id="box-a"></div><p>Non-whitelisted script:</p><div id="box-b"></div><script type="text/javascript">document.querySelector('#box-a').style.backgroundColor = 'green'</script><script type="text/javascript">document.querySelector('#box-b').style.backgroundColor = 'red'</script></body></html>"#;

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn bearer_authorization_scheme_is_case_insensitive() {
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("{scheme} access-token").parse().unwrap(),
            );
            assert_eq!(bearer_token(&headers).unwrap(), "access-token");
        }
    }

    #[derive(Default)]
    struct Backend {
        payload: Mutex<Option<WebhookPayloadRequest>>,
    }

    impl PlatformBackend for Backend {
        fn webhook_jwks(&self) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(serde_json::json!({"keys": []})))
        }

        fn webhook_payload(
            &self,
            request: WebhookPayloadRequest,
        ) -> impl Future<Output = Result<WebhookPayloadResponse, ApiError>> + Send {
            *self.payload.lock().unwrap() = Some(request);
            std::future::ready(Ok(WebhookPayloadResponse {
                data: serde_json::json!([]),
                idempotency_key: Uuid::nil(),
            }))
        }
    }

    #[tokio::test]
    async fn platform_routes_preserve_jwks_and_replay_headers() {
        let backend = Arc::new(Backend::default());
        let router = platform_router(Arc::clone(&backend));
        let response = router
            .oneshot(
                Request::get("/webhooks/payload/00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/0/328/48953748")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(
            backend.payload.lock().unwrap().as_ref().unwrap().isn,
            "0/328/48953748"
        );
    }
}
