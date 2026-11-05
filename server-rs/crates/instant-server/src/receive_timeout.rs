use std::time::Duration;

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    DEFAULT_RECEIVE_TIMEOUT, INSTANT_CONFIG_APP_ID, PostgresPool, ReceiveHandlerTimeoutPolicy,
    ReceiveHandlerTimeouts, ReceiveHandlerTimeoutsFuture, ReceiveTimeoutFuture,
    ReceiveTimeoutPolicy,
};

/// Resolves the config app's per-app `handle-receive-timeout` entity without
/// retaining a JVM flag subscription.
pub struct PostgresReceiveTimeoutPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
    control_plane: Option<std::sync::Arc<crate::ControlPlaneCache>>,
}

impl PostgresReceiveTimeoutPolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self.control_plane = None;
        self
    }

    /// Serves handler limits from a snapshot evicted by any write to the
    /// config app, like upstream's flag subscription.
    pub(crate) fn with_control_plane_cache(
        mut self,
        cache: std::sync::Arc<crate::ControlPlaneCache>,
    ) -> Self {
        assert!(cache.matches_config_app(self.config_app_id));
        self.control_plane = Some(cache);
        self
    }
}

impl ReceiveTimeoutPolicy for PostgresReceiveTimeoutPolicy {
    fn timeout(&self, app_id: Option<Uuid>) -> ReceiveTimeoutFuture<'_> {
        Box::pin(async move {
            let Some(app_id) = app_id else {
                return Ok(DEFAULT_RECEIVE_TIMEOUT);
            };
            let client = self.client.acquire().await.map_err(database_error)?;
            let value = client
                .query_typed_cached_opt(
                    "SELECT timeout_value.value
                     FROM attrs app_attr
                     JOIN triples app_value
                       ON app_value.app_id = $1 AND app_value.attr_id = app_attr.id
                     JOIN attrs timeout_attr
                       ON timeout_attr.app_id = $1
                      AND timeout_attr.etype = 'handle-receive-timeout'
                      AND timeout_attr.label = 'timeoutMs'
                     JOIN triples timeout_value
                       ON timeout_value.app_id = $1
                      AND timeout_value.entity_id = app_value.entity_id
                      AND timeout_value.attr_id = timeout_attr.id
                     WHERE app_attr.app_id = $1
                       AND app_attr.etype = 'handle-receive-timeout'
                       AND app_attr.label = 'appId'
                       AND app_value.value #>> '{}' = $2
                     LIMIT 1",
                    &[
                        (&self.config_app_id, Type::UUID),
                        (&app_id.to_string(), Type::TEXT),
                    ],
                )
                .await
                .map_err(database_error)?
                .map(|row| row.get::<_, Value>(0));
            receive_timeout_from_value(value.as_ref())
        })
    }
}

impl ReceiveHandlerTimeoutPolicy for PostgresReceiveTimeoutPolicy {
    fn handler_timeouts(&self, app_id: Option<Uuid>) -> ReceiveHandlerTimeoutsFuture<'_> {
        Box::pin(async move {
            match &self.control_plane {
                Some(cache) => {
                    cache
                        .receive_timeouts(app_id, || self.read_handler_timeouts(app_id))
                        .await
                }
                None => self.read_handler_timeouts(app_id).await,
            }
        })
    }
}

impl PostgresReceiveTimeoutPolicy {
    async fn read_handler_timeouts(
        &self,
        app_id: Option<Uuid>,
    ) -> Result<ReceiveHandlerTimeouts, ApiError> {
        {
            let client = self.client.acquire().await.map_err(database_error)?;
            // Both values are fresh at the same handler boundary. Scalar
            // subqueries preserve LIMIT 1/missing-row/JSON-null semantics and
            // skip the per-app lookup for init. No control values are cached.
            let row = client.query_typed_cached_one(
                "SELECT (
                   SELECT timeout_value.value
                   FROM attrs app_attr
                   JOIN triples app_value
                     ON app_value.app_id=$1 AND app_value.attr_id=app_attr.id
                   JOIN attrs timeout_attr
                     ON timeout_attr.app_id=$1 AND timeout_attr.etype='handle-receive-timeout'
                    AND timeout_attr.label='timeoutMs'
                   JOIN triples timeout_value
                     ON timeout_value.app_id=$1 AND timeout_value.entity_id=app_value.entity_id
                    AND timeout_value.attr_id=timeout_attr.id
                   WHERE $2::text IS NOT NULL AND app_attr.app_id=$1
                     AND app_attr.etype='handle-receive-timeout' AND app_attr.label='appId'
                     AND app_value.value #>> '{}'=$2 LIMIT 1
                 ) AS receive_timeout, (
                   SELECT flag_value.value
                   FROM attrs setting_attr
                   JOIN triples flag_setting
                     ON flag_setting.app_id=$1 AND flag_setting.attr_id=setting_attr.id
                   JOIN attrs value_attr
                     ON value_attr.app_id=$1 AND value_attr.etype='flags' AND value_attr.label='value'
                   JOIN triples flag_value
                     ON flag_value.app_id=$1 AND flag_value.entity_id=flag_setting.entity_id
                    AND flag_value.attr_id=value_attr.id
                   WHERE setting_attr.app_id=$1 AND setting_attr.etype='flags'
                     AND setting_attr.label='setting'
                     AND flag_setting.value=to_jsonb('statement-cancel-wait-ms'::text) LIMIT 1
                 ) AS cancel_wait",
                &[
                    (&self.config_app_id, Type::UUID),
                    (&app_id.map(|id| id.to_string()), Type::TEXT),
                ],
            ).await.map_err(database_error)?;
            let timeout: Option<Value> = row.get("receive_timeout");
            let cancel_wait: Option<Value> = row.get("cancel_wait");
            // Keep timeout-config errors ahead of cancellation-config errors.
            let timeout = receive_timeout_from_value(timeout.as_ref())?;
            let cancel_wait = crate::statement_cancel_policy::statement_cancel_wait_from_value(
                cancel_wait.as_ref(),
            )?;
            Ok(ReceiveHandlerTimeouts {
                timeout,
                cancel_wait,
            })
        }
    }
}

fn receive_timeout_from_value(value: Option<&Value>) -> Result<Duration, ApiError> {
    value.map_or(Ok(DEFAULT_RECEIVE_TIMEOUT), |value| {
        value.as_u64().map(Duration::from_millis).ok_or_else(|| {
            ApiError::new(
                ApiErrorType::Unknown,
                "config value `handle-receive-timeout.timeoutMs` must be a non-negative integer",
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn timeout_value_preserves_default_zero_and_override() {
        assert_eq!(
            receive_timeout_from_value(None).unwrap(),
            Duration::from_secs(5)
        );
        assert_eq!(
            receive_timeout_from_value(Some(&json!(0))).unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            receive_timeout_from_value(Some(&json!(1_250))).unwrap(),
            Duration::from_millis(1_250)
        );
    }

    #[test]
    fn malformed_timeout_fails_closed() {
        for value in [json!(-1), json!(1.5), json!("5000")] {
            let error = receive_timeout_from_value(Some(&value)).unwrap_err();
            assert_eq!(error.body().kind, ApiErrorType::Unknown);
            assert!(error.body().message.contains("timeoutMs"));
        }
    }
}
