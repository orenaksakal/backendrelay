use std::sync::Arc;

use thiserror::Error;
use tokio_postgres::Client;

use crate::postgres_pool::PostgresClientSource;
use crate::{
    Ed25519WebhookKeys, JwtOrBearerWebhookPayloadAuthorizer, PlatformService,
    PostgresAdminDataAuthorizer, PostgresPool, PostgresWebhookPayloadStore,
    S3WebhookHistoryArchive, ServerConfig, StorageObjectReader, StorageObjectStore,
    StorageWebhookFileUrlSigner,
};

pub type ProductionPlatformService<S> = PlatformService<
    PostgresWebhookPayloadStore<
        JwtOrBearerWebhookPayloadAuthorizer<PostgresAdminDataAuthorizer>,
        StorageWebhookFileUrlSigner<S>,
        S3WebhookHistoryArchive<S>,
    >,
>;

#[derive(Debug, Error)]
pub enum ProductionPlatformError {
    #[error("WAL history bucket must not be empty")]
    EmptyHistoryBucket,
    #[error("production platform configuration is invalid: {0}")]
    Invalid(String),
}

/// Builds the full platform stack for webhook verification and replay.
///
/// # Errors
///
/// Returns an error when required platform settings or verification material are invalid.
#[allow(clippy::too_many_arguments)]
pub fn production_platform_service_from_config<S>(
    config: &ServerConfig,
    replay_client: Client,
    bearer_client: Client,
    objects: &Arc<S>,
    webhook_keys: &Arc<Ed25519WebhookKeys>,
) -> Result<ProductionPlatformService<S>, ProductionPlatformError>
where
    S: StorageObjectStore + StorageObjectReader,
{
    production_platform_service_with_optional_archive(
        PostgresClientSource::fixed(replay_client),
        PostgresAdminDataAuthorizer::new(bearer_client),
        objects,
        webhook_keys,
        config.wal_history().bucket.clone(),
        config.wal_history().prefix.clone(),
    )
}

/// Builds the production platform stack with a shared bounded bearer-token
/// authorization pool while retaining a dedicated replay client.
///
/// # Errors
///
/// Returns an error when required platform settings or verification material are invalid.
#[allow(clippy::too_many_arguments)]
pub fn production_platform_service_from_config_with_bearer_pool<S>(
    config: &ServerConfig,
    replay_client: Client,
    bearer_pool: PostgresPool,
    objects: &Arc<S>,
    webhook_keys: &Arc<Ed25519WebhookKeys>,
) -> Result<ProductionPlatformService<S>, ProductionPlatformError>
where
    S: StorageObjectStore + StorageObjectReader,
{
    production_platform_service_with_optional_archive(
        PostgresClientSource::fixed(replay_client),
        PostgresAdminDataAuthorizer::from_pool(bearer_pool),
        objects,
        webhook_keys,
        config.wal_history().bucket.clone(),
        config.wal_history().prefix.clone(),
    )
}

/// Builds the production platform stack with shared bounded pools for bearer
/// authorization and webhook-history replay reads.
///
/// # Errors
///
/// Returns an error when required platform settings or verification material are invalid.
#[allow(clippy::too_many_arguments)]
pub fn production_platform_service_from_config_with_pools<S>(
    config: &ServerConfig,
    replay_pool: PostgresPool,
    bearer_pool: PostgresPool,
    objects: &Arc<S>,
    webhook_keys: &Arc<Ed25519WebhookKeys>,
) -> Result<ProductionPlatformService<S>, ProductionPlatformError>
where
    S: StorageObjectStore + StorageObjectReader,
{
    production_platform_service_with_optional_archive(
        PostgresClientSource::pooled_read_only(replay_pool),
        PostgresAdminDataAuthorizer::from_pool(bearer_pool),
        objects,
        webhook_keys,
        config.wal_history().bucket.clone(),
        config.wal_history().prefix.clone(),
    )
}

/// Composes the replay-auth, file-signing, and JVM-compatible S3 history
/// adapters used by the platform route group.
///
/// # Errors
///
/// Returns an error when required history settings or platform verification
/// material are invalid.
#[allow(clippy::too_many_arguments)]
pub fn production_platform_service<S>(
    replay_client: Client,
    bearer_client: Client,
    objects: &Arc<S>,
    webhook_keys: &Arc<Ed25519WebhookKeys>,
    history_bucket: impl Into<String>,
    history_prefix: Option<String>,
) -> Result<ProductionPlatformService<S>, ProductionPlatformError>
where
    S: StorageObjectStore + StorageObjectReader,
{
    let history_bucket = history_bucket.into();
    if history_bucket.trim().is_empty() {
        return Err(ProductionPlatformError::EmptyHistoryBucket);
    }
    production_platform_service_with_optional_archive(
        PostgresClientSource::fixed(replay_client),
        PostgresAdminDataAuthorizer::new(bearer_client),
        objects,
        webhook_keys,
        Some(history_bucket),
        history_prefix,
    )
}

fn production_platform_service_with_optional_archive<S>(
    replay_client: PostgresClientSource,
    bearer: PostgresAdminDataAuthorizer,
    objects: &Arc<S>,
    webhook_keys: &Arc<Ed25519WebhookKeys>,
    history_bucket: Option<String>,
    history_prefix: Option<String>,
) -> Result<ProductionPlatformService<S>, ProductionPlatformError>
where
    S: StorageObjectStore + StorageObjectReader,
{
    if history_bucket
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(ProductionPlatformError::EmptyHistoryBucket);
    }
    let authorizer = Arc::new(JwtOrBearerWebhookPayloadAuthorizer::new(
        Arc::clone(webhook_keys),
        Arc::new(bearer),
    ));
    let file_urls = Arc::new(StorageWebhookFileUrlSigner::new(Arc::clone(objects)));
    let archive = Arc::new(S3WebhookHistoryArchive::configured(
        Arc::clone(objects),
        history_bucket,
        history_prefix,
    ));
    let payloads = PostgresWebhookPayloadStore::with_client_source(
        replay_client,
        authorizer,
        file_urls,
        archive,
    );
    PlatformService::new(payloads, webhook_keys.public_jwks())
        .map_err(ProductionPlatformError::Invalid)
}
