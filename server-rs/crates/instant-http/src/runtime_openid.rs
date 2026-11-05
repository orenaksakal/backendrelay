use std::sync::Arc;

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use uuid::Uuid;

use crate::{ApiError, ApiErrorType};

#[derive(Clone, Debug)]
struct OpenIdState {
    server_origin: Arc<str>,
}

#[derive(Debug, Serialize)]
struct OpenIdConfiguration {
    authorization_endpoint: String,
    token_endpoint: String,
}

pub fn runtime_openid_router(server_origin: impl Into<String>) -> Router {
    let origin = server_origin.into().trim_end_matches('/').to_owned();
    Router::new()
        .route(
            "/runtime/{app_id}/.well-known/openid-configuration",
            get(openid_configuration),
        )
        .with_state(OpenIdState {
            server_origin: Arc::from(origin),
        })
}

async fn openid_configuration(
    State(state): State<OpenIdState>,
    app_id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<OpenIdConfiguration>, ApiError> {
    let app_id = app_id.map(|Path(app_id)| app_id).map_err(|error| {
        ApiError::new(
            ApiErrorType::ParamMalformed,
            "Malformed parameter: [params app_id]",
        )
        .with_hint(serde_json::json!({
            "in": "[params app_id]",
            "value": error.to_string(),
        }))
    })?;
    Ok(Json(OpenIdConfiguration {
        authorization_endpoint: format!("{}/runtime/{app_id}/oauth/start", state.server_origin),
        token_endpoint: format!("{}/runtime/{app_id}/oauth/token", state.server_origin),
    }))
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn discovery_uses_app_scoped_runtime_endpoints_and_trimmed_origin() {
        let app_id = Uuid::new_v4();
        let response = runtime_openid_router("https://api.test/")
            .oneshot(
                Request::get(format!(
                    "/runtime/{app_id}/.well-known/openid-configuration"
                ))
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body,
            json!({
                "authorization_endpoint": format!("https://api.test/runtime/{app_id}/oauth/start"),
                "token_endpoint": format!("https://api.test/runtime/{app_id}/oauth/token"),
            })
        );
    }
}
