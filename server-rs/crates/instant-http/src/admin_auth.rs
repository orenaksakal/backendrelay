use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::admin::{json_object, query_params, request_context};
use crate::runtime_auth::coerce_email;
use crate::{AdminRequestContext, ApiError, ApiErrorType, RuntimeAuthSession, RuntimeUser};

/// User selector accepted by admin token creation and sign-out operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdminUserSelector {
    Email(String),
    Id(Uuid),
    RefreshToken(Uuid),
}

/// Request to create a user if necessary and issue a refresh token.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminCreateTokenRequest {
    pub selector: AdminUserSelector,
    pub extra_fields: BTreeMap<String, Value>,
}

/// Request to revoke one token or every token belonging to a selected user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminSignOutRequest {
    pub selector: AdminUserSelector,
}

/// Admin request for generating or delivering a magic code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdminMagicCodeRequest {
    pub email: String,
}

/// Admin request for consuming a magic code.
#[derive(Clone, Debug, PartialEq)]
pub struct AdminVerifyMagicCodeRequest {
    pub email: String,
    pub code: String,
    pub refresh_token: Option<Uuid>,
    pub extra_fields: BTreeMap<String, Value>,
}

/// Admin request for creating a guest user.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdminSignInGuestRequest {
    pub extra_fields: BTreeMap<String, Value>,
}

/// `$users` projection returned by admin lookup and deletion routes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AdminUser {
    pub id: Uuid,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct MagicCodeResponse {
    code: String,
}

/// Auth operations exposed by the official admin SDK.
pub trait AdminAuthBackend: Send + Sync + 'static {
    fn get_user(
        &self,
        context: AdminRequestContext,
        selector: AdminUserSelector,
    ) -> impl Future<Output = Result<Option<AdminUser>, ApiError>> + Send;

    fn delete_user(
        &self,
        context: AdminRequestContext,
        selector: AdminUserSelector,
    ) -> impl Future<Output = Result<Option<AdminUser>, ApiError>> + Send;

    fn create_token(
        &self,
        context: AdminRequestContext,
        request: AdminCreateTokenRequest,
    ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send;

    fn sign_out(
        &self,
        context: AdminRequestContext,
        request: AdminSignOutRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn generate_magic_code(
        &self,
        context: AdminRequestContext,
        request: AdminMagicCodeRequest,
    ) -> impl Future<Output = Result<String, ApiError>> + Send;

    fn send_magic_code(
        &self,
        context: AdminRequestContext,
        request: AdminMagicCodeRequest,
    ) -> impl Future<Output = Result<String, ApiError>> + Send;

    fn verify_magic_code(
        &self,
        context: AdminRequestContext,
        request: AdminVerifyMagicCodeRequest,
    ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send;

    fn sign_in_guest(
        &self,
        context: AdminRequestContext,
        request: AdminSignInGuestRequest,
    ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send;
}

/// Builds the six admin authentication routes used by `@instantdb/admin`.
pub fn admin_auth_router<B>(backend: Arc<B>) -> Router
where
    B: AdminAuthBackend,
{
    crate::body_limit::limit_body(
        Router::new()
            .route("/admin/refresh_tokens", post(create_token::<B>))
            .route("/admin/sign_out", post(sign_out::<B>))
            .route("/admin/magic_code", post(generate_magic_code::<B>))
            .route("/admin/send_magic_code", post(send_magic_code::<B>))
            .route("/admin/verify_magic_code", post(verify_magic_code::<B>))
            .route("/admin/sign_in_guest", post(sign_in_guest::<B>))
            .route("/admin/users", get(get_user::<B>).delete(delete_user::<B>)),
        crate::body_limit::MANAGEMENT_BODY_LIMIT,
        "Admin request body exceeds the 1 MiB limit",
    )
    .with_state(backend)
}

type Params = Result<Query<HashMap<String, String>>, QueryRejection>;
type Payload = Result<Json<Value>, JsonRejection>;

async fn get_user<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
) -> Result<Json<Value>, ApiError>
where
    B: AdminAuthBackend,
{
    let (context, selector) = user_context(&headers, params)?;
    let user = backend.get_user(context, selector).await?;
    Ok(Json(serde_json::json!({"user": user})))
}

async fn delete_user<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
) -> Result<Json<Value>, ApiError>
where
    B: AdminAuthBackend,
{
    let (context, selector) = user_context(&headers, params)?;
    let user = backend.delete_user(context, selector).await?;
    Ok(Json(serde_json::json!({"deleted": user})))
}

async fn create_token<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
    payload: Payload,
) -> Result<Json<RuntimeAuthSession>, ApiError>
where
    B: AdminAuthBackend,
{
    let context = context(&headers, params)?;
    let body = json_object(payload)?;
    let selector = if let Some(email) = optional_string(&body, "email")? {
        AdminUserSelector::Email(coerce_email(email)?)
    } else if let Some(id) = optional_uuid(&body, "id")? {
        AdminUserSelector::Id(id)
    } else {
        return Err(validation(
            "Please provide an `email` or `id`",
            &Value::Object(body),
        ));
    };
    let request = AdminCreateTokenRequest {
        selector,
        extra_fields: optional_object(&body, "extra-fields")?,
    };
    backend.create_token(context, request).await.map(Json)
}

async fn sign_out<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
    payload: Payload,
) -> Result<Json<Value>, ApiError>
where
    B: AdminAuthBackend,
{
    let context = context(&headers, params)?;
    let body = json_object(payload)?;
    let selector = if let Some(id) = optional_uuid(&body, "id")? {
        AdminUserSelector::Id(id)
    } else if let Some(email) = optional_string(&body, "email")? {
        AdminUserSelector::Email(coerce_email(email)?)
    } else if let Some(token) = optional_uuid(&body, "refresh_token")? {
        AdminUserSelector::RefreshToken(token)
    } else {
        return Err(validation(
            "Please provide an `id`, `email`, or `refresh_token`",
            &Value::Object(body),
        ));
    };
    backend
        .sign_out(context, AdminSignOutRequest { selector })
        .await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn generate_magic_code<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
    payload: Payload,
) -> Result<Json<MagicCodeResponse>, ApiError>
where
    B: AdminAuthBackend,
{
    let (context, request) = magic_request(&headers, params, payload)?;
    let code = backend.generate_magic_code(context, request).await?;
    Ok(Json(MagicCodeResponse { code }))
}

async fn send_magic_code<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
    payload: Payload,
) -> Result<Json<MagicCodeResponse>, ApiError>
where
    B: AdminAuthBackend,
{
    let (context, request) = magic_request(&headers, params, payload)?;
    let code = backend.send_magic_code(context, request).await?;
    Ok(Json(MagicCodeResponse { code }))
}

async fn verify_magic_code<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
    payload: Payload,
) -> Result<Json<RuntimeAuthSession>, ApiError>
where
    B: AdminAuthBackend,
{
    let context = context(&headers, params)?;
    let body = json_object(payload)?;
    let email = coerce_email(required_string(&body, "email")?)?;
    let code = required_string(&body, "code")?.trim().to_owned();
    let request = AdminVerifyMagicCodeRequest {
        email,
        code,
        refresh_token: optional_uuid(&body, "refresh-token")?,
        extra_fields: optional_object(&body, "extra-fields")?,
    };
    backend.verify_magic_code(context, request).await.map(Json)
}

async fn sign_in_guest<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Params,
    payload: Payload,
) -> Result<Json<RuntimeAuthSession>, ApiError>
where
    B: AdminAuthBackend,
{
    let context = context(&headers, params)?;
    let body = json_object(payload)?;
    let user = backend
        .sign_in_guest(
            context,
            AdminSignInGuestRequest {
                extra_fields: optional_object(&body, "extra-fields")?,
            },
        )
        .await?;
    Ok(Json(RuntimeAuthSession {
        user,
        created: None,
    }))
}

fn magic_request(
    headers: &HeaderMap,
    params: Params,
    payload: Payload,
) -> Result<(AdminRequestContext, AdminMagicCodeRequest), ApiError> {
    let context = context(headers, params)?;
    let body = json_object(payload)?;
    let email = coerce_email(required_string(&body, "email")?)?;
    Ok((context, AdminMagicCodeRequest { email }))
}

fn context(headers: &HeaderMap, params: Params) -> Result<AdminRequestContext, ApiError> {
    let params = query_params(params)?;
    request_context(headers, &params)
}

fn user_context(
    headers: &HeaderMap,
    params: Params,
) -> Result<(AdminRequestContext, AdminUserSelector), ApiError> {
    let params = query_params(params)?;
    let context = request_context(headers, &params)?;
    let selector =
        if let Some(email) = params.get("email") {
            AdminUserSelector::Email(coerce_email(email)?)
        } else if let Some(token) = params.get("refresh_token") {
            AdminUserSelector::RefreshToken(token.parse().map_err(|_| {
                malformed("[params refresh_token]", &Value::String(token.to_owned()))
            })?)
        } else if let Some(id) = params.get("id") {
            AdminUserSelector::Id(
                id.parse()
                    .map_err(|_| malformed("[params id]", &Value::String(id.to_owned())))?,
            )
        } else {
            return Err(validation(
                "Please provide a user id, email, or refresh_token",
                &serde_json::to_value(params).unwrap_or(Value::Null),
            ));
        };
    Ok((context, selector))
}

fn required_string<'a>(body: &'a Map<String, Value>, key: &str) -> Result<&'a str, ApiError> {
    let value = body
        .get(key)
        .ok_or_else(|| missing(&format!("[body {key}]")))?;
    value
        .as_str()
        .ok_or_else(|| malformed(&format!("[body {key}]"), value))
}

fn optional_string<'a>(
    body: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, ApiError> {
    body.get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| malformed(&format!("[body {key}]"), value))
        })
        .transpose()
}

fn optional_uuid(body: &Map<String, Value>, key: &str) -> Result<Option<Uuid>, ApiError> {
    optional_string(body, key)?
        .map(|value| {
            value
                .parse()
                .map_err(|_| malformed(&format!("[body {key}]"), &Value::String(value.to_owned())))
        })
        .transpose()
}

fn optional_object(
    body: &Map<String, Value>,
    key: &str,
) -> Result<BTreeMap<String, Value>, ApiError> {
    body.get(key)
        .map(|value| {
            value
                .as_object()
                .cloned()
                .map(|object| object.into_iter().collect())
                .ok_or_else(|| malformed(&format!("[body {key}]"), value))
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn missing(path: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {path}"),
    )
}

fn malformed(path: &str, input: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: {path}"),
    )
    .with_hint(serde_json::json!({"in": path, "original-input": input}))
}

fn validation(message: &str, input: &Value) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
        .with_hint(serde_json::json!({"in": "body", "input": input}))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        calls: Mutex<Vec<&'static str>>,
    }

    fn user() -> RuntimeUser {
        RuntimeUser {
            id: Uuid::nil(),
            refresh_token: Uuid::nil(),
            email: Some("a@example.com".to_owned()),
            image_url: None,
            user_type: None,
            is_guest: false,
            extra_fields: BTreeMap::new(),
        }
    }

    impl AdminAuthBackend for FakeBackend {
        fn get_user(
            &self,
            _context: AdminRequestContext,
            _selector: AdminUserSelector,
        ) -> impl Future<Output = Result<Option<AdminUser>, ApiError>> + Send {
            self.calls.lock().unwrap().push("get-user");
            std::future::ready(Ok(Some(AdminUser {
                id: Uuid::nil(),
                email: Some("a@example.com".to_owned()),
                image_url: None,
                user_type: None,
                is_guest: false,
                extra_fields: BTreeMap::new(),
            })))
        }

        fn delete_user(
            &self,
            _context: AdminRequestContext,
            _selector: AdminUserSelector,
        ) -> impl Future<Output = Result<Option<AdminUser>, ApiError>> + Send {
            self.calls.lock().unwrap().push("delete-user");
            std::future::ready(Ok(None))
        }

        fn create_token(
            &self,
            _context: AdminRequestContext,
            _request: AdminCreateTokenRequest,
        ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
            self.calls.lock().unwrap().push("create-token");
            std::future::ready(Ok(RuntimeAuthSession {
                user: user(),
                created: Some(true),
            }))
        }

        fn sign_out(
            &self,
            _context: AdminRequestContext,
            _request: AdminSignOutRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.calls.lock().unwrap().push("sign-out");
            std::future::ready(Ok(()))
        }

        fn generate_magic_code(
            &self,
            _context: AdminRequestContext,
            _request: AdminMagicCodeRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.calls.lock().unwrap().push("generate");
            std::future::ready(Ok("123456".to_owned()))
        }

        fn send_magic_code(
            &self,
            _context: AdminRequestContext,
            _request: AdminMagicCodeRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.calls.lock().unwrap().push("send");
            std::future::ready(Ok("123456".to_owned()))
        }

        fn verify_magic_code(
            &self,
            _context: AdminRequestContext,
            _request: AdminVerifyMagicCodeRequest,
        ) -> impl Future<Output = Result<RuntimeAuthSession, ApiError>> + Send {
            self.calls.lock().unwrap().push("verify");
            std::future::ready(Ok(RuntimeAuthSession {
                user: user(),
                created: Some(false),
            }))
        }

        fn sign_in_guest(
            &self,
            _context: AdminRequestContext,
            _request: AdminSignInGuestRequest,
        ) -> impl Future<Output = Result<RuntimeUser, ApiError>> + Send {
            self.calls.lock().unwrap().push("guest");
            std::future::ready(Ok(user()))
        }
    }

    async fn post(router: Router, path: &str, body: &str) -> axum::response::Response {
        router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer admin-secret")
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn all_six_official_admin_auth_routes_preserve_envelopes() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let cases = [
            ("/admin/refresh_tokens", r#"{"email":"A@EXAMPLE.COM"}"#),
            (
                "/admin/sign_out",
                r#"{"refresh_token":"00000000-0000-0000-0000-000000000000"}"#,
            ),
            ("/admin/magic_code", r#"{"email":"a@example.com"}"#),
            ("/admin/send_magic_code", r#"{"email":"a@example.com"}"#),
            (
                "/admin/verify_magic_code",
                r#"{"email":"a@example.com","code":" 123456 "}"#,
            ),
            (
                "/admin/sign_in_guest",
                r#"{"extra-fields":{"nickname":"A"}}"#,
            ),
        ];
        for (path, body) in cases {
            let response = post(
                admin_auth_router(backend.clone()),
                &format!("{path}?app_id={app_id}"),
                body,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }
        assert_eq!(
            *backend.calls.lock().unwrap(),
            [
                "create-token",
                "sign-out",
                "generate",
                "send",
                "verify",
                "guest"
            ]
        );
    }

    #[tokio::test]
    async fn magic_and_signout_response_shapes_match_the_jvm() {
        let app_id = Uuid::new_v4();
        let backend = Arc::new(FakeBackend::default());
        let response = post(
            admin_auth_router(backend.clone()),
            &format!("/admin/magic_code?app_id={app_id}"),
            r#"{"email":"a@example.com"}"#,
        )
        .await;
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"code":"123456"}"#
        );
        let response = post(
            admin_auth_router(backend),
            &format!("/admin/sign_out?app_id={app_id}"),
            r#"{"id":"00000000-0000-0000-0000-000000000000"}"#,
        )
        .await;
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"ok":true}"#
        );
    }

    #[tokio::test]
    async fn selector_validation_uses_instant_error_envelopes() {
        let app_id = Uuid::new_v4();
        let response = post(
            admin_auth_router(Arc::new(FakeBackend::default())),
            &format!("/admin/refresh_tokens?app_id={app_id}"),
            "{}",
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains(r#""type":"validation-failed""#)
        );
    }

    #[tokio::test]
    async fn user_lookup_and_delete_preserve_nullable_envelopes() {
        let app_id = Uuid::new_v4();
        let backend = Arc::new(FakeBackend::default());
        let get_response = admin_auth_router(backend.clone())
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/users?app_id={app_id}&email=A%40EXAMPLE.COM"
                    ))
                    .header(header::AUTHORIZATION, "Bearer admin-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_response.status(), StatusCode::OK);
        let get_body = to_bytes(get_response.into_body(), usize::MAX)
            .await
            .unwrap();
        let get_body: Value = serde_json::from_slice(&get_body).unwrap();
        assert_eq!(get_body["user"]["id"], Uuid::nil().to_string());
        assert_eq!(get_body["user"]["email"], "a@example.com");
        assert_eq!(get_body["user"]["isGuest"], false);

        let delete_response = admin_auth_router(backend)
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!(
                        "/admin/users?app_id={app_id}&id=00000000-0000-0000-0000-000000000000"
                    ))
                    .header(header::AUTHORIZATION, "Bearer admin-secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete_response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(delete_response.into_body(), usize::MAX)
                .await
                .unwrap(),
            r#"{"deleted":null}"#
        );
    }
}
