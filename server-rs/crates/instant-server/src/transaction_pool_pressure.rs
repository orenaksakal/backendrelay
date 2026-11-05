use std::future::Future;
use std::pin::Pin;

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::fresh_control_reads::{FreshControlQuery, FreshControlReads};
use crate::postgres_pool::{PostgresClientGuard, PostgresClientSource};
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool, PostgresPoolStatus};

const ENABLED_TOGGLE: &str = "rate-limit-tx-based-on-conn-pool?";
const BUFFER_FLAG: &str = "rate-limit-tx-based-on-conn-pool-buffer";
const DEFAULT_BUFFER: i64 = 5;

pub type TransactionPoolPressureFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ApiError>> + Send + 'a>>;

/// Pre-check performed before a permissioned transaction acquires its write lease.
pub trait TransactionPoolPressurePolicy: Send + Sync + 'static {
    fn check(&self, app_id: Uuid) -> TransactionPoolPressureFuture<'_>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DisabledTransactionPoolPressure;

impl TransactionPoolPressurePolicy for DisabledTransactionPoolPressure {
    fn check(&self, _app_id: Uuid) -> TransactionPoolPressureFuture<'_> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Live config-app policy that observes one pool while reading its rollout controls
/// through a separate pool. Keeping those pools distinct is essential: the emergency
/// check must not wait for the very write capacity it is intended to protect.
pub struct PostgresTransactionPoolPressurePolicy {
    observed_pool: PostgresPool,
    config_client: PostgresClientSource,
    config_app_id: Uuid,
    controls_sql: String,
    batch: Option<FreshControlReads<PressureControlsQuery>>,
    administrator_email: Option<String>,
    control_plane: Option<std::sync::Arc<crate::ControlPlaneCache>>,
}

struct PressureControlsQuery {
    sql: String,
}

impl FreshControlQuery for PressureControlsQuery {
    type Input = ();
    type Output = (bool, i64);

    async fn execute(
        &self,
        client: &PostgresClientGuard<'_>,
        inputs: Vec<()>,
    ) -> Result<Vec<Self::Output>, ApiError> {
        // Every member was queued before this fresh statement started. Only
        // global controls from this statement are common; each caller samples
        // current observed-pool pressure after receiving this result.
        let controls = read_controls(client, &self.sql).await?;
        Ok(vec![controls; inputs.len()])
    }
}

impl PostgresTransactionPoolPressurePolicy {
    #[must_use]
    pub fn new(
        observed_pool: PostgresPool,
        config_client: Client,
        administrator_email: Option<String>,
    ) -> Self {
        Self {
            observed_pool,
            config_client: PostgresClientSource::fixed(config_client),
            config_app_id: INSTANT_CONFIG_APP_ID,
            controls_sql: controls_sql(INSTANT_CONFIG_APP_ID),
            batch: None,
            administrator_email,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn from_pools(
        observed_pool: PostgresPool,
        config_pool: PostgresPool,
        administrator_email: Option<String>,
    ) -> Self {
        Self {
            observed_pool,
            config_client: PostgresClientSource::pooled(config_pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            controls_sql: controls_sql(INSTANT_CONFIG_APP_ID),
            batch: None,
            administrator_email,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self.control_plane = None;
        self.controls_sql = controls_sql(config_app_id);
        if self.batch.is_some() {
            self = self.with_batched_reads();
        }
        self
    }

    /// Share a new control statement only among already queued callers. No
    /// completed/in-flight control value or pressure decision is reused, and
    /// the supplied control pool and its connection budget remain unchanged.
    pub(crate) fn with_batched_reads(mut self) -> Self {
        self.batch = Some(FreshControlReads::new(
            self.config_client.clone(),
            PressureControlsQuery {
                sql: self.controls_sql.clone(),
            },
        ));
        self
    }

    /// Serves rollout controls from a snapshot evicted by any write to the
    /// config app. Observed-pool pressure is still sampled on every check.
    pub(crate) fn with_control_plane_cache(
        mut self,
        cache: std::sync::Arc<crate::ControlPlaneCache>,
    ) -> Self {
        assert!(cache.matches_config_app(self.config_app_id));
        self.control_plane = Some(cache);
        self
    }

    async fn controls(&self) -> Result<(bool, i64), ApiError> {
        match &self.control_plane {
            Some(cache) => cache.pool_pressure_controls(|| self.read_controls()).await,
            None => self.read_controls().await,
        }
    }

    async fn read_controls(&self) -> Result<(bool, i64), ApiError> {
        if let Some(batch) = &self.batch {
            return batch.load(()).await;
        }
        let client = self.config_client.acquire().await.map_err(database_error)?;
        read_controls(&client, &self.controls_sql).await
    }
}

async fn read_controls(
    client: &PostgresClientGuard<'_>,
    sql: &str,
) -> Result<(bool, i64), ApiError> {
    let statement = client.prepare_cached(sql).await.map_err(database_error)?;
    let rows = client
        .query(&statement, &[])
        .await
        .map_err(database_error)?;
    let mut enabled = false;
    let mut buffer = None;
    for row in rows {
        let namespace: String = row.get(0);
        let value: Value = row.get(1);
        if namespace == "toggle" {
            enabled = clojure_truthy(&value);
        } else {
            buffer = Some(parse_buffer(&value)?);
        }
    }
    Ok((enabled, buffer.unwrap_or(DEFAULT_BUFFER)))
}

// The UUID is a typed policy constant, not request-supplied SQL. Baking only these
// constants into the statement lets PostgreSQL prune partitions once and reuse
// its plan. Control values remain live; no decisions or rows are cached.
fn controls_sql(config_app_id: Uuid) -> String {
    format!("SELECT 'toggle', toggle_value.value
                 FROM attrs setting_attr
                 JOIN triples toggle_setting
                   ON toggle_setting.app_id = '{config_app_id}'::uuid AND toggle_setting.attr_id = setting_attr.id
                 JOIN attrs toggled_attr
                   ON toggled_attr.app_id = '{config_app_id}'::uuid AND toggled_attr.etype = 'toggles'
                  AND toggled_attr.label = 'toggled'
                 JOIN triples toggle_value
                   ON toggle_value.app_id = '{config_app_id}'::uuid
                  AND toggle_value.entity_id = toggle_setting.entity_id
                  AND toggle_value.attr_id = toggled_attr.id
                 WHERE setting_attr.app_id = '{config_app_id}'::uuid AND setting_attr.etype = 'toggles'
                   AND setting_attr.label = 'setting'
                   AND toggle_setting.value = to_jsonb('{ENABLED_TOGGLE}'::text)
                 UNION ALL
                 SELECT 'flag', flag_value.value
                 FROM attrs setting_attr
                 JOIN triples flag_setting
                   ON flag_setting.app_id = '{config_app_id}'::uuid AND flag_setting.attr_id = setting_attr.id
                 JOIN attrs value_attr
                   ON value_attr.app_id = '{config_app_id}'::uuid AND value_attr.etype = 'flags'
                  AND value_attr.label = 'value'
                 JOIN triples flag_value
                   ON flag_value.app_id = '{config_app_id}'::uuid
                  AND flag_value.entity_id = flag_setting.entity_id
                  AND flag_value.attr_id = value_attr.id
                 WHERE setting_attr.app_id = '{config_app_id}'::uuid AND setting_attr.etype = 'flags'
                   AND setting_attr.label = 'setting'
                   AND flag_setting.value = to_jsonb('{BUFFER_FLAG}'::text)")
}

impl TransactionPoolPressurePolicy for PostgresTransactionPoolPressurePolicy {
    fn check(&self, app_id: Uuid) -> TransactionPoolPressureFuture<'_> {
        Box::pin(async move {
            let (enabled, buffer) = self.controls().await?;
            let status = self.observed_pool.status();
            if !should_rate_limit(enabled, buffer, status) {
                return Ok(());
            }
            tracing::info!(
                target: "instant.permissioned-transaction",
                event_name = "permissioned-transaction/transact-rate-limit",
                %app_id,
                active_connections = status.size.saturating_sub(status.available),
                total_connections = status.max_size,
            );
            Err(crate::app_rate_limit::rate_limit_error(
                self.administrator_email.as_deref(),
            ))
        })
    }
}

fn should_rate_limit(enabled: bool, buffer: i64, status: PostgresPoolStatus) -> bool {
    let active = status.size.saturating_sub(status.available);
    let remaining_capacity = status.max_size.saturating_sub(active);
    enabled && i128::from(buffer) > remaining_capacity as i128
}

fn parse_buffer(value: &Value) -> Result<i64, ApiError> {
    value.as_i64().ok_or_else(|| {
        ApiError::new(
            ApiErrorType::Unknown,
            format!("config flag {BUFFER_FLAG} must be an integer"),
        )
    })
}

fn clojure_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::json;
    use tokio_postgres::types::Json;

    struct ControlsFixture {
        setup: Client,
        schema: String,
        pool_url: String,
        app: Uuid,
        toggled: Uuid,
        buffer: Uuid,
    }

    impl ControlsFixture {
        async fn new() -> Option<Self> {
            let url = instant_test_support::test_database_url("transaction_pool_pressure_fixture")?;
            let (setup, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .unwrap();
            tokio::spawn(connection);
            let schema = format!("batched_pressure_{}", Uuid::new_v4().simple());
            setup.batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.attrs(id uuid PRIMARY KEY,app_id uuid,etype text,label text);
                 CREATE TABLE {schema}.triples(app_id uuid,entity_id uuid,attr_id uuid,value jsonb);"
            )).await.unwrap();
            let app = Uuid::new_v4();
            let setting = Uuid::new_v4();
            let toggled = Uuid::new_v4();
            let flag = Uuid::new_v4();
            let buffer = Uuid::new_v4();
            setup
                .execute(
                    &format!(
                        "INSERT INTO {schema}.attrs VALUES
                 ($1,$5,'toggles','setting'),($2,$5,'toggles','toggled'),
                 ($3,$5,'flags','setting'),($4,$5,'flags','value')"
                    ),
                    &[&setting, &toggled, &flag, &buffer, &app],
                )
                .await
                .unwrap();
            let toggle_entity = Uuid::new_v4();
            let flag_entity = Uuid::new_v4();
            setup
                .execute(
                    &format!(
                        "INSERT INTO {schema}.triples VALUES
                 ($1,$2,$3,$4),($1,$2,$5,'true'),
                 ($1,$6,$7,$8),($1,$6,$9,'2')"
                    ),
                    &[
                        &app,
                        &toggle_entity,
                        &setting,
                        &Json(json!(ENABLED_TOGGLE)),
                        &toggled,
                        &flag_entity,
                        &flag,
                        &Json(json!(BUFFER_FLAG)),
                        &buffer,
                    ],
                )
                .await
                .unwrap();
            let mut scoped = url::Url::parse(&url).unwrap();
            scoped
                .query_pairs_mut()
                .append_pair("options", &format!("-csearch_path={schema}"));
            Some(Self {
                setup,
                schema,
                pool_url: scoped.to_string(),
                app,
                toggled,
                buffer,
            })
        }

        async fn set(&self, attr: Uuid, value: Value) {
            self.setup
                .execute(
                    &format!(
                        "UPDATE {}.triples SET value=$1 WHERE app_id=$2 AND attr_id=$3",
                        self.schema
                    ),
                    &[&Json(value), &self.app, &attr],
                )
                .await
                .unwrap();
        }

        async fn close(self) {
            self.setup
                .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
                .await
                .unwrap();
        }
    }

    async fn wait_for_control_queue(pool: &PostgresPool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while pool.status().waiting == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn status(size: usize, available: usize) -> PostgresPoolStatus {
        PostgresPoolStatus {
            max_size: 20,
            size,
            available,
            waiting: 0,
        }
    }

    #[test]
    fn pressure_uses_total_capacity_minus_active_and_strict_buffer_comparison() {
        assert!(!should_rate_limit(false, 5, status(20, 0)));
        assert!(!should_rate_limit(true, 5, status(5, 0)));
        assert!(should_rate_limit(true, 5, status(20, 4)));
        assert!(!should_rate_limit(true, 5, status(19, 4)));
        assert!(!should_rate_limit(true, -1, status(20, 0)));
    }

    #[test]
    fn controls_preserve_clojure_truthiness_and_integer_validation() {
        assert!(!clojure_truthy(&Value::Null));
        assert!(!clojure_truthy(&Value::Bool(false)));
        assert!(clojure_truthy(&Value::Bool(true)));
        assert!(clojure_truthy(&Value::String("false".to_owned())));
        assert_eq!(parse_buffer(&Value::from(7)).unwrap(), 7);
        assert!(parse_buffer(&Value::from(7.5)).is_err());
        assert!(parse_buffer(&Value::String("7".to_owned())).is_err());
    }

    #[tokio::test]
    async fn queued_pressure_controls_share_one_fresh_read_but_sample_live_capacity() {
        let Some(fixture) = ControlsFixture::new().await else {
            return;
        };
        let observed = PostgresPool::connect(&fixture.pool_url, 3).await.unwrap();
        let controls = PostgresPool::connect(&fixture.pool_url, 1).await.unwrap();
        let policy = Arc::new(
            PostgresTransactionPoolPressurePolicy::from_pools(
                observed.clone(),
                controls.clone(),
                Some("admin@example.test".to_owned()),
            )
            .with_config_app_id(fixture.app)
            .with_batched_reads(),
        );
        let first = observed.acquire().await.unwrap();
        let second = observed.acquire().await.unwrap();
        let held = controls.acquire().await.unwrap();
        let before = controls.acquisition_count();
        let checks = Arc::clone(&policy);
        let joined = tokio::spawn(async move {
            tokio::join!(
                checks.check(Uuid::new_v4()),
                checks.check(Uuid::new_v4()),
                checks.check(Uuid::new_v4()),
                checks.check(Uuid::new_v4())
            )
        });
        wait_for_control_queue(&controls).await;
        drop(held);
        let (a, b, c, d) = joined.await.unwrap();
        for result in [a, b, c, d] {
            let error = result.unwrap_err();
            assert_eq!(error.body().kind, ApiErrorType::RateLimited);
            assert!(error.body().message.contains("admin@example.test"));
        }
        assert_eq!(controls.acquisition_count() - before, 1);
        assert_eq!(controls.max_size(), 1);
        assert_eq!(observed.max_size(), 3);
        // Same controls, different live capacity: strict equality must allow.
        drop(first);
        policy.check(Uuid::new_v4()).await.unwrap();
        fixture.set(fixture.buffer, json!(3)).await;
        assert!(policy.check(Uuid::new_v4()).await.is_err());
        fixture.set(fixture.toggled, json!(false)).await;
        policy.check(Uuid::new_v4()).await.unwrap();
        fixture.set(fixture.buffer, json!("invalid")).await;
        let error = policy.check(Uuid::new_v4()).await.unwrap_err();
        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert!(error.body().message.contains(BUFFER_FLAG));
        fixture.set(fixture.buffer, json!(-1)).await;
        fixture.set(fixture.toggled, json!("false")).await;
        policy.check(Uuid::new_v4()).await.unwrap();
        fixture.set(fixture.buffer, json!(3)).await;
        assert!(policy.check(Uuid::new_v4()).await.is_err());
        drop(second);
        drop(policy);
        fixture.close().await;
    }

    #[tokio::test]
    async fn cancelled_pressure_waiter_cannot_cancel_peers_and_config_replacement_stays_scoped() {
        let Some(fixture) = ControlsFixture::new().await else {
            return;
        };
        let observed = PostgresPool::connect(&fixture.pool_url, 3).await.unwrap();
        let controls = PostgresPool::connect(&fixture.pool_url, 1).await.unwrap();
        let policy = Arc::new(
            PostgresTransactionPoolPressurePolicy::from_pools(
                observed.clone(),
                controls.clone(),
                None,
            )
            .with_batched_reads()
            .with_config_app_id(fixture.app),
        );
        let first = observed.acquire().await.unwrap();
        let second = observed.acquire().await.unwrap();
        let held = controls.acquire().await.unwrap();
        let cancelled_policy = Arc::clone(&policy);
        let cancelled = tokio::spawn(async move { cancelled_policy.check(Uuid::new_v4()).await });
        wait_for_control_queue(&controls).await;
        {
            let peers =
                async { tokio::join!(policy.check(Uuid::new_v4()), policy.check(Uuid::new_v4())) };
            tokio::pin!(peers);
            std::future::poll_fn(|context| {
                assert!(peers.as_mut().poll(context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            cancelled.abort();
            assert!(cancelled.await.unwrap_err().is_cancelled());
            drop(held);
            let (a, b) = peers.await;
            for result in [a, b] {
                let error = result.unwrap_err();
                assert_eq!(error.body().kind, ApiErrorType::RateLimited);
                assert!(error.body().message.contains("operator of this deployment"));
            }
        }
        let mut policy = Arc::try_unwrap(policy).ok().unwrap();
        policy = policy.with_config_app_id(Uuid::new_v4());
        policy.check(Uuid::new_v4()).await.unwrap();
        policy = policy.with_config_app_id(fixture.app);
        assert!(policy.check(Uuid::new_v4()).await.is_err());
        fixture.set(fixture.buffer, json!("invalid")).await;
        let (a, b) = tokio::join!(policy.check(Uuid::new_v4()), policy.check(Uuid::new_v4()));
        for result in [a, b] {
            let error = result.unwrap_err();
            assert_eq!(error.body().kind, ApiErrorType::Unknown);
            assert!(error.body().message.contains(BUFFER_FLAG));
        }
        fixture.set(fixture.buffer, json!(2)).await;
        drop(first);
        policy.check(Uuid::new_v4()).await.unwrap();
        drop(second);
        drop(policy);
        fixture.close().await;
    }
}
