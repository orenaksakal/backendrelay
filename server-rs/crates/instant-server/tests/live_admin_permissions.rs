use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use instant_http::{
    AdminActor, AdminDataBackend, AdminPresenceBackend, AdminQueryPermissionsRequest,
    AdminQueryRequest, AdminRequestContext, AdminStorageBackend, AdminTransactPermissionsRequest,
    ApiError, ApiErrorType, RuntimeFrameworkResponse, RuntimeRealtimeBackend,
    RuntimeStorageContext, RuntimeTransport, admin_data_router,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, DeleteAction,
    system_attribute_catalog,
};
use instant_realtime::RoomStore;
use instant_server::{
    AdminDataAuthorizer, AdminDataScope, AdminDataService, AdminPresenceService,
    AdminStorageService, DashboardControlHookOperation, DashboardControlHooks,
    DashboardStorageControlHooks, EffectiveAppStatusPolicy, NoopDashboardControlHooks,
    PostgresEffectiveAppStatusPolicy, PostgresPool, PostgresRuntimeStreamBackend,
    PostgresTransactionPoolPressurePolicy, PreparedInstantQuery, QueryModifierPolicy,
    QueryPlannerPolicy, QueryPreparationPolicy, QuerySettingsPolicy, ReplicaReadPolicy,
    RuntimeJoinRowsQueryBackend, RuntimeRealtimeBatchQuery, RuntimeRealtimeCatalogBackend,
    RuntimeRealtimeIdentityBackend, RuntimeRealtimeQueryMode, RuntimeRealtimeService,
    RuntimeStreamAppendRequest, RuntimeStreamBackend, RuntimeStreamDisconnectRequest,
    RuntimeStreamLoadRequest, RuntimeStreamPermissionBackend, RuntimeStreamStartRequest,
    RuntimeSyncBackend, RuntimeSyncSnapshotRequest, RuntimeSyncSubscriptionCreate,
    RuntimeSyncSubscriptionResume, StorageAccessPolicy, StorageFileUrlSigner,
    StorageObjectMetadata, StorageObjectStore, StoragePutRequest, migrate_database,
};
use instant_transact::{TransactionOptions, compile_transaction, execute_transaction};
use serde_json::{Map, Value, json};
use tokio_postgres::{NoTls, types::Json};
use tower::ServiceExt as _;
use uuid::Uuid;

#[path = "../src/native_allocator.rs"]
mod native_allocator;

#[path = "live_admin_permissions/lease_release.rs"]
mod lease_release;

#[derive(Clone, Debug)]
struct TestAuthorizer {
    catalog: AttributeCatalog,
}

struct RejectCatalogReload;

impl AdminDataAuthorizer for RejectCatalogReload {
    async fn authorize(
        &self,
        _: AdminRequestContext,
        _: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        Err(ApiError::new(
            ApiErrorType::Unknown,
            "unexpected authorization/catalog reload",
        ))
    }
    async fn load_catalog(&self, _: Uuid) -> Result<AttributeCatalog, ApiError> {
        Err(ApiError::new(
            ApiErrorType::Unknown,
            "unexpected catalog reload",
        ))
    }
}

#[derive(Debug)]
struct RecordingAuthorizer {
    catalog: AttributeCatalog,
    scopes: Mutex<Vec<AdminDataScope>>,
    deny_writes: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct CountingReplicaReadPolicy {
    calls: AtomicUsize,
}

impl ReplicaReadPolicy for CountingReplicaReadPolicy {
    fn enabled(&self) -> instant_server::ReplicaReadPolicyFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(false)))
    }
}

struct BlockedQueryPreparation {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    fail: bool,
}

impl QueryModifierPolicy for BlockedQueryPreparation {
    fn apply<'a>(
        &'a self,
        _app_id: Uuid,
        _original_query_hash: i32,
        _query: &'a Value,
    ) -> instant_server::QueryModifierPolicyFuture<'a> {
        unreachable!("the query must use the combined policy")
    }
}

impl QuerySettingsPolicy for BlockedQueryPreparation {
    fn settings(&self, _original_query_hash: i32) -> instant_server::QuerySettingsPolicyFuture<'_> {
        unreachable!("the query must use the combined policy")
    }
}

impl QueryPlannerPolicy for BlockedQueryPreparation {
    fn options<'a>(&'a self, _query: &'a Value) -> instant_server::QueryPlannerPolicyFuture<'a> {
        unreachable!("the query must use the combined policy")
    }
}

impl QueryPreparationPolicy for BlockedQueryPreparation {
    fn may_overlap_authorization(&self) -> bool {
        true
    }

    fn prepare<'a>(
        &'a self,
        _app_id: Uuid,
        original: &'a Value,
    ) -> instant_server::QueryPreparationPolicyFuture<'a> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            if self.fail {
                return Err(ApiError::new(
                    ApiErrorType::Unknown,
                    "synthetic query-control failure",
                ));
            }
            Ok(PreparedInstantQuery {
                effective: original.clone(),
                original_hash: instant_instaql::normalized_query_hash(original).unwrap(),
                planner_options: instant_datalog::MatchCteOptions::default(),
                postgres_settings: Vec::new(),
                permission_attribute_projection: true,
                shared_root_frames: true,
            })
        })
    }
}

#[derive(Debug, Default)]
struct TestObjectStore {
    objects: Mutex<std::collections::HashMap<String, Vec<u8>>>,
    puts: Mutex<Vec<StoragePutRequest>>,
}

impl StorageObjectStore for TestObjectStore {
    async fn put(
        &self,
        request: StoragePutRequest,
        body: Body,
    ) -> Result<StorageObjectMetadata, String> {
        let bytes = to_bytes(body, usize::MAX)
            .await
            .map_err(|error| error.to_string())?;
        self.puts.lock().unwrap().push(request.clone());
        self.objects
            .lock()
            .unwrap()
            .insert(request.key, bytes.to_vec());
        Ok(StorageObjectMetadata {
            size: bytes.len() as u64,
            content_type: request.content_type,
            content_disposition: request.content_disposition,
            etag: None,
            last_modified_ms: None,
        })
    }

    fn delete(&self, key: String) -> impl Future<Output = Result<(), String>> + Send {
        self.objects.lock().unwrap().remove(&key);
        std::future::ready(Ok(()))
    }

    fn signed_download_url(
        &self,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        std::future::ready(Ok(format!("https://objects.test/{key}")))
    }
}

impl AdminDataAuthorizer for TestAuthorizer {
    fn authorize(
        &self,
        context: AdminRequestContext,
        _scope: AdminDataScope,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        let result = if context.bearer_token.as_deref() == Some("admin-secret") {
            Ok(self.catalog.clone())
        } else {
            Err(ApiError::new(ApiErrorType::PermissionDenied, "Invalid admin token").unauthorized())
        };
        std::future::ready(result)
    }

    fn load_catalog(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(Ok(self.catalog.clone()))
    }
}

impl AdminDataAuthorizer for RecordingAuthorizer {
    fn authorize(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        self.scopes.lock().unwrap().push(scope);
        let result = if scope == AdminDataScope::Write && self.deny_writes.load(Ordering::SeqCst) {
            Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Write scope is required",
            ))
        } else if context.bearer_token.as_deref() == Some("admin-secret") {
            Ok(self.catalog.clone())
        } else {
            Err(ApiError::new(ApiErrorType::PermissionDenied, "Invalid admin token").unauthorized())
        };
        std::future::ready(result)
    }

    fn load_catalog(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<AttributeCatalog, ApiError>> + Send {
        std::future::ready(Ok(self.catalog.clone()))
    }
}

fn attribute(id: Uuid, label: &str, unique: bool) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: "posts".to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique,
        setting_unique: false,
        indexed: unique,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}

fn named_attribute(id: Uuid, etype: &str, label: &str) -> Attribute {
    let mut attribute = attribute(id, label, false);
    etype.clone_into(&mut attribute.etype);
    attribute
}

fn context(app_id: Uuid, actor: AdminActor) -> AdminRequestContext {
    AdminRequestContext {
        app_id,
        bearer_token: Some("admin-secret".to_owned()),
        actor,
        client_ip: None,
    }
}

#[tokio::test]
async fn native_refresh_batch_reuses_catalog_but_keeps_rules_status_and_user_auth_fresh() {
    use tracing::instrument::WithSubscriber as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let Some(url) = instant_test_support::test_database_url(
        "native_refresh_batch_reuses_catalog_but_keeps_rules_status_and_user_auth_fresh",
    ) else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let setup_driver = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    drop(setup);
    setup_driver.await.unwrap().unwrap();
    let scoped = install_shared_schema(&url).await;
    let (mut client, connection) = tokio_postgres::connect(&scoped, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let app = Uuid::new_v4();
    client
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app])
        .await
        .unwrap();
    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let public_attr = Uuid::new_v4();
    let mut attributes = system_attribute_catalog().unwrap().attributes;
    attributes.extend([
        attribute(id_attr, "id", true),
        attribute(title_attr, "title", false),
        attribute(public_attr, "public", false),
    ]);
    let catalog = AttributeCatalog { attributes };
    insert_catalog_attrs(&client, app, &catalog).await;
    client
        .execute(
            "INSERT INTO rules (app_id,code) VALUES ($1,$2)",
            &[
                &app,
                &json!({"posts":{"allow":{"view":"auth.id != null && data.public"}}}),
            ],
        )
        .await
        .unwrap();
    let visible = Uuid::new_v4();
    let hidden = Uuid::new_v4();
    let steps = compile_transaction(
        &json!([
            ["add-triple", visible, id_attr, visible],
            ["add-triple", visible, title_attr, "allowed-post"],
            ["add-triple", visible, public_attr, true],
            ["add-triple", hidden, id_attr, hidden],
            ["add-triple", hidden, title_attr, "denied-post"],
            ["add-triple", hidden, public_attr, false]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app,
        &catalog,
        &steps,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let user = instant_auth::sign_in_guest(&mut client, app, &catalog)
        .await
        .unwrap();
    let pool = PostgresPool::connect(&scoped, 1).await.unwrap();
    let service = AdminDataService::from_pool(pool.clone(), Arc::new(RejectCatalogReload))
        .with_app_status_policy(Arc::new(PostgresEffectiveAppStatusPolicy::from_pool(
            pool.clone(),
        )));
    let request = |actor, mode| RuntimeRealtimeBatchQuery {
        context: context(app, actor),
        query: json!({"posts":{}}).as_object().unwrap().clone(),
        versions: None,
        mode,
    };
    let queries = vec![
        request(
            AdminActor::RefreshToken(user.refresh_token),
            RuntimeRealtimeQueryMode::JoinRows,
        ),
        request(AdminActor::Guest, RuntimeRealtimeQueryMode::JoinRows),
        request(
            AdminActor::RefreshToken(Uuid::new_v4()),
            RuntimeRealtimeQueryMode::JoinRows,
        ),
    ];
    async fn execute_native(
        service: &AdminDataService<RejectCatalogReload>,
        app: Uuid,
        catalog: &AttributeCatalog,
        queries: Vec<RuntimeRealtimeBatchQuery>,
    ) -> Vec<Result<RuntimeFrameworkResponse, ApiError>> {
        let batch = service
            .refresh_realtime_batch(app, catalog.clone(), queries.clone())
            .await
            .unwrap();
        assert!(!batch.external_catalog);
        assert!(
            batch.results.is_none(),
            "native batch must not pre-execute queries outside runtime admission/cancellation"
        );
        let mut results = Vec::new();
        for query in queries {
            results.push(
                service
                    .query_join_rows_on_native_catalog(
                        query.context,
                        AdminQueryRequest {
                            query: query.query,
                            inference: false,
                            versions: query.versions,
                        },
                        app,
                        batch.catalog.as_ref().clone(),
                    )
                    .await,
            );
        }
        results
    }
    struct RuleCompileLeaseObserver {
        pool: PostgresPool,
        available: Arc<Mutex<Vec<usize>>>,
    }
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RuleCompileLeaseObserver {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _: &tracing::span::Id,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() == "permissions/compile-rules" {
                self.available
                    .lock()
                    .unwrap()
                    .push(self.pool.status().available);
            }
        }
    }
    let available = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(RuleCompileLeaseObserver {
        pool: pool.clone(),
        available: available.clone(),
    });
    let results = execute_native(&service, app, &catalog, queries.clone())
        .with_subscriber(subscriber)
        .await;
    assert_eq!(
        *available.lock().unwrap(),
        vec![1, 1],
        "native rule validation must not retain the sole data lease; invalid tokens must still fail before rule validation"
    );
    assert_eq!(results.len(), 3);
    let allowed = results[0].as_ref().unwrap();
    assert!(allowed.result.to_string().contains("allowed-post"));
    assert!(!allowed.result.to_string().contains("denied-post"));
    assert!(allowed.instant_topics.is_some());
    assert!(
        !results[1]
            .as_ref()
            .unwrap()
            .result
            .to_string()
            .contains("allowed-post"),
        "guest borrowed another query's authenticated identity"
    );
    assert!(
        results[2].is_err(),
        "invalid token borrowed another query's authenticated identity"
    );
    struct ExplicitPostgresSettings(bool);
    impl ExplicitPostgresSettings {
        fn prepared(original: &Value) -> PreparedInstantQuery {
            PreparedInstantQuery {
                effective: original.clone(),
                original_hash: instant_instaql::normalized_query_hash(original).unwrap(),
                planner_options: instant_datalog::MatchCteOptions::default(),
                postgres_settings: vec![instant_server::PostgresQuerySetting {
                    setting: "enable_seqscan".to_owned(),
                    value: "off".to_owned(),
                }],
                permission_attribute_projection: true,
                shared_root_frames: true,
            }
        }
    }
    impl QueryModifierPolicy for ExplicitPostgresSettings {
        fn apply<'a>(
            &'a self,
            _: Uuid,
            _: i32,
            _: &'a Value,
        ) -> instant_server::QueryModifierPolicyFuture<'a> {
            unreachable!("combined policy must be used")
        }
    }
    impl QuerySettingsPolicy for ExplicitPostgresSettings {
        fn settings(&self, _: i32) -> instant_server::QuerySettingsPolicyFuture<'_> {
            unreachable!("combined policy must be used")
        }
    }
    impl QueryPlannerPolicy for ExplicitPostgresSettings {
        fn options<'a>(&'a self, _: &'a Value) -> instant_server::QueryPlannerPolicyFuture<'a> {
            unreachable!("combined policy must be used")
        }
    }
    impl QueryPreparationPolicy for ExplicitPostgresSettings {
        fn try_prepare_no_io(
            &self,
            _: Uuid,
            original: &Value,
        ) -> Option<Result<PreparedInstantQuery, ApiError>> {
            self.0.then(|| Ok(Self::prepared(original)))
        }

        fn prepare<'a>(
            &'a self,
            _: Uuid,
            original: &'a Value,
        ) -> instant_server::QueryPreparationPolicyFuture<'a> {
            Box::pin(std::future::ready(Ok(Self::prepared(original))))
        }
    }
    async fn prepared_executions(pool: &PostgresPool) -> i64 {
        pool.acquire().await.unwrap().query_one("SELECT coalesce(sum(generic_plans+custom_plans),0)::bigint FROM pg_prepared_statements WHERE name <> ''",&[]).await.unwrap().get(0)
    }
    assert!(
        !ExplicitPostgresSettings(false).may_overlap_authorization(),
        "custom policies must remain sequential unless they explicitly opt in"
    );
    let before = prepared_executions(&pool).await;
    let controlled = AdminDataService::from_pool(pool.clone(), Arc::new(RejectCatalogReload))
        .with_query_preparation_policy(Arc::new(ExplicitPostgresSettings(false)));
    let controlled_results = execute_native(&controlled, app, &catalog, queries.clone()).await;
    assert!(
        controlled_results[0]
            .as_ref()
            .unwrap()
            .result
            .to_string()
            .contains("allowed-post")
    );
    assert_eq!(
        prepared_executions(&pool).await,
        before,
        "explicit PostgreSQL overrides must not execute a previously cached generic plan"
    );
    let warm = AdminDataService::from_pool(pool.clone(), Arc::new(RejectCatalogReload))
        .with_query_preparation_policy(Arc::new(ExplicitPostgresSettings(true)));
    struct CheckoutObserver(Arc<AtomicUsize>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CheckoutObserver {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() == "instant_server::database_pool" {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let checkouts = Arc::new(AtomicUsize::new(0));
    let query = queries[0].clone();
    let subscriber = tracing_subscriber::registry().with(CheckoutObserver(Arc::clone(&checkouts)));
    let response = warm
        .query_join_rows_on_native_catalog(
            query.context,
            AdminQueryRequest {
                query: query.query,
                inference: false,
                versions: query.versions,
            },
            app,
            catalog.clone(),
        )
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert!(response.result.to_string().contains("allowed-post"));
    assert!(!response.result.to_string().contains("denied-post"));
    assert_eq!(
        checkouts.load(Ordering::Relaxed),
        1,
        "ready no-I/O controls must retain the same request's authorization lease"
    );
    // Initial SDK reads also need a fresh source check, but a negative result
    // must not force a second checkout when no-I/O controls are ready.
    let ordinary = AdminDataService::from_pool(
        pool.clone(),
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    )
    .with_query_preparation_policy(Arc::new(ExplicitPostgresSettings(true)));
    let query = queries[0].clone();
    checkouts.store(0, Ordering::Relaxed);
    let subscriber = tracing_subscriber::registry().with(CheckoutObserver(Arc::clone(&checkouts)));
    let response = ordinary
        .query_join_rows(
            query.context,
            AdminQueryRequest {
                query: query.query,
                inference: false,
                versions: query.versions,
            },
        )
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert!(response.result.to_string().contains("allowed-post"));
    assert!(!response.result.to_string().contains("denied-post"));
    assert_eq!(
        checkouts.load(Ordering::Relaxed),
        1,
        "fresh native source resolution and execution must share the same request's ready lease"
    );
    assert!(
        response
            .attrs
            .as_array()
            .is_some_and(|attrs| !attrs.is_empty())
    );
    for native in [false, true] {
        let query = queries[0].clone();
        let request = AdminQueryRequest {
            query: query.query,
            inference: false,
            versions: query.versions,
        };
        let compact = if native {
            warm.query_join_rows_on_native_catalog_without_attrs(
                query.context,
                request,
                app,
                catalog.clone(),
            )
            .await
            .unwrap()
        } else {
            ordinary
                .query_join_rows_without_attrs(query.context, request)
                .await
                .unwrap()
        };
        assert_eq!(compact.result, response.result);
        assert_eq!(compact.instant_topics, response.instant_topics);
        assert_eq!(compact.byop_topics, response.byop_topics);
        assert_eq!(
            compact.attrs,
            Value::Null,
            "only the internal opt-out may omit redundant catalog JSON"
        );
    }
    client
        .execute(
            "UPDATE rules SET code=$2 WHERE app_id=$1",
            &[&app, &json!({"posts":{"allow":{"view":"false"}}})],
        )
        .await
        .unwrap();
    let denied = execute_native(&service, app, &catalog, queries.clone()).await;
    let query = queries[0].clone();
    let compact_denied = ordinary
        .query_join_rows_without_attrs(
            query.context,
            AdminQueryRequest {
                query: query.query,
                inference: false,
                versions: query.versions,
            },
        )
        .await
        .unwrap();
    assert!(
        !compact_denied.result.to_string().contains("allowed-post"),
        "omitting wire metadata must never reuse authorization decisions"
    );
    assert!(
        !denied[0]
            .as_ref()
            .unwrap()
            .result
            .to_string()
            .contains("allowed-post"),
        "refresh reused an earlier permission decision"
    );
    client
        .execute(
            "UPDATE rules SET code=$2 WHERE app_id=$1",
            &[&app, &json!({"posts":{"allow":{"view":17}}})],
        )
        .await
        .unwrap();
    let malformed = execute_native(&service, app, &catalog, queries.clone()).await;
    assert_eq!(
        malformed[0].as_ref().unwrap_err().body().kind,
        ApiErrorType::ValidationFailed
    );
    assert_ne!(
        malformed[2].as_ref().unwrap_err().body().kind,
        ApiErrorType::ValidationFailed,
        "authentication failure must retain priority over malformed rule JSON"
    );
    client
        .execute(
            "UPDATE rules SET code=$2 WHERE app_id=$1",
            &[&app, &json!({"posts":{"allow":{"view":"false"}}})],
        )
        .await
        .unwrap();
    client
        .execute("UPDATE apps SET status='disabled' WHERE id=$1", &[&app])
        .await
        .unwrap();
    let disabled = execute_native(&service, app, &catalog, queries.clone()).await;
    assert!(
        disabled
            .iter()
            .all(|r| r.as_ref().unwrap_err().body().kind == ApiErrorType::AppDisabled)
    );
    client
        .execute("UPDATE apps SET status='active' WHERE id=$1", &[&app])
        .await
        .unwrap();
    instant_auth::delete_refresh_token(&mut client, app, &catalog, user.refresh_token)
        .await
        .unwrap();
    let revoked = execute_native(&service, app, &catalog, queries).await;
    assert!(
        revoked[0].is_err(),
        "refresh reused a revoked token's identity"
    );
    for query in [
        request(AdminActor::Admin, RuntimeRealtimeQueryMode::JoinRows),
        request(AdminActor::Guest, RuntimeRealtimeQueryMode::Tree),
    ] {
        assert!(
            service
                .refresh_realtime_batch(app, catalog.clone(), vec![query])
                .await
                .unwrap()
                .results
                .is_none(),
            "admin/tree paths must keep their original authorization/routing path"
        );
    }
    let mut other_app = request(AdminActor::Guest, RuntimeRealtimeQueryMode::JoinRows);
    other_app.context.app_id = Uuid::new_v4();
    assert!(
        service
            .query_join_rows_on_native_catalog(
                other_app.context,
                AdminQueryRequest {
                    query: other_app.query,
                    inference: false,
                    versions: None
                },
                app,
                catalog.clone()
            )
            .await
            .is_err(),
        "a batch must not lend one app's catalog/source decision to another app"
    );
    drop(service);
    drop(client);
    driver.await.unwrap().unwrap();
}

#[tokio::test]
async fn guest_query_releases_pool_lease_before_live_status_check() {
    let Some(database_url) = instant_test_support::test_database_url(
        "guest_query_releases_pool_lease_before_live_status_check",
    ) else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_driver = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    drop(setup);
    setup_driver.await.unwrap().unwrap();
    let database_url = install_shared_schema(&database_url).await;
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO apps (id, status) VALUES ($1, 'disabled')",
            &[&app_id],
        )
        .await
        .unwrap();
    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let service = AdminDataService::from_pool(
        pool.clone(),
        Arc::new(TestAuthorizer {
            catalog: system_attribute_catalog().unwrap(),
        }),
    )
    .with_app_status_policy(Arc::new(PostgresEffectiveAppStatusPolicy::from_pool(pool)));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        futures_util::future::join_all((0..32).map(|_| {
            let service = &service;
            async move {
                let request = AdminQueryRequest {
                    query: Map::new(),
                    inference: false,
                    versions: None,
                };
                service
                    .query(context(app_id, AdminActor::Guest), request)
                    .await
            }
        })),
    )
    .await;
    drop(service);
    drop(client);
    driver.await.unwrap().unwrap();
    for outcome in
        result.expect("queries must not wait for a second lease while holding the only connection")
    {
        assert_eq!(outcome.unwrap_err().body().kind, ApiErrorType::AppDisabled);
    }
}

#[tokio::test]
async fn native_guest_refresh_releases_data_lease_while_query_controls_wait() {
    let Some(database_url) = instant_test_support::test_database_url(
        "native_guest_refresh_releases_data_lease_while_query_controls_wait",
    ) else {
        return;
    };
    let database_url = install_shared_schema(&database_url).await;
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let app_id = Uuid::new_v4();
    client
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[&app_id, &json!({"posts": {"allow": {"view": "true"}}})],
        )
        .await
        .unwrap();
    let catalog = AttributeCatalog {
        attributes: vec![attribute(Uuid::new_v4(), "id", true)],
    };
    insert_catalog_attrs(&client, app_id, &catalog).await;
    drop(client);
    driver.await.unwrap().unwrap();

    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let policy = Arc::new(BlockedQueryPreparation {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        fail: false,
    });
    let authorizer = Arc::new(TestAuthorizer { catalog });
    let data = Arc::new(
        AdminDataService::from_pool(pool.clone(), Arc::clone(&authorizer))
            .with_app_status_policy(Arc::new(PostgresEffectiveAppStatusPolicy::from_pool(
                pool.clone(),
            )))
            .with_query_preparation_policy(policy.clone()),
    );
    let service = RuntimeRealtimeService::new(authorizer, data, Uuid::new_v4());
    let mut session = service
        .open(Some(app_id), RuntimeTransport::WebSocket)
        .await
        .unwrap();
    service
        .message(session.session_id, json!({"op": "init", "app-id": app_id}))
        .await
        .unwrap();
    let initialized = tokio::time::timeout(Duration::from_secs(2), session.recv())
        .await
        .expect("native guest session did not initialize")
        .unwrap();
    assert_eq!(initialized["op"], "init-ok", "{initialized}");
    service
        .message(
            session.session_id,
            json!({
                "op": "add-query",
                "return-type": "tree",
                "q": {"posts": {}},
                "client-event-id": "blocked-controls"
            }),
        )
        .await
        .unwrap();
    if tokio::time::timeout(Duration::from_secs(2), policy.entered.notified())
        .await
        .is_err()
    {
        policy.release.add_permits(1);
        let outcome = tokio::time::timeout(Duration::from_millis(250), session.recv()).await;
        drop(session);
        drop(service);
        panic!("native guest query did not reach the blocked control policy: {outcome:?}");
    }

    // The gate is a separate dependency. Waiting for it must not occupy the
    // only data lease and block an unrelated refresh's catalog/status work.
    let independent_read = tokio::time::timeout(Duration::from_millis(250), async {
        let lease = pool.acquire().await.unwrap();
        lease.simple_query("SELECT 1").await.unwrap();
    })
    .await;
    // Release the query and verify completion before asserting the regression,
    // so the intentionally failing baseline does not leave a blocked task.
    policy.release.add_permits(1);
    let response = tokio::time::timeout(Duration::from_secs(2), session.recv())
        .await
        .expect("native query did not complete after releasing its controls")
        .unwrap();
    assert_eq!(response["op"], "add-query-ok", "{response}");
    assert_eq!(response["client-event-id"], "blocked-controls");
    assert_eq!(response["result"], json!({"posts": []}));
    drop(session);
    drop(service);
    assert!(
        independent_read.is_ok(),
        "waiting for query controls retained the only data lease and blocked an independent read"
    );
}

#[tokio::test]
async fn native_refresh_controls_overlap_auth_wait_and_cancel_on_auth_failure() {
    let Some(url) = instant_test_support::test_database_url(
        "native_refresh_controls_overlap_auth_wait_and_cancel_on_auth_failure",
    ) else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let setup_driver = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    drop(setup);
    setup_driver.await.unwrap().unwrap();
    let url = install_shared_schema(&url).await;
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let app = Uuid::new_v4();
    client
        .execute("INSERT INTO apps(id) VALUES($1)", &[&app])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO rules(app_id,code) VALUES($1,$2)",
            &[&app, &json!({"posts":{"allow":{"view":"true"}}})],
        )
        .await
        .unwrap();
    let mut catalog = system_attribute_catalog().unwrap();
    catalog
        .attributes
        .push(attribute(Uuid::new_v4(), "id", true));
    insert_catalog_attrs(&client, app, &catalog).await;
    drop(client);
    driver.await.unwrap().unwrap();
    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    let policy = Arc::new(BlockedQueryPreparation {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        fail: false,
    });
    let service = Arc::new(
        AdminDataService::from_pool(pool.clone(), Arc::new(RejectCatalogReload))
            .with_query_preparation_policy(policy.clone()),
    );
    let held = pool.acquire().await.unwrap();
    let data = service.clone();
    let attrs = catalog.clone();
    let query = tokio::spawn(async move {
        data.query_join_rows_on_native_catalog(
            context(app, AdminActor::Guest),
            AdminQueryRequest {
                query: json!({"posts":{}}).as_object().unwrap().clone(),
                inference: false,
                versions: None,
            },
            app,
            attrs,
        )
        .await
    });
    let overlapped = tokio::time::timeout(Duration::from_millis(250), policy.entered.notified())
        .await
        .is_ok();
    assert!(
        !query.is_finished(),
        "controls alone must never bypass the fresh auth/rule read"
    );
    drop(held);
    policy.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), query)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        overlapped,
        "native controls must begin while fresh authorization is waiting for the only data lease"
    );

    let policy = Arc::new(BlockedQueryPreparation {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        fail: false,
    });
    let data = AdminDataService::from_pool(pool.clone(), Arc::new(RejectCatalogReload))
        .with_query_preparation_policy(policy);
    let invalid = data.query_join_rows_on_native_catalog(
        context(app, AdminActor::RefreshToken(Uuid::new_v4())),
        AdminQueryRequest {
            query: json!({"posts":{}}).as_object().unwrap().clone(),
            inference: false,
            versions: None,
        },
        app,
        catalog.clone(),
    );
    let error = tokio::time::timeout(Duration::from_secs(2), invalid)
        .await
        .expect("auth failure must cancel blocked control preparation without waiting for release")
        .unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::RecordNotFound);
    let failed_policy = Arc::new(BlockedQueryPreparation {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(3),
        fail: true,
    });
    let data = AdminDataService::from_pool(pool.clone(), Arc::new(RejectCatalogReload))
        .with_query_preparation_policy(failed_policy);
    let request = || AdminQueryRequest {
        query: json!({"posts":{}}).as_object().unwrap().clone(),
        inference: false,
        versions: None,
    };
    let error = data
        .query_join_rows_on_native_catalog(
            context(app, AdminActor::RefreshToken(Uuid::new_v4())),
            request(),
            app,
            catalog.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.body().kind,
        ApiErrorType::RecordNotFound,
        "auth failure must retain priority over an already-failed control read"
    );
    let error = data
        .query_join_rows_on_native_catalog(
            context(app, AdminActor::Guest),
            request(),
            app,
            catalog.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.body().kind,
        ApiErrorType::Unknown,
        "control errors must not be ignored for authorized requests"
    );
    let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    setup
        .execute(
            "UPDATE rules SET code=$2 WHERE app_id=$1",
            &[&app, &json!({"posts":{"allow":{"view":17}}})],
        )
        .await
        .unwrap();
    drop(setup);
    driver.await.unwrap().unwrap();
    let error = data
        .query_join_rows_on_native_catalog(context(app, AdminActor::Guest), request(), app, catalog)
        .await
        .unwrap_err();
    assert_eq!(
        error.body().kind,
        ApiErrorType::ValidationFailed,
        "fresh malformed rules must retain priority over an already-failed control read"
    );
    let lease = tokio::time::timeout(Duration::from_millis(250), pool.acquire())
        .await
        .unwrap()
        .unwrap();
    lease.simple_query("SELECT 1").await.unwrap();
}

#[tokio::test]
async fn native_framework_queries_release_data_lease_while_controls_wait() {
    let Some(database_url) = instant_test_support::test_database_url(
        "native_framework_queries_release_data_lease_while_controls_wait",
    ) else {
        return;
    };
    let (mut setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_driver = tokio::spawn(connection);
    migrate_database(&mut setup).await.unwrap();
    drop(setup);
    setup_driver.await.unwrap().unwrap();
    let database_url = install_shared_schema(&database_url).await;
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let app_id = Uuid::new_v4();
    client
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[&app_id, &json!({"posts": {"allow": {"view": "true"}}})],
        )
        .await
        .unwrap();
    let catalog = AttributeCatalog {
        attributes: vec![attribute(Uuid::new_v4(), "id", true)],
    };
    insert_catalog_attrs(&client, app_id, &catalog).await;
    drop(client);
    driver.await.unwrap().unwrap();

    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let policy = Arc::new(BlockedQueryPreparation {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        fail: false,
    });
    let authorizer = Arc::new(TestAuthorizer { catalog });
    let data = Arc::new(
        AdminDataService::from_pool(pool.clone(), Arc::clone(&authorizer))
            .with_app_status_policy(Arc::new(PostgresEffectiveAppStatusPolicy::from_pool(
                pool.clone(),
            )))
            .with_query_preparation_policy(policy.clone()),
    );

    for actor in [AdminActor::Guest, AdminActor::Admin] {
        let data = Arc::clone(&data);
        let query = tokio::spawn(async move {
            RuntimeJoinRowsQueryBackend::query_join_rows(
                data.as_ref(),
                context(app_id, actor),
                AdminQueryRequest {
                    query: serde_json::from_value(json!({"posts": {}})).unwrap(),
                    inference: false,
                    versions: None,
                },
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), policy.entered.notified())
            .await
            .expect("framework query must reach controls");
        let independent_read = tokio::time::timeout(Duration::from_millis(250), async {
            let lease = pool.acquire().await.unwrap();
            lease.simple_query("SELECT 1").await.unwrap();
        })
        .await;
        policy.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), query)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            independent_read.is_ok(),
            "framework controls retained the only data connection"
        );
    }
}

async fn post_guest_transaction(
    router: axum::Router,
    app_id: Uuid,
    steps: Value,
) -> axum::response::Response {
    router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/transact")
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header("as-guest", "true")
                .body(Body::from(json!({"steps": steps}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

const TEST_SCHEMA: &str =
    "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TYPE pg_temp.topics_idx AS ENUM ('any', 'ea', 'eav', 'av', 'ave', 'vae');
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               status text NOT NULL DEFAULT 'active',
               deletion_marked_at timestamptz,
               connection_string text
             );
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE attrs (
               id uuid NOT NULL,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL,
               is_required boolean,
               forward_ident uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               reverse_ident uuid,
               reverse_etype text,
               reverse_label text,
               on_delete attr_on_delete,
               on_delete_reverse attr_on_delete,
               checked_data_type checked_data_type,
               setting_unique boolean,
               indexing boolean,
               checking_data_type boolean,
               inferred_types bit(32),
               metadata jsonb,
               deletion_marked_at timestamptz,
               PRIMARY KEY (app_id, id)
             );
             CREATE UNIQUE INDEX server_perms_attrs_id ON attrs(id);
             CREATE TEMP TABLE rules (app_id uuid PRIMARY KEY, code jsonb NOT NULL);
             CREATE TEMP TABLE app_upload_urls (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               path text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(),
               expired_at timestamp NOT NULL DEFAULT (now() + interval '5 minutes')
             );
             CREATE TEMP TABLE sync_subs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               query text NOT NULL,
               token_hash bytea,
               is_admin boolean NOT NULL,
               user_id uuid,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE sync_sub_topics (
               sync_sub_id uuid NOT NULL REFERENCES sync_subs(id) ON DELETE CASCADE,
               topic_num integer NOT NULL,
               idx topics_idx[] NOT NULL,
               e bytea[] NOT NULL,
               a bytea[] NOT NULL,
               v jsonb[] NOT NULL,
               v_filter jsonb,
               PRIMARY KEY(sync_sub_id, topic_num)
             );
             CREATE TEMP TABLE rust_realtime_stream_buffers (
               app_id uuid NOT NULL,
               stream_id uuid NOT NULL,
               machine_id uuid NOT NULL,
               content text NOT NULL DEFAULT '',
               size bigint NOT NULL DEFAULT 0,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY(app_id, stream_id)
             );
             CREATE TEMP TABLE triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL,
               value_md5 text NOT NULL,
               ea boolean NOT NULL,
               eav boolean NOT NULL,
               av boolean NOT NULL,
               ave boolean NOT NULL,
               vae boolean NOT NULL,
               checked_data_type checked_data_type,
               created_at bigint NOT NULL DEFAULT ((extract(epoch from clock_timestamp()) * 1000)::bigint),
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5)
             );
             CREATE UNIQUE INDEX server_perms_ea ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE TEMP TABLE triples_size_aggregate (
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               pg_size bigint NOT NULL,
               files_size bigint,
               PRIMARY KEY (app_id, attr_id)
             );
             CREATE OR REPLACE FUNCTION public.json_null_to_null(v jsonb)
             RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
               SELECT CASE WHEN v = 'null'::jsonb THEN NULL ELSE v END
             $$;
             CREATE UNIQUE INDEX server_perms_av
               ON triples(app_id, attr_id, public.json_null_to_null(value)) WHERE av;
             CREATE OR REPLACE FUNCTION public.jsonb_deep_merge(obj1 jsonb, obj2 jsonb)
             RETURNS jsonb LANGUAGE plpgsql IMMUTABLE AS $$
             DECLARE key text; item jsonb;
             BEGIN
               IF obj1 IS NULL OR obj2 IS NULL OR jsonb_typeof(obj1) != 'object' OR jsonb_typeof(obj2) != 'object'
               THEN RETURN obj2; END IF;
               FOR key, item IN SELECT * FROM jsonb_each(obj2) LOOP
                 obj1 := CASE
                   WHEN obj1 ? key AND jsonb_typeof(obj1->key) = 'object' AND jsonb_typeof(item) = 'object'
                     THEN jsonb_set(obj1, ARRAY[key], public.jsonb_deep_merge(obj1->key, item))
                   WHEN jsonb_typeof(item) = 'null' THEN obj1 - key
                   ELSE jsonb_set(obj1, ARRAY[key], item)
                 END;
               END LOOP;
               RETURN obj1;
              END $$;";

const TEST_SCHEMA_DDL_LOCK: i64 = 7_843_311_947;

async fn install_schema(client: &tokio_postgres::Client) {
    client
        .batch_execute(&format!(
            "BEGIN; SELECT pg_advisory_xact_lock({TEST_SCHEMA_DDL_LOCK}); {TEST_SCHEMA} COMMIT;"
        ))
        .await
        .unwrap();
}

fn scoped_database_url(database_url: &str, schema: &str) -> String {
    let mut url = url::Url::parse(database_url).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema},public"));
    url.into()
}

async fn install_shared_schema(database_url: &str) -> String {
    let schema = format!("admin_permissions_{}", Uuid::new_v4().simple());
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public"
        ))
        .await
        .unwrap();
    let ddl = TEST_SCHEMA
        .replace("pg_temp.", &format!("{schema}."))
        .replace("CREATE TEMP TABLE", "CREATE TABLE");
    client
        .batch_execute(&format!(
            "BEGIN; SELECT pg_advisory_xact_lock({TEST_SCHEMA_DDL_LOCK}); {ddl} COMMIT;"
        ))
        .await
        .unwrap();
    drop(client);
    connection_task.await.unwrap().unwrap();
    scoped_database_url(database_url, &schema)
}

async fn insert_catalog_attrs(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
) {
    for attribute in &catalog.attributes {
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        let cardinality = match attribute.cardinality {
            Cardinality::One => "one",
            Cardinality::Many => "many",
        };
        client
            .execute(
                "INSERT INTO attrs
                 (id, app_id, value_type, cardinality, is_unique, is_indexed,
                  forward_ident, etype, label)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT DO NOTHING",
                &[
                    &attribute.id,
                    &app_id,
                    &value_type,
                    &cardinality,
                    &attribute.unique,
                    &attribute.indexed,
                    &Uuid::new_v4(),
                    &attribute.etype,
                    &attribute.label,
                ],
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn runtime_sync_snapshot_uses_authorized_framework_rows() {
    let Some(database_url) = instant_test_support::test_database_url(
        "runtime_sync_snapshot_uses_authorized_framework_rows",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let (replica, replica_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let replica_connection_task = tokio::spawn(replica_connection);
    install_schema(&client).await;
    let app_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let first_entity = Uuid::new_v4();
    let second_entity = Uuid::new_v4();
    client
        .execute("insert into transactions(app_id) values ($1)", &[&app_id])
        .await
        .unwrap();
    for (entity_id, created_at, title) in [
        (second_entity, 20_i64, "draft"),
        (first_entity, 10_i64, "published"),
    ] {
        for (attribute_id, value, suffix) in [
            (id_attr, json!(entity_id), "id"),
            (title_attr, json!(title), "title"),
        ] {
            client
                .execute(
                    "insert into triples(app_id, entity_id, attr_id, value, value_md5, \
                       ea, eav, av, ave, vae, created_at) \
                     values ($1, $2, $3, $4, $5, true, false, false, false, false, $6)",
                    &[
                        &app_id,
                        &entity_id,
                        &attribute_id,
                        &tokio_postgres::types::Json(value),
                        &format!("{entity_id}-{suffix}"),
                        &created_at,
                    ],
                )
                .await
                .unwrap();
        }
    }
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(id_attr, "id", true),
            attribute(title_attr, "title", false),
        ],
    };
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &json!({"posts": {"allow": {"view": "data.title == 'published'"}}}),
            ],
        )
        .await
        .unwrap();
    let authorizer = Arc::new(RecordingAuthorizer {
        catalog: catalog.clone(),
        scopes: Mutex::new(Vec::new()),
        deny_writes: std::sync::atomic::AtomicBool::new(false),
    });
    let replica_policy = Arc::new(CountingReplicaReadPolicy::default());
    let service = AdminDataService::new(client, Arc::clone(&authorizer))
        .with_realtime_read_replica(
            replica,
            Arc::clone(&replica_policy) as Arc<dyn ReplicaReadPolicy>,
        );
    let query: Map<String, Value> = serde_json::from_value(json!({"posts": {}})).unwrap();
    let snapshot = service
        .sync_snapshot(RuntimeSyncSnapshotRequest {
            context: context(app_id, AdminActor::Admin),
            query: query.clone(),
        })
        .await
        .unwrap();
    assert_eq!(replica_policy.calls.load(Ordering::SeqCst), 0);
    assert_eq!(snapshot.tx_id, 1);
    assert_eq!(snapshot.join_rows.len(), 2);
    assert!(
        snapshot
            .join_rows
            .iter()
            .all(|row| row.as_array().is_some_and(|triples| triples.len() == 2))
    );
    assert_eq!(
        snapshot
            .join_rows
            .iter()
            .flat_map(|row| row.as_array().into_iter().flatten())
            .filter_map(|triple| triple[0].as_str())
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([first_entity.to_string(), second_entity.to_string()])
    );
    let subscription_id = Uuid::new_v4();
    let token = Uuid::new_v4();
    service
        .create_sync_subscription(RuntimeSyncSubscriptionCreate {
            context: context(app_id, AdminActor::Admin),
            subscription_id,
            token,
            query: query.clone(),
            attribute_ids: snapshot.attribute_ids.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::Read, AdminDataScope::Write]
    );
    let resumed = service
        .resume_sync_subscription(RuntimeSyncSubscriptionResume {
            context: context(app_id, AdminActor::Admin),
            subscription_id,
            token,
        })
        .await
        .unwrap();
    assert_eq!(resumed.app_id, app_id);
    assert_eq!(resumed.query, query);
    assert_eq!(resumed.attribute_ids.len(), 2);
    assert!(resumed.attribute_ids.contains(&id_attr));
    assert!(resumed.attribute_ids.contains(&title_attr));
    let bad_token = service
        .resume_sync_subscription(RuntimeSyncSubscriptionResume {
            context: context(app_id, AdminActor::Admin),
            subscription_id,
            token: Uuid::new_v4(),
        })
        .await
        .unwrap_err();
    assert_eq!(bad_token.body().kind, ApiErrorType::ValidationFailed);
    authorizer.deny_writes.store(true, Ordering::SeqCst);
    let denied_delete = service
        .delete_sync_subscription(context(app_id, AdminActor::Admin), subscription_id)
        .await
        .unwrap_err();
    assert_eq!(denied_delete.body().kind, ApiErrorType::PermissionDenied);
    authorizer.deny_writes.store(false, Ordering::SeqCst);
    service
        .delete_sync_subscription(context(app_id, AdminActor::Admin), subscription_id)
        .await
        .unwrap();
    assert_eq!(
        authorizer.scopes.lock().unwrap().last(),
        Some(&AdminDataScope::Write)
    );
    authorizer.deny_writes.store(true, Ordering::SeqCst);
    let denied_create = service
        .create_sync_subscription(RuntimeSyncSubscriptionCreate {
            context: context(app_id, AdminActor::Admin),
            subscription_id: Uuid::new_v4(),
            token: Uuid::new_v4(),
            query: query.clone(),
            attribute_ids: snapshot.attribute_ids.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(denied_create.body().kind, ApiErrorType::PermissionDenied);
    let guest_snapshot = service
        .sync_snapshot(RuntimeSyncSnapshotRequest {
            context: context(app_id, AdminActor::Guest),
            query: query.clone(),
        })
        .await
        .unwrap();
    assert_eq!(guest_snapshot.join_rows.len(), 1);
    assert!(
        guest_snapshot.join_rows[0]
            .as_array()
            .is_some_and(|triples| triples.iter().any(|triple| triple[2] == "published"))
    );
    let guest_subscription_id = Uuid::new_v4();
    service
        .create_sync_subscription(RuntimeSyncSubscriptionCreate {
            context: context(app_id, AdminActor::Guest),
            subscription_id: guest_subscription_id,
            token: Uuid::new_v4(),
            query: query.clone(),
            attribute_ids: guest_snapshot.attribute_ids,
        })
        .await
        .unwrap();
    service
        .delete_sync_subscription(context(app_id, AdminActor::Guest), guest_subscription_id)
        .await
        .unwrap();
    assert!(
        service
            .resume_sync_subscription(RuntimeSyncSubscriptionResume {
                context: context(app_id, AdminActor::Admin),
                subscription_id,
                token,
            })
            .await
            .is_err()
    );
    drop(service);
    connection_task.abort();
    replica_connection_task.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn admin_permission_debug_filters_queries_and_audits_transaction_rollbacks() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_permission_debug_filters_queries_and_audits_transaction_rollbacks",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let public_attr = Uuid::new_v4();
    let mut attributes = system_attribute_catalog().unwrap().attributes;
    attributes.extend([
        attribute(id_attr, "id", true),
        attribute(title_attr, "title", false),
        attribute(public_attr, "public", false),
    ]);
    let catalog = AttributeCatalog { attributes };
    insert_catalog_attrs(&client, app_id, &catalog).await;
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &json!({"posts": {"allow": {
                    "update": "data.public",
                    "view": "data.public"
                }}, "$streams": {"allow": {
                    "create": "data.clientId == ruleParams.clientId",
                    "view": "data.clientId == ruleParams.clientId"
                }}}),
            ],
        )
        .await
        .unwrap();
    let visible = Uuid::new_v4();
    let hidden = Uuid::new_v4();
    let seed = compile_transaction(
        &json!([
            ["add-triple", visible, id_attr, visible],
            ["add-triple", visible, title_attr, "visible"],
            ["add-triple", visible, public_attr, true],
            ["add-triple", hidden, id_attr, hidden],
            ["add-triple", hidden, title_attr, "hidden"],
            ["add-triple", hidden, public_attr, false]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let room_identity = instant_auth::sign_in_guest(&mut client, app_id, &catalog)
        .await
        .unwrap();

    let service = Arc::new(AdminDataService::new(
        client,
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    ));
    assert_eq!(
        service
            .resolve_realtime_user_id(AdminRequestContext {
                app_id,
                bearer_token: None,
                actor: AdminActor::RefreshToken(room_identity.refresh_token),
                client_ip: None,
            })
            .await
            .unwrap(),
        Some(room_identity.user.id)
    );
    service
        .authorize_stream_create(
            context(app_id, AdminActor::Guest),
            "allowed-stream".to_owned(),
            json!({"clientId": "allowed-stream"}),
        )
        .await
        .unwrap();
    service
        .authorize_stream_view(
            context(app_id, AdminActor::Guest),
            json!({"clientId": "allowed-stream"}),
            json!({"clientId": "allowed-stream"}),
        )
        .await
        .unwrap();
    let denied_stream = service
        .authorize_stream_create(
            context(app_id, AdminActor::Guest),
            "denied-stream".to_owned(),
            json!({"clientId": "other-stream"}),
        )
        .await
        .unwrap_err();
    assert_eq!(denied_stream.body().kind, ApiErrorType::PermissionDenied);
    let debug_query_prepared = service
        .prepare_query_permissions(
            context(app_id, AdminActor::Guest),
            Some(json!({
                "posts": {"allow": {"view": "data.public"}}
            })),
        )
        .await
        .unwrap();
    let debug_query = service
        .query_permissions(
            debug_query_prepared,
            AdminQueryPermissionsRequest {
                query: serde_json::from_value(json!({"posts": {}})).unwrap(),
                inference: false,
                ip_override: Some("127.0.0.1".to_owned()),
                origin_override: None,
            },
        )
        .await
        .unwrap();
    let checks = debug_query["check-results"].as_array().unwrap();
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().any(|check| check["check"] == json!(false)));
    assert_eq!(debug_query["result"]["posts"].as_array().unwrap().len(), 1);

    let impersonated_query = service
        .query(
            context(app_id, AdminActor::Guest),
            AdminQueryRequest {
                query: serde_json::from_value(json!({"posts": {}})).unwrap(),
                inference: false,
                versions: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(impersonated_query["posts"].as_array().unwrap().len(), 1);
    assert_eq!(impersonated_query["posts"][0]["title"], "visible");
    let permissioned_tx = service
        .transact(
            context(app_id, AdminActor::Guest),
            instant_http::AdminTransactRequest {
                steps: vec![json!(["add-triple", visible, title_attr, "permitted"])],
                throw_on_missing_attrs: false,
            },
        )
        .await
        .unwrap();
    assert!(permissioned_tx.tx_id > 0);

    let denied = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![json!(["add-triple", visible, title_attr, "denied"])],
                rules_override: Some(json!({
                    "posts": {"allow": {"update": "false"}}
                })),
                dangerously_commit: false,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: Some("https://example.com".to_owned()),
            },
        )
        .await
        .unwrap();
    assert_eq!(denied["all-checks-ok?"], json!(false));
    assert_eq!(denied["committed?"], json!(false));
    assert_eq!(denied["check-results"][0]["check-result"], json!(false));

    let query = service
        .query(
            context(app_id, AdminActor::Admin),
            AdminQueryRequest {
                query: serde_json::from_value(json!({"posts": {}})).unwrap(),
                inference: false,
                versions: None,
            },
        )
        .await
        .unwrap();
    let titles = query["posts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|post| post["title"].as_str())
        .collect::<Vec<_>>();
    assert!(titles.contains(&"permitted"));
    assert!(!titles.contains(&"denied"));

    let committed = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![json!(["add-triple", visible, title_attr, "committed"])],
                rules_override: Some(json!({
                    "posts": {"allow": {"update": "true"}}
                })),
                dangerously_commit: true,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(committed["all-checks-ok?"], json!(true));
    assert_eq!(committed["committed?"], json!(true));

    let attribute_check = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![json!(["update-attr", {
                    "id": title_attr,
                    "index?": true
                }])],
                rules_override: None,
                dangerously_commit: false,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(attribute_check["all-checks-ok?"], false);
    assert_eq!(attribute_check["committed?"], false);
    assert_eq!(
        attribute_check["check-results"][0],
        json!({
            "scope": "attr",
            "action": "update",
            "etype": "attrs",
            "program": {},
            "check-result": false,
            "check-pass?": false,
        })
    );

    let ordered_checks = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![
                    json!(["add-triple", visible, title_attr, "ordered-visible"]),
                    json!(["update-attr", {"id": public_attr, "index?": true}]),
                    json!(["add-triple", hidden, title_attr, "ordered-hidden"]),
                ],
                rules_override: Some(json!({
                    "posts": {"allow": {"update": true}}
                })),
                dangerously_commit: false,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: None,
            },
        )
        .await
        .unwrap();
    let ordered_checks = ordered_checks["check-results"].as_array().unwrap();
    assert_eq!(ordered_checks.len(), 3);
    assert_eq!(ordered_checks[0]["scope"], "object");
    assert_eq!(ordered_checks[0]["eid"], visible.to_string());
    assert_eq!(ordered_checks[1]["scope"], "attr");
    assert!(ordered_checks[1].get("eid").is_none());
    assert_eq!(ordered_checks[2]["scope"], "object");
    assert_eq!(ordered_checks[2]["eid"], hidden.to_string());

    let lookup_delete_checks = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![
                    json!(["delete-entity", [id_attr, visible], "posts"]),
                    json!(["add-triple", hidden, title_attr, "after-delete-lookup"]),
                ],
                rules_override: Some(json!({
                    "posts": {"allow": {"update": true, "delete": true}}
                })),
                dangerously_commit: false,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: None,
            },
        )
        .await
        .unwrap();
    let lookup_delete_checks = lookup_delete_checks["check-results"].as_array().unwrap();
    assert_eq!(lookup_delete_checks.len(), 2);
    assert_eq!(lookup_delete_checks[0]["action"], "update");
    assert_eq!(lookup_delete_checks[0]["eid"], hidden.to_string());
    assert_eq!(lookup_delete_checks[1]["action"], "delete");
    assert_eq!(lookup_delete_checks[1]["eid"], visible.to_string());

    let unknown_delete = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![json!([
                    "delete-entity",
                    [Uuid::new_v4(), "missing"],
                    "posts"
                ])],
                rules_override: None,
                dangerously_commit: false,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: None,
            },
        )
        .await
        .unwrap();
    assert!(
        unknown_delete["check-results"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let paired_rule_params = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![
                    json!(["delete-entity", [id_attr, visible], "posts"]),
                    json!([
                        "rule-params",
                        [id_attr, visible],
                        "comments",
                        {"localId": "owner"}
                    ]),
                ],
                rules_override: Some(json!({
                    "posts": {"allow": {"delete": true}}
                })),
                dangerously_commit: false,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(paired_rule_params["check-results"][0]["action"], "delete");

    let mismatched_rule_params = service
        .transact_permissions(
            context(app_id, AdminActor::Guest),
            AdminTransactPermissionsRequest {
                steps: vec![json!([
                    "rule-params",
                    [id_attr, visible],
                    "comments",
                    {"localId": "owner"}
                ])],
                rules_override: None,
                dangerously_commit: false,
                throw_on_missing_attrs: false,
                ip_override: None,
                origin_override: None,
            },
        )
        .await
        .unwrap_err();
    let lookup_type_message = "Invalid transaction. The namespace in the lookup attribute is different from the namespace of the attribute that is being set";
    assert_eq!(
        mismatched_rule_params.body().message,
        format!("Validation failed for tx-step: {lookup_type_message}")
    );
    assert_eq!(
        mismatched_rule_params.body().hint,
        Some(json!({
            "data-type": "tx-step",
            "input": [
                "rule-params",
                [id_attr, visible],
                "comments",
                {"localId": "owner"}
            ],
            "errors": [{"message": lookup_type_message}]
        }))
    );

    let no_impersonation = service
        .prepare_query_permissions(context(app_id, AdminActor::Admin), None)
        .await
        .unwrap_err();
    assert_eq!(no_impersonation.body().kind, ApiErrorType::ValidationFailed);

    drop(service);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn request_ip_rule_uses_the_resolved_client_address() {
    let Some(database_url) =
        instant_test_support::test_database_url("request_ip_rule_uses_the_resolved_client_address")
    else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let mut attributes = system_attribute_catalog().unwrap().attributes;
    attributes.extend([
        attribute(id_attr, "id", true),
        attribute(title_attr, "title", false),
    ]);
    let catalog = AttributeCatalog { attributes };
    insert_catalog_attrs(&client, app_id, &catalog).await;
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &json!({"posts": {"allow": {"view": "request.ip == '203.0.113.9'"}}}),
            ],
        )
        .await
        .unwrap();
    let post = Uuid::new_v4();
    let seed = compile_transaction(
        &json!([
            ["add-triple", post, id_attr, post],
            ["add-triple", post, title_attr, "visible"]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();

    let service = Arc::new(AdminDataService::new(
        client,
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    ));
    let query = || AdminQueryRequest {
        query: serde_json::from_value(json!({"posts": {}})).unwrap(),
        inference: false,
        versions: None,
    };

    let mut allowed_context = context(app_id, AdminActor::Guest);
    allowed_context.client_ip = Some("203.0.113.9".parse().unwrap());
    let allowed = service.query(allowed_context, query()).await.unwrap();
    assert_eq!(allowed["posts"].as_array().unwrap().len(), 1);

    let mut denied_context = context(app_id, AdminActor::Guest);
    denied_context.client_ip = Some("203.0.113.10".parse().unwrap());
    let denied = service.query(denied_context, query()).await.unwrap();
    assert!(denied["posts"].as_array().unwrap().is_empty());

    connection_task.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the production pool, HTTP boundary, and write proof together.
async fn permissioned_http_transactions_enforce_pressure_and_exact_denial_rollback() {
    let Some(database_url) = instant_test_support::test_database_url(
        "permissioned_http_transactions_enforce_pressure_and_exact_denial_rollback",
    ) else {
        return;
    };
    let (mut migration_client, migration_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let migration_task = tokio::spawn(migration_connection);
    migrate_database(&mut migration_client).await.unwrap();
    drop(migration_client);
    migration_task.await.unwrap().unwrap();

    let database_url = install_shared_schema(&database_url).await;
    let (setup, setup_connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    let app_id = Uuid::new_v4();
    setup
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    let title_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![attribute(title_attr, "title", false)],
    };
    insert_catalog_attrs(&setup, app_id, &catalog).await;
    setup
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({"posts": {"allow": {"create": "true"}}})),
            ],
        )
        .await
        .unwrap();

    let config_app_id = Uuid::new_v4();
    let toggle_setting_attr = Uuid::new_v4();
    let toggled_attr = Uuid::new_v4();
    let flag_setting_attr = Uuid::new_v4();
    let flag_value_attr = Uuid::new_v4();
    let policy_catalog = AttributeCatalog {
        attributes: vec![
            named_attribute(toggle_setting_attr, "toggles", "setting"),
            named_attribute(toggled_attr, "toggles", "toggled"),
            named_attribute(flag_setting_attr, "flags", "setting"),
            named_attribute(flag_value_attr, "flags", "value"),
        ],
    };
    insert_catalog_attrs(&setup, config_app_id, &policy_catalog).await;
    let toggle_entity = Uuid::new_v4();
    let flag_entity = Uuid::new_v4();
    for (entity_id, attr_id, value, value_md5) in [
        (
            toggle_entity,
            toggle_setting_attr,
            json!("rate-limit-tx-based-on-conn-pool?"),
            "toggle-setting",
        ),
        (toggle_entity, toggled_attr, json!(true), "toggle-value"),
        (
            flag_entity,
            flag_setting_attr,
            json!("rate-limit-tx-based-on-conn-pool-buffer"),
            "flag-setting",
        ),
        (flag_entity, flag_value_attr, json!(2), "flag-value"),
    ] {
        setup
            .execute(
                "INSERT INTO triples
                   (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae)
                 VALUES ($1, $2, $3, $4, $5, true, false, false, false, false)",
                &[
                    &config_app_id,
                    &entity_id,
                    &attr_id,
                    &Json(value),
                    &value_md5,
                ],
            )
            .await
            .unwrap();
    }

    let observed_pool = PostgresPool::connect(&database_url, 3).await.unwrap();
    let config_pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let pressure = PostgresTransactionPoolPressurePolicy::from_pools(
        observed_pool.clone(),
        config_pool,
        Some("admin@example.test".to_owned()),
    )
    .with_config_app_id(config_app_id);
    let service = AdminDataService::from_pool(
        observed_pool.clone(),
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    )
    .with_transaction_pool_pressure_policy(Arc::new(pressure));
    let router = admin_data_router(Arc::new(service));
    let first_lease = observed_pool.acquire().await.unwrap();
    let second_lease = observed_pool.acquire().await.unwrap();

    let guest_entity = Uuid::new_v4();
    let rate_limited = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/transact")
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header("as-guest", "true")
                .body(Body::from(
                    json!({
                        "steps": [["add-triple", guest_entity, title_attr, "guest"]]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rate_limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &to_bytes(rate_limited.into_body(), usize::MAX)
                .await
                .unwrap()
        )
        .unwrap(),
        json!({
            "type": "rate-limited",
            "message": "You're making too many requests. Contact the deployment administrator at admin@example.test."
        })
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let admin_entity = Uuid::new_v4();
    let admin_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/transact")
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header(header::AUTHORIZATION, "Bearer admin-secret")
                .body(Body::from(
                    json!({
                        "steps": [["add-triple", admin_entity, title_attr, "admin"]]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(admin_response.status(), StatusCode::OK);

    drop(second_lease);
    let guest_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/transact")
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header("as-guest", "true")
                .body(Body::from(
                    json!({
                        "steps": [["add-triple", guest_entity, title_attr, "guest"]]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(guest_response.status(), StatusCode::OK);
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = ANY($2) AND attr_id = $3",
                &[&app_id, &&[admin_entity, guest_entity][..], &title_attr],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );

    setup
        .execute(
            "UPDATE rules SET code = $1 WHERE app_id = $2",
            &[
                &Json(json!({"posts": {"allow": {"create": "false"}}})),
                &app_id,
            ],
        )
        .await
        .unwrap();
    let denied_entity = Uuid::new_v4();
    let denied_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/transact")
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header("as-guest", "true")
                .body(Body::from(
                    json!({
                        "steps": [["add-triple", denied_entity, title_attr, "denied"]]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &to_bytes(denied_response.into_body(), usize::MAX)
                .await
                .unwrap()
        )
        .unwrap(),
        json!({
            "type": "permission-denied",
            "message": "Permission denied: not perms-pass?",
            "hint": {
                "input": ["posts", "object"],
                "expected": "perms-pass?"
            }
        })
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &denied_entity],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    drop(first_lease);
    drop(router);
    drop(setup);
    setup_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps cascade planning, HTTP denial, rollback, and commit together.
async fn permissioned_http_cascade_deletes_check_inherited_rule_params_for_children() {
    const ORACLE: &str =
        "instantdb/instant@4e0a1bd8 server/test/instant/db/transaction_test.clj:2212-2425";
    assert!(ORACLE.contains("4e0a1bd8"));
    let Some(database_url) = instant_test_support::test_database_url(
        "permissioned_http_cascade_deletes_check_inherited_rule_params_for_children",
    ) else {
        return;
    };
    let (mut migration_client, migration_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let migration_task = tokio::spawn(migration_connection);
    migrate_database(&mut migration_client).await.unwrap();
    drop(migration_client);
    migration_task.await.unwrap().unwrap();

    let database_url = install_shared_schema(&database_url).await;
    let (mut setup, setup_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    let app_id = Uuid::new_v4();
    setup
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();

    let post_id_attr = Uuid::new_v4();
    let post_handle_attr = Uuid::new_v4();
    let post_local_id_attr = Uuid::new_v4();
    let comment_id_attr = Uuid::new_v4();
    let comment_local_id_attr = Uuid::new_v4();
    let comment_post_attr = Uuid::new_v4();
    let mut relationship = named_attribute(comment_post_attr, "comments", "post");
    relationship.value_type = AttributeValueType::Ref;
    relationship.reverse_etype = Some("posts".to_owned());
    relationship.reverse_label = Some("comments".to_owned());
    relationship.on_delete = Some(DeleteAction::Cascade);
    let catalog = AttributeCatalog {
        attributes: vec![
            {
                let mut attribute = named_attribute(post_id_attr, "posts", "id");
                attribute.unique = true;
                attribute.indexed = true;
                attribute
            },
            {
                let mut attribute = named_attribute(post_handle_attr, "posts", "handle");
                attribute.unique = true;
                attribute.indexed = true;
                attribute
            },
            named_attribute(post_local_id_attr, "posts", "localId"),
            {
                let mut attribute = named_attribute(comment_id_attr, "comments", "id");
                attribute.unique = true;
                attribute.indexed = true;
                attribute
            },
            named_attribute(comment_local_id_attr, "comments", "localId"),
            relationship,
        ],
    };
    insert_catalog_attrs(&setup, app_id, &catalog).await;
    setup
        .execute(
            "UPDATE attrs
             SET reverse_ident = $1, reverse_etype = 'posts', reverse_label = 'comments',
                 on_delete = 'cascade'
             WHERE app_id = $2 AND id = $3",
            &[&Uuid::new_v4(), &app_id, &comment_post_attr],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({
                    "posts": {
                        "allow": {"delete": "isCreator"},
                        "bind": ["isCreator", "data.localId == ruleParams.localId"]
                    },
                    "comments": {
                        "allow": {"delete": "isCreator"},
                        "bind": ["isCreator", "data.localId == ruleParams.localId"]
                    }
                })),
            ],
        )
        .await
        .unwrap();

    let post_id = Uuid::new_v4();
    let comment_id = Uuid::new_v4();
    let seed = compile_transaction(
        &json!([
            ["add-triple", post_id, post_id_attr, post_id],
            ["add-triple", post_id, post_local_id_attr, "owner"],
            ["add-triple", comment_id, comment_id_attr, comment_id],
            ["add-triple", comment_id, comment_local_id_attr, "other"],
            ["add-triple", comment_id, comment_post_attr, post_id]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut setup,
        app_id,
        &catalog,
        &seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let transactions_before = setup
        .query_one(
            "SELECT count(*) FROM transactions WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);

    let pool = PostgresPool::connect(&database_url, 2).await.unwrap();
    let router = admin_data_router(Arc::new(AdminDataService::from_pool(
        pool,
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    )));
    let delete_steps = json!([
        ["delete-entity", post_id, "posts"],
        ["rule-params", post_id, "posts", {"localId": "owner"}]
    ]);
    let denied = post_guest_transaction(router.clone(), app_id, delete_steps.clone()).await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<Value>(&to_bytes(denied.into_body(), usize::MAX).await.unwrap())
            .unwrap(),
        json!({
            "type": "permission-denied",
            "message": "Permission denied: not perms-pass?",
            "hint": {"input": ["comments", "object"], "expected": "perms-pass?"}
        })
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(DISTINCT entity_id) FROM triples
                 WHERE app_id = $1 AND entity_id = ANY($2)",
                &[&app_id, &&[post_id, comment_id][..]],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );

    let repair = compile_transaction(
        &json!([["add-triple", comment_id, comment_local_id_attr, "owner"]]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut setup,
        app_id,
        &catalog,
        &repair,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let allowed = post_guest_transaction(router.clone(), app_id, delete_steps).await;
    assert_eq!(allowed.status(), StatusCode::OK);
    assert!(
        serde_json::from_slice::<Value>(&to_bytes(allowed.into_body(), usize::MAX).await.unwrap())
            .unwrap()["tx-id"]
            .is_i64()
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = ANY($2)",
                &[&app_id, &&[post_id, comment_id][..]],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let lookup_post_id = Uuid::new_v4();
    let lookup_comment_id = Uuid::new_v4();
    let lookup_seed = compile_transaction(
        &json!([
            ["add-triple", lookup_post_id, post_id_attr, lookup_post_id],
            [
                "add-triple",
                lookup_post_id,
                post_handle_attr,
                "lookup-post"
            ],
            ["add-triple", lookup_post_id, post_local_id_attr, "owner"],
            [
                "add-triple",
                lookup_comment_id,
                comment_id_attr,
                lookup_comment_id
            ],
            [
                "add-triple",
                lookup_comment_id,
                comment_local_id_attr,
                "owner"
            ],
            [
                "add-triple",
                lookup_comment_id,
                comment_post_attr,
                lookup_post_id
            ]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut setup,
        app_id,
        &catalog,
        &lookup_seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let lookup_delete = post_guest_transaction(
        router.clone(),
        app_id,
        json!([
            [
                "delete-entity",
                [post_handle_attr, "lookup-post"],
                "posts"
            ],
            [
                "rule-params",
                [post_handle_attr, "lookup-post"],
                "posts",
                {"localId": "owner"}
            ]
        ]),
    )
    .await;
    assert_eq!(lookup_delete.status(), StatusCode::OK);
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = ANY($2)",
                &[&app_id, &&[lookup_post_id, lookup_comment_id][..]],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let post_a = Uuid::new_v4();
    let post_b = Uuid::new_v4();
    let alice_comment_one = Uuid::new_v4();
    let alice_comment_two = Uuid::new_v4();
    let bob_comment_one = Uuid::new_v4();
    let bob_comment_two = Uuid::new_v4();
    let multi_seed = compile_transaction(
        &json!([
            ["add-triple", post_a, post_id_attr, post_a],
            ["add-triple", post_a, post_local_id_attr, "alice"],
            ["add-triple", post_b, post_id_attr, post_b],
            ["add-triple", post_b, post_local_id_attr, "bob"],
            [
                "add-triple",
                alice_comment_one,
                comment_id_attr,
                alice_comment_one
            ],
            [
                "add-triple",
                alice_comment_one,
                comment_local_id_attr,
                "alice"
            ],
            ["add-triple", alice_comment_one, comment_post_attr, post_a],
            [
                "add-triple",
                alice_comment_two,
                comment_id_attr,
                alice_comment_two
            ],
            [
                "add-triple",
                alice_comment_two,
                comment_local_id_attr,
                "alice"
            ],
            ["add-triple", alice_comment_two, comment_post_attr, post_a],
            [
                "add-triple",
                bob_comment_one,
                comment_id_attr,
                bob_comment_one
            ],
            ["add-triple", bob_comment_one, comment_local_id_attr, "bob"],
            ["add-triple", bob_comment_one, comment_post_attr, post_b],
            [
                "add-triple",
                bob_comment_two,
                comment_id_attr,
                bob_comment_two
            ],
            ["add-triple", bob_comment_two, comment_local_id_attr, "bob"],
            ["add-triple", bob_comment_two, comment_post_attr, post_b]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut setup,
        app_id,
        &catalog,
        &multi_seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let multi_entities = [
        post_a,
        post_b,
        alice_comment_one,
        alice_comment_two,
        bob_comment_one,
        bob_comment_two,
    ];
    let transactions_before_multi = setup
        .query_one(
            "SELECT count(*) FROM transactions WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    let wrong_multi = post_guest_transaction(
        router.clone(),
        app_id,
        json!([
            ["delete-entity", post_a, "posts"],
            ["rule-params", post_a, "posts", {"localId": "alice"}],
            ["delete-entity", post_b, "posts"],
            ["rule-params", post_b, "posts", {"localId": "alice"}]
        ]),
    )
    .await;
    assert_eq!(wrong_multi.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_multi
    );
    assert_eq!(
        setup
            .query_one(
                "SELECT count(DISTINCT entity_id) FROM triples
                 WHERE app_id = $1 AND entity_id = ANY($2)",
                &[&app_id, &&multi_entities[..]],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        6
    );
    let correct_multi = post_guest_transaction(
        router.clone(),
        app_id,
        json!([
            ["delete-entity", post_a, "posts"],
            ["rule-params", post_a, "posts", {"localId": "alice"}],
            ["delete-entity", post_b, "posts"],
            ["rule-params", post_b, "posts", {"localId": "bob"}]
        ]),
    )
    .await;
    assert_eq!(correct_multi.status(), StatusCode::OK);
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = ANY($2)",
                &[&app_id, &&multi_entities[..]],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    setup
        .execute(
            "UPDATE rules SET code = $1 WHERE app_id = $2",
            &[
                &Json(json!({
                    "posts": {"allow": {"delete": true}},
                    "comments": {
                        "allow": {"delete": "data.localId == ruleParams.localId"}
                    }
                })),
                &app_id,
            ],
        )
        .await
        .unwrap();
    let cross_namespace_post = Uuid::new_v4();
    let cross_namespace_comment = Uuid::new_v4();
    let cross_namespace_seed = compile_transaction(
        &json!([
            [
                "add-triple",
                cross_namespace_post,
                post_id_attr,
                cross_namespace_post
            ],
            [
                "add-triple",
                cross_namespace_comment,
                comment_id_attr,
                cross_namespace_comment
            ],
            [
                "add-triple",
                cross_namespace_comment,
                comment_local_id_attr,
                "owner"
            ],
            [
                "add-triple",
                cross_namespace_comment,
                comment_post_attr,
                cross_namespace_post
            ]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut setup,
        app_id,
        &catalog,
        &cross_namespace_seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let cross_namespace_delete = post_guest_transaction(
        router.clone(),
        app_id,
        json!([
            ["delete-entity", cross_namespace_post, "posts"],
            [
                "rule-params",
                cross_namespace_post,
                "different-namespace",
                {"localId": "owner"}
            ]
        ]),
    )
    .await;
    assert_eq!(cross_namespace_delete.status(), StatusCode::OK);
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = ANY($2)",
                &[
                    &app_id,
                    &&[cross_namespace_post, cross_namespace_comment][..],
                ],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    drop(router);
    drop(setup);
    setup_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Covers explicit and generated create identities at the HTTP boundary.
async fn permissioned_http_create_projection_matches_scalar_and_lookup_identity_rules() {
    const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/src/instant/db/permissioned_transaction.clj post-create-checks";
    assert!(ORACLE.contains("4e0a1bd8"));
    let Some(database_url) = instant_test_support::test_database_url(
        "permissioned_http_create_projection_matches_scalar_and_lookup_identity_rules",
    ) else {
        return;
    };
    let (mut migration_client, migration_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let migration_task = tokio::spawn(migration_connection);
    migrate_database(&mut migration_client).await.unwrap();
    drop(migration_client);
    migration_task.await.unwrap().unwrap();

    let database_url = install_shared_schema(&database_url).await;
    let (setup, setup_connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    let app_id = Uuid::new_v4();
    setup
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    let handle_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![
            {
                let mut attribute = attribute(handle_attr, "handle", true);
                attribute.indexed = true;
                attribute
            },
            attribute(title_attr, "title", false),
        ],
    };
    insert_catalog_attrs(&setup, app_id, &catalog).await;
    setup
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({
                    "posts": {"allow": {"create": "newData.id == ruleParams.id"}}
                })),
            ],
        )
        .await
        .unwrap();
    let pool = PostgresPool::connect(&database_url, 2).await.unwrap();
    let router = admin_data_router(Arc::new(AdminDataService::from_pool(
        pool,
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    )));

    let explicit_id = Uuid::new_v4();
    let explicit = post_guest_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", explicit_id, title_attr, "explicit"],
            ["rule-params", explicit_id, "posts", {"id": explicit_id}]
        ]),
    )
    .await;
    assert_eq!(explicit.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &explicit_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    setup
        .execute(
            "UPDATE rules SET code = $1 WHERE app_id = $2",
            &[
                &Json(json!({
                    "posts": {"allow": {"create": "newData.handle == 'lookup-created' && request.modifiedFields == ['title']"}}
                })),
                &app_id,
            ],
        )
        .await
        .unwrap();
    let lookup_value = "lookup-created";
    let lookup = post_guest_transaction(
        router.clone(),
        app_id,
        json!([[
            "add-triple",
            [handle_attr, lookup_value],
            title_attr,
            "lookup"
        ]]),
    )
    .await;
    assert_eq!(lookup.status(), StatusCode::OK);
    assert!(
        serde_json::from_slice::<Value>(&to_bytes(lookup.into_body(), usize::MAX).await.unwrap())
            .unwrap()["tx-id"]
            .is_i64()
    );
    let lookup_entity_id = setup
        .query_one(
            "SELECT entity_id FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = $3 AND av",
            &[&app_id, &handle_attr, &Json(json!(lookup_value))],
        )
        .await
        .unwrap()
        .get::<_, Uuid>(0);
    assert_eq!(
        setup
            .query_one(
                "SELECT value FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &lookup_entity_id, &title_attr],
            )
            .await
            .unwrap()
            .get::<_, Json<Value>>(0)
            .0,
        json!("lookup")
    );

    drop(router);
    drop(setup);
    setup_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps persisted rules, query filtering, and create checks at the HTTP boundary.
async fn permissioned_http_persisted_type_rule_filters_queries_and_creates() {
    let Some(database_url) = instant_test_support::test_database_url(
        "permissioned_http_persisted_type_rule_filters_queries_and_creates",
    ) else {
        return;
    };
    let (mut migration_client, migration_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let migration_task = tokio::spawn(migration_connection);
    migrate_database(&mut migration_client).await.unwrap();
    drop(migration_client);
    migration_task.await.unwrap().unwrap();

    let database_url = install_shared_schema(&database_url).await;
    let (mut setup, setup_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    let app_id = Uuid::new_v4();
    setup
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    let id_attr = Uuid::new_v4();
    let rank_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(id_attr, "id", true),
            attribute(rank_attr, "rank", false),
        ],
    };
    insert_catalog_attrs(&setup, app_id, &catalog).await;
    setup
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({
                    "posts": {"allow": {
                        "view": "type(data.rank) == int",
                        "create": "type(newData.rank) == int"
                    }}
                })),
            ],
        )
        .await
        .unwrap();

    let integer_entity = Uuid::new_v4();
    let string_entity = Uuid::new_v4();
    let seed = compile_transaction(
        &json!([
            ["add-triple", integer_entity, id_attr, integer_entity],
            ["add-triple", integer_entity, rank_attr, 7],
            ["add-triple", string_entity, id_attr, string_entity],
            ["add-triple", string_entity, rank_attr, "7"]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut setup,
        app_id,
        &catalog,
        &seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();

    let pool = PostgresPool::connect(&database_url, 2).await.unwrap();
    let router = admin_data_router(Arc::new(AdminDataService::from_pool(
        pool,
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    )));
    let query_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/query")
                .header(header::CONTENT_TYPE, "application/json")
                .header("app-id", app_id.to_string())
                .header("as-guest", "true")
                .body(Body::from(json!({"query": {"posts": {}}}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(query_response.status(), StatusCode::OK);
    let query_body: Value = serde_json::from_slice(
        &to_bytes(query_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        query_body["posts"],
        json!([{"id": integer_entity, "rank": 7}])
    );

    let allowed_entity = Uuid::new_v4();
    let allowed = post_guest_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", allowed_entity, id_attr, allowed_entity],
            ["add-triple", allowed_entity, rank_attr, 8]
        ]),
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::OK);

    let denied_entity = Uuid::new_v4();
    let denied = post_guest_transaction(
        router.clone(),
        app_id,
        json!([
            ["add-triple", denied_entity, id_attr, denied_entity],
            ["add-triple", denied_entity, rank_attr, "8"]
        ]),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        setup
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &denied_entity],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    drop(router);
    drop(setup);
    setup_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the source-pinned runtime envelope and rollback proof together.
async fn runtime_permissioned_link_validation_preserves_source_order_without_residue() {
    const ORACLE: &str =
        "instantdb/instant@4e0a1bd8 server/src/instant/db/permissioned_transaction.clj:112-122";
    assert!(ORACLE.contains("4e0a1bd8"));
    let Some(database_url) = instant_test_support::test_database_url(
        "runtime_permissioned_link_validation_preserves_source_order_without_residue",
    ) else {
        return;
    };
    let (mut migration_client, migration_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let migration_task = tokio::spawn(migration_connection);
    migrate_database(&mut migration_client).await.unwrap();
    drop(migration_client);
    migration_task.await.unwrap().unwrap();
    let database_url = install_shared_schema(&database_url).await;
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let (verifier, verifier_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let verifier_task = tokio::spawn(verifier_connection);

    let app_id = Uuid::new_v4();
    client
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let author_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(id_attr, "id", true),
            attribute(title_attr, "title", false),
            Attribute {
                id: author_attr,
                wire: None,
                etype: "posts".to_owned(),
                label: "author".to_owned(),
                value_type: AttributeValueType::Ref,
                cardinality: Cardinality::One,
                unique: false,
                setting_unique: false,
                indexed: false,
                required: false,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: Some("users".to_owned()),
                reverse_label: Some("posts".to_owned()),
                on_delete: None,
                on_delete_reverse: None,
            },
        ],
    };
    insert_catalog_attrs(&client, app_id, &catalog).await;
    client
        .execute(
            "UPDATE attrs
             SET reverse_ident = $1, reverse_etype = 'users', reverse_label = 'posts'
             WHERE app_id = $2 AND id = $3",
            &[&Uuid::new_v4(), &app_id, &author_attr],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &json!({
                    "posts": {"allow": {"update": "true"}},
                    "users": {"allow": {"update": "true"}}
                }),
            ],
        )
        .await
        .unwrap();

    let post_id = Uuid::new_v4();
    let seed = compile_transaction(
        &json!([
            ["add-triple", post_id, id_attr, post_id],
            ["add-triple", post_id, title_attr, "before"]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();

    let transactions_before = verifier
        .query_one(
            "SELECT count(*) FROM transactions WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    let authorizer = Arc::new(TestAuthorizer {
        catalog: catalog.clone(),
    });
    let data = Arc::new(AdminDataService::new(client, Arc::clone(&authorizer)));
    let service = RuntimeRealtimeService::new(authorizer, data, Uuid::new_v4());
    let mut session = service
        .open(Some(app_id), RuntimeTransport::WebSocket)
        .await
        .unwrap();
    service
        .message(session.session_id, json!({"op": "init", "app-id": app_id}))
        .await
        .unwrap();
    assert_eq!(session.recv().await.unwrap()["op"], "init-ok");

    let event = json!({
        "op": "transact",
        "client-event-id": "invalid-reference-retract",
        "tx-steps": [
            ["add-triple", post_id, title_attr, "must-roll-back"],
            ["deep-merge-triple", post_id, author_attr, "source-first-invalid"],
            ["add-triple", post_id, author_attr, "source-second-invalid"]
        ]
    });
    service
        .message(session.session_id, event.clone())
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), session.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        error,
        json!({
            "op": "error",
            "status": 400,
            "client-event-id": "invalid-reference-retract",
            "original-event": event,
            "type": "validation-failed",
            "message": "Validation failed for eid: Expected link value to be a uuid.",
            "hint": {
                "data-type": "eid",
                "input": "source-first-invalid",
                "errors": [{
                    "message": "Expected link value to be a uuid.",
                    "hint": {
                        "tx-step": [
                            "deep-merge-triple",
                            post_id,
                            author_attr,
                            "source-first-invalid",
                            null
                        ]
                    }
                }]
            }
        })
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT value FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &post_id, &title_attr],
            )
            .await
            .unwrap()
            .get::<_, tokio_postgres::types::Json<Value>>(0)
            .0,
        json!("before")
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM transactions WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before
    );
    assert_eq!(
        verifier
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &post_id, &author_attr],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    drop(service);
    drop(verifier);
    verifier_task.await.unwrap().unwrap();
    connection_task.abort();
}

#[tokio::test]
async fn admin_presence_reads_the_shared_room_store_and_keeps_missing_users_null() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_presence_reads_the_shared_room_store_and_keeps_missing_users_null",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let app_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let rooms = Arc::new(Mutex::new(RoomStore::default()));
    rooms
        .lock()
        .unwrap()
        .join(
            app_id,
            "lobby",
            session_id,
            "instance-a",
            None,
            json!({"cursor": 7}),
        )
        .unwrap();
    let service = AdminPresenceService::new(
        client,
        Arc::new(TestAuthorizer {
            catalog: system_attribute_catalog().unwrap(),
        }),
        rooms,
    );
    let response = service
        .presence(
            context(app_id, AdminActor::Admin),
            "chat".to_owned(),
            "lobby".to_owned(),
        )
        .await
        .unwrap();
    let session = &response.sessions.unwrap()[&session_id.to_string()];
    assert_eq!(session["peer-id"], json!(session_id));
    assert_eq!(session["instance-id"], json!("instance-a"));
    assert_eq!(session["user"], Value::Null);
    assert_eq!(session["data"], json!({"cursor": 7}));

    drop(service);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn admin_storage_coordinates_object_bytes_and_file_metadata() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_storage_coordinates_object_bytes_and_file_metadata",
    ) else {
        return;
    };
    let database_url = install_shared_schema(&database_url).await;
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);

    let app_id = Uuid::new_v4();
    let catalog = system_attribute_catalog().unwrap();
    insert_catalog_attrs(&client, app_id, &catalog).await;
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &json!({
                    "$rateLimits": {"storage": {"limits": [{"capacity": 10}]}},
                    "$files": {"allow": {
                    "create": "rateLimit.storage.limit('global')",
                    "delete": "true",
                    "view": "true"
                }}}),
            ],
        )
        .await
        .unwrap();
    let objects = Arc::new(TestObjectStore::default());
    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let authorizer = Arc::new(RecordingAuthorizer {
        catalog: catalog.clone(),
        scopes: Mutex::new(Vec::new()),
        deny_writes: std::sync::atomic::AtomicBool::new(false),
    });
    let service = Arc::new(AdminStorageService::from_pool(
        pool,
        Arc::clone(&authorizer),
        Arc::clone(&objects),
        "https://api.test/",
    ));
    drop(client);
    let request_context = context(app_id, AdminActor::Admin);

    let uploaded = service
        .upload(
            request_context.clone(),
            "avatars/me.png".to_owned(),
            Some("image/png".to_owned()),
            Some("inline".to_owned()),
            Some(3),
            Body::from("png"),
        )
        .await
        .unwrap();
    let file_id = uploaded["id"].as_str().unwrap();
    assert_eq!(uploaded["size"], json!(3));
    assert_eq!(objects.objects.lock().unwrap().len(), 1);
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite]
    );

    authorizer.scopes.lock().unwrap().clear();
    let deleted = service
        .delete_files(request_context.clone(), Vec::new())
        .await
        .unwrap();
    assert_eq!(deleted["ids"], json!([]));
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite]
    );

    authorizer.scopes.lock().unwrap().clear();
    let deleted = service
        .delete_files(
            request_context.clone(),
            vec![
                "avatars/missing-1.png".to_owned(),
                "avatars/missing-2.png".to_owned(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(deleted["ids"], json!([]));
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite]
    );

    authorizer.scopes.lock().unwrap().clear();
    let files = service.list_files(request_context.clone()).await.unwrap();
    assert_eq!(files.as_array().unwrap().len(), 1);
    assert_eq!(files[0]["name"], json!("avatars/me.png"));
    assert_eq!(files[0]["size"], json!(3));
    assert!(
        files[0]["key"]
            .as_str()
            .unwrap()
            .ends_with(uploaded["location-id"].as_str().unwrap())
    );
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite, AdminDataScope::Read]
    );

    authorizer.scopes.lock().unwrap().clear();
    let download_url = service
        .create_download_url(request_context.clone(), "avatars/me.png".to_owned())
        .await
        .unwrap();
    assert!(
        download_url
            .as_str()
            .unwrap()
            .starts_with("https://objects.test/")
    );
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageRead]
    );
    authorizer.scopes.lock().unwrap().clear();
    let upload_url = service
        .create_upload_url(request_context.clone(), "avatars/next.png".to_owned())
        .await
        .unwrap();
    assert!(
        upload_url
            .as_str()
            .unwrap()
            .starts_with("https://api.test/storage/")
    );
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite]
    );

    authorizer.scopes.lock().unwrap().clear();
    let deleted = service
        .delete_file(request_context.clone(), "avatars/me.png".to_owned())
        .await
        .unwrap();
    assert_eq!(deleted["id"], json!(file_id));
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite]
    );
    authorizer.scopes.lock().unwrap().clear();
    assert_eq!(
        service
            .list_files(request_context.clone())
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite, AdminDataScope::Read]
    );
    assert_eq!(objects.objects.lock().unwrap().len(), 1);

    let runtime_context = RuntimeStorageContext {
        app_id,
        refresh_token: None,
        client_ip: None,
    };
    let runtime_upload = instant_http::RuntimeStorageBackend::upload(
        &*service,
        runtime_context.clone(),
        "public/readme.txt".to_owned(),
        Some("text/plain".to_owned()),
        None,
        Some(5),
        Body::from("hello"),
    )
    .await
    .unwrap();
    assert_eq!(runtime_upload["size"], 5);
    let runtime_download = instant_http::RuntimeStorageBackend::create_download_url(
        &*service,
        runtime_context.clone(),
        "public/readme.txt".to_owned(),
    )
    .await
    .unwrap();
    assert!(
        runtime_download
            .as_str()
            .unwrap()
            .starts_with("https://objects.test/")
    );

    let (query_client, query_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let query_connection_task = tokio::spawn(query_connection);
    let query_service = AdminDataService::new(
        query_client,
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
    )
    .with_file_url_signer(Arc::new(StorageFileUrlSigner::new(Arc::clone(&objects))));
    let query = serde_json::from_value(json!({
        "$files": {"$": {"fields": ["url"]}}
    }))
    .unwrap();
    let query_result = query_service
        .query(
            context(app_id, AdminActor::Guest),
            AdminQueryRequest {
                query,
                inference: false,
                versions: None,
            },
        )
        .await
        .unwrap();
    let queried_file = &query_result["$files"][0];
    assert!(
        queried_file["url"]
            .as_str()
            .unwrap()
            .starts_with("https://objects.test/")
    );
    assert!(queried_file.get("location-id").is_none());

    let (rules_client, rules_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let rules_connection_task = tokio::spawn(rules_connection);
    rules_client
        .execute(
            "UPDATE rules SET code = $2 WHERE app_id = $1",
            &[
                &app_id,
                &json!({
                    "$rateLimits": {"storage": {"limits": [{"capacity": 10}]}},
                    "$files": {
                        "allow": {
                            "create": "rateLimit.storage.limit('global')",
                            "delete": "true",
                            "view": "true"
                        },
                        "fields": {"url": "false"}
                    }
                }),
            ],
        )
        .await
        .unwrap();
    drop(rules_client);
    rules_connection_task.await.unwrap().unwrap();

    let denied = RuntimeJoinRowsQueryBackend::query_join_rows(
        &query_service,
        context(app_id, AdminActor::Guest),
        AdminQueryRequest {
            query: serde_json::from_value(json!({
                "$files": {"$": {"fields": ["url"]}}
            }))
            .unwrap(),
            inference: false,
            versions: None,
        },
    )
    .await
    .unwrap();
    let url_attr_id = catalog.forward("$files", "url").unwrap().id;
    let triples = denied.result[0]["data"]["datalog-result"]["join-rows"][0]
        .as_array()
        .unwrap();
    assert!(
        triples.iter().all(|triple| triple[1] != json!(url_attr_id)),
        "field-level permissions must filter virtual $files.url triples"
    );
    drop(query_service);
    query_connection_task.await.unwrap().unwrap();

    let legacy_url = instant_http::RuntimeStorageBackend::create_upload_url(
        &*service,
        runtime_context.clone(),
        "public/legacy.bin".to_owned(),
    )
    .await
    .unwrap();
    let upload_id = legacy_url
        .as_str()
        .unwrap()
        .split('/')
        .rev()
        .nth(1)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap();
    let consumed = instant_http::RuntimeStorageBackend::consume_upload_url(
        &*service,
        upload_id,
        "application/octet-stream".to_owned(),
        Some(3),
        Body::from("bin"),
    )
    .await
    .unwrap();
    assert_eq!(consumed["size"], 3);
    let reused = instant_http::RuntimeStorageBackend::consume_upload_url(
        &*service,
        upload_id,
        "application/octet-stream".to_owned(),
        Some(3),
        Body::from("bin"),
    )
    .await
    .unwrap_err();
    assert_eq!(reused.body().kind, ApiErrorType::ValidationFailed);

    let runtime_deleted = instant_http::RuntimeStorageBackend::delete_file(
        &*service,
        runtime_context,
        "public/readme.txt".to_owned(),
    )
    .await
    .unwrap();
    assert_eq!(runtime_deleted["id"], runtime_upload["id"]);
    assert_eq!(
        service
            .list_files(request_context)
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let dashboard =
        DashboardStorageControlHooks::new(NoopDashboardControlHooks, Arc::clone(&service));
    let uploaded = dashboard
        .execute(DashboardControlHookOperation::UploadDashboardFile {
            app_id,
            path: "dashboard/report.txt".to_owned(),
            content_type: Some("text/plain".to_owned()),
            content_length: Some(6),
            body: b"report".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(uploaded["size"], json!(6));
    let deleted = dashboard
        .execute(DashboardControlHookOperation::DeleteDashboardFiles {
            app_id,
            paths: vec!["dashboard/report.txt".to_owned()],
        })
        .await
        .unwrap();
    assert_eq!(deleted["ids"].as_array().unwrap().len(), 1);

    drop(dashboard);
    drop(service);
    connection_task.await.unwrap().unwrap();
}

struct QuotaStoragePolicy {
    quota: Option<u64>,
}

impl StorageAccessPolicy for QuotaStoragePolicy {
    fn authorize(&self, _app_id: Uuid) -> impl Future<Output = Result<(), ApiError>> + Send {
        std::future::ready(Ok(()))
    }

    fn detect_content_type(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send {
        std::future::ready(Ok(false))
    }

    fn storage_quota_bytes(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<Option<u64>, ApiError>> + Send {
        std::future::ready(Ok(self.quota))
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Covers the cap, content policy and quota paths end to end.
async fn admin_storage_caps_uploads_sanitizes_types_and_enforces_quota() {
    let Some(database_url) = instant_test_support::test_database_url(
        "admin_storage_caps_uploads_sanitizes_types_and_enforces_quota",
    ) else {
        return;
    };
    let database_url = install_shared_schema(&database_url).await;
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);

    let app_id = Uuid::new_v4();
    let catalog = system_attribute_catalog().unwrap();
    insert_catalog_attrs(&client, app_id, &catalog).await;
    client
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    drop(client);

    let objects = Arc::new(TestObjectStore::default());
    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let authorizer = Arc::new(RecordingAuthorizer {
        catalog,
        scopes: Mutex::new(Vec::new()),
        deny_writes: std::sync::atomic::AtomicBool::new(false),
    });
    let service = Arc::new(
        AdminStorageService::from_pool(pool, authorizer, Arc::clone(&objects), "https://api.test/")
            .with_storage_policy(Arc::new(QuotaStoragePolicy { quota: Some(8) }))
            .with_max_upload_bytes(4),
    );
    let request_context = context(app_id, AdminActor::Admin);

    let error = service
        .upload(
            request_context.clone(),
            "declared.bin".to_owned(),
            None,
            None,
            Some(5),
            Body::from("12345"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(error.body().kind, ApiErrorType::ParameterLimitExceeded);

    let error = service
        .upload(
            request_context.clone(),
            "streamed.bin".to_owned(),
            None,
            None,
            None,
            Body::from("12345"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(objects.objects.lock().unwrap().is_empty());

    for content_length in [Some(5), None] {
        let upload_url = service
            .create_upload_url(request_context.clone(), "legacy.bin".to_owned())
            .await
            .unwrap();
        let upload_id = upload_url
            .as_str()
            .unwrap()
            .split('/')
            .rev()
            .nth(1)
            .and_then(|value| Uuid::parse_str(value).ok())
            .unwrap();
        let error = instant_http::RuntimeStorageBackend::consume_upload_url(
            &*service,
            upload_id,
            "application/octet-stream".to_owned(),
            content_length,
            Body::from("12345"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
    assert!(objects.objects.lock().unwrap().is_empty());

    service
        .upload(
            request_context.clone(),
            "page.html".to_owned(),
            Some("text/html".to_owned()),
            Some("inline".to_owned()),
            Some(3),
            Body::from("abc"),
        )
        .await
        .unwrap();
    service
        .upload(
            request_context.clone(),
            "photo.png".to_owned(),
            Some("Image/PNG".to_owned()),
            Some("inline".to_owned()),
            Some(3),
            Body::from("png"),
        )
        .await
        .unwrap();
    let puts = objects.puts.lock().unwrap().clone();
    assert_eq!(puts.len(), 2);
    assert_eq!(
        puts[0].content_type.as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(puts[0].content_disposition.as_deref(), Some("attachment"));
    assert_eq!(puts[1].content_type.as_deref(), Some("image/png"));
    assert_eq!(puts[1].content_disposition.as_deref(), Some("inline"));

    // Six of the eight quota bytes are already used, so a three-byte upload is
    // refused before it reaches the object store.
    let (usage_client, usage_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let usage_connection_task = tokio::spawn(usage_connection);
    usage_client
        .execute(
            "INSERT INTO triples_size_aggregate (app_id, attr_id, pg_size, files_size)
             VALUES ($1, $2, 0, 6)",
            &[&app_id, &Uuid::new_v4()],
        )
        .await
        .unwrap();
    drop(usage_client);
    usage_connection_task.await.unwrap().unwrap();

    let error = service
        .upload(
            request_context.clone(),
            "over-quota.bin".to_owned(),
            None,
            None,
            Some(3),
            Body::from("123"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::FORBIDDEN);
    assert_eq!(error.body().kind, ApiErrorType::PermissionDenied);
    assert_eq!(
        error.body().hint,
        Some(json!({
            "quota-bytes": 8,
            "used-bytes": 6,
            "requested-bytes": 3,
        }))
    );
    assert_eq!(objects.objects.lock().unwrap().len(), 2);

    // The signed-upload consume route shares the same quota check.
    let upload_url = service
        .create_upload_url(request_context.clone(), "over-quota-signed.bin".to_owned())
        .await
        .unwrap();
    let upload_id = upload_url
        .as_str()
        .unwrap()
        .split('/')
        .rev()
        .nth(1)
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap();
    let error = instant_http::RuntimeStorageBackend::consume_upload_url(
        &*service,
        upload_id,
        "application/octet-stream".to_owned(),
        Some(3),
        Body::from("123"),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status(), StatusCode::FORBIDDEN);
    assert_eq!(objects.objects.lock().unwrap().len(), 2);

    let uploaded = service
        .upload(
            request_context,
            "at-quota.bin".to_owned(),
            None,
            None,
            Some(2),
            Body::from("12"),
        )
        .await
        .unwrap();
    assert_eq!(uploaded["size"], json!(2));
    drop(service);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn legacy_admin_storage_list_checks_live_postgres_status_after_both_scopes() {
    let Some(database_url) = instant_test_support::test_database_url(
        "legacy_admin_storage_list_checks_live_postgres_status_after_both_scopes",
    ) else {
        return;
    };
    let database_url = install_shared_schema(&database_url).await;
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO apps (id, status) VALUES ($1, 'disabled')",
            &[&app_id],
        )
        .await
        .unwrap();

    let catalog = system_attribute_catalog().unwrap();
    let authorizer = Arc::new(RecordingAuthorizer {
        catalog,
        scopes: Mutex::new(Vec::new()),
        deny_writes: std::sync::atomic::AtomicBool::new(false),
    });
    let objects = Arc::new(TestObjectStore::default());
    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let status_policy = Arc::new(PostgresEffectiveAppStatusPolicy::from_pool(pool.clone()));
    let service =
        AdminStorageService::from_pool(pool, Arc::clone(&authorizer), objects, "https://api.test/")
            .with_effective_app_status_policy(status_policy as Arc<dyn EffectiveAppStatusPolicy>);

    let error = service
        .list_files(context(app_id, AdminActor::Admin))
        .await
        .unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::AppDisabled);
    assert_eq!(
        authorizer.scopes.lock().unwrap().as_slice(),
        &[AdminDataScope::StorageWrite, AdminDataScope::Read]
    );

    drop(service);
    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn runtime_stream_persists_identity_parts_and_completion() {
    let Some(database_url) = instant_test_support::test_database_url(
        "runtime_stream_persists_identity_parts_and_completion",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    install_schema(&client).await;

    let app_id = Uuid::new_v4();
    let catalog = system_attribute_catalog().unwrap();
    insert_catalog_attrs(&client, app_id, &catalog).await;
    let objects = Arc::new(TestObjectStore::default());
    let backend = PostgresRuntimeStreamBackend::new(
        client,
        Arc::new(TestAuthorizer { catalog }),
        Arc::clone(&objects),
    );
    let request_context = context(app_id, AdminActor::Admin);
    let token = Uuid::new_v4();
    let machine_id = Uuid::new_v4();
    let started = backend
        .start_stream(RuntimeStreamStartRequest {
            context: request_context.clone(),
            client_id: "durable-client".to_owned(),
            reconnect_token: token,
            machine_id,
        })
        .await
        .unwrap();
    assert_eq!(started.size, 0);

    let first = backend
        .append_stream(RuntimeStreamAppendRequest {
            context: request_context.clone(),
            stream_id: started.stream_id,
            machine_id,
            expected_offset: 0,
            chunks: vec!["hel".to_owned(), "lo".to_owned()],
            done: false,
            abort_reason: None,
        })
        .await
        .unwrap();
    assert_eq!(first.new_offset, 5);
    assert_eq!(first.flushed_offset, None);
    assert!(objects.objects.lock().unwrap().is_empty());
    let loaded = backend
        .load_stream(RuntimeStreamLoadRequest {
            context: request_context.clone(),
            stream_id: Some(started.stream_id),
            client_id: None,
            requested_offset: 2,
        })
        .await
        .unwrap();
    assert_eq!(loaded.size, 5);
    assert_eq!(loaded.replay_offset, 0);
    assert!(loaded.files.is_empty());
    assert_eq!(loaded.content, "hello");
    backend
        .disconnect_stream(RuntimeStreamDisconnectRequest {
            context: request_context.clone(),
            stream_id: started.stream_id,
            machine_id,
        })
        .await
        .unwrap();
    assert_eq!(objects.objects.lock().unwrap().len(), 1);

    let replacement_machine = Uuid::new_v4();
    let resumed = backend
        .start_stream(RuntimeStreamStartRequest {
            context: request_context.clone(),
            client_id: "durable-client".to_owned(),
            reconnect_token: token,
            machine_id: replacement_machine,
        })
        .await
        .unwrap();
    assert_eq!(resumed.stream_id, started.stream_id);
    assert_eq!(resumed.machine_id, replacement_machine);
    assert_eq!(resumed.size, 5);
    assert_eq!(resumed.content, "");
    assert_eq!(resumed.files.len(), 1);

    backend
        .append_stream(RuntimeStreamAppendRequest {
            context: request_context.clone(),
            stream_id: started.stream_id,
            machine_id: replacement_machine,
            expected_offset: 5,
            chunks: vec!["!".to_owned()],
            done: false,
            abort_reason: None,
        })
        .await
        .unwrap();
    let completion = backend
        .append_stream(RuntimeStreamAppendRequest {
            context: request_context.clone(),
            stream_id: started.stream_id,
            machine_id: replacement_machine,
            expected_offset: 6,
            chunks: Vec::new(),
            done: true,
            abort_reason: None,
        })
        .await
        .unwrap();
    assert!(completion.done);
    assert_eq!(completion.new_offset, 6);
    assert_eq!(completion.flushed_offset, Some(6));
    let completed = backend
        .load_stream(RuntimeStreamLoadRequest {
            context: request_context.clone(),
            stream_id: None,
            client_id: Some("durable-client".to_owned()),
            requested_offset: 5,
        })
        .await
        .unwrap();
    assert!(completed.done);
    assert_eq!(completed.size, 6);
    assert_eq!(completed.replay_offset, 5);
    assert_eq!(completed.files.len(), 1);
    assert_eq!(completed.files[0].size, 1);
    assert!(completed.files[0].url.starts_with("https://objects.test/"));
    assert_eq!(objects.objects.lock().unwrap().len(), 2);

    let invalid_token = backend
        .start_stream(RuntimeStreamStartRequest {
            context: request_context,
            client_id: "durable-client".to_owned(),
            reconnect_token: Uuid::new_v4(),
            machine_id: Uuid::new_v4(),
        })
        .await
        .unwrap_err();
    assert_eq!(invalid_token.body().kind, ApiErrorType::ValidationFailed);

    drop(backend);
    connection_task.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One scenario exercises a complete two-owner handoff.
async fn runtime_stream_replacement_fences_stale_writer_and_preserves_shared_buffer() {
    let Some(database_url) = instant_test_support::test_database_url(
        "runtime_stream_replacement_fences_stale_writer_and_preserves_shared_buffer",
    ) else {
        return;
    };
    let database_url = install_shared_schema(&database_url).await;
    let (first_client, first_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let (second_client, second_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let first_connection = tokio::spawn(first_connection);
    let second_connection = tokio::spawn(second_connection);
    let catalog = system_attribute_catalog().unwrap();
    let app_id = Uuid::new_v4();
    insert_catalog_attrs(&first_client, app_id, &catalog).await;
    let objects = Arc::new(TestObjectStore::default());
    let first = PostgresRuntimeStreamBackend::new(
        first_client,
        Arc::new(TestAuthorizer {
            catalog: catalog.clone(),
        }),
        Arc::clone(&objects),
    );
    let second = PostgresRuntimeStreamBackend::new(
        second_client,
        Arc::new(TestAuthorizer { catalog }),
        Arc::clone(&objects),
    );
    let request_context = context(app_id, AdminActor::Admin);
    let token = Uuid::new_v4();
    let first_machine = Uuid::new_v4();
    let second_machine = Uuid::new_v4();
    let started = first
        .start_stream(RuntimeStreamStartRequest {
            context: request_context.clone(),
            client_id: "failover-stream".to_owned(),
            reconnect_token: token,
            machine_id: first_machine,
        })
        .await
        .unwrap();
    first
        .append_stream(RuntimeStreamAppendRequest {
            context: request_context.clone(),
            stream_id: started.stream_id,
            machine_id: first_machine,
            expected_offset: 0,
            chunks: vec!["old".to_owned()],
            done: false,
            abort_reason: None,
        })
        .await
        .unwrap();

    let resumed = second
        .start_stream(RuntimeStreamStartRequest {
            context: request_context.clone(),
            client_id: "failover-stream".to_owned(),
            reconnect_token: token,
            machine_id: second_machine,
        })
        .await
        .unwrap();
    assert_eq!(resumed.stream_id, started.stream_id);
    assert_eq!(resumed.size, 3);
    assert_eq!(resumed.content, "old");

    let stale = first
        .append_stream(RuntimeStreamAppendRequest {
            context: request_context.clone(),
            stream_id: started.stream_id,
            machine_id: first_machine,
            expected_offset: 3,
            chunks: vec!["lost".to_owned()],
            done: false,
            abort_reason: None,
        })
        .await
        .unwrap_err();
    assert_eq!(stale.body().kind, ApiErrorType::ValidationFailed);

    second
        .append_stream(RuntimeStreamAppendRequest {
            context: request_context.clone(),
            stream_id: started.stream_id,
            machine_id: second_machine,
            expected_offset: 3,
            chunks: vec!["new".to_owned()],
            done: true,
            abort_reason: None,
        })
        .await
        .unwrap();
    let completed = second
        .load_stream(RuntimeStreamLoadRequest {
            context: request_context,
            stream_id: Some(started.stream_id),
            client_id: None,
            requested_offset: 0,
        })
        .await
        .unwrap();
    assert!(completed.done);
    assert_eq!(completed.size, 6);
    assert_eq!(completed.files.len(), 1);
    assert!(
        objects
            .objects
            .lock()
            .unwrap()
            .values()
            .any(|value| value == b"oldnew")
    );

    drop(first);
    drop(second);
    first_connection.await.unwrap().unwrap();
    second_connection.await.unwrap().unwrap();
}

// File signing is a real await between query execution and permission preload.
// Use it as a deterministic barrier without adding a production test hook.
struct SnapshotBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl instant_instaql::AsyncFileUrlSigner for SnapshotBarrier {
    fn signed_download_urls(
        &self,
        _app_id: Uuid,
        locations: Vec<String>,
    ) -> instant_instaql::AsyncFileUrlBatchFuture<'_> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
            Ok(locations
                .iter()
                .map(|_| "https://objects.test/file".to_owned())
                .collect())
        })
    }
}

#[tokio::test]
async fn permission_query_keeps_entities_visible_after_concurrent_delete() {
    let Some(database_url) = instant_test_support::test_database_url(
        "permission_query_keeps_entities_visible_after_concurrent_delete",
    ) else {
        return;
    };
    let database_url = install_shared_schema(&database_url).await;
    let (writer, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let writer_driver = tokio::spawn(connection);
    let app_id = Uuid::new_v4();
    let post_id = Uuid::new_v4();
    let file_id = Uuid::new_v4();
    let mut catalog = system_attribute_catalog().unwrap();
    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    catalog.attributes.extend([
        attribute(id_attr, "id", true),
        attribute(title_attr, "title", false),
    ]);
    insert_catalog_attrs(&writer, app_id, &catalog).await;
    writer
        .execute(
            "INSERT INTO rules(app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &json!({
                    "posts": {"allow": {"view": "data.title.startsWith('published')"}},
                    "$files": {"allow": {"view": "true"}}
                }),
            ],
        )
        .await
        .unwrap();
    for (entity, attr, value) in [
        (post_id, id_attr, json!(post_id)),
        (post_id, title_attr, json!("published")),
        (
            file_id,
            catalog.forward("$files", "id").unwrap().id,
            json!(file_id),
        ),
        (
            file_id,
            catalog.forward("$files", "location-id").unwrap().id,
            json!("snapshot-file"),
        ),
    ] {
        writer.execute("INSERT INTO triples(app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae) VALUES ($1,$2,$3,$4,$5,true,false,false,false,false)", &[&app_id,&entity,&attr,&Json(&value),&value.to_string()]).await.unwrap();
    }
    let (reader, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let reader_driver = tokio::spawn(connection);
    let barrier = Arc::new(SnapshotBarrier {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let service = Arc::new(
        AdminDataService::new(reader, Arc::new(TestAuthorizer { catalog }))
            .with_file_url_signer(barrier.clone()),
    );
    let query_service = service.clone();
    let query = tokio::spawn(async move {
        query_service
            .query(
                context(app_id, AdminActor::Guest),
                AdminQueryRequest {
                    query: serde_json::from_value(
                        json!({"posts": {}, "$files": {"$": {"fields": ["url"]}}}),
                    )
                    .unwrap(),
                    inference: false,
                    versions: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        writer
            .execute(
                "DELETE FROM triples WHERE app_id=$1 AND entity_id=$2",
                &[&app_id, &post_id]
            )
            .await
            .unwrap(),
        2
    );
    barrier.release.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), query)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_ok(),
        "query lost its permission entity between execution and preload: {result:?}"
    );
    assert_eq!(result.unwrap()["posts"][0]["title"], "published");
    drop(service);
    drop(writer);
    reader_driver.await.unwrap().unwrap();
    writer_driver.await.unwrap().unwrap();
}
