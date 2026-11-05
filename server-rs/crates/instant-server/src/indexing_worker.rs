use std::sync::Arc;
use std::time::{Duration, Instant};

use instant_http::ApiError;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Json;
use tokio_postgres::{Client, Row};
use uuid::Uuid;

use crate::dashboard_control::{DashboardControlHookOperation, DashboardControlHooks};
use crate::postgres_pool::PostgresClientSource;
use crate::{PostgresPool, PostgresPoolError};

const BATCH_SIZE: i64 = 1_000;
const BATCH_SIZE_U64: u64 = 1_000;
const NULL_JSON_MD5: &str = "37a6259cc0c1dae299a7866489dff0bd";
const INVALID_TRIPLE: &str = "invalid-triple-error";
const TRIPLE_TOO_LARGE: &str = "triple-too-large-error";
const TRIPLE_NOT_UNIQUE: &str = "triple-not-unique-error";
const MISSING_REQUIRED: &str = "missing-required-error";
const INVALID_ATTR_STATE: &str = "invalid-attr-state-error";
const UNEXPECTED: &str = "unexpected-error";
/// A processing row untouched for this long belongs to a dead worker.
const STUCK_JOB_LEASE_SECONDS: i64 = 600;
/// The poll loop reclaims abandoned jobs at most once a minute.
const RECLAIM_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndexingJobOutcome {
    NotClaimed,
    Completed,
    Cancelled,
    Errored { error: String },
}

#[derive(Debug, Error)]
pub enum IndexingWorkerError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
    #[error("invalid indexing job: {0}")]
    InvalidJob(String),
}

/// Native executor for the eight durable schema constraint job types.
pub struct PostgresIndexingWorker {
    client: PostgresClientSource,
    worker_id: String,
    last_reclaim: std::sync::Mutex<Option<Instant>>,
}

impl PostgresIndexingWorker {
    #[must_use]
    pub fn new(client: Client, worker_id: impl Into<String>) -> Self {
        Self::from_source(PostgresClientSource::fixed(client), worker_id)
    }

    #[must_use]
    pub fn with_shared_client(client: Arc<Mutex<Client>>, worker_id: impl Into<String>) -> Self {
        Self::from_source(PostgresClientSource::fixed_shared(client), worker_id)
    }

    /// Builds the production worker over a bounded reconnecting pool.
    #[must_use]
    pub fn from_pool(client: PostgresPool, worker_id: impl Into<String>) -> Self {
        Self::from_source(PostgresClientSource::pooled(client), worker_id)
    }

    fn from_source(client: PostgresClientSource, worker_id: impl Into<String>) -> Self {
        Self {
            client,
            worker_id: worker_id.into(),
            last_reclaim: std::sync::Mutex::new(None),
        }
    }

    /// Runs stage-sized units until the job reaches a terminal state or is no
    /// longer claimable (for example, while waiting on a dependency).
    ///
    /// # Errors
    ///
    /// Returns database failures that prevent claiming or persisting the job's
    /// own terminal error state.
    #[tracing::instrument(name = "indexing-jobs/handle-process", skip_all, fields(job.id = %job_id))]
    pub async fn run_job(&self, job_id: Uuid) -> Result<IndexingJobOutcome, IndexingWorkerError> {
        loop {
            let job = match self.claim(job_id).await {
                Ok(Some(job)) => job,
                Ok(None) => return self.current_outcome(job_id).await,
                Err(IndexingWorkerError::Database(error)) if is_unique_violation(&error) => {
                    return Ok(IndexingJobOutcome::NotClaimed);
                }
                Err(error) => return Err(error),
            };
            let result = self.run_stage(&job).await;
            match result {
                Ok(StageResult::Repeat) => {
                    if !self.release(&job).await? {
                        return self.current_outcome(job_id).await;
                    }
                }
                Ok(StageResult::Done) => {
                    let Some(next) = next_stage(&job.job_type, &job.job_stage)? else {
                        if self.complete(&job).await? {
                            return Ok(IndexingJobOutcome::Completed);
                        }
                        return self.current_outcome(job_id).await;
                    };
                    if !self.advance_and_release(&job, next).await? {
                        return self.current_outcome(job_id).await;
                    }
                }
                Ok(StageResult::Failed(failure)) => {
                    self.mark_failed(&job, &failure).await?;
                    return Ok(IndexingJobOutcome::Errored {
                        error: failure.error.to_owned(),
                    });
                }
                Err(error) => {
                    let failure = JobFailure {
                        error: UNEXPECTED,
                        detail: Some(error.to_string()),
                        invalid_unique_value: None,
                        invalid_entity_id: None,
                        error_data: None,
                    };
                    self.mark_failed(&job, &failure).await?;
                    return Ok(IndexingJobOutcome::Errored {
                        error: UNEXPECTED.to_owned(),
                    });
                }
            }
        }
    }

    /// Finds and runs claimable jobs whose original queue notification was
    /// lost. Claims remain race-safe when several processes sweep together.
    /// Abandoned `processing` rows past the lease are returned to `waiting`
    /// first, at most once a minute.
    ///
    /// # Errors
    ///
    /// Returns a database error when the availability scan or a claimed job
    /// cannot persist its state transition.
    #[tracing::instrument(name = "indexing-jobs/grab-forgotten-jobs!", skip_all)]
    pub async fn run_available_jobs(
        &self,
        limit: i64,
    ) -> Result<Vec<(Uuid, IndexingJobOutcome)>, IndexingWorkerError> {
        self.maybe_reclaim_stuck_jobs().await?;
        let job_ids = {
            let client = self.client.acquire().await?;
            let job_types = JOB_TYPES.as_slice();
            client
                .query(
                    "SELECT j.id FROM indexing_jobs j
                     WHERE j.worker_id IS NULL
                       AND j.job_type = ANY($1)
                       AND j.job_status IN ('waiting', 'processing')
                       AND (j.job_dependency IS NULL OR EXISTS (
                         SELECT 1 FROM indexing_jobs dep
                         WHERE dep.id = j.job_dependency AND dep.job_status = 'completed'
                       ))
                     ORDER BY j.created_at, j.id
                     LIMIT $2",
                    &[&job_types, &limit.clamp(1, 1_000)],
                )
                .await?
                .into_iter()
                .map(|row| row.get::<_, Uuid>(0))
                .collect::<Vec<_>>()
        };
        let mut outcomes = Vec::with_capacity(job_ids.len());
        for job_id in job_ids {
            outcomes.push((job_id, self.run_job(job_id).await?));
        }
        Ok(outcomes)
    }

    /// Returns jobs abandoned by a dead worker to the pending queue, at most
    /// once per [`RECLAIM_INTERVAL`]. A live worker refreshes `updated_at` as it
    /// advances stages, so only rows older than the whole lease are reclaimed.
    async fn maybe_reclaim_stuck_jobs(&self) -> Result<(), IndexingWorkerError> {
        {
            let mut last = self
                .last_reclaim
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.is_some_and(|at| at.elapsed() < RECLAIM_INTERVAL) {
                return Ok(());
            }
            *last = Some(Instant::now());
        }
        let client = self.client.acquire().await?;
        let job_types = JOB_TYPES.as_slice();
        let reclaimed = client
            .execute(
                "UPDATE indexing_jobs
                 SET worker_id = NULL, job_status = 'waiting'
                 WHERE job_status = 'processing'
                   AND worker_id IS NOT NULL
                   AND job_type = ANY($1)
                   AND updated_at < now() - $2::bigint * interval '1 second'",
                &[&job_types, &STUCK_JOB_LEASE_SECONDS],
            )
            .await?;
        if reclaimed > 0 {
            tracing::warn!(
                reclaimed,
                lease_seconds = STUCK_JOB_LEASE_SECONDS,
                "returned stuck indexing jobs to the pending queue"
            );
        }
        Ok(())
    }

    async fn claim(&self, job_id: Uuid) -> Result<Option<IndexingJob>, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let job_types = JOB_TYPES.as_slice();
        let row = client
            .query_opt(
                "UPDATE indexing_jobs j
                 SET worker_id = $2, job_status = 'processing'
                 WHERE j.id = $1
                   AND j.worker_id IS NULL
                   AND j.job_type = ANY($3)
                   AND j.job_status IN ('waiting', 'processing')
                   AND (j.job_dependency IS NULL OR EXISTS (
                     SELECT 1 FROM indexing_jobs dep
                     WHERE dep.id = j.job_dependency AND dep.job_status = 'completed'
                   ))
                   AND NOT EXISTS (
                     SELECT 1 FROM indexing_jobs active
                     WHERE active.id <> j.id
                       AND active.app_id = j.app_id
                       AND active.attr_id = j.attr_id
                       AND active.job_serial_key = j.job_serial_key
                       AND active.job_status = 'processing'
                       AND active.worker_id IS NOT NULL
                   )
                 RETURNING j.id, j.app_id, j.attr_id, j.job_type, j.job_status,
                           j.job_stage, j.checked_data_type::text",
                &[&job_id, &self.worker_id, &job_types],
            )
            .await?;
        Ok(row.as_ref().map(IndexingJob::from_row))
    }

    async fn current_outcome(
        &self,
        job_id: Uuid,
    ) -> Result<IndexingJobOutcome, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let row = client
            .query_opt(
                "SELECT job_status, error FROM indexing_jobs WHERE id = $1",
                &[&job_id],
            )
            .await?;
        Ok(match row {
            None => IndexingJobOutcome::Cancelled,
            Some(row) => match row.get::<_, String>(0).as_str() {
                "completed" => IndexingJobOutcome::Completed,
                "canceled" | "cancelled" => IndexingJobOutcome::Cancelled,
                "errored" => IndexingJobOutcome::Errored {
                    error: row
                        .get::<_, Option<String>>(1)
                        .unwrap_or_else(|| UNEXPECTED.to_owned()),
                },
                _ => IndexingJobOutcome::NotClaimed,
            },
        })
    }

    #[tracing::instrument(
        name = "indexing-jobs/run-next-stage",
        skip_all,
        fields(job.id = %job.id, job.type = %job.job_type, job.stage = %job.job_stage)
    )]
    async fn run_stage(&self, job: &IndexingJob) -> Result<StageResult, IndexingWorkerError> {
        match (job.job_type.as_str(), job.job_stage.as_str()) {
            ("check-data-type", "estimate-work") => self.estimate(job, 3).await,
            ("check-data-type", "validate" | "revalidate") => self.validate_data_type(job).await,
            ("check-data-type", "update-attr-start") => {
                self.mutate_attr(job, AttrMutation::CheckTypeStart).await
            }
            ("check-data-type", "update-triples") => self.set_checked_type_batch(job).await,
            ("check-data-type", "update-attr-done") => {
                self.mutate_attr(job, AttrMutation::CheckTypeDone).await
            }
            ("remove-data-type", "update-attr-start") => {
                self.mutate_attr(job, AttrMutation::RemoveTypeStart).await
            }
            (
                "remove-data-type" | "index" | "remove-index" | "unique" | "remove-unique",
                "estimate-work",
            ) => self.estimate(job, 1).await,
            ("remove-data-type", "update-triples") => self.clear_checked_type_batch(job).await,
            ("remove-data-type", "update-attr-done") => {
                self.mutate_attr(job, AttrMutation::RemoveTypeDone).await
            }
            ("index", "update-attr-start") => self.mutate_attr(job, AttrMutation::IndexStart).await,
            ("index", "estimate-work-with-undefineds") => {
                self.estimate_missing_index_values(job).await
            }
            ("index", "update-triples") => self.set_flag_batch(job, "ave", true).await,
            ("index", "insert-nulls") => self.insert_index_nulls(job).await,
            ("index", "update-attr-done") => self.mutate_attr(job, AttrMutation::IndexDone).await,
            ("remove-index", "update-attr-start") => {
                self.mutate_attr(job, AttrMutation::RemoveIndexStart).await
            }
            ("remove-index", "update-triples") => self.set_flag_batch(job, "ave", false).await,
            ("remove-index", "update-attr-done") => {
                self.mutate_attr(job, AttrMutation::RemoveIndexDone).await
            }
            ("unique", "update-attr-start") => {
                self.mutate_attr(job, AttrMutation::UniqueStart).await
            }
            ("unique", "update-triples") => self.set_flag_batch(job, "av", true).await,
            ("unique", "update-attr-done") => self.mutate_attr(job, AttrMutation::UniqueDone).await,
            ("remove-unique", "update-attr-start") => {
                self.mutate_attr(job, AttrMutation::RemoveUniqueStart).await
            }
            ("remove-unique", "update-triples") => self.set_flag_batch(job, "av", false).await,
            ("remove-unique", "update-attr-done") => {
                self.mutate_attr(job, AttrMutation::RemoveUniqueDone).await
            }
            ("required", "estimate-work") => self.estimate_required(job).await,
            ("required", "validate" | "revalidate") => self.validate_required(job).await,
            ("required", "update-attr") => {
                self.mutate_attr(job, AttrMutation::SetRequired(true)).await
            }
            ("remove-required", "update-attr") => {
                self.mutate_attr(job, AttrMutation::SetRequired(false))
                    .await
            }
            _ => Err(IndexingWorkerError::InvalidJob(format!(
                "unknown job stage {} {}",
                job.job_type, job.job_stage
            ))),
        }
    }

    async fn estimate(
        &self,
        job: &IndexingJob,
        multiplier: i64,
    ) -> Result<StageResult, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let total = client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2",
                &[&job.app_id, &job.attr_id],
            )
            .await?
            .get::<_, i64>(0)
            .max(0)
            .saturating_mul(multiplier);
        let estimate = five_percent_headroom(total);
        client
            .execute(
                "UPDATE indexing_jobs SET work_estimate = $3
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &self.worker_id, &estimate],
            )
            .await?;
        Ok(StageResult::Done)
    }

    async fn estimate_required(
        &self,
        job: &IndexingJob,
    ) -> Result<StageResult, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let Some(attr) = load_attr(&client, job).await? else {
            return Ok(StageResult::invalid_attr());
        };
        let Some(id_attr) = id_attr_id(&client, job.app_id, &attr.etype).await? else {
            return Ok(StageResult::Failed(JobFailure::missing_required(json!({
                "attr-id": job.attr_id,
                "etype": attr.etype,
            }))));
        };
        let entities = client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2",
                &[&job.app_id, &id_attr],
            )
            .await?
            .get::<_, i64>(0)
            .max(1);
        let estimate = five_percent_headroom(entities.saturating_mul(2));
        client
            .execute(
                "UPDATE indexing_jobs SET work_estimate = $3
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &self.worker_id, &estimate],
            )
            .await?;
        Ok(StageResult::Done)
    }

    async fn estimate_missing_index_values(
        &self,
        job: &IndexingJob,
    ) -> Result<StageResult, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let Some(attr) = load_attr(&client, job).await? else {
            return Ok(StageResult::invalid_attr());
        };
        if attr.value_type == "blob"
            && let Some(id_attr) = id_attr_id(&client, job.app_id, &attr.etype).await?
        {
            let entities = client
                .query_one(
                    "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2",
                    &[&job.app_id, &id_attr],
                )
                .await?
                .get::<_, i64>(0)
                .max(1);
            let estimate = clamp_i32(entities);
            client
                .execute(
                    "UPDATE indexing_jobs
                     SET work_estimate = coalesce(work_estimate, 0) + $3
                     WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                    &[&job.id, &self.worker_id, &estimate],
                )
                .await?;
        }
        Ok(StageResult::Done)
    }

    async fn validate_data_type(
        &self,
        job: &IndexingJob,
    ) -> Result<StageResult, IndexingWorkerError> {
        let target = job.checked_data_type.as_deref().ok_or_else(|| {
            IndexingWorkerError::InvalidJob("check-data-type job has no target type".to_owned())
        })?;
        let client = self.client.acquire().await?;
        let invalid = client
            .query_opt(
                "SELECT entity_id FROM triples
                 WHERE app_id = $1 AND attr_id = $2
                   AND NOT triples_valid_value($3::text::checked_data_type, value)
                 LIMIT 1",
                &[&job.app_id, &job.attr_id, &target],
            )
            .await?
            .is_some();
        let completed = client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2",
                &[&job.app_id, &job.attr_id],
            )
            .await?
            .get::<_, i64>(0);
        add_work(&client, job, &self.worker_id, completed).await?;
        drop(client);
        if invalid {
            if job.job_stage == "revalidate" {
                let _ = self.mutate_attr(job, AttrMutation::CheckTypeAbort).await?;
            }
            Ok(StageResult::Failed(JobFailure::simple(INVALID_TRIPLE)))
        } else {
            Ok(StageResult::Done)
        }
    }

    async fn set_checked_type_batch(
        &self,
        job: &IndexingJob,
    ) -> Result<StageResult, IndexingWorkerError> {
        let target = job.checked_data_type.as_deref().ok_or_else(|| {
            IndexingWorkerError::InvalidJob("check-data-type job has no target type".to_owned())
        })?;
        let client = self.client.acquire().await?;
        let updated = client
            .execute(
                "WITH batch AS (
                   SELECT ctid FROM triples
                   WHERE app_id = $1 AND attr_id = $2
                     AND checked_data_type IS DISTINCT FROM $3::text::checked_data_type
                   LIMIT $4 FOR UPDATE
                 )
                 UPDATE triples SET checked_data_type = $3::text::checked_data_type
                 WHERE ctid IN (SELECT ctid FROM batch)",
                &[&job.app_id, &job.attr_id, &target, &BATCH_SIZE],
            )
            .await?;
        add_work(&client, job, &self.worker_id, rows_as_i64(updated)).await?;
        Ok(batch_result(updated))
    }

    async fn clear_checked_type_batch(
        &self,
        job: &IndexingJob,
    ) -> Result<StageResult, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let updated = client
            .execute(
                "WITH batch AS (
                   SELECT ctid FROM triples
                   WHERE app_id = $1 AND attr_id = $2 AND checked_data_type IS NOT NULL
                   LIMIT $3 FOR UPDATE
                 )
                 UPDATE triples SET checked_data_type = NULL
                 WHERE ctid IN (SELECT ctid FROM batch)",
                &[&job.app_id, &job.attr_id, &BATCH_SIZE],
            )
            .await?;
        add_work(&client, job, &self.worker_id, rows_as_i64(updated)).await?;
        Ok(batch_result(updated))
    }

    async fn set_flag_batch(
        &self,
        job: &IndexingJob,
        flag: &'static str,
        enabled: bool,
    ) -> Result<StageResult, IndexingWorkerError> {
        let candidate = {
            let client = self.client.acquire().await?;
            let query = format!(
                "SELECT entity_id FROM triples
                 WHERE app_id = $1 AND attr_id = $2 AND {flag} IS DISTINCT FROM $3
                 LIMIT 1"
            );
            client
                .query_opt(&query, &[&job.app_id, &job.attr_id, &enabled])
                .await?
                .map(|row| row.get::<_, Uuid>(0))
        };
        let updated = {
            let client = self.client.acquire().await?;
            let query = format!(
                "WITH batch AS (
                   SELECT ctid FROM triples
                   WHERE app_id = $1 AND attr_id = $2 AND {flag} IS DISTINCT FROM $3
                   LIMIT $4 FOR UPDATE
                 )
                 UPDATE triples SET {flag} = $3 WHERE ctid IN (SELECT ctid FROM batch)"
            );
            client
                .execute(&query, &[&job.app_id, &job.attr_id, &enabled, &BATCH_SIZE])
                .await
        };
        match updated {
            Ok(updated) => {
                let client = self.client.acquire().await?;
                add_work(&client, job, &self.worker_id, rows_as_i64(updated)).await?;
                Ok(batch_result(updated))
            }
            Err(error) if job.job_type == "unique" && is_unique_violation(&error) => {
                self.abort_unique(job).await?;
                let invalid = self.duplicate_value(job).await?;
                Ok(StageResult::Failed(JobFailure {
                    error: TRIPLE_NOT_UNIQUE,
                    detail: Some(error.to_string()),
                    invalid_unique_value: invalid,
                    invalid_entity_id: None,
                    error_data: None,
                }))
            }
            Err(error) if job.job_type == "index" && is_index_value_too_large(&error) => {
                self.abort_index(job).await?;
                Ok(StageResult::Failed(JobFailure {
                    error: TRIPLE_TOO_LARGE,
                    detail: Some(error.to_string()),
                    invalid_unique_value: None,
                    invalid_entity_id: candidate,
                    error_data: None,
                }))
            }
            Err(error) => {
                if job.job_type == "index" {
                    self.abort_index(job).await?;
                } else if job.job_type == "unique" {
                    self.abort_unique(job).await?;
                }
                Err(error.into())
            }
        }
    }

    async fn insert_index_nulls(
        &self,
        job: &IndexingJob,
    ) -> Result<StageResult, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let Some(attr) = load_attr(&client, job).await? else {
            return Ok(StageResult::invalid_attr());
        };
        if attr.value_type != "blob" {
            return Ok(StageResult::Done);
        }
        let Some(id_attr) = id_attr_id(&client, job.app_id, &attr.etype).await? else {
            return Ok(StageResult::invalid_attr());
        };
        let row = client
            .query_one(
                "WITH ids AS (
                   SELECT app_id, entity_id FROM triples ids
                   WHERE app_id = $1 AND attr_id = $3
                     AND NOT EXISTS (
                       SELECT 1 FROM triples present
                       WHERE present.app_id = $1 AND present.attr_id = $2
                         AND present.entity_id = ids.entity_id
                     )
                   LIMIT $4 FOR UPDATE
                 ), inserted AS (
                   INSERT INTO triples
                     (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae,
                      checked_data_type)
                   SELECT ids.app_id, ids.entity_id, a.id, 'null'::jsonb, $5,
                          a.cardinality = 'one', a.value_type = 'ref', a.is_unique,
                          a.is_indexed, a.value_type = 'ref', a.checked_data_type
                   FROM ids JOIN attrs a ON a.id = $2 AND a.app_id = $1
                   RETURNING 1
                 )
                 SELECT (SELECT count(*) FROM ids), (SELECT count(*) FROM inserted)",
                &[
                    &job.app_id,
                    &job.attr_id,
                    &id_attr,
                    &(BATCH_SIZE * 2),
                    &NULL_JSON_MD5,
                ],
            )
            .await?;
        let checked = row.get::<_, i64>(0);
        add_work(&client, job, &self.worker_id, checked).await?;
        Ok(if checked >= BATCH_SIZE * 2 {
            StageResult::Repeat
        } else {
            StageResult::Done
        })
    }

    async fn validate_required(
        &self,
        job: &IndexingJob,
    ) -> Result<StageResult, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let Some(attr) = load_attr(&client, job).await? else {
            return Ok(StageResult::invalid_attr());
        };
        let Some(id_attr) = id_attr_id(&client, job.app_id, &attr.etype).await? else {
            drop(client);
            let _ = self
                .mutate_attr(job, AttrMutation::SetRequired(false))
                .await?;
            return Ok(StageResult::Failed(JobFailure::missing_required(json!({
                "attr-id": job.attr_id,
                "etype": attr.etype,
            }))));
        };
        let missing = client
            .query(
                "SELECT ids.entity_id FROM triples ids
                 WHERE ids.app_id = $1 AND ids.attr_id = $2
                   AND NOT EXISTS (
                     SELECT 1 FROM triples value
                     WHERE value.app_id = $1 AND value.attr_id = $3
                       AND value.entity_id = ids.entity_id AND value.value <> 'null'::jsonb
                   )
                 LIMIT 10",
                &[&job.app_id, &id_attr, &job.attr_id],
            )
            .await?
            .into_iter()
            .map(|row| row.get::<_, Uuid>(0))
            .collect::<Vec<_>>();
        let completed = client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2",
                &[&job.app_id, &id_attr],
            )
            .await?
            .get::<_, i64>(0);
        add_work(&client, job, &self.worker_id, completed).await?;
        if missing.is_empty() {
            return Ok(StageResult::Done);
        }
        let missing_count = client
            .query_one(
                "SELECT count(*) FROM triples ids
                 WHERE ids.app_id = $1 AND ids.attr_id = $2
                   AND NOT EXISTS (
                     SELECT 1 FROM triples value
                     WHERE value.app_id = $1 AND value.attr_id = $3
                       AND value.entity_id = ids.entity_id AND value.value <> 'null'::jsonb
                   )",
                &[&job.app_id, &id_attr, &job.attr_id],
            )
            .await?
            .get::<_, i64>(0);
        drop(client);
        let _ = self
            .mutate_attr(job, AttrMutation::SetRequired(false))
            .await?;
        Ok(StageResult::Failed(JobFailure::missing_required(json!({
            "count": missing_count,
            "etype": attr.etype,
            "label": attr.label,
            "entity-ids": missing,
        }))))
    }

    #[allow(clippy::too_many_lines)]
    async fn mutate_attr(
        &self,
        job: &IndexingJob,
        mutation: AttrMutation,
    ) -> Result<StageResult, IndexingWorkerError> {
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        transaction
            .execute(
                "INSERT INTO transactions (app_id) VALUES ($1)",
                &[&job.app_id],
            )
            .await?;
        let target = job.checked_data_type.as_deref();
        let updated = match mutation {
            AttrMutation::CheckTypeStart => {
                transaction
                    .execute(
                        "UPDATE attrs SET checked_data_type = $3::text::checked_data_type,
                                          checking_data_type = true
                         WHERE app_id = $1 AND id = $2",
                        &[&job.app_id, &job.attr_id, &target],
                    )
                    .await?
            }
            AttrMutation::CheckTypeAbort => {
                transaction
                    .execute(
                        "UPDATE attrs SET checked_data_type = NULL, checking_data_type = false
                         WHERE app_id = $1 AND id = $2
                           AND checked_data_type = $3::text::checked_data_type
                           AND checking_data_type = true",
                        &[&job.app_id, &job.attr_id, &target],
                    )
                    .await?
            }
            AttrMutation::CheckTypeDone => {
                transaction
                    .execute(
                        "UPDATE attrs SET checking_data_type = false
                         WHERE app_id = $1 AND id = $2
                           AND checked_data_type = $3::text::checked_data_type
                           AND checking_data_type = true",
                        &[&job.app_id, &job.attr_id, &target],
                    )
                    .await?
            }
            AttrMutation::RemoveTypeStart => {
                transaction
                    .execute(
                        "UPDATE attrs SET checked_data_type = NULL, checking_data_type = true
                         WHERE app_id = $1 AND id = $2",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::RemoveTypeDone => {
                transaction
                    .execute(
                        "UPDATE attrs SET checking_data_type = false
                         WHERE app_id = $1 AND id = $2
                           AND checked_data_type IS NULL AND checking_data_type = true",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::IndexStart => {
                transaction
                    .execute(
                        "UPDATE attrs SET is_indexed = true, indexing = true
                         WHERE app_id = $1 AND id = $2",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::IndexAbort => {
                transaction
                    .execute(
                        "UPDATE attrs SET is_indexed = false, indexing = false
                         WHERE app_id = $1 AND id = $2 AND is_indexed AND indexing",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::IndexDone => {
                transaction
                    .execute(
                        "UPDATE attrs SET indexing = false
                         WHERE app_id = $1 AND id = $2 AND is_indexed AND indexing",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::RemoveIndexStart => {
                transaction
                    .execute(
                        "UPDATE attrs SET is_indexed = false, indexing = true
                         WHERE app_id = $1 AND id = $2",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::RemoveIndexDone => {
                transaction
                    .execute(
                        "UPDATE attrs SET indexing = false
                         WHERE app_id = $1 AND id = $2 AND NOT is_indexed AND indexing",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::UniqueStart => {
                transaction
                    .execute(
                        "UPDATE attrs SET is_unique = true, setting_unique = true
                         WHERE app_id = $1 AND id = $2",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::UniqueAbort => {
                transaction
                    .execute(
                        "UPDATE attrs SET is_unique = false, setting_unique = false
                         WHERE app_id = $1 AND id = $2 AND is_unique AND setting_unique",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::UniqueDone => {
                transaction
                    .execute(
                        "UPDATE attrs SET setting_unique = false
                         WHERE app_id = $1 AND id = $2 AND is_unique AND setting_unique",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::RemoveUniqueStart => {
                transaction
                    .execute(
                        "UPDATE attrs SET is_unique = false, setting_unique = true
                         WHERE app_id = $1 AND id = $2",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::RemoveUniqueDone => {
                transaction
                    .execute(
                        "UPDATE attrs SET setting_unique = false
                         WHERE app_id = $1 AND id = $2 AND NOT is_unique AND setting_unique",
                        &[&job.app_id, &job.attr_id],
                    )
                    .await?
            }
            AttrMutation::SetRequired(required) => {
                transaction
                    .execute(
                        "UPDATE attrs SET is_required = $3 WHERE app_id = $1 AND id = $2",
                        &[&job.app_id, &job.attr_id, &required],
                    )
                    .await?
            }
        };
        transaction.commit().await?;
        Ok(if updated == 0 {
            StageResult::invalid_attr()
        } else {
            StageResult::Done
        })
    }

    async fn abort_index(&self, job: &IndexingJob) -> Result<(), IndexingWorkerError> {
        let _ = self.mutate_attr(job, AttrMutation::IndexAbort).await?;
        let client = self.client.acquire().await?;
        client
            .execute(
                "UPDATE triples SET ave = false WHERE app_id = $1 AND attr_id = $2 AND ave",
                &[&job.app_id, &job.attr_id],
            )
            .await?;
        Ok(())
    }

    async fn abort_unique(&self, job: &IndexingJob) -> Result<(), IndexingWorkerError> {
        let _ = self.mutate_attr(job, AttrMutation::UniqueAbort).await?;
        let client = self.client.acquire().await?;
        client
            .execute(
                "UPDATE triples SET av = false WHERE app_id = $1 AND attr_id = $2 AND av",
                &[&job.app_id, &job.attr_id],
            )
            .await?;
        Ok(())
    }

    async fn duplicate_value(
        &self,
        job: &IndexingJob,
    ) -> Result<Option<Value>, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        Ok(client
            .query_opt(
                "SELECT value FROM triples
                 WHERE app_id = $1 AND attr_id = $2 AND value <> 'null'::jsonb
                 GROUP BY value HAVING count(*) > 1 LIMIT 1",
                &[&job.app_id, &job.attr_id],
            )
            .await?
            .map(|row| row.get::<_, Value>(0)))
    }

    #[tracing::instrument(name = "indexing-jobs/release-job", skip_all, fields(job.id = %job.id))]
    async fn release(&self, job: &IndexingJob) -> Result<bool, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        Ok(client
            .execute(
                "UPDATE indexing_jobs SET worker_id = NULL
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &self.worker_id],
            )
            .await?
            > 0)
    }

    #[tracing::instrument(
        name = "indexing-jobs/set-next-stage",
        skip_all,
        fields(job.id = %job.id, job.stage = %next)
    )]
    async fn advance_and_release(
        &self,
        job: &IndexingJob,
        next: &str,
    ) -> Result<bool, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        Ok(client
            .execute(
                "UPDATE indexing_jobs SET job_stage = $3, worker_id = NULL
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &self.worker_id, &next],
            )
            .await?
            > 0)
    }

    #[tracing::instrument(name = "indexing-jobs/mark-job-completed", skip_all, fields(job.id = %job.id))]
    async fn complete(&self, job: &IndexingJob) -> Result<bool, IndexingWorkerError> {
        let client = self.client.acquire().await?;
        Ok(client
            .execute(
                "UPDATE indexing_jobs
                 SET job_status = 'completed', done_at = now()
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &self.worker_id],
            )
            .await?
            > 0)
    }

    #[tracing::instrument(
        name = "indexing-jobs/mark-error",
        skip_all,
        fields(job.id = %job.id, error = %failure.error)
    )]
    async fn mark_failed(
        &self,
        job: &IndexingJob,
        failure: &JobFailure,
    ) -> Result<(), IndexingWorkerError> {
        let client = self.client.acquire().await?;
        let invalid_unique = failure.invalid_unique_value.clone().map(Json);
        let error_data = failure.error_data.clone().map(Json);
        client
            .execute(
                "UPDATE indexing_jobs
                 SET job_status = 'errored', error = $3, error_detail = $4,
                     invalid_unique_value = $5, invalid_entity_id = $6, error_data = $7
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[
                    &job.id,
                    &self.worker_id,
                    &failure.error,
                    &failure.detail,
                    &invalid_unique,
                    &failure.invalid_entity_id,
                    &error_data,
                ],
            )
            .await?;
        Ok(())
    }
}

pub struct PostgresIndexingControlHooks<H> {
    fallback: Arc<H>,
    worker: Arc<PostgresIndexingWorker>,
}

impl<H> PostgresIndexingControlHooks<H> {
    #[must_use]
    pub fn new(fallback: Arc<H>, worker: Arc<PostgresIndexingWorker>) -> Self {
        Self { fallback, worker }
    }
}

impl<H> DashboardControlHooks for PostgresIndexingControlHooks<H>
where
    H: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::EnqueueIndexingJob { job_id } => {
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

const JOB_TYPES: [&str; 8] = [
    "check-data-type",
    "remove-data-type",
    "index",
    "remove-index",
    "unique",
    "remove-unique",
    "required",
    "remove-required",
];

const CHECK_DATA_TYPE_STAGES: &[&str] = &[
    "estimate-work",
    "validate",
    "update-attr-start",
    "revalidate",
    "update-triples",
    "update-attr-done",
];
const REMOVE_DATA_TYPE_STAGES: &[&str] = &[
    "update-attr-start",
    "estimate-work",
    "update-triples",
    "update-attr-done",
];
const INDEX_STAGES: &[&str] = &[
    "update-attr-start",
    "estimate-work",
    "estimate-work-with-undefineds",
    "update-triples",
    "insert-nulls",
    "update-attr-done",
];
const REMOVE_INDEX_STAGES: &[&str] = &[
    "update-attr-start",
    "estimate-work",
    "update-triples",
    "update-attr-done",
];
const UNIQUE_STAGES: &[&str] = &[
    "update-attr-start",
    "estimate-work",
    "update-triples",
    "update-attr-done",
];
const REMOVE_UNIQUE_STAGES: &[&str] = UNIQUE_STAGES;
const REQUIRED_STAGES: &[&str] = &["estimate-work", "validate", "update-attr", "revalidate"];
const REMOVE_REQUIRED_STAGES: &[&str] = &["update-attr"];

fn stages(job_type: &str) -> Option<&'static [&'static str]> {
    match job_type {
        "check-data-type" => Some(CHECK_DATA_TYPE_STAGES),
        "remove-data-type" => Some(REMOVE_DATA_TYPE_STAGES),
        "index" => Some(INDEX_STAGES),
        "remove-index" => Some(REMOVE_INDEX_STAGES),
        "unique" => Some(UNIQUE_STAGES),
        "remove-unique" => Some(REMOVE_UNIQUE_STAGES),
        "required" => Some(REQUIRED_STAGES),
        "remove-required" => Some(REMOVE_REQUIRED_STAGES),
        _ => None,
    }
}

fn next_stage(job_type: &str, current: &str) -> Result<Option<&'static str>, IndexingWorkerError> {
    let stages = stages(job_type)
        .ok_or_else(|| IndexingWorkerError::InvalidJob(format!("unknown job type {job_type}")))?;
    let index = stages
        .iter()
        .position(|stage| *stage == current)
        .ok_or_else(|| {
            IndexingWorkerError::InvalidJob(format!("unknown stage {job_type} {current}"))
        })?;
    Ok(stages.get(index + 1).copied())
}

#[derive(Debug)]
struct IndexingJob {
    id: Uuid,
    app_id: Uuid,
    attr_id: Uuid,
    job_type: String,
    job_stage: String,
    checked_data_type: Option<String>,
}

impl IndexingJob {
    fn from_row(row: &Row) -> Self {
        Self {
            id: row.get(0),
            app_id: row.get(1),
            attr_id: row.get(2),
            job_type: row.get(3),
            job_stage: row.get::<_, Option<String>>(5).unwrap_or_default(),
            checked_data_type: row.get(6),
        }
    }
}

#[derive(Debug)]
enum StageResult {
    Repeat,
    Done,
    Failed(JobFailure),
}

impl StageResult {
    fn invalid_attr() -> Self {
        Self::Failed(JobFailure::simple(INVALID_ATTR_STATE))
    }
}

#[derive(Debug)]
struct JobFailure {
    error: &'static str,
    detail: Option<String>,
    invalid_unique_value: Option<Value>,
    invalid_entity_id: Option<Uuid>,
    error_data: Option<Value>,
}

impl JobFailure {
    fn simple(error: &'static str) -> Self {
        Self {
            error,
            detail: None,
            invalid_unique_value: None,
            invalid_entity_id: None,
            error_data: None,
        }
    }

    fn missing_required(error_data: Value) -> Self {
        Self {
            error: MISSING_REQUIRED,
            detail: None,
            invalid_unique_value: None,
            invalid_entity_id: None,
            error_data: Some(error_data),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum AttrMutation {
    CheckTypeStart,
    CheckTypeAbort,
    CheckTypeDone,
    RemoveTypeStart,
    RemoveTypeDone,
    IndexStart,
    IndexAbort,
    IndexDone,
    RemoveIndexStart,
    RemoveIndexDone,
    UniqueStart,
    UniqueAbort,
    UniqueDone,
    RemoveUniqueStart,
    RemoveUniqueDone,
    SetRequired(bool),
}

#[derive(Debug)]
struct AttrState {
    value_type: String,
    etype: String,
    label: String,
}

async fn load_attr(
    client: &Client,
    job: &IndexingJob,
) -> Result<Option<AttrState>, tokio_postgres::Error> {
    Ok(client
        .query_opt(
            "SELECT value_type, etype, label FROM attrs WHERE app_id = $1 AND id = $2",
            &[&job.app_id, &job.attr_id],
        )
        .await?
        .map(|row| AttrState {
            value_type: row.get(0),
            etype: row.get(1),
            label: row.get(2),
        }))
}

async fn id_attr_id(
    client: &Client,
    app_id: Uuid,
    etype: &str,
) -> Result<Option<Uuid>, tokio_postgres::Error> {
    Ok(client
        .query_opt(
            "SELECT id FROM attrs
             WHERE app_id = $1 AND etype = $2 AND label = 'id'
               AND deletion_marked_at IS NULL",
            &[&app_id, &etype],
        )
        .await?
        .map(|row| row.get(0)))
}

async fn add_work(
    client: &Client,
    job: &IndexingJob,
    worker_id: &str,
    count: i64,
) -> Result<(), tokio_postgres::Error> {
    if count > 0 {
        let count = clamp_i32(count);
        client
            .execute(
                "UPDATE indexing_jobs
                 SET work_completed = coalesce(work_completed, 0) + $3
                 WHERE id = $1 AND worker_id = $2 AND job_status = 'processing'",
                &[&job.id, &worker_id, &count],
            )
            .await?;
    }
    Ok(())
}

fn batch_result(updated: u64) -> StageResult {
    if updated >= BATCH_SIZE_U64 {
        StageResult::Repeat
    } else {
        StageResult::Done
    }
}

fn rows_as_i64(updated: u64) -> i64 {
    i64::try_from(updated).unwrap_or(i64::MAX)
}

fn five_percent_headroom(value: i64) -> i32 {
    clamp_i32(value.saturating_mul(105) / 100)
}

fn clamp_i32(value: i64) -> i32 {
    i32::try_from(value.clamp(0, i64::from(i32::MAX))).unwrap_or(i32::MAX)
}

fn is_unique_violation(error: &tokio_postgres::Error) -> bool {
    error.code() == Some(&SqlState::UNIQUE_VIOLATION)
}

fn is_index_value_too_large(error: &tokio_postgres::Error) -> bool {
    matches!(
        error.code(),
        Some(&SqlState::PROGRAM_LIMIT_EXCEEDED | &SqlState::CONFIGURATION_LIMIT_EXCEEDED)
    ) || error.as_db_error().is_some_and(|database| {
        database.code() == &SqlState::CHECK_VIOLATION
            && database.constraint() == Some("indexed_values_are_constrained")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_graphs_match_the_authoritative_worker() {
        assert_eq!(stages("check-data-type"), Some(CHECK_DATA_TYPE_STAGES));
        assert_eq!(
            next_stage("index", "insert-nulls").unwrap(),
            Some("update-attr-done")
        );
        assert_eq!(next_stage("remove-required", "update-attr").unwrap(), None);
        assert!(next_stage("unique", "unknown").is_err());
    }

    #[test]
    fn estimates_are_bounded_and_include_reference_headroom() {
        assert_eq!(five_percent_headroom(1_000), 1_050);
        assert_eq!(five_percent_headroom(-1), 0);
        assert_eq!(clamp_i32(i64::MAX), i32::MAX);
    }
}
