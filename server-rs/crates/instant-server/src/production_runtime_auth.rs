use std::sync::Arc;

use instant_permissions::RateLimitConsumer;
use tokio_postgres::Client;

use crate::{
    MagicCodeDelivery, MagicCodeExpiryPolicy, MagicCodeRateLimiter, PostgresPool,
    PostgresRuntimeOAuthPolicy, RuntimeAuthConfig, RuntimeAuthService,
};

pub type ProductionRuntimeAuthService<D> = RuntimeAuthService<D, PostgresRuntimeOAuthPolicy>;

/// Composes public runtime magic-code/guest authentication with per-app
/// PostgreSQL catalogs and `$users` signup rules.
#[must_use]
pub fn production_runtime_auth_service<D>(
    runtime_client: Client,
    policy_client: Client,
    delivery: D,
    config: RuntimeAuthConfig,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
    magic_code_rate_limiter: Arc<dyn MagicCodeRateLimiter>,
    magic_code_expiry: Arc<dyn MagicCodeExpiryPolicy>,
) -> ProductionRuntimeAuthService<D>
where
    D: MagicCodeDelivery,
{
    RuntimeAuthService::new(
        runtime_client,
        delivery,
        PostgresRuntimeOAuthPolicy::new(policy_client)
            .with_rate_limit_consumer(rate_limit_consumer),
        config,
    )
    .with_magic_code_rate_limiter(magic_code_rate_limiter)
    .with_magic_code_expiry_policy(magic_code_expiry)
}

/// Pool-backed production composition used by the native server. Runtime
/// mutations and schema/rule policy reads use separate bounded pools so a
/// signup callback can acquire policy data while its mutation is open.
#[must_use]
pub fn production_runtime_auth_service_from_pool<D>(
    runtime_pool: PostgresPool,
    policy_pool: PostgresPool,
    delivery: D,
    config: RuntimeAuthConfig,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
    magic_code_rate_limiter: Arc<dyn MagicCodeRateLimiter>,
    magic_code_expiry: Arc<dyn MagicCodeExpiryPolicy>,
) -> ProductionRuntimeAuthService<D>
where
    D: MagicCodeDelivery,
{
    RuntimeAuthService::from_pool(
        runtime_pool,
        delivery,
        PostgresRuntimeOAuthPolicy::from_pool(policy_pool)
            .with_rate_limit_consumer(rate_limit_consumer),
        config,
    )
    .with_magic_code_rate_limiter(magic_code_rate_limiter)
    .with_magic_code_expiry_policy(magic_code_expiry)
}
