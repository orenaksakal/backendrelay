use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_postgres::Client;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::{
    AuroraCloneError, AuroraCloneLifecycle, BackupWorkerError, PostgresBackupWorker,
    PostgresConnectionDrivers, PostgresConnectionError, PostgresFailoverGate, PostgresPool,
    PostgresPoolError, ScheduledBackupOptions, StorageObjectStore,
};

const PROCESS_COUNT: usize = 8;
const INVALIDATOR_SLOT_NUM: i32 = 2;
const NORMAL_RETENTION_DAYS: i64 = 7;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NightlyBackupRun {
    SkippedFailover,
    AlreadyOwned,
    Cancelled,
    Completed(NightlyBackupReport),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NightlyBackupReport {
    pub process_id: Uuid,
    pub apps_processed: i64,
    pub triples_processed: i64,
    pub failed_apps: Vec<(Uuid, String)>,
}

#[derive(Debug, Error)]
pub enum NightlyBackupError {
    #[error(transparent)]
    Clone(#[from] AuroraCloneError),
    #[error(transparent)]
    Connection(#[from] PostgresConnectionError),
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
    #[error(transparent)]
    Backup(#[from] BackupWorkerError),
    #[error("invalid nightly backup logical start LSN: {0}")]
    InvalidLsn(String),
    #[error("nightly backup worker task failed: {0}")]
    WorkerTask(#[from] tokio::task::JoinError),
    #[error("nightly backup clone cleanup failed after backup error: {backup}; cleanup: {cleanup}")]
    BackupAndCleanup { backup: String, cleanup: String },
}

#[derive(Debug, Error)]
pub enum NightlyBackupSupervisorError {
    #[error("nightly backup task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    #[error("nightly backup failed: {0}")]
    Run(String),
}

/// Fleet-owned automated backup coordinator. Reads come from the selected
/// snapshot provider while progress and backup rows commit to the primary
/// database.
pub struct PostgresNightlyBackupWorker<S, C> {
    primary: PostgresPool,
    objects: Arc<S>,
    clones: Arc<C>,
    failover: Arc<PostgresFailoverGate>,
    source_cluster_id: String,
    machine_id: Uuid,
    ephemeral_creator_email: String,
}

/// Owns the long-running daily operation outside the periodic scheduler so a
/// process shutdown can cancel it and wait for Aurora clone cleanup.
pub struct NightlyBackupSupervisor<S, C> {
    worker: Arc<PostgresNightlyBackupWorker<S, C>>,
    cancellation: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
    last_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl<S, C> NightlyBackupSupervisor<S, C>
where
    S: StorageObjectStore,
    C: AuroraCloneLifecycle,
{
    #[must_use]
    pub fn new(worker: Arc<PostgresNightlyBackupWorker<S, C>>) -> Arc<Self> {
        Arc::new(Self {
            worker,
            cancellation: CancellationToken::new(),
            task: Mutex::new(None),
            last_error: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Starts today's fleet-owned run if this process is idle. A prior
    /// asynchronous failure is surfaced to the scheduler after the replacement
    /// run has been launched.
    ///
    /// # Errors
    ///
    /// Returns a failure recorded by the prior managed run.
    pub async fn kick(
        self: &Arc<Self>,
        date: chrono::NaiveDate,
    ) -> Result<(), NightlyBackupSupervisorError> {
        let mut task = self.task.lock().await;
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        if let Some(completed) = task.take() {
            completed.await?;
        }
        if self.cancellation.is_cancelled() {
            return Ok(());
        }
        let worker = Arc::clone(&self.worker);
        let cancellation = self.cancellation.child_token();
        let last_error = Arc::clone(&self.last_error);
        *task = Some(tokio::spawn(async move {
            if let Err(error) = worker.run(date, &cancellation).await {
                *last_error
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_string());
            }
        }));
        drop(task);
        match self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Some(error) => Err(NightlyBackupSupervisorError::Run(error)),
            None => Ok(()),
        }
    }

    /// Cancels an active clone scan and waits for its `finally`-equivalent
    /// teardown path. Returns `true` only when the process grace elapsed and
    /// the task had to be aborted.
    ///
    /// # Errors
    ///
    /// Returns a task join error or the last clone/backup failure.
    pub async fn shutdown(
        &self,
        grace: std::time::Duration,
    ) -> Result<bool, NightlyBackupSupervisorError> {
        self.cancellation.cancel();
        let Some(mut task) = self.task.lock().await.take() else {
            return self.take_last_error().map(|()| false);
        };
        if let Ok(result) = tokio::time::timeout(grace, &mut task).await {
            result?;
        } else {
            task.abort();
            let _ = task.await;
            return Ok(true);
        }
        self.take_last_error().map(|()| false)
    }

    fn take_last_error(&self) -> Result<(), NightlyBackupSupervisorError> {
        match self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Some(error) => Err(NightlyBackupSupervisorError::Run(error)),
            None => Ok(()),
        }
    }
}

impl<S, C> PostgresNightlyBackupWorker<S, C>
where
    S: StorageObjectStore,
    C: AuroraCloneLifecycle,
{
    #[must_use]
    pub fn new(
        primary: PostgresPool,
        objects: Arc<S>,
        clones: Arc<C>,
        failover: Arc<PostgresFailoverGate>,
        source_cluster_id: impl Into<String>,
        machine_id: Uuid,
        ephemeral_creator_email: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            primary,
            objects,
            clones,
            failover,
            source_cluster_id: source_cluster_id.into(),
            machine_id,
            ephemeral_creator_email: ephemeral_creator_email.into(),
        })
    }

    /// Runs at most once for the supplied Pacific calendar date across the
    /// fleet. Cancellation stops snapshot reads but still attempts teardown.
    ///
    /// # Errors
    ///
    /// Returns provider, PostgreSQL, driver, task, or artifact failures.
    pub async fn run(
        &self,
        date: chrono::NaiveDate,
        cancellation: &CancellationToken,
    ) -> Result<NightlyBackupRun, NightlyBackupError> {
        let span = tracing::info_span!("backup/daily-backup", date = %date);
        let result = Self::run_inner(self, date, cancellation)
            .instrument(span.clone())
            .await;
        if let Err(error) = &result {
            tracing::error_span!(parent: &span, "backup/daily-backup-error")
                .in_scope(|| tracing::error!("nightly backup failed: {error}"));
        }
        result
    }

    async fn run_inner(
        &self,
        date: chrono::NaiveDate,
        cancellation: &CancellationToken,
    ) -> Result<NightlyBackupRun, NightlyBackupError> {
        if self.failover.is_failing_over().await.map_err(|error| {
            NightlyBackupError::Backup(BackupWorkerError::Connection(error.to_string()))
        })? {
            return Ok(NightlyBackupRun::SkippedFailover);
        }
        if !self.try_grab(date).await? {
            return Ok(NightlyBackupRun::AlreadyOwned);
        }
        let options = scheduled_backup_options(Utc::now());
        let clone = self.clones.create(self.source_cluster_id.clone()).await?;
        let work = self
            .backup_clone(
                &clone.database_url,
                clone.snapshot_time,
                self.clones.logical_start_lsn_query(),
                options,
                cancellation,
            )
            .await;
        let cleanup = self.clones.delete(clone).await;
        match (work, cleanup) {
            (Ok((_, true)), Ok(())) => Ok(NightlyBackupRun::Cancelled),
            (Ok((report, false)), Ok(())) => Ok(NightlyBackupRun::Completed(report)),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
            (Err(backup), Err(cleanup)) => Err(NightlyBackupError::BackupAndCleanup {
                backup: backup.to_string(),
                cleanup: cleanup.to_string(),
            }),
        }
    }

    async fn try_grab(&self, date: chrono::NaiveDate) -> Result<bool, NightlyBackupError> {
        let key = format!("daily-backup-{date}");
        let client = self.primary.acquire().await?;
        Ok(client
            .query_opt(
                "INSERT INTO grabs (id) VALUES ($1)
                 ON CONFLICT (id) DO NOTHING RETURNING id",
                &[&key],
            )
            .await?
            .is_some())
    }

    async fn backup_clone(
        &self,
        database_url: &str,
        backup_at: DateTime<Utc>,
        logical_start_lsn_query: &'static str,
        options: ScheduledBackupOptions,
        stop: &CancellationToken,
    ) -> Result<(NightlyBackupReport, bool), NightlyBackupError> {
        let exported_snapshot = self.clones.requires_exported_snapshot();
        let connection_count = PROCESS_COUNT + usize::from(exported_snapshot);
        let (mut clients, drivers) = tokio::select! {
            () = stop.cancelled() => return Ok((NightlyBackupReport::default(), true)),
            connected = PostgresConnectionDrivers::connect_many(database_url, connection_count) => connected?,
        };
        let cancellation = CancellationToken::new();
        let driver_token = cancellation.child_token();
        let driver = tokio::spawn(drivers.supervise(driver_token));
        let result = async {
            if exported_snapshot {
                let exporter = clients
                    .pop()
                    .expect("exported snapshot connection count includes coordinator");
                exporter
                    .batch_execute(
                        "BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY;
                     SET LOCAL idle_in_transaction_session_timeout = '1h'",
                    )
                    .await?;
                let row = exporter
                    .query_one(
                        "SELECT pg_export_snapshot()::text,
                            pg_current_wal_lsn()::text,
                            clock_timestamp()",
                        &[],
                    )
                    .await?;
                let snapshot_id = Arc::<str>::from(row.get::<_, String>(0));
                let lsn = row.get::<_, String>(1);
                let snapshot_time = row.get::<_, DateTime<Utc>>(2);
                let clients = clients
                    .into_iter()
                    .map(|client| Arc::new(Mutex::new(client)))
                    .collect::<Vec<_>>();
                let work = self
                    .backup_with_clients(
                        &clients,
                        snapshot_time,
                        logical_start_lsn_query,
                        Some(lsn),
                        Some(snapshot_id),
                        options,
                        stop,
                    )
                    .await;
                let rollback = exporter.batch_execute("ROLLBACK").await;
                // Stop supervision before the run-local clients are dropped;
                // their expected closure must not look like a driver failure.
                cancellation.cancel();
                match (work, rollback) {
                    (Ok(report), Ok(())) => Ok(report),
                    (Err(error), _) => Err(error),
                    (Ok(_), Err(error)) => Err(error.into()),
                }
            } else {
                let clients = clients
                    .into_iter()
                    .map(|client| Arc::new(Mutex::new(client)))
                    .collect::<Vec<_>>();
                let result = self
                    .backup_with_clients(
                        &clients,
                        backup_at,
                        logical_start_lsn_query,
                        None,
                        None,
                        options,
                        stop,
                    )
                    .await;
                cancellation.cancel();
                result
            }
        }
        .await;
        cancellation.cancel();
        driver.await??;
        result
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn backup_with_clients(
        &self,
        clients: &[Arc<Mutex<Client>>],
        backup_at: DateTime<Utc>,
        logical_start_lsn_query: &'static str,
        logical_start_lsn: Option<String>,
        exported_snapshot_id: Option<Arc<str>>,
        options: ScheduledBackupOptions,
        stop: &CancellationToken,
    ) -> Result<(NightlyBackupReport, bool), NightlyBackupError> {
        if stop.is_cancelled() {
            return Ok((NightlyBackupReport::default(), true));
        }
        for client in clients {
            client
                .lock()
                .await
                .batch_execute("SET idle_in_transaction_session_timeout = '1h'")
                .await?;
        }
        let lsn_text = match logical_start_lsn {
            Some(lsn) => lsn,
            None => clients[0]
                .lock()
                .await
                .query_one(logical_start_lsn_query, &[])
                .await?
                .get::<_, String>(0),
        };
        let lsn = lsn_text
            .parse::<instant_wal::Lsn>()
            .map_err(|_| NightlyBackupError::InvalidLsn(lsn_text))?;
        let isn = instant_wal::Isn::new(INVALIDATOR_SLOT_NUM, lsn);
        let process_id = Uuid::new_v4();
        {
            let primary = self.primary.acquire().await?;
            primary
                .execute(
                    "INSERT INTO backup_jobs (id, isn, backup_at, machine_id)
                     VALUES ($1, $2::text::isn, $3, $4)",
                    &[
                        &process_id,
                        &isn.to_postgres_composite(),
                        &backup_at,
                        &self.machine_id,
                    ],
                )
                .await?;
        }
        let skip = backup_skip_app_ids(
            self.failover
                .flag_value("backup-skip-app-ids")
                .await
                .map_err(|error| {
                    NightlyBackupError::Backup(BackupWorkerError::Connection(error.to_string()))
                })?
                .as_ref(),
        );
        let skip = skip.into_iter().collect::<Vec<_>>();
        let apps = {
            let mut catalog = clients[0].lock().await;
            let transaction = catalog
                .build_transaction()
                .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()
                .await?;
            if let Some(snapshot_id) = exported_snapshot_id.as_deref() {
                let import = crate::backup_worker::postgres_snapshot_import_sql(snapshot_id)?;
                transaction.batch_execute(&import).await?;
            }
            let creator_id = transaction
                .query_opt(
                    "SELECT id FROM instant_users WHERE email = $1 LIMIT 1",
                    &[&self.ephemeral_creator_email],
                )
                .await?
                .map(|row| row.get::<_, Uuid>(0));
            let apps = transaction
                .query(
                    "WITH sums AS (
                   SELECT app_id, sum(total)::bigint AS triple_total
                   FROM attr_sketches GROUP BY app_id
                 )
                 SELECT apps.id FROM apps
                 LEFT JOIN sums ON sums.app_id = apps.id
                 WHERE apps.creator_id IS DISTINCT FROM $1
                   AND apps.deletion_marked_at IS NULL
                   AND apps.id <> ALL($2)
                 ORDER BY coalesce(sums.triple_total, 0) DESC, apps.id",
                    &[&creator_id, &skip],
                )
                .await?
                .into_iter()
                .map(|row| row.get::<_, Uuid>(0))
                .collect::<Vec<_>>();
            transaction.commit().await?;
            Arc::new(apps)
        };
        let next = Arc::new(AtomicUsize::new(0));
        let (outcomes_tx, mut outcomes) = mpsc::unbounded_channel();
        let mut handles = Vec::with_capacity(clients.len());
        for (ordinal, snapshot) in clients.iter().enumerate() {
            let mut worker = PostgresBackupWorker::with_control_pool(
                self.primary.clone(),
                Arc::clone(snapshot),
                Arc::clone(&self.objects),
                format!("nightly-{}-{ordinal}", self.machine_id),
            );
            if let Some(snapshot_id) = exported_snapshot_id.as_ref() {
                worker = worker.with_snapshot_id(Arc::clone(snapshot_id));
            }
            let apps = Arc::clone(&apps);
            let next = Arc::clone(&next);
            let outcomes = outcomes_tx.clone();
            let stop = stop.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    if stop.is_cancelled() {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::AcqRel);
                    let Some(&app_id) = apps.get(index) else {
                        break;
                    };
                    let result = tokio::select! {
                        () = stop.cancelled() => break,
                        result = worker.run_scheduled_app(process_id, app_id, backup_at, isn, options) => result,
                    };
                    if outcomes.send((index, app_id, result)).is_err() {
                        break;
                    }
                }
            }));
        }
        drop(outcomes_tx);
        let mut report = NightlyBackupReport {
            process_id,
            ..NightlyBackupReport::default()
        };
        let mut pending = BTreeMap::new();
        let mut next_progress = 0;
        while let Some((index, app_id, outcome)) = outcomes.recv().await {
            pending.insert(index, (app_id, outcome));
            while let Some((app_id, outcome)) = pending.remove(&next_progress) {
                report.apps_processed = report.apps_processed.saturating_add(1);
                match outcome {
                    Ok(outcome) => {
                        report.triples_processed =
                            report.triples_processed.saturating_add(outcome.triples);
                    }
                    Err(error) => report.failed_apps.push((app_id, error.to_string())),
                }
                let primary = self.primary.acquire().await?;
                primary
                    .execute(
                        "UPDATE backup_jobs
                         SET max_app_id = $2, triples_processed = $3, apps_processed = $4
                         WHERE id = $1",
                        &[
                            &process_id,
                            &app_id,
                            &report.triples_processed,
                            &report.apps_processed,
                        ],
                    )
                    .await?;
                next_progress += 1;
            }
        }
        for handle in handles {
            handle.await?;
        }
        let cancelled = stop.is_cancelled();
        if !cancelled {
            let primary = self.primary.acquire().await?;
            primary
                .execute(
                    "UPDATE backup_jobs SET completed_at = now() WHERE id = $1",
                    &[&process_id],
                )
                .await?;
        }
        Ok((report, cancelled))
    }
}

fn scheduled_backup_options(now: DateTime<Utc>) -> ScheduledBackupOptions {
    ScheduledBackupOptions {
        expires_at: now + chrono::Duration::days(NORMAL_RETENTION_DAYS),
        expire_objects: true,
    }
}

fn backup_skip_app_ids(value: Option<&Value>) -> HashSet<Uuid> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|value| value.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn skip_list_ignores_malformed_values_like_the_jvm_parser() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let parsed =
            backup_skip_app_ids(Some(&json!([first, "invalid", null, second.to_string()])));
        assert_eq!(parsed, HashSet::from([first, second]));
        assert!(backup_skip_app_ids(Some(&json!({}))).is_empty());
    }

    #[test]
    fn scheduled_backups_use_normal_retention_and_object_expiry() {
        let now = DateTime::parse_from_rfc3339("2026-08-29T12:00:00Z")
            .unwrap()
            .to_utc();
        let normal = scheduled_backup_options(now);
        assert!(normal.expire_objects);
        assert_eq!(normal.expires_at, now + chrono::Duration::days(7));
    }
}
