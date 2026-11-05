use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{ApiError, ApiErrorType, RuntimeUser};

const OAUTH_COOKIE_NAME: &str = "__session";
const OAUTH_COOKIE_PREFIX: &str = "instantdb_";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthStartRequest {
    pub app_id: Uuid,
    pub client_name: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub hosted_domain: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthStartResponse {
    pub authorization_url: String,
    pub cookie: Uuid,
    pub secure_cookie: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OAuthCallbackRequest {
    pub params: BTreeMap<String, String>,
    pub cookie: Option<Uuid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OAuthCallbackResponse {
    Redirect(String),
    ExternalApp { email: String, redirect_url: String },
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct OAuthTokenRequest {
    pub app_id: Uuid,
    pub code: Uuid,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<Uuid>,
    #[serde(default)]
    pub extra_fields: BTreeMap<String, Value>,
    #[serde(skip)]
    pub origin: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct OAuthIdTokenRequest {
    pub id_token: String,
    pub app_id: Uuid,
    pub refresh_token: Option<Uuid>,
    pub client_name: String,
    pub nonce: Option<String>,
    #[serde(default)]
    pub extra_fields: BTreeMap<String, Value>,
    #[serde(skip)]
    pub origin: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OAuthSessionResponse {
    pub user: RuntimeUser,
    pub created: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<Uuid>,
}

pub trait RuntimeOAuthBackend: Send + Sync + 'static {
    fn start(
        &self,
        request: OAuthStartRequest,
    ) -> impl Future<Output = Result<OAuthStartResponse, ApiError>> + Send;

    fn callback(
        &self,
        request: OAuthCallbackRequest,
    ) -> impl Future<Output = Result<OAuthCallbackResponse, ApiError>> + Send;

    fn exchange_code(
        &self,
        request: OAuthTokenRequest,
    ) -> impl Future<Output = Result<OAuthSessionResponse, ApiError>> + Send;

    fn exchange_id_token(
        &self,
        request: OAuthIdTokenRequest,
    ) -> impl Future<Output = Result<OAuthSessionResponse, ApiError>> + Send;
}

pub fn runtime_oauth_router<B>(backend: Arc<B>) -> Router
where
    B: RuntimeOAuthBackend,
{
    Router::new()
        .route("/runtime/oauth/start", get(oauth_start::<B>))
        .route("/runtime/{app_id}/oauth/start", get(oauth_start_path::<B>))
        .route(
            "/runtime/oauth/callback",
            get(oauth_callback_get::<B>).post(oauth_callback_post::<B>),
        )
        .route("/runtime/oauth/token", post(oauth_token::<B>))
        .route("/runtime/{app_id}/oauth/token", post(oauth_token_path::<B>))
        .route("/runtime/oauth/id_token", post(oauth_id_token::<B>))
        .with_state(backend)
}

#[derive(Debug, Default, Deserialize)]
struct StartParams {
    app_id: Option<Uuid>,
    client_name: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    hd: Option<String>,
}

async fn oauth_start<B>(
    State(backend): State<Arc<B>>,
    Query(params): Query<StartParams>,
) -> Result<Response, ApiError>
where
    B: RuntimeOAuthBackend,
{
    let app_id = params.app_id.ok_or_else(|| missing("[params app_id]"))?;
    start_response(&*backend, app_id, params).await
}

async fn oauth_start_path<B>(
    State(backend): State<Arc<B>>,
    Path(app_id): Path<Uuid>,
    Query(params): Query<StartParams>,
) -> Result<Response, ApiError>
where
    B: RuntimeOAuthBackend,
{
    start_response(&*backend, app_id, params).await
}

async fn start_response<B>(
    backend: &B,
    app_id: Uuid,
    params: StartParams,
) -> Result<Response, ApiError>
where
    B: RuntimeOAuthBackend,
{
    let client_name = params
        .client_name
        .or(params.client_id)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| missing("[params client_name]"))?;
    let redirect_uri = params
        .redirect_uri
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| missing("[params redirect_uri]"))?;
    let response = backend
        .start(OAuthStartRequest {
            app_id,
            client_name,
            redirect_uri,
            state: params.state,
            code_challenge: params.code_challenge,
            code_challenge_method: params.code_challenge_method,
            hosted_domain: params.hd,
        })
        .await?;
    redirect_with_cookie(&response)
}

async fn oauth_callback_get<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Response, ApiError>
where
    B: RuntimeOAuthBackend,
{
    callback_response(&*backend, headers, params).await
}

async fn oauth_callback_post<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    Form(params): Form<BTreeMap<String, String>>,
) -> Result<Response, ApiError>
where
    B: RuntimeOAuthBackend,
{
    callback_response(&*backend, headers, params).await
}

async fn callback_response<B>(
    backend: &B,
    headers: HeaderMap,
    params: BTreeMap<String, String>,
) -> Result<Response, ApiError>
where
    B: RuntimeOAuthBackend,
{
    if params.contains_key("test-redirect") {
        return Ok(Html(test_redirect_html()).into_response());
    }
    let cookie = oauth_cookie(&headers)?;
    match backend
        .callback(OAuthCallbackRequest { params, cookie })
        .await?
    {
        OAuthCallbackResponse::Redirect(url) => redirect(&url),
        OAuthCallbackResponse::ExternalApp {
            email,
            redirect_url,
        } => Ok(Html(external_app_html(&email, &redirect_url)).into_response()),
    }
}

#[derive(Clone, Copy, Debug)]
enum TokenParamSource {
    Params,
    Body,
    FormParams,
}

impl TokenParamSource {
    const fn name(self) -> &'static str {
        match self {
            Self::Params => "params",
            Self::Body => "body",
            Self::FormParams => "form-params",
        }
    }
}

#[derive(Debug)]
struct TokenPayload {
    headers: HeaderMap,
    params: BTreeMap<String, String>,
    body: Option<serde_json::Map<String, Value>>,
    form_params: BTreeMap<String, String>,
}

impl TokenPayload {
    fn selected_param(&self, key: &str) -> Option<(TokenParamSource, Value)> {
        if let Some(value) = self.params.get(key) {
            return Some((TokenParamSource::Params, Value::String(value.clone())));
        }
        if let Some(value) = self
            .body
            .as_ref()
            .and_then(|body| body.get(key))
            .filter(|value| clojure_truthy(value))
        {
            return Some((TokenParamSource::Body, value.clone()));
        }
        self.form_params
            .get(key)
            .map(|value| (TokenParamSource::FormParams, Value::String(value.clone())))
    }

    fn required_uuid(&self, key: &str) -> Result<Uuid, ApiError> {
        let Some((source, value)) = self.selected_param(key) else {
            return Err(missing_some_parameter(key));
        };
        coerce_uuid(&value).ok_or_else(|| malformed_some_parameter(source, key, &value))
    }

    fn code_verifier(&self) -> Result<Option<String>, ApiError> {
        self.selected_param("code_verifier")
            .map(|(source, value)| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| malformed_some_parameter(source, "code_verifier", &value))
            })
            .transpose()
    }

    fn refresh_token(&self) -> Result<Option<Uuid>, ApiError> {
        let Some(value) = self
            .body
            .as_ref()
            .and_then(|body| body.get("refresh_token"))
            .filter(|value| clojure_truthy(value))
        else {
            return Ok(None);
        };
        coerce_uuid(value)
            .map(Some)
            .ok_or_else(|| malformed_body_parameter("refresh_token", value))
    }

    fn extra_fields(&self) -> Result<BTreeMap<String, Value>, ApiError> {
        let Some(value) = self.body.as_ref().and_then(|body| body.get("extra_fields")) else {
            return Ok(BTreeMap::new());
        };
        serde_json::from_value(value.clone())
            .map_err(|_| malformed_body_parameter("extra_fields", value))
    }
}

async fn oauth_token<B>(
    State(backend): State<Arc<B>>,
    request: Request,
) -> Result<Json<OAuthSessionResponse>, ApiError>
where
    B: RuntimeOAuthBackend,
{
    let payload = token_payload(request).await?;
    let app_id = payload.required_uuid("app_id")?;
    exchange_token(&*backend, app_id, payload).await
}

async fn oauth_token_path<B>(
    State(backend): State<Arc<B>>,
    Path(app_id): Path<String>,
    request: Request,
) -> Result<Json<OAuthSessionResponse>, ApiError>
where
    B: RuntimeOAuthBackend,
{
    let mut payload = token_payload(request).await?;
    payload.params.insert("app_id".to_owned(), app_id);
    let app_id = payload.required_uuid("app_id")?;
    exchange_token(&*backend, app_id, payload).await
}

async fn token_payload(request: Request) -> Result<TokenPayload, ApiError> {
    let (parts, body) = request.into_parts();
    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    let body = to_bytes(body, 1_048_576)
        .await
        .map_err(|_| ApiError::new(ApiErrorType::ParamMalformed, "Malformed OAuth token body"))?;
    let query_params: BTreeMap<String, String> =
        serde_urlencoded::from_str(parts.uri.query().unwrap_or_default()).map_err(|_| {
            ApiError::new(ApiErrorType::ParamMalformed, "Malformed query parameters")
        })?;
    let (json_body, form_params) = if is_json {
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body"))?;
        let body = value
            .as_object()
            .cloned()
            .ok_or_else(|| ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body"))?;
        (Some(body), BTreeMap::new())
    } else {
        let params = serde_urlencoded::from_bytes(&body)
            .map_err(|_| ApiError::new(ApiErrorType::ParamMalformed, "Malformed form body"))?;
        (None, params)
    };
    // Ring 1.13 `params-request` merges form params first and query params second, so query
    // values win. `wrap-keyword-params` then exposes that merged map at `:params` while retaining
    // `:form-params`; Compojure overlays route params after both middleware layers.
    let mut params = if is_json {
        BTreeMap::new()
    } else {
        form_params.clone()
    };
    params.extend(query_params);
    Ok(TokenPayload {
        headers: parts.headers,
        params,
        body: json_body,
        form_params,
    })
}

async fn exchange_token<B>(
    backend: &B,
    app_id: Uuid,
    payload: TokenPayload,
) -> Result<Json<OAuthSessionResponse>, ApiError>
where
    B: RuntimeOAuthBackend,
{
    let code = payload.required_uuid("code")?;
    backend
        .exchange_code(OAuthTokenRequest {
            app_id,
            code,
            code_verifier: payload.code_verifier()?,
            refresh_token: payload.refresh_token()?,
            extra_fields: payload.extra_fields()?,
            origin: optional_header(&payload.headers, header::ORIGIN)?,
        })
        .await
        .map(Json)
}

async fn oauth_id_token<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    payload: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<OAuthSessionResponse>, ApiError>
where
    B: RuntimeOAuthBackend,
{
    let Json(body) = payload.map_err(|error| {
        ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body")
            .with_hint(serde_json::json!({"body": error.body_text()}))
    })?;
    let request = OAuthIdTokenRequest {
        id_token: required_body_non_blank_string(&body, "id_token")?,
        app_id: required_body_uuid(&body, "app_id")?,
        refresh_token: optional_body_uuid(&body, "refresh_token")?,
        client_name: required_body_non_blank_string(&body, "client_name")?,
        nonce: body_parameter(&body, "nonce")
            .and_then(Value::as_str)
            .map(str::to_owned),
        extra_fields: optional_body_map(&body, "extra_fields")?,
        origin: optional_header(&headers, header::ORIGIN)?,
    };
    backend.exchange_id_token(request).await.map(Json)
}

fn redirect_with_cookie(response: &OAuthStartResponse) -> Result<Response, ApiError> {
    redirect_with_cookie_at(response, Utc::now())
}

fn redirect_with_cookie_at(
    response: &OAuthStartResponse,
    now: DateTime<Utc>,
) -> Result<Response, ApiError> {
    let mut result = redirect(&response.authorization_url)?;
    let secure = if response.secure_cookie {
        // Cross-site form_post callbacks must also work after Chrome's short
        // Lax+POST grace period (upstream issue #2892).
        "; Secure; SameSite=None"
    } else {
        ""
    };
    let expires = (now + Duration::hours(1)).format("%a, %d %b %Y %H:%M:%S GMT");
    let cookie = format!(
        "{OAUTH_COOKIE_NAME}={OAUTH_COOKIE_PREFIX}{}; Path=/runtime/oauth; Expires={expires}; HttpOnly{secure}",
        response.cookie,
    );
    result.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|_| ApiError::new(ApiErrorType::Unknown, "Invalid OAuth cookie"))?,
    );
    Ok(result)
}

fn redirect(url: &str) -> Result<Response, ApiError> {
    let location = HeaderValue::from_str(url)
        .map_err(|_| ApiError::new(ApiErrorType::OauthError, "Invalid OAuth redirect URL"))?;
    Ok((StatusCode::FOUND, [(header::LOCATION, location)]).into_response())
}

fn oauth_cookie(headers: &HeaderMap) -> Result<Option<Uuid>, ApiError> {
    let Some(value) = optional_header(headers, header::COOKIE)? else {
        return Ok(None);
    };
    Ok(value.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == OAUTH_COOKIE_NAME)
            .then(|| value.strip_prefix(OAUTH_COOKIE_PREFIX))
            .flatten()
            .and_then(|value| Uuid::parse_str(value).ok())
    }))
}

fn optional_header(
    headers: &HeaderMap,
    name: header::HeaderName,
) -> Result<Option<String>, ApiError> {
    headers
        .get(name)
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                ApiError::new(ApiErrorType::ParamMalformed, "Malformed request header")
            })
        })
        .transpose()
}

fn missing(location: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {location}"),
    )
}

fn clojure_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn coerce_uuid(value: &Value) -> Option<Uuid> {
    let value = value.as_str()?.trim();
    if value.len() != 36 {
        return None;
    }
    Uuid::parse_str(value).ok()
}

fn parameter_path(source: TokenParamSource, key: &str) -> Value {
    serde_json::json!([source.name(), key])
}

fn parameter_paths(key: &str) -> Vec<Value> {
    [
        TokenParamSource::Params,
        TokenParamSource::Body,
        TokenParamSource::FormParams,
    ]
    .into_iter()
    .flat_map(|source| [parameter_path(source, key), parameter_path(source, key)])
    .collect()
}

fn missing_some_parameter(key: &str) -> ApiError {
    let mut paths = parameter_paths(key);
    let first = paths.remove(0);
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: [params {key}]"),
    )
    .with_hint(serde_json::json!({
        "in": first,
        "possible-ins": paths,
    }))
}

fn malformed_some_parameter(source: TokenParamSource, key: &str, value: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: [{} {key}]", source.name()),
    )
    .with_hint(serde_json::json!({
        "in": parameter_path(source, key),
        "possible-ins": parameter_paths(key),
        "original-input": value,
    }))
}

fn malformed_body_parameter(key: &str, value: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: [body {key}]"),
    )
    .with_hint(serde_json::json!({
        "in": ["body", key],
        "original-input": value,
    }))
}

fn body_parameter<'a>(body: &'a Value, key: &str) -> Option<&'a Value> {
    body.as_object().and_then(|body| body.get(key))
}

fn missing_body_parameter(key: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: [body {key}]"),
    )
    .with_hint(serde_json::json!({"in": ["body", key]}))
}

fn required_body_non_blank_string(body: &Value, key: &str) -> Result<String, ApiError> {
    let Some(value) = body_parameter(body, key).filter(|value| clojure_truthy(value)) else {
        return Err(missing_body_parameter(key));
    };
    let coerced = match value {
        Value::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        }
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    };
    coerced.ok_or_else(|| malformed_body_parameter(key, value))
}

fn required_body_uuid(body: &Value, key: &str) -> Result<Uuid, ApiError> {
    let Some(value) = body_parameter(body, key).filter(|value| clojure_truthy(value)) else {
        return Err(missing_body_parameter(key));
    };
    coerce_uuid(value).ok_or_else(|| malformed_body_parameter(key, value))
}

fn optional_body_uuid(body: &Value, key: &str) -> Result<Option<Uuid>, ApiError> {
    let Some(value) = body_parameter(body, key).filter(|value| clojure_truthy(value)) else {
        return Ok(None);
    };
    coerce_uuid(value)
        .map(Some)
        .ok_or_else(|| malformed_body_parameter(key, value))
}

fn optional_body_map(body: &Value, key: &str) -> Result<BTreeMap<String, Value>, ApiError> {
    let Some(value) = body_parameter(body, key).filter(|value| !value.is_null()) else {
        return Ok(BTreeMap::new());
    };
    serde_json::from_value(value.clone()).map_err(|_| malformed_body_parameter(key, value))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn test_redirect_html() -> String {
    "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\"><title>Test redirect</title></head><body><p>Your OAuth redirect looks good!</p></body></html>".to_owned()
}

fn external_app_html(email: &str, redirect_url: &str) -> String {
    let email = escape_html(email);
    let redirect_url = escape_html(redirect_url);
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\"><meta http-equiv=\"refresh\" content=\"0; url={redirect_url}\"><title>Finish Sign In</title></head><body><p>Logged in as {email}</p><p><a href=\"{redirect_url}\">Open app</a></p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use chrono::TimeZone;
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        starts: Mutex<Vec<OAuthStartRequest>>,
        callbacks: Mutex<Vec<OAuthCallbackRequest>>,
        exchanges: Mutex<Vec<OAuthTokenRequest>>,
        id_token_exchanges: Mutex<Vec<OAuthIdTokenRequest>>,
    }

    impl RuntimeOAuthBackend for FakeBackend {
        fn start(
            &self,
            request: OAuthStartRequest,
        ) -> impl Future<Output = Result<OAuthStartResponse, ApiError>> + Send {
            self.starts.lock().unwrap().push(request);
            std::future::ready(Ok(OAuthStartResponse {
                authorization_url: "https://provider.test/auth?state=opaque".to_owned(),
                cookie: Uuid::nil(),
                secure_cookie: true,
            }))
        }

        fn callback(
            &self,
            request: OAuthCallbackRequest,
        ) -> impl Future<Output = Result<OAuthCallbackResponse, ApiError>> + Send {
            self.callbacks.lock().unwrap().push(request);
            std::future::ready(Ok(OAuthCallbackResponse::Redirect(
                "https://app.test/done".to_owned(),
            )))
        }

        fn exchange_code(
            &self,
            request: OAuthTokenRequest,
        ) -> impl Future<Output = Result<OAuthSessionResponse, ApiError>> + Send {
            let code = request.code;
            self.exchanges.lock().unwrap().push(request);
            std::future::ready(Ok(session(code)))
        }

        fn exchange_id_token(
            &self,
            request: OAuthIdTokenRequest,
        ) -> impl Future<Output = Result<OAuthSessionResponse, ApiError>> + Send {
            let app_id = request.app_id;
            self.id_token_exchanges.lock().unwrap().push(request);
            let mut response = session(app_id);
            response.refresh_token = None;
            std::future::ready(Ok(response))
        }
    }

    fn session(value: Uuid) -> OAuthSessionResponse {
        OAuthSessionResponse {
            user: RuntimeUser {
                id: value,
                refresh_token: value,
                email: Some("person@example.com".to_owned()),
                image_url: None,
                user_type: Some("user".to_owned()),
                is_guest: false,
                extra_fields: BTreeMap::new(),
            },
            created: true,
            refresh_token: Some(value),
        }
    }

    #[tokio::test]
    async fn start_aliases_client_id_and_sets_scoped_cookie() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/runtime/{app_id}/oauth/start?client_id=web&redirect_uri=https%3A%2F%2Fapp.test%2Fdone&state=caller"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://provider.test/auth?state=opaque"
        );
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("__session=instantdb_00000000-0000-0000-0000-000000000000")
        );
        let starts = backend.starts.lock().unwrap();
        assert_eq!(starts[0].client_name, "web");
        assert_eq!(starts[0].state.as_deref(), Some("caller"));
    }

    #[test]
    fn start_cookie_preserves_expiry_and_allows_cross_site_form_post() {
        const ORACLE: &str =
            "instantdb/instant@4e0a1bd8 server/src/instant/runtime/routes.clj:243-274";
        assert!(ORACLE.contains("4e0a1bd8"));
        let now = Utc.with_ymd_and_hms(2026, 8, 30, 12, 34, 56).unwrap();
        let response = OAuthStartResponse {
            authorization_url: "https://provider.test/auth".to_owned(),
            cookie: Uuid::nil(),
            secure_cookie: true,
        };
        let secure = redirect_with_cookie_at(&response, now).unwrap();
        assert_eq!(
            secure.headers()[header::SET_COOKIE],
            "__session=instantdb_00000000-0000-0000-0000-000000000000; Path=/runtime/oauth; Expires=Sun, 30 Aug 2026 13:34:56 GMT; HttpOnly; Secure; SameSite=None"
        );

        let development = redirect_with_cookie_at(
            &OAuthStartResponse {
                secure_cookie: false,
                ..response
            },
            now,
        )
        .unwrap();
        assert_eq!(
            development.headers()[header::SET_COOKIE],
            "__session=instantdb_00000000-0000-0000-0000-000000000000; Path=/runtime/oauth; Expires=Sun, 30 Aug 2026 13:34:56 GMT; HttpOnly"
        );
    }

    #[tokio::test]
    async fn source_shaped_start_cookie_round_trips_through_form_post_callback() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/auth/oauth.clj:127-136; server/src/instant/runtime/routes.clj:506-600";
        assert!(ORACLE.contains("4e0a1bd8"));
        let backend = Arc::new(FakeBackend::default());
        let cookie = Uuid::new_v4();
        let start = redirect_with_cookie_at(
            &OAuthStartResponse {
                authorization_url: "https://provider.test/auth".to_owned(),
                cookie,
                secure_cookie: true,
            },
            Utc.with_ymd_and_hms(2026, 8, 30, 12, 34, 56).unwrap(),
        )
        .unwrap();
        let set_cookie = start.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(set_cookie.contains("; Secure; SameSite=None"));
        assert!(!set_cookie.contains("Max-Age"));
        let cookie_pair = set_cookie.split(';').next().unwrap();
        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtime/oauth/callback")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::COOKIE, format!("other=x; {cookie_pair}"))
                    .body(Body::from("state=opaque&code=provider-code"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        let callbacks = backend.callbacks.lock().unwrap();
        assert_eq!(callbacks[0].cookie, Some(cookie));
        assert_eq!(callbacks[0].params["code"], "provider-code");
    }

    #[tokio::test]
    async fn token_routes_preserve_origin_and_response_shape() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let code = Uuid::new_v4();
        let response = runtime_oauth_router(backend)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runtime/{app_id}/oauth/token"))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(header::ORIGIN, "https://app.test")
                    .body(Body::from(format!("code={code}&code_verifier=verifier")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["refresh_token"], code.to_string());
        assert_eq!(body["user"]["refresh_token"], code.to_string());
        assert_eq!(body["created"], true);
    }

    #[tokio::test]
    async fn unscoped_token_route_accepts_the_sdk_json_body() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let code = Uuid::new_v4();
        let response = runtime_oauth_router(backend)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtime/oauth/token")
                    .header(header::CONTENT_TYPE, "Application/JSON; charset=utf-8")
                    .body(Body::from(
                        serde_json::json!({"app_id": app_id, "code": code}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_routes_match_the_upstream_source_precedence_and_body_only_fields() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/runtime/routes.clj:602-631; ring-clojure/ring@1.13.0 ring.middleware.params/params-request; compojure/compojure@1.6.2 compojure.core/route-matches";
        assert!(ORACLE.contains("4e0a1bd8"));
        let backend = Arc::new(FakeBackend::default());
        let query_app = Uuid::new_v4();
        let query_code = Uuid::new_v4();
        let body_app = Uuid::new_v4();
        let body_code = Uuid::new_v4();
        let refresh_token = Uuid::new_v4();

        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/runtime/oauth/token?app_id={query_app}&code={query_code}&code_verifier=query-verifier"
                    ))
                    .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                    .header(header::ORIGIN, "https://app.test")
                    .body(Body::from(
                        serde_json::json!({
                            "app_id": body_app,
                            "code": body_code,
                            "code_verifier": "body-verifier",
                            "refresh_token": refresh_token,
                            "extra_fields": {"plan": "pro"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let path_app = Uuid::new_v4();
        let ignored_query_app = Uuid::new_v4();
        let query_form_code = Uuid::new_v4();
        let ignored_form_code = Uuid::new_v4();
        let ignored_form_refresh = Uuid::new_v4();
        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/runtime/{path_app}/oauth/token?app_id={ignored_query_app}&code={query_form_code}&code_verifier=query-over-form"
                    ))
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "code={ignored_form_code}&code_verifier=form-verifier&refresh_token={ignored_form_refresh}&extra_fields=ignored"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let query_only_app = Uuid::new_v4();
        let query_only_code = Uuid::new_v4();
        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/runtime/oauth/token?app_id={query_only_app}&code={query_only_code}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let exchanges = backend.exchanges.lock().unwrap();
        assert_eq!(exchanges.len(), 3);
        assert_eq!(
            exchanges[0],
            OAuthTokenRequest {
                app_id: query_app,
                code: query_code,
                code_verifier: Some("query-verifier".to_owned()),
                refresh_token: Some(refresh_token),
                extra_fields: BTreeMap::from([("plan".to_owned(), Value::from("pro"))]),
                origin: Some("https://app.test".to_owned()),
            }
        );
        assert_eq!(exchanges[1].app_id, path_app);
        assert_eq!(exchanges[1].code, query_form_code);
        assert_eq!(
            exchanges[1].code_verifier.as_deref(),
            Some("query-over-form")
        );
        assert_eq!(exchanges[1].refresh_token, None);
        assert!(exchanges[1].extra_fields.is_empty());
        assert_eq!(exchanges[2].app_id, query_only_app);
        assert_eq!(exchanges[2].code, query_only_code);
    }

    #[tokio::test]
    async fn token_route_errors_report_the_selected_source_and_all_possible_locations() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/util/exception.clj:440-457; server/src/instant/runtime/routes.clj:602-614";
        assert!(ORACLE.contains("4e0a1bd8"));
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let valid_body_code = Uuid::new_v4();
        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/runtime/oauth/token?app_id={app_id}&code=not-a-uuid"
                    ))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"code": valid_body_code}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "type": "param-malformed",
                "message": "Malformed parameter: [params code]",
                "hint": {
                    "in": ["params", "code"],
                    "possible-ins": [
                        ["params", "code"], ["params", "code"],
                        ["body", "code"], ["body", "code"],
                        ["form-params", "code"], ["form-params", "code"]
                    ],
                    "original-input": "not-a-uuid"
                }
            })
        );
        assert!(backend.exchanges.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn token_route_app_id_errors_preserve_body_and_merged_params_paths_and_missing_envelopes()
    {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/util/exception.clj:440-457; server/src/instant/runtime/routes.clj:602-614";
        assert!(ORACLE.contains("4e0a1bd8"));
        let backend = Arc::new(FakeBackend::default());
        let valid_code = Uuid::new_v4();
        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtime/oauth/token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({"app_id": 42, "code": valid_code}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["message"], "Malformed parameter: [body app_id]");
        assert_eq!(body["hint"]["in"], serde_json::json!(["body", "app_id"]));
        assert_eq!(body["hint"]["original-input"], 42);
        assert_eq!(body["hint"]["possible-ins"].as_array().unwrap().len(), 6);

        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runtime/not-a-uuid/oauth/token?code={valid_code}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["message"], "Malformed parameter: [params app_id]");
        assert_eq!(body["hint"]["in"], serde_json::json!(["params", "app_id"]));
        assert_eq!(body["hint"]["original-input"], "not-a-uuid");

        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtime/oauth/token")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("app_id=bad-form-app&code={valid_code}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["message"], "Malformed parameter: [params app_id]");
        assert_eq!(body["hint"]["in"], serde_json::json!(["params", "app_id"]));
        assert_eq!(body["hint"]["original-input"], "bad-form-app");

        let response = runtime_oauth_router(backend)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtime/oauth/token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "type": "param-missing",
                "message": "Missing parameter: [params app_id]",
                "hint": {
                    "in": ["params", "app_id"],
                    "possible-ins": [
                        ["params", "app_id"],
                        ["body", "app_id"], ["body", "app_id"],
                        ["form-params", "app_id"], ["form-params", "app_id"]
                    ]
                }
            })
        );
    }

    #[tokio::test]
    async fn id_token_route_coerces_raw_body_values_and_preserves_the_production_handoff() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/runtime/routes.clj:661-704; server/src/instant/util/string.clj:31-37";
        assert!(ORACLE.contains("4e0a1bd8"));
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let refresh_token = Uuid::new_v4();
        let cases = [
            (
                serde_json::json!({
                    "id_token": "  signed-token  ",
                    "app_id": format!("  {app_id}  "),
                    "refresh_token": null,
                    "client_name": "  web  ",
                    "nonce": "nonce-1",
                    "extra_fields": null
                }),
                OAuthIdTokenRequest {
                    id_token: "signed-token".to_owned(),
                    app_id,
                    refresh_token: None,
                    client_name: "web".to_owned(),
                    nonce: Some("nonce-1".to_owned()),
                    extra_fields: BTreeMap::new(),
                    origin: Some("https://app.test".to_owned()),
                },
            ),
            (
                serde_json::json!({
                    "id_token": 12345,
                    "app_id": app_id,
                    "refresh_token": refresh_token,
                    "client_name": 42,
                    "nonce": "nonce-2",
                    "extra_fields": {"plan": "pro"}
                }),
                OAuthIdTokenRequest {
                    id_token: "12345".to_owned(),
                    app_id,
                    refresh_token: Some(refresh_token),
                    client_name: "42".to_owned(),
                    nonce: Some("nonce-2".to_owned()),
                    extra_fields: BTreeMap::from([("plan".to_owned(), Value::from("pro"))]),
                    origin: Some("https://app.test".to_owned()),
                },
            ),
        ];

        for (body, _) in &cases {
            let response = runtime_oauth_router(Arc::clone(&backend))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/runtime/oauth/id_token")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::ORIGIN, "https://app.test")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let response_body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(response_body["created"], true);
            assert!(response_body.get("refresh_token").is_none());
        }

        let captured = backend.id_token_exchanges.lock().unwrap();
        assert_eq!(captured.as_slice(), cases.map(|(_, request)| request));
    }

    #[tokio::test]
    async fn id_token_route_reports_exact_pinned_missing_and_malformed_body_envelopes() {
        const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/util/exception.clj:409-438; server/src/instant/runtime/routes.clj:661-669";
        assert!(ORACLE.contains("4e0a1bd8"));
        let app_id = Uuid::new_v4();
        let cases = [
            (
                serde_json::json!({}),
                serde_json::json!({
                    "type": "param-missing",
                    "message": "Missing parameter: [body id_token]",
                    "hint": {"in": ["body", "id_token"]}
                }),
            ),
            (
                serde_json::json!({"id_token": "   ", "app_id": app_id, "client_name": "web"}),
                serde_json::json!({
                    "type": "param-malformed",
                    "message": "Malformed parameter: [body id_token]",
                    "hint": {"in": ["body", "id_token"], "original-input": "   "}
                }),
            ),
            (
                serde_json::json!({"id_token": "token", "app_id": "bad", "client_name": "web"}),
                serde_json::json!({
                    "type": "param-malformed",
                    "message": "Malformed parameter: [body app_id]",
                    "hint": {"in": ["body", "app_id"], "original-input": "bad"}
                }),
            ),
            (
                serde_json::json!({"id_token": "token", "app_id": app_id, "refresh_token": 7, "client_name": "web"}),
                serde_json::json!({
                    "type": "param-malformed",
                    "message": "Malformed parameter: [body refresh_token]",
                    "hint": {"in": ["body", "refresh_token"], "original-input": 7}
                }),
            ),
            (
                serde_json::json!({"id_token": "token", "app_id": app_id, "client_name": false}),
                serde_json::json!({
                    "type": "param-missing",
                    "message": "Missing parameter: [body client_name]",
                    "hint": {"in": ["body", "client_name"]}
                }),
            ),
        ];
        let backend = Arc::new(FakeBackend::default());

        for (body, expected) in cases {
            let response = runtime_oauth_router(Arc::clone(&backend))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/runtime/oauth/id_token")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let actual: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert_eq!(actual, expected);
        }
        assert!(backend.id_token_exchanges.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_redirect_does_not_invoke_provider_backend() {
        let backend = Arc::new(FakeBackend::default());
        let response = runtime_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .uri("/runtime/oauth/callback?test-redirect=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(backend.callbacks.lock().unwrap().is_empty());
    }
}
