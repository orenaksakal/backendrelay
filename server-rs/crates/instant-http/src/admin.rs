use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Query, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{ApiError, ApiErrorType, ResolvedClientIp};

/// The user context selected by the admin SDK's impersonation headers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminActor {
    Admin,
    Guest,
    RefreshToken(Uuid),
    Email(String),
}

/// Authentication and application routing information shared by admin data calls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminRequestContext {
    pub app_id: Uuid,
    pub bearer_token: Option<String>,
    pub actor: AdminActor,
    /// Address resolved through trusted proxies, used by `request.ip` rules.
    pub client_ip: Option<IpAddr>,
}

/// Wire request accepted by `POST /admin/query`.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminQueryRequest {
    pub query: Map<String, Value>,
    pub inference: bool,
    pub versions: Option<Value>,
}

/// Wire request accepted by `POST /admin/transact`.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminTransactRequest {
    pub steps: Vec<Value>,
    pub throw_on_missing_attrs: bool,
}

/// Wire request accepted by `POST /admin/query_perms_check`.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminQueryPermissionsRequest {
    pub query: Map<String, Value>,
    pub inference: bool,
    pub ip_override: Option<String>,
    pub origin_override: Option<String>,
}

/// Wire request accepted by `POST /admin/transact_perms_check`.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminTransactPermissionsRequest {
    pub steps: Vec<Value>,
    pub rules_override: Option<Value>,
    pub dangerously_commit: bool,
    pub throw_on_missing_attrs: bool,
    pub ip_override: Option<String>,
    pub origin_override: Option<String>,
}

/// Result of an admin transaction, matching the JVM's public response envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdminTransactResponse {
    #[serde(rename = "tx-id")]
    pub tx_id: i64,
}

/// Application operations behind the two primary admin data routes.
pub trait AdminDataBackend: Send + Sync + 'static {
    type PreparedQueryPermissions: Send;

    fn query(
        &self,
        context: AdminRequestContext,
        request: AdminQueryRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn transact(
        &self,
        context: AdminRequestContext,
        request: AdminTransactRequest,
    ) -> impl Future<Output = Result<AdminTransactResponse, ApiError>> + Send;

    fn prepare_query_permissions(
        &self,
        context: AdminRequestContext,
        rules_override: Option<Value>,
    ) -> impl Future<Output = Result<Self::PreparedQueryPermissions, ApiError>> + Send;

    fn query_permissions(
        &self,
        prepared: Self::PreparedQueryPermissions,
        request: AdminQueryPermissionsRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn transact_permissions(
        &self,
        context: AdminRequestContext,
        request: AdminTransactPermissionsRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

/// Builds the primary and permission-debug official admin SDK data routes.
pub fn admin_data_router<B>(backend: Arc<B>) -> Router
where
    B: AdminDataBackend,
{
    crate::body_limit::limit_body(
        Router::new()
            .route("/admin/query", post(query::<B>))
            .route("/admin/transact", post(transact::<B>))
            .route("/admin/query_perms_check", post(query_permissions::<B>))
            .route(
                "/admin/transact_perms_check",
                post(transact_permissions::<B>),
            ),
        crate::body_limit::MANAGEMENT_BODY_LIMIT,
        "Admin request body exceeds the 1 MiB limit",
    )
    .with_state(backend)
}

async fn query<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminDataBackend,
{
    let params = query_params(params)?;
    let context = request_context_with_client_ip(&headers, &params, client_ip)?;
    let body = json_object(payload)?;
    let query = required_object(&body, "query")?.clone();
    let request = AdminQueryRequest {
        query,
        inference: body.get("inference?").is_some_and(clojure_truthy),
        versions: body.get("versions").cloned(),
    };
    backend.query(context, request).await.map(Json)
}

async fn transact<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<AdminTransactResponse>, ApiError>
where
    B: AdminDataBackend,
{
    let params = query_params(params)?;
    let body = json_object(payload)?;
    let steps = required_collection(&body, "steps")?;
    // Reject old atomic-enqueue calls rather than silently committing only the data write.
    if body.contains_key("enqueue") {
        return Err(ApiError::new(
            ApiErrorType::ValidationFailed,
            "enqueue is no longer supported",
        ));
    }
    // The JVM validates the required transaction collection before resolving
    // the app or bearer token. Maps pass that collection check and fail later
    // in transaction coercion, after authentication.
    let context = request_context_with_client_ip(&headers, &params, client_ip)?;
    let request = AdminTransactRequest {
        steps: steps
            .as_array()
            .cloned()
            .ok_or_else(|| malformed_parameter("[body steps]", steps))?,
        throw_on_missing_attrs: body
            .get("throw-on-missing-attrs?")
            .is_some_and(clojure_truthy),
    };
    Ok(Json(backend.transact(context, request).await?))
}

async fn query_permissions<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminDataBackend,
{
    let params = query_params(params)?;
    let context = request_context_with_client_ip(&headers, &params, client_ip)?;
    let Json(body_value) = payload.map_err(|error| {
        ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body")
            .with_hint(serde_json::json!({"body": error.body_text()}))
    })?;
    let (inference, rules_override) = body_value.as_object().map_or((false, None), |body| {
        (
            body.get("inference?").is_some_and(clojure_truthy),
            optional_non_null(body, "rules-override"),
        )
    });
    let prepared = backend
        .prepare_query_permissions(context, rules_override)
        .await?;
    let body = body_value
        .as_object()
        .ok_or_else(|| malformed_parameter("[body]", &body_value))?;
    let ip_override = optional_non_blank_string(body, "ip-override")?;
    let origin_override = optional_non_blank_string(body, "origin-override")?;
    let query = required_object(body, "query")?.clone();
    let request = AdminQueryPermissionsRequest {
        query,
        inference,
        ip_override,
        origin_override,
    };
    backend.query_permissions(prepared, request).await.map(Json)
}

async fn transact_permissions<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminDataBackend,
{
    let params = query_params(params)?;
    let context = request_context_with_client_ip(&headers, &params, client_ip)?;
    let body = json_object(payload)?;
    let request = AdminTransactPermissionsRequest {
        steps: required_array(&body, "steps")?.to_vec(),
        rules_override: optional_non_null(&body, "rules-override"),
        dangerously_commit: body
            .get("dangerously-commit-tx")
            .is_some_and(clojure_truthy),
        throw_on_missing_attrs: body
            .get("throw-on-missing-attrs?")
            .is_some_and(clojure_truthy),
        ip_override: optional_non_blank_string(&body, "ip-override")?,
        origin_override: optional_non_blank_string(&body, "origin-override")?,
    };
    backend
        .transact_permissions(context, request)
        .await
        .map(Json)
}

pub(crate) fn request_context(
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> Result<AdminRequestContext, ApiError> {
    let raw_app_id = header(headers, "app-id")
        .or_else(|| params.get("app_id").cloned())
        .ok_or_else(|| missing_parameter("[headers app-id] or [query-params app_id]"))?;
    let app_id = raw_app_id.parse().map_err(|_| {
        malformed_parameter(
            "[headers app-id] or [query-params app_id]",
            &Value::String(raw_app_id),
        )
    })?;

    let bearer_token = header(headers, "authorization")
        .map(|authorization| {
            authorization
                .split_once(' ')
                .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("Bearer").then_some(token))
                .map(str::trim)
                .filter(|token| !token.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    malformed_parameter("[headers authorization]", &Value::String(authorization))
                        .unauthorized()
                })
        })
        .transpose()?;

    let actor = if let Some(token) = header(headers, "as-token") {
        AdminActor::RefreshToken(
            token
                .parse()
                .map_err(|_| malformed_parameter("[asUser token]", &Value::String(token)))?,
        )
    } else if let Some(email) = header(headers, "as-email") {
        AdminActor::Email(email)
    } else if header(headers, "as-guest").is_some() {
        AdminActor::Guest
    } else {
        AdminActor::Admin
    };

    if matches!(actor, AdminActor::Admin | AdminActor::Email(_)) && bearer_token.is_none() {
        return Err(missing_parameter("[headers authorization]").unauthorized());
    }

    Ok(AdminRequestContext {
        app_id,
        bearer_token,
        actor,
        client_ip: None,
    })
}

fn request_context_with_client_ip(
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    client_ip: Option<Extension<ResolvedClientIp>>,
) -> Result<AdminRequestContext, ApiError> {
    let mut context = request_context(headers, params)?;
    context.client_ip = client_ip.and_then(|Extension(client_ip)| client_ip.0);
    Ok(context)
}

fn header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

pub(crate) fn query_params(
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<HashMap<String, String>, ApiError> {
    params.map(|Query(params)| params).map_err(|error| {
        ApiError::new(ApiErrorType::ParamMalformed, "Malformed query parameters")
            .with_hint(serde_json::json!({"original-input": error.body_text()}))
    })
}

pub(crate) fn json_object(
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Map<String, Value>, ApiError> {
    let Json(value) = payload.map_err(|error| {
        ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body")
            .with_hint(serde_json::json!({"body": error.body_text()}))
    })?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| malformed_parameter("[body]", &value))
}

pub(crate) fn required_object<'a>(
    body: &'a Map<String, Value>,
    key: &str,
) -> Result<&'a Map<String, Value>, ApiError> {
    let value = body
        .get(key)
        .ok_or_else(|| missing_parameter(&format!("[body {key}]")))?;
    value
        .as_object()
        .ok_or_else(|| malformed_parameter(&format!("[body {key}]"), value))
}

fn required_array<'a>(body: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], ApiError> {
    let value = body
        .get(key)
        .ok_or_else(|| missing_parameter(&format!("[body {key}]")))?;
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| malformed_parameter(&format!("[body {key}]"), value))
}

fn required_collection<'a>(body: &'a Map<String, Value>, key: &str) -> Result<&'a Value, ApiError> {
    let value = body
        .get(key)
        .ok_or_else(|| missing_parameter(&format!("[body {key}]")))?;
    if value.is_array() || value.is_object() {
        Ok(value)
    } else {
        Err(malformed_parameter(&format!("[body {key}]"), value))
    }
}

fn optional_non_null(body: &Map<String, Value>, key: &str) -> Option<Value> {
    body.get(key).filter(|value| !value.is_null()).cloned()
}

fn optional_non_blank_string(
    body: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, ApiError> {
    let Some(value) = body.get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    value
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .map(Some)
        .ok_or_else(|| malformed_parameter(&format!("[body {key}]"), value))
}

pub(crate) fn clojure_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn missing_parameter(path: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {path}"),
    )
}

fn malformed_parameter(path: &str, original_input: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: {path}"),
    )
    .with_hint(serde_json::json!({
        "in": path,
        "original-input": original_input,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn admin_bearer_authorization_scheme_is_case_insensitive() {
        let app_id = Uuid::new_v4();
        for scheme in ["Bearer", "bearer", "BEARER", "bEaReR"] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                format!("{scheme} admin-token").parse().unwrap(),
            );
            headers.insert("app-id", app_id.to_string().parse().unwrap());
            let context = request_context(&headers, &HashMap::new()).unwrap();
            assert_eq!(context.bearer_token.as_deref(), Some("admin-token"));
        }
    }

    #[test]
    fn admin_bearer_credentials_allow_http_whitespace_separator() {
        let app_id = Uuid::new_v4();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer   token".parse().unwrap());
        headers.insert("app-id", app_id.to_string().parse().unwrap());
        assert_eq!(
            request_context(&headers, &HashMap::new())
                .unwrap()
                .bearer_token
                .as_deref(),
            Some("token")
        );
    }

    #[derive(Default)]
    struct FakeBackend {
        query: Mutex<Option<(AdminRequestContext, AdminQueryRequest)>>,
        transact: Mutex<Option<(AdminRequestContext, AdminTransactRequest)>>,
        query_permissions: Mutex<
            Option<(
                AdminRequestContext,
                Option<Value>,
                AdminQueryPermissionsRequest,
            )>,
        >,
        query_permissions_preflight_error: Mutex<Option<ApiError>>,
        transact_permissions: Mutex<Option<(AdminRequestContext, AdminTransactPermissionsRequest)>>,
    }

    impl AdminDataBackend for FakeBackend {
        type PreparedQueryPermissions = (AdminRequestContext, Option<Value>);

        fn query(
            &self,
            context: AdminRequestContext,
            request: AdminQueryRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            *self.query.lock().unwrap() = Some((context, request));
            std::future::ready(Ok(serde_json::json!({"goals": [{"title": "ship"}]})))
        }

        fn transact(
            &self,
            context: AdminRequestContext,
            request: AdminTransactRequest,
        ) -> impl Future<Output = Result<AdminTransactResponse, ApiError>> + Send {
            *self.transact.lock().unwrap() = Some((context, request));
            std::future::ready(Ok(AdminTransactResponse { tx_id: 42 }))
        }

        fn prepare_query_permissions(
            &self,
            context: AdminRequestContext,
            rules_override: Option<Value>,
        ) -> impl Future<Output = Result<Self::PreparedQueryPermissions, ApiError>> + Send {
            let result = self
                .query_permissions_preflight_error
                .lock()
                .unwrap()
                .take()
                .map_or_else(|| Ok((context, rules_override)), Err);
            std::future::ready(result)
        }

        fn query_permissions(
            &self,
            prepared: Self::PreparedQueryPermissions,
            request: AdminQueryPermissionsRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            *self.query_permissions.lock().unwrap() = Some((prepared.0, prepared.1, request));
            std::future::ready(Ok(serde_json::json!({
                "check-results": [],
                "result": {"goals": []},
                "rule-wheres": {}
            })))
        }

        fn transact_permissions(
            &self,
            context: AdminRequestContext,
            request: AdminTransactPermissionsRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            *self.transact_permissions.lock().unwrap() = Some((context, request));
            std::future::ready(Ok(serde_json::json!({
                "tx-id": 43,
                "all-checks-ok?": true,
                "committed?": false,
                "check-results": []
            })))
        }
    }

    #[tokio::test]
    async fn official_admin_query_contract_round_trips() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let router = admin_data_router(backend.clone());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/query?app_id={app_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::from(r#"{"query":{"goals":{}},"inference?":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"goals":[{"title":"ship"}]}"#
        );
        let (context, request) = backend.query.lock().unwrap().take().unwrap();
        assert_eq!(context.app_id, app_id);
        assert_eq!(context.bearer_token.as_deref(), Some("secret"));
        assert_eq!(context.actor, AdminActor::Admin);
        assert!(request.inference);
        assert!(request.query.contains_key("goals"));
    }

    #[tokio::test]
    async fn admin_query_context_carries_the_resolved_client_ip() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = admin_data_router(backend.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/query")
                    .header("app-id", app_id.to_string())
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header(header::CONTENT_TYPE, "application/json")
                    .extension(ResolvedClientIp(Some(
                        "203.0.113.9".parse::<IpAddr>().unwrap(),
                    )))
                    .body(Body::from(r#"{"query":{"goals":{}}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (context, _) = backend.query.lock().unwrap().take().unwrap();
        assert_eq!(
            context.client_ip,
            Some("203.0.113.9".parse::<IpAddr>().unwrap())
        );
    }

    #[tokio::test]
    async fn transact_preserves_sdk_keys_and_jvm_truthiness() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let router = admin_data_router(backend.clone());
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/transact")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("app-id", app_id.to_string())
                    .header("as-guest", "true")
                    .body(Body::from(
                        r#"{"steps":[["delete-entity","goals","00000000-0000-0000-0000-000000000000"]],"throw-on-missing-attrs?":"truthy"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"tx-id":42}"#
        );
        let (context, request) = backend.transact.lock().unwrap().take().unwrap();
        assert_eq!(context.actor, AdminActor::Guest);
        assert_eq!(context.bearer_token, None);
        assert!(request.throw_on_missing_attrs);
    }

    #[tokio::test]
    async fn removed_enqueue_is_rejected_before_any_data_write() {
        for enqueue in [
            serde_json::json!([]),
            serde_json::json!([{"task":"notify","input":{}}]),
        ] {
            let backend = Arc::new(FakeBackend::default());
            let response = admin_data_router(Arc::clone(&backend))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/admin/transact")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header("app-id", Uuid::new_v4().to_string())
                        .body(Body::from(
                            serde_json::json!({"steps": [], "enqueue": enqueue}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(backend.transact.lock().unwrap().is_none());
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(body["message"], "enqueue is no longer supported");
        }
    }

    #[tokio::test]
    async fn transact_validates_steps_before_app_and_bearer_credentials() {
        let response = admin_data_router(Arc::new(FakeBackend::default()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/transact")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"type":"param-missing","message":"Missing parameter: [body steps]"}"#
        );

        let response = admin_data_router(Arc::new(FakeBackend::default()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/transact")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"steps":{}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body["message"],
            "Missing parameter: [headers app-id] or [query-params app_id]"
        );
    }

    #[tokio::test]
    async fn missing_query_uses_instant_parameter_error() {
        let app_id = Uuid::new_v4();
        let response = admin_data_router(Arc::new(FakeBackend::default()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/query?app_id={app_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains(r#""type":"param-missing""#)
        );
    }

    #[tokio::test]
    async fn admin_data_routes_reject_non_post_methods_before_backend_dispatch() {
        let backend = Arc::new(FakeBackend::default());
        let router = admin_data_router(backend.clone());
        for path in [
            "/admin/query",
            "/admin/transact",
            "/admin/query_perms_check",
            "/admin/transact_perms_check",
        ] {
            let response = router
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
        }
        assert!(backend.query.lock().unwrap().is_none());
        assert!(backend.transact.lock().unwrap().is_none());
        assert!(backend.query_permissions.lock().unwrap().is_none());
        assert!(backend.transact_permissions.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn permission_debug_routes_preserve_sdk_and_jvm_contracts() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let query_response = admin_data_router(backend.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/query_perms_check?app_id={app_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header("as-guest", "true")
                    .body(Body::from(
                        r#"{"query":{"goals":{}},"rules-override":{"goals":{"allow":{"view":"true"}}},"inference?":"yes","ip-override":"127.0.0.1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(query_response.status(), StatusCode::OK);
        let (context, rules_override, request) =
            backend.query_permissions.lock().unwrap().take().unwrap();
        assert_eq!(context.actor, AdminActor::Guest);
        assert!(request.inference);
        assert_eq!(request.ip_override.as_deref(), Some("127.0.0.1"));
        assert!(rules_override.is_some());

        let transact_response = admin_data_router(backend.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/transact_perms_check?app_id={app_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header("as-email", "user@example.com")
                    .body(Body::from(
                        r#"{"steps":[],"dangerously-commit-tx":"yes","origin-override":"https://example.com"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(transact_response.status(), StatusCode::OK);
        let (context, request) = backend.transact_permissions.lock().unwrap().take().unwrap();
        assert_eq!(
            context.actor,
            AdminActor::Email("user@example.com".to_owned())
        );
        assert!(request.dangerously_commit);
        assert_eq!(
            request.origin_override.as_deref(),
            Some("https://example.com")
        );
    }

    #[tokio::test]
    async fn query_permission_preflight_precedes_late_request_validation() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        for body in [
            r"[]",
            r#"{"ip-override":1}"#,
            r#"{"origin-override":1}"#,
            r"{}",
            r#"{"query":[]}"#,
        ] {
            *backend.query_permissions_preflight_error.lock().unwrap() =
                Some(ApiError::new(ApiErrorType::RateLimited, "limited"));
            let response = admin_data_router(Arc::clone(&backend))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/admin/query_perms_check?app_id={app_id}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::AUTHORIZATION, "Bearer secret")
                        .header("as-guest", "true")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(std::str::from_utf8(&body).unwrap().contains("rate-limited"));
            assert!(backend.query_permissions.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn oversized_pre_auth_admin_bodies_are_rejected_with_413() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let body = "x".repeat(crate::body_limit::MANAGEMENT_BODY_LIMIT + 1);
        let response = admin_data_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/query?app_id={app_id}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::CONTENT_LENGTH, body.len())
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "parameter-limit-exceeded");
        assert_eq!(
            body["message"],
            "Admin request body exceeds the 1 MiB limit"
        );
        assert!(backend.query.lock().unwrap().is_none());
    }
}
