use std::sync::Arc;

use instant_permissions::RateLimitConsumer;
use tokio_postgres::Client;

use crate::{
    ConfiguredOAuthCredentialProvider, OAuthSecretCipher, PostgresPool, PostgresRuntimeOAuthPolicy,
    RuntimeOAuthService, ScheduledTask, ServerConfig,
};

pub type ProductionRuntimeOAuthService =
    RuntimeOAuthService<ConfiguredOAuthCredentialProvider, PostgresRuntimeOAuthPolicy>;

pub struct ProductionRuntimeOAuthAdapters {
    pub backend: Arc<ProductionRuntimeOAuthService>,
    pub tasks: Vec<ScheduledTask>,
}

fn runtime_oauth_public_settings(config: &ServerConfig) -> (&str, bool) {
    (config.server_origin(), config.secure_cookies())
}

/// Pool-backed runtime OAuth composition used by the native server.
#[must_use]
pub fn production_runtime_oauth_service_from_pool(
    config: &ServerConfig,
    runtime_pool: PostgresPool,
    policy_pool: PostgresPool,
    oauth_secret_cipher: OAuthSecretCipher,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
) -> ProductionRuntimeOAuthService {
    let credentials =
        ConfiguredOAuthCredentialProvider::new(oauth_secret_cipher, config.shared_oauth_clients());
    let policy = PostgresRuntimeOAuthPolicy::from_pool(policy_pool)
        .with_rate_limit_consumer(rate_limit_consumer);
    let (server_origin, secure_cookie) = runtime_oauth_public_settings(config);
    RuntimeOAuthService::from_pool(
        runtime_pool,
        credentials,
        policy,
        server_origin,
        secure_cookie,
    )
}

/// Composes the production runtime OAuth coordinator from independently owned
/// PostgreSQL clients and secret-managed OAuth key material.
#[must_use]
pub fn production_runtime_oauth_service(
    config: &ServerConfig,
    runtime_client: Client,
    policy_client: Client,
    oauth_secret_cipher: OAuthSecretCipher,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
) -> ProductionRuntimeOAuthService {
    let credentials =
        ConfiguredOAuthCredentialProvider::new(oauth_secret_cipher, config.shared_oauth_clients());
    let policy = PostgresRuntimeOAuthPolicy::new(policy_client)
        .with_rate_limit_consumer(rate_limit_consumer);
    let (server_origin, secure_cookie) = runtime_oauth_public_settings(config);
    RuntimeOAuthService::new(
        runtime_client,
        credentials,
        policy,
        server_origin,
        secure_cookie,
    )
}

/// Returns the route backend together with its Google warm-up and hourly
/// discovery/JWKS refresh worker.
#[must_use]
pub fn production_runtime_oauth_adapters(
    config: &ServerConfig,
    runtime_client: Client,
    policy_client: Client,
    oauth_secret_cipher: OAuthSecretCipher,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
) -> ProductionRuntimeOAuthAdapters {
    let service = production_runtime_oauth_service(
        config,
        runtime_client,
        policy_client,
        oauth_secret_cipher,
        rate_limit_consumer,
    );
    let metadata = service.metadata_cache();
    ProductionRuntimeOAuthAdapters {
        backend: Arc::new(service),
        tasks: vec![crate::OAuthMetadataCache::refresh_task(metadata)],
    }
}

/// Returns the pool-backed route backend and its Google warm-up and hourly
/// discovery/JWKS refresh worker.
#[must_use]
pub fn production_runtime_oauth_adapters_from_pool(
    config: &ServerConfig,
    runtime_pool: PostgresPool,
    policy_pool: PostgresPool,
    oauth_secret_cipher: OAuthSecretCipher,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
) -> ProductionRuntimeOAuthAdapters {
    let service = production_runtime_oauth_service_from_pool(
        config,
        runtime_pool,
        policy_pool,
        oauth_secret_cipher,
        rate_limit_consumer,
    );
    let metadata = service.metadata_cache();
    ProductionRuntimeOAuthAdapters {
        backend: Arc::new(service),
        tasks: vec![crate::OAuthMetadataCache::refresh_task(metadata)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_secure_cookie_setting_reaches_runtime_oauth() {
        let config = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "https://api.example.test"),
            ("INSTANT_SECURE_COOKIES", "true"),
        ])
        .unwrap();

        assert_eq!(
            runtime_oauth_public_settings(&config),
            ("https://api.example.test", true)
        );
        assert!(!config.environment().is_hosted());

        let localhost = ServerConfig::from_pairs([
            ("INSTANT_BACKEND_URL", "http://localhost:8888"),
            ("INSTANT_SECURE_COOKIES", "true"),
        ])
        .unwrap();
        assert_eq!(
            runtime_oauth_public_settings(&localhost),
            ("http://localhost:8888", false)
        );
    }
}
