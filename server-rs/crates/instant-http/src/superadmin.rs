use std::future::Future;
use std::sync::Arc;

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::{AdminActor, ApiError, ApiErrorType};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SuperadminContext {
    pub bearer_token: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SuperadminOperation {
    ListApps {
        include: Vec<String>,
    },
    ListOrgs,
    ListOrgApps {
        org_id: Uuid,
        include: Vec<String>,
    },
    CreateApp {
        body: Value,
    },
    GetApp {
        app_id: Uuid,
    },
    UpdateApp {
        app_id: Uuid,
        body: Value,
    },
    SendTransfer {
        app_id: Uuid,
        body: Value,
    },
    RevokeTransfer {
        app_id: Uuid,
        body: Value,
    },
    GetSchema {
        app_id: Uuid,
    },
    PlanSchema {
        app_id: Uuid,
        body: Value,
    },
    ApplySchema {
        app_id: Uuid,
        body: Value,
    },
    GetPerms {
        app_id: Uuid,
    },
    PutPerms {
        app_id: Uuid,
        body: Value,
    },
    DeleteApp {
        app_id: Uuid,
    },
    Runtime {
        app_id: Uuid,
    },
    Explain {
        app_id: Uuid,
        actor: AdminActor,
        query: Map<String, Value>,
    },
}

pub trait SuperadminBackend: Send + Sync + 'static {
    fn execute(
        &self,
        context: SuperadminContext,
        operation: SuperadminOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

pub fn superadmin_router<B>(backend: Arc<B>) -> Router
where
    B: SuperadminBackend,
{
    let router = Router::new()
        .route(
            "/superadmin/apps",
            get(list_apps::<B>).post(create_app::<B>),
        )
        .route("/superadmin/orgs", get(list_orgs::<B>))
        .route("/superadmin/orgs/{org_id}/apps", get(list_org_apps::<B>))
        .route(
            "/superadmin/apps/{app_id}",
            get(get_app::<B>)
                .post(update_app::<B>)
                .delete(delete_app::<B>),
        )
        .route(
            "/superadmin/apps/{app_id}/transfers/send",
            post(send_transfer::<B>),
        )
        .route(
            "/superadmin/apps/{app_id}/transfers/revoke",
            post(revoke_transfer::<B>),
        )
        .route("/superadmin/apps/{app_id}/schema", get(get_schema::<B>))
        .route(
            "/superadmin/apps/{app_id}/schema/push/plan",
            post(plan_schema::<B>),
        )
        .route(
            "/superadmin/apps/{app_id}/schema/push/apply",
            post(apply_schema::<B>),
        )
        .route(
            "/superadmin/apps/{app_id}/perms",
            get(get_perms::<B>).post(put_perms::<B>),
        )
        .route("/superadmin/apps/{app_id}/runtime", get(get_runtime::<B>))
        .route("/superadmin/apps/{app_id}/explain", post(explain::<B>));
    crate::body_limit::limit_body(
        router,
        crate::body_limit::MANAGEMENT_BODY_LIMIT,
        "Superadmin request body exceeds the 1 MiB limit",
    )
    .with_state(backend)
}

#[derive(Deserialize)]
struct IncludeParams {
    include: Option<String>,
}

async fn list_apps<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    params: Result<Query<IncludeParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    dispatch(
        &*backend,
        &headers,
        SuperadminOperation::ListApps {
            include: includes(params)?,
        },
    )
    .await
}

async fn list_orgs<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    dispatch(&*backend, &headers, SuperadminOperation::ListOrgs).await
}

async fn list_org_apps<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    params: Result<Query<IncludeParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    dispatch(
        &*backend,
        &headers,
        SuperadminOperation::ListOrgApps {
            org_id: path_uuid(path)?,
            include: includes(params)?,
        },
    )
    .await
}

async fn create_app<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    let body = object_body(body)?;
    require_non_blank(&body, "title")?;
    dispatch(&*backend, &headers, SuperadminOperation::CreateApp { body }).await
}

async fn get_app<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    dispatch(
        &*backend,
        &headers,
        SuperadminOperation::GetApp {
            app_id: path_uuid(path)?,
        },
    )
    .await
}

async fn update_app<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    let body = object_body(body)?;
    require_non_blank(&body, "title")?;
    dispatch(
        &*backend,
        &headers,
        SuperadminOperation::UpdateApp {
            app_id: path_uuid(path)?,
            body,
        },
    )
    .await
}

async fn send_transfer<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    transfer(&*backend, &headers, path, body, true).await
}

async fn revoke_transfer<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    transfer(&*backend, &headers, path, body, false).await
}

async fn transfer<B>(
    backend: &B,
    headers: &HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
    send: bool,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    let body = object_body(body)?;
    require_email(&body, "dest_email")?;
    let app_id = path_uuid(path)?;
    let operation = if send {
        SuperadminOperation::SendTransfer { app_id, body }
    } else {
        SuperadminOperation::RevokeTransfer { app_id, body }
    };
    dispatch(backend, headers, operation).await
}

macro_rules! app_get_handler {
    ($name:ident, $variant:ident) => {
        async fn $name<B>(
            State(backend): State<Arc<B>>,
            headers: HeaderMap,
            path: Result<Path<Uuid>, PathRejection>,
        ) -> Result<Json<Value>, ApiError>
        where
            B: SuperadminBackend,
        {
            dispatch(
                &*backend,
                &headers,
                SuperadminOperation::$variant {
                    app_id: path_uuid(path)?,
                },
            )
            .await
        }
    };
}

app_get_handler!(get_schema, GetSchema);
app_get_handler!(get_perms, GetPerms);
app_get_handler!(delete_app, DeleteApp);

async fn plan_schema<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    schema_write(&*backend, &headers, path, body, false).await
}

async fn apply_schema<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    schema_write(&*backend, &headers, path, body, true).await
}

async fn schema_write<B>(
    backend: &B,
    headers: &HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
    apply: bool,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    let body = object_body(body)?;
    if body.get("schema").is_none() {
        return Err(missing("schema"));
    }
    let app_id = path_uuid(path)?;
    let operation = if apply {
        SuperadminOperation::ApplySchema { app_id, body }
    } else {
        SuperadminOperation::PlanSchema { app_id, body }
    };
    dispatch(backend, headers, operation).await
}

async fn put_perms<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    let body = object_body(body)?;
    if body.get("code").is_none() {
        return Err(missing("code"));
    }
    dispatch(
        &*backend,
        &headers,
        SuperadminOperation::PutPerms {
            app_id: path_uuid(path)?,
            body,
        },
    )
    .await
}

async fn get_runtime<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    dispatch(
        &*backend,
        &headers,
        SuperadminOperation::Runtime {
            app_id: path_uuid(path)?,
        },
    )
    .await
}

async fn explain<B>(
    State(backend): State<Arc<B>>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    let body = object_body(body)?;
    let query = body
        .get("query")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| missing("query"))?;
    let actor = explain_actor(&body)?;
    dispatch(
        &*backend,
        &headers,
        SuperadminOperation::Explain {
            app_id: path_uuid(path)?,
            actor,
            query,
        },
    )
    .await
}

fn explain_actor(body: &Value) -> Result<AdminActor, ApiError> {
    let actor = body
        .get("as")
        .and_then(Value::as_object)
        .ok_or_else(|| missing("as"))?;
    let provided = ["email", "token", "guest"]
        .iter()
        .filter(|key| actor.contains_key(**key))
        .count();
    if provided != 1 {
        return Err(malformed(
            "Expected exactly one of as.email, as.token or as.guest",
        ));
    }
    if let Some(email) = actor.get("email") {
        return email
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(|email| AdminActor::Email(email.to_owned()))
            .ok_or_else(|| malformed("Malformed as.email"));
    }
    if let Some(token) = actor.get("token") {
        return token
            .as_str()
            .ok_or_else(|| malformed("Malformed as.token"))
            .and_then(|token| {
                Uuid::parse_str(token)
                    .map(AdminActor::RefreshToken)
                    .map_err(|_| malformed("Malformed as.token"))
            });
    }
    if actor.get("guest").and_then(Value::as_bool) == Some(true) {
        return Ok(AdminActor::Guest);
    }
    Err(malformed("Expected one of as.email, as.token or as.guest"))
}

async fn dispatch<B>(
    backend: &B,
    headers: &HeaderMap,
    operation: SuperadminOperation,
) -> Result<Json<Value>, ApiError>
where
    B: SuperadminBackend,
{
    backend
        .execute(context(headers)?, operation)
        .await
        .map(Json)
}

fn context(headers: &HeaderMap) -> Result<SuperadminContext, ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| missing("Bearer authorization").unauthorized())?;
    let value = value
        .to_str()
        .map_err(|_| malformed("Malformed Authorization header").unauthorized())?;
    let bearer_token = value
        .split_once(' ')
        .and_then(|(scheme, token)| scheme.eq_ignore_ascii_case("Bearer").then_some(token))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| malformed("Malformed Bearer authorization").unauthorized())?;
    Ok(SuperadminContext { bearer_token })
}

fn includes(params: Result<Query<IncludeParams>, QueryRejection>) -> Result<Vec<String>, ApiError> {
    let Query(params) = params.map_err(|error| malformed(error.body_text()))?;
    Ok(params
        .include
        .map(|include| {
            include
                .split(',')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default())
}

fn path_uuid(path: Result<Path<Uuid>, PathRejection>) -> Result<Uuid, ApiError> {
    path.map(|Path(id)| id)
        .map_err(|error| malformed(error.body_text()))
}

fn object_body(body: Result<Json<Value>, JsonRejection>) -> Result<Value, ApiError> {
    let Json(body) = body.map_err(|error| malformed(error.body_text()))?;
    if body.is_object() {
        Ok(body)
    } else {
        Err(malformed("Expected a JSON object"))
    }
}

fn require_non_blank(body: &Value, key: &'static str) -> Result<(), ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|_| ())
        .ok_or_else(|| missing(key))
}

fn require_email(body: &Value, key: &'static str) -> Result<(), ApiError> {
    require_non_blank(body, key)?;
    if body[key].as_str().is_some_and(|email| email.contains('@')) {
        Ok(())
    } else {
        Err(malformed("Malformed email"))
    }
}

fn missing(name: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing {name} parameter"),
    )
}

fn malformed(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorType::ParamMalformed, message)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn malformed_present_authorization_is_not_reported_as_missing() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Basic token".parse().unwrap());
        let error = context(&headers).unwrap_err();
        assert_eq!(error.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(error.body().kind, ApiErrorType::ParamMalformed);
    }

    #[test]
    fn bearer_credentials_allow_http_whitespace_separator() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer   token".parse().unwrap());
        assert_eq!(context(&headers).unwrap().bearer_token, "token");
    }

    #[derive(Default)]
    struct Backend {
        operations: Mutex<Vec<SuperadminOperation>>,
        contexts: Mutex<Vec<SuperadminContext>>,
    }

    impl SuperadminBackend for Backend {
        fn execute(
            &self,
            context: SuperadminContext,
            operation: SuperadminOperation,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            self.contexts.lock().unwrap().push(context);
            self.operations.lock().unwrap().push(operation);
            std::future::ready(Ok(serde_json::json!({"ok": true})))
        }
    }

    #[tokio::test]
    async fn list_create_schema_and_delete_routes_preserve_typed_operations() {
        let backend = Arc::new(Backend::default());
        let router = superadmin_router(Arc::clone(&backend));
        for request in [
            Request::get("/superadmin/apps?include=schema,perms")
                .header(header::AUTHORIZATION, "Bearer per_secret")
                .body(Body::empty())
                .unwrap(),
            Request::post("/superadmin/apps")
                .header(header::AUTHORIZATION, "Bearer per_secret")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"title":"Demo"}"#))
                .unwrap(),
            Request::post("/superadmin/apps/00000000-0000-0000-0000-000000000001/schema/push/plan")
                .header(header::AUTHORIZATION, "Bearer pat_secret")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"schema":{},"check_types":true}"#))
                .unwrap(),
            Request::delete("/superadmin/apps/00000000-0000-0000-0000-000000000001")
                .header(header::AUTHORIZATION, "Bearer per_secret")
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                r#"{"ok":true}"#
            );
        }
        assert!(
            matches!(backend.operations.lock().unwrap()[0], SuperadminOperation::ListApps { ref include } if include == &["schema", "perms"])
        );
        assert!(matches!(
            backend.operations.lock().unwrap()[3],
            SuperadminOperation::DeleteApp { .. }
        ));
        assert_eq!(
            backend.contexts.lock().unwrap()[0].bearer_token,
            "per_secret"
        );
    }

    #[tokio::test]
    async fn runtime_and_explain_routes_preserve_typed_operations() {
        let backend = Arc::new(Backend::default());
        let router = superadmin_router(Arc::clone(&backend));
        let app_id = "00000000-0000-0000-0000-000000000001";
        for request in [
            Request::get(format!("/superadmin/apps/{app_id}/runtime"))
                .header(header::AUTHORIZATION, "Bearer per_secret")
                .body(Body::empty())
                .unwrap(),
            Request::post(format!("/superadmin/apps/{app_id}/explain"))
                .header(header::AUTHORIZATION, "Bearer per_secret")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"query":{"goals":{}},"as":{"email":"person@example.com"}}"#,
                ))
                .unwrap(),
        ] {
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let operations = backend.operations.lock().unwrap();
        assert!(matches!(operations[0], SuperadminOperation::Runtime { .. }));
        assert!(matches!(
            &operations[1],
            SuperadminOperation::Explain {
                actor: AdminActor::Email(email),
                query,
                ..
            } if email == "person@example.com" && query.contains_key("goals")
        ));
    }

    #[tokio::test]
    async fn explain_rejects_malformed_actor_and_missing_auth_is_unauthorized() {
        let backend = Arc::new(Backend::default());
        let router = superadmin_router(Arc::clone(&backend));
        let app_id = "00000000-0000-0000-0000-000000000001";
        for body in [
            r#"{"query":{"goals":{}}}"#,
            r#"{"query":{"goals":{}},"as":{"token":"not-a-uuid"}}"#,
            r#"{"query":{"goals":{}},"as":{}}"#,
            r#"{"query":{"goals":{}},"as":{"guest":true,"email":"person@example.com"}}"#,
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::post(format!("/superadmin/apps/{app_id}/explain"))
                        .header(header::AUTHORIZATION, "Bearer per_secret")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        }
        for request in [
            Request::get(format!("/superadmin/apps/{app_id}/runtime"))
                .body(Body::empty())
                .unwrap(),
            Request::post(format!("/superadmin/apps/{app_id}/explain"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"query":{"goals":{}},"as":{"guest":true}}"#))
                .unwrap(),
        ] {
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        assert!(backend.operations.lock().unwrap().is_empty());
    }
}
