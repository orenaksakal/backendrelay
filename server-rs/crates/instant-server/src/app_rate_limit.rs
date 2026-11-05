use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{Method, header, request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool};

const GENERIC_RATE_LIMIT_MESSAGE: &str = "You're making too many requests. Contact the operator of this deployment if this keeps happening.";
// Matches Axum's default Bytes/Json extractor limit used by the inspected routes.
const JSON_BODY_LIMIT: usize = 2 * 1_024 * 1_024;

/// Dynamic emergency gate shared by HTTP and realtime app operations.
pub type AppRateLimitFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>>;

pub trait AppRateLimitPolicy: Send + Sync + 'static {
    fn check_app_ids<'a>(&'a self, app_ids: &'a [Uuid]) -> AppRateLimitFuture<'a>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllAppRateLimits;

impl AppRateLimitPolicy for AllowAllAppRateLimits {
    fn check_app_ids<'a>(&'a self, _app_ids: &'a [Uuid]) -> AppRateLimitFuture<'a> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Reads the live config-app `rate-limited-apps` collection for every request.
pub struct PostgresAppRateLimitPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
    administrator_email: Option<String>,
    control_plane: Option<Arc<crate::ControlPlaneCache>>,
}

impl PostgresAppRateLimitPolicy {
    #[must_use]
    pub fn new(client: Client, administrator_email: Option<String>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
            administrator_email,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, administrator_email: Option<String>) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            administrator_email,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self.control_plane = None;
        self
    }

    async fn read_rate_limited(&self, app_ids: &[Uuid]) -> Result<bool, ApiError> {
        let app_ids = app_ids.iter().map(Uuid::to_string).collect::<Vec<_>>();
        let client = self.client.acquire().await.map_err(database_error)?;
        Ok(client
            .query_typed_cached_opt(
                "SELECT 1
                 FROM attrs app_attr
                 JOIN triples app_value
                   ON app_value.app_id = $1 AND app_value.attr_id = app_attr.id
                 WHERE app_attr.app_id = $1
                   AND app_attr.etype = 'rate-limited-apps'
                   AND app_attr.label = 'appId'
                   AND app_value.value #>> '{}' = ANY($2)
                 LIMIT 1",
                &[
                    (&self.config_app_id, Type::UUID),
                    (&app_ids, Type::TEXT_ARRAY),
                ],
            )
            .await
            .map_err(database_error)?
            .is_some())
    }

    /// Serves the config-app collection from a snapshot evicted by any write
    /// to the config app.
    pub(crate) fn with_control_plane_cache(mut self, cache: Arc<crate::ControlPlaneCache>) -> Self {
        assert!(cache.matches_config_app(self.config_app_id));
        self.control_plane = Some(cache);
        self
    }
}

impl AppRateLimitPolicy for PostgresAppRateLimitPolicy {
    fn check_app_ids<'a>(&'a self, app_ids: &'a [Uuid]) -> AppRateLimitFuture<'a> {
        Box::pin(async move {
            if app_ids.is_empty() {
                return Ok(());
            }
            let limited = match &self.control_plane {
                Some(cache) => {
                    cache
                        .rate_limited(app_ids, || self.read_rate_limited(app_ids))
                        .await?
                }
                None => self.read_rate_limited(app_ids).await?,
            };
            if limited {
                Err(rate_limit_error(self.administrator_email.as_deref()))
            } else {
                Ok(())
            }
        })
    }
}

/// Selective middleware matching the JVM's `with-rate-limiting` route wrappers.
pub async fn app_rate_limit_middleware<P>(
    State(policy): State<Arc<P>>,
    request: Request,
    next: Next,
) -> Response
where
    P: AppRateLimitPolicy,
{
    if !is_rate_limited_route(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let inspect_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or_default().trim();
            media_type == "application/json" || media_type.ends_with("+json")
        });
    let (body, body_json) = if inspect_json {
        if parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > JSON_BODY_LIMIT as u64)
        {
            return json_body_buffer_error("length limit exceeded");
        }
        match to_bytes(body, JSON_BODY_LIMIT).await {
            Ok(bytes) => {
                let value = serde_json::from_slice::<Value>(&bytes).ok();
                (Body::from(bytes), value)
            }
            Err(error) => return json_body_buffer_error(error),
        }
    } else {
        (body, None)
    };
    let app_ids = request_app_ids(&parts, body_json.as_ref());
    drop(body_json);
    if let Err(error) = policy.check_app_ids(&app_ids).await {
        return error.into_response();
    }
    next.run(Request::from_parts(parts, body)).await
}

fn request_app_ids(parts: &request::Parts, body: Option<&Value>) -> Vec<Uuid> {
    let mut app_ids = BTreeSet::new();
    for name in ["app-id", "app_id"] {
        if let Some(app_id) = parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_uuid)
        {
            app_ids.insert(app_id);
        }
    }
    if let Some(query) = parts.uri.query() {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if key == "app_id"
                && let Some(app_id) = parse_uuid(&value)
            {
                app_ids.insert(app_id);
            }
        }
    }
    for segment in parts.uri.path().split('/') {
        if let Some(app_id) = parse_uuid(segment) {
            app_ids.insert(app_id);
        }
    }
    if let Some(body) = body.and_then(Value::as_object) {
        for name in ["app-id", "app_id"] {
            if let Some(app_id) = body.get(name).and_then(Value::as_str).and_then(parse_uuid) {
                app_ids.insert(app_id);
            }
        }
    }
    app_ids.into_iter().collect()
}

fn is_rate_limited_route(method: &Method, path: &str) -> bool {
    matches!(
        (method.as_str(), path),
        (
            "POST",
            "/runtime/auth/send_magic_code"
                | "/runtime/auth/verify_magic_code"
                | "/runtime/auth/verify_refresh_token"
                | "/runtime/auth/sign_in_guest"
                | "/runtime/framework/query"
                | "/admin/query"
                | "/admin/sse"
                | "/admin/sse/push"
                | "/admin/subscribe-query"
                | "/admin/transact"
                | "/admin/query_perms_check"
                | "/admin/transact_perms_check"
                | "/admin/storage/signed-upload-url"
                | "/admin/storage/files/delete"
                | "/storage/signed-upload-url"
        ) | (
            "GET",
            "/runtime/oauth/start"
                | "/admin/storage/signed-download-url"
                | "/admin/storage/files"
                | "/storage/signed-download-url"
        ) | ("PUT", "/admin/storage/upload" | "/storage/upload")
            | ("DELETE", "/admin/storage/files" | "/storage/files")
    ) || runtime_oauth_path(method, path)
}

fn runtime_oauth_path(method: &Method, path: &str) -> bool {
    let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
    segments.len() == 4
        && segments[0] == "runtime"
        && Uuid::parse_str(segments[1]).is_ok()
        && segments[2] == "oauth"
        && ((method == Method::GET && segments[3] == "start")
            || (method == Method::POST && segments[3] == "token"))
}

fn parse_uuid(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value.trim()).ok()
}

pub(crate) fn rate_limit_error(administrator_email: Option<&str>) -> ApiError {
    let message = administrator_email.map_or_else(
        || GENERIC_RATE_LIMIT_MESSAGE.to_owned(),
        |email| {
            format!(
                "You're making too many requests. Contact the deployment administrator at {email}."
            )
        },
    );
    ApiError::new(ApiErrorType::RateLimited, message)
}

fn json_body_buffer_error(error: impl std::fmt::Display) -> Response {
    ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body")
        .with_hint(serde_json::json!({
            "body": format!("Failed to buffer the request body: {error}")
        }))
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::middleware;
    use axum::routing::post;
    use futures_util::stream;
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;

    struct TestPolicy {
        limited: Uuid,
    }

    impl AppRateLimitPolicy for TestPolicy {
        fn check_app_ids<'a>(&'a self, app_ids: &'a [Uuid]) -> AppRateLimitFuture<'a> {
            Box::pin(async move {
                if app_ids.contains(&self.limited) {
                    Err(rate_limit_error(Some("admin@example.test")))
                } else {
                    Ok(())
                }
            })
        }
    }

    fn parts(method: Method, uri: &str, headers: HeaderMap) -> request::Parts {
        let request = Request::builder()
            .method(method)
            .uri(uri.parse::<Uri>().unwrap())
            .body(Body::empty())
            .unwrap();
        let (mut parts, _) = request.into_parts();
        parts.headers = headers;
        parts
    }

    fn test_app(limited: Uuid, handler_runs: Arc<AtomicUsize>) -> Router {
        Router::new()
            .route(
                "/runtime/auth/sign_in_guest",
                post(move |body: Bytes| {
                    let handler_runs = Arc::clone(&handler_runs);
                    async move {
                        handler_runs.fetch_add(1, Ordering::SeqCst);
                        body
                    }
                }),
            )
            .layer(middleware::from_fn_with_state(
                Arc::new(TestPolicy { limited }),
                app_rate_limit_middleware::<TestPolicy>,
            ))
    }

    async fn assert_oversized_response(response: Response) {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 1_024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({
                "type": "param-malformed",
                "message": "Malformed JSON body",
                "hint": {
                    "body": "Failed to buffer the request body: length limit exceeded"
                }
            })
        );
    }

    #[test]
    fn route_inventory_matches_the_jvm_wrappers() {
        assert!(is_rate_limited_route(
            &Method::POST,
            "/runtime/auth/send_magic_code"
        ));
        assert!(is_rate_limited_route(
            &Method::GET,
            &format!("/runtime/{}/oauth/start", Uuid::nil())
        ));
        assert!(is_rate_limited_route(
            &Method::DELETE,
            "/admin/storage/files"
        ));
        assert!(!is_rate_limited_route(
            &Method::POST,
            "/runtime/oauth/token"
        ));
        assert!(!is_rate_limited_route(
            &Method::PUT,
            &format!("/storage/{}/consume-upload-url", Uuid::nil())
        ));
        assert!(!is_rate_limited_route(&Method::POST, "/runtime/signout"));
    }

    #[test]
    fn extraction_checks_every_request_location_and_ignores_invalid_values() {
        let header_id = Uuid::new_v4();
        let query_id = Uuid::new_v4();
        let path_id = Uuid::new_v4();
        let body_id = Uuid::new_v4();
        let mut headers = HeaderMap::new();
        headers.insert("app-id", header_id.to_string().parse().unwrap());
        headers.insert("app_id", "invalid".parse().unwrap());
        let parts = parts(
            Method::POST,
            &format!("/runtime/{path_id}/oauth/token?app_id={query_id}"),
            headers,
        );

        let app_ids = request_app_ids(&parts, Some(&json!({"app_id": body_id})));

        assert_eq!(
            app_ids.into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([header_id, query_id, path_id, body_id])
        );
    }

    #[test]
    fn rate_limit_message_names_the_configured_administrator() {
        assert_eq!(
            rate_limit_error(None).body().message,
            GENERIC_RATE_LIMIT_MESSAGE
        );
        assert_eq!(
            rate_limit_error(Some("ops@example.test")).body().message,
            "You're making too many requests. Contact the deployment administrator at ops@example.test."
        );
    }

    #[tokio::test]
    async fn middleware_checks_body_even_when_a_different_header_is_allowed() {
        let allowed = Uuid::new_v4();
        let limited = Uuid::new_v4();
        let app = Router::new()
            .route(
                "/runtime/auth/sign_in_guest",
                post(|| async { "handler ran" }),
            )
            .layer(middleware::from_fn_with_state(
                Arc::new(TestPolicy { limited }),
                app_rate_limit_middleware::<TestPolicy>,
            ));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/runtime/auth/sign_in_guest")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("app-id", allowed.to_string())
                    .body(Body::from(json!({"app-id": limited}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 429);
    }

    #[tokio::test]
    async fn middleware_replays_a_body_at_the_json_limit_byte_for_byte() {
        let limited = Uuid::new_v4();
        let handler_runs = Arc::new(AtomicUsize::new(0));
        let app = test_app(limited, Arc::clone(&handler_runs));
        let mut payload = b"{}".to_vec();
        payload.resize(JSON_BODY_LIMIT, b' ');
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/runtime/auth/sign_in_guest")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        let body = to_bytes(response.into_body(), JSON_BODY_LIMIT)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), payload);
        assert_eq!(handler_runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn middleware_rejects_declared_oversized_json_before_the_handler() {
        let handler_runs = Arc::new(AtomicUsize::new(0));
        let app = test_app(Uuid::new_v4(), Arc::clone(&handler_runs));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/runtime/auth/sign_in_guest")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::CONTENT_LENGTH, JSON_BODY_LIMIT + 1)
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_oversized_response(response).await;
        assert_eq!(handler_runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn middleware_rejects_chunked_json_overflow_before_the_handler() {
        let handler_runs = Arc::new(AtomicUsize::new(0));
        let app = test_app(Uuid::new_v4(), Arc::clone(&handler_runs));
        let chunks = [
            Bytes::from_static(b"{}"),
            Bytes::from(vec![b' '; JSON_BODY_LIMIT - 2]),
            Bytes::from_static(b" "),
        ];
        let body = Body::from_stream(stream::iter(chunks.map(Ok::<_, Infallible>)));
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/runtime/auth/sign_in_guest")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::TRANSFER_ENCODING, "chunked")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_oversized_response(response).await;
        assert_eq!(handler_runs.load(Ordering::SeqCst), 0);
    }
}
