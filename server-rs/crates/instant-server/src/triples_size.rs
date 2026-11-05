use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_postgres::Client;

use crate::postgres_pool::PostgresClientSource;
use crate::{PostgresPool, PostgresPoolError};

pub const DEFAULT_TRIPLES_SIZE_COLLECTION_BATCH_SIZE: i64 = 5_000;
pub const DEFAULT_TRIPLES_SIZE_COLLECTION_MAX_LOOPS: u64 = 1_000;
pub const DEFAULT_TRIPLES_SIZE_COLLECTION_ALERT_THRESHOLD: u64 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriplesSizeCollectionConfig {
    pub disabled: bool,
    pub batch_size: i64,
    pub max_loops: u64,
    pub alert_threshold: u64,
}

impl Default for TriplesSizeCollectionConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            batch_size: DEFAULT_TRIPLES_SIZE_COLLECTION_BATCH_SIZE,
            max_loops: DEFAULT_TRIPLES_SIZE_COLLECTION_MAX_LOOPS,
            alert_threshold: DEFAULT_TRIPLES_SIZE_COLLECTION_ALERT_THRESHOLD,
        }
    }
}

impl TriplesSizeCollectionConfig {
    /// Parses the four live config-app flags used by the reference daemon.
    ///
    /// # Errors
    ///
    /// Returns a named validation error for numeric values that PostgreSQL's
    /// `LIMIT` or the bounded native loop cannot safely consume.
    pub fn from_flags(flags: &HashMap<String, Value>) -> Result<Self, TriplesSizeCollectionError> {
        let defaults = Self::default();
        Ok(Self {
            disabled: flags
                .get("disable-triples-size-collection")
                .is_some_and(clojure_truthy),
            batch_size: flags
                .get("triples-size-collection-batch-size")
                .map_or(Ok(defaults.batch_size), |value| {
                    positive_i64("triples-size-collection-batch-size", value)
                })?,
            max_loops: flags
                .get("triples-size-collection-max-loops")
                .map_or(Ok(defaults.max_loops), |value| {
                    non_negative_u64("triples-size-collection-max-loops", value)
                })?,
            alert_threshold: flags
                .get("triples-size-collection-loop-limit-alert-threshold")
                .map_or(Ok(defaults.alert_threshold), |value| {
                    non_negative_u64("triples-size-collection-loop-limit-alert-threshold", value)
                })?,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TriplesSizeBatchOutcome {
    pub deleted: u64,
    pub aggregated: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TriplesSizeCollectionOutcome {
    pub loops: u64,
    pub total_collected: u64,
    pub total_aggregated: u64,
    pub reached_loop_limit: bool,
    pub consecutive_limit_hits: u64,
}

#[derive(Debug, Error)]
pub enum TriplesSizeCollectionError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
    #[error("config flag `{name}` must be {expected}")]
    InvalidFlag {
        name: &'static str,
        expected: &'static str,
    },
}

/// Native consumer for the trigger-fed `triples_size_updates` queue.
pub struct PostgresTriplesSizeCollector {
    client: PostgresClientSource,
    consecutive_limit_hits: AtomicU64,
}

impl PostgresTriplesSizeCollector {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self::from_source(PostgresClientSource::fixed(client))
    }

    #[must_use]
    pub fn with_shared_client(client: Arc<Mutex<Client>>) -> Self {
        Self::from_source(PostgresClientSource::fixed_shared(client))
    }

    /// Uses reconnecting pooled leases for each atomic aggregation batch.
    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool))
    }

    fn from_source(client: PostgresClientSource) -> Self {
        Self {
            client,
            consecutive_limit_hits: AtomicU64::new(0),
        }
    }

    /// Atomically claims, aggregates, and deletes one FIFO queue batch.
    /// Parent joins deliberately discard deltas for apps or attributes deleted
    /// between trigger insertion and collection.
    ///
    /// # Errors
    ///
    /// Returns a database error without partially consuming the batch.
    pub async fn collect_batch(
        &self,
        batch_size: i64,
    ) -> Result<TriplesSizeBatchOutcome, TriplesSizeCollectionError> {
        if batch_size <= 0 {
            return Err(invalid_flag(
                "triples-size-collection-batch-size",
                "a positive integer",
            ));
        }
        let client = self.client.acquire().await?;
        let row = client
            .query_one(
                "WITH ids AS (
                   SELECT id FROM triples_size_updates
                   ORDER BY id
                   LIMIT $1
                   FOR UPDATE SKIP LOCKED
                 ), deletes AS (
                   DELETE FROM triples_size_updates updates
                   USING ids
                   WHERE updates.id = ids.id
                   RETURNING updates.app_id, updates.attr_id,
                             updates.pg_size, updates.files_size
                 ), aggregate AS (
                   INSERT INTO triples_size_aggregate
                     (app_id, attr_id, pg_size, files_size)
                   SELECT deletes.app_id, deletes.attr_id,
                          sum(deletes.pg_size), sum(deletes.files_size)
                   FROM deletes
                   JOIN apps ON apps.id = deletes.app_id
                   JOIN attrs ON attrs.id = deletes.attr_id
                   GROUP BY deletes.app_id, deletes.attr_id
                   ORDER BY deletes.app_id, deletes.attr_id
                   ON CONFLICT ON CONSTRAINT triples_size_aggregate_pkey
                   DO UPDATE SET
                     pg_size = triples_size_aggregate.pg_size + excluded.pg_size,
                     files_size = coalesce(
                       triples_size_aggregate.files_size + excluded.files_size,
                       triples_size_aggregate.files_size,
                       excluded.files_size
                     )
                   RETURNING 1
                 )
                 SELECT (SELECT count(*) FROM deletes)::bigint,
                        (SELECT count(*) FROM aggregate)::bigint",
                &[&batch_size],
            )
            .await?;
        Ok(TriplesSizeBatchOutcome {
            deleted: non_negative_count(row.get::<_, i64>(0)),
            aggregated: non_negative_count(row.get::<_, i64>(1)),
        })
    }

    /// Drains full batches until a short batch proves the queue was empty at
    /// the statement boundary, or the live loop ceiling is reached.
    ///
    /// # Errors
    ///
    /// Returns the first database or configuration failure.
    pub async fn collect_batches(
        &self,
        max_loops: u64,
        batch_size: i64,
    ) -> Result<TriplesSizeCollectionOutcome, TriplesSizeCollectionError> {
        if batch_size <= 0 {
            return Err(invalid_flag(
                "triples-size-collection-batch-size",
                "a positive integer",
            ));
        }
        let mut outcome = TriplesSizeCollectionOutcome::default();
        for _ in 0..max_loops {
            let batch = self.collect_batch(batch_size).await?;
            outcome.total_collected = outcome.total_collected.saturating_add(batch.deleted);
            outcome.total_aggregated = outcome.total_aggregated.saturating_add(batch.aggregated);
            if batch.deleted < batch_size.cast_unsigned() {
                self.consecutive_limit_hits.store(0, Ordering::Relaxed);
                return Ok(outcome);
            }
            outcome.loops = outcome.loops.saturating_add(1);
        }
        outcome.reached_loop_limit = true;
        outcome.consecutive_limit_hits = self
            .consecutive_limit_hits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            })
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        Ok(outcome)
    }
}

fn non_negative_count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn clojure_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn positive_i64(name: &'static str, value: &Value) -> Result<i64, TriplesSizeCollectionError> {
    let value = value
        .as_i64()
        .filter(|value| *value > 0)
        .ok_or_else(|| invalid_flag(name, "a positive integer"))?;
    Ok(value)
}

fn non_negative_u64(name: &'static str, value: &Value) -> Result<u64, TriplesSizeCollectionError> {
    value
        .as_u64()
        .ok_or_else(|| invalid_flag(name, "a non-negative integer"))
}

const fn invalid_flag(name: &'static str, expected: &'static str) -> TriplesSizeCollectionError {
    TriplesSizeCollectionError::InvalidFlag { name, expected }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn defaults_and_live_controls_match_the_reference_daemon() {
        assert_eq!(
            TriplesSizeCollectionConfig::from_flags(&HashMap::new()).unwrap(),
            TriplesSizeCollectionConfig::default()
        );
        let flags = HashMap::from([
            ("disable-triples-size-collection".into(), json!([])),
            ("triples-size-collection-batch-size".into(), json!(25)),
            ("triples-size-collection-max-loops".into(), json!(7)),
            (
                "triples-size-collection-loop-limit-alert-threshold".into(),
                json!(2),
            ),
        ]);
        assert_eq!(
            TriplesSizeCollectionConfig::from_flags(&flags).unwrap(),
            TriplesSizeCollectionConfig {
                disabled: true,
                batch_size: 25,
                max_loops: 7,
                alert_threshold: 2,
            }
        );
    }

    #[test]
    fn malformed_numeric_controls_fail_closed() {
        for (name, value) in [
            ("triples-size-collection-batch-size", json!(0)),
            ("triples-size-collection-max-loops", json!(-1)),
            (
                "triples-size-collection-loop-limit-alert-threshold",
                json!("ten"),
            ),
        ] {
            let error =
                TriplesSizeCollectionConfig::from_flags(&HashMap::from([(name.to_owned(), value)]))
                    .unwrap_err();
            assert!(error.to_string().contains(name));
        }
    }
}
