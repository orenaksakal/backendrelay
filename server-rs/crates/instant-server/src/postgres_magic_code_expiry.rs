use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    INSTANT_CONFIG_APP_ID, MAX_MAGIC_CODE_EXPIRY_MINUTES, MagicCodeExpiryFuture,
    MagicCodeExpiryPolicy, PostgresPool,
};

const DEFAULT_MAGIC_CODE_EXPIRY_MINUTES: u64 = 15;

/// Clamps a configured lifetime to the supported maximum. Stored app overrides
/// and config flags can predate the current cap, so values above it are
/// narrowed rather than rejected.
fn clamp_expiry_minutes(minutes: u64) -> u64 {
    minutes.min(MAX_MAGIC_CODE_EXPIRY_MINUTES)
}

/// Resolves an app override and the live config-app fallback used by JVM magic codes.
#[derive(Debug)]
pub struct PostgresMagicCodeExpiryPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
}

impl PostgresMagicCodeExpiryPolicy {
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

impl MagicCodeExpiryPolicy for PostgresMagicCodeExpiryPolicy {
    fn expiry_minutes(&self, app_id: Uuid) -> MagicCodeExpiryFuture<'_> {
        Box::pin(async move {
            let client = self.client.acquire().await.map_err(database_error)?;
            let row = client
                .query_opt(
                    "SELECT app.magic_code_expiry_minutes,
                            (SELECT flag_value.value
                               FROM attrs setting_attr
                               JOIN triples flag_setting
                                 ON flag_setting.app_id = $2
                                AND flag_setting.attr_id = setting_attr.id
                               JOIN attrs value_attr
                                 ON value_attr.app_id = $2 AND value_attr.etype = 'flags'
                                AND value_attr.label = 'value'
                               JOIN triples flag_value
                                 ON flag_value.app_id = $2
                                AND flag_value.entity_id = flag_setting.entity_id
                                AND flag_value.attr_id = value_attr.id
                              WHERE setting_attr.app_id = $2 AND setting_attr.etype = 'flags'
                                AND setting_attr.label = 'setting'
                                AND flag_setting.value #>> '{}' =
                                    'default-magic-code-expiry-minutes'
                              LIMIT 1)
                       FROM apps app
                      WHERE app.id = $1 AND app.deletion_marked_at IS NULL",
                    &[&app_id, &self.config_app_id],
                )
                .await
                .map_err(database_error)?
                .ok_or_else(|| {
                    ApiError::new(ApiErrorType::RecordNotFound, "Record not found: app")
                })?;
            if let Some(minutes) = row.get::<_, Option<i32>>(0) {
                return u64::try_from(minutes)
                    .map(clamp_expiry_minutes)
                    .map_err(|_| invalid_expiry());
            }
            row.get::<_, Option<Value>>(1)
                .map_or(Ok(DEFAULT_MAGIC_CODE_EXPIRY_MINUTES), |value| {
                    value
                        .as_u64()
                        .map(clamp_expiry_minutes)
                        .ok_or_else(invalid_expiry)
                })
        })
    }
}

fn invalid_expiry() -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        "config flag `default-magic-code-expiry-minutes` must be a non-negative integer",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_configured_expiry_are_capped_at_one_hour() {
        assert_eq!(DEFAULT_MAGIC_CODE_EXPIRY_MINUTES, 15);
        assert_eq!(MAX_MAGIC_CODE_EXPIRY_MINUTES, 60);
        assert_eq!(clamp_expiry_minutes(1), 1);
        assert_eq!(clamp_expiry_minutes(15), 15);
        assert_eq!(clamp_expiry_minutes(60), 60);
        assert_eq!(clamp_expiry_minutes(61), 60);
        assert_eq!(clamp_expiry_minutes(1_440), 60);
    }
}
