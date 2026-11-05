use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio_postgres::Client;

use crate::postgres_pool::PostgresClientSource;
use crate::{PostgresPool, PostgresPoolError};

const WAL_PARTITION_COUNT: u8 = 8;
const HISTORY_PARTITION_COUNT: u8 = 13;
const HISTORY_BUCKET_DAYS: i64 = 30;
const HISTORY_BUCKET_SECONDS: i64 = HISTORY_BUCKET_DAYS * 24 * 60 * 60;
const DEFAULT_HISTORY_RETENTION_DAYS: i64 = 30;
const HISTORY_RETENTION_DAYS_ENV: &str = "INSTANT_HISTORY_RETENTION_DAYS";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionRetentionOutcome {
    pub wal_partitions: Vec<u8>,
    pub history_partitions: Vec<u8>,
}

impl PartitionRetentionOutcome {
    fn wal(partitions: [u8; 2]) -> Self {
        Self {
            wal_partitions: partitions.to_vec(),
            history_partitions: Vec::new(),
        }
    }

    fn history(partitions: Vec<u8>) -> Self {
        Self {
            wal_partitions: Vec::new(),
            history_partitions: partitions,
        }
    }
}

#[derive(Debug, Error)]
pub enum PartitionRetentionError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
}

/// Native owner for the JVM WAL-log and webhook-history partition truncators.
pub struct PostgresPartitionRetention {
    client: PostgresClientSource,
    history_retention_days: i64,
}

impl PostgresPartitionRetention {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            history_retention_days: history_retention_days_from_env(),
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            history_retention_days: history_retention_days_from_env(),
        }
    }

    /// Overrides the configured history retention, at least one day.
    #[must_use]
    pub fn with_history_retention_days(mut self, days: i64) -> Self {
        self.history_retention_days = days.max(1);
        self
    }

    /// Truncates the JVM's second- and third-oldest hourly WAL partitions.
    ///
    /// # Errors
    ///
    /// Returns a pool or PostgreSQL error and stops before later partitions,
    /// matching the reference sequential daemon.
    pub async fn truncate_wal(
        &self,
        now: DateTime<Utc>,
    ) -> Result<PartitionRetentionOutcome, PartitionRetentionError> {
        let partitions = wal_partitions_to_truncate(now);
        let client = self.client.acquire().await?;
        for partition in partitions {
            client
                .batch_execute(&format!("TRUNCATE wal_logs_{partition}"))
                .await?;
        }
        Ok(PartitionRetentionOutcome::wal(partitions))
    }

    /// Truncates the 30-day history buckets outside the configured retention
    /// window. The default keeps 30 days; the previous 30-day buckets are
    /// truncated oldest first. Each webhook-events partition is truncated
    /// before its matching history partition exactly as in the reference
    /// worker.
    ///
    /// # Errors
    ///
    /// Returns a pool or PostgreSQL error and stops before later statements.
    pub async fn truncate_history(
        &self,
        now: DateTime<Utc>,
    ) -> Result<PartitionRetentionOutcome, PartitionRetentionError> {
        let partitions = history_partitions_to_truncate(now, self.history_retention_days);
        let client = self.client.acquire().await?;
        for partition in &partitions {
            client
                .batch_execute(&format!("TRUNCATE webhook_events_{partition}"))
                .await?;
            client
                .batch_execute(&format!("TRUNCATE history_{partition}"))
                .await?;
        }
        Ok(PartitionRetentionOutcome::history(partitions))
    }
}

#[must_use]
pub fn wal_partitions_to_truncate(now: DateTime<Utc>) -> [u8; 2] {
    let current = u8::try_from(now.timestamp().div_euclid(60 * 60).rem_euclid(24))
        .unwrap_or_default()
        % WAL_PARTITION_COUNT;
    [
        (current + 2) % WAL_PARTITION_COUNT,
        (current + 3) % WAL_PARTITION_COUNT,
    ]
}

#[must_use]
pub fn history_partition_bucket(now: DateTime<Utc>) -> u8 {
    u8::try_from(
        now.timestamp()
            .div_euclid(HISTORY_BUCKET_SECONDS)
            .rem_euclid(i64::from(HISTORY_PARTITION_COUNT)),
    )
    .unwrap_or_default()
}

#[must_use]
pub fn history_partitions_to_truncate(now: DateTime<Utc>, retention_days: i64) -> Vec<u8> {
    let current = history_partition_bucket(now);
    let keep = history_buckets_to_keep(retention_days);
    (1..=HISTORY_PARTITION_COUNT - keep)
        .map(|offset| (current + offset) % HISTORY_PARTITION_COUNT)
        .collect()
}

/// Number of 30-day buckets retained, rounded up so a retention shorter than a
/// bucket still keeps the current one.
fn history_buckets_to_keep(retention_days: i64) -> u8 {
    let buckets = retention_days
        .max(1)
        .saturating_add(HISTORY_BUCKET_DAYS - 1)
        / HISTORY_BUCKET_DAYS;
    u8::try_from(buckets.clamp(1, i64::from(HISTORY_PARTITION_COUNT)))
        .unwrap_or(HISTORY_PARTITION_COUNT)
}

/// Reads `INSTANT_HISTORY_RETENTION_DAYS`; invalid or missing values keep the
/// 30-day default.
#[must_use]
pub fn history_retention_days_from_env() -> i64 {
    parse_history_retention_days(std::env::var(HISTORY_RETENTION_DAYS_ENV).ok().as_deref())
}

fn parse_history_retention_days(value: Option<&str>) -> i64 {
    value
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|days| *days >= 1)
        .unwrap_or(DEFAULT_HISTORY_RETENTION_DAYS)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn wal_rotation_skips_the_current_next_and_oldest_partitions() {
        let at_hour_five = Utc.with_ymd_and_hms(2026, 8, 28, 5, 0, 0).unwrap();
        assert_eq!(wal_partitions_to_truncate(at_hour_five), [7, 0]);
        let at_hour_twenty_three = Utc.with_ymd_and_hms(2026, 8, 28, 23, 59, 59).unwrap();
        assert_eq!(wal_partitions_to_truncate(at_hour_twenty_three), [1, 2]);
    }

    #[test]
    fn history_rotation_keeps_current_and_three_previous_buckets() {
        let now = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        let current = history_partition_bucket(now);
        let truncated = history_partitions_to_truncate(now, 120);
        assert_eq!(truncated.len(), 9);
        assert_eq!(truncated[0], (current + 1) % HISTORY_PARTITION_COUNT);
        assert_eq!(truncated[8], (current + 9) % HISTORY_PARTITION_COUNT);
        for kept_offset in [0, 10, 11, 12] {
            assert!(!truncated.contains(&((current + kept_offset) % HISTORY_PARTITION_COUNT)));
        }
    }

    #[test]
    fn history_retention_defaults_to_thirty_days_and_keeps_the_current_bucket() {
        assert_eq!(parse_history_retention_days(None), 30);
        assert_eq!(parse_history_retention_days(Some("1")), 1);
        assert_eq!(parse_history_retention_days(Some(" 45 ")), 45);
        assert_eq!(parse_history_retention_days(Some("0")), 30);
        assert_eq!(parse_history_retention_days(Some("not-a-number")), 30);

        let now = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        let current = history_partition_bucket(now);
        let truncated = history_partitions_to_truncate(now, 30);
        assert_eq!(truncated.len(), 12);
        assert!(!truncated.contains(&current));
    }

    #[test]
    fn history_retention_longer_than_the_partition_ring_keeps_every_bucket() {
        let now = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        assert!(history_partitions_to_truncate(now, 365).is_empty());
    }
}
