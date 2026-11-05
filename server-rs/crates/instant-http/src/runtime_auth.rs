use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::LazyLock;

use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use axum::routing::post;
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::SocketAddr;
use uuid::Uuid;

use crate::{ApiError, ApiErrorType, ResolvedClientIp};

static EMAIL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*@(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$")
        .expect("reference email expression is valid")
});

/// Public `$users` shape consumed by the official core SDK.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeUser {
    pub id: Uuid,
    pub refresh_token: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(rename = "imageURL", skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub user_type: Option<String>,
    #[serde(rename = "isGuest")]
    pub is_guest: bool,
    #[serde(flatten)]
    pub extra_fields: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeAuthSession {
    pub user: RuntimeUser,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SendMagicCodeRequest {
    #[serde(rename = "app-id")]
    pub app_id: Uuid,
    pub email: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VerifyMagicCodeRequest {
    #[serde(rename = "app-id")]
    pub app_id: Uuid,
    pub email: String,
    pub code: String,
    #[serde(rename = "refresh-token")]
    pub refresh_token: Option<Uuid>,
    #[serde(rename = "extra-fields", default)]
    pub extra_fields: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerifyRefreshTokenRequest {
    #[serde(rename = "app-id")]
    pub app_id: Uuid,
    #[serde(rename = "refresh-token")]
    pub refresh_token: Uuid,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SignInGuestRequest {
    #[serde(rename = "app-id")]
    pub app_id: Uuid,
    #[serde(rename = "extra-fields", default)]
    pub extra_fields: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SignOutRequest {
    pub app_id: Uuid,
    pub refresh_token: Uuid,
}

/// Runtime-auth operations required by the public HTTP routes.
pub trait RuntimeAuthBackend: Send + Sync + 'static {
    fn send_magic_code(
        &self,
        request: SendMagicCodeRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    /// Supplies the resolved client address separately from the JSON body so
    /// the public send route can apply a per-client-IP quota.
    fn send_magic_code_with_ip(
        &self,
        request: SendMagicCodeRequest,
        _client_ip: Option<String>,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        self.send_magic_code(request)
    }

    fn verify_magic_code(
        &self,
        request: VerifyMagicCodeRequest,
    ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send;

    /// Supplies HTTP metadata separately from caller-controlled JSON fields.
    fn verify_magic_code_with_origin(
        &self,
        request: VerifyMagicCodeRequest,
        _origin: Option<String>,
    ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
        self.verify_magic_code(request)
    }

    /// Supplies the request origin and the resolved client address so the
    /// public verify route can apply a per-client-IP quota.
    fn verify_magic_code_with_context(
        &self,
        request: VerifyMagicCodeRequest,
        origin: Option<String>,
        _client_ip: Option<String>,
    ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
        self.verify_magic_code_with_origin(request, origin)
    }

    fn sign_in_guest_with_origin(
        &self,
        request: SignInGuestRequest,
        _origin: Option<String>,
    ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send {
        self.sign_in_guest(request)
    }

    fn verify_refresh_token(
        &self,
        request: VerifyRefreshTokenRequest,
    ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send;

    fn sign_in_guest(
        &self,
        request: SignInGuestRequest,
    ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send;

    fn sign_out(
        &self,
        request: SignOutRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

/// Builds the five non-OAuth runtime authentication routes.
pub fn runtime_auth_router<B>(backend: Arc<B>) -> Router
where
    B: RuntimeAuthBackend,
{
    Router::new()
        .route("/runtime/auth/send_magic_code", post(send_magic_code::<B>))
        .route(
            "/runtime/auth/verify_magic_code",
            post(verify_magic_code::<B>),
        )
        .route(
            "/runtime/auth/verify_refresh_token",
            post(verify_refresh_token::<B>),
        )
        .route("/runtime/auth/sign_in_guest", post(sign_in_guest::<B>))
        .route("/runtime/signout", post(sign_out::<B>))
        .with_state(backend)
}

async fn send_magic_code<B>(
    State(backend): State<Arc<B>>,
    ClientIp(client_ip): ClientIp,
    payload: Result<Json<SendMagicCodeRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: RuntimeAuthBackend,
{
    let mut request = json_payload(payload)?;
    request.email = coerce_email(&request.email)?;
    backend.send_magic_code_with_ip(request, client_ip).await?;
    Ok(Json(serde_json::json!({"sent": true})))
}

async fn verify_magic_code<B>(
    State(backend): State<Arc<B>>,
    ClientIp(client_ip): ClientIp,
    headers: HeaderMap,
    payload: Result<Json<VerifyMagicCodeRequest>, JsonRejection>,
) -> Result<Json<RuntimeAuthSession>, ApiError>
where
    B: RuntimeAuthBackend,
{
    let mut request = json_payload(payload)?;
    request.email = coerce_email(&request.email)?;
    request.code = request.code.trim().to_owned();
    backend
        .verify_magic_code_with_context(request, request_origin(&headers), client_ip)
        .await
        .map(Json)
}

async fn verify_refresh_token<B>(
    State(backend): State<Arc<B>>,
    payload: Result<Json<VerifyRefreshTokenRequest>, JsonRejection>,
) -> Result<Json<RuntimeAuthSession>, ApiError>
where
    B: RuntimeAuthBackend,
{
    let user = backend.verify_refresh_token(json_payload(payload)?).await?;
    Ok(Json(RuntimeAuthSession {
        user,
        created: None,
    }))
}

async fn sign_in_guest<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    payload: Result<Json<SignInGuestRequest>, JsonRejection>,
) -> Result<Json<RuntimeAuthSession>, ApiError>
where
    B: RuntimeAuthBackend,
{
    let user = backend
        .sign_in_guest_with_origin(json_payload(payload)?, request_origin(&headers))
        .await?;
    Ok(Json(RuntimeAuthSession {
        user,
        created: None,
    }))
}

async fn sign_out<B>(
    State(backend): State<Arc<B>>,
    payload: Result<Json<SignOutRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: RuntimeAuthBackend,
{
    backend.sign_out(json_payload(payload)?).await?;
    Ok(Json(serde_json::json!({})))
}

fn request_origin(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Client address for the runtime auth quota. Production inserts the address
/// resolved through trusted proxies; tests and embedders fall back to the
/// direct peer, and carry no address when neither is available.
struct ClientIp(Option<String>);

impl<S> FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let address = parts.extensions.get::<ResolvedClientIp>().map_or_else(
            || {
                parts
                    .extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|ConnectInfo(address)| address.ip())
            },
            |resolved| resolved.0,
        );
        Ok(Self(address.map(|address| address.to_string())))
    }
}

fn json_payload<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    payload.map(|Json(value)| value).map_err(|error| {
        ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body")
            .with_hint(serde_json::json!({"body": error.body_text()}))
    })
}

pub(crate) fn coerce_email(input: &str) -> Result<String, ApiError> {
    let normalized = input.trim().to_lowercase();
    if EMAIL_PATTERN.is_match(&normalized) {
        Ok(normalized)
    } else {
        Err(ApiError::new(
            ApiErrorType::ParamMalformed,
            "Malformed parameter: [body email]",
        )
        .with_hint(serde_json::json!({
            "in": ["body", "email"],
            "original-input": input
        })))
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;

    #[derive(Debug, Default)]
    struct FakeBackend {
        sent_from_ip: std::sync::Mutex<Option<Option<String>>>,
    }

    fn user(refresh_token: Uuid) -> RuntimeUser {
        RuntimeUser {
            id: Uuid::nil(),
            refresh_token,
            email: Some("test@example.com".to_owned()),
            image_url: None,
            user_type: Some("guest".to_owned()),
            is_guest: true,
            extra_fields: [("handle".to_owned(), serde_json::json!("tester"))].into(),
        }
    }

    impl RuntimeAuthBackend for FakeBackend {
        fn send_magic_code(
            &self,
            _request: SendMagicCodeRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(Ok(()))
        }

        fn send_magic_code_with_ip(
            &self,
            _request: SendMagicCodeRequest,
            client_ip: Option<String>,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            *self.sent_from_ip.lock().unwrap() = Some(client_ip);
            std::future::ready(Ok(()))
        }

        fn verify_magic_code(
            &self,
            request: VerifyMagicCodeRequest,
        ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
            std::future::ready(Ok(RuntimeAuthSession {
                user: user(request.refresh_token.unwrap_or_default()),
                created: Some(true),
            }))
        }

        async fn verify_magic_code_with_origin(
            &self,
            request: VerifyMagicCodeRequest,
            origin: Option<String>,
        ) -> Result<RuntimeAuthSession, ApiError> {
            let mut session = self.verify_magic_code(request).await?;
            session
                .user
                .extra_fields
                .insert("requestOrigin".into(), serde_json::json!(origin));
            Ok(session)
        }

        async fn sign_in_guest_with_origin(
            &self,
            request: SignInGuestRequest,
            origin: Option<String>,
        ) -> Result<RuntimeUser, ApiError> {
            let mut user = self.sign_in_guest(request).await?;
            user.extra_fields
                .insert("requestOrigin".into(), serde_json::json!(origin));
            Ok(user)
        }

        fn verify_refresh_token(
            &self,
            request: VerifyRefreshTokenRequest,
        ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send {
            std::future::ready(Ok(user(request.refresh_token)))
        }

        fn sign_in_guest(
            &self,
            _request: SignInGuestRequest,
        ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send {
            std::future::ready(Ok(user(Uuid::nil())))
        }

        fn sign_out(
            &self,
            _request: SignOutRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(Ok(()))
        }
    }

    async fn request(path: &str, body: &str) -> axum::response::Response {
        runtime_auth_router(Arc::new(FakeBackend::default()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn signup_origin_comes_from_http_headers_and_never_json() {
        for path in [
            "/runtime/auth/verify_magic_code",
            "/runtime/auth/sign_in_guest",
        ] {
            for origin in [Some("https://example.com"), None] {
                let mut request = Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json");
                if let Some(origin) = origin {
                    request = request.header(header::ORIGIN, origin);
                }
                let response = runtime_auth_router(Arc::new(FakeBackend::default())).oneshot(
                    request.body(Body::from(serde_json::json!({
                        "app-id":Uuid::new_v4(), "email":"person@example.com", "code":"123456",
                        "origin":"https://spoofed.example", "request":{"origin":"https://spoofed.example"},
                        "extra-fields":{"origin":"https://spoofed.example"}
                    }).to_string())).unwrap()
                ).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body: Value =
                    serde_json::from_slice(&to_bytes(response.into_body(), 16384).await.unwrap())
                        .unwrap();
                assert_eq!(body["user"]["requestOrigin"], serde_json::json!(origin));
            }
        }
    }

    fn send_magic_code_request() -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/runtime/auth/send_magic_code")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({
                    "app-id": Uuid::new_v4(),
                    "email": "person@example.com",
                })
                .to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn send_magic_code_prefers_resolved_client_ip_over_connect_info() {
        let backend = Arc::new(FakeBackend::default());
        let mut request = send_magic_code_request();
        request
            .extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("10.0.0.5:1234".parse().unwrap()));
        request
            .extensions_mut()
            .insert(ResolvedClientIp(Some("198.51.100.7".parse().unwrap())));

        let response = runtime_auth_router(Arc::clone(&backend))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            *backend.sent_from_ip.lock().unwrap(),
            Some(Some("198.51.100.7".to_owned()))
        );
    }

    #[tokio::test]
    async fn send_magic_code_falls_back_to_connect_info_without_a_resolved_ip() {
        let backend = Arc::new(FakeBackend::default());
        let mut request = send_magic_code_request();
        request
            .extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("10.0.0.5:1234".parse().unwrap()));

        let response = runtime_auth_router(Arc::clone(&backend))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            *backend.sent_from_ip.lock().unwrap(),
            Some(Some("10.0.0.5".to_owned()))
        );
    }

    #[tokio::test]
    async fn core_sdk_payload_keys_round_trip() {
        let token = Uuid::new_v4();
        let response = request(
            "/runtime/auth/verify_magic_code",
            &serde_json::json!({
                "app-id": Uuid::new_v4(),
                "email": "test@example.com",
                "code": "123456",
                "refresh-token": token,
                "extra-fields": {"handle": "tester"}
            })
            .to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(value["created"], true);
        assert_eq!(value["user"]["refresh_token"], token.to_string());
        assert_eq!(value["user"]["imageURL"], Value::Null);
        assert_eq!(value["user"]["isGuest"], true);
        assert_eq!(value["user"]["handle"], "tester");
    }

    #[tokio::test]
    async fn malformed_json_uses_instant_error_envelope_instead_of_axum_422() {
        let response = request("/runtime/auth/sign_in_guest", r#"{"app-id":"nope"}"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(value["type"], "param-malformed");
        assert_eq!(value["message"], "Malformed JSON body");
    }

    #[test]
    fn signout_retains_underscore_request_contract() {
        let request: SignOutRequest = serde_json::from_value(serde_json::json!({
            "app_id": Uuid::nil(),
            "refresh_token": Uuid::nil()
        }))
        .unwrap();
        assert_eq!(request.app_id, Uuid::nil());
    }

    #[test]
    fn email_coercion_matches_the_jvm_expression_and_normalization() {
        assert_eq!(
            coerce_email("  First.Last+tag@Example.COM ").unwrap(),
            "first.last+tag@example.com"
        );
        let error = coerce_email("not-an-email").unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
        assert_eq!(
            error.body().hint.as_ref().unwrap()["original-input"],
            "not-an-email"
        );
    }
}
