//! Coalesce queued metadata reads, never catalog values or authorization.
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock};

use instant_http::ApiError;
use instant_instaql::SYSTEM_CATALOG_APP_ID;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::PostgresPool;
use crate::admin_data::{RefreshCatalogSnapshot, internal_error};
use crate::admin_metadata::{ATTRIBUTE_COLUMNS, attributes_to_catalog, row_to_attribute};
use crate::fresh_control_reads::{FreshControlQuery, FreshControlReads};
use crate::postgres_pool::{DatabaseWorkload, PostgresClientGuard, PostgresClientSource};

pub(crate) struct RefreshCatalogQuery;
pub(crate) type RefreshCatalogReads = FreshControlReads<RefreshCatalogQuery>;

static CATALOG_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT {ATTRIBUTE_COLUMNS} FROM attrs
     WHERE (app_id = ANY($1::uuid[]) OR app_id = $2)
       AND deletion_marked_at IS NULL ORDER BY id"
    )
});

pub(crate) fn reader(pool: PostgresPool) -> RefreshCatalogReads {
    // Pin the explicitly supplied reservation before spawning the reader.
    // Production supplies the existing control pool, not the refresh data pool.
    // Only metadata is executed here, and its lease ends before callers resume.
    FreshControlReads::new(
        PostgresClientSource::pooled_read_only(pool.for_workload(DatabaseWorkload::Interactive)),
        RefreshCatalogQuery,
    )
}

impl FreshControlQuery for RefreshCatalogQuery {
    type Input = (Uuid, bool);
    type Output = Result<RefreshCatalogSnapshot, ApiError>;

    async fn execute(
        &self,
        client: &PostgresClientGuard<'_>,
        inputs: Vec<Self::Input>,
    ) -> Result<Vec<Self::Output>, ApiError> {
        let apps = inputs
            .iter()
            .map(|(app, _)| *app)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let source_apps = inputs
            .iter()
            .filter_map(|(app, include)| include.then_some(*app))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        // Both statements start after this batch's checkout/collection boundary.
        // Pipeline independent metadata reads on the same lease; later requests
        // never join these results. Ordinary catalog-only callers need no apps SQL.
        let (rows, external) = tokio::join!(
            async {
                client
                    .query_typed_cached(
                        CATALOG_SQL.as_str(),
                        &[
                            (&apps, Type::UUID_ARRAY),
                            (&SYSTEM_CATALOG_APP_ID, Type::UUID),
                        ],
                    )
                    .await
                    .map_err(internal_error)
            },
            async {
                if source_apps.is_empty() {
                    Ok(Default::default())
                } else {
                    crate::refresh_source_reads::external_apps(client, &source_apps).await
                }
            }
        );
        let rows = rows?;
        // Partition once instead of scanning every returned catalog for every
        // requested app. Ordinal merges retain the SQL's exact global ID order,
        // including duplicate IDs in custom compatibility schemas.
        let mut by_app = HashMap::<Uuid, Vec<usize>>::new();
        for (position, row) in rows.iter().enumerate() {
            by_app.entry(row.get(1)).or_default().push(position);
        }
        let system = by_app
            .get(&SYSTEM_CATALOG_APP_ID)
            .map_or(&[][..], Vec::as_slice);
        let mut catalogs = HashMap::new();
        for app in &apps {
            catalogs.entry(*app).or_insert_with(|| {
                // Keep the same ID order, system attributes and conversion/error
                // semantics as load_attribute_catalog. Invalid metadata for one
                // app must not poison another app in the same fresh statement.
                let own = if *app == SYSTEM_CATALOG_APP_ID {
                    &[][..]
                } else {
                    by_app.get(app).map_or(&[][..], Vec::as_slice)
                };
                let attributes = merge_positions(system, own)
                    .map(|position| row_to_attribute(&rows[position]))
                    .collect::<Result<Vec<_>, _>>()?;
                attributes_to_catalog(&attributes)
            });
        }
        Ok(inputs
            .iter()
            .map(|(app, include_source)| {
                let catalog = catalogs
                    .get(app)
                    .expect("every batch position was resolved")
                    .clone()?;
                let source = if *include_source {
                    Some((*app, external.as_ref().map_err(Clone::clone)?.contains(app)))
                } else {
                    None
                };
                Ok(RefreshCatalogSnapshot {
                    catalog: Arc::new(catalog),
                    source,
                })
            })
            .collect())
    }
}

fn merge_positions<'a>(left: &'a [usize], right: &'a [usize]) -> impl Iterator<Item = usize> + 'a {
    let mut left = left.iter().copied().peekable();
    let mut right = right.iter().copied().peekable();
    std::iter::from_fn(move || match (left.peek(), right.peek()) {
        (Some(a), Some(b)) if a > b => right.next(),
        (Some(_), _) => left.next(),
        (None, _) => right.next(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_data::AdminDataAuthorizer;
    use crate::admin_metadata::load_attribute_catalog;
    use crate::postgres_admin_authorizer::PostgresAdminDataAuthorizer;
    use instant_instaql::AttributeCatalog;

    #[test]
    fn catalog_partition_merge_retains_statement_order_without_cross_app_rows() {
        assert_eq!(
            merge_positions(&[1, 3, 7], &[0, 4, 9]).collect::<Vec<_>>(),
            vec![0, 1, 3, 4, 7, 9]
        );
        assert_eq!(
            merge_positions(&[], &[2, 5]).collect::<Vec<_>>(),
            vec![2, 5]
        );
        assert_eq!(
            merge_positions(&[1, 3], &[]).collect::<Vec<_>>(),
            vec![1, 3]
        );
        let system = (0..30_000).step_by(3).collect::<Vec<_>>();
        let own = (1..30_000).step_by(3).collect::<Vec<_>>();
        let positions = merge_positions(&system, &own).collect::<Vec<_>>();
        assert_eq!(positions.len(), 20_000);
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(positions.iter().all(|position| position % 3 != 2));
    }

    #[tokio::test]
    async fn refresh_metadata_uses_configured_reservation_when_both_data_pools_are_full() {
        let Some(url) = instant_test_support::test_database_url(
            "refresh_metadata_uses_configured_reservation_when_both_data_pools_are_full",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&url, 16)
            .await
            .unwrap();
        let refresh = pool.for_workload(DatabaseWorkload::Refresh);
        let metadata = pool.control_pool();
        let mut data_leases = Vec::new();
        for _ in 0..pool.max_size() {
            data_leases.push(pool.acquire().await.unwrap());
        }
        for _ in 0..refresh.max_size() {
            data_leases.push(refresh.acquire().await.unwrap());
        }
        let authorizer = PostgresAdminDataAuthorizer::from_pool(pool.clone())
            .with_fresh_catalog_reads(metadata.clone());
        let app = Uuid::new_v4();
        let before = metadata.acquisition_count();
        let snapshot = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            DatabaseWorkload::Refresh.scope(authorizer.load_refresh_catalog(app)),
        )
        .await
        .expect("metadata must not wait for either saturated data reservation")
        .unwrap();
        assert_eq!(snapshot.source, Some((app, false)));
        assert_eq!(metadata.acquisition_count() - before, 1);
        assert_eq!(
            data_leases.len(),
            12,
            "the existing 6/6/4 budget is unchanged"
        );
        // The reader finishes and releases metadata before returning results.
        assert!(metadata.status().available > 0);
        drop(data_leases);
    }

    #[tokio::test]
    async fn batched_refresh_catalogs_match_fresh_reads_and_keep_app_scope() {
        let Some(url) = instant_test_support::test_database_url(
            "batched_refresh_catalogs_match_fresh_reads_and_keep_app_scope",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let schema = format!("catalog_batch_{}", Uuid::new_v4().simple());
        setup.batch_execute(&format!("CREATE SCHEMA {schema}; CREATE TABLE {schema}.attrs AS SELECT * FROM public.attrs WITH NO DATA; CREATE TABLE {schema}.apps(id uuid PRIMARY KEY, connection_string bytea)")).await.unwrap();
        let app_a = Uuid::new_v4();
        let app_b = Uuid::new_v4();
        let missing = Uuid::new_v4();
        setup
            .execute(
                &format!("INSERT INTO {schema}.apps VALUES ($1,NULL),($2,decode('01','hex'))"),
                &[&app_a, &app_b],
            )
            .await
            .unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let system = Uuid::new_v4();
        for (app, id, label) in [
            (app_a, a, "a"),
            (app_b, b, "b"),
            (SYSTEM_CATALOG_APP_ID, system, "system"),
        ] {
            setup.execute(&format!("INSERT INTO {schema}.attrs (id,app_id,value_type,cardinality,is_unique,is_indexed,is_required,forward_ident,etype,label) VALUES ($1,$2,'blob','one',false,true,false,$1,'items',$3)"), &[&id,&app,&label]).await.unwrap();
        }
        let mut scoped = url::Url::parse(&url).unwrap();
        scoped
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PostgresPool::connect(scoped.as_str(), 1).await.unwrap();
        let source = PostgresClientSource::pooled_read_only(pool.clone());
        let reads = reader(pool.clone());
        let before = pool.acquisition_count();
        let (ca, cb, cm, ca2, cs) = tokio::join!(
            reads.load((app_a, true)),
            reads.load((app_b, true)),
            reads.load((missing, true)),
            reads.load((app_a, false)),
            reads.load((SYSTEM_CATALOG_APP_ID, false))
        );
        assert_eq!(pool.acquisition_count() - before, 1);
        let ca = ca.unwrap().unwrap();
        let cb = cb.unwrap().unwrap();
        let cm = cm.unwrap().unwrap();
        let ca2 = ca2.unwrap().unwrap();
        let cs = cs.unwrap().unwrap();
        assert_eq!(ca.source, Some((app_a, false)));
        assert_eq!(cb.source, Some((app_b, true)));
        assert_eq!(cm.source, Some((missing, false)));
        assert_eq!(
            ca2.source, None,
            "catalog-only callers confer no source hint"
        );
        let (ca, cb, cm) = (ca.catalog, cb.catalog, cm.catalog);
        let ids = |catalog: &AttributeCatalog| {
            catalog
                .attributes
                .iter()
                .map(|attribute| attribute.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&ca), ids(&ca2.catalog));
        assert!(ids(&ca).contains(&a) && !ids(&ca).contains(&b));
        assert!(ids(&cb).contains(&b) && !ids(&cb).contains(&a));
        assert_eq!(ids(&cm), vec![system]);
        assert_eq!(
            ids(&cs.catalog),
            vec![system],
            "system catalog requests must not duplicate system rows"
        );
        let client = source.acquire().await.unwrap();
        assert_eq!(
            ids(&ca),
            ids(&load_attribute_catalog(&client, app_a).await.unwrap())
        );
        drop(client);
        setup
            .execute(
                &format!("UPDATE {schema}.attrs SET label='updated' WHERE id=$1"),
                &[&a],
            )
            .await
            .unwrap();
        setup
            .execute(
                &format!(
                    "UPDATE {schema}.apps SET connection_string=decode('02','hex') WHERE id=$1"
                ),
                &[&app_a],
            )
            .await
            .unwrap();
        let updated = reads.load((app_a, true)).await.unwrap().unwrap();
        assert_eq!(
            updated.source,
            Some((app_a, true)),
            "later refreshes must see native-to-BYOP transitions"
        );
        assert_eq!(
            updated
                .catalog
                .attributes
                .iter()
                .find(|attr| attr.id == a)
                .unwrap()
                .label,
            "updated"
        );
        setup
            .execute(
                &format!("UPDATE {schema}.attrs SET deletion_marked_at=now() WHERE id=$1"),
                &[&a],
            )
            .await
            .unwrap();
        setup
            .execute(
                &format!("UPDATE {schema}.apps SET connection_string=NULL WHERE id=$1"),
                &[&app_a],
            )
            .await
            .unwrap();
        let deleted = reads.load((app_a, true)).await.unwrap().unwrap();
        assert!(!ids(&deleted.catalog).contains(&a));
        assert_eq!(
            deleted.source,
            Some((app_a, false)),
            "later refreshes must see BYOP-to-native transitions"
        );
        // Routing failures do not poison catalog-only callers, including custom
        // authorizers using a schema with no apps relation.
        setup
            .batch_execute(&format!("DROP TABLE {schema}.apps"))
            .await
            .unwrap();
        let (catalog_only, with_source) =
            tokio::join!(reads.load((app_a, false)), reads.load((app_b, true)));
        assert!(catalog_only.unwrap().is_ok());
        assert!(with_source.unwrap().is_err());
        drop(reads);
        drop(source);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
}
