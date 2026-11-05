use std::time::Duration;

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool, SseRetryIntervalFuture, SseRetryIntervalPolicy};

const DEFAULT_RETRY_INTERVAL_MS: u64 = 500;

pub struct PostgresSseRetryIntervalPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
}

impl PostgresSseRetryIntervalPolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }
}

impl SseRetryIntervalPolicy for PostgresSseRetryIntervalPolicy {
    fn retry_interval(&self) -> SseRetryIntervalFuture<'_> {
        Box::pin(async move {
            let client = self.client.acquire().await.map_err(database_error)?;
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
                       AND flag_setting.value = '\"sse-retry-interval-ms\"'::jsonb
                     LIMIT 1",
                    &[&self.config_app_id],
                )
                .await
                .map_err(database_error)?
                .map(|row| row.get::<_, Value>(0));
            parse_retry_interval(value.as_ref())
        })
    }
}

fn parse_retry_interval(value: Option<&Value>) -> Result<Duration, ApiError> {
    let milliseconds = match value {
        None => DEFAULT_RETRY_INTERVAL_MS,
        Some(value) => value.as_u64().ok_or_else(|| {
            ApiError::new(
                ApiErrorType::Unknown,
                "config flag `sse-retry-interval-ms` must be a non-negative integer",
            )
        })?,
    };
    Ok(Duration::from_millis(milliseconds))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn retry_interval_preserves_default_override_and_validation() {
        assert_eq!(
            parse_retry_interval(None).unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            parse_retry_interval(Some(&json!(1250))).unwrap(),
            Duration::from_millis(1250)
        );
        for invalid in [json!(-1), json!(1.5), json!("500"), Value::Null] {
            assert!(parse_retry_interval(Some(&invalid)).is_err());
        }
    }
}
