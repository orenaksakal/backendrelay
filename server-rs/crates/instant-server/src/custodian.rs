use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, Transaction};
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::postgres_realtime_cluster::MACHINE_HEALTH_GRACE;
use crate::{FailoverGateError, PostgresFailoverGate, ServerEnvironment};
use crate::{PostgresPool, PostgresPoolError};

const DEFAULT_BATCH_SIZE: i64 = 1_000;
const DEFAULT_ATTR_BATCH_SIZE: i64 = 1;
const DEFAULT_MAX_ATTEMPTS: i32 = 3;
const DEFAULT_LEASE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_FAILED_RETENTION: Duration = Duration::from_hours(24);
const DEFAULT_MAX_REPLICATION_LAG_BYTES: i64 = 100 * 1_024 * 1_024;
const DEFAULT_HOSTED_IDLE_SLEEP: Duration = Duration::from_secs(5);
const DEFAULT_DEVELOPMENT_IDLE_SLEEP: Duration = Duration::from_secs(600);
const DEFAULT_TRANSACTION_RETENTION_DAYS: i64 = 30;
const TRANSACTION_RETENTION_DAYS_ENV: &str = "INSTANT_TRANSACTION_RETENTION_DAYS";

const RUNTIME_FLAG_NAMES: &[&str] = &[
    "custodian-batch-size",
    "custodian-idle-sleep-ms",
    "custodian-attr-batch-size",
    "custodian-lease-timeout-seconds",
    "custodian-max-attempts",
    "custodian-failed-retention-seconds",
    "custodian-max-replication-lag-bytes",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CustodianRuntimeConfig {
    batch_size: i64,
    idle_sleep: Duration,
    attr_batch_size: i64,
    lease_timeout: Duration,
    max_attempts: i32,
    failed_retention: Duration,
    max_replication_lag_bytes: i64,
}

impl CustodianRuntimeConfig {
    #[must_use]
    pub const fn for_environment(environment: ServerEnvironment) -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            idle_sleep: if matches!(environment, ServerEnvironment::Development) {
                DEFAULT_DEVELOPMENT_IDLE_SLEEP
            } else {
                DEFAULT_HOSTED_IDLE_SLEEP
            },
            attr_batch_size: DEFAULT_ATTR_BATCH_SIZE,
            lease_timeout: DEFAULT_LEASE_TIMEOUT,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            failed_retention: DEFAULT_FAILED_RETENTION,
            max_replication_lag_bytes: DEFAULT_MAX_REPLICATION_LAG_BYTES,
        }
    }

    #[must_use]
    pub const fn idle_sleep(self) -> Duration {
        self.idle_sleep
    }

    #[must_use]
    pub const fn batch_size(self) -> i64 {
        self.batch_size
    }

    #[must_use]
    pub const fn lease_timeout(self) -> Duration {
        self.lease_timeout
    }

    #[must_use]
    pub const fn failed_retention(self) -> Duration {
        self.failed_retention
    }

    fn apply_values(mut self, values: &HashMap<String, Value>) -> Result<Self, CustodianError> {
        self.batch_size = positive_i64(values, "custodian-batch-size", self.batch_size)?;
        self.attr_batch_size =
            positive_i64(values, "custodian-attr-batch-size", self.attr_batch_size)?;
        self.max_replication_lag_bytes = non_negative_i64(
            values,
            "custodian-max-replication-lag-bytes",
            self.max_replication_lag_bytes,
        )?;
        self.idle_sleep = Duration::from_millis(non_negative_u64(
            values,
            "custodian-idle-sleep-ms",
            u64::try_from(self.idle_sleep.as_millis()).unwrap_or(u64::MAX),
        )?);
        self.lease_timeout = Duration::from_secs(positive_u64(
            values,
            "custodian-lease-timeout-seconds",
            self.lease_timeout.as_secs(),
        )?);
        self.failed_retention = Duration::from_secs(positive_u64(
            values,
            "custodian-failed-retention-seconds",
            self.failed_retention.as_secs(),
        )?);
        self.max_attempts = i32::try_from(positive_i64(
            values,
            "custodian-max-attempts",
            i64::from(self.max_attempts),
        )?)
        .map_err(|_| invalid_flag(values, "custodian-max-attempts"))?;
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CustodianTickOutcome {
    Idle,
    Paused { lag_bytes: i64 },
    FailoverPaused,
    CompletedStep { step_id: Uuid, kind: String },
    AbortedRestoredPlan { app_id: Uuid, attr_id: Option<Uuid> },
    FailedAttempt { step_id: Uuid },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HardDeletionSweepOutcome {
    pub attrs_enqueued: u64,
    pub apps_enqueued: u64,
}

#[derive(Debug, Error)]
pub enum CustodianError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
    #[error("custodian step {0} is no longer owned by this worker")]
    OwnershipLost(Uuid),
    #[error("unknown custodian step type {0}")]
    UnknownStep(String),
    #[error(
        "config flag `{setting}` must be a valid non-negative or positive integer, found {value}"
    )]
    InvalidConfigFlag { setting: &'static str, value: Value },
    #[error(transparent)]
    FailoverGate(#[from] FailoverGateError),
}

/// Durable, dependency-ordered hard deletion worker.
pub struct PostgresCustodian {
    client: PostgresClientSource,
    worker_id: String,
    defaults: CustodianRuntimeConfig,
    failover_gate: Option<Arc<PostgresFailoverGate>>,
}

impl PostgresCustodian {
    #[must_use]
    pub fn new(client: Client, worker_id: impl Into<String>) -> Self {
        Self::with_shared_client(Arc::new(Mutex::new(client)), worker_id)
    }

    #[must_use]
    pub fn with_shared_client(client: Arc<Mutex<Client>>, worker_id: impl Into<String>) -> Self {
        Self {
            client: PostgresClientSource::fixed_shared(client),
            worker_id: worker_id.into(),
            defaults: CustodianRuntimeConfig::for_environment(ServerEnvironment::Production),
            failover_gate: None,
        }
    }

    #[must_use]
    pub fn from_pool(client: PostgresPool, worker_id: impl Into<String>) -> Self {
        Self {
            client: PostgresClientSource::pooled(client),
            worker_id: worker_id.into(),
            defaults: CustodianRuntimeConfig::for_environment(ServerEnvironment::Production),
            failover_gate: None,
        }
    }

    #[must_use]
    pub fn with_limits(mut self, batch_size: i64, attr_batch_size: i64) -> Self {
        self.defaults.batch_size = batch_size.clamp(1, 10_000);
        self.defaults.attr_batch_size = attr_batch_size.clamp(1, 100);
        self
    }

    #[must_use]
    pub fn with_environment(mut self, environment: ServerEnvironment) -> Self {
        self.defaults.idle_sleep = CustodianRuntimeConfig::for_environment(environment).idle_sleep;
        self
    }

    #[must_use]
    pub fn with_failover_gate(mut self, gate: Arc<PostgresFailoverGate>) -> Self {
        self.failover_gate = Some(gate);
        self
    }

    /// Resolves every live custodian tuning flag in one config-app read.
    ///
    /// # Errors
    ///
    /// Returns a database error or an explicit malformed-value error.
    pub async fn runtime_config(&self) -> Result<CustodianRuntimeConfig, CustodianError> {
        let Some(gate) = &self.failover_gate else {
            return Ok(self.defaults);
        };
        let values = gate.flag_values(RUNTIME_FLAG_NAMES).await?;
        self.defaults.apply_values(&values)
    }

    /// Atomically enqueues `triples <- transactions <- attrs <- app`.
    /// Returns false when the same plan is already present.
    ///
    /// # Errors
    ///
    /// Returns non-uniqueness database failures.
    pub async fn enqueue_app_deletion(&self, app_id: Uuid) -> Result<bool, CustodianError> {
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        let result = transaction
            .execute(
                "WITH triples_row AS (
                   INSERT INTO custodian (app_id, type) VALUES ($1, 'triples') RETURNING id
                 ), transactions_row AS (
                   INSERT INTO custodian (app_id, type, depends_on)
                   SELECT $1, 'transactions', id FROM triples_row RETURNING id
                 ), attrs_row AS (
                   INSERT INTO custodian (app_id, type, depends_on)
                   SELECT $1, 'attrs', id FROM transactions_row RETURNING id
                 )
                 INSERT INTO custodian (app_id, type, depends_on)
                 SELECT $1, 'app', id FROM attrs_row",
                &[&app_id],
            )
            .await;
        match result {
            Ok(_) => {
                transaction.commit().await?;
                Ok(true)
            }
            Err(error) if error.code() == Some(&SqlState::UNIQUE_VIOLATION) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Atomically enqueues `triples <- attr`.
    ///
    /// # Errors
    ///
    /// Returns non-uniqueness database failures.
    pub async fn enqueue_attr_deletion(
        &self,
        app_id: Uuid,
        attr_id: Uuid,
    ) -> Result<bool, CustodianError> {
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        let result = transaction
            .execute(
                "WITH triples_row AS (
                   INSERT INTO custodian (app_id, attr_id, type)
                   VALUES ($1, $2, 'triples') RETURNING id
                 )
                 INSERT INTO custodian (app_id, attr_id, type, depends_on)
                 SELECT $1, $2, 'attr', id FROM triples_row",
                &[&app_id, &attr_id],
            )
            .await;
        match result {
            Ok(_) => {
                transaction.commit().await?;
                Ok(true)
            }
            Err(error) if error.code() == Some(&SqlState::UNIQUE_VIOLATION) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Enqueues every app and attribute whose soft-delete marker is at least
    /// `grace_days` old, matching the daily hard-deletion sweep.
    ///
    /// # Errors
    ///
    /// Returns database or enqueue failures.
    pub async fn enqueue_due_deletions(
        &self,
        now: DateTime<Utc>,
        grace_days: i64,
    ) -> Result<HardDeletionSweepOutcome, CustodianError> {
        let cutoff = now - ChronoDuration::days(grace_days.max(0));
        let (attrs, apps) = {
            let client = self.client.acquire().await?;
            let attrs = client
                .query(
                    "SELECT app_id, id FROM attrs
                     WHERE deletion_marked_at IS NOT NULL AND deletion_marked_at <= $1",
                    &[&cutoff],
                )
                .await?
                .into_iter()
                .map(|row| (row.get::<_, Uuid>(0), row.get::<_, Uuid>(1)))
                .collect::<Vec<_>>();
            let apps = client
                .query(
                    "SELECT id FROM apps
                     WHERE deletion_marked_at IS NOT NULL AND deletion_marked_at <= $1",
                    &[&cutoff],
                )
                .await?
                .into_iter()
                .map(|row| row.get::<_, Uuid>(0))
                .collect::<Vec<_>>();
            (attrs, apps)
        };
        let mut outcome = HardDeletionSweepOutcome::default();
        for (app_id, attr_id) in attrs {
            outcome.attrs_enqueued += u64::from(self.enqueue_attr_deletion(app_id, attr_id).await?);
        }
        for app_id in apps {
            outcome.apps_enqueued += u64::from(self.enqueue_app_deletion(app_id).await?);
        }
        Ok(outcome)
    }

    /// Deletes `transactions` rows older than `retention_days` in bounded
    /// batches, keeping the newest marker row for every app. The startup and
    /// sync watermarks read that row, while every other reader only needs rows
    /// since the last daily-metrics run.
    ///
    /// # Errors
    ///
    /// Returns database or pool failures.
    pub async fn prune_transactions(
        &self,
        now: DateTime<Utc>,
        retention_days: i64,
        batch_size: i64,
    ) -> Result<u64, CustodianError> {
        let cutoff = now - ChronoDuration::days(retention_days.max(1));
        let batch_size = batch_size.clamp(1, 10_000);
        let mut deleted_total = 0_u64;
        loop {
            let client = self.client.acquire().await?;
            let deleted = client
                .execute(
                    "DELETE FROM transactions WHERE ctid IN (
                       SELECT older.ctid FROM transactions older
                       WHERE older.created_at < $1
                         AND older.id < (
                           SELECT max(newest.id) FROM transactions newest
                           WHERE newest.app_id = older.app_id
                         )
                       ORDER BY older.created_at
                       LIMIT $2
                       FOR UPDATE SKIP LOCKED)",
                    &[&cutoff, &batch_size],
                )
                .await?;
            deleted_total += deleted;
            if deleted == 0 {
                return Ok(deleted_total);
            }
        }
    }

    /// Returns the worst active logical replication lag in bytes.
    ///
    /// Inactive slots are only counted while their machine still heartbeats, so
    /// a slot orphaned by a dead process cannot pause destructive work forever.
    ///
    /// # Errors
    ///
    /// Returns a database error when slot state cannot be read.
    pub async fn max_replication_lag_bytes(&self) -> Result<i64, CustodianError> {
        let client = self.client.acquire().await?;
        let grace = i64::try_from(MACHINE_HEALTH_GRACE.as_secs()).unwrap_or(i64::MAX);
        Ok(client
            .query_one(
                "SELECT coalesce(max(pg_wal_lsn_diff(pg_current_wal_lsn(), slot.confirmed_flush_lsn)), 0)::bigint
                 FROM pg_replication_slots AS slot
                 LEFT JOIN rust_realtime_machines AS machine
                   ON machine.machine_id = CASE
                        WHEN pg_input_is_valid(
                               regexp_replace(slot.slot_name, '^invalidator_', ''), 'uuid')
                          THEN regexp_replace(slot.slot_name, '^invalidator_', '')::uuid
                      END
                 WHERE slot.confirmed_flush_lsn IS NOT NULL
                   AND (slot.active
                        OR slot.slot_name NOT LIKE 'invalidator\\_%' ESCAPE '\\'
                        OR machine.updated_at > now() - $1::bigint * interval '1 second')",
                &[&grace],
            )
            .await?
            .get(0))
    }

    /// Claims and completely drains one runnable step, unless WAL consumers are
    /// over the supplied backpressure threshold.
    ///
    /// # Errors
    ///
    /// Returns failures that cannot be recorded on the claimed step.
    pub async fn tick(
        &self,
        max_replication_lag_bytes: i64,
    ) -> Result<CustodianTickOutcome, CustodianError> {
        let mut config = self.defaults;
        config.max_replication_lag_bytes = max_replication_lag_bytes.max(0);
        self.tick_with_config(config).await
    }

    /// Claims and drains one runnable step with a previously resolved live configuration.
    /// Replication and failover capacity are rechecked between every committed delete batch.
    ///
    /// # Errors
    ///
    /// Returns failures that cannot be recorded on the claimed step.
    pub async fn tick_with_config(
        &self,
        config: CustodianRuntimeConfig,
    ) -> Result<CustodianTickOutcome, CustodianError> {
        if let Some(outcome) = self
            .capacity_pause(config.max_replication_lag_bytes)
            .await?
        {
            return Ok(outcome);
        }
        let Some(step) = self.claim().await? else {
            return Ok(CustodianTickOutcome::Idle);
        };
        match self.process(&step, config).await {
            Ok(ProcessResult::Completed) => Ok(CustodianTickOutcome::CompletedStep {
                step_id: step.id,
                kind: step.kind,
            }),
            Ok(ProcessResult::AbortedRestored) => Ok(CustodianTickOutcome::AbortedRestoredPlan {
                app_id: step.app_id,
                attr_id: step.attr_id,
            }),
            Ok(ProcessResult::Paused(outcome)) => {
                self.release(step.id).await?;
                Ok(outcome)
            }
            Err(CustodianError::FailoverGate(error)) => {
                self.release(step.id).await?;
                Err(CustodianError::FailoverGate(error))
            }
            Err(error) => {
                self.fail(step.id, &error.to_string(), config.max_attempts)
                    .await?;
                Ok(CustodianTickOutcome::FailedAttempt { step_id: step.id })
            }
        }
    }

    /// Reclaims expired two-minute leases.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn reap_stuck(&self, now: DateTime<Utc>) -> Result<Vec<Uuid>, CustodianError> {
        self.reap_stuck_with_timeout(now, self.defaults.lease_timeout)
            .await
    }

    /// Reclaims leases older than the live configured timeout.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn reap_stuck_with_timeout(
        &self,
        now: DateTime<Utc>,
        lease_timeout: Duration,
    ) -> Result<Vec<Uuid>, CustodianError> {
        let stale = now
            - ChronoDuration::seconds(i64::try_from(lease_timeout.as_secs()).unwrap_or(i64::MAX));
        let client = self.client.acquire().await?;
        Ok(client
            .query(
                "UPDATE custodian SET worker_id = NULL, status = 'waiting'
                 WHERE status = 'working' AND worker_id IS NOT NULL AND updated_at < $1
                 RETURNING id",
                &[&stale],
            )
            .await?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    /// Deletes failed steps older than one day and every transitive dependent,
    /// preventing an incomplete chain from becoming runnable.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn reap_failed(&self, now: DateTime<Utc>) -> Result<Vec<Uuid>, CustodianError> {
        self.reap_failed_with_retention(now, self.defaults.failed_retention)
            .await
    }

    /// Deletes failed dependency chains older than the live configured retention.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn reap_failed_with_retention(
        &self,
        now: DateTime<Utc>,
        failed_retention: Duration,
    ) -> Result<Vec<Uuid>, CustodianError> {
        let stale = now
            - ChronoDuration::seconds(
                i64::try_from(failed_retention.as_secs()).unwrap_or(i64::MAX),
            );
        let client = self.client.acquire().await?;
        Ok(client
            .query(
                "WITH RECURSIVE plan(id) AS (
                   SELECT id FROM custodian WHERE status = 'failed' AND updated_at < $1
                   UNION
                   SELECT c.id FROM custodian c JOIN plan p ON c.depends_on = p.id
                 )
                 DELETE FROM custodian WHERE id IN (SELECT id FROM plan) RETURNING id",
                &[&stale],
            )
            .await?
            .into_iter()
            .map(|row| row.get(0))
            .collect())
    }

    async fn claim(&self) -> Result<Option<CustodianStep>, CustodianError> {
        let client = self.client.acquire().await?;
        Ok(client
            .query_opt(
                "WITH claimed AS (
                   SELECT id FROM custodian
                   WHERE status = 'waiting' AND depends_on IS NULL AND worker_id IS NULL
                   ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED
                 )
                 UPDATE custodian c SET worker_id = $1, status = 'working'
                 FROM claimed WHERE c.id = claimed.id
                 RETURNING c.id, c.app_id, c.attr_id, c.type",
                &[&self.worker_id],
            )
            .await?
            .map(|row| CustodianStep {
                id: row.get(0),
                app_id: row.get(1),
                attr_id: row.get(2),
                kind: row.get(3),
            }))
    }

    #[tracing::instrument(
        name = "custodian/process-row",
        skip_all,
        err,
        fields(
            custodian.id = %step.id,
            custodian.type = %step.kind,
            app.id = %step.app_id,
            attr.id = ?step.attr_id,
        )
    )]
    async fn process(
        &self,
        step: &CustodianStep,
        config: CustodianRuntimeConfig,
    ) -> Result<ProcessResult, CustodianError> {
        match step.kind.as_str() {
            "triples" => {
                self.drain(step, DrainTable::Triples, config.batch_size, config)
                    .await
            }
            "transactions" => {
                self.drain(step, DrainTable::Transactions, config.batch_size, config)
                    .await
            }
            "attrs" => {
                self.drain(step, DrainTable::Attrs, config.attr_batch_size, config)
                    .await
            }
            "attr" => self.delete_terminal_attr(step).await,
            "app" => self.delete_terminal_app(step).await,
            other => Err(CustodianError::UnknownStep(other.to_owned())),
        }
    }

    async fn drain(
        &self,
        step: &CustodianStep,
        table: DrainTable,
        limit: i64,
        config: CustodianRuntimeConfig,
    ) -> Result<ProcessResult, CustodianError> {
        loop {
            if let Some(outcome) = self
                .capacity_pause(config.max_replication_lag_bytes)
                .await?
            {
                return Ok(ProcessResult::Paused(outcome));
            }
            match self.delete_batch(step, table, limit).await? {
                DeleteBatch::Deleted(0) => {
                    self.finish(step.id).await?;
                    return Ok(ProcessResult::Completed);
                }
                DeleteBatch::Deleted(_) => {}
                DeleteBatch::Restored => {
                    self.abort_plan(step).await?;
                    return Ok(ProcessResult::AbortedRestored);
                }
            }
        }
    }

    async fn delete_batch(
        &self,
        step: &CustodianStep,
        table: DrainTable,
        limit: i64,
    ) -> Result<DeleteBatch, CustodianError> {
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        assert_owned(&transaction, step.id, &self.worker_id).await?;
        if !marked_for_deletion(&transaction, step).await? {
            transaction.rollback().await?;
            return Ok(DeleteBatch::Restored);
        }
        let (query, params): (&str, Vec<&(dyn tokio_postgres::types::ToSql + Sync)>) = match table {
            DrainTable::Triples if step.attr_id.is_some() => (
                "DELETE FROM triples WHERE ctid IN (
                   SELECT ctid FROM triples WHERE app_id = $1 AND attr_id = $2
                   ORDER BY attr_id, created_at LIMIT $3 FOR UPDATE)",
                vec![&step.app_id, step.attr_id.as_ref().unwrap(), &limit],
            ),
            DrainTable::Triples => (
                "DELETE FROM triples WHERE ctid IN (
                   SELECT ctid FROM triples WHERE app_id = $1
                   ORDER BY attr_id, created_at LIMIT $2 FOR UPDATE)",
                vec![&step.app_id, &limit],
            ),
            DrainTable::Transactions => (
                "DELETE FROM transactions WHERE ctid IN (
                   SELECT ctid FROM transactions WHERE app_id = $1
                   ORDER BY id LIMIT $2 FOR UPDATE)",
                vec![&step.app_id, &limit],
            ),
            DrainTable::Attrs => (
                "DELETE FROM attrs WHERE ctid IN (
                   SELECT ctid FROM attrs WHERE app_id = $1 ORDER BY id LIMIT $2 FOR UPDATE)",
                vec![&step.app_id, &limit],
            ),
        };
        let deleted = transaction.execute(query, &params).await?;
        transaction
            .execute(
                "UPDATE custodian SET updated_at = now()
                 WHERE id = $1 AND worker_id = $2 AND status = 'working'",
                &[&step.id, &self.worker_id],
            )
            .await?;
        transaction.commit().await?;
        Ok(DeleteBatch::Deleted(deleted))
    }

    async fn delete_terminal_attr(
        &self,
        step: &CustodianStep,
    ) -> Result<ProcessResult, CustodianError> {
        let Some(attr_id) = step.attr_id else {
            return Err(CustodianError::UnknownStep(
                "attr without attr_id".to_owned(),
            ));
        };
        let client = self.client.acquire().await?;
        let deleted = client
            .execute(
                "DELETE FROM attrs WHERE app_id = $1 AND id = $2
                 AND deletion_marked_at IS NOT NULL",
                &[&step.app_id, &attr_id],
            )
            .await?;
        drop(client);
        if deleted == 0 {
            self.abort_plan(step).await?;
            Ok(ProcessResult::AbortedRestored)
        } else {
            self.finish(step.id).await?;
            Ok(ProcessResult::Completed)
        }
    }

    async fn delete_terminal_app(
        &self,
        step: &CustodianStep,
    ) -> Result<ProcessResult, CustodianError> {
        let client = self.client.acquire().await?;
        let deleted = client
            .execute(
                "DELETE FROM apps WHERE id = $1 AND deletion_marked_at IS NOT NULL",
                &[&step.app_id],
            )
            .await?;
        drop(client);
        if deleted == 0 {
            self.abort_plan(step).await?;
            Ok(ProcessResult::AbortedRestored)
        } else {
            self.finish(step.id).await?;
            Ok(ProcessResult::Completed)
        }
    }

    async fn finish(&self, id: Uuid) -> Result<(), CustodianError> {
        let client = self.client.acquire().await?;
        client
            .execute(
                "DELETE FROM custodian WHERE id = $1 AND worker_id = $2",
                &[&id, &self.worker_id],
            )
            .await?;
        Ok(())
    }

    async fn abort_plan(&self, step: &CustodianStep) -> Result<(), CustodianError> {
        let client = self.client.acquire().await?;
        if let Some(attr_id) = step.attr_id {
            client
                .execute(
                    "DELETE FROM custodian WHERE app_id = $1 AND attr_id = $2",
                    &[&step.app_id, &attr_id],
                )
                .await?;
        } else {
            client
                .execute(
                    "DELETE FROM custodian WHERE app_id = $1 AND attr_id IS NULL",
                    &[&step.app_id],
                )
                .await?;
        }
        Ok(())
    }

    async fn fail(&self, id: Uuid, error: &str, max_attempts: i32) -> Result<(), CustodianError> {
        let client = self.client.acquire().await?;
        client
            .execute(
                "UPDATE custodian
                 SET attempts = attempts + 1, error = $3,
                     worker_id = CASE WHEN attempts + 1 >= $4 THEN worker_id ELSE NULL END,
                     status = CASE WHEN attempts + 1 >= $4 THEN 'failed' ELSE 'waiting' END
                 WHERE id = $1 AND worker_id = $2",
                &[&id, &self.worker_id, &error, &max_attempts],
            )
            .await?;
        Ok(())
    }

    async fn release(&self, id: Uuid) -> Result<(), CustodianError> {
        let client = self.client.acquire().await?;
        client
            .execute(
                "UPDATE custodian SET worker_id = NULL, status = 'waiting'
                 WHERE id = $1 AND worker_id = $2 AND status = 'working'",
                &[&id, &self.worker_id],
            )
            .await?;
        Ok(())
    }

    async fn writes_paused(&self) -> Result<bool, CustodianError> {
        let Some(gate) = &self.failover_gate else {
            return Ok(false);
        };
        if gate.is_failing_over().await? {
            return Ok(true);
        }
        Ok(gate.is_toggled("custodian-disabled?").await?)
    }

    async fn capacity_pause(
        &self,
        max_replication_lag_bytes: i64,
    ) -> Result<Option<CustodianTickOutcome>, CustodianError> {
        if self.writes_paused().await? {
            return Ok(Some(CustodianTickOutcome::FailoverPaused));
        }
        let lag = self.max_replication_lag_bytes().await?;
        Ok((lag > max_replication_lag_bytes)
            .then_some(CustodianTickOutcome::Paused { lag_bytes: lag }))
    }
}

fn positive_i64(
    values: &HashMap<String, Value>,
    setting: &'static str,
    default: i64,
) -> Result<i64, CustodianError> {
    let Some(value) = values.get(setting) else {
        return Ok(default);
    };
    value
        .as_i64()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_flag(values, setting))
}

fn non_negative_i64(
    values: &HashMap<String, Value>,
    setting: &'static str,
    default: i64,
) -> Result<i64, CustodianError> {
    let Some(value) = values.get(setting) else {
        return Ok(default);
    };
    value
        .as_i64()
        .filter(|value| *value >= 0)
        .ok_or_else(|| invalid_flag(values, setting))
}

fn positive_u64(
    values: &HashMap<String, Value>,
    setting: &'static str,
    default: u64,
) -> Result<u64, CustodianError> {
    let Some(value) = values.get(setting) else {
        return Ok(default);
    };
    value
        .as_u64()
        .filter(|value| *value > 0 && i64::try_from(*value).is_ok())
        .ok_or_else(|| invalid_flag(values, setting))
}

fn non_negative_u64(
    values: &HashMap<String, Value>,
    setting: &'static str,
    default: u64,
) -> Result<u64, CustodianError> {
    let Some(value) = values.get(setting) else {
        return Ok(default);
    };
    value
        .as_u64()
        .filter(|value| i64::try_from(*value).is_ok())
        .ok_or_else(|| invalid_flag(values, setting))
}

fn invalid_flag(values: &HashMap<String, Value>, setting: &'static str) -> CustodianError {
    CustodianError::InvalidConfigFlag {
        setting,
        value: values.get(setting).cloned().unwrap_or(Value::Null),
    }
}

async fn assert_owned(
    transaction: &Transaction<'_>,
    id: Uuid,
    worker_id: &str,
) -> Result<(), CustodianError> {
    if transaction
        .query_opt(
            "SELECT 1 FROM custodian
             WHERE id = $1 AND worker_id = $2 AND status = 'working' FOR UPDATE",
            &[&id, &worker_id],
        )
        .await?
        .is_none()
    {
        return Err(CustodianError::OwnershipLost(id));
    }
    Ok(())
}

async fn marked_for_deletion(
    transaction: &Transaction<'_>,
    step: &CustodianStep,
) -> Result<bool, CustodianError> {
    let present = if let Some(attr_id) = step.attr_id {
        transaction
            .query_opt(
                "SELECT 1 FROM attrs WHERE id = $1 AND deletion_marked_at IS NOT NULL",
                &[&attr_id],
            )
            .await?
    } else {
        transaction
            .query_opt(
                "SELECT 1 FROM apps WHERE id = $1 AND deletion_marked_at IS NOT NULL",
                &[&step.app_id],
            )
            .await?
    };
    Ok(present.is_some())
}

struct CustodianStep {
    id: Uuid,
    app_id: Uuid,
    attr_id: Option<Uuid>,
    kind: String,
}

#[derive(Clone, Copy)]
enum DrainTable {
    Triples,
    Transactions,
    Attrs,
}

enum DeleteBatch {
    Deleted(u64),
    Restored,
}

enum ProcessResult {
    Completed,
    AbortedRestored,
    Paused(CustodianTickOutcome),
}

/// Reads `INSTANT_TRANSACTION_RETENTION_DAYS`; invalid or missing values keep
/// the 30-day default.
#[must_use]
pub fn transaction_retention_days_from_env() -> i64 {
    parse_transaction_retention_days(
        std::env::var(TRANSACTION_RETENTION_DAYS_ENV)
            .ok()
            .as_deref(),
    )
}

fn parse_transaction_retention_days(value: Option<&str>) -> i64 {
    value
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|days| *days >= 1)
        .unwrap_or(DEFAULT_TRANSACTION_RETENTION_DAYS)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn defaults_match_the_reference_worker() {
        assert_eq!(DEFAULT_BATCH_SIZE, 1_000);
        assert_eq!(DEFAULT_MAX_ATTEMPTS, 3);
        let hosted = CustodianRuntimeConfig::for_environment(ServerEnvironment::Production);
        assert_eq!(hosted.idle_sleep(), Duration::from_secs(5));
        assert_eq!(hosted.lease_timeout(), Duration::from_secs(120));
        assert_eq!(hosted.failed_retention(), Duration::from_hours(24));
        assert_eq!(
            CustodianRuntimeConfig::for_environment(ServerEnvironment::Development).idle_sleep(),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn transaction_retention_defaults_to_thirty_days() {
        assert_eq!(parse_transaction_retention_days(None), 30);
        assert_eq!(parse_transaction_retention_days(Some(" 90 ")), 90);
        assert_eq!(parse_transaction_retention_days(Some("0")), 30);
        assert_eq!(parse_transaction_retention_days(Some("bad")), 30);
    }

    #[test]
    fn live_runtime_flags_replace_every_reference_tuning_value() {
        let values = HashMap::from([
            ("custodian-batch-size".to_owned(), json!(250)),
            ("custodian-idle-sleep-ms".to_owned(), json!(75)),
            ("custodian-attr-batch-size".to_owned(), json!(4)),
            ("custodian-lease-timeout-seconds".to_owned(), json!(30)),
            ("custodian-max-attempts".to_owned(), json!(7)),
            ("custodian-failed-retention-seconds".to_owned(), json!(3600)),
            (
                "custodian-max-replication-lag-bytes".to_owned(),
                json!(4096),
            ),
        ]);

        let config = CustodianRuntimeConfig::for_environment(ServerEnvironment::Production)
            .apply_values(&values)
            .unwrap();

        assert_eq!(config.batch_size, 250);
        assert_eq!(config.idle_sleep, Duration::from_millis(75));
        assert_eq!(config.attr_batch_size, 4);
        assert_eq!(config.lease_timeout, Duration::from_secs(30));
        assert_eq!(config.max_attempts, 7);
        assert_eq!(config.failed_retention, Duration::from_secs(3600));
        assert_eq!(config.max_replication_lag_bytes, 4096);
    }

    #[test]
    fn malformed_runtime_flags_fail_closed_without_replacing_defaults() {
        for (setting, value) in [
            ("custodian-batch-size", json!(0)),
            ("custodian-idle-sleep-ms", json!(-1)),
            ("custodian-attr-batch-size", json!("many")),
            ("custodian-lease-timeout-seconds", json!(0)),
            ("custodian-max-attempts", json!(i64::MAX)),
            ("custodian-failed-retention-seconds", json!(null)),
            ("custodian-max-replication-lag-bytes", json!(-1)),
        ] {
            let values = HashMap::from([(setting.to_owned(), value.clone())]);
            assert!(matches!(
                CustodianRuntimeConfig::for_environment(ServerEnvironment::Production)
                    .apply_values(&values),
                Err(CustodianError::InvalidConfigFlag {
                    setting: actual,
                    value: actual_value,
                }) if actual == setting && actual_value == value
            ));
        }
    }
}
