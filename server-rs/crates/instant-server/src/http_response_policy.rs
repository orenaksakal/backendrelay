use std::time::Duration;

use instant_http::{DynamicHttpResponsePolicy, HttpResponsePolicySnapshot};
use serde_json::Value;
use thiserror::Error;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool, PostgresPoolError, ScheduledTask, TaskSchedule};

#[derive(Debug, Error)]
pub enum HttpResponsePolicyError {
    #[error(transparent)]
    Pool(#[from] PostgresPoolError),
    #[error("database error: {0}")]
    Database(#[from] tokio_postgres::Error),
}

#[derive(Clone)]
pub struct PostgresHttpResponsePolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
    policy: DynamicHttpResponsePolicy,
}

impl PostgresHttpResponsePolicy {
    /// Loads the initial config-app snapshot before the HTTP router is exposed.
    ///
    /// # Errors
    ///
    /// Returns a database error if the authoritative flags cannot be read.
    pub async fn load(client: Client) -> Result<Self, HttpResponsePolicyError> {
        let policy = Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
            policy: DynamicHttpResponsePolicy::default(),
        };
        policy.refresh().await?;
        Ok(policy)
    }

    /// Loads the initial snapshot through a shared native acquisition pool.
    ///
    /// # Errors
    ///
    /// Returns a pool or database error if the authoritative flags cannot be read.
    pub async fn load_pool(pool: PostgresPool) -> Result<Self, HttpResponsePolicyError> {
        let policy = Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            policy: DynamicHttpResponsePolicy::default(),
        };
        policy.refresh().await?;
        Ok(policy)
    }

    #[must_use]
    pub fn snapshot_handle(&self) -> DynamicHttpResponsePolicy {
        self.policy.clone()
    }

    /// Refreshes the three JVM response-middleware controls in one query.
    ///
    /// # Errors
    ///
    /// Returns a database error and retains the last valid snapshot.
    pub async fn refresh(&self) -> Result<(), HttpResponsePolicyError> {
        let client = self.client.acquire().await?;
        let rows = client
            .query(
                "SELECT 'toggle', toggle_setting.value #>> '{}', toggle_value.value
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
                   AND toggle_setting.value = '\"disable-preflight-caching\"'::jsonb
                 UNION ALL
                 SELECT 'flag', flag_setting.value #>> '{}', flag_value.value
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
                     '\"cors-max-age\"'::jsonb,
                     '\"filter-security-headers\"'::jsonb
                   )",
                &[&self.config_app_id],
            )
            .await?;
        let mut snapshot = HttpResponsePolicySnapshot::default();
        for row in rows {
            let setting: String = row.get(1);
            let value: Value = row.get(2);
            apply_value(&mut snapshot, &setting, &value);
        }
        self.policy.replace(snapshot);
        Ok(())
    }

    #[must_use]
    pub fn refresh_task(self) -> ScheduledTask {
        ScheduledTask::new(
            "http-response-policy-refresh",
            TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: false,
            },
            Duration::from_millis(900),
            move || {
                let policy = self.clone();
                async move { policy.refresh().await }
            },
        )
    }
}

fn apply_value(snapshot: &mut HttpResponsePolicySnapshot, setting: &str, value: &Value) {
    match setting {
        "disable-preflight-caching" => {
            snapshot.disable_preflight_caching = !matches!(value, Value::Null | Value::Bool(false));
        }
        "cors-max-age" => {
            if let Some(value) = scalar_string(value) {
                snapshot.cors_max_age = value;
            }
        }
        "filter-security-headers" => {
            if let Some(values) = value.as_array() {
                snapshot.filtered_security_headers = values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect();
            }
        }
        _ => {}
    }
}

fn scalar_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn config_values_preserve_defaults_truthiness_and_exact_filter_names() {
        let mut snapshot = HttpResponsePolicySnapshot::default();
        apply_value(&mut snapshot, "disable-preflight-caching", &json!("yes"));
        apply_value(&mut snapshot, "cors-max-age", &json!(42));
        apply_value(
            &mut snapshot,
            "filter-security-headers",
            &json!(["X-Frame-Options", "content-security-policy", 3]),
        );
        assert!(snapshot.disable_preflight_caching);
        assert_eq!(snapshot.cors_max_age, "42");
        assert_eq!(
            snapshot.filtered_security_headers,
            ["X-Frame-Options", "content-security-policy"]
        );

        let mut defaults = HttpResponsePolicySnapshot::default();
        apply_value(&mut defaults, "disable-preflight-caching", &Value::Null);
        apply_value(&mut defaults, "cors-max-age", &json!({"bad": true}));
        apply_value(&mut defaults, "filter-security-headers", &json!(false));
        assert_eq!(defaults, HttpResponsePolicySnapshot::default());
    }
}
