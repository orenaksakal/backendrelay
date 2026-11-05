use std::sync::Arc;

use instant_http::{AdminRequestContext, ApiError, ApiErrorType};
use instant_instaql::AttributeCatalog;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::admin_data::{
    AdminDataAuthorizer, AdminDataScope, RefreshCatalogSnapshot, internal_error,
};
use crate::admin_metadata::load_attribute_catalog;
use crate::postgres_pool::{DatabaseWorkload, PostgresClientSource};
use crate::{PostgresPool, WebhookBearerAuthorizer};

const INVALID_ADMIN_TOKEN: &str =
    "This admin token may be expired or invalid. Or you may have provided an incorrect app ID.";

/// PostgreSQL-backed app admin-token validation and attribute catalog loading.
///
/// UUID app admin tokens are matched to the requested app. Other token families
/// remain denied until their scoped platform authorization is composed explicitly.
pub struct PostgresAdminDataAuthorizer {
    client: PostgresClientSource,
    catalog_reads: Option<crate::refresh_catalog_reads::RefreshCatalogReads>,
    control_plane: Option<Arc<crate::ControlPlaneCache>>,
}

impl PostgresAdminDataAuthorizer {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            catalog_reads: None,
            control_plane: None,
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self {
            client: PostgresClientSource::pooled_read_only(pool),
            catalog_reads: None,
            control_plane: None,
        }
    }

    pub(crate) fn with_fresh_catalog_reads(mut self, metadata_pool: PostgresPool) -> Self {
        self.catalog_reads = Some(crate::refresh_catalog_reads::reader(metadata_pool));
        self
    }

    /// Serves runtime catalogs and source routing from WAL-invalidated snapshots.
    /// Admin-token authorization keeps its per-request read.
    pub(crate) fn with_control_plane_cache(mut self, cache: Arc<crate::ControlPlaneCache>) -> Self {
        self.control_plane = Some(cache);
        self
    }

    async fn read_catalog(&self, app_id: Uuid) -> Result<Arc<AttributeCatalog>, ApiError> {
        // Foreground reads and invalidation-predicate compilation do not queue
        // behind the data queries they enable while the explicit metadata
        // reservation is idle. The reader collects only after checkout.
        if let Some(reads) = &self.catalog_reads {
            return reads
                .load((app_id, false))
                .await?
                .map(|snapshot| snapshot.catalog);
        }
        let client = self.client.acquire().await.map_err(internal_error)?;
        load_attribute_catalog(&client, app_id).await.map(Arc::new)
    }
}

impl AdminDataAuthorizer for PostgresAdminDataAuthorizer {
    async fn authorize(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        let client = self.client.acquire().await.map_err(internal_error)?;
        authorize_context(&client, &context, scope).await?;
        load_attribute_catalog(&client, context.app_id).await
    }

    async fn authorize_only(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<(), ApiError> {
        let client = self.client.acquire().await.map_err(internal_error)?;
        authorize_context(&client, &context, scope).await
    }

    async fn load_catalog(&self, app_id: Uuid) -> Result<AttributeCatalog, ApiError> {
        self.load_catalog_shared(app_id)
            .await
            .map(|catalog| (*catalog).clone())
    }

    async fn load_catalog_shared(&self, app_id: Uuid) -> Result<Arc<AttributeCatalog>, ApiError> {
        match &self.control_plane {
            Some(cache) => cache.catalog(app_id, || self.read_catalog(app_id)).await,
            None => self.read_catalog(app_id).await,
        }
    }

    async fn load_refresh_catalog(&self, app_id: Uuid) -> Result<RefreshCatalogSnapshot, ApiError> {
        if DatabaseWorkload::current() == DatabaseWorkload::Refresh
            && let Some(reads) = &self.catalog_reads
        {
            let Some(cache) = &self.control_plane else {
                return reads.load((app_id, true)).await?;
            };
            if let Some(external) = cache.cached_external_source(app_id) {
                return Ok(RefreshCatalogSnapshot {
                    catalog: self.load_catalog_shared(app_id).await?,
                    source: Some((app_id, external)),
                });
            }
            let routing = cache.begin_external_source_read(app_id);
            let snapshot = reads.load((app_id, true)).await??;
            if let Some((_, external)) = snapshot.source {
                cache.finish_external_source_read(app_id, routing, external);
            }
            return Ok(snapshot);
        }
        self.load_catalog_shared(app_id)
            .await
            .map(RefreshCatalogSnapshot::from)
    }
}

async fn authorize_context(
    client: &Client,
    context: &AdminRequestContext,
    scope: AdminDataScope,
) -> Result<(), ApiError> {
    let bearer = context
        .bearer_token
        .as_deref()
        .ok_or_else(invalid_admin_token)?;
    if bearer.starts_with("pat_") || bearer.starts_with("per_") {
        return authorize_platform_token(client, bearer, context.app_id, scope).await;
    }
    let token = Uuid::parse_str(bearer).map_err(|_| invalid_admin_token())?;
    let valid = client
        .query_opt(
            "SELECT 1 FROM app_admin_tokens WHERE token = $1 AND app_id = $2",
            &[&token, &context.app_id],
        )
        .await
        .map_err(internal_error)?
        .is_some();
    if valid {
        Ok(())
    } else {
        Err(invalid_admin_token())
    }
}

impl WebhookBearerAuthorizer for PostgresAdminDataAuthorizer {
    async fn authorize(&self, app_id: Uuid, bearer: &str) -> Result<(), ApiError> {
        let client = self.client.acquire().await.map_err(internal_error)?;
        if bearer.starts_with("pat_") || bearer.starts_with("per_") {
            authorize_platform_token(&client, bearer, app_id, AdminDataScope::Read).await
        } else {
            let token = Uuid::parse_str(bearer).map_err(|_| invalid_admin_token())?;
            let valid = client
                .query_opt(
                    "SELECT 1 FROM app_admin_tokens WHERE token = $1 AND app_id = $2",
                    &[&token, &app_id],
                )
                .await
                .map_err(internal_error)?
                .is_some();
            if valid {
                Ok(())
            } else {
                Err(invalid_admin_token())
            }
        }
    }
}

async fn authorize_platform_token(
    client: &Client,
    bearer: &str,
    app_id: Uuid,
    scope: AdminDataScope,
) -> Result<(), ApiError> {
    let digest = Sha256::digest(bearer.as_bytes()).to_vec();
    let user_id: Uuid = if bearer.starts_with("pat_") {
        let row = client
            .query_opt(
                "SELECT user_id, scopes FROM instant_user_oauth_access_tokens
                 WHERE lookup_key = $1 AND expires_at > now()",
                &[&digest],
            )
            .await
            .map_err(internal_error)?
            .ok_or_else(invalid_platform_token)?;
        let scopes: Vec<String> = row.get(1);
        if !satisfies_scope(&scopes, scope) {
            return Err(missing_scope_error(scope));
        }
        row.get(0)
    } else {
        client
            .query_opt(
                "SELECT user_id FROM instant_personal_access_tokens WHERE lookup_key = $1",
                &[&digest],
            )
            .await
            .map_err(internal_error)?
            .ok_or_else(invalid_platform_token)?
            .get(0)
    };
    let owns_app = client
        .query_opt(
            "SELECT 1 FROM apps
             WHERE id = $1 AND creator_id = $2 AND deletion_marked_at IS NULL",
            &[&app_id, &user_id],
        )
        .await
        .map_err(internal_error)?
        .is_some();
    if !owns_app {
        return Err(ApiError::new(
            ApiErrorType::RecordNotFound,
            "Record not found: app",
        ));
    }
    Ok(())
}

const fn scope_name(scope: AdminDataScope) -> &'static str {
    match scope {
        AdminDataScope::AppsRead => "apps-read",
        AdminDataScope::AppsWrite => "apps-write",
        AdminDataScope::Read => "data-read",
        AdminDataScope::Write => "data-write",
        AdminDataScope::StorageRead => "storage-read",
        AdminDataScope::StorageWrite => "storage-write",
    }
}

fn satisfies_scope(scopes: &[String], required: AdminDataScope) -> bool {
    scopes.iter().any(|scope| {
        scope == scope_name(required)
            || matches!(
                (required, scope.as_str()),
                (AdminDataScope::AppsRead, "apps-write")
                    | (AdminDataScope::Read, "data-write")
                    | (AdminDataScope::StorageRead, "storage-write")
            )
    })
}

fn missing_scope_error(required: AdminDataScope) -> ApiError {
    let required = scope_name(required);
    ApiError::new(
        ApiErrorType::PermissionDenied,
        format!("You are missing the {required} scope"),
    )
    .with_hint(json!({"required-scope": required}))
}

fn invalid_admin_token() -> ApiError {
    ApiError::new(ApiErrorType::PermissionDenied, INVALID_ADMIN_TOKEN).unauthorized()
}

fn invalid_platform_token() -> ApiError {
    ApiError::new(ApiErrorType::RecordNotFound, "Record not found: token").unauthorized()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn foreground_catalog_reads_bypass_saturated_data_pools_without_extra_connections() {
        let Some(url) = instant_test_support::test_database_url(
            "foreground_catalog_reads_bypass_saturated_data_pools_without_extra_connections",
        ) else {
            return;
        };
        let pool = PostgresPool::connect_workload_partitioned(&url, 16)
            .await
            .unwrap();
        let refresh = pool.for_workload(DatabaseWorkload::Refresh);
        let metadata = pool.control_pool();
        let authorizer = PostgresAdminDataAuthorizer::from_pool(pool.clone())
            .with_fresh_catalog_reads(metadata.clone());
        let mut held = Vec::new();
        for _ in 0..pool.max_size() {
            held.push(pool.acquire().await.unwrap());
        }
        for _ in 0..refresh.max_size() {
            held.push(refresh.acquire().await.unwrap());
        }
        let before = metadata.acquisition_count();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            DatabaseWorkload::Interactive.scope(authorizer.load_catalog(Uuid::new_v4())),
        )
        .await
        .expect("foreground metadata must not wait for a data reservation")
        .unwrap();
        assert_eq!(metadata.acquisition_count() - before, 1);
        assert_eq!(held.len(), 12);
        assert_eq!(
            pool.max_size() + refresh.max_size() + metadata.max_size(),
            16
        );
        assert!(metadata.status().available > 0);
    }

    #[tokio::test]
    async fn foreground_catalog_reads_are_app_scoped_and_observe_the_next_schema_change() {
        let Some(url) = instant_test_support::test_database_url(
            "foreground_catalog_reads_are_app_scoped_and_observe_the_next_schema_change",
        ) else {
            return;
        };
        let (setup, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let schema = format!("foreground_catalog_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema}; CREATE TABLE {schema}.attrs AS SELECT * FROM public.attrs WITH NO DATA"
            ))
            .await
            .unwrap();
        let app_a = Uuid::new_v4();
        let app_b = Uuid::new_v4();
        let attr = Uuid::new_v4();
        setup
            .execute(
                &format!(
                    "INSERT INTO {schema}.attrs (id,app_id,value_type,cardinality,is_unique,is_indexed,is_required,forward_ident,etype,label) VALUES ($1,$2,'blob','one',false,true,false,$1,'items','first')"
                ),
                &[&attr, &app_a],
            )
            .await
            .unwrap();
        let mut scoped = url::Url::parse(&url).unwrap();
        scoped
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        let pool = PostgresPool::connect_workload_partitioned(scoped.as_str(), 16)
            .await
            .unwrap();
        let authorizer = PostgresAdminDataAuthorizer::from_pool(pool.clone())
            .with_fresh_catalog_reads(pool.control_pool());
        let (a, b) = tokio::join!(
            authorizer.load_catalog(app_a),
            authorizer.load_catalog(app_b)
        );
        let a = a.unwrap();
        assert_eq!(a.attributes.len(), 1);
        assert_eq!(a.attributes[0].label, "first");
        assert!(b.unwrap().attributes.is_empty());
        setup
            .execute(
                &format!("UPDATE {schema}.attrs SET label='changed' WHERE id=$1"),
                &[&attr],
            )
            .await
            .unwrap();
        let changed = authorizer.load_catalog(app_a).await.unwrap();
        assert_eq!(changed.attributes[0].label, "changed");
        assert_eq!(a.attributes[0].label, "first");
        setup
            .execute(&format!("DELETE FROM {schema}.attrs WHERE id=$1"), &[&attr])
            .await
            .unwrap();
        assert!(
            authorizer
                .load_catalog(app_a)
                .await
                .unwrap()
                .attributes
                .is_empty()
        );
        drop(authorizer);
        drop(pool);
        setup
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
    #[test]
    fn platform_scopes_are_isolated_and_write_only_implies_same_family_read() {
        let scopes = [
            (AdminDataScope::AppsRead, "apps-read"),
            (AdminDataScope::AppsWrite, "apps-write"),
            (AdminDataScope::Read, "data-read"),
            (AdminDataScope::Write, "data-write"),
            (AdminDataScope::StorageRead, "storage-read"),
            (AdminDataScope::StorageWrite, "storage-write"),
        ];

        for (required, required_name) in scopes {
            for (granted, granted_name) in scopes {
                let expected = required == granted
                    || matches!(
                        (required, granted),
                        (AdminDataScope::AppsRead, AdminDataScope::AppsWrite)
                            | (AdminDataScope::Read, AdminDataScope::Write)
                            | (AdminDataScope::StorageRead, AdminDataScope::StorageWrite)
                    );
                assert_eq!(
                    satisfies_scope(&[granted_name.to_owned()], required),
                    expected,
                    "{granted_name} satisfying {required_name}"
                );
            }
        }
    }

    #[test]
    fn every_missing_scope_error_names_and_hints_the_exact_required_scope() {
        for (scope, name) in [
            (AdminDataScope::AppsRead, "apps-read"),
            (AdminDataScope::AppsWrite, "apps-write"),
            (AdminDataScope::Read, "data-read"),
            (AdminDataScope::Write, "data-write"),
            (AdminDataScope::StorageRead, "storage-read"),
            (AdminDataScope::StorageWrite, "storage-write"),
        ] {
            let error = missing_scope_error(scope);
            assert_eq!(error.body().kind, ApiErrorType::PermissionDenied);
            assert_eq!(
                error.body().message,
                format!("You are missing the {name} scope")
            );
            assert_eq!(error.body().hint, Some(json!({"required-scope": name})));
        }
    }
}
