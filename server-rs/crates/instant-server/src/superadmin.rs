use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use instant_http::{
    AdminActor, ApiError, SuperadminBackend, SuperadminContext, SuperadminOperation,
};
use serde_json::{Map, Value};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuperadminScope {
    AppsRead,
    AppsWrite,
    AppsTransfer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppRole {
    Collaborator,
    Admin,
    Owner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SuperadminAccess {
    pub scope: SuperadminScope,
    pub app: Option<(Uuid, AppRole)>,
    pub org_id: Option<Uuid>,
    pub allow_app_admin: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SuperadminPrincipal {
    pub user_id: Option<Uuid>,
    pub admin_app_id: Option<Uuid>,
}

/// Resolves personal/platform/admin/dashboard tokens and enforces scope and membership.
pub trait SuperadminAuthorizer: Send + Sync + 'static {
    fn authorize(
        &self,
        bearer_token: String,
        access: SuperadminAccess,
    ) -> impl Future<Output = Result<SuperadminPrincipal, ApiError>> + Send;
}

/// Durable app/org/schema/rules/invite operations after authorization succeeds.
pub trait SuperadminStore: Send + Sync + 'static {
    fn execute(
        &self,
        principal: SuperadminPrincipal,
        context: SuperadminContext,
        operation: SuperadminOperation,
    ) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

pub struct SuperadminService<A, S> {
    authorizer: A,
    store: S,
    debug: Option<Arc<dyn SuperadminDebug>>,
}

impl<A, S> SuperadminService<A, S> {
    #[must_use]
    pub const fn new(authorizer: A, store: S) -> Self {
        Self {
            authorizer,
            store,
            debug: None,
        }
    }

    #[must_use]
    pub fn with_debug(mut self, debug: Arc<dyn SuperadminDebug>) -> Self {
        self.debug = Some(debug);
        self
    }
}

/// Read-only operator diagnostics that are not part of the durable store.
pub trait SuperadminDebug: Send + Sync + 'static {
    /// Returns operator diagnostics for one app.
    ///
    /// # Errors
    ///
    /// Returns an API error when the app's diagnostics cannot be read.
    fn runtime(&self, app_id: Uuid) -> Result<Value, ApiError>;

    fn explain(
        &self,
        app_id: Uuid,
        actor: AdminActor,
        query: Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + '_>>;
}

impl<A, S> SuperadminBackend for SuperadminService<A, S>
where
    A: SuperadminAuthorizer,
    S: SuperadminStore,
{
    async fn execute(
        &self,
        context: SuperadminContext,
        operation: SuperadminOperation,
    ) -> Result<Value, ApiError> {
        let access = required_access(&operation);
        let principal = self
            .authorizer
            .authorize(context.bearer_token.clone(), access)
            .await?;
        match operation {
            SuperadminOperation::Runtime { app_id } => self.debug()?.runtime(app_id),
            SuperadminOperation::Explain {
                app_id,
                actor,
                query,
            } => self.debug()?.explain(app_id, actor, query).await,
            operation => self.store.execute(principal, context, operation).await,
        }
    }
}

impl<A, S> SuperadminService<A, S> {
    fn debug(&self) -> Result<&dyn SuperadminDebug, ApiError> {
        self.debug.as_deref().ok_or_else(|| {
            ApiError::new(
                instant_http::ApiErrorType::Unknown,
                "Operator diagnostics are not configured",
            )
        })
    }
}

#[must_use]
pub fn required_access(operation: &SuperadminOperation) -> SuperadminAccess {
    use AppRole::{Admin, Collaborator, Owner};
    use SuperadminOperation::{
        ApplySchema, CreateApp, DeleteApp, Explain, GetApp, GetPerms, GetSchema, ListApps,
        ListOrgApps, ListOrgs, PlanSchema, PutPerms, RevokeTransfer, Runtime, SendTransfer,
        UpdateApp,
    };
    use SuperadminScope::{AppsRead, AppsTransfer, AppsWrite};

    match operation {
        ListApps { .. } | ListOrgs => access(AppsRead, None, None, false),
        ListOrgApps { org_id, .. } => access(AppsRead, None, Some(*org_id), false),
        CreateApp { .. } => access(AppsWrite, None, None, false),
        GetApp { app_id }
        | GetSchema { app_id }
        | PlanSchema { app_id, .. }
        | GetPerms { app_id }
        | Runtime { app_id }
        | Explain { app_id, .. } => access(AppsRead, Some((*app_id, Collaborator)), None, true),
        UpdateApp { app_id, .. } | DeleteApp { app_id } => {
            access(AppsWrite, Some((*app_id, Admin)), None, false)
        }
        ApplySchema { app_id, .. } | PutPerms { app_id, .. } => {
            access(AppsWrite, Some((*app_id, Collaborator)), None, true)
        }
        SendTransfer { app_id, .. } | RevokeTransfer { app_id, .. } => {
            access(AppsTransfer, Some((*app_id, Owner)), None, false)
        }
    }
}

const fn access(
    scope: SuperadminScope,
    app: Option<(Uuid, AppRole)>,
    org_id: Option<Uuid>,
    allow_app_admin: bool,
) -> SuperadminAccess {
    SuperadminAccess {
        scope,
        app,
        org_id,
        allow_app_admin,
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Authorizer(Mutex<Vec<SuperadminAccess>>);

    impl SuperadminAuthorizer for Authorizer {
        fn authorize(
            &self,
            _bearer_token: String,
            access: SuperadminAccess,
        ) -> impl Future<Output = Result<SuperadminPrincipal, ApiError>> + Send {
            self.0.lock().unwrap().push(access);
            ready(Ok(SuperadminPrincipal {
                user_id: Some(Uuid::nil()),
                admin_app_id: None,
            }))
        }
    }

    struct Store;

    impl SuperadminStore for Store {
        fn execute(
            &self,
            _principal: SuperadminPrincipal,
            _context: SuperadminContext,
            _operation: SuperadminOperation,
        ) -> impl Future<Output = Result<Value, ApiError>> + Send {
            ready(Ok(serde_json::json!({"ok": true})))
        }
    }

    #[tokio::test]
    async fn maps_every_sensitive_operation_to_the_reference_scope_and_role() {
        let app_id = Uuid::new_v4();
        let service = SuperadminService::new(Authorizer::default(), Store);
        let operations = [
            SuperadminOperation::GetSchema { app_id },
            SuperadminOperation::ApplySchema {
                app_id,
                body: serde_json::json!({}),
            },
            SuperadminOperation::UpdateApp {
                app_id,
                body: serde_json::json!({}),
            },
            SuperadminOperation::SendTransfer {
                app_id,
                body: serde_json::json!({}),
            },
        ];
        for operation in operations {
            service
                .execute(
                    SuperadminContext {
                        bearer_token: "per_secret".to_owned(),
                    },
                    operation,
                )
                .await
                .unwrap();
        }
        let accesses = service.authorizer.0.lock().unwrap();
        assert_eq!(accesses[0].app, Some((app_id, AppRole::Collaborator)));
        assert_eq!(accesses[0].scope, SuperadminScope::AppsRead);
        assert!(accesses[0].allow_app_admin);
        assert_eq!(accesses[1].scope, SuperadminScope::AppsWrite);
        assert!(accesses[1].allow_app_admin);
        assert_eq!(accesses[2].app, Some((app_id, AppRole::Admin)));
        assert!(!accesses[2].allow_app_admin);
        assert_eq!(accesses[3].scope, SuperadminScope::AppsTransfer);
        assert_eq!(accesses[3].app, Some((app_id, AppRole::Owner)));
        assert!(!accesses[3].allow_app_admin);
    }

    #[test]
    fn debug_operations_require_app_read_scope_with_app_admin_tokens() {
        let app_id = Uuid::new_v4();
        for operation in [
            SuperadminOperation::Runtime { app_id },
            SuperadminOperation::Explain {
                app_id,
                actor: AdminActor::Guest,
                query: serde_json::Map::new(),
            },
        ] {
            let access = required_access(&operation);
            assert_eq!(access.scope, SuperadminScope::AppsRead);
            assert_eq!(access.app, Some((app_id, AppRole::Collaborator)));
            assert!(access.allow_app_admin);
        }
    }

    struct DeniedAuthorizer;

    impl SuperadminAuthorizer for DeniedAuthorizer {
        fn authorize(
            &self,
            _bearer_token: String,
            _access: SuperadminAccess,
        ) -> impl Future<Output = Result<SuperadminPrincipal, ApiError>> + Send {
            ready(Err(ApiError::new(
                instant_http::ApiErrorType::PermissionDenied,
                "Insufficient app member role",
            )
            .with_status(axum::http::StatusCode::FORBIDDEN)))
        }
    }

    struct PanicDebug;

    impl SuperadminDebug for PanicDebug {
        fn runtime(&self, _app_id: Uuid) -> Result<Value, ApiError> {
            panic!("authorization must run before operator diagnostics");
        }

        fn explain(
            &self,
            _app_id: Uuid,
            _actor: AdminActor,
            _query: Map<String, Value>,
        ) -> Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + '_>> {
            panic!("authorization must run before operator diagnostics");
        }
    }

    #[tokio::test]
    async fn debug_operations_reject_callers_without_app_read_access() {
        let service =
            SuperadminService::new(DeniedAuthorizer, Store).with_debug(Arc::new(PanicDebug));
        let error = service
            .execute(
                SuperadminContext {
                    bearer_token: "per_secret".to_owned(),
                },
                SuperadminOperation::Runtime {
                    app_id: Uuid::new_v4(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
    }
}
