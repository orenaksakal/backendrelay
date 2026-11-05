//! Read-only operator diagnostics wired into the superadmin API.
//!
//! Both endpoints reuse existing services rather than duplicating query or
//! session bookkeeping: the realtime registry reports this process's local
//! sessions, and the admin data service plans the query under the requested
//! user's rules.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use instant_http::{AdminActor, ApiError};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::admin_data::{AdminDataAuthorizer, AdminDataService};
use crate::runtime_realtime::{RuntimeAppRuntimeReport, RuntimeRealtimeService};
use crate::superadmin::SuperadminDebug;

pub trait SuperadminExplainBackend: Send + Sync + 'static {
    fn explain<'a>(
        &'a self,
        app_id: Uuid,
        actor: AdminActor,
        query: Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + 'a>>;
}

pub trait SuperadminRuntimeBackend: Send + Sync + 'static {
    fn app_runtime(&self, app_id: Uuid) -> RuntimeAppRuntimeReport;
}

pub struct SuperadminDebugService<E, R> {
    explain: Arc<E>,
    runtime: Arc<R>,
}

impl<E, R> SuperadminDebugService<E, R> {
    #[must_use]
    pub const fn new(explain: Arc<E>, runtime: Arc<R>) -> Self {
        Self { explain, runtime }
    }
}

impl<E, R> SuperadminDebug for SuperadminDebugService<E, R>
where
    E: SuperadminExplainBackend,
    R: SuperadminRuntimeBackend,
{
    fn runtime(&self, app_id: Uuid) -> Result<Value, ApiError> {
        let report = self.runtime.app_runtime(app_id);
        Ok(json!({
            "scope": "this-process",
            "sessions": report.sessions,
            "subscriptions_by_query_hash": report.subscriptions_by_query_hash,
            "oldest_refresh_wait_ms": report.oldest_refresh_wait_ms,
        }))
    }

    fn explain(
        &self,
        app_id: Uuid,
        actor: AdminActor,
        query: Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + '_>> {
        self.explain.explain(app_id, actor, query)
    }
}

impl<A> SuperadminExplainBackend for AdminDataService<A>
where
    A: AdminDataAuthorizer,
{
    fn explain<'a>(
        &'a self,
        app_id: Uuid,
        actor: AdminActor,
        query: Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + 'a>> {
        Box::pin(self.explain_instaql_as_user(app_id, actor, query))
    }
}

impl<A, D> SuperadminRuntimeBackend for RuntimeRealtimeService<A, D>
where
    A: Send + Sync + 'static,
    D: Send + Sync + 'static,
{
    fn app_runtime(&self, app_id: Uuid) -> RuntimeAppRuntimeReport {
        self.app_runtime_report(app_id)
    }
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use super::*;
    use crate::runtime_realtime::RuntimeQueryHashCount;

    struct Explain;

    impl SuperadminExplainBackend for Explain {
        fn explain<'a>(
            &'a self,
            _app_id: Uuid,
            _actor: AdminActor,
            query: Map<String, Value>,
        ) -> Pin<Box<dyn Future<Output = Result<Value, ApiError>> + Send + 'a>> {
            Box::pin(ready(Ok(Value::Object(query))))
        }
    }

    struct Runtime(RuntimeAppRuntimeReport);

    impl SuperadminRuntimeBackend for Runtime {
        fn app_runtime(&self, _app_id: Uuid) -> RuntimeAppRuntimeReport {
            self.0.clone()
        }
    }

    #[test]
    fn runtime_response_names_its_process_scope_and_backend_counts() {
        let service = SuperadminDebugService::new(
            Arc::new(Explain),
            Arc::new(Runtime(RuntimeAppRuntimeReport {
                sessions: 3,
                subscriptions_by_query_hash: vec![RuntimeQueryHashCount { hash: 42, count: 7 }],
                oldest_refresh_wait_ms: 250,
            })),
        );
        assert_eq!(
            service.runtime(Uuid::nil()).unwrap(),
            json!({
                "scope": "this-process",
                "sessions": 3,
                "subscriptions_by_query_hash": [{"hash": 42, "count": 7}],
                "oldest_refresh_wait_ms": 250,
            })
        );
    }

    #[tokio::test]
    async fn explain_passes_the_impersonated_actor_and_query_through() {
        let service = SuperadminDebugService::new(
            Arc::new(Explain),
            Arc::new(Runtime(RuntimeAppRuntimeReport::default())),
        );
        let query = Map::from_iter([("goals".to_owned(), json!({}))]);
        let explained = service
            .explain(
                Uuid::nil(),
                AdminActor::Email("person@example.com".to_owned()),
                query.clone(),
            )
            .await
            .unwrap();
        assert_eq!(explained, Value::Object(query));
    }
}
