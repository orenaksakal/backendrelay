use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::admin::{json_object, query_params};
use crate::{ApiError, ApiErrorType, ResolvedClientIp};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStorageContext {
    pub app_id: Uuid,
    pub refresh_token: Option<String>,
    /// Address resolved through trusted proxies, used by `request.ip` rules.
    pub client_ip: Option<IpAddr>,
}

pub trait RuntimeStorageBackend: Send + Sync + 'static {
    fn upload(
        &self,
        context: RuntimeStorageContext,
        path: String,
        content_type: Option<String>,
        content_disposition: Option<String>,
        content_length: Option<u64>,
        body: Body,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn delete_file(
        &self,
        context: RuntimeStorageContext,
        path: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn create_upload_url(
        &self,
        context: RuntimeStorageContext,
        path: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn consume_upload_url(
        &self,
        upload_id: Uuid,
        content_type: String,
        content_length: Option<u64>,
        body: Body,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn create_download_url(
        &self,
        context: RuntimeStorageContext,
        path: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

pub fn runtime_storage_router<B>(backend: Arc<B>) -> Router
where
    B: RuntimeStorageBackend,
{
    Router::new()
        .route("/storage/upload", put(upload::<B>))
        .route("/storage/files", axum::routing::delete(delete_file::<B>))
        .route("/storage/signed-upload-url", post(create_upload_url::<B>))
        .route(
            "/storage/{upload_id}/consume-upload-url",
            put(consume_upload_url::<B>),
        )
        .route(
            "/storage/signed-download-url",
            get(create_download_url::<B>),
        )
        .with_state(backend)
}

async fn upload<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    body: Body,
) -> Result<Json<Value>, ApiError>
where
    B: RuntimeStorageBackend,
{
    let params = query_params(params)?;
    let context = apply_client_ip(
        if params.contains_key("app-id") || params.contains_key("app_id") {
            context_from_params(&headers, &params)?
        } else {
            context_from_headers(&headers)?
        },
        client_ip,
    );
    let path = match params.get("path") {
        Some(path) => non_blank(path)?,
        None => required_header(&headers, &["path"])?,
    };
    let content_type = optional_content_type(&headers)?;
    let content_disposition = match params.get("content-disposition") {
        Some(value) => Some(non_blank(value)?),
        None => optional_header(&headers, "content-disposition")?,
    };
    let content_length = optional_content_length(&headers)?;
    backend
        .upload(
            context,
            path,
            content_type,
            content_disposition,
            content_length,
            body,
        )
        .await
        .map(data)
}

async fn delete_file<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: RuntimeStorageBackend,
{
    let params = query_params(params)?;
    let context = apply_client_ip(context_from_params(&headers, &params)?, client_ip);
    let path = required_param(&params, &["path", "filename"])?;
    backend.delete_file(context, path).await.map(data)
}

async fn create_upload_url<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: RuntimeStorageBackend,
{
    let body = json_object(payload)?;
    let context = apply_client_ip(context_from_body(&headers, &body)?, client_ip);
    let path = required_value(&body, &["path", "filename"])?;
    backend.create_upload_url(context, path).await.map(data)
}

async fn consume_upload_url<B>(
    State(backend): State<Arc<B>>,
    path: Result<Path<Uuid>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError>
where
    B: RuntimeStorageBackend,
{
    let upload_id = path
        .map(|Path(upload_id)| upload_id)
        .map_err(|error| malformed("[params upload-id]", Some(json!(error.to_string()))))?;
    let content_type = optional_header(&headers, "content-type")?
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let content_length = optional_content_length(&headers)?;
    backend
        .consume_upload_url(upload_id, content_type, content_length, body)
        .await
        .map(data)
}

async fn create_download_url<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    client_ip: Option<Extension<ResolvedClientIp>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: RuntimeStorageBackend,
{
    let params = query_params(params)?;
    let context = apply_client_ip(context_from_params(&headers, &params)?, client_ip);
    let path = required_param(&params, &["path", "filename"])?;
    backend.create_download_url(context, path).await.map(data)
}

fn apply_client_ip(
    mut context: RuntimeStorageContext,
    client_ip: Option<Extension<ResolvedClientIp>>,
) -> RuntimeStorageContext {
    context.client_ip = client_ip.and_then(|Extension(client_ip)| client_ip.0);
    context
}

fn context_from_headers(headers: &HeaderMap) -> Result<RuntimeStorageContext, ApiError> {
    Ok(RuntimeStorageContext {
        app_id: parse_uuid(&required_header(headers, &["app-id", "app_id"])?)?,
        refresh_token: bearer(headers)?,
        client_ip: None,
    })
}

fn context_from_params(
    headers: &HeaderMap,
    params: &HashMap<String, String>,
) -> Result<RuntimeStorageContext, ApiError> {
    Ok(RuntimeStorageContext {
        app_id: parse_uuid(&required_param(params, &["app-id", "app_id"])?)?,
        refresh_token: bearer(headers)?,
        client_ip: None,
    })
}

fn context_from_body(
    headers: &HeaderMap,
    body: &serde_json::Map<String, Value>,
) -> Result<RuntimeStorageContext, ApiError> {
    Ok(RuntimeStorageContext {
        app_id: parse_uuid(&required_value(body, &["app-id", "app_id"])?)?,
        refresh_token: bearer(headers)?,
        client_ip: None,
    })
}

fn bearer(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| malformed("[headers authorization]", None))?;
    let token = value
        .split_once(' ')
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("Bearer").then_some(token))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| malformed("[headers authorization]", Some(json!(value))))?;
    Ok(Some(token.to_owned()))
}

fn required_header(headers: &HeaderMap, keys: &[&str]) -> Result<String, ApiError> {
    for key in keys {
        if let Some(value) = optional_header(headers, key)? {
            return Ok(value);
        }
    }
    Err(missing(&format!("[headers {}]", keys.join(" or "))))
}

fn optional_header(headers: &HeaderMap, key: &str) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get(key) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| malformed(&format!("[headers {key}]"), None))?;
    non_blank(value).map(Some)
}

fn optional_content_type(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    Ok(optional_header(headers, "content-type")?.and_then(|value| {
        let normalized = value.trim();
        (!matches!(
            normalized.to_ascii_lowercase().as_str(),
            "null" | "undefined"
        ))
        .then(|| normalized.to_owned())
    }))
}

fn optional_content_length(headers: &HeaderMap) -> Result<Option<u64>, ApiError> {
    let Some(value) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| malformed("[headers content-length]", None))?;
    value
        .parse()
        .map(Some)
        .map_err(|_| malformed("[headers content-length]", Some(json!(value))))
}

fn required_param(params: &HashMap<String, String>, keys: &[&str]) -> Result<String, ApiError> {
    keys.iter()
        .find_map(|key| params.get(*key))
        .ok_or_else(|| missing(&format!("[params {}]", keys.join(" or "))))
        .and_then(|value| non_blank(value))
}

fn required_value(
    body: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Result<String, ApiError> {
    let value = keys
        .iter()
        .find_map(|key| body.get(*key))
        .ok_or_else(|| missing(&format!("[body {}]", keys.join(" or "))))?;
    value
        .as_str()
        .ok_or_else(|| {
            malformed(
                &format!("[body {}]", keys.join(" or ")),
                Some(value.clone()),
            )
        })
        .and_then(non_blank)
}

fn non_blank(value: &str) -> Result<String, ApiError> {
    if value.trim().is_empty() {
        Err(malformed("string", Some(json!(value))))
    } else {
        Ok(value.to_owned())
    }
}

fn parse_uuid(value: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(value).map_err(|_| malformed("uuid", Some(json!(value))))
}

fn data(value: Value) -> Json<Value> {
    let mut body = serde_json::Map::new();
    body.insert("data".to_owned(), value);
    Json(Value::Object(body))
}

fn missing(input: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {input}"),
    )
    .with_hint(json!({"in": input}))
}

fn malformed(input: &str, value: Option<Value>) -> ApiError {
    let mut hint = serde_json::Map::new();
    hint.insert("in".to_owned(), json!(input));
    hint.insert("value".to_owned(), value.unwrap_or(Value::Null));
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: {input}"),
    )
    .with_hint(Value::Object(hint))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct TestBackend {
        uploads: Mutex<Vec<(RuntimeStorageContext, String, Vec<u8>)>>,
        consumed: Mutex<Vec<(Uuid, String, Vec<u8>)>>,
    }

    impl RuntimeStorageBackend for TestBackend {
        async fn upload(
            &self,
            context: RuntimeStorageContext,
            path: String,
            _content_type: Option<String>,
            _content_disposition: Option<String>,
            _content_length: Option<u64>,
            body: Body,
        ) -> Result<Value, ApiError> {
            let bytes = to_bytes(body, usize::MAX).await.unwrap().to_vec();
            self.uploads.lock().unwrap().push((context, path, bytes));
            Ok(json!({"id": Uuid::nil()}))
        }

        fn delete_file(
            &self,
            _context: RuntimeStorageContext,
            _path: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(json!({"id": null})))
        }

        fn create_upload_url(
            &self,
            _context: RuntimeStorageContext,
            _path: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(json!("https://api.test/storage/upload-id")))
        }

        async fn consume_upload_url(
            &self,
            upload_id: Uuid,
            content_type: String,
            _content_length: Option<u64>,
            body: Body,
        ) -> Result<Value, ApiError> {
            let bytes = to_bytes(body, usize::MAX).await.unwrap().to_vec();
            self.consumed
                .lock()
                .unwrap()
                .push((upload_id, content_type, bytes));
            Ok(json!({"id": Uuid::nil()}))
        }

        fn create_download_url(
            &self,
            _context: RuntimeStorageContext,
            _path: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(json!("https://objects.test/file")))
        }
    }

    #[tokio::test]
    async fn unicode_upload_query_metadata_overrides_legacy_headers() {
        let backend = Arc::new(TestBackend::default());
        let app_id = Uuid::new_v4();
        let response = runtime_storage_router(Arc::clone(&backend))
            .oneshot(Request::put(format!("/storage/upload?app_id={app_id}&path=%E8%B3%87%E6%96%99%2Fcaf%C3%A9%20%F0%9F%A6%80.txt&content-disposition=attachment%3B%20filename%3D%E8%B3%87%E6%96%99.txt"))
                .header("authorization", "Bearer secret")
                .header("path", "stale.txt")
                .header("content-disposition", "inline")
                .body(Body::from("raw")).unwrap())
            .await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let upload = backend.uploads.lock().unwrap()[0].clone();
        assert_eq!(upload.0.app_id, app_id);
        assert_eq!(upload.1, "資料/café 🦀.txt");
        assert_eq!(upload.2, b"raw");
    }

    #[tokio::test]
    async fn blank_upload_query_path_does_not_fall_back_to_header() {
        let backend = Arc::new(TestBackend::default());
        let app_id = Uuid::new_v4();
        let response = runtime_storage_router(backend)
            .oneshot(
                Request::put(format!("/storage/upload?app_id={app_id}&path=%20"))
                    .header("authorization", "Bearer secret")
                    .header("path", "valid.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn custom_image_route_is_removed() {
        let app_id = Uuid::new_v4();
        for token in ["allowed", "denied"] {
            let response = runtime_storage_router(Arc::new(TestBackend::default()))
                .oneshot(
                    Request::get(format!(
                        "/storage/image?app_id={app_id}&path=photos%2Fa%20b.png&w=120"
                    ))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn upload_and_consume_routes_preserve_raw_bytes_and_runtime_identity() {
        let backend = Arc::new(TestBackend::default());
        let app_id = Uuid::new_v4();
        let refresh_token = Uuid::new_v4();
        let response = runtime_storage_router(Arc::clone(&backend))
            .oneshot(
                Request::put("/storage/upload")
                    .header("app-id", app_id.to_string())
                    .header("path", "avatars/me.png")
                    .header("authorization", format!("Bearer {refresh_token}"))
                    .header("content-type", "image/png")
                    .header("content-length", "3")
                    .body(Body::from("png"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let upload = backend.uploads.lock().unwrap()[0].clone();
        let refresh_token = refresh_token.to_string();
        assert_eq!(upload.0.app_id, app_id);
        assert_eq!(
            upload.0.refresh_token.as_deref(),
            Some(refresh_token.as_str())
        );
        assert_eq!(upload.1, "avatars/me.png");
        assert_eq!(upload.2, b"png");

        let upload_id = Uuid::new_v4();
        let response = runtime_storage_router(Arc::clone(&backend))
            .oneshot(
                Request::put(format!("/storage/{upload_id}/consume-upload-url"))
                    .body(Body::from("raw"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let consumed = backend.consumed.lock().unwrap()[0].clone();
        assert_eq!(consumed.0, upload_id);
        assert_eq!(consumed.1, "application/octet-stream");
        assert_eq!(consumed.2, b"raw");
    }

    #[tokio::test]
    async fn legacy_and_file_routes_keep_data_envelopes_and_aliases() {
        let backend = Arc::new(TestBackend::default());
        let app_id = Uuid::new_v4();
        for (method, uri, body) in [
            (
                "POST",
                "/storage/signed-upload-url".to_owned(),
                Body::from(json!({"app_id": app_id, "filename": "a.txt"}).to_string()),
            ),
            (
                "DELETE",
                format!("/storage/files?app_id={app_id}&filename=a.txt"),
                Body::empty(),
            ),
            (
                "GET",
                format!("/storage/signed-download-url?app_id={app_id}&filename=a.txt"),
                Body::empty(),
            ),
        ] {
            let response = runtime_storage_router(Arc::clone(&backend))
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(body)
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let value: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                    .unwrap();
            assert!(value.get("data").is_some());
        }
    }

    #[tokio::test]
    async fn malformed_authorization_header_is_rejected_before_storage_dispatch() {
        let backend = Arc::new(TestBackend::default());
        let response = runtime_storage_router(backend.clone())
            .oneshot(
                Request::put("/storage/upload")
                    .header("app-id", Uuid::new_v4().to_string())
                    .header("path", "avatars/me.png")
                    .header("authorization", "Basic not-a-bearer-token")
                    .body(Body::from("payload"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let value: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(value["type"], "param-malformed");
        assert_eq!(
            value["message"],
            "Malformed parameter: [headers authorization]"
        );
        assert!(backend.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn content_type_sentinels_are_case_insensitive_and_trimmed() {
        for value in [" null ", "UNDEFINED"] {
            let mut headers = HeaderMap::new();
            headers.insert("content-type", value.parse().unwrap());
            assert_eq!(optional_content_type(&headers).unwrap(), None);
        }
        let mut headers = HeaderMap::new();
        headers.insert("content-type", " text/plain ".parse().unwrap());
        assert_eq!(
            optional_content_type(&headers).unwrap().as_deref(),
            Some("text/plain")
        );
    }

    #[test]
    fn bearer_credentials_allow_http_whitespace_separator() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer   token".parse().unwrap());
        assert_eq!(bearer(&headers).unwrap().as_deref(), Some("token"));
    }
}
