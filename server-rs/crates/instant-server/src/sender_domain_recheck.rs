use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::cloudflare_email::EmailDomainVerifier;
use crate::lifecycle::{JvmTaskSchedules, ScheduledTask};
use crate::postgres_pool::{PostgresClientSource, PostgresPool, PostgresPoolError};

/// One re-checked sender domain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderDomainRecheckOutcome {
    pub app_id: Uuid,
    pub domain: String,
    pub verified: bool,
    pub error: Option<String>,
}

#[derive(Debug, Error)]
pub enum SenderDomainRecheckError {
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
    #[error(transparent)]
    DatabasePool(#[from] PostgresPoolError),
}

/// Daily re-check of every stored sender domain against Cloudflare. A domain
/// whose listing is disabled loses its `verified_at`, which makes sends fall
/// back to the platform sender.
pub struct PostgresSenderDomainRecheck {
    client: PostgresClientSource,
    verifier: Arc<dyn EmailDomainVerifier>,
}

impl PostgresSenderDomainRecheck {
    #[must_use]
    pub fn new(client: Client, verifier: Arc<dyn EmailDomainVerifier>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            verifier,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, verifier: Arc<dyn EmailDomainVerifier>) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            verifier,
        }
    }

    /// Re-checks each stored domain and records the result.
    ///
    /// # Errors
    ///
    /// Returns database or pool failures. Cloudflare lookup failures are
    /// recorded on the row and reported in the outcome instead.
    pub async fn run(&self) -> Result<Vec<SenderDomainRecheckOutcome>, SenderDomainRecheckError> {
        let client = self.client.acquire().await?;
        let rows = client
            .query(
                "SELECT app_id, domain FROM app_sender_domains ORDER BY app_id, domain",
                &[],
            )
            .await?;
        drop(client);
        let mut outcomes = Vec::with_capacity(rows.len());
        for row in rows {
            let app_id: Uuid = row.get(0);
            let domain: String = row.get(1);
            let outcome = match self.verifier.verify_domain(&domain).await {
                Ok(status) => {
                    let client = self.client.acquire().await?;
                    client
                        .execute(
                            "UPDATE app_sender_domains
                             SET cloudflare_zone_id = $3,
                                 verified_at = CASE WHEN $4
                                   AND ownership_verified_at IS NOT NULL
                                   THEN now() ELSE NULL END,
                                 last_checked_at = now(),
                                 last_error = NULL
                             WHERE app_id = $1 AND domain = $2",
                            &[&app_id, &domain, &status.zone_id, &status.enabled],
                        )
                        .await?;
                    SenderDomainRecheckOutcome {
                        app_id,
                        domain,
                        verified: status.enabled,
                        error: None,
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    let client = self.client.acquire().await?;
                    client
                        .execute(
                            "UPDATE app_sender_domains
                             SET last_checked_at = now(), last_error = $3
                             WHERE app_id = $1 AND domain = $2",
                            &[&app_id, &domain, &message],
                        )
                        .await?;
                    SenderDomainRecheckOutcome {
                        app_id,
                        domain,
                        verified: false,
                        error: Some(message),
                    }
                }
            };
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    #[must_use]
    pub fn scheduled_task(self: &Arc<Self>) -> ScheduledTask {
        let worker = Arc::clone(self);
        ScheduledTask::new(
            "sender-domain-recheck",
            JvmTaskSchedules::SENDER_DOMAIN_RECHECK,
            Duration::from_secs(300),
            move || {
                let worker = Arc::clone(&worker);
                async move { worker.run().await.map(drop) }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recheck_runs_on_the_daily_schedule() {
        assert_eq!(
            JvmTaskSchedules::SENDER_DOMAIN_RECHECK,
            crate::lifecycle::TaskSchedule::Daily {
                timezone: chrono_tz::US::Pacific,
                hour: 3,
                minute: 0,
            }
        );
    }
}
