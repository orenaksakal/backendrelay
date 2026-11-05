use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures_util::future::join_all;
use instant_http::ApiError;
use instant_wal::Isn;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_postgres::{Client, Transaction};
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::{
    DashboardControlHookOperation, DashboardControlHooks, GuardedWebhookHttpTransport,
    INSTANT_CONFIG_APP_ID, PostgresPool, PostgresPoolError, payload_idempotency_key,
};

const PARTITION_BUCKET_COUNT: i32 = 13;
const MAX_ATTEMPTS: i64 = 11;
const DEFAULT_WORKER_COUNT: usize = 10;
const DEFAULT_RETRY_WORKER_COUNT: usize = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookQueuedEvent {
    pub webhook_id: Uuid,
    pub isn: Isn,
    pub partition_bucket: i32,
    pub app_id: Uuid,
    pub base_time: DateTime<Utc>,
    pub attempt_count: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookDeliveryRequest {
    pub url: String,
    pub idempotency_key: Uuid,
    pub queue_latency: Duration,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookAttempt {
    pub attempted_at: DateTime<Utc>,
    pub duration_millis: i32,
    pub success: bool,
    pub status_code: Option<i32>,
    pub response_text: Option<String>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
}

pub trait WebhookPayloadTokenIssuer: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Creates the scoped, short-lived token included in the delivery envelope.
    ///
    /// # Errors
    ///
    /// Returns a key-management or signing error.
    fn issue(&self, app_id: Uuid, webhook_id: Uuid, isn: Isn) -> Result<String, Self::Error>;
}

pub trait WebhookTransport: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn deliver(
        &self,
        request: WebhookDeliveryRequest,
    ) -> impl Future<Output = Result<WebhookAttempt, Self::Error>> + Send;
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WebhookWorkOutcome {
    pub claimed: usize,
    pub recorded: usize,
    pub may_have_more: bool,
}

#[derive(Debug, Error)]
pub enum WebhookWorkerError {
    #[error("database pool error: {0}")]
    Pool(#[from] PostgresPoolError),
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("stored webhook event has an invalid ISN: {0}")]
    InvalidIsn(String),
    #[error("webhook payload token could not be issued: {0}")]
    Token(String),
    #[error("webhook transport failed before producing an attempt: {0}")]
    Transport(String),
    #[error("active webhook has no sink URL")]
    MissingSinkUrl,
    #[error("webhook delivery envelope could not be encoded: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("config flag `{setting}` must be an integer, found {value}")]
    InvalidWorkerCount { setting: &'static str, value: Value },
}

/// Durable webhook queue processor. Cryptographic token issuance and guarded
/// outbound HTTP are injected so deployments cannot accidentally bypass their
/// key-management or SSRF policy.
pub struct PostgresWebhookWorker<T, I> {
    client: PostgresClientSource,
    transport: Arc<T>,
    token_issuer: Arc<I>,
    server_origin: String,
    machine_id: Uuid,
    config_app_id: Uuid,
}

/// Native dashboard webhook controls. URL validation and resend wakeups are
/// delegated to the exact guarded transport and durable worker used for
/// delivery; config-app WAL logging enablement continues through the composed
/// PostgreSQL policy hook.
pub struct WebhookDashboardControlHooks<D, T, I> {
    inner: D,
    transport: Arc<GuardedWebhookHttpTransport>,
    worker: Arc<PostgresWebhookWorker<T, I>>,
}

impl<D, T, I> WebhookDashboardControlHooks<D, T, I> {
    #[must_use]
    pub const fn new(
        inner: D,
        transport: Arc<GuardedWebhookHttpTransport>,
        worker: Arc<PostgresWebhookWorker<T, I>>,
    ) -> Self {
        Self {
            inner,
            transport,
            worker,
        }
    }
}

impl<D, T, I> DashboardControlHooks for WebhookDashboardControlHooks<D, T, I>
where
    D: DashboardControlHooks,
    T: WebhookTransport + 'static,
    I: WebhookPayloadTokenIssuer + 'static,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::ValidateWebhookSink { url } => {
                Ok(Value::Bool(self.transport.validate_target(&url).await))
            }
            DashboardControlHookOperation::NotifyWebhookEvent { .. } => {
                let worker = Arc::clone(&self.worker);
                std::mem::drop(tokio::spawn(async move {
                    let _durable_queue_recovers_failures = worker.drain_pending().await;
                }));
                Ok(Value::Null)
            }
            other => self.inner.execute(other).await,
        }
    }
}

impl<T, I> PostgresWebhookWorker<T, I>
where
    T: WebhookTransport,
    I: WebhookPayloadTokenIssuer,
{
    #[must_use]
    pub fn new(
        client: Client,
        transport: Arc<T>,
        token_issuer: Arc<I>,
        server_origin: impl Into<String>,
        machine_id: Uuid,
    ) -> Self {
        Self::from_source(
            PostgresClientSource::fixed(client),
            transport,
            token_issuer,
            server_origin,
            machine_id,
        )
    }

    #[must_use]
    pub fn with_shared_client(
        client: Arc<Mutex<Client>>,
        transport: Arc<T>,
        token_issuer: Arc<I>,
        server_origin: impl Into<String>,
        machine_id: Uuid,
    ) -> Self {
        Self::from_source(
            PostgresClientSource::fixed_shared(client),
            transport,
            token_issuer,
            server_origin,
            machine_id,
        )
    }

    #[must_use]
    pub fn from_pool(
        pool: PostgresPool,
        transport: Arc<T>,
        token_issuer: Arc<I>,
        server_origin: impl Into<String>,
        machine_id: Uuid,
    ) -> Self {
        Self::from_source(
            PostgresClientSource::pooled(pool),
            transport,
            token_issuer,
            server_origin,
            machine_id,
        )
    }

    fn from_source(
        client: PostgresClientSource,
        transport: Arc<T>,
        token_issuer: Arc<I>,
        server_origin: impl Into<String>,
        machine_id: Uuid,
    ) -> Self {
        Self {
            client,
            transport,
            token_issuer,
            server_origin: server_origin.into().trim_end_matches('/').to_owned(),
            machine_id,
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }

    /// Claims pending events fairly across at most `max_apps`, taking at most
    /// `max_per_app` oldest rows from each app.
    ///
    /// # Errors
    ///
    /// Returns a database or persisted-ISN decoding error.
    pub async fn claim_pending(
        &self,
        max_apps: u32,
        max_per_app: u32,
    ) -> Result<Vec<WebhookQueuedEvent>, WebhookWorkerError> {
        if max_apps == 0 || max_per_app == 0 {
            return Ok(Vec::new());
        }
        let buckets = recent_partition_buckets(Utc::now()).to_vec();
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        let mut seen = Vec::<Uuid>::new();
        let mut events = Vec::new();
        for _ in 0..max_apps {
            let Some(row) = transaction
                .query_opt(
                    "SELECT app_id FROM webhook_events
                     WHERE status = 'pending'::webhook_event_status
                       AND partition_bucket = ANY($1)
                       AND NOT (app_id = ANY($2))
                     ORDER BY created_at LIMIT 1 FOR UPDATE SKIP LOCKED",
                    &[&buckets, &seen],
                )
                .await?
            else {
                break;
            };
            let app_id: Uuid = row.get(0);
            let claimed =
                claim_pending_for_app(&transaction, self.machine_id, app_id, &buckets, max_per_app)
                    .await?;
            if claimed.is_empty() {
                break;
            }
            seen.push(app_id);
            events.extend(claimed);
        }
        transaction.commit().await?;
        Ok(events)
    }

    /// Claims due retry events oldest-first from the current and previous partitions.
    ///
    /// # Errors
    ///
    /// Returns a database or persisted-ISN decoding error.
    pub async fn claim_retries(
        &self,
        max_events: u32,
        attempt_before: DateTime<Utc>,
    ) -> Result<Vec<WebhookQueuedEvent>, WebhookWorkerError> {
        if max_events == 0 {
            return Ok(Vec::new());
        }
        let buckets = recent_partition_buckets(attempt_before).to_vec();
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        let rows = transaction
            .query(
                "SELECT webhook_id, isn::text, partition_bucket, app_id,
                        next_attempt_after, coalesce(cardinality(attempts), 0)::bigint
                 FROM webhook_events
                 WHERE status = 'error'::webhook_event_status
                   AND next_attempt_after < $1
                   AND partition_bucket = ANY($2)
                 ORDER BY next_attempt_after
                 LIMIT $3 FOR UPDATE SKIP LOCKED",
                &[&attempt_before, &buckets, &i64::from(max_events)],
            )
            .await?;
        let events = decode_event_rows(rows)?;
        for event in &events {
            transaction
                .execute(
                    "UPDATE webhook_events
                     SET status = 'processing'::webhook_event_status,
                         machine_id = $1, next_attempt_after = NULL, updated_at = now()
                     WHERE webhook_id = $2 AND isn = $3::text::isn
                       AND partition_bucket = $4",
                    &[
                        &self.machine_id,
                        &event.webhook_id,
                        &event.isn.to_postgres_composite(),
                        &event.partition_bucket,
                    ],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(events)
    }

    /// Delivers one owned event and durably records its attempt.
    ///
    /// # Errors
    ///
    /// Returns when webhook lookup, token issuance, transport, or ownership-checked
    /// attempt persistence fails.
    #[tracing::instrument(
        name = "send-webhook",
        skip_all,
        fields(app.id = %event.app_id, webhook.id = %event.webhook_id, webhook.isn = %event.isn)
    )]
    pub async fn handle_event(
        &self,
        event: &WebhookQueuedEvent,
    ) -> Result<bool, WebhookWorkerError> {
        let (status, url) = {
            let client = self.client.acquire().await?;
            let row = client
                .query_one(
                    "SELECT status::text, sink ->> 'url' FROM webhooks
                     WHERE app_id = $1 AND id = $2",
                    &[&event.app_id, &event.webhook_id],
                )
                .await?;
            (row.get::<_, String>(0), row.get::<_, Option<String>>(1))
        };
        let attempt = if status == "active" {
            let url = url.ok_or(WebhookWorkerError::MissingSinkUrl)?;
            let token = self
                .token_issuer
                .issue(event.app_id, event.webhook_id, event.isn)
                .map_err(|error| WebhookWorkerError::Token(error.to_string()))?;
            let payload_url = format!(
                "{}/webhooks/payload/{}/{}/{}",
                self.server_origin, event.app_id, event.webhook_id, event.isn
            );
            let body = serde_json::to_vec(&json!({
                "payloadUrl": payload_url,
                "token": token,
            }))?;
            self.transport
                .deliver(WebhookDeliveryRequest {
                    url,
                    idempotency_key: payload_idempotency_key(event.webhook_id, event.isn),
                    queue_latency: Utc::now()
                        .signed_duration_since(event.base_time)
                        .to_std()
                        .unwrap_or_default(),
                    body,
                })
                .await
                .map_err(|error| WebhookWorkerError::Transport(error.to_string()))?
        } else {
            WebhookAttempt {
                attempted_at: Utc::now(),
                duration_millis: 0,
                success: false,
                status_code: None,
                response_text: None,
                error_type: Some("disabled".to_owned()),
                error_message: Some("Webhook is disabled.".to_owned()),
            }
        };
        let delivered = self.record_attempt(event, &attempt).await?;
        crate::metrics::record_webhook_delivery(webhook_delivery_outcome(
            &attempt,
            event.attempt_count,
        ));
        crate::metrics::register_app(event.app_id);
        if !attempt.success {
            crate::metrics::record_app_error(
                event.app_id,
                crate::metrics::AppErrorSurface::Webhook,
            );
        }
        Ok(delivered)
    }

    /// Claims and processes one fair pending-work pass.
    ///
    /// # Errors
    ///
    /// Returns the first claim or delivery failure.
    pub async fn work_pending(
        &self,
        max_apps: u32,
        max_per_app: u32,
    ) -> Result<WebhookWorkOutcome, WebhookWorkerError> {
        let events = self.claim_pending(max_apps, max_per_app).await?;
        self.process_claimed(events, max_apps, max_per_app).await
    }

    /// Drains pending events with the live config-app worker count. Each native
    /// task corresponds to one Clojure queue worker; claimed deliveries inside
    /// a task run concurrently like the reference virtual-thread executor.
    ///
    /// # Errors
    ///
    /// Returns a config, database, claim, or delivery failure.
    pub async fn drain_pending(&self) -> Result<WebhookWorkOutcome, WebhookWorkerError> {
        let worker_count = self
            .worker_count("webhook-worker-count", DEFAULT_WORKER_COUNT)
            .await?;
        self.drain(worker_count, || self.work_pending(10, 10)).await
    }

    /// Claims and processes one retry pass.
    ///
    /// # Errors
    ///
    /// Returns the first claim or delivery failure.
    pub async fn work_retries(
        &self,
        max_events: u32,
        attempt_before: DateTime<Utc>,
    ) -> Result<WebhookWorkOutcome, WebhookWorkerError> {
        let events = self.claim_retries(max_events, attempt_before).await?;
        let mut outcome = WebhookWorkOutcome {
            claimed: events.len(),
            may_have_more: events.len() == usize::try_from(max_events).unwrap_or(usize::MAX),
            ..WebhookWorkOutcome::default()
        };
        outcome.recorded = self.process_events(&events).await?;
        Ok(outcome)
    }

    /// Drains due retries with the live config-app retry-worker count.
    ///
    /// # Errors
    ///
    /// Returns a config, database, claim, or delivery failure.
    pub async fn drain_retries(&self) -> Result<WebhookWorkOutcome, WebhookWorkerError> {
        let worker_count = self
            .worker_count("webhook-retry-worker-count", DEFAULT_RETRY_WORKER_COUNT)
            .await?;
        self.drain(worker_count, || self.work_retries(100, Utc::now()))
            .await
    }

    /// Releases processing rows whose owner has not heartbeated for one minute.
    ///
    /// # Errors
    ///
    /// Returns a database error if leases cannot be reset.
    pub async fn free_stuck_events(&self) -> Result<u64, WebhookWorkerError> {
        let client = self.client.acquire().await?;
        Ok(client
            .execute(
                "UPDATE webhook_events
                 SET status = 'pending'::webhook_event_status,
                     machine_id = NULL, updated_at = now()
                 WHERE status = 'processing'::webhook_event_status
                   AND updated_at < now() - interval '1 minute'",
                &[],
            )
            .await?)
    }

    async fn process_claimed(
        &self,
        events: Vec<WebhookQueuedEvent>,
        max_apps: u32,
        max_per_app: u32,
    ) -> Result<WebhookWorkOutcome, WebhookWorkerError> {
        let distinct_apps = events
            .iter()
            .map(|event| event.app_id)
            .collect::<HashSet<_>>()
            .len();
        let mut app_counts = std::collections::HashMap::<Uuid, usize>::new();
        for event in &events {
            *app_counts.entry(event.app_id).or_default() += 1;
        }
        let mut outcome = WebhookWorkOutcome {
            claimed: events.len(),
            may_have_more: distinct_apps == usize::try_from(max_apps).unwrap_or(usize::MAX)
                || app_counts
                    .values()
                    .any(|count| *count == usize::try_from(max_per_app).unwrap_or(usize::MAX)),
            ..WebhookWorkOutcome::default()
        };
        outcome.recorded = self.process_events(&events).await?;
        Ok(outcome)
    }

    async fn process_events(
        &self,
        events: &[WebhookQueuedEvent],
    ) -> Result<usize, WebhookWorkerError> {
        let results = join_all(events.iter().map(|event| self.handle_event(event))).await;
        results.into_iter().try_fold(0usize, |recorded, result| {
            result.map(|was_recorded| recorded + usize::from(was_recorded))
        })
    }

    async fn drain<F, Fut>(
        &self,
        worker_count: usize,
        work: F,
    ) -> Result<WebhookWorkOutcome, WebhookWorkerError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WebhookWorkOutcome, WebhookWorkerError>>,
    {
        let mut total = WebhookWorkOutcome::default();
        loop {
            let passes = join_all((0..worker_count).map(|_| work())).await;
            let mut repeat = false;
            for pass in passes {
                let pass = pass?;
                total.claimed = total.claimed.saturating_add(pass.claimed);
                total.recorded = total.recorded.saturating_add(pass.recorded);
                repeat |= pass.may_have_more;
            }
            if !repeat {
                return Ok(total);
            }
        }
    }

    async fn worker_count(
        &self,
        setting: &'static str,
        default: usize,
    ) -> Result<usize, WebhookWorkerError> {
        let client = self.client.acquire().await?;
        let value = client
            .query_opt(
                "SELECT flag_value.value
                 FROM attrs setting_attr
                 JOIN triples flag_setting
                   ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
                 JOIN attrs value_attr
                   ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
                  AND value_attr.label = 'value'
                 JOIN triples flag_value
                   ON flag_value.app_id = $1
                  AND flag_value.entity_id = flag_setting.entity_id
                  AND flag_value.attr_id = value_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
                   AND setting_attr.label = 'setting'
                   AND flag_setting.value = to_jsonb($2::text)
                 LIMIT 1",
                &[&self.config_app_id, &setting],
            )
            .await?
            .map(|row| row.get::<_, Value>(0));
        configured_worker_count(setting, value, default)
    }

    async fn record_attempt(
        &self,
        event: &WebhookQueuedEvent,
        attempt: &WebhookAttempt,
    ) -> Result<bool, WebhookWorkerError> {
        let gone = attempt.status_code == Some(410);
        let terminal = gone
            || attempt.error_type.as_deref() == Some("disabled")
            || event.attempt_count.saturating_add(1) >= MAX_ATTEMPTS;
        let status = if attempt.success {
            "success"
        } else if terminal {
            "failed"
        } else {
            "error"
        };
        let next_attempt_after =
            (status == "error").then(|| Utc::now() + retry_delay(event.attempt_count));
        let mut client = self.client.acquire().await?;
        let transaction = client.transaction().await?;
        let updated = transaction
            .execute(
                "UPDATE webhook_events
                 SET status = $1::text::webhook_event_status,
                     attempts = array_append(attempts,
                       ROW($2, $3, $4, $5, $6, $7, $8)::webhook_attempt),
                     machine_id = NULL, next_attempt_after = $9, updated_at = now()
                 WHERE webhook_id = $10 AND isn = $11::text::isn
                   AND partition_bucket = $12 AND machine_id = $13",
                &[
                    &status,
                    &attempt.attempted_at,
                    &attempt.duration_millis,
                    &attempt.success,
                    &attempt.status_code,
                    &attempt.response_text,
                    &attempt.error_type,
                    &attempt.error_message,
                    &next_attempt_after,
                    &event.webhook_id,
                    &event.isn.to_postgres_composite(),
                    &event.partition_bucket,
                    &self.machine_id,
                ],
            )
            .await?;
        if gone && updated > 0 {
            transaction
                .execute(
                    "UPDATE webhooks
                     SET status = 'disabled'::webhook_status,
                         disabled_reason = 'Endpoint returned 410 status code.'
                     WHERE app_id = $1 AND id = $2",
                    &[&event.app_id, &event.webhook_id],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(updated > 0)
    }
}

async fn claim_pending_for_app(
    transaction: &Transaction<'_>,
    machine_id: Uuid,
    app_id: Uuid,
    buckets: &[i32],
    max_per_app: u32,
) -> Result<Vec<WebhookQueuedEvent>, WebhookWorkerError> {
    let rows = transaction
        .query(
            "SELECT webhook_id, isn::text, partition_bucket, app_id, created_at,
                    coalesce(cardinality(attempts), 0)::bigint
             FROM webhook_events
             WHERE app_id = $1 AND partition_bucket = ANY($2)
               AND status = 'pending'::webhook_event_status
             ORDER BY created_at LIMIT $3 FOR UPDATE SKIP LOCKED",
            &[&app_id, &buckets, &i64::from(max_per_app)],
        )
        .await?;
    let events = decode_event_rows(rows)?;
    for event in &events {
        transaction
            .execute(
                "UPDATE webhook_events
                 SET status = 'processing'::webhook_event_status,
                     machine_id = $1, updated_at = now()
                 WHERE webhook_id = $2 AND isn = $3::text::isn
                   AND partition_bucket = $4",
                &[
                    &machine_id,
                    &event.webhook_id,
                    &event.isn.to_postgres_composite(),
                    &event.partition_bucket,
                ],
            )
            .await?;
    }
    Ok(events)
}

fn decode_event_rows(
    rows: Vec<tokio_postgres::Row>,
) -> Result<Vec<WebhookQueuedEvent>, WebhookWorkerError> {
    rows.into_iter()
        .map(|row| {
            let isn_text: String = row.get(1);
            let isn = Isn::from_postgres_composite(&isn_text)
                .map_err(|_| WebhookWorkerError::InvalidIsn(isn_text))?;
            Ok(WebhookQueuedEvent {
                webhook_id: row.get(0),
                isn,
                partition_bucket: row.get(2),
                app_id: row.get(3),
                base_time: row.get(4),
                attempt_count: row.get(5),
            })
        })
        .collect()
}

fn partition_bucket(time: DateTime<Utc>) -> i32 {
    i32::try_from((time.timestamp() / 86_400 / 30).rem_euclid(i64::from(PARTITION_BUCKET_COUNT)))
        .expect("partition bucket is in 0..13")
}

fn recent_partition_buckets(time: DateTime<Utc>) -> [i32; 2] {
    let current = partition_bucket(time);
    [current, (current - 1).rem_euclid(PARTITION_BUCKET_COUNT)]
}

/// Classifies one recorded attempt into the fixed delivery-outcome vocabulary.
/// `failed` means the event will be retried, `retry_exhausted` means this was
/// its final allowed attempt, and `rejected` means the endpoint is gone or the
/// webhook is disabled.
fn webhook_delivery_outcome(
    attempt: &WebhookAttempt,
    attempt_count: i64,
) -> crate::metrics::WebhookDeliveryOutcome {
    use crate::metrics::WebhookDeliveryOutcome;
    if attempt.success {
        WebhookDeliveryOutcome::Delivered
    } else if attempt.status_code == Some(410) || attempt.error_type.as_deref() == Some("disabled")
    {
        WebhookDeliveryOutcome::Rejected
    } else if attempt_count.saturating_add(1) >= MAX_ATTEMPTS {
        WebhookDeliveryOutcome::RetryExhausted
    } else {
        WebhookDeliveryOutcome::Failed
    }
}

fn retry_delay(previous_attempt_count: i64) -> TimeDelta {
    match previous_attempt_count {
        0 => TimeDelta::minutes(1),
        1 => TimeDelta::minutes(5),
        2 => TimeDelta::minutes(30),
        3 => TimeDelta::hours(2),
        4 => TimeDelta::hours(5),
        5 => TimeDelta::hours(10),
        _ => TimeDelta::hours(12),
    }
}

fn configured_worker_count(
    setting: &'static str,
    value: Option<Value>,
    default: usize,
) -> Result<usize, WebhookWorkerError> {
    let Some(value) = value else {
        return Ok(default.max(1));
    };
    let Some(count) = value.as_i64() else {
        return Err(WebhookWorkerError::InvalidWorkerCount { setting, value });
    };
    Ok(usize::try_from(count.max(1)).unwrap_or(usize::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_match_the_reference_three_day_schedule() {
        assert_eq!(retry_delay(0), TimeDelta::minutes(1));
        assert_eq!(retry_delay(1), TimeDelta::minutes(5));
        assert_eq!(retry_delay(2), TimeDelta::minutes(30));
        assert_eq!(retry_delay(3), TimeDelta::hours(2));
        assert_eq!(retry_delay(4), TimeDelta::hours(5));
        assert_eq!(retry_delay(5), TimeDelta::hours(10));
        assert_eq!(retry_delay(6), TimeDelta::hours(12));
        assert_eq!(retry_delay(100), TimeDelta::hours(12));
    }

    #[test]
    fn delivery_outcomes_use_the_fixed_vocabulary() {
        use crate::metrics::WebhookDeliveryOutcome;

        let attempt = |success, status_code, error_type: Option<&str>| WebhookAttempt {
            attempted_at: Utc::now(),
            duration_millis: 0,
            success,
            status_code,
            response_text: None,
            error_type: error_type.map(str::to_owned),
            error_message: None,
        };

        assert_eq!(
            webhook_delivery_outcome(&attempt(true, Some(200), None), 0),
            WebhookDeliveryOutcome::Delivered
        );
        assert_eq!(
            webhook_delivery_outcome(&attempt(false, Some(500), None), 0),
            WebhookDeliveryOutcome::Failed
        );
        assert_eq!(
            webhook_delivery_outcome(&attempt(false, Some(500), None), MAX_ATTEMPTS - 1),
            WebhookDeliveryOutcome::RetryExhausted
        );
        assert_eq!(
            webhook_delivery_outcome(&attempt(false, Some(410), None), 0),
            WebhookDeliveryOutcome::Rejected
        );
        assert_eq!(
            webhook_delivery_outcome(&attempt(false, None, Some("disabled")), 0),
            WebhookDeliveryOutcome::Rejected
        );
    }

    #[test]
    fn partition_buckets_wrap_at_thirteen() {
        let now = Utc::now();
        let [current, previous] = recent_partition_buckets(now);
        assert!((0..13).contains(&current));
        assert_eq!(previous, (current - 1).rem_euclid(13));
    }

    #[test]
    fn worker_count_defaults_and_has_a_floor_of_one() {
        assert_eq!(configured_worker_count("workers", None, 10).unwrap(), 10);
        assert_eq!(
            configured_worker_count("workers", Some(json!(0)), 10).unwrap(),
            1
        );
        assert_eq!(
            configured_worker_count("workers", Some(json!(-20)), 10).unwrap(),
            1
        );
        assert_eq!(
            configured_worker_count("workers", Some(json!(23)), 10).unwrap(),
            23
        );
    }

    #[test]
    fn worker_count_rejects_non_integer_flags() {
        let error = configured_worker_count("workers", Some(json!("many")), 10).unwrap_err();
        assert!(matches!(
            error,
            WebhookWorkerError::InvalidWorkerCount {
                setting: "workers",
                ..
            }
        ));
    }
}
