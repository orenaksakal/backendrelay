use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::Value;

use crate::admin::{json_object, query_params, request_context};
use crate::{AdminRequestContext, ApiError, ApiErrorType};

pub trait AdminStorageBackend: Send + Sync + 'static {
    fn upload(
        &self,
        context: AdminRequestContext,
        path: String,
        content_type: Option<String>,
        content_disposition: Option<String>,
        content_length: Option<u64>,
        body: Body,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn delete_file(
        &self,
        context: AdminRequestContext,
        path: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn delete_files(
        &self,
        context: AdminRequestContext,
        paths: Vec<String>,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn list_files(
        &self,
        context: AdminRequestContext,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn create_upload_url(
        &self,
        context: AdminRequestContext,
        path: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;

    fn create_download_url(
        &self,
        context: AdminRequestContext,
        path: String,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

pub fn admin_storage_router<B>(backend: Arc<B>) -> Router
where
    B: AdminStorageBackend,
{
    let upload = Router::new().route("/admin/storage/upload", put(upload::<B>));
    let limited = crate::body_limit::limit_body(
        Router::new()
            .route(
                "/admin/storage/signed-upload-url",
                post(create_upload_url::<B>),
            )
            .route(
                "/admin/storage/signed-download-url",
                get(create_download_url::<B>),
            )
            .route(
                "/admin/storage/files",
                get(list_files::<B>).delete(delete_file::<B>),
            )
            .route("/admin/storage/files/delete", post(delete_files::<B>)),
        crate::body_limit::MANAGEMENT_BODY_LIMIT,
        "Admin request body exceeds the 1 MiB limit",
    );
    upload.merge(limited).with_state(backend)
}

async fn upload<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    body: Body,
) -> Result<Json<Value>, ApiError>
where
    B: AdminStorageBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let path = match params.get("path") {
        Some(value) => non_blank("[params path]", value)?,
        None => required_non_blank_header(&headers, "path")?,
    };
    let content_type = optional_content_type(&headers)?;
    let content_disposition = match params.get("content-disposition") {
        Some(value) => Some(non_blank("[params content-disposition]", value)?),
        None => optional_non_blank_header(&headers, "content-disposition")?,
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
        .map(|data| Json(serde_json::json!({"data": data})))
}

async fn delete_file<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminStorageBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let path = required_non_blank_param(&params, "filename")?;
    backend
        .delete_file(context, path)
        .await
        .map(|data| Json(serde_json::json!({"data": data})))
}

async fn delete_files<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminStorageBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let body = json_object(payload)?;
    let value = body
        .get("filenames")
        .ok_or_else(|| missing("[body filenames]"))?;
    let paths = value
        .as_array()
        .ok_or_else(|| malformed("[body filenames]", value))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| malformed("[body filenames]", value))
        })
        .collect::<Result<Vec<_>, _>>()?;
    backend
        .delete_files(context, paths)
        .await
        .map(|data| Json(serde_json::json!({"data": data})))
}

async fn list_files<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminStorageBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    backend
        .list_files(context)
        .await
        .map(|data| Json(serde_json::json!({"data": data})))
}

async fn create_upload_url<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminStorageBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let body = json_object(payload)?;
    let path = required_non_blank_value(&body, "filename")?;
    backend
        .create_upload_url(context, path)
        .await
        .map(|data| Json(serde_json::json!({"data": data})))
}

async fn create_download_url<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminStorageBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let path = required_non_blank_param(&params, "filename")?;
    backend
        .create_download_url(context, path)
        .await
        .map(|data| Json(serde_json::json!({"data": data})))
}

fn required_non_blank_header(headers: &HeaderMap, key: &'static str) -> Result<String, ApiError> {
    optional_non_blank_header(headers, key)?.ok_or_else(|| missing(&format!("[headers {key}]")))
}

fn optional_non_blank_header(
    headers: &HeaderMap,
    key: &'static str,
) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get(key) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| malformed(&format!("[headers {key}]"), &Value::Null))?;
    if value.trim().is_empty() {
        return Err(malformed(
            &format!("[headers {key}]"),
            &Value::String(value.to_owned()),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn optional_content_type(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    Ok(
        optional_non_blank_header(headers, "content-type")?.and_then(|value| {
            let normalized = value.trim();
            (!matches!(
                normalized.to_ascii_lowercase().as_str(),
                "null" | "undefined"
            ))
            .then(|| normalized.to_owned())
        }),
    )
}

fn optional_content_length(headers: &HeaderMap) -> Result<Option<u64>, ApiError> {
    let Some(value) = headers.get("content-length") else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .map_err(|_| malformed("[headers content-length]", &Value::Null))?;
    text.parse()
        .map(Some)
        .map_err(|_| malformed("[headers content-length]", &Value::String(text.to_owned())))
}

fn required_non_blank_param(
    params: &HashMap<String, String>,
    key: &str,
) -> Result<String, ApiError> {
    let value = params
        .get(key)
        .ok_or_else(|| missing(&format!("[params {key}]")))?;
    non_blank(&format!("[params {key}]"), value)
}

fn required_non_blank_value(
    body: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String, ApiError> {
    let value = body
        .get(key)
        .ok_or_else(|| missing(&format!("[body {key}]")))?;
    let value = value
        .as_str()
        .ok_or_else(|| malformed(&format!("[body {key}]"), value))?;
    non_blank(&format!("[body {key}]"), value)
}

fn non_blank(path: &str, value: &str) -> Result<String, ApiError> {
    if value.trim().is_empty() {
        Err(malformed(path, &Value::String(value.to_owned())))
    } else {
        Ok(value.to_owned())
    }
}

fn missing(path: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {path}"),
    )
}

fn malformed(path: &str, value: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: {path}"),
    )
    .with_hint(serde_json::json!({"in": path, "original-input": value}))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;

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

    type UploadCapture = (String, Option<String>, Option<String>, Option<u64>, Vec<u8>);

    #[derive(Default)]
    struct FakeBackend {
        upload: Mutex<Option<UploadCapture>>,
        deleted: Mutex<Vec<String>>,
    }

    impl AdminStorageBackend for FakeBackend {
        async fn upload(
            &self,
            _context: AdminRequestContext,
            path: String,
            content_type: Option<String>,
            content_disposition: Option<String>,
            content_length: Option<u64>,
            body: Body,
        ) -> Result<Value, ApiError> {
            let bytes = to_bytes(body, usize::MAX).await.unwrap().to_vec();
            *self.upload.lock().unwrap() = Some((
                path,
                content_type,
                content_disposition,
                content_length,
                bytes,
            ));
            Ok(serde_json::json!({"id": Uuid::nil()}))
        }

        fn delete_file(
            &self,
            _context: AdminRequestContext,
            path: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            self.deleted.lock().unwrap().push(path);
            std::future::ready(Ok(serde_json::json!({"id": null})))
        }

        fn delete_files(
            &self,
            _context: AdminRequestContext,
            paths: Vec<String>,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            self.deleted.lock().unwrap().extend(paths);
            std::future::ready(Ok(serde_json::json!({"ids": []})))
        }

        fn list_files(
            &self,
            _context: AdminRequestContext,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(serde_json::json!([])))
        }

        fn create_upload_url(
            &self,
            _context: AdminRequestContext,
            path: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(Value::String(format!(
                "https://instant.test/storage/{path}"
            ))))
        }

        fn create_download_url(
            &self,
            _context: AdminRequestContext,
            path: String,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            std::future::ready(Ok(Value::String(format!("https://objects.test/{path}"))))
        }
    }

    fn request(method: &str, uri: &str, app_id: Uuid, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer secret")
            .header("app-id", app_id.to_string())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
    }

    #[tokio::test]
    async fn unicode_upload_query_metadata_overrides_legacy_headers() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = admin_storage_router(Arc::clone(&backend))
            .oneshot(Request::put(format!("/admin/storage/upload?app_id={app_id}&path=%E8%B3%87%E6%96%99%2Fcaf%C3%A9%20%F0%9F%A6%80.txt&content-disposition=attachment%3B%20filename%3D%E8%B3%87%E6%96%99.txt"))
                .header("authorization", "Bearer secret")
                .header("path", "stale.txt")
                .header("content-disposition", "inline")
                .body(Body::from("raw")).unwrap())
            .await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let upload = backend.upload.lock().unwrap().take().unwrap();
        assert_eq!(upload.0, "資料/café 🦀.txt");
        assert_eq!(upload.2.as_deref(), Some("attachment; filename=資料.txt"));
        assert_eq!(upload.4, b"raw");
    }

    #[tokio::test]
    async fn blank_upload_query_path_does_not_fall_back_to_header() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = admin_storage_router(backend)
            .oneshot(
                Request::put(format!("/admin/storage/upload?app_id={app_id}&path=%20"))
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
    async fn raw_upload_preserves_body_and_storage_metadata_headers() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = admin_storage_router(backend.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/admin/storage/upload?app_id={app_id}"))
                    .header("app-id", app_id.to_string())
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .header("path", "photos/a.png")
                    .header("content-type", "image/png")
                    .header("content-disposition", "inline")
                    .header("content-length", "3")
                    .body(Body::from("png"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let upload = backend.upload.lock().unwrap().take().unwrap();
        assert_eq!(upload.0, "photos/a.png");
        assert_eq!(upload.1.as_deref(), Some("image/png"));
        assert_eq!(upload.2.as_deref(), Some("inline"));
        assert_eq!(upload.3, Some(3));
        assert_eq!(upload.4, b"png");
    }

    #[tokio::test]
    async fn delete_bulk_and_legacy_url_routes_match_sdk_envelopes() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let delete_response = admin_storage_router(backend.clone())
            .oneshot(request(
                "POST",
                &format!("/admin/storage/files/delete?app_id={app_id}"),
                app_id,
                Body::from(r#"{"filenames":["a","b"]}"#),
            ))
            .await
            .unwrap();
        assert_eq!(delete_response.status(), StatusCode::OK);
        assert_eq!(&*backend.deleted.lock().unwrap(), &["a", "b"]);

        let upload_url = admin_storage_router(backend)
            .oneshot(request(
                "POST",
                &format!("/admin/storage/signed-upload-url?app_id={app_id}"),
                app_id,
                Body::from(r#"{"filename":"legacy.png"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(upload_url.status(), StatusCode::OK);
        let body = to_bytes(upload_url.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            body,
            r#"{"data":"https://instant.test/storage/legacy.png"}"#
        );
    }
}
