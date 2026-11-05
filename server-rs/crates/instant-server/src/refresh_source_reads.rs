//! Fresh source routing for concurrently queued realtime refreshes.
//! Never caches a native/BYOP decision or exposes connection strings.
use std::collections::HashSet;

use instant_http::ApiError;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::admin_data::internal_error;
use crate::fresh_control_reads::{FreshControlQuery, FreshControlReads};
use crate::postgres_pool::{DatabaseWorkload, PostgresClientGuard, PostgresClientSource};

pub(crate) struct RefreshSourceQuery;
pub(crate) type RefreshSourceReads = FreshControlReads<RefreshSourceQuery>;

pub(crate) async fn external_apps(
    client: &PostgresClientGuard<'_>,
    apps: &[Uuid],
) -> Result<HashSet<Uuid>, ApiError> {
    Ok(client
        .query_typed_cached(
            "SELECT id FROM apps WHERE id = ANY($1::uuid[]) AND connection_string IS NOT NULL",
            &[(&apps, Type::UUID_ARRAY)],
        )
        .await
        .map_err(internal_error)?
        .into_iter()
        .map(|row| row.get::<_, Uuid>(0))
        .collect())
}

pub(crate) fn reader(source: &PostgresClientSource) -> RefreshSourceReads {
    // Worker tasks do not inherit the caller's workload task-local. Freeze the
    // existing refresh reservation; do not create or expand any connection pool.
    FreshControlReads::new(
        source.for_workload(DatabaseWorkload::Refresh),
        RefreshSourceQuery,
    )
}

impl FreshControlQuery for RefreshSourceQuery {
    type Input = Uuid;
    type Output = bool;

    async fn execute(
        &self,
        client: &PostgresClientGuard<'_>,
        apps: Vec<Uuid>,
    ) -> Result<Vec<bool>, ApiError> {
        let external = external_apps(client, &apps).await?;
        // Preserve positions and duplicates, including the existing native
        // fallback for a missing app. This is routing, never app authorization.
        Ok(apps.iter().map(|app| external.contains(app)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PostgresPool;

    #[tokio::test]
    async fn queued_routes_are_fresh_scoped_and_do_not_reuse_completed_results() {
        let Some(url) = instant_test_support::test_database_url(
            "queued_routes_are_fresh_scoped_and_do_not_reuse_completed_results",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let schema = format!("refresh_source_{}", Uuid::new_v4().simple());
        setup.batch_execute(&format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.apps(id uuid PRIMARY KEY, connection_string bytea)")).await.unwrap();
        let native = Uuid::new_v4();
        let external = Uuid::new_v4();
        let missing = Uuid::new_v4();
        setup
            .execute(
                &format!("INSERT INTO {schema}.apps VALUES ($1,NULL),($2,decode('01','hex'))"),
                &[&native, &external],
            )
            .await
            .unwrap();
        let mut scoped = url::Url::parse(&url).unwrap();
        scoped
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PostgresPool::connect(scoped.as_str(), 1).await.unwrap();
        let source = PostgresClientSource::pooled_read_only(pool.clone());
        let reads = reader(&source);
        let before = pool.acquisition_count();
        let (a, b, c, d) = tokio::join!(
            reads.load(native),
            reads.load(external),
            reads.load(missing),
            reads.load(external)
        );
        assert!(!a.unwrap());
        assert!(b.unwrap());
        assert!(!c.unwrap());
        assert!(d.unwrap());
        assert_eq!(pool.acquisition_count() - before, 1);
        setup
            .execute(
                &format!(
                    "UPDATE {schema}.apps SET connection_string=decode('02','hex') WHERE id=$1"
                ),
                &[&native],
            )
            .await
            .unwrap();
        assert!(
            reads.load(native).await.unwrap(),
            "a later request must see newly configured BYOP"
        );
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET connection_string=NULL WHERE id=$1"),
                &[&external],
            )
            .await
            .unwrap();
        assert!(
            !reads.load(external).await.unwrap(),
            "native transition must not reuse an older routing result"
        );
        drop(reads);
        drop(source);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
