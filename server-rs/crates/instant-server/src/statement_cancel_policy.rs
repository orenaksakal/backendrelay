use std::time::Duration;

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    INSTANT_CONFIG_APP_ID, PostgresPool, StatementCancelWaitFuture, StatementCancelWaitPolicy,
};

const DEFAULT_STATEMENT_CANCEL_WAIT: Duration = Duration::from_millis(500);

/// Reads the config-app `statement-cancel-wait-ms` flag.
///
/// Production serves it from a WAL-invalidated control-plane snapshot: the value
/// only bounds how long a cancelled statement may keep running after its client
/// disconnects, and upstream receives it through a flag subscription. Constructors
/// without a cache keep the original fresh-per-cancellation read.
pub struct PostgresStatementCancelWaitPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
    batch: Option<crate::fresh_control_reads::FreshControlReads<CancelWaitQuery>>,
    control_plane: Option<std::sync::Arc<crate::ControlPlaneCache>>,
}

struct CancelWaitQuery {
    config_app_id: Uuid,
}

impl crate::fresh_control_reads::FreshControlQuery for CancelWaitQuery {
    type Input = ();
    type Output = Duration;

    async fn execute(
        &self,
        client: &crate::postgres_pool::PostgresClientGuard<'_>,
        inputs: Vec<()>,
    ) -> Result<Vec<Duration>, ApiError> {
        let value = client
            .query_typed_cached_opt(
                SQL,
                &[
                    (&self.config_app_id, Type::UUID),
                    (&"statement-cancel-wait-ms", Type::TEXT),
                ],
            )
            .await
            .map_err(database_error)?
            .map(|row| row.get::<_, Value>(0));
        let duration = statement_cancel_wait_from_value(value.as_ref())?;
        Ok(vec![duration; inputs.len()])
    }
}

const SQL: &str = "SELECT flag_value.value
    FROM attrs setting_attr
    JOIN triples flag_setting ON flag_setting.app_id=$1 AND flag_setting.attr_id=setting_attr.id
    JOIN attrs value_attr ON value_attr.app_id=$1 AND value_attr.etype='flags' AND value_attr.label='value'
    JOIN triples flag_value ON flag_value.app_id=$1 AND flag_value.entity_id=flag_setting.entity_id
      AND flag_value.attr_id=value_attr.id
    WHERE setting_attr.app_id=$1 AND setting_attr.etype='flags' AND setting_attr.label='setting'
      AND flag_setting.value=to_jsonb($2::text) LIMIT 1";

impl PostgresStatementCancelWaitPolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            config_app_id: INSTANT_CONFIG_APP_ID,
            batch: None,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            batch: None,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self.batch = None;
        self.control_plane = None;
        self
    }

    pub(crate) fn with_batched_reads(mut self) -> Self {
        self.batch = Some(crate::fresh_control_reads::FreshControlReads::new(
            self.client.clone(),
            CancelWaitQuery {
                config_app_id: self.config_app_id,
            },
        ));
        self
    }

    /// Serves the flag from a snapshot evicted by any config-app write, as
    /// upstream's flag subscription does.
    pub(crate) fn with_control_plane_cache(
        mut self,
        cache: std::sync::Arc<crate::ControlPlaneCache>,
    ) -> Self {
        assert!(cache.matches_config_app(self.config_app_id));
        self.control_plane = Some(cache);
        self
    }

    async fn read_cancel_wait(&self) -> Result<Duration, ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let value = client
            .query_typed_cached_opt(
                SQL,
                &[
                    (&self.config_app_id, Type::UUID),
                    (&"statement-cancel-wait-ms", Type::TEXT),
                ],
            )
            .await
            .map_err(database_error)?
            .map(|row| row.get::<_, Value>(0));
        statement_cancel_wait_from_value(value.as_ref())
    }
}

impl StatementCancelWaitPolicy for PostgresStatementCancelWaitPolicy {
    fn wait_duration(&self) -> StatementCancelWaitFuture<'_> {
        Box::pin(async move {
            if let Some(cache) = &self.control_plane {
                return cache.cancel_wait(|| self.read_cancel_wait()).await;
            }
            if let Some(batch) = &self.batch {
                return batch.load(()).await;
            }
            self.read_cancel_wait().await
        })
    }
}

pub(crate) fn statement_cancel_wait_from_value(
    value: Option<&Value>,
) -> Result<Duration, ApiError> {
    value.map_or(Ok(DEFAULT_STATEMENT_CANCEL_WAIT), |value| {
        value.as_u64().map(Duration::from_millis).ok_or_else(|| {
            ApiError::new(
                ApiErrorType::Unknown,
                "config flag `statement-cancel-wait-ms` must be a non-negative integer",
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn statement_cancel_wait_preserves_default_zero_and_integral_override() {
        assert_eq!(
            statement_cancel_wait_from_value(None).unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            statement_cancel_wait_from_value(Some(&json!(0))).unwrap(),
            Duration::ZERO
        );
        assert_eq!(
            statement_cancel_wait_from_value(Some(&json!(1_250))).unwrap(),
            Duration::from_millis(1_250)
        );
    }

    #[test]
    fn malformed_statement_cancel_wait_fails_closed() {
        let overflow = serde_json::from_str("18446744073709551616").unwrap();
        for value in [json!(-1), json!(1.5), json!("500"), Value::Null, overflow] {
            let error = statement_cancel_wait_from_value(Some(&value)).unwrap_err();
            assert_eq!(error.body().kind, ApiErrorType::Unknown);
            assert!(error.body().message.contains("statement-cancel-wait-ms"));
        }
    }

    #[tokio::test]
    async fn cached_flag_serves_the_snapshot_until_config_app_eviction() {
        use tokio_postgres::{NoTls, types::Json};

        let Some(database_url) = instant_test_support::test_database_url(
            "cached_flag_serves_the_snapshot_until_config_app_eviction",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let schema = format!("cancel_wait_cache_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.attrs (
                   id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL,
                   label text NOT NULL
                 );
                 CREATE TABLE {schema}.triples (
                   app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
                   value jsonb NOT NULL
                 );"
            ))
            .await
            .unwrap();
        let config_app_id = Uuid::new_v4();
        let setting_attr = Uuid::new_v4();
        let value_attr = Uuid::new_v4();
        let entity = Uuid::new_v4();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.attrs VALUES
                       ($1,$3,'flags','setting'), ($2,$3,'flags','value')"
                ),
                &[&setting_attr, &value_attr, &config_app_id],
            )
            .await
            .unwrap();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.triples VALUES
                       ($1,$2,$3,$4), ($1,$2,$5,$6)"
                ),
                &[
                    &config_app_id,
                    &entity,
                    &setting_attr,
                    &Json(json!("statement-cancel-wait-ms")),
                    &value_attr,
                    &Json(json!(750)),
                ],
            )
            .await
            .unwrap();
        let mut url = url::Url::parse(&database_url).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = crate::PostgresPool::connect(url.as_str(), 1).await.unwrap();
        let cache = std::sync::Arc::new(crate::ControlPlaneCache::new(config_app_id));
        let policy = PostgresStatementCancelWaitPolicy::from_pool(pool.clone())
            .with_config_app_id(config_app_id)
            .with_control_plane_cache(std::sync::Arc::clone(&cache));
        assert_eq!(policy.wait_duration().await.unwrap().as_millis(), 750);

        setup
            .execute(
                &format!("UPDATE {schema}.triples SET value=$2 WHERE attr_id=$1"),
                &[&value_attr, &Json(json!(25))],
            )
            .await
            .unwrap();
        assert_eq!(
            policy.wait_duration().await.unwrap().as_millis(),
            750,
            "a served snapshot stays until its config-app eviction arrives"
        );
        cache.evict(&instant_wal::WalCacheEviction::Attribute {
            app_id: Some(config_app_id),
            attr_id: None,
        });
        assert_eq!(policy.wait_duration().await.unwrap().as_millis(), 25);

        drop(policy);
        drop(cache);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
