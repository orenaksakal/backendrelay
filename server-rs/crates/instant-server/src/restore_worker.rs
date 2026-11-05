use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::io::{BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use instant_http::{AdminAttribute, ApiError, ApiErrorType};
use instant_permissions::RuleSet;
use instant_transact::AppStatusWriteCheck;
use serde::Deserialize;
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Interval, MissedTickBehavior};
use tokio_postgres::binary_copy::BinaryCopyInWriter;
use tokio_postgres::types::{Json, ToSql, Type};
use tokio_postgres::{Client, Row};
use tokio_util::io::ReaderStream;
use uuid::Uuid;
use zip::write::{SimpleFileOptions, ZipWriter};
use zip::{CompressionMethod, ZipArchive};

use crate::admin_storage::{StorageObjectStore, StoragePutRequest, object_key};
use crate::backup_worker::{BackupObjectStore, decode_bounded_zstd};
use crate::dashboard_control::{
    DashboardControlHookOperation, DashboardControlHooks,
    apply_schema_plan_with_app_status_write_check, load_dashboard_attributes, schema_push_plan,
};
use crate::postgres_pool::PostgresClientSource;
use crate::{PostgresPool, PostgresPoolError, is_reserved_app_id};

const TRIPLE_BATCH_SIZE: usize = 1_000;
const TRIPLE_BATCH_SIZE_I64: i64 = 1_000;
const EVENT_BUFFER_SIZE: usize = 16;
const MAX_CONFIG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STAGED_BACKUP_OBJECT_BYTES: usize = 512 * 1024 * 1024;

/// Result of attempting to run one durable restore job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreJobOutcome {
    NotClaimed,
    Completed {
        app_id: Uuid,
        triples: u64,
        files: u64,
    },
    Cancelled,
}

/// Failure returned to an in-process restore coordinator. The worker also
/// persists the root error on the restore job before returning it.
#[derive(Debug, Error)]
pub enum RestoreWorkerError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
    #[error("restore archive error: {0}")]
    Archive(String),
    #[error("invalid restore backup: {0}")]
    InvalidBackup(String),
    #[error("restore object-store error: {0}")]
    ObjectStore(String),
    #[error("restore I/O error: {0}")]
    Io(String),
    #[error("restore task failed: {0}")]
    Join(String),
    #[error("restore was cancelled")]
    Cancelled,
}

/// PostgreSQL/object-store implementation of `InstantDB`'s restore data plane.
/// A process runs at most one restore at a time, matching the self-hosted JVM
/// coordinator and preventing two jobs from sharing a connection-local COPY
/// staging table.
pub struct PostgresRestoreWorker<S> {
    client: PostgresClientSource,
    objects: Arc<S>,
    staging_creator_email: String,
    run_lock: Mutex<()>,
}

impl<S> PostgresRestoreWorker<S>
where
    S: StorageObjectStore,
{
    #[must_use]
    pub fn new(client: Client, objects: Arc<S>, staging_creator_email: impl Into<String>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            objects,
            staging_creator_email: staging_creator_email.into(),
            run_lock: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn with_shared_client(
        client: Arc<Mutex<Client>>,
        objects: Arc<S>,
        staging_creator_email: impl Into<String>,
    ) -> Self {
        Self {
            client: PostgresClientSource::fixed_shared(client),
            objects,
            staging_creator_email: staging_creator_email.into(),
            run_lock: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn from_pool(
        client: PostgresPool,
        objects: Arc<S>,
        staging_creator_email: impl Into<String>,
    ) -> Self {
        Self {
            client: PostgresClientSource::pooled(client),
            objects,
            staging_creator_email: staging_creator_email.into(),
            run_lock: Mutex::new(()),
        }
    }

    /// Atomically claims and executes a waiting restore job.
    ///
    /// # Errors
    ///
    /// Returns the underlying archive, database, filesystem, or object-store
    /// failure after recording it on the job and cleaning partial state.
    #[tracing::instrument(name = "app-restore-jobs/run-restore", skip_all, fields(job.id = %job_id))]
    pub async fn run_job(&self, job_id: Uuid) -> Result<RestoreJobOutcome, RestoreWorkerError> {
        let _run = self.run_lock.lock().await;
        let Some(job) = self.claim_job(job_id).await? else {
            self.remove_cancelled_staged_archive(job_id).await;
            return Ok(RestoreJobOutcome::NotClaimed);
        };

        let mut uploaded_keys = Vec::new();
        let mut restore_token = None;
        let execution = self
            .execute_restore(&job, &mut uploaded_keys, &mut restore_token)
            .await;
        let _ = tokio::fs::remove_file(&job.zip_path).await;

        match execution {
            Ok(stats) => {
                let Some(marker) = restore_token else {
                    let error = RestoreWorkerError::InvalidBackup(
                        "restore completed without an app marker".to_owned(),
                    );
                    self.mark_errored(job.id, &error).await;
                    return Err(error);
                };
                match self.finalize_success(&job, marker).await {
                    Ok(true) => Ok(RestoreJobOutcome::Completed {
                        app_id: job.app_id,
                        triples: stats.triples,
                        files: stats.files,
                    }),
                    Ok(false) => {
                        self.cleanup_partial(job.app_id, Some(marker), &uploaded_keys)
                            .await;
                        Ok(RestoreJobOutcome::Cancelled)
                    }
                    Err(error) => {
                        self.cleanup_partial(job.app_id, Some(marker), &uploaded_keys)
                            .await;
                        self.mark_errored(job.id, &error).await;
                        Err(error)
                    }
                }
            }
            Err(RestoreWorkerError::Cancelled) => {
                self.cleanup_partial(job.app_id, restore_token, &uploaded_keys)
                    .await;
                Ok(RestoreJobOutcome::Cancelled)
            }
            Err(error) => {
                self.cleanup_partial(job.app_id, restore_token, &uploaded_keys)
                    .await;
                self.mark_errored(job.id, &error).await;
                Err(error)
            }
        }
    }

    /// Finds and runs waiting restore jobs whose original local enqueue was
    /// lost during process shutdown or failover. `run_job` performs the actual
    /// atomic claim, so concurrent recovery scans remain safe.
    ///
    /// # Errors
    ///
    /// Returns a database, archive, filesystem, or object-store failure from
    /// the availability scan or a claimed restore.
    pub async fn run_available_jobs(
        &self,
        limit: i64,
    ) -> Result<Vec<(Uuid, RestoreJobOutcome)>, RestoreWorkerError> {
        let ids = {
            let client = self.client.acquire().await?;
            client
                .query(
                    "SELECT id FROM app_restore_jobs
                     WHERE job_status = 'waiting'
                     ORDER BY created_at, id LIMIT $1",
                    &[&limit.clamp(1, 1_000)],
                )
                .await?
                .into_iter()
                .map(|row| row.get(0))
                .collect::<Vec<Uuid>>()
        };
        let mut outcomes = Vec::with_capacity(ids.len());
        for id in ids {
            outcomes.push((id, self.run_job(id).await?));
        }
        Ok(outcomes)
    }

    async fn claim_job(&self, job_id: Uuid) -> Result<Option<RestoreJob>, RestoreWorkerError> {
        let client = self.client.acquire().await?;
        let row = client
            .query_opt(
                "UPDATE app_restore_jobs
                 SET job_status = 'processing'
                 WHERE id = $1 AND job_status = 'waiting'
                 RETURNING id, app_id, creator_id, org_id, title, zip_path",
                &[&job_id],
            )
            .await?;
        Ok(row.as_ref().map(RestoreJob::from_row))
    }

    async fn remove_cancelled_staged_archive(&self, job_id: Uuid) {
        let path = {
            let Ok(client) = self.client.acquire().await else {
                return;
            };
            client
                .query_opt(
                    "SELECT zip_path FROM app_restore_jobs
                     WHERE id = $1 AND job_status = 'cancelled'",
                    &[&job_id],
                )
                .await
                .ok()
                .flatten()
                .map(|row| row.get::<_, String>(0))
        };
        if let Some(path) = path {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    #[allow(clippy::too_many_lines)] // Keeps producer, consumers, and cancellation joined safely.
    async fn execute_restore(
        &self,
        job: &RestoreJob,
        uploaded_keys: &mut Vec<String>,
        restore_token: &mut Option<Uuid>,
    ) -> Result<RestoreStats, RestoreWorkerError> {
        if !self
            .report_progress(job.id, "Preparing restore…".to_owned())
            .await?
        {
            return Err(RestoreWorkerError::Cancelled);
        }

        let archive_path = job.zip_path.clone();
        let config = tokio::task::spawn_blocking(move || inspect_archive(&archive_path))
            .await
            .map_err(|error| RestoreWorkerError::Join(error.to_string()))??;
        self.initialize_app(job, &config, restore_token).await?;

        let attributes = {
            let client = self.client.acquire().await?;
            let client_ref: &Client = &client;
            load_dashboard_attributes(client_ref, job.app_id)
                .await
                .map_err(|error| api_error(&error))?
        };
        let attr_map = Arc::new(attribute_map(&attributes));
        let work_dir = RestoreWorkDir::create()?;
        let (sender, mut receiver) = mpsc::channel(EVENT_BUFFER_SIZE);
        let producer_archive = job.zip_path.clone();
        let producer_dir = work_dir.path.clone();
        let producer_app_id = job.app_id;
        let cancelled = Arc::new(AtomicBool::new(false));
        let producer_cancelled = Arc::clone(&cancelled);
        let producer = tokio::task::spawn_blocking(move || {
            produce_archive_events(
                &producer_archive,
                &producer_dir,
                producer_app_id,
                &attr_map,
                &sender,
                &producer_cancelled,
            )
        });

        let totals = config.totals();
        let mut stats = RestoreStats::default();
        let mut processing_error = None;
        let mut progress_interval = tokio::time::interval(std::time::Duration::from_secs(1));
        progress_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        progress_interval.tick().await;
        loop {
            let event = tokio::select! {
                event = receiver.recv() => event,
                _ = progress_interval.tick() => {
                    match self.report_progress(job.id, format_progress(stats, totals)).await {
                        Ok(true) => continue,
                        Ok(false) => {
                            processing_error = Some(RestoreWorkerError::Cancelled);
                            cancelled.store(true, Ordering::Release);
                            break;
                        }
                        Err(error) => {
                            processing_error = Some(error);
                            cancelled.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
            };
            let Some(event) = event else {
                break;
            };
            let processed = match event {
                RestoreEvent::Triples(triples) => self.copy_triples(&triples).await.map(|()| {
                    stats.triples += u64::try_from(triples.len()).unwrap_or(u64::MAX);
                }),
                RestoreEvent::File(file) => {
                    let key = object_key(job.app_id, &file.location_id);
                    match tokio::fs::File::open(&file.path).await.map_err(io_error) {
                        Ok(input) => {
                            uploaded_keys.push(key.clone());
                            self.with_cancellation(
                                job.id,
                                stats,
                                totals,
                                &mut progress_interval,
                                &cancelled,
                                async {
                                    self.objects
                                        .put(
                                            StoragePutRequest {
                                                key,
                                                content_type: file.content_type,
                                                content_disposition: file.content_disposition,
                                                content_encoding: None,
                                                content_length: file.content_length,
                                                tags: std::collections::BTreeMap::new(),
                                            },
                                            Body::from_stream(ReaderStream::new(input)),
                                        )
                                        .await
                                        .map_err(RestoreWorkerError::ObjectStore)
                                },
                            )
                            .await
                            .map(|_| {
                                stats.files += 1;
                            })
                        }
                        Err(error) => Err(error),
                    }
                }
            };
            if let Err(error) = processed {
                processing_error = Some(error);
                cancelled.store(true, Ordering::Release);
                break;
            }
        }
        drop(receiver);
        let producer_result = producer
            .await
            .map_err(|error| RestoreWorkerError::Join(error.to_string()))?;
        if let Some(error) = processing_error {
            return Err(error);
        }
        producer_result?;
        if !self
            .report_progress(job.id, format_progress(stats, totals))
            .await?
        {
            return Err(RestoreWorkerError::Cancelled);
        }
        Ok(stats)
    }

    async fn with_cancellation<T, F>(
        &self,
        job_id: Uuid,
        stats: RestoreStats,
        totals: RestoreTotals,
        interval: &mut Interval,
        cancelled: &AtomicBool,
        future: F,
    ) -> Result<T, RestoreWorkerError>
    where
        F: Future<Output = Result<T, RestoreWorkerError>>,
    {
        tokio::pin!(future);
        loop {
            tokio::select! {
                result = &mut future => return result,
                _ = interval.tick() => {
                    match self.report_progress(job_id, format_progress(stats, totals)).await {
                        Ok(true) => {}
                        Ok(false) => {
                            cancelled.store(true, Ordering::Release);
                            return Err(RestoreWorkerError::Cancelled);
                        }
                        Err(error) => {
                            cancelled.store(true, Ordering::Release);
                            return Err(error);
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)] // Keeps app initialization and its cleanup marker ordered.
    async fn initialize_app(
        &self,
        job: &RestoreJob,
        config: &RestoreConfig,
        restore_token: &mut Option<Uuid>,
    ) -> Result<(), RestoreWorkerError> {
        if let Some(rules) = &config.rules {
            RuleSet::try_new(rules.clone()).map_err(|issues| {
                RestoreWorkerError::InvalidBackup(format!("rules are invalid: {issues:?}"))
            })?;
        }
        let mut client = self.client.acquire().await?;
        let staging_creator_id = client
            .query_opt(
                "SELECT id FROM instant_users WHERE email = $1",
                &[&self.staging_creator_email],
            )
            .await?
            .map(|row| row.get::<_, Uuid>(0))
            .ok_or_else(|| {
                RestoreWorkerError::InvalidBackup(format!(
                    "staging creator {} does not exist",
                    self.staging_creator_email
                ))
            })?;
        validate_job_owner(&client, job).await?;
        if is_reserved_app_id(job.app_id) {
            return Err(RestoreWorkerError::InvalidBackup(format!(
                "app {} is reserved for deployment configuration",
                job.app_id
            )));
        }
        if client
            .query_opt("SELECT 1 FROM apps WHERE id = $1", &[&job.app_id])
            .await?
            .is_some()
        {
            return Err(RestoreWorkerError::InvalidBackup(format!(
                "app {} already exists",
                job.app_id
            )));
        }

        let client_ref: &Client = &client;
        let current_attributes = load_dashboard_attributes(client_ref, job.app_id)
            .await
            .map_err(|error| api_error(&error))?;
        let plan = schema_push_plan(&current_attributes, &config.schema, true, false)
            .map_err(|error| api_error(&error))?;
        let steps = plan
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                RestoreWorkerError::InvalidBackup("schema plan has no steps".to_owned())
            })?
            .clone();
        let title = job.title.clone().unwrap_or_else(|| {
            format!("{} (Restored)", config.title.as_deref().unwrap_or_default())
        });
        let marker = Uuid::new_v4();
        let transaction = client.transaction().await?;
        transaction
            .execute(
                "INSERT INTO apps (id, creator_id, title, status)
                 VALUES ($1, $2, $3, 'disabled')",
                &[&job.app_id, &staging_creator_id, &title],
            )
            .await?;
        transaction
            .execute(
                "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
                &[&marker, &job.app_id],
            )
            .await?;
        if let Some(rules) = &config.rules {
            transaction
                .execute(
                    "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
                    &[&job.app_id, &Json(rules)],
                )
                .await?;
        }
        transaction.commit().await?;
        *restore_token = Some(marker);
        apply_schema_plan_with_app_status_write_check(
            &mut client,
            job.app_id,
            &current_attributes,
            &steps,
            false,
            AppStatusWriteCheck::Skip,
        )
        .await
        .map_err(|error| api_error(&error))?;
        for template in &config.email_templates {
            client
                .execute(
                    "INSERT INTO app_email_templates
                       (id, app_id, email_type, sender_id, body, subject, name)
                     VALUES ($1, $2, $3, NULL, $4, $5, $6)",
                    &[
                        &Uuid::new_v4(),
                        &job.app_id,
                        &template.kind,
                        &template.body,
                        &template.subject,
                        &template.name,
                    ],
                )
                .await?;
        }
        for origin in &config.authorized_redirect_origins {
            client
                .execute(
                    "INSERT INTO app_authorized_redirect_origins
                       (id, app_id, service, params)
                     VALUES ($1, $2, $3, $4)",
                    &[
                        &Uuid::new_v4(),
                        &job.app_id,
                        &origin.service,
                        &origin.params,
                    ],
                )
                .await?;
        }
        Ok(())
    }

    async fn copy_triples(&self, triples: &[RestoreTriple]) -> Result<(), RestoreWorkerError> {
        if triples.is_empty() {
            return Ok(());
        }
        let client = self.client.acquire().await?;
        client
            .batch_execute(
                "CREATE TEMP TABLE IF NOT EXISTS instant_restore_triples_stage (
                   app_id uuid NOT NULL,
                   entity_id uuid NOT NULL,
                   attr_id uuid NOT NULL,
                   value jsonb NOT NULL,
                   ea boolean NOT NULL,
                   eav boolean NOT NULL,
                   av boolean NOT NULL,
                   ave boolean NOT NULL,
                   vae boolean NOT NULL,
                   created_at bigint NOT NULL,
                   checked_data_type text
                 ) ON COMMIT PRESERVE ROWS;
                 TRUNCATE instant_restore_triples_stage;",
            )
            .await?;
        let sink = client
            .copy_in(
                "COPY instant_restore_triples_stage
                   (app_id, entity_id, attr_id, value, ea, eav, av, ave, vae,
                    created_at, checked_data_type)
                 FROM STDIN BINARY",
            )
            .await?;
        let writer = BinaryCopyInWriter::new(
            sink,
            &[
                Type::UUID,
                Type::UUID,
                Type::UUID,
                Type::JSONB,
                Type::BOOL,
                Type::BOOL,
                Type::BOOL,
                Type::BOOL,
                Type::BOOL,
                Type::INT8,
                Type::TEXT,
            ],
        );
        tokio::pin!(writer);
        for triple in triples {
            let value = Json(&triple.value);
            let values: [&(dyn ToSql + Sync); 11] = [
                &triple.app_id,
                &triple.entity_id,
                &triple.attr_id,
                &value,
                &triple.ea,
                &triple.eav,
                &triple.av,
                &triple.ave,
                &triple.vae,
                &triple.created_at,
                &triple.checked_data_type,
            ];
            writer.as_mut().write(&values).await?;
        }
        writer.as_mut().finish().await?;
        client
            .execute(
                "INSERT INTO triples
                   (app_id, entity_id, attr_id, value, value_md5, ea, eav, av,
                    ave, vae, created_at, checked_data_type)
                 SELECT app_id, entity_id, attr_id, value, md5(value::text), ea,
                        eav, av, ave, vae, created_at,
                        checked_data_type::checked_data_type
                 FROM instant_restore_triples_stage",
                &[],
            )
            .await?;
        Ok(())
    }

    async fn report_progress(
        &self,
        job_id: Uuid,
        progress: String,
    ) -> Result<bool, RestoreWorkerError> {
        let client = self.client.acquire().await?;
        Ok(client
            .execute(
                "UPDATE app_restore_jobs SET progress = $2
                 WHERE id = $1 AND job_status = 'processing'",
                &[&job_id, &progress],
            )
            .await?
            > 0)
    }

    async fn finalize_success(
        &self,
        job: &RestoreJob,
        restore_token: Uuid,
    ) -> Result<bool, RestoreWorkerError> {
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        let processing = transaction
            .query_opt(
                "SELECT 1 FROM app_restore_jobs j
                 WHERE j.id = $1 AND j.job_status = 'processing'
                   AND EXISTS (
                     SELECT 1 FROM app_admin_tokens t
                     WHERE t.app_id = $2 AND t.token = $3
                   )
                 FOR UPDATE",
                &[&job.id, &job.app_id, &restore_token],
            )
            .await?
            .is_some();
        if !processing {
            transaction.rollback().await?;
            return Ok(false);
        }
        transaction
            .execute(
                "UPDATE apps
                 SET creator_id = $2, org_id = $3, status = 'active'
                 WHERE id = $1",
                &[&job.app_id, &job.creator_id, &job.org_id],
            )
            .await?;
        transaction
            .execute(
                "UPDATE app_restore_jobs
                 SET job_status = 'completed', done_at = now()
                 WHERE id = $1",
                &[&job.id],
            )
            .await?;
        transaction.commit().await?;
        Ok(true)
    }

    async fn cleanup_partial(
        &self,
        app_id: Uuid,
        restore_token: Option<Uuid>,
        uploaded_keys: &[String],
    ) {
        let Some(restore_token) = restore_token else {
            return;
        };
        let Ok(client) = self.client.acquire().await else {
            return;
        };
        let owns_app = client
            .query_opt(
                "SELECT 1 FROM app_admin_tokens WHERE app_id = $1 AND token = $2",
                &[&app_id, &restore_token],
            )
            .await
            .ok()
            .flatten()
            .is_some();
        if !owns_app {
            return;
        }
        drop(client);
        for key in uploaded_keys.iter().rev() {
            let _ = self.objects.delete(key.clone()).await;
        }
        let Ok(client) = self.client.acquire().await else {
            return;
        };
        if client
            .query_opt(
                "SELECT 1 FROM app_admin_tokens WHERE app_id = $1 AND token = $2",
                &[&app_id, &restore_token],
            )
            .await
            .ok()
            .flatten()
            .is_none()
        {
            return;
        }
        loop {
            let deleted = client
                .execute(
                    "WITH to_delete AS (
                       SELECT ctid FROM triples WHERE app_id = $1 LIMIT $2 FOR UPDATE
                     )
                     DELETE FROM triples WHERE ctid IN (SELECT ctid FROM to_delete)",
                    &[&app_id, &TRIPLE_BATCH_SIZE_I64],
                )
                .await
                .unwrap_or(0);
            if deleted == 0 {
                break;
            }
        }
        let _ = client
            .execute("DELETE FROM apps WHERE id = $1", &[&app_id])
            .await;
    }

    async fn mark_errored(&self, job_id: Uuid, error: &RestoreWorkerError) {
        let Ok(client) = self.client.acquire().await else {
            return;
        };
        let _ = client
            .execute(
                "UPDATE app_restore_jobs
                 SET job_status = 'errored', error = $2, done_at = now()
                 WHERE id = $1 AND job_status IN ('waiting', 'processing')",
                &[&job_id, &error.to_string()],
            )
            .await;
    }
}

/// Stages a stored app backup as a restore ZIP. Backups keep their objects
/// individually (config plus per-entity JSONL streams, all zstd-compressed),
/// while `PostgresRestoreWorker` consumes one archive, so this adapter reads
/// the backup prefix and writes the equivalent ZIP before enqueueing the job.
pub struct PostgresBackupRestoreControlHooks<H, B> {
    fallback: Arc<H>,
    backup_objects: Arc<B>,
    upload_dir: PathBuf,
}

impl<H, B> PostgresBackupRestoreControlHooks<H, B> {
    #[must_use]
    pub fn new(fallback: Arc<H>, backup_objects: Arc<B>) -> Self {
        Self {
            fallback,
            backup_objects,
            upload_dir: std::env::temp_dir().join("instant-restore-uploads"),
        }
    }

    #[must_use]
    pub fn with_upload_dir(mut self, upload_dir: impl Into<PathBuf>) -> Self {
        self.upload_dir = upload_dir.into();
        self
    }
}

impl<H, B> PostgresBackupRestoreControlHooks<H, B>
where
    B: BackupObjectStore,
{
    async fn stage_backup_zip(
        &self,
        job_id: Uuid,
        storage_prefix: &str,
    ) -> Result<Value, ApiError> {
        if storage_prefix.trim().is_empty() {
            return Err(stage_backup_error(
                "backup has no storage prefix".to_owned(),
            ));
        }
        let prefix = format!("{}/", storage_prefix.trim_end_matches('/'));
        let objects = self
            .backup_objects
            .list_backup_objects(prefix.clone())
            .await
            .map_err(stage_backup_error)?;
        if objects.is_empty() {
            return Err(stage_backup_error(format!(
                "backup prefix {storage_prefix} has no stored objects"
            )));
        }
        let mut entries = Vec::with_capacity(objects.len());
        for object in objects {
            let name = object
                .key
                .strip_prefix(&prefix)
                .filter(|name| safe_archive_name(name))
                .ok_or_else(|| {
                    stage_backup_error(format!(
                        "backup object {} is outside the restore namespace",
                        object.key
                    ))
                })?
                .to_owned();
            let compressed = self
                .backup_objects
                .read_backup_object(object.key.clone(), MAX_STAGED_BACKUP_OBJECT_BYTES)
                .await
                .map_err(stage_backup_error)?
                .ok_or_else(|| {
                    stage_backup_error(format!("backup object {} is missing", object.key))
                })?;
            entries.push((name, compressed));
        }
        if !entries.iter().any(|(name, _)| name == "config.json") {
            return Err(stage_backup_error(
                "backup prefix has no config.json object".to_owned(),
            ));
        }
        entries.sort_by(|left, right| {
            (left.0 != "config.json", &left.0).cmp(&(right.0 != "config.json", &right.0))
        });

        tokio::fs::create_dir_all(&self.upload_dir)
            .await
            .map_err(stage_backup_error)?;
        let destination = self.upload_dir.join(format!("{job_id}.zip"));
        let write_path = destination.clone();
        let written = tokio::task::spawn_blocking(move || write_backup_zip(&write_path, &entries))
            .await
            .map_err(|error| stage_backup_error(error.to_string()))?;
        if let Err(error) = written {
            let _ = tokio::fs::remove_file(&destination).await;
            return Err(stage_backup_error(error));
        }
        Ok(Value::String(destination.to_string_lossy().into_owned()))
    }
}

impl<H, B> DashboardControlHooks for PostgresBackupRestoreControlHooks<H, B>
where
    H: DashboardControlHooks,
    B: BackupObjectStore,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::StageBackupRestoreZip {
                job_id,
                storage_prefix,
            } => self.stage_backup_zip(job_id, &storage_prefix).await,
            other => self.fallback.execute(other).await,
        }
    }
}

fn safe_archive_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn write_backup_zip(path: &Path, entries: &[(String, Vec<u8>)]) -> Result<(), String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, compressed) in entries {
        let decoded = decode_bounded_zstd(compressed, MAX_STAGED_BACKUP_OBJECT_BYTES)?;
        archive
            .start_file(name, options)
            .map_err(|error| error.to_string())?;
        archive
            .write_all(&decoded)
            .map_err(|error| error.to_string())?;
    }
    archive.finish().map_err(|error| error.to_string())?;
    Ok(())
}

fn stage_backup_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        format!("Could not stage backup restore ZIP: {error}"),
    )
}

/// Restore-aware dashboard hook adapter. It stages uploaded bodies to a stable
/// job path, starts the local worker, and delegates every non-restore operation
/// to the wrapped dashboard hook implementation.
pub struct PostgresRestoreControlHooks<H, S> {
    fallback: Arc<H>,
    worker: Arc<PostgresRestoreWorker<S>>,
    upload_dir: PathBuf,
}

impl<H, S> PostgresRestoreControlHooks<H, S> {
    #[must_use]
    pub fn new(fallback: Arc<H>, worker: Arc<PostgresRestoreWorker<S>>) -> Self {
        Self {
            fallback,
            worker,
            upload_dir: std::env::temp_dir().join("instant-restore-uploads"),
        }
    }

    #[must_use]
    pub fn with_upload_dir(mut self, upload_dir: impl Into<PathBuf>) -> Self {
        self.upload_dir = upload_dir.into();
        self
    }

    async fn persist_zip(
        &self,
        job_id: Uuid,
        staged_path: Option<String>,
        body: Vec<u8>,
    ) -> Result<Value, ApiError> {
        tokio::fs::create_dir_all(&self.upload_dir)
            .await
            .map_err(restore_api_io)?;
        let destination = self.upload_dir.join(format!("{job_id}.zip"));
        let mut output = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .await
            .map_err(restore_api_io)?;
        let result = async {
            if let Some(source) = staged_path {
                let mut input = tokio::fs::File::open(source)
                    .await
                    .map_err(restore_api_io)?;
                tokio::io::copy(&mut input, &mut output)
                    .await
                    .map_err(restore_api_io)?;
            } else {
                output.write_all(&body).await.map_err(restore_api_io)?;
            }
            output.flush().await.map_err(restore_api_io)
        }
        .await;
        drop(output);
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&destination).await;
            return Err(error);
        }
        Ok(Value::String(destination.to_string_lossy().into_owned()))
    }

    async fn discard_zip(&self, path: &str) -> Result<Value, ApiError> {
        let path = Path::new(path);
        let valid_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".zip"))
            .and_then(|stem| stem.parse::<Uuid>().ok())
            .is_some();
        if path.parent() != Some(self.upload_dir.as_path()) || !valid_name {
            return Err(ApiError::new(
                ApiErrorType::ParamMalformed,
                "Refusing to discard a restore ZIP outside the upload directory",
            ));
        }
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(Value::Null),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
            Err(error) => Err(restore_api_io(error)),
        }
    }
}

impl<H, S> DashboardControlHooks for PostgresRestoreControlHooks<H, S>
where
    H: DashboardControlHooks,
    S: StorageObjectStore,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::PersistRestoreZip {
                job_id,
                staged_path,
                body,
                ..
            } => self.persist_zip(job_id, staged_path, body).await,
            DashboardControlHookOperation::DiscardRestoreZip { path } => {
                self.discard_zip(&path).await
            }
            DashboardControlHookOperation::EnqueueRestoreJob { job_id } => {
                let worker = Arc::clone(&self.worker);
                tokio::spawn(async move {
                    let _ = worker.run_job(job_id).await;
                });
                Ok(Value::Null)
            }
            other => self.fallback.execute(other).await,
        }
    }
}

#[derive(Debug)]
struct RestoreJob {
    id: Uuid,
    app_id: Uuid,
    creator_id: Option<Uuid>,
    org_id: Option<Uuid>,
    title: Option<String>,
    zip_path: String,
}

impl RestoreJob {
    fn from_row(row: &Row) -> Self {
        Self {
            id: row.get(0),
            app_id: row.get(1),
            creator_id: row.get(2),
            org_id: row.get(3),
            title: row.get(4),
            zip_path: row.get(5),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct RestoreConfig {
    schema: Value,
    #[serde(default)]
    rules: Option<Value>,
    #[serde(default, rename = "emailTemplates")]
    email_templates: Vec<RestoreEmailTemplate>,
    #[serde(default, rename = "authorizedRedirectOrigins")]
    authorized_redirect_origins: Vec<RestoreRedirectOrigin>,
    #[serde(default)]
    counts: BTreeMap<String, u64>,
    #[serde(default, rename = "tripleCount")]
    triple_count: Option<u64>,
    #[serde(default)]
    title: Option<String>,
}

impl RestoreConfig {
    fn totals(&self) -> RestoreTotals {
        RestoreTotals {
            triples: self.triple_count,
            files: self.counts.get("$files").copied(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct RestoreEmailTemplate {
    #[serde(rename = "type")]
    kind: String,
    body: String,
    subject: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct RestoreRedirectOrigin {
    service: String,
    params: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RestoreStats {
    triples: u64,
    files: u64,
}

#[derive(Clone, Copy, Debug)]
struct RestoreTotals {
    triples: Option<u64>,
    files: Option<u64>,
}

#[derive(Debug)]
enum RestoreEvent {
    Triples(Vec<RestoreTriple>),
    File(RestoreFile),
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Exact persisted triple index flags.
struct RestoreTriple {
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    value: Value,
    ea: bool,
    eav: bool,
    av: bool,
    ave: bool,
    vae: bool,
    created_at: i64,
    checked_data_type: Option<String>,
}

#[derive(Debug)]
struct RestoreFile {
    location_id: String,
    path: PathBuf,
    content_type: Option<String>,
    content_disposition: Option<String>,
    content_length: Option<u64>,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_field_names)] // Mirrors object-store metadata terminology.
struct RestoreFileMetadata {
    content_type: Option<String>,
    content_disposition: Option<String>,
    content_length: Option<u64>,
}

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)] // Exact persisted attribute flags.
struct RestoreAttribute {
    id: Uuid,
    one: bool,
    reference: bool,
    unique: bool,
    indexed: bool,
    checked_data_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EntityRow {
    entity: Map<String, Value>,
    #[serde(rename = "createdAt")]
    created_at: i64,
}

fn inspect_archive(path: &str) -> Result<RestoreConfig, RestoreWorkerError> {
    let file = std::fs::File::open(path).map_err(io_error)?;
    let mut archive = ZipArchive::new(file).map_err(archive_error)?;
    if archive.is_empty() {
        return Err(RestoreWorkerError::InvalidBackup(
            "backup ZIP is empty".to_owned(),
        ));
    }
    let mut names = HashSet::new();
    let mut in_files = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(archive_error)?;
        let name = entry.name().map_err(archive_error)?.into_owned();
        if !names.insert(name.clone()) {
            return Err(RestoreWorkerError::InvalidBackup(format!(
                "duplicate ZIP entry {name}"
            )));
        }
        if index == 0 {
            if name != "config.json" {
                return Err(RestoreWorkerError::InvalidBackup(format!(
                    "expected first ZIP entry to be config.json, found {name}"
                )));
            }
            continue;
        }
        if let Some(etype) = entity_entry_type(&name) {
            if in_files {
                return Err(RestoreWorkerError::InvalidBackup(format!(
                    "entity entry {name} appears after file entries"
                )));
            }
            if etype.is_empty() {
                return Err(RestoreWorkerError::InvalidBackup(
                    "entity entry has an empty type".to_owned(),
                ));
            }
        } else if let Some(location_id) = file_entry_location(&name) {
            in_files = true;
            validate_location_id(location_id)?;
        } else {
            return Err(RestoreWorkerError::InvalidBackup(format!(
                "unexpected ZIP entry {name}; expected entities/*.jsonl or files/*"
            )));
        }
    }
    let entry = archive.by_index(0).map_err(archive_error)?;
    let mut bytes = Vec::new();
    entry
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(RestoreWorkerError::InvalidBackup(
            "config.json exceeds 64 MiB".to_owned(),
        ));
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| RestoreWorkerError::InvalidBackup(error.to_string()))?;
    if !value.is_object() || value.get("schema").is_none() {
        return Err(RestoreWorkerError::InvalidBackup(
            "config.json must be an object with a schema key".to_owned(),
        ));
    }
    serde_json::from_value(value)
        .map_err(|error| RestoreWorkerError::InvalidBackup(error.to_string()))
}

#[allow(clippy::too_many_lines)] // The ZIP ordering state machine is intentionally contiguous.
fn produce_archive_events(
    archive_path: &str,
    work_dir: &Path,
    app_id: Uuid,
    attributes: &HashMap<(String, String), RestoreAttribute>,
    sender: &mpsc::Sender<RestoreEvent>,
    cancelled: &AtomicBool,
) -> Result<(), RestoreWorkerError> {
    let file = std::fs::File::open(archive_path).map_err(io_error)?;
    let mut archive = ZipArchive::new(file).map_err(archive_error)?;
    let mut batch = Vec::with_capacity(TRIPLE_BATCH_SIZE);
    let mut files = HashMap::<String, RestoreFileMetadata>::new();
    let mut seen_file_entries = HashSet::new();
    let mut in_files = false;
    let mut extracted_index = 0_u64;

    for index in 1..archive.len() {
        check_cancelled(cancelled)?;
        let name = archive
            .by_index(index)
            .map_err(archive_error)?
            .name()
            .map_err(archive_error)?
            .into_owned();
        if let Some(etype) = entity_entry_type(&name) {
            if in_files {
                return Err(RestoreWorkerError::InvalidBackup(format!(
                    "entity entry {name} appears after file entries"
                )));
            }
            let entry = archive.by_index(index).map_err(archive_error)?;
            let reader = BufReader::new(entry);
            for row in serde_json::Deserializer::from_reader(reader).into_iter::<EntityRow>() {
                check_cancelled(cancelled)?;
                let row = row.map_err(|error| {
                    RestoreWorkerError::InvalidBackup(format!("{name}: {error}"))
                })?;
                let entity_id = row
                    .entity
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        RestoreWorkerError::InvalidBackup(format!(
                            "{name} contains an entity without a string id"
                        ))
                    })?
                    .parse::<Uuid>()
                    .map_err(|error| {
                        RestoreWorkerError::InvalidBackup(format!(
                            "{name} contains an invalid entity id: {error}"
                        ))
                    })?;
                if etype == "$files" {
                    let (location_id, metadata) = file_metadata(&row.entity)?;
                    if files.insert(location_id.clone(), metadata).is_some() {
                        return Err(RestoreWorkerError::InvalidBackup(format!(
                            "duplicate $files location-id {location_id}"
                        )));
                    }
                }
                append_entity_triples(
                    &mut batch,
                    sender,
                    attributes,
                    app_id,
                    etype,
                    entity_id,
                    row.entity,
                    row.created_at,
                )?;
            }
            continue;
        }

        let Some(location_id) = file_entry_location(&name) else {
            return Err(RestoreWorkerError::InvalidBackup(format!(
                "unexpected ZIP entry {name}"
            )));
        };
        in_files = true;
        validate_location_id(location_id)?;
        if !seen_file_entries.insert(location_id.to_owned()) {
            return Err(RestoreWorkerError::InvalidBackup(format!(
                "duplicate file entry for {location_id}"
            )));
        }
        let Some(metadata) = files.remove(location_id) else {
            return Err(RestoreWorkerError::InvalidBackup(format!(
                "file {location_id} has no matching $files entity"
            )));
        };
        extracted_index += 1;
        let extracted_path = work_dir.join(format!("{extracted_index}.blob"));
        let mut entry = archive.by_index(index).map_err(archive_error)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&extracted_path)
            .map_err(io_error)?;
        let mut buffer = vec![0_u8; 128 * 1024];
        loop {
            check_cancelled(cancelled)?;
            let read = entry.read(&mut buffer).map_err(io_error)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read]).map_err(io_error)?;
        }
        send_event(
            sender,
            RestoreEvent::File(RestoreFile {
                location_id: location_id.to_owned(),
                path: extracted_path,
                content_type: metadata.content_type,
                content_disposition: metadata.content_disposition,
                content_length: metadata.content_length,
            }),
        )?;
    }
    if !batch.is_empty() {
        send_event(sender, RestoreEvent::Triples(batch))?;
    }
    if !files.is_empty() {
        let mut missing = files.into_keys().collect::<Vec<_>>();
        missing.sort();
        return Err(RestoreWorkerError::InvalidBackup(format!(
            "$files entities are missing blobs: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Carries the complete authoritative JSONL row context.
fn append_entity_triples(
    triples: &mut Vec<RestoreTriple>,
    sender: &mpsc::Sender<RestoreEvent>,
    attributes: &HashMap<(String, String), RestoreAttribute>,
    app_id: Uuid,
    etype: &str,
    entity_id: Uuid,
    entity: Map<String, Value>,
    created_at: i64,
) -> Result<(), RestoreWorkerError> {
    for (label, value) in entity {
        let attribute = attributes
            .get(&(etype.to_owned(), label.clone()))
            .ok_or_else(|| {
                RestoreWorkerError::InvalidBackup(format!(
                    "missing attribute for {etype}.{label} on entity {entity_id}"
                ))
            })?;
        if attribute.one {
            push_restore_triple(
                triples,
                sender,
                restore_triple(attribute, app_id, entity_id, value, created_at),
            )?;
        } else {
            let values = value.as_array().ok_or_else(|| {
                RestoreWorkerError::InvalidBackup(format!(
                    "cardinality-many attribute {etype}.{label} is not an array"
                ))
            })?;
            for value in values.iter().cloned() {
                push_restore_triple(
                    triples,
                    sender,
                    restore_triple(attribute, app_id, entity_id, value, created_at),
                )?;
            }
        }
    }
    Ok(())
}

fn push_restore_triple(
    triples: &mut Vec<RestoreTriple>,
    sender: &mpsc::Sender<RestoreEvent>,
    triple: RestoreTriple,
) -> Result<(), RestoreWorkerError> {
    triples.push(triple);
    if triples.len() == TRIPLE_BATCH_SIZE {
        let ready = std::mem::replace(triples, Vec::with_capacity(TRIPLE_BATCH_SIZE));
        send_event(sender, RestoreEvent::Triples(ready))?;
    }
    Ok(())
}

fn restore_triple(
    attribute: &RestoreAttribute,
    app_id: Uuid,
    entity_id: Uuid,
    value: Value,
    created_at: i64,
) -> RestoreTriple {
    RestoreTriple {
        app_id,
        entity_id,
        attr_id: attribute.id,
        value,
        ea: attribute.one,
        eav: attribute.reference,
        av: attribute.unique,
        ave: attribute.indexed,
        vae: attribute.reference,
        created_at,
        checked_data_type: attribute.checked_data_type.clone(),
    }
}

fn attribute_map(attributes: &[AdminAttribute]) -> HashMap<(String, String), RestoreAttribute> {
    attributes
        .iter()
        .map(|attribute| {
            (
                (
                    attribute.forward_identity.1.clone(),
                    attribute.forward_identity.2.clone(),
                ),
                RestoreAttribute {
                    id: attribute.id,
                    one: attribute.cardinality == "one",
                    reference: attribute.value_type == "ref",
                    unique: attribute.unique,
                    indexed: attribute.indexed,
                    checked_data_type: attribute.checked_data_type.clone(),
                },
            )
        })
        .collect()
}

fn file_metadata(
    entity: &Map<String, Value>,
) -> Result<(String, RestoreFileMetadata), RestoreWorkerError> {
    let location_id = entity
        .get("location-id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            RestoreWorkerError::InvalidBackup(
                "$files entity is missing a string location-id".to_owned(),
            )
        })?
        .to_owned();
    validate_location_id(&location_id)?;
    let string_field = |name: &str| -> Result<Option<String>, RestoreWorkerError> {
        entity
            .get(name)
            .map(|value| {
                value.as_str().map(str::to_owned).ok_or_else(|| {
                    RestoreWorkerError::InvalidBackup(format!("$files {name} must be a string"))
                })
            })
            .transpose()
    };
    let content_length = entity
        .get("size")
        .map(|value| {
            value.as_u64().ok_or_else(|| {
                RestoreWorkerError::InvalidBackup(
                    "$files size must be an unsigned integer".to_owned(),
                )
            })
        })
        .transpose()?;
    Ok((
        location_id,
        RestoreFileMetadata {
            content_type: string_field("content-type")?,
            content_disposition: string_field("content-disposition")?,
            content_length,
        },
    ))
}

fn entity_entry_type(name: &str) -> Option<&str> {
    name.strip_prefix("entities/")?.strip_suffix(".jsonl")
}

fn file_entry_location(name: &str) -> Option<&str> {
    name.strip_prefix("files/")
}

fn validate_location_id(location_id: &str) -> Result<(), RestoreWorkerError> {
    if location_id.is_empty()
        || location_id.contains('/')
        || location_id.contains('\\')
        || location_id.contains("..")
    {
        return Err(RestoreWorkerError::InvalidBackup(format!(
            "unsafe file location-id {location_id:?}"
        )));
    }
    Ok(())
}

fn send_event(
    sender: &mpsc::Sender<RestoreEvent>,
    event: RestoreEvent,
) -> Result<(), RestoreWorkerError> {
    sender
        .blocking_send(event)
        .map_err(|_| RestoreWorkerError::Cancelled)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), RestoreWorkerError> {
    if cancelled.load(Ordering::Acquire) {
        Err(RestoreWorkerError::Cancelled)
    } else {
        Ok(())
    }
}

async fn validate_job_owner(client: &Client, job: &RestoreJob) -> Result<(), RestoreWorkerError> {
    if job.creator_id.is_some() == job.org_id.is_some() {
        return Err(RestoreWorkerError::InvalidBackup(
            "restore job must have exactly one owner".to_owned(),
        ));
    }
    let (table, id) = if let Some(creator_id) = job.creator_id {
        ("instant_users", creator_id)
    } else if let Some(org_id) = job.org_id {
        ("orgs", org_id)
    } else {
        return Err(RestoreWorkerError::InvalidBackup(
            "restore job has no owner".to_owned(),
        ));
    };
    let query = format!("SELECT 1 FROM {table} WHERE id = $1");
    if client.query_opt(&query, &[&id]).await?.is_none() {
        return Err(RestoreWorkerError::InvalidBackup(format!(
            "restore owner {id} no longer exists"
        )));
    }
    Ok(())
}

fn format_progress(stats: RestoreStats, totals: RestoreTotals) -> String {
    match (totals.triples, totals.files) {
        (Some(triples), Some(files)) => format!(
            "Restored {} / {} triples, {} / {} files",
            stats.triples, triples, stats.files, files
        ),
        _ => format!("Restored {} triples, {} files", stats.triples, stats.files),
    }
}

struct RestoreWorkDir {
    path: PathBuf,
}

impl RestoreWorkDir {
    fn create() -> Result<Self, RestoreWorkerError> {
        let path = std::env::temp_dir().join(format!("instant-restore-work-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).map_err(io_error)?;
        Ok(Self { path })
    }
}

impl Drop for RestoreWorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn archive_error(error: impl std::fmt::Display) -> RestoreWorkerError {
    RestoreWorkerError::Archive(error.to_string())
}

fn io_error(error: impl std::fmt::Display) -> RestoreWorkerError {
    RestoreWorkerError::Io(error.to_string())
}

fn api_error(error: &ApiError) -> RestoreWorkerError {
    RestoreWorkerError::InvalidBackup(format!("{error:?}"))
}

fn restore_api_io(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::internal_error(error)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    fn write_test_archive(entries: &[(&str, &[u8])]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("instant-restore-unit-{}.zip", Uuid::new_v4()));
        let file = std::fs::File::create(&path).unwrap();
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, contents) in entries {
            archive.start_file(*name, options).unwrap();
            archive.write_all(contents).unwrap();
        }
        archive.finish().unwrap();
        path
    }

    #[test]
    fn restore_zip_errors_hide_the_io_cause() {
        let error = restore_api_io("permission denied writing /srv/secret/restore.zip");

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Internal error");
        assert!(!error.body().message.contains("secret"));
    }

    #[test]
    fn checked_in_backup_has_the_authoritative_shape() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zeneca-backup.zip");
        let config = inspect_archive(path.to_str().unwrap()).unwrap();
        assert_eq!(config.triple_count, Some(2_855));
        assert_eq!(config.counts.get("users"), Some(&4));
        assert!(config.schema.get("entities").is_some());
    }

    #[test]
    fn progress_uses_totals_only_when_both_are_known() {
        let stats = RestoreStats {
            triples: 12,
            files: 3,
        };
        assert_eq!(
            format_progress(
                stats,
                RestoreTotals {
                    triples: Some(20),
                    files: Some(4)
                }
            ),
            "Restored 12 / 20 triples, 3 / 4 files"
        );
        assert_eq!(
            format_progress(
                stats,
                RestoreTotals {
                    triples: Some(20),
                    files: None
                }
            ),
            "Restored 12 triples, 3 files"
        );
    }

    #[test]
    fn location_ids_cannot_escape_the_restore_namespace() {
        assert!(validate_location_id("8f789820-6d60-4d66-b5b5-14243cd4e85a").is_ok());
        assert!(validate_location_id("../secret").is_err());
        assert!(validate_location_id("nested/blob").is_err());
        assert!(validate_location_id("").is_err());
    }

    #[test]
    fn producer_pairs_file_metadata_with_blob_and_preserves_app_id() {
        let config =
            br#"{"schema":{"entities":{},"links":{}},"tripleCount":5,"counts":{"$files":1}}"#;
        let entity_id = Uuid::new_v4();
        let entity = format!(
            "{}\n",
            json!({
                "entity": {
                    "id": entity_id,
                    "location-id": "file-location",
                    "content-type": "text/plain",
                    "content-disposition": "inline",
                    "size": 4
                },
                "createdAt": 42
            })
        );
        let path = write_test_archive(&[
            ("config.json", config),
            ("entities/$files.jsonl", entity.as_bytes()),
            ("files/file-location", b"blob"),
        ]);
        let app_id = Uuid::new_v4();
        let attributes = [
            "id",
            "location-id",
            "content-type",
            "content-disposition",
            "size",
        ]
        .into_iter()
        .map(|label| {
            (
                ("$files".to_owned(), label.to_owned()),
                RestoreAttribute {
                    id: Uuid::new_v4(),
                    one: true,
                    reference: false,
                    unique: false,
                    indexed: false,
                    checked_data_type: None,
                },
            )
        })
        .collect::<HashMap<_, _>>();
        let work_dir = RestoreWorkDir::create().unwrap();
        let (sender, mut receiver) = mpsc::channel(EVENT_BUFFER_SIZE);
        produce_archive_events(
            path.to_str().unwrap(),
            &work_dir.path,
            app_id,
            &attributes,
            &sender,
            &AtomicBool::new(false),
        )
        .unwrap();
        drop(sender);

        let mut triples = Vec::new();
        let mut restored_file = None;
        while let Some(event) = receiver.blocking_recv() {
            match event {
                RestoreEvent::Triples(batch) => triples.extend(batch),
                RestoreEvent::File(file) => restored_file = Some(file),
            }
        }
        assert_eq!(triples.len(), 5);
        assert!(triples.iter().all(|triple| triple.app_id == app_id));
        assert!(triples.iter().all(|triple| triple.created_at == 42));
        let restored_file = restored_file.unwrap();
        assert_eq!(restored_file.location_id, "file-location");
        assert_eq!(restored_file.content_type.as_deref(), Some("text/plain"));
        assert_eq!(restored_file.content_length, Some(4));
        assert_eq!(std::fs::read(restored_file.path).unwrap(), b"blob");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn archive_rejects_config_after_entity_data() {
        let path = write_test_archive(&[
            ("entities/items.jsonl", b""),
            ("config.json", br#"{"schema":{}}"#),
        ]);
        let error = inspect_archive(path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("first ZIP entry"));
        std::fs::remove_file(path).unwrap();
    }
}
