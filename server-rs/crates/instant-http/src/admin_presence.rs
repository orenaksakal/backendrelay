use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::admin::{query_params, request_context};
use crate::{AdminRequestContext, ApiError, ApiErrorType};

/// Response returned by the official admin room-presence API.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AdminPresenceResponse {
    pub sessions: Option<Map<String, Value>>,
}

pub trait AdminPresenceBackend: Send + Sync + 'static {
    fn presence(
        &self,
        context: AdminRequestContext,
        room_type: String,
        room_id: String,
    ) -> impl Future<Output = Result<AdminPresenceResponse, ApiError>> + Send;
}

pub fn admin_presence_router<B>(backend: Arc<B>) -> Router
where
    B: AdminPresenceBackend,
{
    crate::body_limit::limit_body(
        Router::new().route("/admin/rooms/presence", get(presence::<B>)),
        crate::body_limit::MANAGEMENT_BODY_LIMIT,
        "Admin request body exceeds the 1 MiB limit",
    )
    .with_state(backend)
}

async fn presence<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<AdminPresenceResponse>, ApiError>
where
    B: AdminPresenceBackend,
{
    let params = query_params(params)?;
    let context = request_context(&headers, &params)?;
    let room_type = required_non_blank(&params, "room-type")?;
    let room_id = required_non_blank(&params, "room-id")?;
    backend
        .presence(context, room_type, room_id)
        .await
        .map(Json)
}

fn required_non_blank(params: &HashMap<String, String>, key: &str) -> Result<String, ApiError> {
    let value = params.get(key).ok_or_else(|| {
        ApiError::new(
            ApiErrorType::ParamMissing,
            format!("Missing parameter: [params {key}]"),
        )
    })?;
    if value.trim().is_empty() {
        return Err(ApiError::new(
            ApiErrorType::ParamMalformed,
            format!("Malformed parameter: [params {key}]"),
        )
        .with_hint(serde_json::json!({
            "in": format!("[params {key}]"),
            "original-input": value,
        })));
    }
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;

    #[derive(Default)]
    struct FakeBackend {
        request: Mutex<Option<(AdminRequestContext, String, String)>>,
    }

    impl AdminPresenceBackend for FakeBackend {
        fn presence(
            &self,
            context: AdminRequestContext,
            room_type: String,
            room_id: String,
        ) -> impl Future<Output = Result<AdminPresenceResponse, ApiError>> + Send {
            *self.request.lock().unwrap() = Some((context, room_type, room_id));
            std::future::ready(Ok(AdminPresenceResponse {
                sessions: Some(Map::from_iter([(
                    "peer".to_owned(),
                    serde_json::json!({"peer-id": "peer", "user": null, "data": {}}),
                )])),
            }))
        }
    }

    #[tokio::test]
    async fn presence_route_preserves_hyphenated_query_and_response_contract() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = admin_presence_router(backend.clone())
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/rooms/presence?app_id={app_id}&room-type=chat&room-id=lobby"
                    ))
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            r#"{"sessions":{"peer":{"data":{},"peer-id":"peer","user":null}}}"#
        );
        let (context, room_type, room_id) = backend.request.lock().unwrap().take().unwrap();
        assert_eq!(context.app_id, app_id);
        assert_eq!(room_type, "chat");
        assert_eq!(room_id, "lobby");
    }

    #[tokio::test]
    async fn presence_requires_both_non_blank_room_parameters() {
        let app_id = Uuid::new_v4();
        let response = admin_presence_router(Arc::new(FakeBackend::default()))
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/rooms/presence?app_id={app_id}&room-type=chat&room-id="
                    ))
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
