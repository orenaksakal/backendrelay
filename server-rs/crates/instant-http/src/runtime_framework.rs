use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{ApiError, ApiErrorType, ResolvedClientIp};

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RuntimeFrameworkRequest {
    pub app_id: Uuid,
    pub refresh_token: Option<Uuid>,
    pub query: Map<String, Value>,
    pub versions: Option<Value>,
    /// Address resolved through trusted proxies, used by `request.ip` rules.
    pub client_ip: Option<IpAddr>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuntimeFrameworkResponse {
    pub result: Value,
    pub attrs: Value,
    #[serde(skip)]
    pub byop_topics: Option<Vec<Value>>,
    #[serde(skip)]
    pub instant_topics: Option<RuntimeFrameworkQueryTopics>,
}

/// Typed invalidation topics handed from query execution to realtime routing.
/// Internal only: responses skip this field, so it never crosses the wire.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeFrameworkQueryTopics {
    pub precise: Vec<instant_datalog::InvalidationTopic>,
    pub coarse: Vec<instant_datalog::InvalidationTopic>,
}

pub trait RuntimeFrameworkBackend: Send + Sync + 'static {
    fn query(
        &self,
        request: RuntimeFrameworkRequest,
    ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send;
}

pub fn runtime_framework_router<B>(backend: Arc<B>) -> Router
where
    B: RuntimeFrameworkBackend,
{
    Router::new()
        .route("/runtime/framework/query", post(framework_query::<B>))
        .with_state(backend)
}

#[derive(Debug, Default, Deserialize)]
struct AppQuery {
    app_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct FrameworkBody {
    query: Map<String, Value>,
    versions: Option<Value>,
}

async fn framework_query<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    Query(query): Query<AppQuery>,
    payload: Result<Json<FrameworkBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<RuntimeFrameworkResponse>, ApiError>
where
    B: RuntimeFrameworkBackend,
{
    let app_id = if let Some(value) = headers.get("app-id") {
        let value = value.to_str().map_err(|_| {
            ApiError::new(
                ApiErrorType::ParamMalformed,
                "Malformed parameter: [headers app-id]",
            )
        })?;
        Uuid::parse_str(value).map_err(|_| {
            ApiError::new(
                ApiErrorType::ParamMalformed,
                "Malformed parameter: [headers app-id]",
            )
        })?
    } else {
        query.app_id.ok_or_else(|| {
            ApiError::new(
                ApiErrorType::ParamMissing,
                "Missing parameter: [headers app-id]",
            )
        })?
    };
    let refresh_token = headers
        .get("authorization")
        .map(|value| {
            let value = value.to_str().map_err(|_| malformed_auth())?;
            let token = value
                .split_once(' ')
                .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("Bearer").then_some(token))
                .filter(|token| !token.is_empty())
                .ok_or_else(malformed_auth)?;
            Uuid::parse_str(token).map_err(|_| malformed_auth())
        })
        .transpose()?;
    let Json(body) = payload.map_err(|error| {
        ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body")
            .with_hint(serde_json::json!({"body": error.body_text()}))
    })?;
    backend
        .query(RuntimeFrameworkRequest {
            app_id,
            refresh_token,
            query: body.query,
            versions: body.versions,
            client_ip: client_ip.and_then(|Extension(client_ip)| client_ip.0),
        })
        .await
        .map(Json)
}

fn malformed_auth() -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        "Malformed parameter: [headers authorization]",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct FakeBackend(Mutex<Vec<RuntimeFrameworkRequest>>);

    impl RuntimeFrameworkBackend for FakeBackend {
        fn query(
            &self,
            request: RuntimeFrameworkRequest,
        ) -> impl Future<Output = Result<RuntimeFrameworkResponse, ApiError>> + Send {
            self.0.lock().unwrap().push(request);
            std::future::ready(Ok(RuntimeFrameworkResponse {
                result: serde_json::json!([{"data": {"datalog-result": {"join-rows": [[]]}}}]),
                attrs: serde_json::json!([]),
                byop_topics: None,
                instant_topics: None,
            }))
        }
    }

    #[tokio::test]
    async fn framework_route_accepts_sdk_headers_and_returns_raw_result() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let response = runtime_framework_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtime/framework/query")
                    .header("app-id", app_id.to_string())
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":{"posts":{}}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert!(body["result"].is_array());
        let requests = backend.0.lock().unwrap();
        assert_eq!(requests[0].app_id, app_id);
        assert_eq!(requests[0].refresh_token, Some(token));
    }

    #[tokio::test]
    async fn malformed_app_id_header_is_not_replaced_by_query_fallback() {
        let backend = Arc::new(FakeBackend::default());
        let response = runtime_framework_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/runtime/framework/query?app_id={}",
                        Uuid::new_v4()
                    ))
                    .header("app-id", "not-a-uuid")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":{}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["type"], "param-malformed");
        assert_eq!(body["message"], "Malformed parameter: [headers app-id]");
        assert!(backend.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn framework_bearer_scheme_is_case_insensitive() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let response = runtime_framework_router(backend.clone())
            .oneshot(
                Request::post("/runtime/framework/query")
                    .header("app-id", app_id.to_string())
                    .header("authorization", format!("bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"query":{}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.0.lock().unwrap()[0].refresh_token, Some(token));
    }
}
