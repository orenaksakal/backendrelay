use serde_json::Value;
use std::sync::Arc;
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{CoarseTopicsFuture, CoarseTopicsPolicy, INSTANT_CONFIG_APP_ID, PostgresPool};

/// Reads `coarse-topics-apps`, optionally sharing production's WAL snapshot.
pub struct PostgresCoarseTopicsPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
    controls: Option<Arc<crate::QueryControlCache>>,
}

impl PostgresCoarseTopicsPolicy {
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

impl CoarseTopicsPolicy for PostgresCoarseTopicsPolicy {
    fn use_coarse_topics(&self, app_id: Uuid) -> CoarseTopicsFuture<'_> {
        Box::pin(async move {
            if let Some(controls) = &self.controls {
                let flags = controls.runtime_flags().await?;
                return Ok(flag_contains_app(flags.get("coarse-topics-apps"), app_id));
            }
            let client = self.client.acquire().await.map_err(database_error)?;
            let row = client
                .query_typed_cached_opt(
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
                       AND flag_setting.value #>> '{}' = 'coarse-topics-apps'",
                    &[(&self.config_app_id, Type::UUID)],
                )
                .await
                .map_err(database_error)?;
            Ok(row.is_some_and(|row| flag_contains_app(Some(&row.get::<_, Value>(0)), app_id)))
        })
    }
}

pub(crate) fn flag_contains_app(value: Option<&Value>, app_id: Uuid) -> bool {
    value.and_then(Value::as_array).is_some_and(|apps| {
        apps.iter()
            .any(|value| value.as_str().and_then(|id| Uuid::parse_str(id).ok()) == Some(app_id))
    })
}
