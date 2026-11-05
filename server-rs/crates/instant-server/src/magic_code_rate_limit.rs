use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use instant_http::{ApiError, ApiErrorType};
use instant_permissions::{
    RateLimit, RateLimitBucketConfig, RateLimitConsumer, RateLimitDecision, RateLimitRequest,
    RefillType,
};
use serde_json::{Value, json};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::metrics::{self, EmailOutcome};
use crate::postgres_pool::PostgresClientSource;
use crate::{
    EmailProvider, INSTANT_CONFIG_APP_ID, MagicCodeRateLimitFuture, MagicCodeRateLimiter,
    MagicCodeVerifyFailureFuture, PostgresPool,
};

const DEFAULT_CAPACITY: u64 = 20;
const DEFAULT_DAILY_LIMIT_PER_APP: u64 = 1_000;
const DEFAULT_IP_HOURLY_LIMIT: u64 = 30;
const DEFAULT_VERIFY_IP_HOURLY_LIMIT: u64 = 60;
const DEFAULT_VERIFY_FAILURE_LIMIT: u64 = 5;
const REFILL_PERIOD: Duration = Duration::from_hours(1);
const DAILY_PERIOD: Duration = Duration::from_hours(24);
const VERIFY_FAILURE_PERIOD: Duration = Duration::from_mins(15);
const MAX_LOCAL_BUCKETS: usize = 100_000;
const DAILY_LIMIT_ENV: &str = "INSTANT_MAGIC_CODE_DAILY_LIMIT_PER_APP";
const IP_HOURLY_LIMIT_ENV: &str = "INSTANT_MAGIC_CODE_RATE_LIMIT_PER_IP_HOUR";
const VERIFY_IP_HOURLY_LIMIT_ENV: &str = "INSTANT_MAGIC_CODE_VERIFY_LIMIT_PER_IP_HOUR";
const RATE_LIMIT_MESSAGE: &str =
    "Too many verification codes requested for this email. Please try again later.";
static IP_QUOTA_DISABLED_WARNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
struct LocalBucket {
    count: u64,
    written_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct MagicCodeLimits {
    capacity: u64,
    daily_limit_per_app: u64,
    ip_hourly_limit: u64,
    verify_ip_hourly_limit: u64,
    durable: bool,
}

/// Runtime magic-code limiter matching the JVM's live flags and independent
/// create/consume hourly buckets, plus per-app daily and per-IP hourly caps.
#[derive(Debug)]
pub struct PostgresMagicCodeRateLimiter {
    client: PostgresClientSource,
    consumer: Arc<dyn RateLimitConsumer>,
    local_send_buckets: StdMutex<HashMap<(Uuid, String), LocalBucket>>,
    local_daily_buckets: StdMutex<HashMap<Uuid, LocalBucket>>,
    local_verify_failure_buckets: StdMutex<HashMap<(Uuid, String), LocalBucket>>,
    config_app_id: Uuid,
    daily_limit_per_app: u64,
    ip_hourly_limit: u64,
    verify_ip_hourly_limit: u64,
    email_provider: EmailProvider,
}

impl PostgresMagicCodeRateLimiter {
    #[must_use]
    pub fn new(client: Client, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        Self::from_source(PostgresClientSource::fixed(client), consumer)
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        Self::from_source(PostgresClientSource::pooled(pool), consumer)
    }

    fn from_source(client: PostgresClientSource, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        Self {
            client,
            consumer,
            local_send_buckets: StdMutex::new(HashMap::new()),
            local_daily_buckets: StdMutex::new(HashMap::new()),
            local_verify_failure_buckets: StdMutex::new(HashMap::new()),
            config_app_id: INSTANT_CONFIG_APP_ID,
            daily_limit_per_app: env_limit(DAILY_LIMIT_ENV, DEFAULT_DAILY_LIMIT_PER_APP),
            ip_hourly_limit: env_limit(IP_HOURLY_LIMIT_ENV, DEFAULT_IP_HOURLY_LIMIT),
            verify_ip_hourly_limit: env_limit(
                VERIFY_IP_HOURLY_LIMIT_ENV,
                DEFAULT_VERIFY_IP_HOURLY_LIMIT,
            ),
            email_provider: EmailProvider::Auto,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }

    #[must_use]
    pub const fn with_daily_limit_per_app(mut self, limit: u64) -> Self {
        self.daily_limit_per_app = limit;
        self
    }

    #[must_use]
    pub const fn with_ip_hourly_limit(mut self, limit: u64) -> Self {
        self.ip_hourly_limit = limit;
        self
    }

    #[must_use]
    pub const fn with_verify_ip_hourly_limit(mut self, limit: u64) -> Self {
        self.verify_ip_hourly_limit = limit;
        self
    }

    /// Selects the provider label used by the outbound email metric.
    #[must_use]
    pub const fn with_email_provider(mut self, provider: EmailProvider) -> Self {
        self.email_provider = provider;
        self
    }

    async fn configuration(&self) -> Result<MagicCodeLimits, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let flags = client
            .query(
                "SELECT flag_setting.value #>> '{}', flag_value.value
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
                   AND flag_setting.value IN (
                     to_jsonb('magic-code-rate-limit-per-hour'::text),
                     to_jsonb('magic-code-daily-limit-per-app'::text),
                     to_jsonb('magic-code-rate-limit-per-ip-hour'::text),
                     to_jsonb('magic-code-verify-limit-per-ip-hour'::text)
                   )",
                &[&self.config_app_id],
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, Value>(1)))
            .collect::<HashMap<_, _>>();
        let capacity = flags
            .get("magic-code-rate-limit-per-hour")
            .and_then(positive_u64)
            .unwrap_or(DEFAULT_CAPACITY);
        let daily_limit_per_app = flags
            .get("magic-code-daily-limit-per-app")
            .and_then(positive_u64)
            .unwrap_or(self.daily_limit_per_app);
        let ip_hourly_limit = flags
            .get("magic-code-rate-limit-per-ip-hour")
            .and_then(positive_u64)
            .unwrap_or(self.ip_hourly_limit);
        let verify_ip_hourly_limit = flags
            .get("magic-code-verify-limit-per-ip-hour")
            .and_then(positive_u64)
            .unwrap_or(self.verify_ip_hourly_limit);
        let toggle = client
            .query_opt(
                "SELECT toggle_value.value
                 FROM attrs setting_attr
                 JOIN triples toggle_setting
                   ON toggle_setting.app_id = $1 AND toggle_setting.attr_id = setting_attr.id
                 JOIN attrs toggled_attr
                   ON toggled_attr.app_id = $1 AND toggled_attr.etype = 'toggles'
                  AND toggled_attr.label = 'toggled'
                 JOIN triples toggle_value
                   ON toggle_value.app_id = $1
                  AND toggle_value.entity_id = toggle_setting.entity_id
                  AND toggle_value.attr_id = toggled_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'toggles'
                   AND setting_attr.label = 'setting'
                   AND toggle_setting.value = to_jsonb('use-bucket4j'::text)
                 LIMIT 1",
                &[&self.config_app_id],
            )
            .await
            .map_err(database_error)?
            .map(|row| row.get::<_, Value>(0));
        let durable = toggle.as_ref().is_none_or(toggle_truthy);
        Ok(MagicCodeLimits {
            capacity,
            daily_limit_per_app,
            ip_hourly_limit,
            verify_ip_hourly_limit,
            durable,
        })
    }

    fn request(
        action: &str,
        app_id: Uuid,
        key: Value,
        capacity: u64,
        period: Duration,
    ) -> RateLimitRequest {
        RateLimitRequest {
            app_id,
            bucket_name: action.to_owned(),
            config: RateLimitBucketConfig {
                limits: vec![RateLimit {
                    capacity,
                    refill_amount: capacity,
                    refill_period: period,
                    refill_type: RefillType::Interval,
                }],
            },
            bucket_key: key,
            tokens: 1,
        }
    }

    fn consume_durable(
        &self,
        action: &str,
        app_id: Uuid,
        key: Value,
        capacity: u64,
        period: Duration,
    ) -> Result<(), ApiError> {
        let request = Self::request(action, app_id, key, capacity, period);
        match self.consumer.consume(&request).map_err(rate_limit_error)? {
            RateLimitDecision::Allowed { .. } => Ok(()),
            RateLimitDecision::Denied { .. } => Err(self.rate_limited()),
        }
    }

    fn bump_local<K>(buckets: &StdMutex<HashMap<K, LocalBucket>>, key: K, window: Duration) -> u64
    where
        K: Eq + Hash + Clone,
    {
        let now = Instant::now();
        let mut buckets = buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buckets.retain(|_, bucket| now.duration_since(bucket.written_at) < window);
        let oldest = if !buckets.contains_key(&key) && buckets.len() >= MAX_LOCAL_BUCKETS {
            buckets
                .iter()
                .min_by_key(|(_, bucket)| bucket.written_at)
                .map(|(key, _)| key.clone())
        } else {
            None
        };
        if let Some(oldest) = oldest {
            buckets.remove(&oldest);
        }
        let bucket = buckets.entry(key).or_insert(LocalBucket {
            count: 0,
            written_at: now,
        });
        bucket.count = bucket.count.saturating_add(1);
        bucket.count
    }

    fn consume_local<K>(
        &self,
        buckets: &StdMutex<HashMap<K, LocalBucket>>,
        key: K,
        capacity: u64,
        window: Duration,
    ) -> Result<(), ApiError>
    where
        K: Eq + Hash + Clone,
    {
        if Self::bump_local(buckets, key, window) > capacity {
            Err(self.rate_limited())
        } else {
            Ok(())
        }
    }

    async fn check(&self, action: &str, app_id: Uuid, email: &str) -> Result<(), ApiError> {
        let normalized_email = email.trim().to_ascii_lowercase();
        let limits = self.configuration().await?;
        if limits.durable {
            self.consume_durable(
                action,
                app_id,
                json!(normalized_email),
                limits.capacity,
                REFILL_PERIOD,
            )
        } else if action == "create" {
            self.consume_local(
                &self.local_send_buckets,
                (app_id, normalized_email),
                limits.capacity,
                REFILL_PERIOD,
            )
        } else {
            Ok(())
        }
    }

    async fn check_send_from_ip(
        &self,
        app_id: Uuid,
        email: &str,
        client_ip: Option<&str>,
    ) -> Result<(), ApiError> {
        let normalized_email = email.trim().to_ascii_lowercase();
        let client_ip = ip_quota_key(client_ip);
        let limits = self.configuration().await?;
        if limits.durable {
            let mut requests = vec![
                Self::request(
                    "create",
                    app_id,
                    json!(normalized_email),
                    limits.capacity,
                    REFILL_PERIOD,
                ),
                Self::request(
                    "create-daily",
                    app_id,
                    json!("daily"),
                    limits.daily_limit_per_app,
                    DAILY_PERIOD,
                ),
            ];
            if let Some(client_ip) = client_ip {
                requests.push(Self::request(
                    "create-ip",
                    app_id,
                    json!(client_ip),
                    limits.ip_hourly_limit,
                    REFILL_PERIOD,
                ));
            }
            match self.consumer.consume_all(&requests) {
                Ok(decisions)
                    if decisions
                        .iter()
                        .all(|decision| matches!(decision, RateLimitDecision::Allowed { .. })) =>
                {
                    Ok(())
                }
                Ok(_) => Err(self.rate_limited()),
                Err(error) => Err(rate_limit_error(error)),
            }
        } else {
            self.consume_local(
                &self.local_send_buckets,
                (app_id, normalized_email),
                limits.capacity,
                REFILL_PERIOD,
            )?;
            self.consume_local(
                &self.local_daily_buckets,
                app_id,
                limits.daily_limit_per_app,
                DAILY_PERIOD,
            )?;
            if let Some(client_ip) = client_ip {
                self.consume_local(
                    &self.local_send_buckets,
                    (app_id, format!("ip:{client_ip}")),
                    limits.ip_hourly_limit,
                    REFILL_PERIOD,
                )?;
            }
            Ok(())
        }
    }

    async fn check_verify_from_ip(
        &self,
        app_id: Uuid,
        email: &str,
        client_ip: Option<&str>,
    ) -> Result<(), ApiError> {
        self.check("consume", app_id, email).await?;
        let Some(client_ip) = ip_quota_key(client_ip) else {
            return Ok(());
        };
        let limits = self.configuration().await?;
        if limits.durable {
            self.consume_durable(
                "consume-ip",
                app_id,
                json!(client_ip),
                limits.verify_ip_hourly_limit,
                REFILL_PERIOD,
            )
        } else {
            self.consume_local(
                &self.local_send_buckets,
                (app_id, format!("verify-ip:{client_ip}")),
                limits.verify_ip_hourly_limit,
                REFILL_PERIOD,
            )
        }
    }

    async fn record_verify_failure(&self, app_id: Uuid, email: &str) -> Result<bool, ApiError> {
        let normalized_email = email.trim().to_ascii_lowercase();
        let limits = self.configuration().await?;
        if limits.durable {
            match self
                .consumer
                .consume(&Self::request(
                    "verify-failure",
                    app_id,
                    json!(normalized_email),
                    DEFAULT_VERIFY_FAILURE_LIMIT,
                    VERIFY_FAILURE_PERIOD,
                ))
                .map_err(rate_limit_error)?
            {
                RateLimitDecision::Allowed { remaining_tokens } => Ok(remaining_tokens == 0),
                RateLimitDecision::Denied { .. } => Ok(true),
            }
        } else {
            let count = Self::bump_local(
                &self.local_verify_failure_buckets,
                (app_id, normalized_email),
                VERIFY_FAILURE_PERIOD,
            );
            Ok(count >= DEFAULT_VERIFY_FAILURE_LIMIT)
        }
    }

    fn reset_verify_failures(&self, app_id: Uuid, email: &str) {
        let normalized_email = email.trim().to_ascii_lowercase();
        self.local_verify_failure_buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(app_id, normalized_email.clone()));
        if let Err(error) = self.consumer.reset(&Self::request(
            "verify-failure",
            app_id,
            json!(normalized_email),
            DEFAULT_VERIFY_FAILURE_LIMIT,
            VERIFY_FAILURE_PERIOD,
        )) {
            tracing::warn!(%error, "unable to clear the magic-code verification failure bucket");
        }
    }

    fn rate_limited(&self) -> ApiError {
        metrics::record_email(self.email_provider, EmailOutcome::RateLimited);
        rate_limited()
    }
}

impl MagicCodeRateLimiter for PostgresMagicCodeRateLimiter {
    fn check_send<'a>(&'a self, app_id: Uuid, email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(self.check_send_from_ip(app_id, email, None))
    }

    fn check_send_from_ip<'a>(
        &'a self,
        app_id: Uuid,
        email: &'a str,
        client_ip: Option<&'a str>,
    ) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(PostgresMagicCodeRateLimiter::check_send_from_ip(
            self, app_id, email, client_ip,
        ))
    }

    fn check_verify<'a>(&'a self, app_id: Uuid, email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(self.check("consume", app_id, email))
    }

    fn check_verify_from_ip<'a>(
        &'a self,
        app_id: Uuid,
        email: &'a str,
        client_ip: Option<&'a str>,
    ) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(PostgresMagicCodeRateLimiter::check_verify_from_ip(
            self, app_id, email, client_ip,
        ))
    }

    fn record_verify_failure<'a>(
        &'a self,
        app_id: Uuid,
        email: &'a str,
    ) -> MagicCodeVerifyFailureFuture<'a> {
        Box::pin(PostgresMagicCodeRateLimiter::record_verify_failure(
            self, app_id, email,
        ))
    }

    fn reset_verify_failures<'a>(
        &'a self,
        app_id: Uuid,
        email: &'a str,
    ) -> MagicCodeRateLimitFuture<'a> {
        self.reset_verify_failures(app_id, email);
        Box::pin(std::future::ready(Ok(())))
    }

    fn check_custom_sender<'a>(&'a self, email: &'a str) -> MagicCodeRateLimitFuture<'a> {
        Box::pin(self.check("custom-sender-verify", Uuid::nil(), email))
    }
}

/// Returns the per-IP quota key, or `None` when the address cannot represent a
/// distinct public client. Private addresses usually mean the proxy network was
/// not configured, so limiting them would throttle every user behind it.
fn ip_quota_key(client_ip: Option<&str>) -> Option<&str> {
    let client_ip = client_ip?;
    let Ok(address) = client_ip.parse::<IpAddr>() else {
        return Some(client_ip);
    };
    if !is_internal_address(address) {
        return Some(client_ip);
    }
    if !IP_QUOTA_DISABLED_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            "per-IP magic-code quota is inactive because requests arrive from a private \
             address; set INSTANT_TRUSTED_PROXY_CIDRS to the proxy network"
        );
    }
    None
}

pub(crate) fn is_internal_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_loopback() || address.is_private() || address.is_link_local()
        }
        IpAddr::V6(address) => {
            address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
        }
    }
}

fn env_limit(variable: &str, default: u64) -> u64 {
    std::env::var(variable)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn positive_u64(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

fn toggle_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn rate_limited() -> ApiError {
    ApiError::new(ApiErrorType::RateLimited, RATE_LIMIT_MESSAGE)
}

fn rate_limit_error(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::log_database_error(&error.to_string());
    ApiError::new(ApiErrorType::Unknown, "Unable to enforce rate limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_store_errors_hide_the_backend_cause() {
        let error = rate_limit_error("relation \"secret_buckets\" does not exist");

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Unable to enforce rate limit");
        assert!(!error.body().message.contains("secret_buckets"));
    }

    #[test]
    fn capacity_only_accepts_positive_integers() {
        assert_eq!(positive_u64(&json!(21)), Some(21));
        assert_eq!(positive_u64(&json!("22")), None);
        assert_eq!(positive_u64(&json!(0)), None);
        assert_eq!(positive_u64(&json!(-1)), None);
        assert_eq!(positive_u64(&json!(2.5)), None);
    }

    #[test]
    fn toggle_truthiness_matches_clojure() {
        assert!(!toggle_truthy(&Value::Null));
        assert!(!toggle_truthy(&Value::Bool(false)));
        assert!(toggle_truthy(&Value::Bool(true)));
        assert!(toggle_truthy(&json!(0)));
    }

    #[test]
    fn private_client_addresses_skip_the_ip_quota() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.4.5",
            "192.168.0.8",
            "169.254.1.1",
            "::1",
            "fd00::1",
            "fe80::1",
        ] {
            assert_eq!(ip_quota_key(Some(private)), None, "{private}");
        }
        assert_eq!(ip_quota_key(None), None);
    }

    #[test]
    fn public_client_addresses_keep_the_ip_quota() {
        assert_eq!(ip_quota_key(Some("203.0.113.9")), Some("203.0.113.9"));
        assert_eq!(ip_quota_key(Some("2001:db8::7")), Some("2001:db8::7"));
    }
}
