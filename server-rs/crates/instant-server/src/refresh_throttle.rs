use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool, RefreshThrottleFuture, RefreshThrottlePolicy};

/// Reads refresh throttle settings, optionally using production's WAL snapshot.
pub struct PostgresRefreshThrottlePolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
    controls: Option<Arc<crate::QueryControlCache>>,
}

impl PostgresRefreshThrottlePolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
            controls: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            controls: None,
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self.controls = None;
        self
    }

    pub(crate) fn with_query_control_cache(
        mut self,
        controls: Arc<crate::QueryControlCache>,
    ) -> Self {
        assert!(controls.matches_config_app(self.config_app_id));
        self.controls = Some(controls);
        self
    }
}

impl RefreshThrottlePolicy for PostgresRefreshThrottlePolicy {
    fn delay_after_noop(&self, app_id: Uuid) -> RefreshThrottleFuture<'_> {
        Box::pin(async move {
            if let Some(controls) = &self.controls {
                let flags = controls.runtime_flags().await?;
                return delay_from_flags(app_id, &flags);
            }
            let client = self.client.acquire().await.map_err(database_error)?;
            let rows = client
                .query_typed_cached(
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
                    &[
                        (&self.config_app_id, Type::UUID),
                        (
                            &vec!["refresh-throttled-apps", "refresh-throttle-ms"],
                            Type::TEXT_ARRAY,
                        ),
                    ],
                )
                .await
                .map_err(database_error)?;
            let flags = rows
                .into_iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect();
            delay_from_flags(app_id, &flags)
        })
    }
}

fn delay_from_flags(
    app_id: Uuid,
    flags: &HashMap<String, Value>,
) -> Result<Option<Duration>, ApiError> {
    let throttled = flags
        .get("refresh-throttled-apps")
        .and_then(Value::as_array)
        .is_some_and(|apps| {
            apps.iter()
                .any(|value| value.as_str().and_then(|id| Uuid::parse_str(id).ok()) == Some(app_id))
        });
    if !throttled {
        return Ok(None);
    }
    let delay = flags.get("refresh-throttle-ms").map_or(Ok(1000), |value| {
        value.as_u64().ok_or_else(|| {
            ApiError::new(
                ApiErrorType::Unknown,
                "config flag `refresh-throttle-ms` must be a non-negative integer",
            )
        })
    })?;
    Ok(Some(Duration::from_millis(delay)))
}
