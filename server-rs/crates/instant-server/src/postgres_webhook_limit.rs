use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool, WebhookLimitFuture, WebhookLimitPolicy};

const DEFAULT_MAXIMUM_ACTIVE_WEBHOOKS: i64 = 100;

/// Reads the live config-app `maximum-active-webhooks` flag without a JVM flag cache.
#[derive(Debug)]
pub struct PostgresWebhookLimitPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
}

impl PostgresWebhookLimitPolicy {
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
            client: PostgresClientSource::pooled_read_only(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }
}

impl WebhookLimitPolicy for PostgresWebhookLimitPolicy {
    fn maximum_active_webhooks(&self) -> WebhookLimitFuture<'_> {
        Box::pin(async move {
            let client = self.client.acquire().await.map_err(database_error)?;
            let row = client
                .query_opt(
                    "SELECT flag_value.value
                       FROM attrs setting_attr
                       JOIN triples flag_setting
                         ON flag_setting.app_id = $1
                        AND flag_setting.attr_id = setting_attr.id
                       JOIN attrs value_attr
                         ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
                        AND value_attr.label = 'value'
                       JOIN triples flag_value
                         ON flag_value.app_id = $1
                        AND flag_value.entity_id = flag_setting.entity_id
                        AND flag_value.attr_id = value_attr.id
                      WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
                        AND setting_attr.label = 'setting'
                        AND flag_setting.value #>> '{}' = 'maximum-active-webhooks'
                      LIMIT 1",
                    &[&self.config_app_id],
                )
                .await
                .map_err(|error| database_error(&error))?;
            row.map_or(Ok(DEFAULT_MAXIMUM_ACTIVE_WEBHOOKS), |row| {
                row.get::<_, Value>(0).as_i64().ok_or_else(invalid_limit)
            })
        })
    }
}

fn invalid_limit() -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        "config flag `maximum-active-webhooks` must be an integer",
    )
}
