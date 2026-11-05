use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::{ApiError, ApiErrorType};

const COOKIE_NAME: &str = "__session";
const COOKIE_PREFIX: &str = "instantdb_";
const DECISION_BODY_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformOAuthStartRequest {
    pub client_id: Uuid,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
    pub state: String,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformOAuthStartResponse {
    pub dashboard_url: String,
    pub cookie: Uuid,
    pub secure_cookie: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformOAuthClaimRequest {
    pub bearer_token: String,
    pub redirect_id: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformOAuthDecisionRequest {
    pub redirect_id: Uuid,
    pub grant_token: Uuid,
    pub cookie: Option<Uuid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformOAuthTokenRequest {
    pub grant_type: String,
    pub client_id: Uuid,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
    pub code: Option<Uuid>,
    pub code_verifier: Option<String>,
    pub refresh_token: Option<String>,
}

pub trait PlatformOAuthBackend: Send + Sync + 'static {
    fn start(
        &self,
        request: PlatformOAuthStartRequest,
    ) -> impl Future<Output = Result<PlatformOAuthStartResponse, ApiError>> + Send;

    fn claim(
        &self,
        request: PlatformOAuthClaimRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn grant(
        &self,
        request: PlatformOAuthDecisionRequest,
    ) -> impl Future<Output = Result<String, ApiError>> + Send;

    fn deny(
        &self,
        request: PlatformOAuthDecisionRequest,
    ) -> impl Future<Output = Result<String, ApiError>> + Send;

    fn token(
        &self,
        request: PlatformOAuthTokenRequest,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn token_info(
        &self,
        access_token: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn revoke(&self, token: String) -> impl Future<Output = Result<(), ApiError>> + Send;
}

pub fn platform_oauth_router<B>(backend: Arc<B>) -> Router
where
    B: PlatformOAuthBackend,
{
    Router::new()
        .route("/platform/oauth/start", get(start::<B>))
        .route("/platform/oauth/claim", post(claim::<B>))
        .route("/platform/oauth/grant", post(grant::<B>))
        .route("/platform/oauth/deny", post(deny::<B>))
        .route("/platform/oauth/token", post(token::<B>))
        .route("/platform/oauth/token-info", get(token_info::<B>))
        .route("/platform/oauth/revoke", post(revoke::<B>))
        .with_state(backend)
}

#[derive(Debug, Deserialize)]
struct StartParams {
    client_id: Option<Uuid>,
    redirect_uri: Option<String>,
    response_type: Option<String>,
    scope: Option<String>,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
}

async fn start<B>(
    State(backend): State<Arc<B>>,
    params: Result<Query<StartParams>, QueryRejection>,
) -> Result<Response, ApiError>
where
    B: PlatformOAuthBackend,
{
    let Query(params) = params.map_err(|error| query_error(&error))?;
    let response_type = required(params.response_type, "response_type")?;
    if response_type != "code" {
        return Err(malformed(
            "`response_type` parameter must have value `code`",
        ));
    }
    let scope = required(params.scope, "scope")?;
    let scopes = scope.split(' ').map(str::to_owned).collect::<Vec<_>>();
    if scopes.iter().any(String::is_empty) {
        return Err(malformed("The scope param contains an empty scope"));
    }
    if params.code_challenge.is_some() != params.code_challenge_method.is_some() {
        return Err(malformed(
            "code_challenge and code_challenge_method must be provided together",
        ));
    }
    if !matches!(
        params.code_challenge_method.as_deref(),
        None | Some("S256" | "plain")
    ) {
        return Err(malformed("Invalid code_challenge_method"));
    }
    let result = backend
        .start(PlatformOAuthStartRequest {
            client_id: params.client_id.ok_or_else(|| missing("client_id"))?,
            redirect_uri: required(params.redirect_uri, "redirect_uri")?,
            scopes,
            state: required(params.state, "state")?,
            code_challenge: params.code_challenge,
            code_challenge_method: params.code_challenge_method,
        })
        .await?;
    let secure = if result.secure_cookie { "; Secure" } else { "" };
    let cookie = format!(
        "{COOKIE_NAME}={COOKIE_PREFIX}{}; Path=/platform/oauth; Max-Age=3600; HttpOnly; SameSite=Lax{secure}",
        result.cookie
    );
    let mut response = Redirect::temporary(&result.dashboard_url).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| malformed("Invalid OAuth cookie"))?,
    );
    Ok(response)
}

#[derive(Debug, Deserialize)]
struct ClaimBody {
    redirect: Option<Uuid>,
}

async fn claim<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    body: Result<Json<ClaimBody>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: PlatformOAuthBackend,
{
    let Json(body) = body.map_err(|error| json_error(&error))?;
    backend
        .claim(PlatformOAuthClaimRequest {
            bearer_token: bearer_token(&headers)?,
            redirect_id: body.redirect.ok_or_else(|| missing("redirect"))?,
        })
        .await
        .map(Json)
}

async fn grant<B>(State(backend): State<Arc<B>>, request: Request) -> Result<Response, ApiError>
where
    B: PlatformOAuthBackend,
{
    let result = async {
        let request = decision_request(request).await?;
        let url = backend.grant(request).await?;
        found(&url)
    }
    .await;

    match result {
        Ok(response) => Ok(response),
        Err(error) if error.body().kind != ApiErrorType::Unknown => {
            Ok(oauth_error_page(&error.body().message))
        }
        Err(error) => {
            // The JVM route only converts ExceptionInfo domain failures carrying a
            // public message into the browser error page. Database/runtime failures
            // fall through to the global error handler; do the same without
            // reflecting their internal diagnostic into either HTML or JSON.
            let trace_id = error.body().trace_id.clone();
            let mut public_error = ApiError::new(ApiErrorType::Unknown, "Internal server error");
            if let Some(trace_id) = trace_id {
                public_error = public_error.with_trace_id(trace_id);
            }
            Err(public_error)
        }
    }
}

async fn deny<B>(State(backend): State<Arc<B>>, request: Request) -> Result<Response, ApiError>
where
    B: PlatformOAuthBackend,
{
    backend
        .deny(decision_request(request).await?)
        .await
        .and_then(|url| found(&url))
}

async fn token<B>(State(backend): State<Arc<B>>, request: Request) -> Result<Json<Value>, ApiError>
where
    B: PlatformOAuthBackend,
{
    let params = body_params(request).await?;
    backend.token(token_request(&params)?).await.map(Json)
}

async fn token_info<B>(
    State(backend): State<Arc<B>>,
    params: Result<Query<BTreeMap<String, String>>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: PlatformOAuthBackend,
{
    let Query(params) = params.map_err(|error| query_error(&error))?;
    backend
        .token_info(required_map(&params, "access_token")?)
        .await
        .map(Json)
}

async fn revoke<B>(State(backend): State<Arc<B>>, request: Request) -> Result<Json<Value>, ApiError>
where
    B: PlatformOAuthBackend,
{
    let params = body_params(request).await?;
    backend.revoke(required_map(&params, "token")?).await?;
    Ok(Json(serde_json::json!({})))
}

async fn decision_request(request: Request) -> Result<PlatformOAuthDecisionRequest, ApiError> {
    let cookie = oauth_cookie(request.headers())?;
    let query = request
        .uri()
        .query()
        .map(|query| decision_params(query.as_bytes(), "Malformed query parameters"))
        .transpose()?
        .unwrap_or_default();

    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mut params = if content_type
        .split(';')
        .next()
        .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
    {
        let body = to_bytes(request.into_body(), DECISION_BODY_LIMIT)
            .await
            .map_err(|_| malformed("Could not read request body"))?;
        decision_params(&body, "Malformed form body")?
    } else {
        BTreeMap::new()
    };
    // Ring parses the form first and then assocs query values over it.
    params.extend(query);

    Ok(PlatformOAuthDecisionRequest {
        redirect_id: required_decision_uuid(&params, "redirect_id")?,
        grant_token: required_decision_uuid(&params, "grant_token")?,
        cookie,
    })
}

fn decision_params(
    input: &[u8],
    malformed_message: &'static str,
) -> Result<BTreeMap<String, Vec<String>>, ApiError> {
    let pairs = serde_urlencoded::from_bytes::<Vec<(String, String)>>(input)
        .map_err(|_| malformed(malformed_message))?;
    let mut params = BTreeMap::<String, Vec<String>>::new();
    for (key, value) in pairs {
        params.entry(key).or_default().push(value);
    }
    Ok(params)
}

fn required_decision_uuid(
    params: &BTreeMap<String, Vec<String>>,
    key: &'static str,
) -> Result<Uuid, ApiError> {
    let values = params.get(key).ok_or_else(|| decision_missing(key))?;
    if let [value] = values.as_slice()
        && java_string_trim(value).len() == 36
        && let Ok(uuid) = Uuid::parse_str(java_string_trim(value))
    {
        return Ok(uuid);
    }
    Err(decision_malformed(key, values))
}

fn decision_missing(key: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: [params {key}]"),
    )
    .with_hint(serde_json::json!({"in": ["params", key]}))
}

fn decision_malformed(key: &'static str, values: &[String]) -> ApiError {
    let original_input = match values {
        [value] => Value::String(value.clone()),
        values => Value::Array(values.iter().cloned().map(Value::String).collect()),
    };
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: [params {key}]"),
    )
    .with_hint(serde_json::json!({
        "in": ["params", key],
        "original-input": original_input,
    }))
}

fn found(url: &str) -> Result<Response, ApiError> {
    let location = HeaderValue::from_str(url).map_err(|_| malformed("Invalid redirect URL"))?;
    Ok((
        axum::http::StatusCode::FOUND,
        [(header::LOCATION, location)],
    )
        .into_response())
}

fn oauth_error_page(error: &str) -> Response {
    let error = escape_html(error);
    let body = format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="UTF-8" /><meta content="width=device-width, initial-scale=1.0" name="viewport" /><title>OAuth error</title><style>
                           body {{
                             margin: 0;
                             height: 100vh;
                             display: flex;
                             justify-content: center;
                             align-items: center;
                             background-color: white;
                             flex-direction: column;
                             font-family: sans-serif;
                           }}

                           a.button {{
                             text-decoration: none;
                             padding: 15px 30px;
                             font-size: 18px;
                             border-radius: 5px;
                             font-family: sans-serif;
                             text-align: center;
                           }}

                           a {{
                             cursor: pointer;
                           }}

                           @media (prefers-color-scheme: dark) {{
                             body {{
                               background-color: black;
                             }}
                             a.button {{
                               color: black;
                               background-color: white;
                             }}
                           }}

                           @media (prefers-color-scheme: light) {{
                             a.button {{
                               color: white;
                               background-color: black;
                             }}
                           }}</style></head><body><p>There was an error with your OAuth request.</p><p>{error}</p></body></html>"#
    );
    (
        axum::http::StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "text/html")],
        body,
    )
        .into_response()
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

async fn body_params(request: Request) -> Result<BTreeMap<String, String>, ApiError> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| malformed("Malformed Content-Type header"))
        })
        .transpose()?
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let body = to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|_| malformed("Could not read request body"))?;
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type == "application/json" {
        let values = serde_json::from_slice::<BTreeMap<String, Value>>(&body)
            .map_err(|_| malformed("Malformed JSON body"))?;
        return values
            .into_iter()
            .map(|(key, value)| match value {
                Value::String(value) => Ok((key, value)),
                _ => Err(malformed("OAuth parameters must be strings")),
            })
            .collect();
    }
    if media_type == "application/x-www-form-urlencoded" {
        return serde_urlencoded::from_bytes(&body).map_err(|_| malformed("Malformed form body"));
    }
    Err(malformed("Unsupported OAuth token content type"))
}

fn token_request(params: &BTreeMap<String, String>) -> Result<PlatformOAuthTokenRequest, ApiError> {
    Ok(PlatformOAuthTokenRequest {
        grant_type: required_map(params, "grant_type")?,
        client_id: Uuid::parse_str(&required_map(params, "client_id")?)
            .map_err(|_| malformed("Malformed client_id"))?,
        client_secret: optional_map(params, "client_secret"),
        redirect_uri: optional_map(params, "redirect_uri"),
        code: optional_uuid(params, "code")?,
        code_verifier: optional_map(params, "code_verifier"),
        refresh_token: optional_map(params, "refresh_token"),
    })
}

fn bearer_token(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| missing("Bearer authorization").unauthorized())?;
    let value = value
        .to_str()
        .map_err(|_| malformed("Malformed Authorization header").unauthorized())?;
    value
        .split_once(' ')
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("Bearer").then_some(token))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| malformed("Malformed Bearer authorization").unauthorized())
}

fn oauth_cookie(headers: &HeaderMap) -> Result<Option<Uuid>, ApiError> {
    let mut cookie = None;
    for raw in headers.get_all(header::COOKIE) {
        let raw = raw
            .to_str()
            .map_err(|_| malformed("Malformed Cookie header"))?;
        for part in raw.split([';', ',']) {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name != COOKIE_NAME {
                continue;
            }
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value);
            if let Some(value) = value.strip_prefix(COOKIE_PREFIX)
                && java_string_trim(value).len() == 36
                && let Ok(value) = Uuid::parse_str(java_string_trim(value))
            {
                cookie = Some(value);
            }
        }
    }
    Ok(cookie)
}

fn java_string_trim(value: &str) -> &str {
    value.trim_matches(|character| character <= '\u{20}')
}

fn optional_uuid(
    params: &BTreeMap<String, String>,
    key: &'static str,
) -> Result<Option<Uuid>, ApiError> {
    optional_map(params, key)
        .map(|value| Uuid::parse_str(&value).map_err(|_| malformed("Malformed UUID parameter")))
        .transpose()
}

fn optional_map(params: &BTreeMap<String, String>, key: &str) -> Option<String> {
    params
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn required_map(params: &BTreeMap<String, String>, key: &'static str) -> Result<String, ApiError> {
    required(optional_map(params, key), key)
}

fn required(value: Option<String>, key: &'static str) -> Result<String, ApiError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| missing(key))
}

fn missing(key: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing {key} parameter"),
    )
}

fn malformed(message: &'static str) -> ApiError {
    ApiError::new(ApiErrorType::ParamMalformed, message)
}

fn query_error(error: &QueryRejection) -> ApiError {
    malformed("Malformed query parameters")
        .with_hint(serde_json::json!({"error": error.body_text()}))
}

fn json_error(error: &JsonRejection) -> ApiError {
    malformed("Malformed JSON body").with_hint(serde_json::json!({"error": error.body_text()}))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::StatusCode;
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct Backend {
        starts: Mutex<Vec<PlatformOAuthStartRequest>>,
        grants: Mutex<Vec<PlatformOAuthDecisionRequest>>,
        grant_error: Mutex<Option<ApiError>>,
        denies: Mutex<Vec<PlatformOAuthDecisionRequest>>,
        tokens: Mutex<Vec<PlatformOAuthTokenRequest>>,
    }

    impl PlatformOAuthBackend for Backend {
        fn start(
            &self,
            request: PlatformOAuthStartRequest,
        ) -> impl Future<Output = Result<PlatformOAuthStartResponse, ApiError>> + Send {
            self.starts.lock().unwrap().push(request);
            std::future::ready(Ok(PlatformOAuthStartResponse {
                dashboard_url: "https://dash.test/platform/oauth/start?redirect-id=1".to_owned(),
                cookie: Uuid::nil(),
                secure_cookie: true,
            }))
        }

        fn claim(
            &self,
            _request: PlatformOAuthClaimRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(serde_json::json!({"appName": "Example"})))
        }

        fn grant(
            &self,
            request: PlatformOAuthDecisionRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.grants.lock().unwrap().push(request);
            std::future::ready(match self.grant_error.lock().unwrap().take() {
                Some(error) => Err(error),
                None => Ok("https://client.test/callback?code=1".to_owned()),
            })
        }

        fn deny(
            &self,
            request: PlatformOAuthDecisionRequest,
        ) -> impl Future<Output = Result<String, ApiError>> + Send {
            self.denies.lock().unwrap().push(request);
            std::future::ready(Ok(
                "https://client.test/callback?error=access_denied".to_owned()
            ))
        }

        fn token(
            &self,
            request: PlatformOAuthTokenRequest,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            self.tokens.lock().unwrap().push(request);
            std::future::ready(Ok(serde_json::json!({"token_type": "Bearer"})))
        }

        fn token_info(
            &self,
            _access_token: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(serde_json::json!({"token_type": "Bearer"})))
        }

        fn revoke(&self, _token: String) -> impl Future<Output = Result<(), ApiError>> + Send {
            std::future::ready(Ok(()))
        }
    }

    async fn assert_oauth_error_page(response: Response, message: &str) {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert_eq!(body, oauth_error_page_body(message));
    }

    fn oauth_error_page_body(message: &str) -> String {
        let message = escape_html(message);
        format!(
            r#"<!DOCTYPE html><html lang="en"><head><meta charset="UTF-8" /><meta content="width=device-width, initial-scale=1.0" name="viewport" /><title>OAuth error</title><style>
                           body {{
                             margin: 0;
                             height: 100vh;
                             display: flex;
                             justify-content: center;
                             align-items: center;
                             background-color: white;
                             flex-direction: column;
                             font-family: sans-serif;
                           }}

                           a.button {{
                             text-decoration: none;
                             padding: 15px 30px;
                             font-size: 18px;
                             border-radius: 5px;
                             font-family: sans-serif;
                             text-align: center;
                           }}

                           a {{
                             cursor: pointer;
                           }}

                           @media (prefers-color-scheme: dark) {{
                             body {{
                               background-color: black;
                             }}
                             a.button {{
                               color: black;
                               background-color: white;
                             }}
                           }}

                           @media (prefers-color-scheme: light) {{
                             a.button {{
                               color: white;
                               background-color: black;
                             }}
                           }}</style></head><body><p>There was an error with your OAuth request.</p><p>{message}</p></body></html>"#
        )
    }

    #[tokio::test]
    async fn grant_outer_failures_render_the_pinned_oauth_browser_error_page() {
        let backend = Arc::new(Backend::default());
        let router = platform_oauth_router(Arc::clone(&backend));
        let response = router
            .clone()
            .oneshot(
                Request::post("/platform/oauth/grant")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_oauth_error_page(response, "Missing parameter: [params redirect_id]").await;
        assert!(backend.grants.lock().unwrap().is_empty());

        *backend.grant_error.lock().unwrap() = Some(ApiError::new(
            ApiErrorType::RecordExpired,
            "Expired oauth-app-redirect <gone> & 'spent'",
        ));
        let response = router
            .oneshot(
                Request::post(format!(
                    "/platform/oauth/grant?redirect_id={}&grant_token={}",
                    Uuid::from_u128(1),
                    Uuid::from_u128(2),
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_oauth_error_page(response, "Expired oauth-app-redirect <gone> & 'spent'").await;
        assert_eq!(backend.grants.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn grant_internal_failures_use_the_sanitized_global_error_response() {
        let backend = Arc::new(Backend::default());
        *backend.grant_error.lock().unwrap() = Some(
            ApiError::new(
                ApiErrorType::Unknown,
                "Database error: oauth-secret-material <must-not-be-reflected>",
            )
            .with_trace_id("trace-123"),
        );

        let response = platform_oauth_router(Arc::clone(&backend))
            .oneshot(
                Request::post(format!(
                    "/platform/oauth/grant?redirect_id={}&grant_token={}",
                    Uuid::from_u128(1),
                    Uuid::from_u128(2),
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "type": "unknown",
                "message": "Internal server error",
                "trace-id": "trace-123",
            })
        );
        assert_eq!(backend.grants.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deny_outer_failures_keep_the_existing_json_error_contract() {
        let response = platform_oauth_router(Arc::new(Backend::default()))
            .oneshot(
                Request::post("/platform/oauth/deny")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"type":"param-missing","message":"Missing parameter: [params redirect_id]","hint":{"in":["params","redirect_id"]}}"#
        );
    }

    #[tokio::test]
    async fn dashboard_decision_forms_and_query_override_redirect_with_get_semantics() {
        let backend = Arc::new(Backend::default());
        let router = platform_oauth_router(Arc::clone(&backend));
        let cookie = Uuid::from_u128(3);

        let response = router
            .clone()
            .oneshot(
                Request::post(format!(
                    "/platform/oauth/grant?redirect_id={}&grant_token={}",
                    Uuid::from_u128(10),
                    Uuid::from_u128(11),
                ))
                .header(
                    header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded; charset=UTF-8",
                )
                .header(
                    header::COOKIE,
                    format!("{COOKIE_NAME}={COOKIE_PREFIX}{cookie}"),
                )
                .body(Body::from(format!(
                    "redirect_id={}&grant_token={}",
                    Uuid::from_u128(1),
                    Uuid::from_u128(2),
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://client.test/callback?code=1"
        );
        assert_eq!(
            backend.grants.lock().unwrap().as_slice(),
            &[PlatformOAuthDecisionRequest {
                redirect_id: Uuid::from_u128(10),
                grant_token: Uuid::from_u128(11),
                cookie: Some(cookie),
            }]
        );

        let response = router
            .oneshot(
                Request::post("/platform/oauth/deny")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header(
                        header::COOKIE,
                        format!("{COOKIE_NAME}={COOKIE_PREFIX}{cookie}"),
                    )
                    .body(Body::from(format!(
                        "redirect_id={}&grant_token={}",
                        Uuid::from_u128(4),
                        Uuid::from_u128(5),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            response.headers()[header::LOCATION],
            "https://client.test/callback?error=access_denied"
        );
        assert_eq!(
            backend.denies.lock().unwrap().as_slice(),
            &[PlatformOAuthDecisionRequest {
                redirect_id: Uuid::from_u128(4),
                grant_token: Uuid::from_u128(5),
                cookie: Some(cookie),
            }]
        );
    }

    #[tokio::test]
    async fn decision_params_reject_duplicates_with_exact_ring_errors() {
        for request in [
            Request::post(format!(
                "/platform/oauth/grant?redirect_id={0}&redirect_id={1}&grant_token={2}",
                Uuid::from_u128(1),
                Uuid::from_u128(2),
                Uuid::from_u128(3),
            ))
            .body(Body::empty())
            .unwrap(),
            Request::post("/platform/oauth/grant")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!(
                    "redirect_id={0}&redirect_id={1}&grant_token={2}",
                    Uuid::from_u128(1),
                    Uuid::from_u128(2),
                    Uuid::from_u128(3),
                )))
                .unwrap(),
        ] {
            let error = decision_request(request).await.unwrap_err();
            assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
            assert_eq!(
                error.body().message,
                "Malformed parameter: [params redirect_id]"
            );
            assert_eq!(
                error.body().hint,
                Some(serde_json::json!({
                    "in": ["params", "redirect_id"],
                    "original-input": [
                        Uuid::from_u128(1).to_string(),
                        Uuid::from_u128(2).to_string(),
                    ],
                }))
            );
        }

        let missing = decision_request(
            Request::post("/platform/oauth/grant")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap_err();
        assert_eq!(missing.body().kind, ApiErrorType::ParamMissing);
        assert_eq!(
            missing.body().message,
            "Missing parameter: [params redirect_id]"
        );
        assert_eq!(
            missing.body().hint,
            Some(serde_json::json!({"in": ["params", "redirect_id"]}))
        );
    }

    #[test]
    fn decision_cookie_keeps_only_the_last_valid_prefixed_value() {
        let expected = Uuid::from_u128(3);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!(
                "{COOKIE_NAME}={}; {COOKIE_NAME}={COOKIE_PREFIX}not-a-uuid; {COOKIE_NAME}=\"{COOKIE_PREFIX}{expected}\"",
                Uuid::from_u128(2),
            )
            .parse()
            .unwrap(),
        );
        assert_eq!(oauth_cookie(&headers).unwrap(), Some(expected));

        headers.insert(
            header::COOKIE,
            format!("{COOKIE_NAME}={}", Uuid::from_u128(2))
                .parse()
                .unwrap(),
        );
        assert_eq!(oauth_cookie(&headers).unwrap(), None);
    }

    #[test]
    fn decision_uuid_coercion_uses_java_trim_boundaries() {
        let id = Uuid::from_u128(1);
        let ascii_whitespace =
            BTreeMap::from([("redirect_id".to_owned(), vec![format!(" \t{id}\r\n")])]);
        assert_eq!(
            required_decision_uuid(&ascii_whitespace, "redirect_id"),
            Ok(id)
        );

        let nbsp = '\u{a0}';
        let unicode_whitespace =
            BTreeMap::from([("redirect_id".to_owned(), vec![format!("{nbsp}{id}{nbsp}")])]);
        let error = required_decision_uuid(&unicode_whitespace, "redirect_id").unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
        assert_eq!(
            error.body().hint,
            Some(serde_json::json!({
                "in": ["params", "redirect_id"],
                "original-input": format!("{nbsp}{id}{nbsp}"),
            }))
        );
    }

    #[tokio::test]
    async fn decision_form_body_limit_accepts_boundary_and_rejects_excess() {
        let prefix = format!(
            "redirect_id={}&grant_token={}&padding=",
            Uuid::from_u128(1),
            Uuid::from_u128(2),
        );
        let body = format!("{prefix}{}", "x".repeat(DECISION_BODY_LIMIT - prefix.len()));
        let request = Request::post("/platform/oauth/grant")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body.clone()))
            .unwrap();
        assert!(decision_request(request).await.is_ok());

        let request = Request::post("/platform/oauth/grant")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("{body}x")))
            .unwrap();
        let error = decision_request(request).await.unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
        assert_eq!(error.body().message, "Could not read request body");
    }

    #[tokio::test]
    async fn decision_routes_remain_post_only() {
        let response = platform_oauth_router(Arc::new(Backend::default()))
            .oneshot(
                Request::get("/platform/oauth/grant")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn decision_form_media_type_matches_ring_case_sensitive_prefix() {
        let body = format!(
            "redirect_id={}&grant_token={}",
            Uuid::from_u128(1),
            Uuid::from_u128(2),
        );
        let accepted = Request::post("/platform/oauth/grant")
            .header(
                header::CONTENT_TYPE,
                "application/x-www-form-urlencodedevil; charset=UTF-8",
            )
            .body(Body::from(body.clone()))
            .unwrap();
        assert!(decision_request(accepted).await.is_ok());

        let ignored = Request::post("/platform/oauth/grant")
            .header(header::CONTENT_TYPE, "Application/X-Www-Form-Urlencoded")
            .body(Body::from(body))
            .unwrap();
        let error = decision_request(ignored).await.unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ParamMissing);
    }

    #[test]
    fn decision_redirect_rejects_header_injection() {
        let error = found("https://client.test/callback\r\nX-Injected: true").unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
        assert_eq!(error.body().message, "Invalid redirect URL");
    }

    #[tokio::test]
    async fn start_and_token_preserve_redirect_cookie_and_dual_body_contract() {
        let backend = Arc::new(Backend::default());
        let router = platform_oauth_router(Arc::clone(&backend));
        let response = router
            .clone()
            .oneshot(Request::get("/platform/oauth/start?client_id=00000000-0000-0000-0000-000000000001&redirect_uri=https%3A%2F%2Fclient.test%2Fcb&response_type=code&scope=apps-read%20data-write&state=s&code_challenge=x&code_challenge_method=S256").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert!(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("__session=instantdb_00000000-0000-0000-0000-000000000000")
        );
        assert_eq!(
            backend.starts.lock().unwrap()[0].scopes,
            ["apps-read", "data-write"]
        );

        let response = router
            .oneshot(Request::post("/platform/oauth/token").header(header::CONTENT_TYPE, "application/x-www-form-urlencoded").body(Body::from("grant_type=authorization_code&client_id=00000000-0000-0000-0000-000000000001&code=00000000-0000-0000-0000-000000000002&redirect_uri=https%3A%2F%2Fclient.test%2Fcb&code_verifier=v")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"token_type":"Bearer"}"#
        );
        assert_eq!(
            backend.tokens.lock().unwrap()[0].code_verifier.as_deref(),
            Some("v")
        );
    }

    #[tokio::test]
    async fn token_accepts_case_insensitive_json_media_type() {
        let backend = Arc::new(Backend::default());
        let response = platform_oauth_router(backend)
            .oneshot(
                Request::post("/platform/oauth/token")
                    .header(header::CONTENT_TYPE, "Application/JSON; charset=utf-8")
                    .body(Body::from(
                        r#"{"grant_type":"authorization_code","client_id":"00000000-0000-0000-0000-000000000001"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_rejects_non_utf8_content_type_header() {
        let request = Request::post("/platform/oauth/token")
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_bytes(&[0xff]).unwrap(),
            )
            .body(Body::from(""))
            .unwrap();
        let error = body_params(request).await.unwrap_err();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
        assert_eq!(error.body().message, "Malformed Content-Type header");
    }

    #[tokio::test]
    async fn token_rejects_jsonp_media_type_instead_of_treating_it_as_json() {
        let request = Request::post("/platform/oauth/token")
            .header(header::CONTENT_TYPE, "application/jsonp")
            .body(Body::from("{}"))
            .unwrap();
        let error = body_params(request).await.unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
        assert_eq!(error.body().message, "Unsupported OAuth token content type");
    }

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

    #[test]
    fn malformed_present_authorization_is_not_reported_as_missing() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic token".parse().unwrap());
        let error = bearer_token(&headers).unwrap_err();
        assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
    }

    #[test]
    fn bearer_credentials_allow_http_whitespace_separator() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer   token".parse().unwrap());
        assert_eq!(bearer_token(&headers).unwrap(), "token");
    }

    #[test]
    fn blank_oauth_parameters_are_missing() {
        let error = required(Some("  ".to_owned()), "state").unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::ParamMissing);
        assert_eq!(
            optional_map(
                &BTreeMap::from([("state".to_owned(), "  ".to_owned())]),
                "state"
            ),
            None
        );
    }
}
