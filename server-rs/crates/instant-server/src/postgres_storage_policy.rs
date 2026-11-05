use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use std::collections::HashMap;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool, ServerEnvironment, StorageAccessPolicy};

/// Production storage block-list policy backed by the config app's
/// `storage-block-list` entities.
pub struct PostgresStorageAccessPolicy {
    client: PostgresClientSource,
    environment: ServerEnvironment,
    config_app_id: Uuid,
}

impl PostgresStorageAccessPolicy {
    #[must_use]
    pub fn new(client: Client, environment: ServerEnvironment) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            environment,
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, environment: ServerEnvironment) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            environment,
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }
}

impl StorageAccessPolicy for PostgresStorageAccessPolicy {
    async fn authorize(&self, app_id: Uuid) -> Result<(), ApiError> {
        if self.environment != ServerEnvironment::Production {
            return Ok(());
        }
        let client = self.client.acquire().await.map_err(database_error)?;
        let blocked = client
            .query_opt(
                "SELECT 1
                 FROM attrs app_attr
                 JOIN triples app_value
                   ON app_value.app_id = $1 AND app_value.attr_id = app_attr.id
                 JOIN attrs disabled_attr
                   ON disabled_attr.app_id = $1
                  AND disabled_attr.etype = 'storage-block-list'
                  AND disabled_attr.label = 'isDisabled'
                 JOIN triples disabled_value
                   ON disabled_value.app_id = $1
                  AND disabled_value.entity_id = app_value.entity_id
                  AND disabled_value.attr_id = disabled_attr.id
                 WHERE app_attr.app_id = $1
                   AND app_attr.etype = 'storage-block-list'
                   AND app_attr.label = 'appId'
                   AND app_value.value = to_jsonb($2::text)
                   AND disabled_value.value = 'true'::jsonb
                 LIMIT 1",
                &[&self.config_app_id, &app_id.to_string()],
            )
            .await
            .map_err(database_error)?
            .is_some();
        if blocked {
            Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                "Storage is disabled for this app",
            ))
        } else {
            Ok(())
        }
    }

    async fn detect_content_type(&self, app_id: Uuid) -> Result<bool, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let value: Option<Value> = client
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
                   AND flag_setting.value = '\"tika-enabled-apps\"'::jsonb
                 LIMIT 1",
                &[&self.config_app_id],
            )
            .await
            .map_err(database_error)?
            .map(|row| row.get(0));
        Ok(value.is_some_and(|value| app_id_is_enabled(&value, app_id)))
    }

    async fn storage_quota_bytes(&self, app_id: Uuid) -> Result<Option<u64>, ApiError> {
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
                     to_jsonb('storage-quota-bytes'::text),
                     to_jsonb('storage-quota-bytes-by-app'::text)
                   )",
                &[&self.config_app_id],
            )
            .await
            .map_err(database_error)?
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, Value>(1)))
            .collect::<HashMap<_, _>>();
        Ok(storage_quota_from_flags(&flags, app_id))
    }
}

/// Picks the per-app quota override, then the deployment default. Unset or
/// non-positive values mean unlimited.
fn storage_quota_from_flags(flags: &HashMap<String, Value>, app_id: Uuid) -> Option<u64> {
    let override_quota = flags
        .get("storage-quota-bytes-by-app")
        .and_then(|value| value.get(app_id.to_string()))
        .and_then(quota_bytes);
    override_quota.or_else(|| flags.get("storage-quota-bytes").and_then(quota_bytes))
}

fn quota_bytes(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|bytes| *bytes > 0)
}

fn app_id_is_enabled(value: &Value, app_id: Uuid) -> bool {
    value.as_array().is_some_and(|values| {
        values.iter().any(|value| {
            value.as_str().and_then(|value| Uuid::parse_str(value).ok()) == Some(app_id)
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn content_type_detection_flag_ignores_invalid_app_ids() {
        let enabled = Uuid::new_v4();
        let value = json!(["invalid", enabled.to_string(), 42]);

        assert!(app_id_is_enabled(&value, enabled));
        assert!(!app_id_is_enabled(&value, Uuid::new_v4()));
        assert!(!app_id_is_enabled(&json!({}), enabled));
    }

    #[test]
    fn storage_quotas_prefer_the_per_app_override_and_ignore_unusable_values() {
        let app_id = Uuid::new_v4();
        let other = Uuid::new_v4();
        let flags = HashMap::from([
            ("storage-quota-bytes".to_owned(), json!(1_000)),
            (
                "storage-quota-bytes-by-app".to_owned(),
                json!({app_id.to_string(): 5_000, other.to_string(): "not-a-number"}),
            ),
        ]);
        assert_eq!(storage_quota_from_flags(&flags, app_id), Some(5_000));
        assert_eq!(storage_quota_from_flags(&flags, other), Some(1_000));

        let string_valued = HashMap::from([("storage-quota-bytes".to_owned(), json!("2048"))]);
        assert_eq!(
            storage_quota_from_flags(&string_valued, app_id),
            Some(2_048)
        );

        for value in [json!(null), json!(0), json!(-5), json!(""), json!({})] {
            let flags = HashMap::from([("storage-quota-bytes".to_owned(), value)]);
            assert_eq!(storage_quota_from_flags(&flags, app_id), None);
        }
        assert_eq!(storage_quota_from_flags(&HashMap::new(), app_id), None);
    }
}
