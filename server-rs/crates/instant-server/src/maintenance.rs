use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Mutex;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::{PostgresPool, PostgresPoolError, ServerEnvironment};

pub const RATE_LIMIT_SWEEP_BATCH_SIZE: i64 = 10_000;
pub const DEFAULT_RATE_LIMIT_SWEEP_MAX_LOOPS: u32 = 100;
pub const RATE_LIMIT_SWEEP_BACKLOG_ERROR: &str = "Too many rate limit sweep loops";
const EPHEMERAL_EXPIRATION_DAYS: i32 = 14;
const EPHEMERAL_GRACE_DAYS: i32 = 1;
const EPHEMERAL_DEVELOPMENT_EMAIL: &str = "ephemeral-apps-dev@backendrelay.invalid";
const EPHEMERAL_HOSTED_EMAIL: &str = "ephemeral-apps@backendrelay.invalid";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RateLimitSweepOutcome {
    pub loops: u32,
    pub deleted: u64,
    pub reached_loop_limit: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EphemeralAppSweepOutcome {
    pub deleted_app_ids: Vec<Uuid>,
}

#[derive(Debug, Error)]
pub enum MaintenanceSweepError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
}

/// Deletes durable rate-limit buckets that have not been updated for two days.
///
/// The JVM daemon invokes this hourly. Keeping scheduling outside this type lets
/// the same race-safe sweep be driven by a process lifecycle or an external job.
pub struct PostgresRateLimitSweeper {
    client: PostgresClientSource,
}

impl PostgresRateLimitSweeper {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self::from_source(PostgresClientSource::fixed(client))
    }

    #[must_use]
    pub fn with_shared_client(client: Arc<Mutex<Client>>) -> Self {
        Self::from_source(PostgresClientSource::fixed_shared(client))
    }

    /// Uses reconnecting pooled leases for each bounded deletion batch.
    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool))
    }

    fn from_source(client: PostgresClientSource) -> Self {
        Self { client }
    }

    /// Deletes one locked batch and returns the number of removed keys.
    ///
    /// # Errors
    ///
    /// Returns a database error if the batch cannot be claimed or deleted.
    pub async fn process_batch(&self, limit: i64) -> Result<u64, MaintenanceSweepError> {
        let client = self.client.acquire().await?;
        Ok(client
            .execute(
                "WITH keys AS (
                   SELECT key FROM rate_limit_keys
                   WHERE updated_at < now() - interval '2 days'
                   ORDER BY updated_at
                   LIMIT $1 FOR UPDATE SKIP LOCKED
                 )
                 DELETE FROM rate_limit_keys stored
                 USING keys WHERE stored.key = keys.key",
                &[&limit.clamp(1, RATE_LIMIT_SWEEP_BATCH_SIZE)],
            )
            .await?)
    }

    /// Drains stale keys until an empty batch or the configured loop bound.
    ///
    /// # Errors
    ///
    /// Returns the first database failure.
    pub async fn sweep(
        &self,
        max_loops: u32,
        limit: i64,
    ) -> Result<RateLimitSweepOutcome, MaintenanceSweepError> {
        let limit = limit.clamp(1, RATE_LIMIT_SWEEP_BATCH_SIZE);
        let mut outcome = RateLimitSweepOutcome::default();
        if max_loops == 0 {
            outcome.reached_loop_limit = true;
            return Ok(outcome);
        }
        for current in 1..=max_loops {
            let deleted = self.process_batch(limit).await?;
            outcome.loops = current;
            outcome.deleted = outcome.deleted.saturating_add(deleted);
            if deleted == 0 {
                return Ok(outcome);
            }
        }
        outcome.reached_loop_limit = true;
        Ok(outcome)
    }
}

/// Hard-deletes expired apps owned by the dedicated ephemeral-app creator.
/// Foreign keys perform the same cascading cleanup as the JVM app model.
pub struct PostgresEphemeralAppSweeper {
    client: PostgresClientSource,
}

impl PostgresEphemeralAppSweeper {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self::from_source(PostgresClientSource::fixed(client))
    }

    #[must_use]
    pub fn with_shared_client(client: Arc<Mutex<Client>>) -> Self {
        Self::from_source(PostgresClientSource::fixed_shared(client))
    }

    /// Uses reconnecting pooled leases for creator lookup and bounded deletion.
    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool))
    }

    fn from_source(client: PostgresClientSource) -> Self {
        Self { client }
    }

    /// Deletes the creator's non-soft-deleted apps older than the supplied age.
    /// This lower-level form is useful for deterministic tests and manual sweeps.
    ///
    /// # Errors
    ///
    /// Returns a database error if the sweep cannot run.
    pub async fn sweep_created_before(
        &self,
        creator_id: Uuid,
        age_days: i32,
    ) -> Result<EphemeralAppSweepOutcome, MaintenanceSweepError> {
        let age_days = age_days.max(0);
        let client = self.client.acquire().await?;
        let rows = client
            .query(
                "DELETE FROM apps
                 WHERE creator_id = $1
                   AND created_at < now() - make_interval(days => $2)
                   AND deletion_marked_at IS NULL
                 RETURNING id",
                &[&creator_id, &age_days],
            )
            .await?;
        Ok(EphemeralAppSweepOutcome {
            deleted_app_ids: rows.into_iter().map(|row| row.get(0)).collect(),
        })
    }

    /// Applies the JVM lifetime: fourteen days plus one extra grace day.
    ///
    /// # Errors
    ///
    /// Returns a database error if the sweep cannot run.
    pub async fn sweep_expired(
        &self,
        creator_id: Uuid,
    ) -> Result<EphemeralAppSweepOutcome, MaintenanceSweepError> {
        self.sweep_created_before(creator_id, EPHEMERAL_EXPIRATION_DAYS + EPHEMERAL_GRACE_DAYS)
            .await
    }

    /// Resolves the authoritative environment-specific ephemeral creator and
    /// applies the normal expiration sweep. A deployment without that seeded
    /// identity has no ephemeral apps and returns an empty outcome.
    ///
    /// # Errors
    ///
    /// Returns a database error if creator resolution or deletion fails.
    pub async fn sweep_environment(
        &self,
        environment: ServerEnvironment,
    ) -> Result<EphemeralAppSweepOutcome, MaintenanceSweepError> {
        let email = if environment == ServerEnvironment::Development {
            EPHEMERAL_DEVELOPMENT_EMAIL
        } else {
            EPHEMERAL_HOSTED_EMAIL
        };
        let creator_id = {
            let client = self.client.acquire().await?;
            client
                .query_opt("SELECT id FROM instant_users WHERE email = $1", &[&email])
                .await?
                .map(|row| row.get(0))
        };
        match creator_id {
            Some(creator_id) => self.sweep_expired(creator_id).await,
            None => Ok(EphemeralAppSweepOutcome::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_outcomes_start_empty() {
        assert_eq!(RateLimitSweepOutcome::default().deleted, 0);
        assert!(
            EphemeralAppSweepOutcome::default()
                .deleted_app_ids
                .is_empty()
        );
    }

    #[test]
    fn rate_limit_sweep_production_controls_match_the_reference_daemon() {
        assert_eq!(DEFAULT_RATE_LIMIT_SWEEP_MAX_LOOPS, 100);
        assert_eq!(RATE_LIMIT_SWEEP_BATCH_SIZE, 10_000);
        assert_eq!(
            RATE_LIMIT_SWEEP_BACKLOG_ERROR,
            "Too many rate limit sweep loops"
        );
    }
}
