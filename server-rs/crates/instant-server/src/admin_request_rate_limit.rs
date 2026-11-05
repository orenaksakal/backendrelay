use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use instant_http::{ApiError, ApiErrorType};
use instant_permissions::{
    RateLimitBucketConfig, RateLimitConsumer, RateLimitDecision, RateLimitRequest,
};
use serde_json::{Value, json};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool};

const PER_APP_FLAG: &str = "admin-rate-limit-for-app";
const GLOBAL_FLAG: &str = "global-admin-rate-limit";

pub type AdminRequestRateLimitFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminRequestOperation {
    Query,
    Transact,
}

impl AdminRequestOperation {
    const fn bucket_name(self) -> &'static str {
        match self {
            Self::Query => "__instant-admin-query",
            Self::Transact => "__instant-admin-transact",
        }
    }
}

pub trait AdminRequestRateLimitPolicy: Send + Sync + 'static {
    fn check(
        &self,
        app_id: Uuid,
        operation: AdminRequestOperation,
    ) -> AdminRequestRateLimitFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllAdminRequestRateLimits;

impl AdminRequestRateLimitPolicy for AllowAllAdminRequestRateLimits {
    fn check(
        &self,
        _app_id: Uuid,
        _operation: AdminRequestOperation,
    ) -> AdminRequestRateLimitFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Loads the current per-app/global admin request policy before every check and
/// consumes through the shared durable permission bucket store.
pub struct PostgresAdminRequestRateLimitPolicy {
    client: PostgresClientSource,
    consumer: Arc<dyn RateLimitConsumer>,
    config_app_id: Uuid,
}

impl PostgresAdminRequestRateLimitPolicy {
    #[must_use]
    pub fn new(client: Client, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            consumer,
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            consumer,
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }

    async fn configuration(
        &self,
        app_id: Uuid,
    ) -> Result<Option<(RateLimitBucketConfig, Value)>, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let settings = vec![PER_APP_FLAG, GLOBAL_FLAG];
        let rows = client
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
                   AND flag_setting.value #>> '{}' = ANY($2)",
                &[&self.config_app_id, &settings],
            )
            .await
            .map_err(database_error)?;
        let flags = rows
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, Value>(1)))
            .collect::<HashMap<_, _>>();
        let Some(config) = select_configuration(&flags, app_id).cloned() else {
            return Ok(None);
        };
        parse_configuration(&config).map(|parsed| Some((parsed, config.clone())))
    }
}

impl AdminRequestRateLimitPolicy for PostgresAdminRequestRateLimitPolicy {
    fn check(
        &self,
        app_id: Uuid,
        operation: AdminRequestOperation,
    ) -> AdminRequestRateLimitFuture<'_> {
        Box::pin(async move {
            let Some((config, config_key)) = self.configuration(app_id).await? else {
                return Ok(());
            };
            consume(
                self.consumer.as_ref(),
                app_id,
                operation.bucket_name(),
                config,
                &config_key,
            )
        })
    }
}

fn select_configuration(flags: &HashMap<String, Value>, app_id: Uuid) -> Option<&Value> {
    flags
        .get(PER_APP_FLAG)
        .and_then(Value::as_object)
        .and_then(|configs| configs.get(&app_id.to_string()))
        .filter(|value| clojure_truthy(value))
        .or_else(|| flags.get(GLOBAL_FLAG).filter(|value| clojure_truthy(value)))
}

fn consume(
    consumer: &dyn RateLimitConsumer,
    app_id: Uuid,
    bucket_name: &str,
    config: RateLimitBucketConfig,
    config_key: &Value,
) -> Result<(), ApiError> {
    let request = RateLimitRequest {
        app_id,
        bucket_name: bucket_name.to_owned(),
        config,
        // The JVM hashes the unnormalized config into the bucket identity, so
        // any operator edit starts a fresh bucket even when defaults normalize
        // to the same parsed bandwidths.
        bucket_key: json!([app_id, config_key]),
        tokens: 1,
    };
    match consumer.consume(&request).map_err(rate_limit_store_error)? {
        RateLimitDecision::Allowed { .. } => Ok(()),
        RateLimitDecision::Denied {
            retry_after,
            remaining_tokens,
        } => Err(rate_limited(retry_after, remaining_tokens)),
    }
}

fn clojure_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

fn parse_configuration(config: &Value) -> Result<RateLimitBucketConfig, ApiError> {
    let Some(limits_value) = config
        .as_object()
        .and_then(|config| config.get("limits"))
        .filter(|value| clojure_truthy(value))
    else {
        return Err(missing_parameter(&["limits"]));
    };
    let Some(limits) = limits_value.as_array().filter(|limits| !limits.is_empty()) else {
        return Err(validation_error(
            config,
            "The rate limit config must have at least one limit in the `limits` array.",
        ));
    };
    for limit in limits {
        let Some(limit) = limit.as_object() else {
            return Err(missing_parameter(&["capacity"]));
        };
        required_positive_integer(limit.get("capacity"), &["capacity"])?;
        let Some(refill) = limit.get("refill") else {
            continue;
        };
        if refill.is_null() {
            continue;
        }
        let Some(refill) = refill.as_object() else {
            return Err(malformed_parameter(&["refill"], refill));
        };
        if let Some(amount) = refill.get("amount") {
            required_positive_integer(Some(amount), &["refill", "amount"])?;
        }
        if let Some(refill_type) = refill.get("type") {
            if !clojure_truthy(refill_type) {
                return Err(missing_parameter(&["refill", "type"]));
            }
            if !matches!(refill_type.as_str(), Some("greedy" | "interval")) {
                return Err(malformed_parameter(&["refill", "type"], refill_type));
            }
        }
        if let Some(period) = refill.get("period") {
            if !clojure_truthy(period) {
                return Err(missing_parameter(&["refill", "period"]));
            }
            let Some(period_string) = period.as_str() else {
                return Err(malformed_parameter(&["refill", "period"], period));
            };
            let duration = instant_permissions::parse_duration(period_string)
                .map_err(|_| malformed_parameter(&["refill", "period"], period))?;
            if duration.as_secs() == 0 {
                return Err(validation_error(
                    config,
                    "The refill period must be longer than a second.",
                ));
            }
            if duration.as_secs() / 3_600 > 24 {
                return Err(validation_error(
                    config,
                    "The refill period can't be longer than a day.",
                ));
            }
        }
    }
    let mut normalized = config.clone();
    for limit in normalized["limits"].as_array_mut().into_iter().flatten() {
        if let Some(limit) = limit.as_object_mut()
            && limit.get("refill").is_some_and(Value::is_null)
        {
            limit.remove("refill");
        }
    }
    RateLimitBucketConfig::parse(&normalized)
        .map_err(|error| validation_error(config, &error.to_string()))
}

fn required_positive_integer(value: Option<&Value>, path: &[&str]) -> Result<u64, ApiError> {
    let Some(value) = value.filter(|value| clojure_truthy(value)) else {
        return Err(missing_parameter(path));
    };
    value
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| malformed_parameter(path, value))
}

fn parameter_path(path: &[&str]) -> String {
    format!("[{}]", path.join(" "))
}

fn missing_parameter(path: &[&str]) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {}", parameter_path(path)),
    )
    .with_hint(json!({"in": path}))
}

fn malformed_parameter(path: &[&str], value: &Value) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMalformed,
        format!("Malformed parameter: {}", parameter_path(path)),
    )
    .with_hint(json!({"in": path, "original-input": value}))
}

fn validation_error(config: &Value, message: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ValidationFailed,
        format!("Validation failed for rules: {message}"),
    )
    .with_hint(json!({
        "data-type": "rules",
        "input": config,
        "errors": [{"message": message}],
    }))
}

fn rate_limited(retry_after: std::time::Duration, remaining_tokens: u64) -> ApiError {
    let retry_at = chrono::Utc::now()
        .checked_add_signed(
            chrono::Duration::from_std(retry_after).unwrap_or(chrono::Duration::MAX),
        )
        .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
    let retry_after_seconds = u64::try_from(
        retry_at
            .signed_duration_since(chrono::Utc::now())
            .num_milliseconds()
            .max(0),
    )
    .unwrap_or(u64::MAX)
    .saturating_add(999)
        / 1_000;
    ApiError::new(
        ApiErrorType::RateLimited,
        "Your request exceeded the rate limit.",
    )
    .with_hint(json!({
        "retry-at": retry_at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        "retry-after": retry_after_seconds,
        "remaining-tokens": remaining_tokens,
    }))
}

fn rate_limit_store_error(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::log_database_error(&error.to_string());
    ApiError::new(ApiErrorType::Unknown, "Unable to enforce rate limit")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use instant_permissions::{InMemoryRateLimiter, RateLimit, RefillType};

    use super::*;

    fn one_request_config() -> RateLimitBucketConfig {
        RateLimitBucketConfig {
            limits: vec![RateLimit {
                capacity: 1,
                refill_amount: 1,
                refill_period: Duration::from_hours(1),
                refill_type: RefillType::Interval,
            }],
        }
    }

    #[test]
    fn rate_limit_store_errors_hide_the_backend_cause() {
        let error = rate_limit_store_error("relation \"secret_buckets\" does not exist");

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Unable to enforce rate limit");
        assert!(!error.body().message.contains("secret_buckets"));
    }

    #[test]
    fn per_app_config_precedes_global_and_false_falls_back() {
        let app_id = Uuid::new_v4();
        let per_app = json!({"limits": [{"capacity": 1}]});
        let global = json!({"limits": [{"capacity": 2}]});
        let mut flags = HashMap::from([
            (
                PER_APP_FLAG.to_owned(),
                json!({app_id.to_string(): per_app}),
            ),
            (GLOBAL_FLAG.to_owned(), global.clone()),
        ]);
        assert_eq!(select_configuration(&flags, app_id), Some(&per_app));

        flags.insert(PER_APP_FLAG.to_owned(), json!({app_id.to_string(): false}));
        assert_eq!(select_configuration(&flags, app_id), Some(&global));
        flags.insert(GLOBAL_FLAG.to_owned(), Value::Null);
        assert_eq!(select_configuration(&flags, app_id), None);
    }

    #[test]
    fn malformed_configs_preserve_parameter_and_validation_error_families() {
        let iso = parse_configuration(&json!({
            "limits": [{"capacity": 1, "refill": {"period": "PT1H"}}]
        }))
        .unwrap();
        assert_eq!(iso.limits[0].refill_period, Duration::from_hours(1));

        let missing = parse_configuration(&json!({})).unwrap_err();
        assert_eq!(missing.body().kind, ApiErrorType::ParamMissing);
        assert_eq!(missing.body().message, "Missing parameter: [limits]");

        let empty = parse_configuration(&json!({"limits": []})).unwrap_err();
        assert_eq!(empty.body().kind, ApiErrorType::ValidationFailed);
        assert!(empty.body().message.contains("at least one limit"));

        let malformed = parse_configuration(&json!({"limits": [{"capacity": 0}]})).unwrap_err();
        assert_eq!(malformed.body().kind, ApiErrorType::ParamMalformed);
        assert_eq!(malformed.body().message, "Malformed parameter: [capacity]");
    }

    #[test]
    fn query_and_transact_use_independent_per_app_buckets() {
        let consumer = InMemoryRateLimiter::default();
        let app_id = Uuid::new_v4();
        consume(
            &consumer,
            app_id,
            AdminRequestOperation::Query.bucket_name(),
            one_request_config(),
            &json!({"limits": [{"capacity": 1}]}),
        )
        .unwrap();
        let query_error = consume(
            &consumer,
            app_id,
            AdminRequestOperation::Query.bucket_name(),
            one_request_config(),
            &json!({"limits": [{"capacity": 1}]}),
        )
        .unwrap_err();
        assert_eq!(query_error.body().kind, ApiErrorType::RateLimited);
        assert_eq!(
            query_error.body().message,
            "Your request exceeded the rate limit."
        );
        let hint = query_error.body().hint.as_ref().unwrap();
        assert!(hint.get("retry-at").is_some_and(Value::is_string));
        assert_eq!(hint["retry-after"], 3_600);
        assert_eq!(hint["remaining-tokens"], 0);
        assert!(hint.get("retry-after-ms").is_none());

        let boundary = rate_limited(std::time::Duration::from_micros(1_000_500), 0);
        assert_eq!(boundary.body().hint.as_ref().unwrap()["retry-after"], 1);

        consume(
            &consumer,
            app_id,
            AdminRequestOperation::Query.bucket_name(),
            one_request_config(),
            &json!({"limits": [{"capacity": 1}], "revision": 2}),
        )
        .unwrap();

        consume(
            &consumer,
            app_id,
            AdminRequestOperation::Transact.bucket_name(),
            one_request_config(),
            &json!({"limits": [{"capacity": 1}]}),
        )
        .unwrap();
    }
}
