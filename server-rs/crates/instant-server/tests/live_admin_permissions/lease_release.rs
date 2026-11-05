//! Exercise the real framework-query snapshot and pool, not a mocked loader.
use super::*;
use std::sync::Condvar;
use std::sync::atomic::AtomicBool;
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;

struct Fixture {
    writer: tokio_postgres::Client,
    driver: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
    pool: PostgresPool,
    app: Uuid,
    post: Uuid,
    owner: Uuid,
    catalog: AttributeCatalog,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let url = instant_test_support::test_database_url("lease_release_fixture")?;
        let scoped = install_shared_schema(&url).await;
        let (writer, connection) = tokio_postgres::connect(&scoped, NoTls).await.unwrap();
        let driver = tokio::spawn(connection);
        let app = Uuid::new_v4();
        writer
            .execute("INSERT INTO apps(id) VALUES ($1)", &[&app])
            .await
            .unwrap();
        let mut catalog = system_attribute_catalog().unwrap();
        let mut owner = attribute(Uuid::new_v4(), "owner", false);
        owner.value_type = AttributeValueType::Ref;
        owner.reverse_etype = Some("people".into());
        owner.reverse_label = Some("posts".into());
        catalog.attributes.extend([
            attribute(Uuid::new_v4(), "id", true),
            attribute(Uuid::new_v4(), "title", false),
            attribute(Uuid::new_v4(), "unfetched", false),
            owner,
            named_attribute(Uuid::new_v4(), "people", "id"),
            named_attribute(Uuid::new_v4(), "people", "name"),
        ]);
        insert_catalog_attrs(&writer, app, &catalog).await;
        let fixture = Self {
            writer,
            driver,
            pool: PostgresPool::connect(&scoped, 1).await.unwrap(),
            app,
            post: Uuid::new_v4(),
            owner: Uuid::new_v4(),
            catalog,
        };
        for (entity, etype, label, value) in [
            (fixture.post, "posts", "id", json!(fixture.post)),
            (fixture.post, "posts", "title", json!("published")),
            (
                fixture.post,
                "posts",
                "unfetched",
                json!("allowed-in-snapshot"),
            ),
            (fixture.post, "posts", "owner", json!(fixture.owner)),
            (fixture.owner, "people", "id", json!(fixture.owner)),
            (fixture.owner, "people", "name", json!("allowed")),
            (Uuid::new_v4(), "$files", "id", Value::Null),
        ] {
            let value = if value.is_null() {
                json!(entity)
            } else {
                value
            };
            fixture.insert(entity, etype, label, value).await;
            if etype == "$files" {
                fixture
                    .insert(entity, etype, "location-id", json!("lease-release-fixture"))
                    .await;
            }
        }
        fixture
            .rules(json!({"posts":{"allow":{"view":"true"}}}))
            .await;
        Some(fixture)
    }

    async fn insert(&self, entity: Uuid, etype: &str, label: &str, value: Value) {
        let attr = self.catalog.forward(etype, label).unwrap();
        self.writer.execute(
            "INSERT INTO triples(app_id,entity_id,attr_id,value,value_md5,ea,eav,av,ave,vae) VALUES($1,$2,$3,$4,$5,true,$6,$7,$8,$6)",
            &[&self.app,&entity,&attr.id,&Json(&value),&value.to_string(),
              &(attr.value_type == AttributeValueType::Ref),&attr.unique,&attr.indexed],
        ).await.unwrap();
    }

    async fn update(&self, entity: Uuid, etype: &str, label: &str, value: Value) {
        let attr = self.catalog.forward(etype, label).unwrap();
        assert_eq!(self.writer.execute(
            "UPDATE triples SET value=$4,value_md5=$5 WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3",
            &[&self.app,&entity,&attr.id,&Json(&value),&value.to_string()],
        ).await.unwrap(), 1);
    }

    async fn rules(&self, rules: Value) {
        self.writer.execute(
            "INSERT INTO rules(app_id,code) VALUES($1,$2) ON CONFLICT(app_id) DO UPDATE SET code=excluded.code",
            &[&self.app,&Json(rules)],
        ).await.unwrap();
    }

    fn service(&self) -> AdminDataService<TestAuthorizer> {
        AdminDataService::from_pool(
            self.pool.clone(),
            Arc::new(TestAuthorizer {
                catalog: self.catalog.clone(),
            }),
        )
    }

    async fn query(
        &self,
        service: &AdminDataService<TestAuthorizer>,
        native: bool,
        raw_query: Value,
    ) -> Result<RuntimeFrameworkResponse, ApiError> {
        query(service, self.app, self.catalog.clone(), native, raw_query).await
    }

    async fn assert_clean_connection(&self) {
        let client = tokio::time::timeout(Duration::from_secs(2), self.pool.acquire())
            .await
            .expect("query leaked the only pool lease")
            .unwrap();
        let row = client.query_one(
            "SELECT current_setting('transaction_read_only'),current_setting('transaction_isolation'),1", &[],
        ).await.expect("pooled connection retained an aborted transaction");
        assert_eq!(row.get::<_, String>(0), "off");
        assert_eq!(row.get::<_, String>(1), "read committed");
        assert_eq!(row.get::<_, i32>(2), 1);
        assert_eq!(self.pool.status().waiting, 0);
    }

    async fn close(self) {
        self.assert_clean_connection().await;
        drop(self.pool);
        drop(self.writer);
        self.driver.await.unwrap().unwrap();
    }
}

async fn query(
    service: &AdminDataService<TestAuthorizer>,
    app: Uuid,
    catalog: AttributeCatalog,
    native: bool,
    query: Value,
) -> Result<RuntimeFrameworkResponse, ApiError> {
    let request = AdminQueryRequest {
        query: query.as_object().unwrap().clone(),
        inference: false,
        versions: None,
    };
    if native {
        service
            .query_join_rows_on_native_catalog(
                context(app, AdminActor::Guest),
                request,
                app,
                catalog,
            )
            .await
    } else {
        service
            .query_join_rows(context(app, AdminActor::Guest), request)
            .await
    }
}

fn selected_query() -> Value {
    json!({"posts":{"$":{"fields":["id","title"]}}})
}

// Existing production spans give deterministic stage barriers. block_in_place
// hands the runtime's timer/I/O driver to another worker while this test-only
// observer waits; no production hook or artificial await is used.
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    once: AtomicBool,
    released: Mutex<bool>,
    changed: Condvar,
}

struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.changed.notify_all();
    }
}

impl Gate {
    fn stop(&self) {
        if self.once.swap(true, Ordering::SeqCst) {
            return;
        }
        self.entered.notify_one();
        tokio::task::block_in_place(|| {
            let (guard, timeout) = self
                .changed
                .wait_timeout_while(
                    self.released.lock().unwrap(),
                    Duration::from_secs(10),
                    |released| !*released,
                )
                .unwrap();
            drop(guard);
            assert!(!timeout.timed_out(), "test stage barrier was not released");
        });
    }

    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("query did not reach the requested permission stage");
    }
}

struct Probe {
    name: &'static str,
    on_close: bool,
    pool: PostgresPool,
    available: Arc<Mutex<Vec<usize>>>,
    gate: Option<Arc<Gate>>,
}

impl Probe {
    fn observe(&self) {
        self.available
            .lock()
            .unwrap()
            .push(self.pool.status().available);
        if let Some(gate) = &self.gate {
            gate.stop();
        }
    }
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> tracing_subscriber::Layer<S> for Probe {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _: &tracing::span::Id,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !self.on_close && attrs.metadata().name() == self.name {
            self.observe();
        }
    }

    fn on_close(&self, id: tracing::span::Id, context: tracing_subscriber::layer::Context<'_, S>) {
        if self.on_close
            && context
                .span(&id)
                .is_some_and(|span| span.name() == self.name)
        {
            self.observe();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pure_framework_evaluation_returns_actual_lease_and_owns_complete_fresh_inputs() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for native in [false, true] {
        fixture
            .rules(
                json!({"posts":{"allow":{"view":"data.unfetched.startsWith('allowed')"},
            "fields":{"title":"data.unfetched == 'allowed-in-snapshot'"}}}),
            )
            .await;
        fixture
            .update(
                fixture.post,
                "posts",
                "unfetched",
                json!("allowed-in-snapshot"),
            )
            .await;
        let available = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let subscriber = tracing_subscriber::registry().with(Probe {
            name: "permissions/filter-query",
            on_close: false,
            pool: fixture.pool.clone(),
            available: available.clone(),
            gate: Some(gate.clone()),
        });
        let service = Arc::new(fixture.service());
        let task_service = service.clone();
        let (app, catalog) = (fixture.app, fixture.catalog.clone());
        let tracker = instant_server::StatementTracker::default();
        let task_tracker = tracker.clone();
        let pending = tokio::spawn(async move {
            task_tracker
                .scope(
                    query(&task_service, app, catalog, native, selected_query())
                        .with_subscriber(subscriber),
                )
                .await
        });
        gate.wait().await;
        assert_eq!(
            tracker.active_count(),
            0,
            "early return retained a cancellation registration for the recycled connection"
        );
        assert_eq!(
            *available.lock().unwrap(),
            vec![1],
            "CPU-only evaluation still owns the only data connection"
        );
        fixture.assert_clean_connection().await;
        let idle_in_transaction:i64 = fixture.writer.query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND state='idle in transaction'",&[],
        ).await.unwrap().get(0);
        assert_eq!(
            idle_in_transaction, 0,
            "early return left a PostgreSQL transaction open"
        );
        fixture
            .update(
                fixture.post,
                "posts",
                "unfetched",
                json!("denied-after-preload"),
            )
            .await;
        drop(release);
        let response = pending.await.unwrap().unwrap();
        assert!(response.result.to_string().contains("published"));
        assert!(!response.result.to_string().contains("unfetched"));
        assert!(response.instant_topics.is_some());
        let fresh = fixture
            .query(&service, native, selected_query())
            .await
            .unwrap();
        assert!(
            !fresh.result.to_string().contains("published"),
            "prepared data or decisions escaped this request"
        );
        drop(service);
    }
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn framework_query_and_complete_permission_preload_keep_the_same_snapshot() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.rules(json!({"posts":{"allow":{"view":"data.unfetched.startsWith('allowed')"},
        "fields":{"title":"data.unfetched == 'allowed-in-snapshot'"}},"$files":{"allow":{"view":"true"}}})).await;
    let barrier = Arc::new(SnapshotBarrier {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    let service = Arc::new(fixture.service().with_file_url_signer(barrier.clone()));
    let task_service = service.clone();
    let (app, catalog) = (fixture.app, fixture.catalog.clone());
    let pending = tokio::spawn(async move {
        query(
            &task_service,
            app,
            catalog,
            true,
            json!({"posts":{"$":{"fields":["id","title"]}},"$files":{"$":{"fields":["url"]}}}),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
        .await
        .unwrap();
    assert_eq!(fixture.pool.status().available, 0);
    assert_eq!(
        fixture
            .writer
            .execute(
                "DELETE FROM triples WHERE app_id=$1 AND entity_id=$2",
                &[&fixture.app, &fixture.post]
            )
            .await
            .unwrap(),
        4
    );
    barrier.release.add_permits(1);
    let response = pending.await.unwrap().unwrap();
    assert!(
        response.result.to_string().contains("published"),
        "permission preload saw a later delete, not the query's original snapshot"
    );
    drop(service);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn literal_dynamic_bind_dead_and_unfetched_field_refs_retain_original_snapshot() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for fields in [
        json!({"title":"data.ref('owner.name') == ['allowed']"}),
        json!({"title":"data.ref(ruleParams.path) == ['allowed']"}),
        json!({"title":"true || data.ref(ruleParams.path) == []"}),
        json!({"unfetched":"data.ref(ruleParams.path) == ['allowed']"}),
    ] {
        fixture
            .rules(json!({"posts":{"allow":{"view":"true"},"fields":fields}}))
            .await;
        fixture
            .update(fixture.owner, "people", "name", json!("allowed"))
            .await;
        let available = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let subscriber = tracing_subscriber::registry().with(Probe {
            name: "permissions/filter-query",
            on_close: false,
            pool: fixture.pool.clone(),
            available: available.clone(),
            gate: Some(gate.clone()),
        });
        let service = Arc::new(fixture.service());
        let task_service = service.clone();
        let (app, catalog) = (fixture.app, fixture.catalog.clone());
        let tracker = instant_server::StatementTracker::default();
        let task_tracker = tracker.clone();
        let pending = tokio::spawn(async move {
            let mut request = selected_query();
            request["$$ruleParams"] = json!({"path":"owner.name"});
            task_tracker
                .scope(
                    query(&task_service, app, catalog, true, request).with_subscriber(subscriber),
                )
                .await
        });
        gate.wait().await;
        assert_eq!(
            tracker.active_count(),
            1,
            "callback fallback lost its active snapshot registration"
        );
        assert_eq!(
            *available.lock().unwrap(),
            vec![0],
            "callback-capable field released its snapshot"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(30), fixture.pool.acquire())
                .await
                .is_err()
        );
        fixture
            .update(fixture.owner, "people", "name", json!("denied-after-query"))
            .await;
        drop(release);
        let response = pending.await.unwrap().unwrap();
        assert_eq!(tracker.active_count(), 0);
        assert!(
            response.result.to_string().contains("published"),
            "reference load escaped the original query snapshot"
        );
        fixture.assert_clean_connection().await;
        drop(service);
    }
    // A reachable bind is independent of the source expression's literal refs.
    fixture.rules(json!({"posts":{"bind":["owners","data.ref(ruleParams.path)"],"allow":{"view":"true"},"fields":{"title":"owners == ['allowed']"}}})).await;
    fixture
        .update(fixture.owner, "people", "name", json!("allowed"))
        .await;
    let available = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(Probe {
        name: "permissions/filter-query",
        on_close: false,
        pool: fixture.pool.clone(),
        available: available.clone(),
        gate: None,
    });
    let mut request = selected_query();
    request["$$ruleParams"] = json!({"path":"owner.name"});
    let response = fixture
        .query(&fixture.service(), true, request)
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert!(response.result.to_string().contains("published"));
    assert_eq!(*available.lock().unwrap(), vec![0]);
    fixture.close().await;
}

struct CountingRateLimits {
    pool: PostgresPool,
    calls: Mutex<Vec<(u64, usize)>>,
    deny: AtomicBool,
}

impl std::fmt::Debug for CountingRateLimits {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CountingRateLimits")
            .finish_non_exhaustive()
    }
}

impl instant_permissions::RateLimitConsumer for CountingRateLimits {
    fn consume(
        &self,
        request: &instant_permissions::RateLimitRequest,
    ) -> Result<instant_permissions::RateLimitDecision, String> {
        self.calls
            .lock()
            .unwrap()
            .push((request.tokens, self.pool.status().available));
        Ok(if self.deny.load(Ordering::Relaxed) {
            instant_permissions::RateLimitDecision::Denied {
                retry_after: Duration::from_secs(1),
                remaining_tokens: 0,
            }
        } else {
            instant_permissions::RateLimitDecision::Allowed {
                remaining_tokens: 100,
            }
        })
    }
}

#[tokio::test]
async fn deferred_query_and_dynamic_field_rate_limits_keep_lease_and_charge_once() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for (view, field) in [
        ("rateLimit.reads.limit('all',2) && true", "true"),
        ("true", "rateLimit[ruleParams.bucket].limit('all',2)"),
    ] {
        fixture
            .rules(
                json!({"$rateLimits":{"reads":{"limits":[{"capacity":100}]}},
            "posts":{"allow":{"view":view},"fields":{"title":field}}}),
            )
            .await;
        let limiter = Arc::new(CountingRateLimits {
            pool: fixture.pool.clone(),
            calls: Mutex::new(Vec::new()),
            deny: AtomicBool::new(false),
        });
        let service = fixture.service().with_rate_limit_consumer(limiter.clone());
        let mut request = selected_query();
        request["$$ruleParams"] = json!({"bucket":"reads"});
        assert!(
            fixture
                .query(&service, true, request.clone())
                .await
                .unwrap()
                .result
                .to_string()
                .contains("published")
        );
        assert_eq!(
            *limiter.calls.lock().unwrap(),
            vec![(2, 0)],
            "limit was skipped, doubled, or moved outside its original lease"
        );
        limiter.deny.store(true, Ordering::Relaxed);
        let error = fixture.query(&service, true, request).await.unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::RateLimited);
        assert_eq!(*limiter.calls.lock().unwrap(), vec![(2, 0), (2, 0)]);
        fixture.assert_clean_connection().await;
    }
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_failure_is_deferred_behind_cel_error_and_never_returns_success() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for field in ["data.title / 0 == 1", "true"] {
        fixture
            .rules(json!({"posts":{"allow":{"view":"true"},"fields":{"title":field}}}))
            .await;
        let service = Arc::new(fixture.service());
        let baseline = fixture.query(&service, true, selected_query()).await;
        let pid: i32 = fixture
            .pool
            .acquire()
            .await
            .unwrap()
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let gate = Arc::new(Gate::default());
        let release = Release(gate.clone());
        let subscriber = tracing_subscriber::registry().with(Probe {
            name: "permissions/prepare-query",
            on_close: true,
            pool: fixture.pool.clone(),
            available: Arc::new(Mutex::new(Vec::new())),
            gate: Some(gate.clone()),
        });
        let task_service = service.clone();
        let (app, catalog) = (fixture.app, fixture.catalog.clone());
        let pending = tokio::spawn(async move {
            query(&task_service, app, catalog, true, selected_query())
                .with_subscriber(subscriber)
                .await
        });
        gate.wait().await;
        assert!(
            fixture
                .writer
                .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
        drop(release);
        let error = pending.await.unwrap().unwrap_err();
        if field == "true" {
            assert!(baseline.is_ok());
            assert_eq!(
                error.body().kind,
                ApiErrorType::Unknown,
                "failed snapshot end was silently ignored"
            );
        } else {
            let expected = baseline.unwrap_err();
            assert_eq!(
                expected.body().kind,
                ApiErrorType::PermissionEvaluationFailed
            );
            assert_eq!(
                error.body(),
                expected.body(),
                "rollback error masked the original permission error"
            );
        }
        fixture.assert_clean_connection().await;
        drop(service);
    }
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callback_loader_error_still_uses_open_snapshot_and_cleans_connection() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.rules(json!({"posts":{"allow":{"view":"true"},"fields":{"title":"data.ref(ruleParams.path) == ['allowed']"}}})).await;
    let pid: i32 = fixture
        .pool
        .acquire()
        .await
        .unwrap()
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let gate = Arc::new(Gate::default());
    let release = Release(gate.clone());
    let subscriber = tracing_subscriber::registry().with(Probe {
        name: "permissions/filter-query",
        on_close: false,
        pool: fixture.pool.clone(),
        available: Arc::new(Mutex::new(Vec::new())),
        gate: Some(gate.clone()),
    });
    let service = Arc::new(fixture.service());
    let task_service = service.clone();
    let (app, catalog) = (fixture.app, fixture.catalog.clone());
    let pending = tokio::spawn(async move {
        let mut request = selected_query();
        request["$$ruleParams"] = json!({"path":"owner.name"});
        query(&task_service, app, catalog, true, request)
            .with_subscriber(subscriber)
            .await
    });
    gate.wait().await;
    assert!(
        fixture
            .writer
            .query_one("SELECT pg_terminate_backend($1)", &[&pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(release);
    let error = pending.await.unwrap().unwrap_err();
    assert_eq!(error.body().kind, ApiErrorType::PermissionEvaluationFailed);
    fixture.assert_clean_connection().await;
    drop(service);
    fixture.close().await;
}

#[tokio::test]
async fn cancellation_during_snapshot_await_returns_clean_lease_for_both_framework_paths() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture
        .rules(json!({"posts":{"allow":{"view":"true"}},"$files":{"allow":{"view":"true"}}}))
        .await;
    for native in [false, true] {
        let barrier = Arc::new(SnapshotBarrier {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        });
        let service = Arc::new(fixture.service().with_file_url_signer(barrier.clone()));
        let task_service = service.clone();
        let (app, catalog) = (fixture.app, fixture.catalog.clone());
        let tracker = instant_server::StatementTracker::default();
        let task_tracker = tracker.clone();
        let pending = tokio::spawn(async move {
            task_tracker
                .scope(query(
                    &task_service,
                    app,
                    catalog,
                    native,
                    json!({"posts":{},"$files":{"$":{"fields":["url"]}}}),
                ))
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
            .await
            .unwrap();
        assert_eq!(fixture.pool.status().available, 0);
        assert_eq!(tracker.active_count(), 1);
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert_eq!(tracker.active_count(), 0);
        fixture.assert_clean_connection().await;
        let active:i64 = fixture.writer.query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND state='idle in transaction'",&[],
        ).await.unwrap().get(0);
        assert_eq!(
            active, 0,
            "cancellation left an open snapshot on the returned connection"
        );
        drop(service);
    }
    fixture.close().await;
}

#[tokio::test]
async fn cancellation_while_waiting_for_query_lease_removes_waiter() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let service = Arc::new(fixture.service());
    let held = fixture.pool.acquire().await.unwrap();
    let task_service = service.clone();
    let (app, catalog) = (fixture.app, fixture.catalog.clone());
    let pending =
        tokio::spawn(
            async move { query(&task_service, app, catalog, true, selected_query()).await },
        );
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.pool.status().waiting == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert_eq!(fixture.pool.status().waiting, 0);
    drop(held);
    fixture.assert_clean_connection().await;
    assert!(
        fixture
            .query(&service, true, selected_query())
            .await
            .unwrap()
            .result
            .to_string()
            .contains("published")
    );
    drop(service);
    fixture.close().await;
}
