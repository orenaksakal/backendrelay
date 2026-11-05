use std::future::Future;
use std::pin::Pin;

use instant_http::{ApiError, ApiErrorType};
use tokio_postgres::Client;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::PostgresPool;
use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;

/// The three persisted app states, ordered from least to most restrictive.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum AppStatus {
    #[default]
    Active,
    ReadOnly,
    Disabled,
}

impl AppStatus {
    #[must_use]
    pub const fn parse(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"active" => Some(Self::Active),
            b"read-only" => Some(Self::ReadOnly),
            b"disabled" => Some(Self::Disabled),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::ReadOnly => "read-only",
            Self::Disabled => "disabled",
        }
    }

    /// Matches `instant.model.app/assert-read-allowed!`.
    ///
    /// Read-only apps retain data reads; disabled apps do not.
    ///
    /// # Errors
    ///
    /// Returns the public `app-disabled` error when this status is disabled.
    pub fn assert_read_allowed(self) -> Result<(), ApiError> {
        match self {
            Self::Active | Self::ReadOnly => Ok(()),
            Self::Disabled => Err(app_disabled_error()),
        }
    }

    /// Matches `instant.model.app/assert-write-allowed!`.
    ///
    /// # Errors
    ///
    /// Returns the matching public app-status error unless this status is
    /// active.
    pub fn assert_write_allowed(self) -> Result<(), ApiError> {
        match self {
            Self::Active => Ok(()),
            Self::ReadOnly => Err(app_read_only_error()),
            Self::Disabled => Err(app_disabled_error()),
        }
    }
}

/// One consistent read of an app's persisted status.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AppStatusSnapshot {
    pub own_status: AppStatus,
    pub effective_status: AppStatus,
}

pub type AppStatusFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AppStatusSnapshot, ApiError>> + Send + 'a>>;

/// Shared asynchronous status source for query and transaction boundaries.
pub trait EffectiveAppStatusPolicy: Send + Sync {
    fn status(&self, app_id: Uuid) -> AppStatusFuture<'_>;
}

/// PostgreSQL-backed app-status reader.
///
/// A missing or soft-deleted
/// app has an own status of `active`, matching the JVM's `get-own-status`
/// fallback; callers that require app existence must keep their normal record
/// lookup ahead of this policy check.
pub struct PostgresEffectiveAppStatusPolicy {
    client: PostgresClientSource,
    batch: Option<crate::fresh_control_reads::FreshControlReads<StatusQuery>>,
    control_plane: Option<std::sync::Arc<crate::ControlPlaneCache>>,
}

struct StatusQuery;

impl crate::fresh_control_reads::FreshControlQuery for StatusQuery {
    type Input = Uuid;
    type Output = Result<AppStatusSnapshot, ApiError>;

    async fn execute(
        &self,
        client: &crate::postgres_pool::PostgresClientGuard<'_>,
        apps: Vec<Uuid>,
    ) -> Result<Vec<Self::Output>, ApiError> {
        let rows = retry_once_on_query_cancellation(|| async {
            client
                .query_typed_cached(BATCH_STATUS_SQL, &[(&apps, Type::UUID_ARRAY)])
                .await
                .map_err(instant_pg::PgQueryError::from)
        })
        .await
        .map_err(database_error)?;
        Ok(rows
            .into_iter()
            .map(|row| status_snapshot(row.get::<_, String>(0)))
            .collect())
    }
}

impl std::fmt::Debug for PostgresEffectiveAppStatusPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PostgresEffectiveAppStatusPolicy")
            .field("client", &self.client)
            .field("batched", &self.batch.is_some())
            .finish()
    }
}

const BATCH_STATUS_SQL: &str = "SELECT COALESCE(app.status, 'active')
    FROM unnest($1::uuid[]) WITH ORDINALITY requested(id, position)
    LEFT JOIN apps app ON app.id=requested.id AND app.deletion_marked_at IS NULL
    ORDER BY requested.position";

impl PostgresEffectiveAppStatusPolicy {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            batch: None,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            batch: None,
            control_plane: None,
        }
    }

    /// Batch only requests queued before a new SQL execution. No completed or
    /// in-flight result is reused for requests arriving after dispatch. Every
    /// caller's statement snapshot therefore starts after its own request.
    pub(crate) fn with_batched_reads(mut self) -> Self {
        self.batch = Some(crate::fresh_control_reads::FreshControlReads::new(
            self.client.clone(),
            StatusQuery,
        ));
        self
    }

    /// Serves statuses from a snapshot evicted by `apps` row changes, as
    /// upstream's app model cache is.
    pub(crate) fn with_control_plane_cache(
        mut self,
        cache: std::sync::Arc<crate::ControlPlaneCache>,
    ) -> Self {
        self.control_plane = Some(cache);
        self
    }

    async fn read_status(&self, app_id: Uuid) -> Result<AppStatusSnapshot, ApiError> {
        if let Some(batch) = &self.batch {
            return batch.load(app_id).await?;
        }
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = retry_once_on_query_cancellation(|| async {
            client
                .query_typed_cached_one(
                    "SELECT COALESCE(
                           (SELECT app.status
                              FROM apps app
                             WHERE app.id = $1
                               AND app.deletion_marked_at IS NULL),
                           'active'
                         )",
                    &[(&app_id, Type::UUID)],
                )
                .await
                .map_err(instant_pg::PgQueryError::from)
        })
        .await
        .map_err(database_error)?;
        status_snapshot(row.get::<_, String>(0))
    }
}

fn status_snapshot(raw: String) -> Result<AppStatusSnapshot, ApiError> {
    let own_status = AppStatus::parse(&raw).ok_or_else(|| {
        ApiError::new(
            ApiErrorType::Unknown,
            format!("app status `{raw}` is not active, read-only, or disabled"),
        )
    })?;
    Ok(AppStatusSnapshot {
        own_status,
        effective_status: own_status,
    })
}

pub(crate) async fn retry_once_on_query_cancellation<T, F, Fut>(
    mut operation: F,
) -> Result<T, instant_pg::PgQueryError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, instant_pg::PgQueryError>>,
{
    match operation().await {
        Err(instant_pg::PgQueryError::QueryCancelled) => operation().await,
        result => result,
    }
}

impl EffectiveAppStatusPolicy for PostgresEffectiveAppStatusPolicy {
    fn status(&self, app_id: Uuid) -> AppStatusFuture<'_> {
        Box::pin(async move {
            match &self.control_plane {
                Some(cache) => cache.app_status(app_id, || self.read_status(app_id)).await,
                None => self.read_status(app_id).await,
            }
        })
    }
}

#[must_use]
pub fn app_read_only_error() -> ApiError {
    ApiError::new(ApiErrorType::AppReadOnly, "This app is in read-only mode.")
        .with_hint(serde_json::json!({"status": "read-only"}))
}

#[must_use]
pub fn app_disabled_error() -> ApiError {
    ApiError::new(ApiErrorType::AppDisabled, "This app is currently disabled.")
        .with_hint(serde_json::json!({"status": "disabled"}))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio_postgres::{NoTls, types::Json};

    use super::*;

    #[tokio::test]
    async fn batched_status_reads_preserve_position_scope_errors_and_next_read_freshness() {
        let Some(url) = instant_test_support::test_database_url(
            "batched_status_reads_preserve_position_scope_errors_and_next_read_freshness",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let schema = format!("status_batch_{}", Uuid::new_v4().simple());
        setup.batch_execute(&format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.apps(id uuid PRIMARY KEY,status text,deletion_marked_at timestamptz)")).await.unwrap();
        let active = Uuid::new_v4();
        let disabled = Uuid::new_v4();
        let invalid = Uuid::new_v4();
        let missing = Uuid::new_v4();
        let deleted = Uuid::new_v4();
        setup.execute(&format!("INSERT INTO {schema}.apps VALUES ($1,'active',NULL),($2,'disabled',NULL),($3,'invalid',NULL),($4,'disabled',now())"), &[&active,&disabled,&invalid,&deleted]).await.unwrap();
        let mut scoped = url::Url::parse(&url).unwrap();
        scoped
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PostgresPool::connect(scoped.as_str(), 1).await.unwrap();
        let policy = PostgresEffectiveAppStatusPolicy::from_pool(pool.clone()).with_batched_reads();
        let before = pool.acquisition_count();
        let (a, b, c, d, e, f) = tokio::join!(
            policy.status(active),
            policy.status(disabled),
            policy.status(invalid),
            policy.status(missing),
            policy.status(deleted),
            policy.status(active)
        );
        assert_eq!(
            pool.acquisition_count() - before,
            1,
            "simultaneous callers use one fresh statement"
        );
        assert_eq!(a.unwrap().own_status, AppStatus::Active);
        assert_eq!(b.unwrap().own_status, AppStatus::Disabled);
        assert!(
            c.is_err(),
            "one invalid app must not poison other positions"
        );
        assert_eq!(d.unwrap().own_status, AppStatus::Active);
        assert_eq!(e.unwrap().own_status, AppStatus::Active);
        assert_eq!(f.unwrap().own_status, AppStatus::Active);
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='disabled' WHERE id=$1"),
                &[&active],
            )
            .await
            .unwrap();
        assert_eq!(
            policy.status(active).await.unwrap().own_status,
            AppStatus::Disabled,
            "completed results are never cached"
        );
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET status='read-only' WHERE id=$1"),
                &[&active],
            )
            .await
            .unwrap();
        assert_eq!(
            policy.status(active).await.unwrap().own_status,
            AppStatus::ReadOnly
        );
        drop(policy);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn status_read_retries_one_typed_query_cancellation() {
        let calls = AtomicUsize::new(0);
        let result = retry_once_on_query_cancellation(|| {
            let call = calls.fetch_add(1, Ordering::Relaxed);
            async move {
                if call == 0 {
                    Err(instant_pg::PgQueryError::QueryCancelled)
                } else {
                    Ok(7)
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn status_read_does_not_retry_a_deterministic_database_error() {
        let calls = AtomicUsize::new(0);
        let result = retry_once_on_query_cancellation(|| {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Err::<(), _>(instant_pg::PgQueryError::InvalidBatchEnvelope))
        })
        .await;

        assert!(matches!(
            result,
            Err(instant_pg::PgQueryError::InvalidBatchEnvelope)
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn read_and_write_errors_match_the_jvm_wire_contract() {
        AppStatus::Active.assert_read_allowed().unwrap();
        AppStatus::ReadOnly.assert_read_allowed().unwrap();
        AppStatus::Active.assert_write_allowed().unwrap();

        let read_only = AppStatus::ReadOnly.assert_write_allowed().unwrap_err();
        assert_eq!(read_only.body().kind, ApiErrorType::AppReadOnly);
        assert_eq!(read_only.body().message, "This app is in read-only mode.");
        assert_eq!(
            read_only.body().hint,
            Some(serde_json::json!({"status": "read-only"}))
        );

        for error in [
            AppStatus::Disabled.assert_read_allowed().unwrap_err(),
            AppStatus::Disabled.assert_write_allowed().unwrap_err(),
        ] {
            assert_eq!(error.body().kind, ApiErrorType::AppDisabled);
            assert_eq!(error.body().message, "This app is currently disabled.");
            assert_eq!(
                error.body().hint,
                Some(serde_json::json!({"status": "disabled"}))
            );
        }
    }

    async fn postgres_client(database_url: &str) -> Client {
        let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client
    }

    #[tokio::test]
    async fn pooled_prepared_status_reads_observe_disable_and_keep_app_parameters_isolated() {
        let Some(url) = instant_test_support::test_database_url(
            "pooled_prepared_status_reads_observe_disable_and_keep_app_parameters_isolated",
        ) else {
            return;
        };
        let pool = PostgresPool::connect(&url, 1).await.unwrap();
        let app = Uuid::new_v4();
        let other = Uuid::new_v4();
        let client = pool.acquire().await.unwrap();
        client.batch_execute("CREATE TEMP TABLE apps (id uuid PRIMARY KEY, status text NOT NULL, deletion_marked_at timestamptz)").await.unwrap();
        client
            .execute(
                "INSERT INTO apps VALUES ($1,'active',NULL),($2,'active',NULL)",
                &[&app, &other],
            )
            .await
            .unwrap();
        drop(client);
        let policy = PostgresEffectiveAppStatusPolicy::from_pool(pool.clone());
        for _ in 0..8 {
            assert_eq!(
                policy.status(app).await.unwrap().effective_status,
                AppStatus::Active
            );
        }
        let client = pool.acquire().await.unwrap();
        client
            .execute("UPDATE apps SET status='disabled' WHERE id=$1", &[&app])
            .await
            .unwrap();
        drop(client);
        assert_eq!(
            policy.status(app).await.unwrap().effective_status,
            AppStatus::Disabled
        );
        assert_eq!(
            policy.status(other).await.unwrap().effective_status,
            AppStatus::Active
        );
        let client = pool.acquire().await.unwrap();
        client
            .execute("UPDATE apps SET status='invalid' WHERE id=$1", &[&app])
            .await
            .unwrap();
        drop(client);
        assert!(
            policy.status(app).await.is_err(),
            "cached SQL must not retain the previously valid status"
        );
    }

    #[tokio::test]
    async fn postgres_reader_uses_only_persisted_app_statuses() {
        let Some(database_url) = instant_test_support::test_database_url(
            "postgres_reader_uses_only_persisted_app_statuses",
        ) else {
            return;
        };
        let setup = postgres_client(&database_url).await;
        let schema = format!("app_status_policy_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.apps (
                   id uuid PRIMARY KEY, status text NOT NULL,
                   deletion_marked_at timestamptz
                 );
                 CREATE TABLE {schema}.attrs (
                   id uuid PRIMARY KEY, app_id uuid NOT NULL,
                   etype text NOT NULL, label text NOT NULL
                 );
                 CREATE TABLE {schema}.triples (
                   app_id uuid NOT NULL, entity_id uuid NOT NULL,
                   attr_id uuid NOT NULL, value jsonb NOT NULL
                 );"
            ))
            .await
            .unwrap();

        let app_id = Uuid::new_v4();
        let read_only_app_id = Uuid::new_v4();
        let disabled_app_id = Uuid::new_v4();
        let deleted_app_id = Uuid::new_v4();
        let config_app_id = Uuid::new_v4();
        let setting_attr_id = Uuid::new_v4();
        let value_attr_id = Uuid::new_v4();
        let flag_entity_id = Uuid::new_v4();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.apps VALUES
                       ($1, 'active', NULL), ($2, 'read-only', NULL),
                       ($3, 'disabled', NULL), ($4, 'read-only', now())"
                ),
                &[
                    &app_id,
                    &read_only_app_id,
                    &disabled_app_id,
                    &deleted_app_id,
                ],
            )
            .await
            .unwrap();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.attrs VALUES
                       ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')"
                ),
                &[&setting_attr_id, &value_attr_id, &config_app_id],
            )
            .await
            .unwrap();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.triples VALUES
                       ($1, $2, $3, $4), ($1, $2, $5, $6)"
                ),
                &[
                    &config_app_id,
                    &flag_entity_id,
                    &setting_attr_id,
                    &Json(serde_json::json!("sunset-stage")),
                    &value_attr_id,
                    &Json(serde_json::json!("disabled")),
                ],
            )
            .await
            .unwrap();

        let reader_client = postgres_client(&database_url).await;
        reader_client
            .batch_execute(&format!("SET search_path TO {schema}"))
            .await
            .unwrap();
        let policy = PostgresEffectiveAppStatusPolicy::new(reader_client);

        assert_eq!(
            policy.status(app_id).await.unwrap(),
            AppStatusSnapshot {
                own_status: AppStatus::Active,
                effective_status: AppStatus::Active,
            }
        );
        for (app_id, status) in [
            (read_only_app_id, AppStatus::ReadOnly),
            (disabled_app_id, AppStatus::Disabled),
        ] {
            assert_eq!(
                policy.status(app_id).await.unwrap(),
                AppStatusSnapshot {
                    own_status: status,
                    effective_status: status,
                }
            );
        }
        for missing_or_deleted in [Uuid::new_v4(), deleted_app_id] {
            let snapshot = policy.status(missing_or_deleted).await.unwrap();
            assert_eq!(snapshot.own_status, AppStatus::Active);
            assert_eq!(snapshot.effective_status, AppStatus::Active);
        }

        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
