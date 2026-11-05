use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::admin_storage::{StorageObjectStore, object_key};
use crate::postgres_pool::PostgresClientSource;
use crate::{PostgresPool, PostgresPoolError};

const DEFAULT_BATCH_SIZE: i64 = 1_000;
pub const DEFAULT_MAX_LOOPS: u32 = 1_000;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StorageSweepOutcome {
    pub loops: u32,
    pub deleted: u64,
    pub reached_loop_limit: bool,
}

#[derive(Debug, Error)]
pub enum StorageSweepError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
    #[error("object store batch delete failed for {count} keys: {message}")]
    ObjectStore { count: usize, message: String },
    #[error("storage-sweeper-max-loops must be a non-negative integer, found {0}")]
    InvalidMaxLoops(Value),
}

/// Deletes storage objects queued by triple deletion after the backup/lifecycle
/// grace period, then removes their durable queue rows.
pub struct PostgresStorageSweeper<S> {
    client: PostgresClientSource,
    objects: Arc<S>,
    process_id: String,
}

impl<S> PostgresStorageSweeper<S>
where
    S: StorageObjectStore,
{
    #[must_use]
    pub fn new(client: Client, objects: Arc<S>, process_id: impl Into<String>) -> Self {
        Self::from_source(PostgresClientSource::fixed(client), objects, process_id)
    }

    #[must_use]
    pub fn with_shared_client(
        client: Arc<Mutex<Client>>,
        objects: Arc<S>,
        process_id: impl Into<String>,
    ) -> Self {
        Self::from_source(
            PostgresClientSource::fixed_shared(client),
            objects,
            process_id,
        )
    }

    /// Builds the production sweeper over a bounded reconnecting pool.
    #[must_use]
    pub fn from_pool(client: PostgresPool, objects: Arc<S>, process_id: impl Into<String>) -> Self {
        Self::from_source(PostgresClientSource::pooled(client), objects, process_id)
    }

    fn from_source(
        client: PostgresClientSource,
        objects: Arc<S>,
        process_id: impl Into<String>,
    ) -> Self {
        Self {
            client,
            objects,
            process_id: process_id.into(),
        }
    }

    /// Processes one claimed batch. Objects are removed before their queue rows
    /// so failures remain recoverable after the five-minute claim lease.
    ///
    /// # Errors
    ///
    /// Returns the first database or object-store failure.
    #[tracing::instrument(
        name = "storage-sweeper/process-sweep!",
        skip_all,
        fields(app.id = ?app_id, batch.limit = limit)
    )]
    pub async fn process_batch(
        &self,
        app_id: Option<Uuid>,
        limit: i64,
    ) -> Result<u64, StorageSweepError> {
        let rows = {
            let client = self.client.acquire().await?;
            client
                .query(
                    "WITH claimed AS (
                       SELECT id FROM app_files_to_sweep
                       WHERE created_at < now() - interval '32 days'
                         AND ($2::uuid IS NULL OR app_id = $2)
                         AND (process_id IS NULL OR updated_at < now() - interval '5 minutes')
                       ORDER BY created_at
                       LIMIT $3 FOR UPDATE SKIP LOCKED
                     )
                     UPDATE app_files_to_sweep queued
                     SET process_id = $1, updated_at = now()
                     FROM claimed WHERE queued.id = claimed.id
                     RETURNING queued.id, queued.app_id, queued.location_id",
                    &[
                        &self.process_id,
                        &app_id,
                        &limit.clamp(1, DEFAULT_BATCH_SIZE),
                    ],
                )
                .await?
                .into_iter()
                .map(|row| SweepRow {
                    id: row.get(0),
                    app_id: row.get(1),
                    location_id: row.get(2),
                })
                .collect::<Vec<_>>()
        };
        if rows.is_empty() {
            return Ok(0);
        }
        let keys = rows
            .iter()
            .map(|row| object_key(row.app_id, &row.location_id))
            .collect::<Vec<_>>();
        self.objects
            .delete_many(keys)
            .await
            .map_err(|message| StorageSweepError::ObjectStore {
                count: rows.len(),
                message,
            })?;
        let ids = rows.iter().map(|row| row.id).collect::<Vec<_>>();
        let client = self.client.acquire().await?;
        let deleted = client
            .execute(
                "DELETE FROM app_files_to_sweep
                 WHERE id = ANY($1) AND process_id = $2",
                &[&ids, &self.process_id],
            )
            .await?;
        Ok(deleted)
    }

    /// Drains eligible files until a short batch or the configured loop bound.
    ///
    /// # Errors
    ///
    /// Returns the first batch failure.
    #[tracing::instrument(
        name = "storage-sweeper/handle-sweep!",
        skip_all,
        fields(app.id = ?app_id, max.loops = max_loops, batch.limit = limit)
    )]
    pub async fn sweep(
        &self,
        app_id: Option<Uuid>,
        max_loops: u32,
        limit: i64,
    ) -> Result<StorageSweepOutcome, StorageSweepError> {
        let limit = limit.clamp(1, DEFAULT_BATCH_SIZE);
        let mut outcome = StorageSweepOutcome::default();
        if max_loops == 0 {
            outcome.reached_loop_limit = true;
            return Ok(outcome);
        }
        for current in 1..=max_loops {
            let deleted = self.process_batch(app_id, limit).await?;
            outcome.loops = current;
            outcome.deleted = outcome.deleted.saturating_add(deleted);
            if deleted < u64::try_from(limit).unwrap_or(u64::MAX) {
                return Ok(outcome);
            }
        }
        outcome.reached_loop_limit = true;
        Ok(outcome)
    }
}

/// Resolves the live sweep-loop flag while preserving the JVM default.
///
/// # Errors
///
/// Returns an explicit error for negative, fractional, string, or oversized values.
pub fn configured_storage_sweep_max_loops(value: Option<&Value>) -> Result<u32, StorageSweepError> {
    let Some(value) = value else {
        return Ok(DEFAULT_MAX_LOOPS);
    };
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| StorageSweepError::InvalidMaxLoops(value.clone()))
}

struct SweepRow {
    id: Uuid,
    app_id: Uuid,
    location_id: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn sweep_outcome_starts_empty() {
        assert_eq!(StorageSweepOutcome::default().deleted, 0);
        assert!(!StorageSweepOutcome::default().reached_loop_limit);
    }

    #[test]
    fn max_loop_flag_preserves_default_override_zero_and_fail_closed_behavior() {
        assert_eq!(configured_storage_sweep_max_loops(None).unwrap(), 1_000);
        assert_eq!(
            configured_storage_sweep_max_loops(Some(&json!(37))).unwrap(),
            37
        );
        assert_eq!(
            configured_storage_sweep_max_loops(Some(&json!(0))).unwrap(),
            0
        );
        for invalid in [json!(-1), json!(1.5), json!("1000"), json!(u64::MAX)] {
            assert!(matches!(
                configured_storage_sweep_max_loops(Some(&invalid)),
                Err(StorageSweepError::InvalidMaxLoops(value)) if value == invalid
            ));
        }
    }
}
