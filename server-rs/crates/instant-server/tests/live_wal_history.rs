use std::fmt::Write as _;
use std::future::{Future, ready};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use instant_http::{ApiError, WebhookPayloadRequest};
use instant_realtime::ReplicationPipelineOutcome;
use instant_server::{
    DurableInvalidationSink, PostgresDurableInvalidationSink, PostgresPool,
    PostgresRealtimeCluster, PostgresWebhookPayloadStore, RuntimeRealtimeCluster,
    StoredWebhookEventKey, WebhookEventNotifier, WebhookFileUrlSigner, WebhookHistoryArchive,
    WebhookLatencyRecorder, WebhookPayloadAuthorizer, WebhookPayloadStore,
};
use instant_wal::{Isn, Lsn, WalAction, WalColumn, WalEntry, WalRecord};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_postgres::NoTls;
use url::Url;
use uuid::Uuid;

async fn connect_client(database_url: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

fn scoped_url(url: &str, schema: &str) -> String {
    let mut url = Url::parse(url).expect("TEST_DATABASE_URL must be a PostgreSQL URL");
    let mut pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let mut options = pairs
        .iter()
        .filter(|(key, _)| key == "options")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if !options.is_empty() {
        options.push(' ');
    }
    write!(options, "-csearch_path={schema},public").expect("writing to a String cannot fail");
    pairs.retain(|(key, _)| key != "options");
    url.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("options", &options);
    url.into()
}

#[test]
fn scoped_url_preserves_existing_query_and_session_options() {
    let scoped = scoped_url(
        "postgresql://localhost/instant?sslmode=disable&options=-cstatement_timeout%3D5s",
        "wal_history_test",
    );
    let url = Url::parse(&scoped).unwrap();
    let pairs = url.query_pairs().collect::<Vec<_>>();
    assert!(pairs.contains(&("sslmode".into(), "disable".into())));
    assert!(pairs.contains(&(
        "options".into(),
        "-cstatement_timeout=5s -csearch_path=wal_history_test,public".into(),
    )));
}

#[derive(Default)]
struct Notifier(StdMutex<Vec<StoredWebhookEventKey>>);

impl WebhookEventNotifier for Notifier {
    type Error = std::convert::Infallible;

    fn notify(
        &self,
        events: &[StoredWebhookEventKey],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.0.lock().unwrap().extend_from_slice(events);
        ready(Ok(()))
    }
}

#[derive(Default)]
struct Latencies(StdMutex<Vec<DateTime<Utc>>>);

impl WebhookLatencyRecorder for Latencies {
    fn record(&self, tx_created_at: DateTime<Utc>) {
        self.0.lock().unwrap().push(tx_created_at);
    }
}

struct Allow;

impl WebhookPayloadAuthorizer for Allow {
    fn authorize(
        &self,
        _request: &WebhookPayloadRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        ready(Ok(()))
    }
}

struct FileUrls;

impl WebhookFileUrlSigner for FileUrls {
    fn sign(
        &self,
        app_id: Uuid,
        location_id: &str,
    ) -> impl Future<Output = Result<String, ApiError>> + Send {
        ready(Ok(format!("https://files.test/{app_id}/{location_id}")))
    }
}

struct NoArchive;

impl WebhookHistoryArchive for NoArchive {
    fn load(
        &self,
        _app_id: Uuid,
        _isn: Isn,
    ) -> impl Future<Output = Result<WalRecord, ApiError>> + Send {
        ready(Err(ApiError::new(
            instant_http::ApiErrorType::Unknown,
            "archive is unused",
        )))
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn durable_sink_atomically_creates_replayable_webhook_history() {
    let Some(url) = instant_test_support::test_database_url(
        "durable_sink_atomically_creates_replayable_webhook_history",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TYPE pg_temp.isn AS (slot_num int, lsn pg_lsn);
             CREATE TYPE pg_temp.history_storage AS ENUM ('s3', 'pg');
             CREATE TYPE pg_temp.webhook_action AS ENUM ('create', 'update', 'delete');
             CREATE TYPE pg_temp.webhook_status AS ENUM ('active', 'disabled');
             CREATE TYPE pg_temp.webhook_event_status AS ENUM
               ('pending', 'processing', 'success', 'error', 'failed');
             CREATE TEMP TABLE history (
               isn isn NOT NULL, app_id uuid NOT NULL, topics bigint NOT NULL,
               storage history_storage NOT NULL, content bytea, partition_bucket int NOT NULL,
               PRIMARY KEY (isn, partition_bucket)
             );
             CREATE TEMP TABLE webhooks (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, topics bigint NOT NULL,
               id_attr_ids uuid[], actions webhook_action[] NOT NULL,
               status webhook_status NOT NULL, sink jsonb NOT NULL
             );
             CREATE TEMP TABLE webhook_events (
               webhook_id uuid NOT NULL, isn isn NOT NULL, app_id uuid NOT NULL,
               status webhook_event_status NOT NULL, partition_bucket int NOT NULL,
               PRIMARY KEY (webhook_id, isn, partition_bucket)
             );
             CREATE TEMP TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL,
               value_type text NOT NULL, cardinality text NOT NULL,
               is_unique boolean NOT NULL DEFAULT false,
               is_indexed boolean NOT NULL DEFAULT false,
               is_required boolean NOT NULL DEFAULT false,
               checked_data_type text, indexing boolean, checking_data_type boolean,
               deletion_marked_at timestamptz
             );",
        )
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let webhook_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let name_attr = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs (id, app_id, etype, label, value_type, cardinality)
             VALUES ($1, $3, 'users', 'id', 'blob', 'one'),
                    ($2, $3, 'users', 'name', 'blob', 'one')",
            &[&id_attr, &name_attr, &app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO webhooks
               (id, app_id, topics, id_attr_ids, actions, status, sink)
             VALUES ($1, $2, -1, ARRAY[$3::uuid]::uuid[], ARRAY['create']::webhook_action[],
                     'active', '{\"url\":\"https://sink.test\"}')",
            &[&webhook_id, &app_id, &id_attr],
        )
        .await
        .unwrap();
    let record = create_record(app_id, entity_id, id_attr, name_attr);
    let shared = Arc::new(Mutex::new(client));
    let notifier = Arc::new(Notifier::default());
    let latencies = Arc::new(Latencies::default());
    let mut sink = PostgresDurableInvalidationSink::with_shared_client(
        Arc::clone(&shared),
        Arc::clone(&notifier),
    )
    .with_webhook_latency_recorder(Arc::clone(&latencies) as Arc<dyn WebhookLatencyRecorder>);
    let inserted = sink.persist_record(&record).await.unwrap();
    assert_eq!(inserted.len(), 1);
    assert_eq!(inserted[0].webhook_id, webhook_id);
    assert!(sink.persist_record(&record).await.unwrap().is_empty());
    assert_eq!(notifier.0.lock().unwrap().len(), 1);
    sink.persist(&ReplicationPipelineOutcome {
        record: Some(record.clone()),
        ..ReplicationPipelineOutcome::default()
    })
    .await
    .unwrap();
    assert_eq!(
        latencies.0.lock().unwrap().as_slice(),
        &[record.tx_created_at.unwrap()]
    );
    let payloads = PostgresWebhookPayloadStore::with_shared_client(
        Arc::clone(&shared),
        Arc::new(Allow),
        Arc::new(FileUrls),
        Arc::new(NoArchive),
    );
    let payload = payloads
        .load(WebhookPayloadRequest {
            app_id,
            webhook_id,
            isn: record.isn.to_string(),
            bearer_token: "authorized".to_owned(),
        })
        .await
        .unwrap();
    let rows = payload.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["namespace"], "users");
    assert_eq!(rows[0]["action"], "create");
    assert_eq!(rows[0]["id"], entity_id.to_string());
    assert_eq!(rows[0]["before"], Value::Null);
    assert_eq!(rows[0]["after"]["id"], entity_id.to_string());
    assert_eq!(rows[0]["after"]["name"], "Ada");
    let client = shared.lock().await;
    let history_count: i64 = client
        .query_one("SELECT count(*) FROM history", &[])
        .await
        .unwrap()
        .get(0);
    let event_count: i64 = client
        .query_one("SELECT count(*) FROM webhook_events", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!((history_count, event_count), (1, 1));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pooled_durable_sink_replaces_terminated_backend_without_losing_atomicity() {
    let Some(database_url) = instant_test_support::test_database_url(
        "pooled_durable_sink_replaces_terminated_backend_without_losing_atomicity",
    ) else {
        return;
    };
    let setup = connect_client(&database_url).await;
    let schema = format!("wal_history_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TYPE {schema}.isn AS (slot_num int, lsn pg_lsn);
             CREATE TYPE {schema}.history_storage AS ENUM ('s3', 'pg');
             CREATE TYPE {schema}.webhook_action AS ENUM ('create', 'update', 'delete');
             CREATE TYPE {schema}.webhook_status AS ENUM ('active', 'disabled');
             CREATE TYPE {schema}.webhook_event_status AS ENUM
               ('pending', 'processing', 'success', 'error', 'failed');
             CREATE TABLE {schema}.history (
               isn {schema}.isn NOT NULL, app_id uuid NOT NULL, topics bigint NOT NULL,
               storage {schema}.history_storage NOT NULL, content bytea,
               partition_bucket int NOT NULL,
               PRIMARY KEY (isn, partition_bucket)
             );
             CREATE TABLE {schema}.webhooks (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, topics bigint NOT NULL,
               id_attr_ids uuid[], actions {schema}.webhook_action[] NOT NULL,
               status {schema}.webhook_status NOT NULL, sink jsonb NOT NULL
             );
             CREATE TABLE {schema}.attrs (id uuid PRIMARY KEY, app_id uuid, etype text, label text);
             CREATE TABLE {schema}.triples (app_id uuid, entity_id uuid, attr_id uuid, value jsonb);
             CREATE TABLE {schema}.config (
               k text PRIMARY KEY NOT NULL, v json NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_machines (
               machine_id uuid PRIMARY KEY, created_at timestamptz DEFAULT now(), updated_at timestamptz DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_event_consumers (
               machine_id uuid, consumer_kind text, event_cursor bigint, updated_at timestamptz DEFAULT now(), PRIMARY KEY(machine_id, consumer_kind)
             );
             CREATE TABLE {schema}.rust_realtime_room_events (
               id bigserial PRIMARY KEY, source_machine_id uuid NOT NULL,
               kind text NOT NULL, app_id uuid NOT NULL, room_id text NOT NULL, payload jsonb NOT NULL
             );
             CREATE UNIQUE INDEX wal_event_unique ON {schema}.rust_realtime_room_events
               (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal');
             CREATE TABLE {schema}.webhook_events (
               webhook_id uuid NOT NULL, isn {schema}.isn NOT NULL, app_id uuid NOT NULL,
               status {schema}.webhook_event_status NOT NULL, partition_bucket int NOT NULL,
               PRIMARY KEY (webhook_id, isn, partition_bucket)
             );"
        ))
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let webhook_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let name_attr = Uuid::new_v4();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.webhooks
                   (id, app_id, topics, id_attr_ids, actions, status, sink)
                 VALUES ($1, $2, -1, ARRAY[$3::uuid]::uuid[],
                         ARRAY['create']::{schema}.webhook_action[],
                         'active'::{schema}.webhook_status,
                         '{{\"url\":\"https://sink.test\"}}')"
            ),
            &[&webhook_id, &app_id, &id_attr],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&scoped_url(&database_url, &schema), 1)
        .await
        .unwrap();
    let notifier = Arc::new(Notifier::default());
    let machine_id = Uuid::new_v4();
    let sink = PostgresDurableInvalidationSink::from_cluster_writer(
        pool.clone(),
        Arc::clone(&notifier),
        machine_id,
    );
    let first = create_record(app_id, Uuid::new_v4(), id_attr, name_attr);
    assert_eq!(sink.persist_record(&first).await.unwrap().len(), 1);
    let published = setup
        .query_one(
            &format!("SELECT count(*) FROM {schema}.rust_realtime_room_events"),
            &[],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(
        published, 1,
        "history and realtime publication must commit together"
    );

    let doomed = pool.acquire().await.unwrap();
    let doomed_pid = doomed
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    assert!(
        setup
            .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(doomed);

    let mut second = create_record(app_id, Uuid::new_v4(), id_attr, name_attr);
    second.tx_id = 43;
    second.previous_isn = Some(first.isn);
    second.isn = Isn::new(2, Lsn::from_u64(200));
    second.next_lsn = second.isn.lsn();
    let inserted = tokio::time::timeout(Duration::from_secs(20), sink.persist_record(&second))
        .await
        .expect("WAL-history sink did not reconnect within the bounded deadline")
        .unwrap();
    assert_eq!(inserted.len(), 1);

    let replacement = pool.acquire().await.unwrap();
    let replacement_row = replacement
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(replacement_row.get::<_, i32>(0), doomed_pid);
    assert_eq!(replacement_row.get::<_, String>(1), schema);
    drop(replacement);
    assert!(sink.persist_record(&second).await.unwrap().is_empty());

    let history_count = setup
        .query_one(&format!("SELECT count(*) FROM {schema}.history"), &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    let event_count = setup
        .query_one(
            &format!("SELECT count(*) FROM {schema}.webhook_events"),
            &[],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!((history_count, event_count), (2, 2));
    assert_eq!(notifier.0.lock().unwrap().len(), 2);

    assert_original_record_batch(&setup, &schema, &pool, &sink, &notifier, &first, machine_id)
        .await;

    if std::env::var_os("BENCHMARK_WAL_BATCH").is_some() {
        benchmark_original_record_batches(&setup, &schema, &sink, &first).await;
    }

    // Measure the production persistence method against an isolated PostgreSQL schema.
    // This is diagnostic evidence, not an environment-dependent acceptance threshold.
    if std::env::var_os("BENCHMARK_WAL_HISTORY").is_some() {
        setup
            .execute(
                &format!("UPDATE {schema}.webhooks SET status='disabled'"),
                &[],
            )
            .await
            .unwrap();
        let history_pool = PostgresPool::connect(&scoped_url(&database_url, &schema), 1)
            .await
            .unwrap();
        let legacy =
            PostgresDurableInvalidationSink::from_pool(history_pool.clone(), Arc::clone(&notifier));
        let cluster = PostgresRealtimeCluster::from_pools(machine_id, pool.clone(), history_pool)
            .await
            .unwrap();
        for round in 0..4_u64 {
            for atomic in [false, true] {
                let started = std::time::Instant::now();
                for n in 0..5000 {
                    let mut record = first.clone();
                    record.isn = Isn::new(
                        2,
                        Lsn::from_u64(10000 + round * 10000 + u64::from(atomic) * 5000 + n),
                    );
                    record.next_lsn = record.isn.lsn();
                    if atomic {
                        assert!(sink.persist_record(&record).await.unwrap().is_empty());
                    } else {
                        assert!(legacy.persist_record(&record).await.unwrap().is_empty());
                        cluster.publish_wal(record).await.unwrap();
                    }
                }
                eprintln!(
                    "durable-publication-burst round={round} atomic={atomic} records=5000 elapsed_ms={:.3}",
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        drop(cluster);
        drop(legacy);
        setup
            .execute(
                &format!("UPDATE {schema}.webhooks SET status='active'"),
                &[],
            )
            .await
            .unwrap();
    }

    // Match all action variants, nullable IDs, app/status isolation, and topic filtering.
    let mut sequence = 3000_u64;
    for (action_name, action) in [
        ("create", WalAction::Insert),
        ("update", WalAction::Update),
        ("delete", WalAction::Delete),
    ] {
        for configured_action in ["create", "update", "delete"] {
            for variant in 0..7 {
                sequence += 1;
                let mut record = first.clone();
                record.isn = Isn::new(2, Lsn::from_u64(sequence));
                record.next_lsn = record.isn.lsn();
                record.triple_changes.truncate(1);
                let change = &mut record.triple_changes[0];
                change.action = action.clone();
                change.identity = change.columns.clone();
                if action == WalAction::Delete {
                    change.columns.clear();
                }
                if variant == 6 && action == WalAction::Update {
                    change
                        .identity
                        .iter_mut()
                        .find(|c| c.name == "pg_size")
                        .unwrap()
                        .value = json!(9);
                }
                let ids = match variant {
                    1 => None,
                    2 => Some(vec![Uuid::new_v4()]),
                    _ => Some(vec![id_attr]),
                };
                let owner = if variant == 3 { Uuid::new_v4() } else { app_id };
                let status = if variant == 4 { "disabled" } else { "active" };
                let topics: i64 = if variant == 5 { 0 } else { -1 };
                setup.execute(&format!("UPDATE {schema}.webhooks SET actions=ARRAY[$1::text::{schema}.webhook_action], id_attr_ids=$2, app_id=$3, status=$4::text::{schema}.webhook_status, topics=$5"), &[&configured_action,&ids,&owner,&status,&topics]).await.unwrap();
                let expected = action_name == configured_action
                    && (variant == 0 || (variant == 6 && action != WalAction::Update));
                assert_eq!(
                    sink.persist_record(&record).await.unwrap().len(),
                    usize::from(expected),
                    "action={action_name}, configured={configured_action}, variant={variant}"
                );
                let batched = record_at(&record, sequence + 1_000_000);
                let mut unrelated = record_at(&record, sequence + 2_000_000);
                unrelated.app_id = Uuid::new_v4();
                let inserted = sink.persist_records(&[&batched, &unrelated]).await.unwrap();
                assert_eq!(
                    inserted.len(),
                    usize::from(expected),
                    "batch action={action_name}, configured={configured_action}, variant={variant}"
                );
                if expected {
                    assert_eq!(inserted[0].webhook_id, webhook_id);
                    assert_eq!(inserted[0].isn, batched.isn);
                }
            }
        }
    }
    setup.execute(&format!("UPDATE {schema}.webhooks SET actions=ARRAY['create']::{schema}.webhook_action[], id_attr_ids=ARRAY[$1::uuid], app_id=$2, status='active', topics=-1"), &[&id_attr,&app_id]).await.unwrap();

    // A queue insertion failure must roll back the corresponding history insert.
    setup
        .batch_execute(&format!(
            "ALTER TABLE {schema}.webhook_events ADD CONSTRAINT reject_new CHECK (false) NOT VALID"
        ))
        .await
        .unwrap();
    let mut rejected = first.clone();
    rejected.isn = Isn::new(2, Lsn::from_u64(5000));
    rejected.next_lsn = rejected.isn.lsn();
    assert!(sink.persist_record(&rejected).await.is_err());
    let absent: i64 = setup
        .query_one(
            &format!("SELECT count(*) FROM {schema}.history WHERE isn=$1::text::{schema}.isn"),
            &[&rejected.isn.to_postgres_composite()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        absent, 0,
        "failed webhook insertion must not leave durable history"
    );
    setup
        .batch_execute(&format!(
            "ALTER TABLE {schema}.webhook_events DROP CONSTRAINT reject_new"
        ))
        .await
        .unwrap();

    // A publication failure must roll back BOTH history and matching webhooks.
    setup.batch_execute(&format!("ALTER TABLE {schema}.rust_realtime_room_events ADD CONSTRAINT reject_publication CHECK (false) NOT VALID")).await.unwrap();
    assert!(sink.persist_record(&rejected).await.is_err());
    let rejected_counts = setup.query_one(&format!("SELECT (SELECT count(*) FROM {schema}.history WHERE isn=$1::text::{schema}.isn), (SELECT count(*) FROM {schema}.webhook_events WHERE isn=$1::text::{schema}.isn), (SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE room_id=$2)"), &[&rejected.isn.to_postgres_composite(), &rejected.isn.to_string()]).await.unwrap();
    assert_eq!(
        (
            rejected_counts.get::<_, i64>(0),
            rejected_counts.get::<_, i64>(1),
            rejected_counts.get::<_, i64>(2)
        ),
        (0, 0, 0)
    );
    setup
        .batch_execute(&format!(
            "ALTER TABLE {schema}.rust_realtime_room_events DROP CONSTRAINT reject_publication"
        ))
        .await
        .unwrap();

    // Cache-only/empty-history records still publish, without requiring a timestamp.
    let mut empty = first.clone();
    empty.isn = Isn::new(2, Lsn::from_u64(7000));
    empty.next_lsn = empty.isn.lsn();
    empty.messages.clear();
    empty.wal_logs.clear();
    empty.tx_created_at = None;
    assert!(sink.persist_record(&empty).await.unwrap().is_empty());
    assert!(sink.persist_record(&empty).await.unwrap().is_empty());
    let empty_counts = setup.query_one(&format!("SELECT (SELECT count(*) FROM {schema}.history WHERE isn=$1::text::{schema}.isn), (SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE room_id=$2 AND payload=$3)"), &[&empty.isn.to_postgres_composite(), &empty.isn.to_string(), &tokio_postgres::types::Json(&empty)]).await.unwrap();
    assert_eq!(
        (empty_counts.get::<_, i64>(0), empty_counts.get::<_, i64>(1)),
        (0, 1)
    );

    // Concurrent replay on separate connections inserts and notifies only once.
    let other_pool = PostgresPool::connect(&scoped_url(&database_url, &schema), 1)
        .await
        .unwrap();
    let other_sink = PostgresDurableInvalidationSink::from_cluster_writer(
        other_pool.clone(),
        Arc::clone(&notifier),
        machine_id,
    );
    let mut concurrent = first.clone();
    concurrent.isn = Isn::new(2, Lsn::from_u64(6000));
    concurrent.next_lsn = concurrent.isn.lsn();
    let (left, right) = tokio::join!(
        sink.persist_record(&concurrent),
        other_sink.persist_record(&concurrent)
    );
    assert_eq!(left.unwrap().len() + right.unwrap().len(), 1);
    let replay_count = setup.query_one(&format!("SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE room_id=$1 AND payload=$2"), &[&concurrent.isn.to_string(), &tokio_postgres::types::Json(&concurrent)]).await.unwrap().get::<_,i64>(0);
    assert_eq!(replay_count, 1);

    drop(other_sink);
    drop(other_pool);

    let lease = pool.acquire().await.unwrap();
    let prepared: i64 = lease.query_one("SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE 'WITH persisted_history AS%'", &[]).await.unwrap().get(0);
    assert_eq!(
        prepared, 1,
        "history and webhook persistence must reuse one atomic statement"
    );
    drop(lease);

    drop(sink);
    drop(pool);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn assert_original_record_batch(
    setup: &tokio_postgres::Client,
    schema: &str,
    pool: &PostgresPool,
    sink: &PostgresDurableInvalidationSink<Notifier>,
    notifier: &Notifier,
    first: &WalRecord,
    machine_id: Uuid,
) {
    assert!(sink.persist_records(&[]).await.unwrap().is_empty());
    let records = (0..32_u64)
        .map(|index| {
            let mut record = record_at(first, 100_000 + index);
            record.previous_isn = Some(if index == 0 {
                first.isn
            } else {
                Isn::new(2, Lsn::from_u64(99_999 + index))
            });
            // Exercise each record's own partition and encoded payload.
            record.tx_created_at = first.tx_created_at.map(|created| {
                created + chrono::Duration::days(i64::try_from(index).unwrap() * 30)
            });
            record
        })
        .collect::<Vec<_>>();
    let references = records.iter().collect::<Vec<_>>();
    let before = notifier.0.lock().unwrap().len();
    let inserted = sink.persist_records(&references).await.unwrap();
    assert_eq!(inserted.len(), records.len());
    assert_eq!(notifier.0.lock().unwrap().len(), before + records.len());
    let mut transaction_ids = std::collections::HashSet::new();
    let mut previous_event_id = None;
    for record in &records {
        let row = setup
            .query_one(
                &format!(
                    "SELECT h.app_id, h.storage::text, h.content, h.partition_bucket,
                            h.xmin::text AS history_xmin,
                            w.webhook_id, w.status::text, w.partition_bucket,
                            w.xmin::text AS webhook_xmin,
                            e.source_machine_id, e.kind, e.app_id, e.payload, e.id,
                            e.xmin::text AS publication_xmin
                     FROM {schema}.history h
                     JOIN {schema}.webhook_events w ON w.isn=h.isn
                       AND w.partition_bucket=h.partition_bucket
                     JOIN {schema}.rust_realtime_room_events e ON e.room_id=$2
                     WHERE h.isn=$1::text::{schema}.isn"
                ),
                &[&record.isn.to_postgres_composite(), &record.isn.to_string()],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, Uuid>(0), record.app_id);
        assert_eq!(row.get::<_, String>(1), "pg");
        let content: Vec<u8> = row.get(2);
        let compressed = content
            .strip_prefix(b"instant-rust-wal\0\x01")
            .expect("history must contain a replayable WAL payload");
        let decoded: Value =
            serde_json::from_slice(&zstd::stream::decode_all(compressed).unwrap()).unwrap();
        assert_eq!(
            decoded,
            serde_json::to_value(record).unwrap(),
            "batch persistence must retain every original record"
        );
        let key = inserted.iter().find(|key| key.isn == record.isn).unwrap();
        assert_eq!(row.get::<_, Uuid>(5), key.webhook_id);
        assert_eq!(row.get::<_, String>(6), "pending");
        assert_eq!(row.get::<_, i32>(3), key.partition_bucket);
        assert_eq!(row.get::<_, i32>(7), key.partition_bucket);
        assert_eq!(row.get::<_, Uuid>(9), machine_id);
        assert_eq!(row.get::<_, String>(10), "wal");
        assert_eq!(row.get::<_, Uuid>(11), record.app_id);
        assert_eq!(
            row.get::<_, Value>(12),
            serde_json::to_value(record).unwrap()
        );
        let event_id: i64 = row.get(13);
        assert!(previous_event_id.is_none_or(|previous| previous < event_id));
        previous_event_id = Some(event_id);
        for column in ["history_xmin", "webhook_xmin", "publication_xmin"] {
            transaction_ids.insert(row.get::<_, String>(column));
        }
    }
    assert_eq!(
        transaction_ids.len(),
        1,
        "32 original records must share one durable history/webhook/publication commit"
    );
    assert!(sink.persist_records(&references).await.unwrap().is_empty());
    assert_eq!(notifier.0.lock().unwrap().len(), before + records.len());
    let room_ids = records
        .iter()
        .map(|record| record.isn.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        setup
            .query_one(
                &format!("SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE room_id=ANY($1::text[])"),
                &[&room_ids],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        32,
        "batch replay must not republish cluster events"
    );

    // Records from another app and records without replay history retain their
    // own publication data, without inheriting the adjacent record's matches.
    let local = record_at(first, 100_100);
    let mut other_app = record_at(first, 100_101);
    other_app.app_id = Uuid::new_v4();
    let mut empty = record_at(first, 100_102);
    empty.messages.clear();
    empty.wal_logs.clear();
    empty.tx_created_at = None;
    let mixed = [&local, &other_app, &empty];
    let inserted = sink.persist_records(&mixed).await.unwrap();
    assert_eq!(inserted.len(), 1);
    assert_eq!(inserted[0].isn, local.isn);
    for record in mixed {
        let row = setup
            .query_one(
                &format!(
                    "SELECT (SELECT count(*) FROM {schema}.history WHERE isn=$1::text::{schema}.isn),
                            (SELECT count(*) FROM {schema}.webhook_events WHERE isn=$1::text::{schema}.isn),
                            app_id, payload
                     FROM {schema}.rust_realtime_room_events WHERE room_id=$2"
                ),
                &[&record.isn.to_postgres_composite(), &record.isn.to_string()],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, i64>(0), i64::from(record.isn != empty.isn));
        assert_eq!(row.get::<_, i64>(1), i64::from(record.isn == local.isn));
        assert_eq!(row.get::<_, Uuid>(2), record.app_id);
        assert_eq!(
            row.get::<_, Value>(3),
            serde_json::to_value(record).unwrap()
        );
    }
    assert!(sink.persist_records(&mixed).await.unwrap().is_empty());

    // Duplicate inputs preserve the same exactly-once conflict behavior as two
    // sequential calls, including notification keys.
    let duplicate = record_at(first, 100_150);
    let before = notifier.0.lock().unwrap().len();
    let inserted = sink
        .persist_records(&[&duplicate, &duplicate])
        .await
        .unwrap();
    assert_eq!(inserted.len(), 1);
    assert_eq!(notifier.0.lock().unwrap().len(), before + 1);
    let existing_webhook = inserted[0].webhook_id;
    let added_webhook = Uuid::new_v4();
    setup.execute(
        &format!("INSERT INTO {schema}.webhooks (id, app_id, topics, id_attr_ids, actions, status, sink)
                  SELECT $1, app_id, topics, id_attr_ids, actions, status, sink
                  FROM {schema}.webhooks WHERE id=$2"),
        &[&added_webhook, &existing_webhook],
    ).await.unwrap();
    let inserted = sink
        .persist_records(&[&duplicate, &duplicate])
        .await
        .unwrap();
    assert_eq!(
        inserted.len(),
        1,
        "history replay must still match a newly configured webhook"
    );
    assert_eq!(inserted[0].webhook_id, added_webhook);
    assert_eq!(notifier.0.lock().unwrap().len(), before + 2);
    setup
        .execute(
            &format!("DELETE FROM {schema}.webhooks WHERE id=$1"),
            &[&added_webhook],
        )
        .await
        .unwrap();

    // A later publication failure must also roll back an earlier valid record
    // in the same chunk, including history, webhook rows and notifications.
    let valid = record_at(first, 100_200);
    let rejected = record_at(first, 100_201);
    setup
        .batch_execute(&format!(
            "ALTER TABLE {schema}.rust_realtime_room_events
             ADD CONSTRAINT reject_batch_publication CHECK (room_id <> '{}') NOT VALID",
            rejected.isn
        ))
        .await
        .unwrap();
    let before = notifier.0.lock().unwrap().len();
    assert!(sink.persist_records(&[&valid, &rejected]).await.is_err());
    assert_eq!(notifier.0.lock().unwrap().len(), before);
    for record in [&valid, &rejected] {
        let row = setup
            .query_one(
                &format!(
                    "SELECT (SELECT count(*) FROM {schema}.history WHERE isn=$1::text::{schema}.isn),
                            (SELECT count(*) FROM {schema}.webhook_events WHERE isn=$1::text::{schema}.isn),
                            (SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE room_id=$2)"
                ),
                &[&record.isn.to_postgres_composite(), &record.isn.to_string()],
            )
            .await
            .unwrap();
        assert_eq!(
            (
                row.get::<_, i64>(0),
                row.get::<_, i64>(1),
                row.get::<_, i64>(2)
            ),
            (0, 0, 0)
        );
    }
    setup
        .batch_execute(&format!(
            "ALTER TABLE {schema}.rust_realtime_room_events DROP CONSTRAINT reject_batch_publication"
        ))
        .await
        .unwrap();
    assert_eq!(
        sink.persist_records(&[&valid, &rejected])
            .await
            .unwrap()
            .len(),
        2
    );

    let doomed = pool.acquire().await.unwrap();
    let doomed_pid: i32 = doomed
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    assert!(
        setup
            .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(doomed);
    let reconnected = [record_at(first, 100_300), record_at(first, 100_301)];
    let references = reconnected.iter().collect::<Vec<_>>();
    let inserted = tokio::time::timeout(Duration::from_secs(20), sink.persist_records(&references))
        .await
        .expect("batch WAL persistence must reconnect within its bounded deadline")
        .unwrap();
    assert_eq!(inserted.len(), 2);
    let replacement = pool.acquire().await.unwrap();
    let row = replacement
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(row.get::<_, i32>(0), doomed_pid);
    assert_eq!(row.get::<_, String>(1), schema);
    drop(replacement);
    assert!(sink.persist_records(&references).await.unwrap().is_empty());
}

async fn benchmark_original_record_batches(
    setup: &tokio_postgres::Client,
    schema: &str,
    sink: &PostgresDurableInvalidationSink<Notifier>,
    first: &WalRecord,
) {
    setup
        .execute(
            &format!("UPDATE {schema}.webhooks SET status='disabled'"),
            &[],
        )
        .await
        .unwrap();
    for round in 0..4_u64 {
        let order = if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        };
        for batched in order {
            // Both paths use the identical record template and production writer.
            // Generate outside the timed region; reserve disjoint ISNs for every run.
            let records = (0..5000_u64)
                .map(|n| {
                    record_at(
                        first,
                        10_000_000 + round * 10_000 + u64::from(batched) * 5000 + n,
                    )
                })
                .collect::<Vec<_>>();
            let references = records.iter().collect::<Vec<_>>();
            let started = std::time::Instant::now();
            if batched {
                for chunk in references.chunks(32) {
                    assert!(sink.persist_records(chunk).await.unwrap().is_empty());
                }
            } else {
                for record in &records {
                    assert!(sink.persist_record(record).await.unwrap().is_empty());
                }
            }
            eprintln!(
                "durable-original-record-burst round={round} batched={batched} records=5000 chunk_size=32 elapsed_ms={:.3}",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    setup
        .execute(
            &format!("UPDATE {schema}.webhooks SET status='active'"),
            &[],
        )
        .await
        .unwrap();
}

fn record_at(template: &WalRecord, sequence: u64) -> WalRecord {
    let mut record = template.clone();
    record.tx_id = i64::try_from(sequence).unwrap();
    record.isn = Isn::new(2, Lsn::from_u64(sequence));
    record.next_lsn = record.isn.lsn();
    record
}

fn create_record(app_id: Uuid, entity: Uuid, id_attr: Uuid, name_attr: Uuid) -> WalRecord {
    let isn = Isn::new(2, Lsn::from_u64(100));
    let mut attributes = serde_json::Map::new();
    attributes.insert(id_attr.to_string(), json!(entity));
    attributes.insert(name_attr.to_string(), json!("Ada"));
    WalRecord {
        app_id,
        tx_id: 42,
        isn,
        previous_isn: None,
        tx_created_at: Some(Utc::now()),
        tx_bytes: 100,
        next_lsn: isn.lsn(),
        attr_changes: Vec::new(),
        ident_changes: Vec::new(),
        triple_changes: vec![
            inserted_triple(entity, id_attr, &json!(entity)),
            inserted_triple(entity, name_attr, &json!("Ada")),
        ],
        messages: vec![WalEntry {
            action: WalAction::Message,
            tx_bytes: 10,
            table: None,
            columns: Vec::new(),
            identity: Vec::new(),
            prefix: Some("update_ents".to_owned()),
            content: Some(json!([["users", entity, attributes]]).to_string()),
            lsn: None,
            nextlsn: None,
        }],
        wal_logs: Vec::new(),
        combined_count: 1,
    }
}

fn inserted_triple(entity: Uuid, attr: Uuid, value: &Value) -> WalEntry {
    WalEntry {
        action: WalAction::Insert,
        tx_bytes: 10,
        table: Some("triples".to_owned()),
        columns: vec![
            WalColumn {
                name: "entity_id".to_owned(),
                value: json!(entity),
            },
            WalColumn {
                name: "attr_id".to_owned(),
                value: json!(attr),
            },
            WalColumn {
                name: "value".to_owned(),
                value: Value::String(value.to_string()),
            },
            WalColumn {
                name: "pg_size".to_owned(),
                value: json!(10),
            },
        ],
        identity: Vec::new(),
        prefix: None,
        content: None,
        lsn: None,
        nextlsn: None,
    }
}
