use std::time::Duration;

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    DatalogCachePolicy, DatalogCacheSettings, DatalogCacheSettingsFuture, INSTANT_CONFIG_APP_ID,
    PostgresPool,
};

const FLAG_NAMES: [&str; 5] = [
    "datalog-cache-max-weight",
    "datalog-cache-ttl",
    "datalog-cache-size-multiple",
    "datalog-cache-process-max-in-flight",
    "datalog-cache-max-in-flight",
];

/// Loads realtime query-cache controls in one
/// config-app snapshot whenever a new per-app cache is created.
pub struct PostgresDatalogCachePolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
}

impl PostgresDatalogCachePolicy {
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

impl DatalogCachePolicy for PostgresDatalogCachePolicy {
    fn settings(&self) -> DatalogCacheSettingsFuture<'_> {
        Box::pin(async move {
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
                        (&FLAG_NAMES.as_slice(), Type::TEXT_ARRAY),
                    ],
                )
                .await
                .map_err(database_error)?;
            parse_settings(
                rows.into_iter()
                    .map(|row| (row.get::<_, String>(0), row.get::<_, Value>(1))),
            )
        })
    }
}

fn parse_settings(
    values: impl IntoIterator<Item = (String, Value)>,
) -> Result<DatalogCacheSettings, ApiError> {
    let mut settings = DatalogCacheSettings::default();
    for (name, value) in values {
        match name.as_str() {
            "datalog-cache-max-weight" => {
                settings.max_weight = non_negative_integer(&name, &value)?;
            }
            "datalog-cache-ttl" => {
                let milliseconds = positive_integer(&name, &value)?;
                settings.ttl = Duration::from_millis(milliseconds);
            }
            "datalog-cache-size-multiple" => {
                settings.size_multiple = non_negative_integer(&name, &value)?;
            }
            "datalog-cache-process-max-in-flight" => {
                settings.process_max_in_flight = non_negative_usize(&name, &value)?;
            }
            "datalog-cache-max-in-flight" => {
                settings.max_in_flight = non_negative_usize(&name, &value)?;
            }
            _ => {}
        }
    }
    Ok(settings)
}

fn non_negative_usize(name: &str, value: &Value) -> Result<usize, ApiError> {
    non_negative_integer(name, value).and_then(|value| {
        usize::try_from(value)
            .map_err(|_| invalid_flag(name, "an integer supported by this process"))
    })
}

fn non_negative_integer(name: &str, value: &Value) -> Result<u64, ApiError> {
    value
        .as_u64()
        .ok_or_else(|| invalid_flag(name, "a non-negative integer"))
}

fn positive_integer(name: &str, value: &Value) -> Result<u64, ApiError> {
    non_negative_integer(name, value).and_then(|value| {
        if value == 0 {
            Err(invalid_flag(name, "a positive integer"))
        } else {
            Ok(value)
        }
    })
}

fn invalid_flag(name: &str, expected: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        format!("config flag `{name}` must be {expected}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_reference_cache() {
        assert_eq!(
            parse_settings([]).unwrap(),
            DatalogCacheSettings {
                max_weight: 500 * 1024 * 1024,
                ttl: Duration::from_hours(1),
                size_multiple: 4,
                process_max_in_flight: 0,
                max_in_flight: 0,
            }
        );
    }

    #[test]
    fn parses_all_controls_from_one_snapshot() {
        assert_eq!(
            parse_settings([
                ("datalog-cache-max-weight".into(), Value::from(12_345)),
                ("datalog-cache-ttl".into(), Value::from(2_500)),
                ("datalog-cache-size-multiple".into(), Value::from(7)),
                (
                    "datalog-cache-process-max-in-flight".into(),
                    Value::from(12)
                ),
                ("datalog-cache-max-in-flight".into(), Value::from(3)),
            ])
            .unwrap(),
            DatalogCacheSettings {
                max_weight: 12_345,
                ttl: Duration::from_millis(2_500),
                size_multiple: 7,
                process_max_in_flight: 12,
                max_in_flight: 3,
            }
        );
    }

    #[test]
    fn malformed_controls_fail_closed() {
        for (name, value) in [
            ("datalog-cache-max-weight", Value::from(-1)),
            ("datalog-cache-ttl", Value::from(0)),
            ("datalog-cache-size-multiple", Value::from("four")),
            ("datalog-cache-process-max-in-flight", Value::from(-1)),
            ("datalog-cache-max-in-flight", Value::from(-1)),
        ] {
            let error = parse_settings([(name.into(), value)]).unwrap_err();
            assert!(error.body().message.contains(name));
        }
    }

    #[test]
    fn zero_load_limits_disable_admission_without_rejecting_the_snapshot() {
        let settings = parse_settings([
            ("datalog-cache-process-max-in-flight".into(), Value::from(0)),
            ("datalog-cache-max-in-flight".into(), Value::from(0)),
        ])
        .unwrap();

        assert_eq!(settings.process_max_in_flight, 0);
        assert_eq!(settings.max_in_flight, 0);
    }
}
