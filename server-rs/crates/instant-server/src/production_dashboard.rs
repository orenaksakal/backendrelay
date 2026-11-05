use std::sync::Arc;

use tokio_postgres::Client;

use crate::postgres_pool::PostgresClientSource;
use crate::{
    DatabaseQueryThroughput, GoogleDashboardControlHooks, OAuthSecretCipher, OAuthSettingsHooks,
    PostgresDashboardCoreHooks, PostgresDashboardMetricsHooks, PostgresDashboardPolicyHooks,
    PostgresPool, PostmarkAdapters, RuntimeSessionReportCollector, RuntimeSessionReporter,
    ServerConfig, TinkOAuthSettingsHooks,
};

pub type ProductionDashboardHooks<D> = GoogleDashboardControlHooks<
    PostgresDashboardPolicyHooks<
        OAuthSettingsHooks<
            TinkOAuthSettingsHooks<PostgresDashboardCoreHooks<PostgresDashboardMetricsHooks<D>>>,
        >,
    >,
>;

pub type ProductionDashboardAdapters<D> = PostmarkAdapters<ProductionDashboardHooks<D>>;

/// Composes the concrete dashboard provider stack used by a production server.
///
/// Separate PostgreSQL clients avoid serializing core mutations, operator
/// metrics, and template reads behind one mutex. The returned email adapters
/// are shared by runtime/admin authentication and the dashboard subsystem.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn production_dashboard_adapters<D>(
    config: &ServerConfig,
    template_client: Client,
    core_client: Client,
    metrics_client: Client,
    policy_client: Client,
    oauth_secret_cipher: OAuthSecretCipher,
    excluded_metric_emails: Vec<String>,
    inner: D,
) -> ProductionDashboardAdapters<D> {
    production_dashboard_adapters_with_runtime(
        config,
        template_client,
        core_client,
        metrics_client,
        policy_client,
        oauth_secret_cipher,
        excluded_metric_emails,
        None,
        None,
        inner,
    )
}

/// Composes the dashboard with local realtime reporting plus an injectable
/// cross-machine collector.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn production_dashboard_adapters_with_runtime<D>(
    config: &ServerConfig,
    template_client: Client,
    core_client: Client,
    metrics_client: Client,
    policy_client: Client,
    oauth_secret_cipher: OAuthSecretCipher,
    excluded_metric_emails: Vec<String>,
    session_metrics: Option<(
        Arc<dyn RuntimeSessionReporter>,
        Arc<dyn RuntimeSessionReportCollector>,
    )>,
    database_query_throughput: Option<Arc<dyn DatabaseQueryThroughput>>,
    inner: D,
) -> ProductionDashboardAdapters<D> {
    production_dashboard_adapters_with_runtime_source(
        config,
        PostgresClientSource::fixed(template_client),
        PostgresClientSource::fixed(core_client),
        PostgresClientSource::fixed(metrics_client),
        PostgresClientSource::fixed(policy_client),
        oauth_secret_cipher,
        excluded_metric_emails,
        session_metrics,
        database_query_throughput,
        inner,
    )
}

/// Composes the production dashboard while acquiring policy operations from a
/// reconnecting pool. The other database owners retain their existing
/// independent-client contracts.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn production_dashboard_adapters_with_runtime_policy_pool<D>(
    config: &ServerConfig,
    template_client: Client,
    core_client: Client,
    metrics_client: Client,
    policy_pool: PostgresPool,
    oauth_secret_cipher: OAuthSecretCipher,
    excluded_metric_emails: Vec<String>,
    session_metrics: Option<(
        Arc<dyn RuntimeSessionReporter>,
        Arc<dyn RuntimeSessionReportCollector>,
    )>,
    database_query_throughput: Option<Arc<dyn DatabaseQueryThroughput>>,
    inner: D,
) -> ProductionDashboardAdapters<D> {
    production_dashboard_adapters_with_runtime_source(
        config,
        PostgresClientSource::fixed(template_client),
        PostgresClientSource::fixed(core_client),
        PostgresClientSource::fixed(metrics_client),
        PostgresClientSource::pooled(policy_pool),
        oauth_secret_cipher,
        excluded_metric_emails,
        session_metrics,
        database_query_throughput,
        inner,
    )
}

/// Composes the production dashboard with reconnecting pools for every
/// PostgreSQL-backed dashboard adapter.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn production_dashboard_adapters_with_runtime_pools<D>(
    config: &ServerConfig,
    template_pool: PostgresPool,
    core_pool: PostgresPool,
    metrics_pool: PostgresPool,
    policy_pool: PostgresPool,
    oauth_secret_cipher: OAuthSecretCipher,
    excluded_metric_emails: Vec<String>,
    session_metrics: Option<(
        Arc<dyn RuntimeSessionReporter>,
        Arc<dyn RuntimeSessionReportCollector>,
    )>,
    database_query_throughput: Option<Arc<dyn DatabaseQueryThroughput>>,
    inner: D,
) -> ProductionDashboardAdapters<D> {
    production_dashboard_adapters_with_runtime_source(
        config,
        PostgresClientSource::pooled(template_pool),
        PostgresClientSource::pooled(core_pool),
        PostgresClientSource::pooled(metrics_pool),
        PostgresClientSource::pooled(policy_pool),
        oauth_secret_cipher,
        excluded_metric_emails,
        session_metrics,
        database_query_throughput,
        inner,
    )
}

#[allow(clippy::too_many_arguments)]
fn production_dashboard_adapters_with_runtime_source<D>(
    config: &ServerConfig,
    template_client: PostgresClientSource,
    core_client: PostgresClientSource,
    metrics_client: PostgresClientSource,
    policy_client: PostgresClientSource,
    oauth_secret_cipher: OAuthSecretCipher,
    excluded_metric_emails: Vec<String>,
    session_metrics: Option<(
        Arc<dyn RuntimeSessionReporter>,
        Arc<dyn RuntimeSessionReportCollector>,
    )>,
    database_query_throughput: Option<Arc<dyn DatabaseQueryThroughput>>,
    inner: D,
) -> ProductionDashboardAdapters<D> {
    let mut metrics =
        PostgresDashboardMetricsHooks::from_source(metrics_client, inner, excluded_metric_emails);
    if let Some((reporter, collector)) = session_metrics {
        metrics = metrics.with_session_metrics(reporter, collector);
    }
    if let Some(throughput) = database_query_throughput {
        metrics = metrics.with_database_query_throughput(throughput);
    }
    let core = PostgresDashboardCoreHooks::from_source(core_client, metrics);
    let oauth_encryption = TinkOAuthSettingsHooks::new(oauth_secret_cipher, core);
    let oauth_settings = OAuthSettingsHooks::new(oauth_encryption);
    let policy = PostgresDashboardPolicyHooks::from_source(
        policy_client,
        config.environment(),
        oauth_settings,
    )
    .with_self_hosted_plan_enabled(true)
    .with_superuser_email(config.superuser_email())
    .with_dashboard_allowed_emails(config.dashboard_allowed_emails().cloned())
    .with_dashboard_signup_open(config.dashboard_signup_open())
    .with_ephemeral_apps_enabled(config.ephemeral_apps_enabled())
    .with_shared_oauth_providers(
        config
            .shared_oauth_clients()
            .iter()
            .map(crate::SharedOAuthClientConfig::provider_name),
    );
    let google = GoogleDashboardControlHooks::from_config(config, policy);
    PostmarkAdapters::from_config_source(
        config.email(),
        config.dashboard_origin(),
        template_client,
        google,
    )
}
