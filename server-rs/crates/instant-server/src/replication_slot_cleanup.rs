use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use instant_pg::{
    ReplicationSlotError, cleanup_inactive_replication_slots, drop_logical_replication_slot,
    inactive_invalidator_slots, stale_invalidator_slots,
};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_postgres::Client;

use crate::postgres_pool::PostgresClientSource;
use crate::{JvmTaskSchedules, PostgresPool, PostgresPoolError, ScheduledTask};

const JVM_SLOT_CLEANUP_GRACE: Duration = Duration::from_secs(300);
/// A machine that has not refreshed its five-second heartbeat for this long is
/// treated as gone when a process starts.
const STALE_MACHINE_GRACE: Duration = Duration::from_secs(60);
const SLOT_DROP_ATTEMPTS: usize = 8;
const SLOT_DROP_RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplicationSlotCleanupOutcome {
    pub candidates: Vec<String>,
    pub cleaned: Vec<String>,
    pub retained: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ReplicationSlotCleanupError {
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
    #[error(transparent)]
    Slot(#[from] ReplicationSlotError),
}

/// Two-phase cleanup for abandoned per-machine invalidator slots.
///
/// It snapshots inactive slots, waits the JVM's five-minute setup grace, and
/// asks PostgreSQL to drop only candidates that are still inactive.
pub struct PostgresReplicationSlotCleaner {
    client: PostgresClientSource,
    grace: Duration,
}

impl PostgresReplicationSlotCleaner {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self::from_source(PostgresClientSource::fixed(client))
    }

    #[must_use]
    pub fn with_shared_client(client: Arc<Mutex<Client>>) -> Self {
        Self::from_source(PostgresClientSource::fixed_shared(client))
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool))
    }

    fn from_source(client: PostgresClientSource) -> Self {
        Self {
            client,
            grace: JVM_SLOT_CLEANUP_GRACE,
        }
    }

    /// Overrides the delay between snapshot and cleanup. Production uses the
    /// JVM-compatible five-minute default; shorter values support bounded tests.
    #[must_use]
    pub const fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Runs one cleanup pass.
    ///
    /// # Errors
    ///
    /// Returns a PostgreSQL or replication-slot validation failure.
    pub async fn run_once(
        &self,
    ) -> Result<ReplicationSlotCleanupOutcome, ReplicationSlotCleanupError> {
        let candidates = {
            let client = self.client.acquire().await?;
            inactive_invalidator_slots(&*client).await?
        };
        if candidates.is_empty() {
            return Ok(ReplicationSlotCleanupOutcome::default());
        }
        tokio::time::sleep(self.grace).await;
        let cleaned = {
            let client = self.client.acquire().await?;
            cleanup_inactive_replication_slots(&*client, &candidates).await?
        };
        Ok(classify(candidates, cleaned))
    }

    /// Runs the startup pass that removes slots left by machines that are gone.
    ///
    /// Heartbeat staleness (60 seconds) replaces the hourly cleaner's five-minute
    /// post-snapshot grace. The second query still rechecks inactivity before
    /// dropping, so a machine that starts during this pass keeps its slot.
    ///
    /// # Errors
    ///
    /// Returns a PostgreSQL or replication-slot validation failure.
    pub async fn run_stale_machine_cleanup(
        &self,
    ) -> Result<ReplicationSlotCleanupOutcome, ReplicationSlotCleanupError> {
        let candidates = {
            let client = self.client.acquire().await?;
            stale_invalidator_slots(&*client, STALE_MACHINE_GRACE).await?
        };
        if candidates.is_empty() {
            return Ok(ReplicationSlotCleanupOutcome::default());
        }
        let cleaned = {
            let client = self.client.acquire().await?;
            cleanup_inactive_replication_slots(&*client, &candidates).await?
        };
        Ok(classify(candidates, cleaned))
    }

    /// Creates the JVM-compatible hourly supervised cleanup task.
    #[must_use]
    pub fn scheduled_task(self: &Arc<Self>) -> ScheduledTask {
        let cleaner = Arc::clone(self);
        ScheduledTask::new(
            "replication-slot-cleanup",
            JvmTaskSchedules::REPLICATION_SLOT_CLEANUP,
            self.grace + Duration::from_secs(60),
            move || {
                let cleaner = Arc::clone(&cleaner);
                async move { cleaner.run_once().await.map(drop) }
            },
        )
    }
}

/// Drops this process's invalidator slot after a graceful drain.
///
/// A replication connection may still be closing when the invalidator task
/// returns, so the drop retries briefly while PostgreSQL reports the slot as
/// active. A missing slot is already the desired end state.
pub async fn drop_process_invalidator_slot(pool: &PostgresPool, slot_name: &str) {
    for attempt in 1..=SLOT_DROP_ATTEMPTS {
        let Ok(client) = pool.acquire().await else {
            tracing::warn!(
                slot = %slot_name,
                "could not acquire a connection to drop this process's invalidator slot"
            );
            return;
        };
        match drop_logical_replication_slot(&**client, slot_name).await {
            Ok(()) => {
                tracing::info!(
                    slot = %slot_name,
                    "dropped this process's invalidator slot during shutdown"
                );
                return;
            }
            Err(ReplicationSlotError::Postgres(error)) if missing_slot(&error) => return,
            Err(_error) if attempt < SLOT_DROP_ATTEMPTS => {
                drop(client);
                tokio::time::sleep(SLOT_DROP_RETRY_DELAY).await;
            }
            Err(error) => {
                tracing::warn!(
                    slot = %slot_name,
                    %error,
                    "could not drop this process's invalidator slot during shutdown"
                );
                return;
            }
        }
    }
}

fn missing_slot(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|error| error.code() == &tokio_postgres::error::SqlState::UNDEFINED_OBJECT)
}

fn classify(
    mut candidates: Vec<String>,
    mut cleaned: Vec<String>,
) -> ReplicationSlotCleanupOutcome {
    candidates.sort();
    candidates.dedup();
    cleaned.sort();
    cleaned.dedup();
    let cleaned_set = cleaned.iter().cloned().collect::<HashSet<_>>();
    let retained = candidates
        .iter()
        .filter(|slot| !cleaned_set.contains(*slot))
        .cloned()
        .collect();
    ReplicationSlotCleanupOutcome {
        candidates,
        cleaned,
        retained,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_reports_slots_that_became_active_during_grace() {
        let outcome = classify(
            vec!["invalidator_b".to_owned(), "invalidator_a".to_owned()],
            vec!["invalidator_b".to_owned()],
        );
        assert_eq!(
            outcome,
            ReplicationSlotCleanupOutcome {
                candidates: vec!["invalidator_a".to_owned(), "invalidator_b".to_owned()],
                cleaned: vec!["invalidator_b".to_owned()],
                retained: vec!["invalidator_a".to_owned()],
            }
        );
    }
}
