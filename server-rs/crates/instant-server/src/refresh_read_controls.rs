//! Fresh app status per refresh, with an optional WAL-invalidated topic flag.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    AppStatus, AppStatusSnapshot, EffectiveAppStatusPolicy, INSTANT_CONFIG_APP_ID, PostgresPool,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshReadControls {
    pub status: AppStatusSnapshot,
    pub use_coarse_topics: bool,
}

pub type RefreshReadControlsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<RefreshReadControls, ApiError>> + Send + 'a>>;

/// Explicit opt-in for two adjacent refresh-only reads. Replacing either
/// standalone runtime policy disables this optimization. Cancellation controls,
/// metadata, per-query authorization and query preparation remain separate.
pub trait RefreshReadControlsPolicy: Send + Sync {
    fn read(&self, app_id: Uuid, has_stale_queries: bool) -> RefreshReadControlsFuture<'_>;
}

pub struct PostgresRefreshReadControlsPolicy {
    client: PostgresClientSource,
    config_app_id: Uuid,
    controls: Option<Arc<crate::QueryControlCache>>,
    status: Option<Arc<crate::PostgresEffectiveAppStatusPolicy>>,
}

const STATUS_SQL: &str = "SELECT COALESCE((
    SELECT app.status FROM apps app
    WHERE app.id=$1 AND app.deletion_marked_at IS NULL
), 'active')";

const SQL: &str = "WITH status AS MATERIALIZED (
    SELECT COALESCE((SELECT app.status FROM apps app
                    WHERE app.id=$1 AND app.deletion_marked_at IS NULL), 'active') AS own_status
)
SELECT own_status, CASE WHEN $3 AND own_status IN ('active','read-only') THEN (
    SELECT flag_value.value
    FROM attrs setting_attr
    JOIN triples flag_setting
      ON flag_setting.app_id=$2 AND flag_setting.attr_id=setting_attr.id
    JOIN attrs value_attr
      ON value_attr.app_id=$2 AND value_attr.etype='flags' AND value_attr.label='value'
    JOIN triples flag_value
      ON flag_value.app_id=$2 AND flag_value.entity_id=flag_setting.entity_id
     AND flag_value.attr_id=value_attr.id
    WHERE setting_attr.app_id=$2 AND setting_attr.etype='flags'
      AND setting_attr.label='setting'
      AND flag_setting.value #>> '{}'='coarse-topics-apps'
) ELSE NULL::jsonb END FROM status";

impl PostgresRefreshReadControlsPolicy {
    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            config_app_id: INSTANT_CONFIG_APP_ID,
            controls: None,
            status: None,
        }
    }

    #[must_use]
    pub fn with_config_app_id(mut self, app_id: Uuid) -> Self {
        self.config_app_id = app_id;
        self.controls = None;
        self.status = None;
        self
    }

    pub(crate) fn with_status_policy(
        mut self,
        status: Arc<crate::PostgresEffectiveAppStatusPolicy>,
    ) -> Self {
        assert!(self.controls.is_some());
        self.status = Some(status);
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

impl RefreshReadControlsPolicy for PostgresRefreshReadControlsPolicy {
    fn read(&self, app_id: Uuid, has_stale_queries: bool) -> RefreshReadControlsFuture<'_> {
        Box::pin(async move {
            let (own_status, flag) = if let Some(status) = &self.status {
                (status.status(app_id).await?.own_status, None)
            } else {
                let row = {
                    let client = self.client.acquire().await.map_err(database_error)?;
                    crate::app_status_policy::retry_once_on_query_cancellation(|| async {
                        let result = if self.controls.is_some() {
                            client
                                .query_typed_cached_one(STATUS_SQL, &[(&app_id, Type::UUID)])
                                .await
                        } else {
                            client
                                .query_typed_cached_one(
                                    SQL,
                                    &[
                                        (&app_id, Type::UUID),
                                        (&self.config_app_id, Type::UUID),
                                        (&has_stale_queries, Type::BOOL),
                                    ],
                                )
                                .await
                        };
                        result.map_err(instant_pg::PgQueryError::from)
                    })
                    .await
                    .map_err(database_error)?
                }; // Release the status lease before a snapshot miss can acquire one.
                let raw: String = row.get(0);
                let own_status = AppStatus::parse(&raw).ok_or_else(|| {
                    ApiError::new(
                        ApiErrorType::Unknown,
                        format!("app status `{raw}` is not active, read-only, or disabled"),
                    )
                })?;
                let flag = if self.controls.is_none() {
                    row.get::<_, Option<Value>>(1)
                } else {
                    None
                };
                (own_status, flag)
            };
            // A topic read/cache failure must not outrank an invalid or disabled
            // app status, or block a schema-only refresh that needs no topics.
            let use_coarse_topics = if !has_stale_queries || own_status == AppStatus::Disabled {
                false
            } else if let Some(controls) = &self.controls {
                let flags = controls.runtime_flags().await?;
                crate::postgres_coarse_topics::flag_contains_app(
                    flags.get("coarse-topics-apps"),
                    app_id,
                )
            } else {
                crate::postgres_coarse_topics::flag_contains_app(flag.as_ref(), app_id)
            };
            Ok(RefreshReadControls {
                status: AppStatusSnapshot {
                    own_status,
                    effective_status: own_status,
                },
                use_coarse_topics,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_postgres::{NoTls, types::Json};

    struct Fixture {
        setup: tokio_postgres::Client,
        schema: String,
        app: Uuid,
        value: Uuid,
        entity: Uuid,
        pool: PostgresPool,
        policy: PostgresRefreshReadControlsPolicy,
    }

    async fn fixture(database_url: &str) -> Fixture {
        let (setup, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let schema = format!("refresh_controls_{}", Uuid::new_v4().simple());
        setup.batch_execute(&format!("CREATE SCHEMA {schema};
            CREATE TABLE {schema}.apps(id uuid PRIMARY KEY,status text NOT NULL,deletion_marked_at timestamptz);
            CREATE TABLE {schema}.attrs(id uuid PRIMARY KEY,app_id uuid,etype text,label text);
            CREATE TABLE {schema}.triples(app_id uuid,entity_id uuid,attr_id uuid,value jsonb);")).await.unwrap();
        let app = Uuid::new_v4();
        let config = Uuid::new_v4();
        let setting = Uuid::new_v4();
        let value = Uuid::new_v4();
        let entity = Uuid::new_v4();
        setup
            .execute(
                &format!("INSERT INTO {schema}.apps VALUES ($1,'active',NULL)"),
                &[&app],
            )
            .await
            .unwrap();
        setup.execute(&format!("INSERT INTO {schema}.attrs VALUES ($1,$3,'flags','setting'),($2,$3,'flags','value')"), &[&setting,&value,&config]).await.unwrap();
        setup
            .execute(
                &format!("INSERT INTO {schema}.triples VALUES ($1,$2,$3,$4),($1,$2,$5,$6)"),
                &[
                    &config,
                    &entity,
                    &setting,
                    &Json(json!("coarse-topics-apps")),
                    &value,
                    &Json(json!([app])),
                ],
            )
            .await
            .unwrap();
        let mut url = url::Url::parse(database_url).unwrap();
        url.query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PostgresPool::connect(url.as_str(), 1).await.unwrap();
        let policy =
            PostgresRefreshReadControlsPolicy::from_pool(pool.clone()).with_config_app_id(config);
        Fixture {
            setup,
            schema,
            app,
            value,
            entity,
            pool,
            policy,
        }
    }

    #[tokio::test]
    async fn combined_refresh_control_read_is_one_lease_fresh_and_app_scoped() {
        let Some(database_url) = instant_test_support::test_database_url(
            "combined_refresh_control_read_is_one_lease_fresh_and_app_scoped",
        ) else {
            return;
        };
        let Fixture {
            setup,
            schema,
            app,
            value,
            entity,
            pool,
            policy,
        } = fixture(&database_url).await;
        let before = pool.acquisition_count();
        let first = policy.read(app, true).await.unwrap();
        assert_eq!(pool.acquisition_count() - before, 1);
        assert_eq!(first.status.effective_status, AppStatus::Active);
        assert!(first.use_coarse_topics);
        assert!(
            !policy
                .read(Uuid::new_v4(), true)
                .await
                .unwrap()
                .use_coarse_topics
        );
        assert!(!policy.read(app, false).await.unwrap().use_coarse_topics);
        setup
            .execute(
                &format!("UPDATE {schema}.triples SET value=$2 WHERE attr_id=$1"),
                &[&value, &Json(json!([]))],
            )
            .await
            .unwrap();
        assert!(!policy.read(app, true).await.unwrap().use_coarse_topics);
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='read-only' WHERE id=$1"),
                &[&app],
            )
            .await
            .unwrap();
        assert_eq!(
            policy
                .read(app, true)
                .await
                .unwrap()
                .status
                .effective_status,
            AppStatus::ReadOnly
        );
        // A broken topic policy must not outrank disabled/invalid status or
        // schema-only refreshes which never need a topic decision.
        setup.execute(&format!("INSERT INTO {schema}.triples SELECT app_id,$1,attr_id,value FROM {schema}.triples WHERE entity_id=$2"), &[&Uuid::new_v4(),&entity]).await.unwrap();
        assert!(policy.read(app, true).await.is_err());
        assert!(policy.read(app, false).await.is_ok());
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='disabled' WHERE id=$1"),
                &[&app],
            )
            .await
            .unwrap();
        assert_eq!(
            policy
                .read(app, true)
                .await
                .unwrap()
                .status
                .effective_status,
            AppStatus::Disabled
        );
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='invalid' WHERE id=$1"),
                &[&app],
            )
            .await
            .unwrap();
        assert!(
            policy
                .read(app, true)
                .await
                .unwrap_err()
                .body()
                .message
                .contains("app status")
        );
        drop(policy);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn snapshot_topics_keep_status_fresh_and_release_the_status_lease_before_reload() {
        use crate::RealtimeInvalidationConsumer as _;
        let Some(url) = instant_test_support::test_database_url(
            "snapshot_topics_keep_status_fresh_and_release_the_status_lease_before_reload",
        ) else {
            return;
        };
        let Fixture {
            setup,
            schema,
            app,
            value,
            pool,
            policy,
            ..
        } = fixture(&url).await;
        let config = policy.config_app_id;
        let cache = Arc::new(crate::QueryControlCache::new(pool.clone(), config));
        let policy = policy.with_query_control_cache(Arc::clone(&cache));
        let before = pool.acquisition_count();
        // The fixture has one connection: retaining it across the snapshot miss
        // would deadlock rather than complete with two short sequential leases.
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), policy.read(app, true))
            .await
            .unwrap()
            .unwrap();
        assert!(first.use_coarse_topics);
        assert_eq!(pool.acquisition_count() - before, 2);
        let before = pool.acquisition_count();
        assert!(policy.read(app, true).await.unwrap().use_coarse_topics);
        assert_eq!(
            pool.acquisition_count() - before,
            1,
            "only status needs a lease on a cache hit"
        );
        setup
            .execute(
                &format!("UPDATE {schema}.triples SET value=$2 WHERE attr_id=$1"),
                &[&value, &Json(json!([]))],
            )
            .await
            .unwrap();
        cache
            .evict_cache(&instant_wal::WalCacheEviction::Attribute {
                app_id: Some(config),
                attr_id: None,
            })
            .await
            .unwrap();
        assert!(!policy.read(app, true).await.unwrap().use_coarse_topics);
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='read-only' WHERE id=$1"),
                &[&app],
            )
            .await
            .unwrap();
        assert_eq!(
            policy.read(app, true).await.unwrap().status.own_status,
            AppStatus::ReadOnly
        );
        // Make the next snapshot load fail. Disabled/invalid status and
        // schema-only refreshes must still take precedence over that failure.
        cache
            .evict_cache(&instant_wal::WalCacheEviction::Attribute {
                app_id: Some(config),
                attr_id: None,
            })
            .await
            .unwrap();
        setup
            .batch_execute(&format!(
                "ALTER TABLE {schema}.attrs RENAME TO unavailable_attrs"
            ))
            .await
            .unwrap();
        assert!(
            policy.read(app, true).await.is_err(),
            "never use stale topic controls on a reload failure"
        );
        assert!(!policy.read(app, false).await.unwrap().use_coarse_topics);
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='disabled' WHERE id=$1"),
                &[&app],
            )
            .await
            .unwrap();
        assert_eq!(
            policy.read(app, true).await.unwrap().status.own_status,
            AppStatus::Disabled
        );
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='invalid' WHERE id=$1"),
                &[&app],
            )
            .await
            .unwrap();
        assert!(
            policy
                .read(app, true)
                .await
                .unwrap_err()
                .body()
                .message
                .contains("app status")
        );
        drop(policy);
        drop(cache);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
