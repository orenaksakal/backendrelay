use std::collections::HashMap;
use std::future::Future;
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::{Body, to_bytes};
use chrono::Utc;
use instant_server::{
    AuroraClone, AuroraCloneError, AuroraCloneLifecycle, BackupJobOutcome, NightlyBackupRun,
    PostgresBackupWorker, PostgresBackupWorkerPool, PostgresFailoverGate,
    PostgresNightlyBackupWorker, PostgresPool, StorageObjectMetadata, StorageObjectStore,
    StoragePutRequest,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify};
use tokio_postgres::{Client, NoTls, types::Json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Default)]
struct MemoryObjects(StdMutex<HashMap<String, (StoragePutRequest, Vec<u8>)>>);

#[derive(Default)]
struct BlockingObjects {
    started: Notify,
}

struct LocalClone {
    database_url: String,
    deleted: AtomicBool,
}

impl AuroraCloneLifecycle for LocalClone {
    fn create(
        &self,
        _source_cluster_id: String,
    ) -> impl Future<Output = Result<AuroraClone, AuroraCloneError>> + Send {
        std::future::ready(Ok(AuroraClone {
            cluster_id: "local-clone".to_owned(),
            instance_id: "local-clone-0".to_owned(),
            snapshot_time: Utc::now(),
            database_url: self.database_url.clone(),
        }))
    }

    fn delete(
        &self,
        _clone: AuroraClone,
    ) -> impl Future<Output = Result<(), AuroraCloneError>> + Send {
        self.deleted.store(true, Ordering::Release);
        std::future::ready(Ok(()))
    }

    fn logical_start_lsn_query(&self) -> &'static str {
        "SELECT pg_current_wal_lsn()::text"
    }

    fn requires_exported_snapshot(&self) -> bool {
        true
    }
}

impl StorageObjectStore for BlockingObjects {
    async fn put(
        &self,
        _request: StoragePutRequest,
        _body: Body,
    ) -> Result<StorageObjectMetadata, String> {
        self.started.notify_one();
        std::future::pending().await
    }

    fn delete(&self, _key: String) -> impl Future<Output = Result<(), String>> + Send {
        std::future::ready(Ok(()))
    }

    fn signed_download_url(
        &self,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        std::future::ready(Ok(format!("memory://{key}")))
    }
}

impl StorageObjectStore for MemoryObjects {
    async fn put(
        &self,
        request: StoragePutRequest,
        body: Body,
    ) -> Result<StorageObjectMetadata, String> {
        let bytes = to_bytes(body, usize::MAX)
            .await
            .map_err(|error| error.to_string())?
            .to_vec();
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.0
            .lock()
            .unwrap()
            .insert(request.key.clone(), (request.clone(), bytes));
        Ok(StorageObjectMetadata {
            size,
            content_type: request.content_type,
            content_disposition: request.content_disposition,
            etag: None,
            last_modified_ms: None,
        })
    }

    fn delete(&self, key: String) -> impl Future<Output = Result<(), String>> + Send {
        self.0.lock().unwrap().remove(&key);
        std::future::ready(Ok(()))
    }

    fn signed_download_url(
        &self,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        std::future::ready(Ok(format!("memory://{key}")))
    }
}

async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

fn schema_name() -> String {
    format!("backup_worker_{}", Uuid::new_v4().simple())
}

fn scoped_url(url: &str, schema: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}%2Cpublic")
}

async fn setup(client: &Client, schema: &str) {
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             SET search_path TO {schema}, public;
             CREATE TABLE apps (
               id uuid PRIMARY KEY, title text NOT NULL, creator_id uuid,
               deletion_marked_at timestamptz
             );
             CREATE TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, value_type text NOT NULL,
               cardinality text NOT NULL, is_unique boolean NOT NULL DEFAULT false,
               is_indexed boolean NOT NULL DEFAULT false,
               is_required boolean NOT NULL DEFAULT false, etype text NOT NULL,
               label text NOT NULL, reverse_etype text, reverse_label text,
               checked_data_type text, on_delete text, on_delete_reverse text,
               deletion_marked_at timestamptz
             );
             CREATE TABLE triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
               value jsonb NOT NULL, value_md5 text NOT NULL, created_at bigint NOT NULL,
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5)
             );
             CREATE TABLE rules (app_id uuid PRIMARY KEY, code jsonb NOT NULL);
             CREATE TABLE webhooks (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, id_attr_ids uuid[],
               actions text[] NOT NULL, status text NOT NULL, sink jsonb NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE app_email_senders (id uuid PRIMARY KEY, email text NOT NULL);
             CREATE TABLE app_email_templates (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, email_type text NOT NULL,
               sender_id uuid, body text NOT NULL, subject text NOT NULL, name text
             );
             CREATE TABLE app_authorized_redirect_origins (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, service text NOT NULL,
               params text[] NOT NULL
             );
             CREATE TABLE triples_size_aggregate (
               app_id uuid NOT NULL, attr_id uuid NOT NULL, pg_size bigint NOT NULL,
               files_size bigint NOT NULL
             );
             CREATE TABLE app_backups (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, backup_at timestamptz NOT NULL,
               isn isn,
               storage_prefix text NOT NULL, files_size bigint, db_size bigint,
               uncompressed_size bigint, description text, expires_at timestamptz,
               deletion_marked_at timestamptz
             );
             CREATE TABLE app_backup_jobs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, app_backup_id uuid,
               worker_id text, job_status text NOT NULL, description text, error text,
               work_estimate bigint, work_completed bigint,
               created_at timestamptz NOT NULL DEFAULT now(), done_at timestamptz,
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE UNIQUE INDEX one_live_backup_per_app ON app_backup_jobs(app_id)
               WHERE job_status IN ('waiting', 'processing');
             CREATE TABLE backup_jobs (
               id uuid PRIMARY KEY, isn isn NOT NULL, backup_at timestamptz NOT NULL,
               machine_id uuid NOT NULL, max_app_id uuid, triples_processed bigint,
               apps_processed bigint, completed_at timestamptz
             );
             CREATE TABLE grabs (id text PRIMARY KEY);
             CREATE TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL);
             CREATE TABLE attr_sketches (app_id uuid NOT NULL, total bigint NOT NULL);
             CREATE FUNCTION aurora_volume_logical_start_lsn() RETURNS pg_lsn
               LANGUAGE SQL IMMUTABLE AS 'SELECT ''0/10''::pg_lsn';"
        ))
        .await
        .unwrap();
    client
        .batch_execute(include_str!(
            "../migrations/142_app_backup_schedules.up.sql"
        ))
        .await
        .unwrap();
}

async fn set_search_path(client: &Client, schema: &str) {
    client
        .batch_execute(&format!("SET search_path TO {schema}, public"))
        .await
        .unwrap();
}

async fn insert_triple(
    client: &Client,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    value: Value,
    created_at: i64,
) {
    client
        .execute(
            "INSERT INTO triples
               (app_id, entity_id, attr_id, value, value_md5, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &app_id,
                &entity_id,
                &attr_id,
                &Json(value),
                &Uuid::new_v4().to_string(),
                &created_at,
            ],
        )
        .await
        .unwrap();
}

fn decompress(bytes: &[u8]) -> Vec<u8> {
    let mut decoder = zstd::stream::read::Decoder::new(bytes).unwrap();
    let mut output = Vec::new();
    decoder.read_to_end(&mut output).unwrap();
    output
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn worker_streams_restore_compatible_zstd_objects_and_completes_atomically() {
    let Some(url) = instant_test_support::test_database_url(
        "worker_streams_restore_compatible_zstd_objects_and_completes_atomically",
    ) else {
        return;
    };
    let control = connect(&url).await;
    let snapshot = connect(&url).await;
    let schema = schema_name();
    setup(&control, &schema).await;
    set_search_path(&snapshot, &schema).await;
    let app_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let name_attr = Uuid::new_v4();
    let tags_attr = Uuid::new_v4();
    control
        .execute(
            "INSERT INTO apps (id, title) VALUES ($1, 'Backup source')",
            &[&app_id],
        )
        .await
        .unwrap();
    control
        .execute(
            "INSERT INTO attrs
               (id, app_id, value_type, cardinality, is_unique, is_indexed,
                is_required, etype, label, checked_data_type)
             VALUES
               ($1, $4, 'blob', 'one', true, false, false, 'items', 'id', NULL),
               ($2, $4, 'blob', 'one', false, true, true, 'items', 'name', 'string'),
               ($3, $4, 'blob', 'many', false, false, false, 'items', 'tags', 'string')",
            &[&id_attr, &name_attr, &tags_attr, &app_id],
        )
        .await
        .unwrap();
    insert_triple(
        &control,
        app_id,
        entity_id,
        id_attr,
        json!(entity_id),
        1_234,
    )
    .await;
    insert_triple(&control, app_id, entity_id, name_attr, json!("Ada"), 2_000).await;
    insert_triple(&control, app_id, entity_id, tags_attr, json!("a"), 2_001).await;
    insert_triple(&control, app_id, entity_id, tags_attr, json!("b"), 2_002).await;
    control
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({"items": {"allow": {"view": "true"}}})),
            ],
        )
        .await
        .unwrap();
    control
        .execute(
            "INSERT INTO app_authorized_redirect_origins
               (id, app_id, service, params) VALUES ($1, $2, 'generic', $3)",
            &[&Uuid::new_v4(), &app_id, &vec!["app.example.test"]],
        )
        .await
        .unwrap();
    control
        .execute(
            "INSERT INTO triples_size_aggregate (app_id, attr_id, pg_size, files_size)
             VALUES ($1, $2, 100, 25)",
            &[&app_id, &id_attr],
        )
        .await
        .unwrap();
    let job_id = Uuid::new_v4();
    control
        .execute(
            "INSERT INTO app_backup_jobs
               (id, app_id, job_status, description, work_estimate)
             VALUES ($1, $2, 'waiting', 'Requested backup', 4)",
            &[&job_id, &app_id],
        )
        .await
        .unwrap();

    let objects = Arc::new(MemoryObjects::default());
    let control = Arc::new(Mutex::new(control));
    let snapshot = Arc::new(Mutex::new(snapshot));
    let worker = PostgresBackupWorker::with_shared_clients(
        Arc::clone(&control),
        snapshot,
        Arc::clone(&objects),
        "backup-test-worker",
    );
    let outcome = worker.run_job(job_id).await.unwrap();
    let BackupJobOutcome::Completed { backup_id, triples } = outcome else {
        panic!("backup did not complete: {outcome:?}");
    };
    assert_eq!(triples, 4);

    let stored = objects.0.lock().unwrap().clone();
    assert_eq!(stored.len(), 2);
    for (request, _) in stored.values() {
        assert_eq!(request.content_encoding.as_deref(), Some("zstd"));
        assert_eq!(request.tags.get("expire").map(String::as_str), Some("true"));
    }
    let config_bytes = stored
        .iter()
        .find(|(key, _)| key.ends_with("/config.json"))
        .map(|(_, (_, bytes))| decompress(bytes))
        .unwrap();
    let config: Value = serde_json::from_slice(&config_bytes).unwrap();
    assert_eq!(config["title"], "Backup source");
    assert_eq!(config["tripleCount"], 4);
    assert_eq!(config["counts"]["items"], 1);
    assert_eq!(
        config["schema"]["entities"]["items"]["attrs"]["name"]["valueType"],
        "string"
    );
    let entity_bytes = stored
        .iter()
        .find(|(key, _)| key.contains("/entities/items.jsonl"))
        .map(|(_, (_, bytes))| decompress(bytes))
        .unwrap();
    let entity: Value = serde_json::from_slice(
        entity_bytes
            .split(|byte| *byte == b'\n')
            .find(|line| !line.is_empty())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(entity["createdAt"], 1_234);
    assert_eq!(entity["entity"]["name"], "Ada");
    assert_eq!(entity["entity"]["tags"], json!(["a", "b"]));

    let canceled_id = Uuid::new_v4();
    let recovery_id = Uuid::new_v4();
    let stuck_id = Uuid::new_v4();
    {
        let database = control.lock().await;
        let stuck_app_id = Uuid::new_v4();
        database
            .execute(
                "INSERT INTO app_backup_jobs (id, app_id, job_status)
                 VALUES ($1, $4, 'cancelled'), ($2, $4, 'waiting'), ($3, $5, 'processing')",
                &[
                    &canceled_id,
                    &recovery_id,
                    &stuck_id,
                    &app_id,
                    &stuck_app_id,
                ],
            )
            .await
            .unwrap();
        database
            .execute(
                "UPDATE app_backup_jobs SET updated_at = now() - interval '11 minutes'
                 WHERE id = $1",
                &[&stuck_id],
            )
            .await
            .unwrap();
    }
    assert_eq!(
        worker.run_job(canceled_id).await.unwrap(),
        BackupJobOutcome::Cancelled
    );
    let recovered = worker.run_available_jobs(10).await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].0, recovery_id);
    assert!(matches!(
        recovered[0].1,
        BackupJobOutcome::Completed { triples: 4, .. }
    ));
    assert_eq!(worker.reclaim_stuck_jobs().await.unwrap(), 1);

    let control = control.lock().await;
    let row = control
        .query_one(
            "SELECT job_status, app_backup_id, work_completed FROM app_backup_jobs WHERE id = $1",
            &[&job_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "completed");
    assert_eq!(row.get::<_, Option<Uuid>>(1), Some(backup_id));
    assert_eq!(row.get::<_, Option<i64>>(2), Some(4));
    control
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
async fn independent_workers_claim_distinct_queue_heads() {
    let Some(url) =
        instant_test_support::test_database_url("independent_workers_claim_distinct_queue_heads")
    else {
        return;
    };
    let coordinator = connect(&url).await;
    let schema = schema_name();
    setup(&coordinator, &schema).await;
    let mut worker_clients = Vec::new();
    for _ in 0..4 {
        let client = connect(&url).await;
        set_search_path(&client, &schema).await;
        worker_clients.push(client);
    }
    let first_app = Uuid::new_v4();
    let second_app = Uuid::new_v4();
    let first_job = Uuid::new_v4();
    let second_job = Uuid::new_v4();
    coordinator
        .execute(
            "INSERT INTO apps (id, title) VALUES ($1, 'First'), ($2, 'Second')",
            &[&first_app, &second_app],
        )
        .await
        .unwrap();
    coordinator
        .execute(
            "INSERT INTO app_backup_jobs (id, app_id, job_status, work_estimate)
             VALUES ($1, $2, 'waiting', 0), ($3, $4, 'waiting', 0)",
            &[&first_job, &first_app, &second_job, &second_app],
        )
        .await
        .unwrap();
    let objects = Arc::new(MemoryObjects::default());
    let second_snapshot = worker_clients.pop().unwrap();
    let second_control = worker_clients.pop().unwrap();
    let first_snapshot = worker_clients.pop().unwrap();
    let first_control = worker_clients.pop().unwrap();
    let first = PostgresBackupWorker::new(
        first_control,
        first_snapshot,
        Arc::clone(&objects),
        "parallel-first",
    );
    let second = PostgresBackupWorker::new(
        second_control,
        second_snapshot,
        Arc::clone(&objects),
        "parallel-second",
    );
    let (first, second) = tokio::join!(first.run_next_job(), second.run_next_job());
    let claimed = [first.unwrap().unwrap().0, second.unwrap().unwrap().0]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(claimed.len(), 2);
    assert!(claimed.contains(&first_job));
    assert!(claimed.contains(&second_job));
    assert_eq!(objects.0.lock().unwrap().len(), 2);
    coordinator
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
async fn pooled_worker_replaces_terminated_control_and_snapshot_backends() {
    let Some(url) = instant_test_support::test_database_url(
        "pooled_worker_replaces_terminated_control_and_snapshot_backends",
    ) else {
        return;
    };
    let coordinator = connect(&url).await;
    let schema = schema_name();
    setup(&coordinator, &schema).await;
    let scoped_url = scoped_url(&url, &schema);
    let control = PostgresPool::connect(&scoped_url, 1).await.unwrap();
    let snapshot = PostgresPool::connect(&scoped_url, 1).await.unwrap();
    let control_lease = control.acquire().await.unwrap();
    let snapshot_lease = snapshot.acquire().await.unwrap();
    let control_pid = control_lease
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    let snapshot_pid = snapshot_lease
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    for pid in [control_pid, snapshot_pid] {
        assert!(
            coordinator
                .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
    }
    drop(control_lease);
    drop(snapshot_lease);

    let app_id = Uuid::new_v4();
    let job_id = Uuid::new_v4();
    coordinator
        .execute(
            "INSERT INTO apps (id, title) VALUES ($1, 'Reconnect')",
            &[&app_id],
        )
        .await
        .unwrap();
    coordinator
        .execute(
            "INSERT INTO app_backup_jobs (id, app_id, job_status, work_estimate)
             VALUES ($1, $2, 'waiting', 0)",
            &[&job_id, &app_id],
        )
        .await
        .unwrap();
    let worker = PostgresBackupWorker::from_pools(
        control.clone(),
        snapshot.clone(),
        Arc::new(MemoryObjects::default()),
        "reconnect-worker",
    );
    assert!(matches!(
        worker.run_job(job_id).await.unwrap(),
        BackupJobOutcome::Completed { triples: 0, .. }
    ));
    let replacement_control = control.acquire().await.unwrap();
    let replacement_snapshot = snapshot.acquire().await.unwrap();
    assert_ne!(
        replacement_control
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        control_pid
    );
    assert_ne!(
        replacement_snapshot
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        snapshot_pid
    );
    drop(replacement_control);
    drop(replacement_snapshot);
    coordinator
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
async fn forced_pool_shutdown_requeues_owned_work() {
    let Some(url) =
        instant_test_support::test_database_url("forced_pool_shutdown_requeues_owned_work")
    else {
        return;
    };
    let coordinator = connect(&url).await;
    let schema = schema_name();
    setup(&coordinator, &schema).await;
    let scoped_url = scoped_url(&url, &schema);
    let control = connect(&scoped_url).await;
    let snapshot = connect(&scoped_url).await;
    let app_id = Uuid::new_v4();
    let job_id = Uuid::new_v4();
    coordinator
        .execute(
            "INSERT INTO apps (id, title) VALUES ($1, 'Interrupted')",
            &[&app_id],
        )
        .await
        .unwrap();
    coordinator
        .execute(
            "INSERT INTO app_backup_jobs (id, app_id, job_status, work_estimate)
             VALUES ($1, $2, 'waiting', 0)",
            &[&job_id, &app_id],
        )
        .await
        .unwrap();

    let objects = Arc::new(BlockingObjects::default());
    let worker = Arc::new(PostgresBackupWorker::new(
        control,
        snapshot,
        Arc::clone(&objects),
        "shutdown-worker",
    ));
    let pool =
        PostgresBackupWorkerPool::new(worker, Arc::clone(&objects), scoped_url, "shutdown-worker");
    pool.refresh_and_kick().await.unwrap();
    objects.started.notified().await;
    assert!(pool.shutdown(std::time::Duration::ZERO).await.unwrap());

    let row = coordinator
        .query_one(
            "SELECT job_status, worker_id FROM app_backup_jobs WHERE id = $1",
            &[&job_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "waiting");
    assert_eq!(row.get::<_, Option<String>>(1), None);
    coordinator
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn nightly_worker_replaces_terminated_control_backend_and_keeps_one_snapshot() {
    let Some(url) = instant_test_support::test_database_url(
        "nightly_worker_replaces_terminated_control_backend_and_keeps_one_snapshot",
    ) else {
        return;
    };
    let coordinator = connect(&url).await;
    let schema = schema_name();
    setup(&coordinator, &schema).await;
    let scoped_url = scoped_url(&url, &schema);
    let primary = PostgresPool::connect(&scoped_url, 1).await.unwrap();
    let doomed = primary.acquire().await.unwrap();
    let doomed_pid = doomed
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    assert!(
        coordinator
            .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert!(doomed.query_one("SELECT 1", &[]).await.is_err());
    drop(doomed);
    let gate_client = connect(&scoped_url).await;
    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    coordinator
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, 'creator@example.com')",
            &[&creator_id],
        )
        .await
        .unwrap();
    coordinator
        .execute(
            "INSERT INTO attrs
               (id, app_id, value_type, cardinality, is_unique, is_indexed,
                is_required, etype, label)
             VALUES ($1, $2, 'blob', 'one', true, false, false, 'items', 'id')",
            &[&id_attr, &app_id],
        )
        .await
        .unwrap();
    insert_triple(
        &coordinator,
        app_id,
        entity_id,
        id_attr,
        json!(entity_id),
        1,
    )
    .await;
    coordinator
        .execute(
            "INSERT INTO apps (id, title, creator_id) VALUES ($1, 'Nightly', $2)",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();

    let objects = Arc::new(MemoryObjects::default());
    let clones = Arc::new(LocalClone {
        database_url: scoped_url,
        deleted: AtomicBool::new(false),
    });
    let worker = PostgresNightlyBackupWorker::new(
        primary.clone(),
        Arc::clone(&objects),
        Arc::clone(&clones),
        Arc::new(PostgresFailoverGate::new(gate_client).with_config_app_id(Uuid::new_v4())),
        "source-cluster",
        Uuid::new_v4(),
        "ephemeral@example.com",
    );
    let date = Utc::now().date_naive();
    let run = worker.run(date, &CancellationToken::new()).await.unwrap();
    let NightlyBackupRun::Completed(report) = run else {
        panic!("nightly backup did not run: {run:?}");
    };
    assert_eq!(report.apps_processed, 1);
    assert_eq!(report.triples_processed, 1);
    assert!(report.failed_apps.is_empty());
    assert!(clones.deleted.load(Ordering::Acquire));
    assert_eq!(objects.0.lock().unwrap().len(), 2);
    let replacement = primary.acquire().await.unwrap();
    assert_ne!(
        replacement
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        doomed_pid
    );
    drop(replacement);
    assert_eq!(
        worker.run(date, &CancellationToken::new()).await.unwrap(),
        NightlyBackupRun::AlreadyOwned
    );
    let row = coordinator
        .query_one(
            "SELECT isn::text, completed_at IS NOT NULL, apps_processed, triples_processed
             FROM backup_jobs WHERE id = $1",
            &[&report.process_id],
        )
        .await
        .unwrap();
    let backup_isn = row.get::<_, String>(0);
    assert!(backup_isn.starts_with("(2,"));
    assert!(row.get::<_, bool>(1));
    assert_eq!(row.get::<_, Option<i64>>(2), Some(1));
    assert_eq!(row.get::<_, Option<i64>>(3), Some(1));
    let backup = coordinator
        .query_one(
            "SELECT isn::text, description, expires_at > now(), storage_prefix
             FROM app_backups WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap();
    assert_eq!(backup.get::<_, String>(0), backup_isn);
    assert_eq!(
        backup.get::<_, Option<String>>(1).as_deref(),
        Some("Automated Daily Snapshot")
    );
    assert!(backup.get::<_, bool>(2));
    assert_eq!(
        backup.get::<_, String>(3),
        format!("{app_id}/{}", report.process_id)
    );
    coordinator
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
async fn schedules_coalesce_missed_runs_skip_overlap_and_do_not_double_enqueue() {
    let Some(url) = instant_test_support::test_database_url(
        "schedules_coalesce_missed_runs_skip_overlap_and_do_not_double_enqueue",
    ) else {
        return;
    };
    let client = connect(&url).await;
    let schema = schema_name();
    setup(&client, &schema).await;
    let due = Uuid::new_v4();
    let paused = Uuid::new_v4();
    let active = Uuid::new_v4();
    let future = Uuid::new_v4();
    let deleted = Uuid::new_v4();
    for app in [due, paused, active, future, deleted] {
        client
            .execute(
                "INSERT INTO apps(id,title) VALUES ($1,'Scheduled test')",
                &[&app],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO app_backup_schedules(app_id,cron,enabled,next_run_at)
            VALUES ($1,'0 * * * *',$2,now() + $3 * interval '1 hour')",
                &[
                    &app,
                    &(app != paused),
                    &(if app == future { 1.0_f64 } else { -48.0_f64 }),
                ],
            )
            .await
            .unwrap();
    }
    client
        .execute(
            "INSERT INTO app_backup_jobs(id,app_id,job_status) VALUES ($1,$2,'processing')",
            &[&Uuid::new_v4(), &active],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE apps SET deletion_marked_at=now() WHERE id=$1",
            &[&deleted],
        )
        .await
        .unwrap();
    let scoped = scoped_url(&url, &schema);
    let make_worker = || async {
        PostgresBackupWorker::from_pools(
            PostgresPool::connect(&scoped, 1).await.unwrap(),
            PostgresPool::connect(&scoped, 1).await.unwrap(),
            Arc::new(MemoryObjects::default()),
            Uuid::new_v4().to_string(),
        )
    };
    let a = make_worker().await;
    let b = make_worker().await;
    let (one, two) = tokio::join!(a.enqueue_scheduled_backups(), b.enqueue_scheduled_backups());
    one.unwrap();
    two.unwrap();
    a.enqueue_scheduled_backups().await.unwrap();
    let jobs = client
        .query("SELECT app_id FROM app_backup_jobs ORDER BY app_id", &[])
        .await
        .unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().any(|r| r.get::<_, Uuid>(0) == due));
    assert!(jobs.iter().any(|r| r.get::<_, Uuid>(0) == active));
    let rows = client.query("SELECT app_id,next_run_at>now(),last_enqueued_at IS NOT NULL FROM app_backup_schedules",&[]).await.unwrap();
    for row in rows {
        let id: Uuid = row.get(0);
        assert_eq!(row.get::<_, bool>(1), id != paused && id != deleted);
        assert_eq!(row.get::<_, bool>(2), id == due);
    }
    drop(a);
    drop(b);
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
