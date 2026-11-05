use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::admin::{clojure_truthy, json_object, query_params, request_context, required_object};
use crate::{AdminRequestContext, ApiError, ApiErrorType};

/// Receiver and synchronous cleanup hook for one server-sent-event session.
pub struct AdminSseSession {
    receiver: mpsc::Receiver<Value>,
    cleanup: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl AdminSseSession {
    #[must_use]
    pub fn new(
        receiver: mpsc::Receiver<Value>,
        cleanup: impl FnOnce() + Send + Sync + 'static,
    ) -> Self {
        Self {
            receiver,
            cleanup: Some(Box::new(cleanup)),
        }
    }

    /// Receives the next protocol value. HTTP adapters normally consume this as SSE.
    pub async fn recv(&mut self) -> Option<Value> {
        self.receiver.recv().await
    }
}

impl Drop for AdminSseSession {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdminSsePushRequest {
    pub app_id: Uuid,
    pub machine_id: Uuid,
    pub session_id: Uuid,
    pub sse_token: Uuid,
    pub messages: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdminSseOpenRequest {
    pub query: Option<serde_json::Map<String, Value>>,
    pub inference: bool,
    pub versions: Option<Value>,
}

pub trait AdminRealtimeBackend: Send + Sync + 'static {
    fn open(
        &self,
        context: AdminRequestContext,
        request: AdminSseOpenRequest,
    ) -> impl Future<Output = Result<AdminSseSession, ApiError>> + Send;

    fn push(
        &self,
        request: AdminSsePushRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

pub fn admin_realtime_router<B>(backend: Arc<B>) -> Router
where
    B: AdminRealtimeBackend,
{
    crate::body_limit::limit_body(
        Router::new()
            .route("/admin/sse", post(open::<B>))
            .route("/admin/sse/push", post(push::<B>))
            .route("/admin/subscribe-query", post(subscribe_query::<B>)),
        crate::body_limit::MANAGEMENT_BODY_LIMIT,
        "Admin request body exceeds the 1 MiB limit",
    )
    .with_state(backend)
}

async fn open<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError>
where
    B: AdminRealtimeBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let body = json_object(payload)?;
    let request = AdminSseOpenRequest {
        query: None,
        inference: body.get("inference?").is_some_and(clojure_truthy),
        versions: body.get("versions").cloned(),
    };
    let session = backend.open(context, request).await?;
    Ok(sse_response(session))
}

async fn subscribe_query<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError>
where
    B: AdminRealtimeBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let body = json_object(payload)?;
    let request = AdminSseOpenRequest {
        query: Some(required_object(&body, "query")?.clone()),
        inference: body.get("inference?").is_some_and(clojure_truthy),
        versions: body.get("versions").cloned(),
    };
    let session = backend.open(context, request).await?;
    Ok(sse_response(session))
}

async fn push<B>(
    State(backend): State<Arc<B>>,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: AdminRealtimeBackend,
{
    let params = query_params(params)?;
    let body = json_object(payload)?;
    let request = AdminSsePushRequest {
        app_id: parse_uuid_param(&params, "app_id")?,
        machine_id: parse_uuid_value(&body, "machine_id")?,
        session_id: parse_uuid_value(&body, "session_id")?,
        sse_token: parse_uuid_value(&body, "sse_token")?,
        messages: body
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| malformed("[body messages]", body.get("messages")))?,
    };
    backend.push(request).await?;
    Ok(Json(serde_json::json!({})))
}

fn sse_response(
    session: AdminSseSession,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let events = stream::unfold((session, true), |(mut session, first)| async move {
        let value = session.recv().await?;
        let data = serde_json::to_string(&value).expect("JSON values are serializable");
        let mut event = Event::default().data(data);
        if first {
            event = event.retry(Duration::from_millis(500));
        }
        Some((Ok(event), (session, false)))
    });
    Sse::new(events).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

fn parse_uuid_param(params: &HashMap<String, String>, key: &str) -> Result<Uuid, ApiError> {
    let value = params
        .get(key)
        .ok_or_else(|| missing(&format!("[params {key}]")))?;
    Uuid::parse_str(value).map_err(|_| {
        malformed(
            &format!("[params {key}]"),
            Some(&Value::String(value.clone())),
        )
    })
}

fn parse_uuid_value(body: &serde_json::Map<String, Value>, key: &str) -> Result<Uuid, ApiError> {
    let value = body
        .get(key)
        .ok_or_else(|| missing(&format!("[body {key}]")))?;
    value
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| malformed(&format!("[body {key}]"), Some(value)))
}

fn missing(input: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {input}"),
    )
    .with_hint(serde_json::json!({"in": input}))
}

fn malformed(input: &str, value: Option<&Value>) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: {input}"),
    )
    .with_hint(serde_json::json!({"in": input, "value": value}))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct TestBackend {
        opened: Mutex<Vec<(AdminRequestContext, AdminSseOpenRequest)>>,
        pushed: Mutex<Vec<AdminSsePushRequest>>,
    }

    impl AdminRealtimeBackend for TestBackend {
        async fn open(
            &self,
            context: AdminRequestContext,
            request: AdminSseOpenRequest,
        ) -> Result<AdminSseSession, ApiError> {
            self.opened.lock().unwrap().push((context, request));
            let (sender, receiver) = mpsc::channel(1);
            sender
                .send(json!({"op": "sse-init", "session-id": Uuid::nil()}))
                .await
                .unwrap();
            drop(sender);
            Ok(AdminSseSession::new(receiver, || {}))
        }

        fn push(
            &self,
            request: AdminSsePushRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.pushed.lock().unwrap().push(request);
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn subscribe_query_preserves_post_sse_and_query_contract() {
        let backend = Arc::new(TestBackend::default());
        let app_id = Uuid::new_v4();
        let response = admin_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::post(format!("/admin/subscribe-query?app_id={app_id}"))
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "query": {"goals": {}},
                            "inference?": true,
                            "versions": {"@instantdb/admin": "1.0.0"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains("data: {\"op\":\"sse-init\"")
        );
        let opened = backend.opened.lock().unwrap();
        assert_eq!(opened[0].0.app_id, app_id);
        assert_eq!(opened[0].0.actor, crate::AdminActor::Admin);
        assert!(opened[0].1.inference);
        assert!(opened[0].1.query.as_ref().unwrap().contains_key("goals"));
    }

    #[tokio::test]
    async fn push_accepts_token_authenticated_message_batches_without_admin_headers() {
        let backend = Arc::new(TestBackend::default());
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let response = admin_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::post(format!("/admin/sse/push?app_id={app_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "machine_id": machine_id,
                            "session_id": session_id,
                            "sse_token": token,
                            "messages": [{"op": "remove-query", "q": {"goals": {}}}]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.pushed.lock().unwrap()[0].app_id, app_id);
        assert_eq!(backend.pushed.lock().unwrap()[0].machine_id, machine_id);
    }
}
