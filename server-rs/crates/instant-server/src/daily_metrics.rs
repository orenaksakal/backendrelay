use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use thiserror::Error;
use tokio_postgres::Client;
use tracing::{Instrument as _, info, info_span};

use crate::postgres_pool::PostgresClientSource;
use crate::{
    JvmTaskSchedules, PostgresFailoverGate, PostgresPool, PostgresPoolError, ScheduledTask,
};

pub type DailyMetricsGateFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>>;

pub trait DailyMetricsFailoverGate: Send + Sync + 'static {
    fn is_failing_over(&self) -> DailyMetricsGateFuture<'_>;
}

impl DailyMetricsFailoverGate for PostgresFailoverGate {
    fn is_failing_over(&self) -> DailyMetricsGateFuture<'_> {
        Box::pin(async move {
            PostgresFailoverGate::is_failing_over(self)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DailyMetricsRun {
    SkippedFailover,
    AlreadyClaimed,
    Completed { inserted_activity_rows: u64 },
}

#[derive(Debug, Error)]
pub enum DailyMetricsError {
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
    #[error("failover gate error: {0}")]
    Failover(String),
}

/// Native owner for daily activity aggregation.
pub struct PostgresDailyMetricsWorker {
    client: PostgresClientSource,
    failover: Arc<dyn DailyMetricsFailoverGate>,
}

impl PostgresDailyMetricsWorker {
    #[must_use]
    pub fn new(client: Client, failover: Arc<dyn DailyMetricsFailoverGate>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            failover,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, failover: Arc<dyn DailyMetricsFailoverGate>) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            failover,
        }
    }

    /// Runs the Pacific-calendar pass represented by `now`.
    ///
    /// # Errors
    ///
    /// Returns failover gate or database failures.
    pub async fn run_at(&self, now: DateTime<Utc>) -> Result<DailyMetricsRun, DailyMetricsError> {
        if self
            .failover
            .is_failing_over()
            .await
            .map_err(DailyMetricsError::Failover)?
        {
            return Ok(DailyMetricsRun::SkippedFailover);
        }
        let run_date = now.with_timezone(&chrono_tz::US::Pacific).date_naive();
        let mut client = self.client.acquire().await?;
        let grab_id = format!("daily-metrics-{run_date}");
        if client
            .query_opt(
                "INSERT INTO grabs (id) VALUES ($1)
                 ON CONFLICT (id) DO NOTHING RETURNING id",
                &[&grab_id],
            )
            .await?
            .is_none()
        {
            return Ok(DailyMetricsRun::AlreadyClaimed);
        }
        let transaction = client.transaction().await?;
        transaction
            .batch_execute("SET LOCAL statement_timeout = '360s'")
            .await?;
        let inserted_activity_rows = transaction
            .execute(
                "WITH date_range AS (
                   SELECT
                     coalesce(max(date) + interval '1 day', '2022-01-01') AS last_seen_date,
                     current_date AS max_date
                   FROM daily_app_transactions
                 ), window_txns AS (
                   SELECT txn.app_id, txn.created_at
                   FROM transactions txn, date_range dr
                   WHERE txn.created_at > dr.last_seen_date
                     AND txn.created_at < dr.max_date
                 ), recorded_active_dates AS (
                   SELECT app_id, min(active_date) AS active_date
                   FROM daily_app_transactions
                   GROUP BY app_id
                 ), app_active_dates AS (
                   SELECT w.app_id,
                          coalesce(recorded.active_date,
                                   min(w.created_at) + interval '7 days') AS active_date
                   FROM window_txns w
                   LEFT JOIN recorded_active_dates recorded ON recorded.app_id = w.app_id
                   GROUP BY w.app_id, recorded.active_date
                 )
                 INSERT INTO daily_app_transactions (date, app_id, active_date, is_active, count)
                 SELECT date(w.created_at), w.app_id, active.active_date,
                        w.created_at > active.active_date, count(*)
                 FROM window_txns w
                 JOIN app_active_dates active ON active.app_id = w.app_id
                 GROUP BY 1, 2, 3, 4",
                &[],
            )
            .await?;
        transaction.commit().await?;
        Ok(DailyMetricsRun::Completed {
            inserted_activity_rows,
        })
    }

    #[must_use]
    pub fn scheduled_task(self: &Arc<Self>) -> ScheduledTask {
        let worker = Arc::clone(self);
        ScheduledTask::new_unbounded(
            "daily-metrics",
            JvmTaskSchedules::DAILY_METRICS,
            move || {
                let worker = Arc::clone(&worker);
                async move {
                    let outcome = worker
                        .run_at(Utc::now())
                        .instrument(info_span!("daily-metrics/run"))
                        .await?;
                    info!(target: "instant.daily-metrics", ?outcome, "daily metrics pass completed");
                    Ok::<(), DailyMetricsError>(())
                }
            },
        )
    }
}
