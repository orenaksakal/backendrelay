use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use chrono::{DateTime, Utc};
use futures_util::{StreamExt as _, TryStreamExt as _, pin_mut};
use instant_http::{ApiError, DashboardResponseBody};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_postgres::types::Json;
use tokio_postgres::{Client, IsolationLevel};
use tokio_util::io::ReaderStream;
use tokio_util::io::{StreamReader, SyncIoBridge};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zstd::stream::write::Encoder;

use crate::admin_storage::{StorageObjectStore, StoragePutRequest, object_key};
use crate::dashboard_control::{
    DashboardBackupStreamer, DashboardControlHookOperation, DashboardControlHooks,
};
use crate::postgres_pool::PostgresClientSource;
use crate::{PostgresPool, PostgresPoolError};

const DEFAULT_DESCRIPTION: &str = "On-demand backup";
const DEFAULT_EXPIRY_DAYS: i64 = 30;
const DEFAULT_WORKER_COUNT: usize = 3;
const MAX_WORKER_COUNT: usize = 32;
const MAX_BACKUP_FILES_INDEX_BYTES: usize = 64 * 1024 * 1024;
const MAX_BACKUP_FILE_ROW_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupObjectEntry {
    pub key: String,
    pub size: u64,
}

/// Object operations needed by the dashboard backup download routes.
pub trait BackupObjectStore: Send + Sync + 'static {
    fn list_backup_objects(
        &self,
        prefix: String,
    ) -> impl Future<Output = Result<Vec<BackupObjectEntry>, String>> + Send;

    fn sign_backup_object(
        &self,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send;

    fn sign_backup_object_for_app(
        &self,
        app_id: Uuid,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        let _ = app_id;
        self.sign_backup_object(key)
    }

    fn read_backup_object(
        &self,
        key: String,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, String>> + Send;

    fn stream_backup_object(
        &self,
        key: String,
    ) -> impl Future<Output = Result<Option<Body>, String>> + Send {
        async move {
            self.read_backup_object(key, usize::MAX)
                .await
                .map(|body| body.map(Body::from))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackupJobOutcome {
    NotClaimed,
    Completed { backup_id: Uuid, triples: i64 },
    Cancelled,
    Errored { error: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledBackupOutcome {
    pub backup_id: Option<Uuid>,
    pub triples: i64,
}

/// Retention policy for one app produced by the fleet-owned scheduled backup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledBackupOptions {
    pub expires_at: DateTime<Utc>,
    pub expire_objects: bool,
}

#[derive(Debug, Error)]
pub enum BackupWorkerError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
    #[error("object store error: {0}")]
    ObjectStore(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("backup worker connection error: {0}")]
    Connection(String),
    #[error("backup was cancelled")]
    Cancelled,
    #[error("invalid PostgreSQL exported snapshot identifier")]
    InvalidSnapshotId,
}

pub struct PostgresBackupWorker<S> {
    control: PostgresClientSource,
    snapshot: PostgresClientSource,
    objects: Arc<S>,
    worker_id: String,
    expiry_days: Option<i64>,
    snapshot_id: Option<Arc<str>>,
}

impl<S> PostgresBackupWorker<S>
where
    S: StorageObjectStore,
{
    #[must_use]
    pub fn new(
        control: Client,
        snapshot: Client,
        objects: Arc<S>,
        worker_id: impl Into<String>,
    ) -> Self {
        Self::with_shared_clients(
            Arc::new(Mutex::new(control)),
            Arc::new(Mutex::new(snapshot)),
            objects,
            worker_id,
        )
    }

    #[must_use]
    pub fn with_shared_clients(
        control: Arc<Mutex<Client>>,
        snapshot: Arc<Mutex<Client>>,
        objects: Arc<S>,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            control: PostgresClientSource::fixed_shared(control),
            snapshot: PostgresClientSource::fixed_shared(snapshot),
            objects,
            worker_id: worker_id.into(),
            expiry_days: None,
            snapshot_id: None,
        }
    }

    /// Uses a reconnecting control pool while retaining a caller-owned
    /// snapshot session. Nightly backups need this mixed ownership: control
    /// writes must survive backend replacement, while each clone reader must
    /// keep its imported exported snapshot for the full app backup.
    #[must_use]
    pub fn with_control_pool(
        control: PostgresPool,
        snapshot: Arc<Mutex<Client>>,
        objects: Arc<S>,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            control: PostgresClientSource::pooled(control),
            snapshot: PostgresClientSource::fixed_shared(snapshot),
            objects,
            worker_id: worker_id.into(),
            expiry_days: None,
            snapshot_id: None,
        }
    }

    #[must_use]
    pub fn from_pools(
        control: PostgresPool,
        snapshot: PostgresPool,
        objects: Arc<S>,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            control: PostgresClientSource::pooled(control),
            snapshot: PostgresClientSource::pooled(snapshot),
            objects,
            worker_id: worker_id.into(),
            expiry_days: None,
            snapshot_id: None,
        }
    }

    #[must_use]
    pub fn with_expiry_days(mut self, days: i64) -> Self {
        self.expiry_days = Some(days.clamp(1, 30));
        self
    }

    /// Imports the supplied PostgreSQL snapshot into every per-app
    /// repeatable-read transaction. The exporting transaction must remain
    /// open until this worker finishes.
    #[must_use]
    pub fn with_snapshot_id(mut self, snapshot_id: Arc<str>) -> Self {
        self.snapshot_id = Some(snapshot_id);
        self
    }

    async fn pool_state(&self) -> Result<(usize, bool), BackupWorkerError> {
        let client = self.control.acquire().await?;
        let row = client
            .query_one(
                "SELECT (
                   SELECT flag_value.value
                   FROM attrs setting_attr
                   JOIN triples flag_setting
                     ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
                   JOIN attrs value_attr
                     ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
                    AND value_attr.label = 'value'
                   JOIN triples flag_value
                     ON flag_value.app_id = $1
                    AND flag_value.entity_id = flag_setting.entity_id
                    AND flag_value.attr_id = value_attr.id
                   WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
                     AND setting_attr.label = 'setting'
                     AND flag_setting.value = to_jsonb('on-demand-backup-worker-count'::text)
                   LIMIT 1
                 ), EXISTS (
                   SELECT 1 FROM app_backup_jobs
                   WHERE worker_id IS NULL AND job_status = 'waiting'
                 )",
                &[&crate::INSTANT_CONFIG_APP_ID],
            )
            .await?;
        let value = row.get::<_, Option<Value>>(0);
        Ok((backup_worker_count(value.as_ref()), row.get(1)))
    }

    /// Claims and runs one on-demand app backup.
    ///
    /// # Errors
    ///
    /// Returns infrastructure failures that prevent the job outcome from being
    /// recorded. Backup failures that are successfully persisted are returned
    /// as [`BackupJobOutcome::Errored`].
    #[tracing::instrument(name = "app-backup-jobs/run-job", skip_all, fields(job.id = %job_id))]
    pub async fn run_job(&self, job_id: Uuid) -> Result<BackupJobOutcome, BackupWorkerError> {
        let Some(job) = self.claim(job_id).await? else {
            return self.current_outcome(job_id).await;
        };
        match self.create_backup(&job).await {
            Ok(backup) => self.finish(&job, backup).await,
            Err(BackupWorkerError::Cancelled) => Ok(BackupJobOutcome::Cancelled),
            Err(error) => {
                let message = error.to_string();
                self.mark_error(&job, &message).await?;
                self.current_outcome(job_id).await
            }
        }
    }

    /// Claims the oldest waiting jobs, including work left behind by a lost
    /// queue notification or process restart.
    ///
    /// # Errors
    ///
    /// Returns database or backup infrastructure failures.
    pub async fn run_available_jobs(
        &self,
        limit: i64,
    ) -> Result<Vec<(Uuid, BackupJobOutcome)>, BackupWorkerError> {
        let limit = usize::try_from(limit.clamp(1, 1_000)).unwrap_or(1);
        let mut outcomes = Vec::with_capacity(limit);
        while outcomes.len() < limit {
            let Some(outcome) = self.run_next_job().await? else {
                break;
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// Atomically claims and executes the oldest available job.
    ///
    /// `SKIP LOCKED` lets independent worker connections claim different jobs
    /// without first selecting the same queue head.
    ///
    /// # Errors
    ///
    /// Returns database, snapshot, serialization, or object-store failures.
    #[tracing::instrument(name = "app-backup-jobs/run-next", skip_all)]
    pub async fn run_next_job(
        &self,
    ) -> Result<Option<(Uuid, BackupJobOutcome)>, BackupWorkerError> {
        let Some(job) = self.claim_next().await? else {
            return Ok(None);
        };
        let id = job.id;
        let outcome = match self.create_backup(&job).await {
            Ok(backup) => self.finish(&job, backup).await?,
            Err(BackupWorkerError::Cancelled) => BackupJobOutcome::Cancelled,
            Err(error) => {
                let message = error.to_string();
                self.mark_error(&job, &message).await?;
                self.current_outcome(id).await?
            }
        };
        Ok(Some((id, outcome)))
    }

    /// Enqueues due per-app schedules atomically, coalescing missed occurrences.
    ///
    /// # Errors
    /// Returns database or cron validation errors.
    pub async fn enqueue_scheduled_backups(&self) -> Result<(), BackupWorkerError> {
        let mut client = self.control.acquire().await?;
        let tx = client.transaction().await?;
        // Lock schedules and advance them with the enqueue in one transaction.
        // Missed occurrences coalesce into one run; an active backup skips this occurrence.
        let rows = tx
            .query(
                "SELECT app_id,cron FROM app_backup_schedules
            WHERE enabled AND next_run_at <= now()
              AND EXISTS (SELECT 1 FROM apps WHERE apps.id=app_backup_schedules.app_id AND apps.deletion_marked_at IS NULL)
            ORDER BY next_run_at
            LIMIT 20 FOR UPDATE SKIP LOCKED",
                &[],
            )
            .await?;
        for row in rows {
            let app_id: Uuid = row.get(0);
            let cron: String = row.get(1);
            let parsed = crate::backup_schedule::Cron::parse(&cron)
                .map_err(|e| BackupWorkerError::Connection(format!("{e:?}")))?;
            let next = crate::backup_schedule::next_cron(parsed, Utc::now())
                .await
                .map_err(|e| BackupWorkerError::Connection(format!("{e:?}")))?;
            let inserted = tx
                .execute(
                    "INSERT INTO app_backup_jobs(id,app_id,description,work_estimate,job_status)
                SELECT $1,$2,'Scheduled backup',coalesce(sum(total),0)::bigint,'waiting'
                FROM attr_sketches WHERE app_id=$2
                ON CONFLICT DO NOTHING",
                    &[&Uuid::new_v4(), &app_id],
                )
                .await?;
            tx.execute(
                "UPDATE app_backup_schedules SET next_run_at=$2,
                last_enqueued_at=CASE WHEN $3 THEN now() ELSE last_enqueued_at END WHERE app_id=$1",
                &[&app_id, &next, &(inserted > 0)],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Marks workers without a progress heartbeat for ten minutes as errored.
    ///
    /// # Errors
    ///
    /// Returns a database error if reclamation cannot be persisted.
    #[tracing::instrument(name = "app-backup-jobs/reclaim-stuck-jobs!", skip_all)]
    pub async fn reclaim_stuck_jobs(&self) -> Result<u64, BackupWorkerError> {
        let client = self.control.acquire().await?;
        Ok(client
            .execute(
                "UPDATE app_backup_jobs
                 SET job_status = 'errored', error = 'Backup timed out', done_at = now()
                 WHERE job_status = 'processing'
                   AND interval '10 minutes' < now() - updated_at",
                &[],
            )
            .await?)
    }

    async fn claim(&self, id: Uuid) -> Result<Option<BackupJob>, BackupWorkerError> {
        let client = self.control.acquire().await?;
        Ok(client
            .query_opt(
                "UPDATE app_backup_jobs
                 SET worker_id = $2, job_status = 'processing'
                 WHERE id = $1 AND worker_id IS NULL AND job_status = 'waiting'
                 RETURNING id, app_id, description",
                &[&id, &self.worker_id],
            )
            .await?
            .map(|row| BackupJob {
                id: row.get(0),
                app_id: row.get(1),
                description: row.get(2),
                backup_at: None,
                isn: None,
                storage_id: None,
                track_progress: true,
                expire_objects: true,
            }))
    }

    async fn claim_next(&self) -> Result<Option<BackupJob>, BackupWorkerError> {
        let client = self.control.acquire().await?;
        Ok(client
            .query_opt(
                "WITH candidate AS (
                   SELECT id FROM app_backup_jobs
                   WHERE worker_id IS NULL AND job_status = 'waiting'
                   ORDER BY created_at, id
                   FOR UPDATE SKIP LOCKED LIMIT 1
                 )
                 UPDATE app_backup_jobs jobs
                 SET worker_id = $1, job_status = 'processing'
                 FROM candidate
                 WHERE jobs.id = candidate.id
                 RETURNING jobs.id, jobs.app_id, jobs.description",
                &[&self.worker_id],
            )
            .await?
            .map(|row| BackupJob {
                id: row.get(0),
                app_id: row.get(1),
                description: row.get(2),
                backup_at: None,
                isn: None,
                storage_id: None,
                track_progress: true,
                expire_objects: true,
            }))
    }

    async fn current_outcome(&self, id: Uuid) -> Result<BackupJobOutcome, BackupWorkerError> {
        let client = self.control.acquire().await?;
        let row = client
            .query_opt(
                "SELECT job_status, app_backup_id, work_completed, error
                 FROM app_backup_jobs WHERE id = $1",
                &[&id],
            )
            .await?;
        Ok(match row {
            None => BackupJobOutcome::NotClaimed,
            Some(row) => match row.get::<_, String>(0).as_str() {
                "completed" => BackupJobOutcome::Completed {
                    backup_id: row.get::<_, Option<Uuid>>(1).unwrap_or_default(),
                    triples: row.get::<_, Option<i64>>(2).unwrap_or_default(),
                },
                "cancelled" | "canceled" => BackupJobOutcome::Cancelled,
                "errored" => BackupJobOutcome::Errored {
                    error: row.get::<_, Option<String>>(3).unwrap_or_default(),
                },
                _ => BackupJobOutcome::NotClaimed,
            },
        })
    }

    /// Creates and records one app artifact as part of an automated daily
    /// snapshot. This path deliberately does not touch `app_backup_jobs`;
    /// fleet-level progress belongs to the parent `backup_jobs` row.
    ///
    /// # Errors
    ///
    /// Returns snapshot, serialization, upload, or primary database failures.
    pub async fn run_scheduled_app(
        &self,
        process_id: Uuid,
        app_id: Uuid,
        backup_at: DateTime<Utc>,
        isn: instant_wal::Isn,
        options: ScheduledBackupOptions,
    ) -> Result<ScheduledBackupOutcome, BackupWorkerError> {
        let job = BackupJob {
            id: process_id,
            app_id,
            description: Some("Automated Daily Snapshot".to_owned()),
            backup_at: Some(backup_at),
            isn: Some(isn.to_string()),
            storage_id: Some(process_id),
            track_progress: false,
            expire_objects: options.expire_objects,
        };
        let artifact = self.create_backup(&job).await?;
        if artifact.triples == 0 {
            return Ok(ScheduledBackupOutcome {
                backup_id: None,
                triples: 0,
            });
        }
        self.finish_scheduled(&job, artifact, isn, options.expires_at)
            .await
    }

    #[allow(clippy::too_many_lines)]
    #[tracing::instrument(
        name = "backup/backup-app-on-primary",
        skip_all,
        fields(app.id = %job.app_id, backup.id = %job.id)
    )]
    async fn create_backup(&self, job: &BackupJob) -> Result<BackupArtifact, BackupWorkerError> {
        let storage_id = job.storage_id.unwrap_or_else(Uuid::new_v4);
        let work = BackupWorkDir::create()?;
        let mut source = self.snapshot.acquire().await?;
        let transaction = source
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        if let Some(snapshot_id) = self.snapshot_id.as_deref() {
            let import = postgres_snapshot_import_sql(snapshot_id)?;
            transaction.batch_execute(&import).await?;
        }
        let backup_at = job.backup_at.unwrap_or_else(Utc::now);
        let metadata = load_config_metadata(&transaction, job.app_id).await?;
        let mut streams = HashMap::<String, EntityStream>::new();
        let mut current_entity = None;
        let mut entities = BTreeMap::<String, Entity>::new();
        let mut triple_count = 0_i64;
        {
            let rows = transaction
                .query_raw(
                    "SELECT t.entity_id, t.value, t.created_at, a.etype, a.label,
                        a.cardinality = 'many'
                 FROM triples t JOIN attrs a ON a.id = t.attr_id
                 WHERE t.app_id = $1 AND a.deletion_marked_at IS NULL
                 ORDER BY t.entity_id, t.attr_id, t.value_md5",
                    [&job.app_id],
                )
                .await?;
            pin_mut!(rows);
            let mut progress = tokio::time::interval(Duration::from_secs(1));
            progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = progress.tick() => {
                        if !self.report_progress(job.id, triple_count).await? {
                            return Err(BackupWorkerError::Cancelled);
                        }
                    }
                    row = rows.next() => {
                        let Some(row) = row else { break };
                        let row = row?;
                        let entity_id = row.get::<_, Uuid>(0);
                        if current_entity.is_some_and(|current| current != entity_id) {
                            flush_entities(&work, &mut streams, &mut entities)?;
                        }
                        current_entity = Some(entity_id);
                        let value = row.get::<_, Value>(1);
                        let created_at = row.get::<_, i64>(2);
                        let etype = row.get::<_, String>(3);
                        let label = row.get::<_, String>(4);
                        let many = row.get::<_, bool>(5);
                        entities.entry(etype).or_default().insert(label, value, many, created_at);
                        triple_count = triple_count.saturating_add(1);
                    }
                }
            }
        }
        flush_entities(&work, &mut streams, &mut entities)?;
        transaction.commit().await?;
        drop(source);
        if !job.track_progress && triple_count == 0 {
            return Ok(BackupArtifact {
                storage_prefix: format!("{}/{storage_id}", job.app_id),
                backup_at,
                triples: 0,
                uncompressed_size: 0,
                files_size: metadata.files_size,
                db_size: metadata.db_size,
                _work: work,
            });
        }
        let mut counts = BTreeMap::new();
        let mut objects = Vec::new();
        let mut uncompressed_size = 0_i64;
        for (etype, stream) in streams {
            counts.insert(etype.clone(), stream.entities);
            uncompressed_size = uncompressed_size.saturating_add(stream.uncompressed);
            let path = stream.finish()?;
            objects.push(BackupObject {
                key: format!("{}/{storage_id}/entities/{etype}.jsonl", job.app_id),
                content_type: "application/x-jsonlines",
                path,
                track_progress: job.track_progress,
            });
        }
        let files_size = metadata.files_size;
        let db_size = metadata.db_size;
        let config = metadata.into_config(
            job.app_id,
            backup_at,
            job.description.as_deref().unwrap_or(DEFAULT_DESCRIPTION),
            job.isn.as_deref(),
            &counts,
            triple_count,
        );
        let config_bytes = serde_json::to_vec(&config)?;
        uncompressed_size =
            uncompressed_size.saturating_add(i64::try_from(config_bytes.len()).unwrap_or(i64::MAX));
        let config_path = work.path.join("config.json.zst");
        write_zstd(&config_path, &config_bytes)?;
        objects.push(BackupObject {
            key: format!("{}/{storage_id}/config.json", job.app_id),
            content_type: "application/json",
            path: config_path,
            track_progress: job.track_progress,
        });
        for object in &objects {
            self.upload(job.app_id, job.id, triple_count, job.expire_objects, object)
                .await?;
        }
        Ok(BackupArtifact {
            storage_prefix: format!("{}/{storage_id}", job.app_id),
            backup_at,
            triples: triple_count,
            uncompressed_size,
            files_size,
            db_size,
            _work: work,
        })
    }

    #[tracing::instrument(
        name = "backup/upload-to-s3",
        skip_all,
        fields(
            app.id = %app_id,
            backup.id = %job_id,
            object.key = %object.key,
        )
    )]
    async fn upload(
        &self,
        app_id: Uuid,
        job_id: Uuid,
        triples: i64,
        expire_objects: bool,
        object: &BackupObject,
    ) -> Result<(), BackupWorkerError> {
        if object.track_progress && !self.report_progress(job_id, triples).await? {
            return Err(BackupWorkerError::Cancelled);
        }
        let input = tokio::fs::File::open(&object.path)
            .await
            .map_err(io_error)?;
        let size = input.metadata().await.map_err(io_error)?.len();
        let upload = self.objects.put(
            StoragePutRequest {
                key: object.key.clone(),
                content_type: Some(object.content_type.to_owned()),
                content_disposition: None,
                content_encoding: Some("zstd".to_owned()),
                content_length: Some(size),
                tags: BTreeMap::from([("expire".to_owned(), expire_objects.to_string())]),
            },
            Body::from_stream(ReaderStream::new(input)),
        );
        pin_mut!(upload);
        let mut progress = tokio::time::interval(Duration::from_secs(1));
        progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                result = &mut upload => {
                    result.map_err(BackupWorkerError::ObjectStore)?;
                    return Ok(());
                }
                _ = progress.tick() => {
                    if object.track_progress && !self.report_progress(job_id, triples).await? {
                        return Err(BackupWorkerError::Cancelled);
                    }
                }
            }
        }
    }

    async fn report_progress(&self, id: Uuid, triples: i64) -> Result<bool, BackupWorkerError> {
        let client = self.control.acquire().await?;
        Ok(client
            .execute(
                "UPDATE app_backup_jobs SET work_completed = $3
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&id, &self.worker_id, &triples],
            )
            .await?
            > 0)
    }

    async fn finish(
        &self,
        job: &BackupJob,
        backup: BackupArtifact,
    ) -> Result<BackupJobOutcome, BackupWorkerError> {
        let mut client = self.control.acquire().await?;
        let transaction = client.transaction().await?;
        if transaction
            .query_opt(
                "SELECT 1 FROM app_backup_jobs
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'
                 FOR UPDATE",
                &[&job.id, &self.worker_id],
            )
            .await?
            .is_none()
        {
            return Ok(BackupJobOutcome::Cancelled);
        }
        let backup_id = Uuid::new_v4();
        let expiry_days = match self.expiry_days {
            Some(days) => days,
            None => dynamic_backup_expiry_days(&transaction).await?,
        };
        let expires_at = backup.backup_at + chrono::Duration::days(expiry_days);
        transaction
            .execute(
                "INSERT INTO app_backups
                   (id, app_id, backup_at, storage_prefix, files_size, db_size,
                    uncompressed_size, description, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                &[
                    &backup_id,
                    &job.app_id,
                    &backup.backup_at,
                    &backup.storage_prefix,
                    &backup.files_size,
                    &backup.db_size,
                    &backup.uncompressed_size,
                    &job.description.as_deref().unwrap_or(DEFAULT_DESCRIPTION),
                    &expires_at,
                ],
            )
            .await?;
        transaction
            .execute(
                "UPDATE app_backup_jobs
                 SET job_status = 'completed', app_backup_id = $3,
                     work_completed = $4, done_at = now()
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &self.worker_id, &backup_id, &backup.triples],
            )
            .await?;
        transaction.commit().await?;
        Ok(BackupJobOutcome::Completed {
            backup_id,
            triples: backup.triples,
        })
    }

    async fn finish_scheduled(
        &self,
        job: &BackupJob,
        backup: BackupArtifact,
        isn: instant_wal::Isn,
        expires_at: DateTime<Utc>,
    ) -> Result<ScheduledBackupOutcome, BackupWorkerError> {
        let client = self.control.acquire().await?;
        let backup_id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO app_backups
                   (id, app_id, isn, backup_at, storage_prefix, files_size, db_size,
                    uncompressed_size, description, expires_at)
                 VALUES ($1, $2, $3::text::isn, $4, $5, $6, $7, $8, $9, $10)",
                &[
                    &backup_id,
                    &job.app_id,
                    &isn.to_postgres_composite(),
                    &backup.backup_at,
                    &backup.storage_prefix,
                    &backup.files_size,
                    &backup.db_size,
                    &backup.uncompressed_size,
                    &job.description,
                    &expires_at,
                ],
            )
            .await?;
        Ok(ScheduledBackupOutcome {
            backup_id: Some(backup_id),
            triples: backup.triples,
        })
    }

    async fn mark_error(&self, job: &BackupJob, message: &str) -> Result<(), BackupWorkerError> {
        let client = self.control.acquire().await?;
        client
            .execute(
                "UPDATE app_backup_jobs
                 SET job_status = 'errored', error = $3, done_at = now()
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &self.worker_id, &message],
            )
            .await?;
        Ok(())
    }
}

pub(crate) fn postgres_snapshot_import_sql(snapshot_id: &str) -> Result<String, BackupWorkerError> {
    if snapshot_id.is_empty()
        || !snapshot_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(BackupWorkerError::InvalidSnapshotId);
    }
    Ok(format!("SET TRANSACTION SNAPSHOT '{snapshot_id}'"))
}

/// Live-resizable on-demand backup executor. Each active slot owns independent
/// control and repeatable-read snapshot connections, so the configured worker
/// count represents real backup concurrency rather than tasks queued behind a
/// shared client mutex.
pub struct PostgresBackupWorkerPool<S> {
    initial: Arc<PostgresBackupWorker<S>>,
    initial_available: AtomicBool,
    objects: Arc<S>,
    database_url: String,
    worker_prefix: String,
    desired: AtomicUsize,
    active: AtomicUsize,
    shutting_down: AtomicBool,
    force_cancellation: CancellationToken,
    idle: Notify,
    dispatch: Mutex<()>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    last_error: std::sync::Mutex<Option<String>>,
}

impl<S> PostgresBackupWorkerPool<S>
where
    S: StorageObjectStore,
{
    #[must_use]
    pub fn new(
        initial: Arc<PostgresBackupWorker<S>>,
        objects: Arc<S>,
        database_url: impl Into<String>,
        worker_prefix: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            initial,
            initial_available: AtomicBool::new(true),
            objects,
            database_url: database_url.into(),
            worker_prefix: worker_prefix.into(),
            desired: AtomicUsize::new(DEFAULT_WORKER_COUNT),
            active: AtomicUsize::new(0),
            shutting_down: AtomicBool::new(false),
            force_cancellation: CancellationToken::new(),
            idle: Notify::new(),
            dispatch: Mutex::new(()),
            tasks: Mutex::new(Vec::new()),
            last_error: std::sync::Mutex::new(None),
        })
    }

    /// Reloads the worker-count flag and starts enough independent slots to
    /// drain the durable queue. Idle slots exit immediately; running slots
    /// above a reduced limit retire after their current job.
    ///
    /// # Errors
    ///
    /// Returns config-query failures or a connection failure reported by a
    /// previously started slot.
    pub async fn refresh_and_kick(self: &Arc<Self>) -> Result<(), BackupWorkerError> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Ok(());
        }
        self.initial.enqueue_scheduled_backups().await?;
        let (desired, waiting) = self.initial.pool_state().await?;
        self.desired.store(desired, Ordering::Release);
        if let Some(error) = self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            return Err(BackupWorkerError::Connection(error));
        }
        if !waiting {
            return Ok(());
        }
        let _dispatch = self.dispatch.lock().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Ok(());
        }
        let active = self.active.load(Ordering::Acquire);
        let mut started = Vec::with_capacity(desired.saturating_sub(active));
        for ordinal in active..desired {
            self.active.fetch_add(1, Ordering::AcqRel);
            let initial = self
                .initial_available
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
                .then(|| Arc::clone(&self.initial));
            let pool = Arc::clone(self);
            started.push(tokio::spawn(async move {
                let outcome = match initial {
                    Some(worker) => run_backup_slot(&pool, &worker).await,
                    None => run_connected_backup_slot(&pool, ordinal).await,
                };
                if outcome.used_initial {
                    pool.initial_available.store(true, Ordering::Release);
                }
                if !outcome.retired {
                    pool.active.fetch_sub(1, Ordering::AcqRel);
                }
                if pool.active.load(Ordering::Acquire) == 0 {
                    pool.idle.notify_waiters();
                }
                if let Err(error) = outcome.result {
                    *pool
                        .last_error
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(error.to_string());
                }
            }));
        }
        let mut tasks = self.tasks.lock().await;
        tasks.retain(|task| !task.is_finished());
        tasks.extend(started);
        Ok(())
    }

    /// Stops accepting work, waits for active backups to finish, and then
    /// interrupts any slots that outlive the grace period. Interrupted durable
    /// jobs are returned to the waiting queue before database shutdown.
    ///
    /// Returns `true` when force cancellation was required.
    ///
    /// # Errors
    ///
    /// Returns a database error if interrupted ownership cannot be released.
    pub async fn shutdown(&self, grace: Duration) -> Result<bool, BackupWorkerError> {
        self.shutting_down.store(true, Ordering::Release);
        self.desired.store(0, Ordering::Release);
        {
            let _dispatch = self.dispatch.lock().await;
        }
        if tokio::time::timeout(grace, self.wait_until_idle())
            .await
            .is_ok()
        {
            return Ok(false);
        }

        self.force_cancellation.cancel();
        if tokio::time::timeout(Duration::from_secs(1), self.wait_until_idle())
            .await
            .is_err()
        {
            let mut tasks = self.tasks.lock().await;
            for task in &*tasks {
                task.abort();
            }
            let aborted = std::mem::take(&mut *tasks);
            drop(tasks);
            for task in aborted {
                let _ = task.await;
            }
        }
        self.release_owned_jobs().await?;
        Ok(true)
    }

    /// Reclaims stale ownership using the durable coordinator connection.
    ///
    /// # Errors
    ///
    /// Returns a database error if stale ownership cannot be updated.
    pub async fn reclaim_stuck_jobs(&self) -> Result<u64, BackupWorkerError> {
        self.initial.reclaim_stuck_jobs().await
    }

    async fn wait_until_idle(&self) {
        loop {
            let idle = self.idle.notified();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            idle.await;
        }
    }

    async fn release_owned_jobs(&self) -> Result<u64, BackupWorkerError> {
        let client = self.initial.control.acquire().await?;
        Ok(client
            .execute(
                "UPDATE app_backup_jobs
                 SET worker_id = NULL, job_status = 'waiting'
                 WHERE job_status = 'processing'
                   AND (worker_id = $1 OR worker_id LIKE $1 || '-%')",
                &[&self.worker_prefix],
            )
            .await?)
    }

    fn try_retire(&self) -> bool {
        loop {
            let active = self.active.load(Ordering::Acquire);
            let desired = self.desired.load(Ordering::Acquire);
            if active <= desired {
                return false;
            }
            if self
                .active
                .compare_exchange(active, active - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }
}

struct BackupSlotOutcome {
    result: Result<(), BackupWorkerError>,
    retired: bool,
    used_initial: bool,
}

async fn run_backup_slot<S>(
    pool: &PostgresBackupWorkerPool<S>,
    worker: &PostgresBackupWorker<S>,
) -> BackupSlotOutcome
where
    S: StorageObjectStore,
{
    loop {
        let next = tokio::select! {
            () = pool.force_cancellation.cancelled() => {
                return BackupSlotOutcome {
                    result: Ok(()),
                    retired: false,
                    used_initial: true,
                };
            }
            next = worker.run_next_job() => next,
        };
        match next {
            Ok(Some(_)) if pool.try_retire() => {
                return BackupSlotOutcome {
                    result: Ok(()),
                    retired: true,
                    used_initial: true,
                };
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                return BackupSlotOutcome {
                    result: Ok(()),
                    retired: false,
                    used_initial: true,
                };
            }
            Err(error) => {
                return BackupSlotOutcome {
                    result: Err(error),
                    retired: false,
                    used_initial: true,
                };
            }
        }
    }
}

async fn run_connected_backup_slot<S>(
    pool: &PostgresBackupWorkerPool<S>,
    ordinal: usize,
) -> BackupSlotOutcome
where
    S: StorageObjectStore,
{
    let result = async {
        let control = tokio::select! {
            () = pool.force_cancellation.cancelled() => return Ok(false),
            connected = PostgresPool::connect_maintenance(&pool.database_url, 1, "backup_control") => {
                connected?
            }
        };
        let snapshot =
            PostgresPool::connect_maintenance(&pool.database_url, 1, "backup_snapshot").await?;
        let worker = PostgresBackupWorker::from_pools(
            control,
            snapshot,
            Arc::clone(&pool.objects),
            format!("{}-{ordinal}", pool.worker_prefix),
        );
        async {
            loop {
                let next = tokio::select! {
                    () = pool.force_cancellation.cancelled() => return Ok(false),
                    next = worker.run_next_job() => next?,
                };
                match next {
                    Some(_) if pool.try_retire() => return Ok(true),
                    Some(_) => {}
                    None => return Ok(false),
                }
            }
        }
        .await
    }
    .await;
    match result {
        Ok(retired) => BackupSlotOutcome {
            result: Ok(()),
            retired,
            used_initial: false,
        },
        Err(error) => BackupSlotOutcome {
            result: Err(error),
            retired: false,
            used_initial: false,
        },
    }
}

pub trait BackupJobDispatcher: Send + Sync + 'static {
    fn enqueue(self: Arc<Self>, job_id: Uuid);
}

impl<S> BackupJobDispatcher for PostgresBackupWorker<S>
where
    S: StorageObjectStore,
{
    fn enqueue(self: Arc<Self>, job_id: Uuid) {
        tokio::spawn(async move {
            let _ = self.run_job(job_id).await;
        });
    }
}

impl<S> BackupJobDispatcher for PostgresBackupWorkerPool<S>
where
    S: StorageObjectStore,
{
    fn enqueue(self: Arc<Self>, _job_id: Uuid) {
        tokio::spawn(async move {
            let _ = self.refresh_and_kick().await;
        });
    }
}

pub struct PostgresBackupControlHooks<H, W> {
    fallback: Arc<H>,
    worker: Arc<W>,
}

/// Connects the backup object download routes to the native object store.
pub struct BackupObjectControlHooks<H, B, F = B> {
    fallback: H,
    backup_objects: Arc<B>,
    file_objects: Arc<F>,
}

impl<H, S> BackupObjectControlHooks<H, S, S> {
    #[must_use]
    pub fn new(fallback: H, objects: Arc<S>) -> Self {
        Self {
            fallback,
            backup_objects: Arc::clone(&objects),
            file_objects: objects,
        }
    }
}

impl<H, B, F> BackupObjectControlHooks<H, B, F> {
    #[must_use]
    pub const fn with_file_objects(
        fallback: H,
        backup_objects: Arc<B>,
        file_objects: Arc<F>,
    ) -> Self {
        Self {
            fallback,
            backup_objects,
            file_objects,
        }
    }
}

impl<H, B, F> DashboardControlHooks for BackupObjectControlHooks<H, B, F>
where
    H: DashboardControlHooks,
    B: BackupObjectStore,
    F: StorageObjectStore,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::ListBackupFiles { storage_prefix, .. } => {
                let prefix = format!("{}/", storage_prefix.trim_end_matches('/'));
                let mut files = self
                    .backup_objects
                    .list_backup_objects(prefix.clone())
                    .await
                    .map_err(backup_object_error)?
                    .into_iter()
                    .filter_map(|object| {
                        object.key.strip_prefix(&prefix).map(|name| {
                            json!({
                                "name": name,
                                "size": object.size,
                            })
                        })
                    })
                    .collect::<Vec<_>>();
                files.sort_by(|left, right| {
                    let left_name = left["name"].as_str().unwrap_or_default();
                    let right_name = right["name"].as_str().unwrap_or_default();
                    (left_name != "config.json", left_name)
                        .cmp(&(right_name != "config.json", right_name))
                });
                Ok(Value::Array(files))
            }
            DashboardControlHookOperation::PresignBackupFile {
                app_id, object_key, ..
            } => self
                .backup_objects
                .sign_backup_object_for_app(app_id, object_key)
                .await
                .map(Value::String)
                .map_err(backup_object_error),
            DashboardControlHookOperation::StreamBackupStorageFiles {
                app_id,
                object_key: backup_key,
                ..
            } => {
                let Some(compressed) = self
                    .backup_objects
                    .read_backup_object(backup_key, MAX_BACKUP_FILES_INDEX_BYTES)
                    .await
                    .map_err(backup_object_error)?
                else {
                    return Ok(Value::String("{\"done\":true}\n".to_owned()));
                };
                let decoded = decode_bounded_zstd(&compressed, MAX_BACKUP_FILES_INDEX_BYTES)
                    .map_err(backup_object_error)?;
                let mut output = String::new();
                for line in decoded.split(|byte| *byte == b'\n') {
                    if line.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let row: Value = serde_json::from_slice(line).map_err(|error| {
                        backup_object_error(format!("invalid backup file row: {error}"))
                    })?;
                    let Some(entity) = row.get("entity") else {
                        continue;
                    };
                    let Some(location_id) = entity.get("location-id").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    let url = self
                        .file_objects
                        .signed_download_url_for_app(app_id, object_key(app_id, location_id))
                        .await
                        .map_err(backup_object_error)?;
                    let record = json!({
                        "locationId": location_id,
                        "path": entity.get("path").cloned().unwrap_or(Value::Null),
                        "url": url,
                    });
                    output.push_str(&serde_json::to_string(&record).map_err(|error| {
                        backup_object_error(format!("could not encode backup file row: {error}"))
                    })?);
                    output.push('\n');
                }
                output.push_str("{\"done\":true}\n");
                Ok(Value::String(output))
            }
            other => self.fallback.execute(other).await,
        }
    }
}

impl<H, B, F> DashboardBackupStreamer for BackupObjectControlHooks<H, B, F>
where
    H: DashboardControlHooks,
    B: BackupObjectStore,
    F: StorageObjectStore,
{
    fn stream_backup_storage_files(
        &self,
        app_id: Uuid,
        backup_id: Uuid,
        object_key: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<DashboardResponseBody, ApiError>> + Send + '_>>
    {
        Box::pin(async move {
            let _ = backup_id;
            let Some(body) = self
                .backup_objects
                .stream_backup_object(object_key)
                .await
                .map_err(backup_object_error)?
            else {
                return Ok(Body::from("{\"done\":true}\n").into());
            };
            Ok(stream_backup_file_rows(Arc::clone(&self.file_objects), app_id, body).into())
        })
    }
}

#[derive(Debug)]
struct BackupFileRow {
    location_id: String,
    path: Value,
}

fn stream_backup_file_rows<S>(objects: Arc<S>, app_id: Uuid, body: Body) -> Body
where
    S: StorageObjectStore,
{
    let (decoded_sender, mut decoded_receiver) =
        mpsc::channel::<Result<Option<BackupFileRow>, String>>(16);
    tokio::task::spawn_blocking(move || {
        let stream = body
            .into_data_stream()
            .map_err(|error| std::io::Error::other(error.to_string()));
        let reader = StreamReader::new(stream);
        let bridge = SyncIoBridge::new(reader);
        let decoder = match zstd::stream::read::Decoder::new(bridge) {
            Ok(decoder) => decoder,
            Err(error) => {
                let _ = decoded_sender.blocking_send(Err(error.to_string()));
                return;
            }
        };
        let mut reader = std::io::BufReader::new(decoder);
        let mut line = Vec::new();
        loop {
            line.clear();
            match read_bounded_line(&mut reader, &mut line, MAX_BACKUP_FILE_ROW_BYTES) {
                Ok(false) => break,
                Ok(true) => {}
                Err(error) => {
                    let _ = decoded_sender.blocking_send(Err(error));
                    break;
                }
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let row = serde_json::from_slice::<Value>(&line)
                .map_err(|error| format!("invalid backup file row: {error}"))
                .map(|row| backup_file_row(&row));
            if decoded_sender.blocking_send(row).is_err() {
                break;
            }
        }
    });

    let (output_sender, output_receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    tokio::spawn(async move {
        while let Some(row) = decoded_receiver.recv().await {
            let row = match row {
                Ok(Some(row)) => row,
                Ok(None) => continue,
                Err(error) => {
                    let _ = output_sender.send(Err(std::io::Error::other(error))).await;
                    return;
                }
            };
            let url = match objects
                .signed_download_url_for_app(app_id, object_key(app_id, &row.location_id))
                .await
            {
                Ok(url) => url,
                Err(error) => {
                    let _ = output_sender.send(Err(std::io::Error::other(error))).await;
                    return;
                }
            };
            let record = json!({
                "locationId": row.location_id,
                "path": row.path,
                "url": url,
            });
            let mut encoded = match serde_json::to_vec(&record) {
                Ok(encoded) => encoded,
                Err(error) => {
                    let _ = output_sender.send(Err(std::io::Error::other(error))).await;
                    return;
                }
            };
            encoded.push(b'\n');
            if output_sender.send(Ok(encoded.into())).await.is_err() {
                return;
            }
        }
        let _ = output_sender
            .send(Ok(Bytes::from_static(b"{\"done\":true}\n")))
            .await;
    });
    Body::from_stream(futures_util::stream::unfold(
        output_receiver,
        |mut receiver| async move { receiver.recv().await.map(|item| (item, receiver)) },
    ))
}

fn backup_file_row(row: &Value) -> Option<BackupFileRow> {
    let entity = row.get("entity")?;
    let location_id = entity.get("location-id").and_then(Value::as_str)?;
    Some(BackupFileRow {
        location_id: location_id.to_owned(),
        path: entity.get("path").cloned().unwrap_or(Value::Null),
    })
}

fn read_bounded_line(
    reader: &mut impl std::io::BufRead,
    output: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<bool, String> {
    loop {
        let available = reader.fill_buf().map_err(|error| error.to_string())?;
        if available.is_empty() {
            return Ok(!output.is_empty());
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if output.len().saturating_add(consumed) > max_bytes {
            return Err(format!(
                "backup file row exceeds the {max_bytes}-byte limit"
            ));
        }
        let complete = available.get(consumed.saturating_sub(1)) == Some(&b'\n');
        output.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if complete {
            return Ok(true);
        }
    }
}

pub(crate) fn decode_bounded_zstd(input: &[u8], max_bytes: usize) -> Result<Vec<u8>, String> {
    let decoder = zstd::stream::read::Decoder::new(input).map_err(|error| error.to_string())?;
    let mut output = Vec::new();
    std::io::Read::take(decoder, u64::try_from(max_bytes).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut output)
        .map_err(|error| error.to_string())?;
    if output.len() > max_bytes {
        return Err(format!(
            "backup file index exceeds the {max_bytes}-byte decompressed limit"
        ));
    }
    Ok(output)
}

fn backup_object_error(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::internal_error(error)
}

impl<H, W> PostgresBackupControlHooks<H, W> {
    #[must_use]
    pub fn new(fallback: Arc<H>, worker: Arc<W>) -> Self {
        Self { fallback, worker }
    }
}

impl<H, W> DashboardControlHooks for PostgresBackupControlHooks<H, W>
where
    H: DashboardControlHooks,
    W: BackupJobDispatcher,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::EnqueueBackupJob { job_id } => {
                Arc::clone(&self.worker).enqueue(job_id);
                Ok(Value::Null)
            }
            other => self.fallback.execute(other).await,
        }
    }
}

struct BackupJob {
    id: Uuid,
    app_id: Uuid,
    description: Option<String>,
    backup_at: Option<DateTime<Utc>>,
    isn: Option<String>,
    storage_id: Option<Uuid>,
    track_progress: bool,
    expire_objects: bool,
}

struct BackupArtifact {
    storage_prefix: String,
    backup_at: DateTime<Utc>,
    triples: i64,
    uncompressed_size: i64,
    files_size: i64,
    db_size: i64,
    _work: BackupWorkDir,
}

struct BackupObject {
    key: String,
    content_type: &'static str,
    path: PathBuf,
    track_progress: bool,
}

#[derive(Default)]
struct Entity {
    fields: Map<String, Value>,
    created_at: i64,
}

impl Entity {
    fn insert(&mut self, label: String, value: Value, many: bool, created_at: i64) {
        if label == "id" {
            self.created_at = created_at;
        }
        if many {
            self.fields
                .entry(label)
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .expect("many-valued fields are arrays")
                .push(value);
        } else {
            self.fields.insert(label, value);
        }
    }

    fn sort_many_values(&mut self) {
        for value in self.fields.values_mut() {
            if let Some(values) = value.as_array_mut() {
                values.sort_by_key(Value::to_string);
            }
        }
    }
}

struct EntityStream {
    encoder: Encoder<'static, File>,
    path: PathBuf,
    entities: i64,
    uncompressed: i64,
}

impl EntityStream {
    fn create(work: &BackupWorkDir) -> Result<Self, BackupWorkerError> {
        let path = work.path.join(format!("{}.jsonl.zst", Uuid::new_v4()));
        let file = File::create(&path).map_err(io_error)?;
        let encoder = Encoder::new(file, 3).map_err(io_error)?;
        Ok(Self {
            encoder,
            path,
            entities: 0,
            uncompressed: 0,
        })
    }

    fn write(&mut self, entity: &Entity) -> Result<(), BackupWorkerError> {
        let line = serde_json::to_vec(&json!({
            "entity": entity.fields,
            "createdAt": entity.created_at,
        }))?;
        self.encoder.write_all(&line).map_err(io_error)?;
        self.encoder.write_all(b"\n").map_err(io_error)?;
        self.entities = self.entities.saturating_add(1);
        self.uncompressed = self
            .uncompressed
            .saturating_add(i64::try_from(line.len() + 1).unwrap_or(i64::MAX));
        Ok(())
    }

    fn finish(self) -> Result<PathBuf, BackupWorkerError> {
        self.encoder.finish().map_err(io_error)?;
        Ok(self.path)
    }
}

fn flush_entities(
    work: &BackupWorkDir,
    streams: &mut HashMap<String, EntityStream>,
    entities: &mut BTreeMap<String, Entity>,
) -> Result<(), BackupWorkerError> {
    for (etype, mut entity) in std::mem::take(entities) {
        entity.sort_many_values();
        if !streams.contains_key(&etype) {
            streams.insert(etype.clone(), EntityStream::create(work)?);
        }
        streams
            .get_mut(&etype)
            .expect("entity stream was inserted")
            .write(&entity)?;
    }
    Ok(())
}

struct ConfigMetadata {
    title: String,
    schema: Value,
    rules: Value,
    webhooks: Vec<Value>,
    email_templates: Vec<Value>,
    redirect_origins: Vec<Value>,
    files_size: i64,
    db_size: i64,
}

impl ConfigMetadata {
    fn into_config(
        self,
        app_id: Uuid,
        backup_at: DateTime<Utc>,
        description: &str,
        isn: Option<&str>,
        counts: &BTreeMap<String, i64>,
        triple_count: i64,
    ) -> Value {
        json!({
            "counts": counts,
            "tripleCount": triple_count,
            "schema": self.schema,
            "rules": self.rules,
            "webhooks": self.webhooks,
            "emailTemplates": self.email_templates,
            "authorizedRedirectOrigins": self.redirect_origins,
            "title": self.title,
            "appId": app_id,
            "backupAt": backup_at,
            "isn": isn,
            "description": description,
        })
    }
}

#[allow(clippy::too_many_lines)]
async fn load_config_metadata<C>(
    client: &C,
    app_id: Uuid,
) -> Result<ConfigMetadata, BackupWorkerError>
where
    C: tokio_postgres::GenericClient + Sync,
{
    let title = client
        .query_one("SELECT title FROM apps WHERE id = $1", &[&app_id])
        .await?
        .get(0);
    let attr_rows = client
        .query(
            "SELECT id, value_type, cardinality, is_unique, is_indexed,
                    coalesce(is_required, false), etype, label, reverse_etype,
                    reverse_label, checked_data_type::text, on_delete::text,
                    on_delete_reverse::text
             FROM attrs WHERE app_id = $1 AND deletion_marked_at IS NULL ORDER BY id",
            &[&app_id],
        )
        .await?;
    let mut entities = Map::new();
    let mut links = Map::new();
    for row in attr_rows {
        let value_type = row.get::<_, String>(1);
        let etype = row.get::<_, String>(6);
        let label = row.get::<_, String>(7);
        if value_type == "ref" {
            let Some(reverse_etype) = row.get::<_, Option<String>>(8) else {
                continue;
            };
            let Some(reverse_label) = row.get::<_, Option<String>>(9) else {
                continue;
            };
            links.insert(
                row.get::<_, Uuid>(0).to_string(),
                json!({
                    "forward": {
                        "on": etype,
                        "label": label,
                        "has": row.get::<_, String>(2),
                        "required": row.get::<_, bool>(5),
                        "onDelete": row.get::<_, Option<String>>(11),
                    },
                    "reverse": {
                        "on": reverse_etype,
                        "label": reverse_label,
                        "has": if row.get::<_, bool>(3) { "one" } else { "many" },
                        "onDelete": row.get::<_, Option<String>>(12),
                    }
                }),
            );
        } else if label != "id" {
            let attrs = entities
                .entry(etype)
                .or_insert_with(|| json!({"attrs": {}}))["attrs"]
                .as_object_mut()
                .expect("entity attrs are objects");
            attrs.insert(
                label,
                json!({
                    "valueType": row.get::<_, Option<String>>(10).unwrap_or_else(|| "any".to_owned()),
                    "required": row.get::<_, bool>(5),
                    "config": {
                        "unique": row.get::<_, bool>(3),
                        "indexed": row.get::<_, bool>(4),
                    }
                }),
            );
        }
    }
    let rules = client
        .query_opt("SELECT code FROM rules WHERE app_id = $1", &[&app_id])
        .await?
        .map_or_else(|| json!({}), |row| row.get::<_, Json<Value>>(0).0);
    let webhooks = client
        .query(
            "SELECT jsonb_build_object(
                       'namespaces', ARRAY(
                         SELECT a.etype FROM attrs a
                         WHERE (a.app_id = $1 OR a.app_id = $2)
                           AND a.id = ANY(w.id_attr_ids)
                       ),
                       'sink', w.sink, 'status', w.status, 'actions', w.actions)
             FROM webhooks w WHERE w.app_id = $1 ORDER BY w.created_at DESC",
            &[&app_id, &instant_instaql::SYSTEM_CATALOG_APP_ID],
        )
        .await?
        .into_iter()
        .map(|row| row.get::<_, Json<Value>>(0).0)
        .collect();
    let email_templates = json_rows(
        client,
        "SELECT jsonb_build_object('type', t.email_type, 'body', t.body, 'name', t.name,
                                   'subject', t.subject, 'email', s.email)
         FROM app_email_templates t JOIN app_email_senders s ON s.id = t.sender_id
         WHERE t.app_id = $1",
        app_id,
    )
    .await?;
    let redirect_origins = json_rows(
        client,
        "SELECT jsonb_build_object('service', service, 'params', params)
         FROM app_authorized_redirect_origins WHERE app_id = $1",
        app_id,
    )
    .await?;
    let (db_size, files_size) = client
        .query_one(
            "SELECT coalesce(sum(pg_size), 0)::bigint, coalesce(sum(files_size), 0)::bigint
             FROM triples_size_aggregate WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .map(|row| (row.get(0), row.get(1)))?;
    Ok(ConfigMetadata {
        title,
        schema: json!({"entities": entities, "links": links}),
        rules,
        webhooks,
        email_templates,
        redirect_origins,
        files_size,
        db_size,
    })
}

async fn json_rows<C>(
    client: &C,
    query: &str,
    app_id: Uuid,
) -> Result<Vec<Value>, tokio_postgres::Error>
where
    C: tokio_postgres::GenericClient + Sync,
{
    Ok(client
        .query(query, &[&app_id])
        .await?
        .into_iter()
        .map(|row| row.get::<_, Json<Value>>(0).0)
        .collect())
}

async fn dynamic_backup_expiry_days<C>(client: &C) -> Result<i64, tokio_postgres::Error>
where
    C: tokio_postgres::GenericClient + Sync,
{
    let value = client
        .query_opt(
            "SELECT flag_value.value
             FROM attrs setting_attr
             JOIN triples flag_setting
               ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
             JOIN attrs value_attr
               ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
              AND value_attr.label = 'value'
             JOIN triples flag_value
               ON flag_value.app_id = $1
              AND flag_value.entity_id = flag_setting.entity_id
              AND flag_value.attr_id = value_attr.id
             WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
               AND setting_attr.label = 'setting'
               AND flag_setting.value = to_jsonb('on-demand-backup-expiry-days'::text)
             LIMIT 1",
            &[&crate::INSTANT_CONFIG_APP_ID],
        )
        .await?
        .map(|row| row.get::<_, Value>(0));
    Ok(backup_expiry_days(value.as_ref()))
}

fn backup_expiry_days(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_i64)
        .filter(|days| *days > 0)
        .unwrap_or(DEFAULT_EXPIRY_DAYS)
        .min(30)
}

fn backup_worker_count(value: Option<&Value>) -> usize {
    usize::try_from(
        value
            .and_then(Value::as_i64)
            .unwrap_or(i64::try_from(DEFAULT_WORKER_COUNT).unwrap_or(3))
            .clamp(1, i64::try_from(MAX_WORKER_COUNT).unwrap_or(32)),
    )
    .unwrap_or(DEFAULT_WORKER_COUNT)
}

struct BackupWorkDir {
    path: PathBuf,
}

impl BackupWorkDir {
    fn create() -> Result<Self, BackupWorkerError> {
        let path = std::env::temp_dir().join(format!("instant-backup-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).map_err(io_error)?;
        Ok(Self { path })
    }
}

impl Drop for BackupWorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_zstd(path: &Path, bytes: &[u8]) -> Result<(), BackupWorkerError> {
    let file = File::create(path).map_err(io_error)?;
    let mut encoder = Encoder::new(file, 3).map_err(io_error)?;
    encoder.write_all(bytes).map_err(io_error)?;
    encoder.finish().map_err(io_error)?;
    Ok(())
}

fn io_error(error: impl std::fmt::Display) -> BackupWorkerError {
    BackupWorkerError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_object_errors_hide_the_storage_cause() {
        let error = backup_object_error("S3 access denied for secret-bucket");

        assert_eq!(error.body().kind, instant_http::ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Internal error");
        assert!(!error.body().message.contains("secret-bucket"));
    }

    #[test]
    fn exported_snapshot_ids_are_restricted_before_sql_interpolation() {
        assert_eq!(
            postgres_snapshot_import_sql("00000003-0000001B-1").unwrap(),
            "SET TRANSACTION SNAPSHOT '00000003-0000001B-1'"
        );
        assert!(postgres_snapshot_import_sql("").is_err());
        assert!(postgres_snapshot_import_sql("snapshot'id").is_err());
        assert!(postgres_snapshot_import_sql("snapshot id").is_err());
    }

    struct FakeBackupObjects {
        entries: Vec<BackupObjectEntry>,
        file_index: Option<Vec<u8>>,
    }

    impl BackupObjectStore for FakeBackupObjects {
        fn list_backup_objects(
            &self,
            _prefix: String,
        ) -> impl Future<Output = Result<Vec<BackupObjectEntry>, String>> + Send {
            std::future::ready(Ok(self.entries.clone()))
        }

        fn sign_backup_object(
            &self,
            key: String,
        ) -> impl Future<Output = Result<String, String>> + Send {
            std::future::ready(Ok(format!("https://objects.test/{key}")))
        }

        fn read_backup_object(
            &self,
            _key: String,
            _max_bytes: usize,
        ) -> impl Future<Output = Result<Option<Vec<u8>>, String>> + Send {
            std::future::ready(Ok(self.file_index.clone()))
        }
    }

    struct FakeFileObjects;

    impl StorageObjectStore for FakeFileObjects {
        fn put(
            &self,
            _request: StoragePutRequest,
            _body: Body,
        ) -> impl Future<Output = Result<crate::StorageObjectMetadata, String>> + Send {
            std::future::ready(Err("uploads are not used by this test".to_owned()))
        }

        fn delete(&self, _key: String) -> impl Future<Output = Result<(), String>> + Send {
            std::future::ready(Err("deletes are not used by this test".to_owned()))
        }

        fn signed_download_url(
            &self,
            key: String,
        ) -> impl Future<Output = Result<String, String>> + Send {
            std::future::ready(Ok(format!("https://files.test/{key}")))
        }
    }

    #[test]
    fn entity_accumulator_preserves_many_values_and_id_timestamp() {
        let mut entity = Entity::default();
        entity.insert("id".to_owned(), json!(Uuid::nil()), false, 42);
        entity.insert("tags".to_owned(), json!("b"), true, 1);
        entity.insert("tags".to_owned(), json!("a"), true, 2);
        entity.sort_many_values();
        assert_eq!(entity.created_at, 42);
        assert_eq!(entity.fields["tags"], json!(["a", "b"]));
    }

    #[tokio::test]
    async fn backup_object_hooks_list_sign_and_project_file_index() {
        let app_id = Uuid::new_v4();
        let prefix = format!("{app_id}/backup");
        let index = format!(
            "{}\n{}\n",
            json!({"entity": {"path": "cover.jpg", "location-id": "location-a"}}),
            json!({"entity": {"path": "ignored"}}),
        );
        let compressed = zstd::stream::encode_all(index.as_bytes(), 3).unwrap();
        let backup_objects = Arc::new(FakeBackupObjects {
            entries: vec![
                BackupObjectEntry {
                    key: format!("{prefix}/entities/books.jsonl"),
                    size: 20,
                },
                BackupObjectEntry {
                    key: format!("{prefix}/config.json"),
                    size: 10,
                },
            ],
            file_index: Some(compressed),
        });
        let hooks = BackupObjectControlHooks::with_file_objects(
            crate::NoopDashboardControlHooks,
            backup_objects,
            Arc::new(FakeFileObjects),
        );

        let files = hooks
            .execute(DashboardControlHookOperation::ListBackupFiles {
                app_id,
                backup_id: Uuid::new_v4(),
                storage_prefix: prefix.clone(),
            })
            .await
            .unwrap();
        assert_eq!(files[0], json!({"name": "config.json", "size": 10}));
        assert_eq!(files[1]["name"], "entities/books.jsonl");

        let signed = hooks
            .execute(DashboardControlHookOperation::PresignBackupFile {
                app_id,
                backup_id: Uuid::new_v4(),
                object_key: format!("{prefix}/config.json"),
            })
            .await
            .unwrap();
        assert_eq!(
            signed,
            json!(format!("https://objects.test/{prefix}/config.json"))
        );

        let stream = hooks
            .execute(DashboardControlHookOperation::StreamBackupStorageFiles {
                app_id,
                backup_id: Uuid::new_v4(),
                object_key: format!("{prefix}/entities/$files.jsonl"),
            })
            .await
            .unwrap();
        let lines = stream.as_str().unwrap().lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        let file: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(file["locationId"], "location-a");
        assert_eq!(file["path"], "cover.jpg");
        assert_eq!(
            file["url"],
            format!("https://files.test/{}", object_key(app_id, "location-a"))
        );
        assert_eq!(
            serde_json::from_str::<Value>(lines[1]).unwrap(),
            json!({"done": true})
        );

        let streamed = hooks
            .stream_backup_storage_files(
                app_id,
                Uuid::new_v4(),
                format!("{prefix}/entities/$files.jsonl"),
            )
            .await
            .unwrap();
        assert!(streamed.buffered().is_none());
        let streamed = axum::body::to_bytes(streamed.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let streamed_lines = streamed.split(|byte| *byte == b'\n').collect::<Vec<_>>();
        assert_eq!(streamed_lines.len(), 3);
        let file: Value = serde_json::from_slice(streamed_lines[0]).unwrap();
        assert_eq!(file["locationId"], "location-a");
        assert_eq!(file["path"], "cover.jpg");
        assert_eq!(
            serde_json::from_slice::<Value>(streamed_lines[1]).unwrap(),
            json!({"done": true})
        );
    }

    #[test]
    fn streaming_decoder_rejects_an_oversized_row_without_buffering_the_export() {
        let oversized = vec![b'a'; MAX_BACKUP_FILE_ROW_BYTES.saturating_add(1)];
        let mut reader = std::io::BufReader::new(oversized.as_slice());
        let mut line = Vec::new();
        let error =
            read_bounded_line(&mut reader, &mut line, MAX_BACKUP_FILE_ROW_BYTES).unwrap_err();
        assert!(error.contains("exceeds"));
        assert!(line.len() <= MAX_BACKUP_FILE_ROW_BYTES);
    }

    #[test]
    fn backup_expiry_flag_matches_the_jvm_default_and_cap() {
        assert_eq!(backup_expiry_days(None), 30);
        assert_eq!(backup_expiry_days(Some(&json!(14))), 14);
        assert_eq!(backup_expiry_days(Some(&json!(90))), 30);
        assert_eq!(backup_expiry_days(Some(&json!(0))), 30);
        assert_eq!(backup_expiry_days(Some(&json!("invalid"))), 30);
    }

    #[test]
    fn backup_worker_count_flag_matches_the_jvm_bounds() {
        assert_eq!(backup_worker_count(None), 3);
        assert_eq!(backup_worker_count(Some(&json!(8))), 8);
        assert_eq!(backup_worker_count(Some(&json!(0))), 1);
        assert_eq!(backup_worker_count(Some(&json!(-10))), 1);
        assert_eq!(backup_worker_count(Some(&json!(100))), 32);
        assert_eq!(backup_worker_count(Some(&json!("invalid"))), 3);
    }
}
