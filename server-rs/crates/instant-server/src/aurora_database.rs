use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Eq, PartialEq)]
pub struct AuroraDatabase {
    pub cluster_id: String,
    pub instance_id: String,
    pub cluster_status: String,
    pub database_url: String,
}

impl std::fmt::Debug for AuroraDatabase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuroraDatabase")
            .field("cluster_id", &self.cluster_id)
            .field("instance_id", &self.instance_id)
            .field("cluster_status", &self.cluster_status)
            .field("database_url", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum AuroraDatabaseError {
    #[error("Aurora discovery is unavailable in the self-hosted instant-server build")]
    AwsHostedFeatureDisabled,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AwsAuroraDatabaseProvider;

impl AwsAuroraDatabaseProvider {
    #[must_use]
    pub fn new(_config: &()) -> Self {
        Self
    }
    /// Rejects hosted discovery in a cloud-neutral build.
    ///
    /// # Errors
    ///
    /// Always returns [`AuroraDatabaseError::AwsHostedFeatureDisabled`].
    pub fn discover(
        &self,
        _cluster_id: &str,
        _application_name: &str,
    ) -> impl std::future::Future<Output = Result<AuroraDatabase, AuroraDatabaseError>> + Send {
        std::future::ready(Err(AuroraDatabaseError::AwsHostedFeatureDisabled))
    }
    pub async fn wait_for_writer_change(
        &self,
        _current_instance_id: &str,
        _cluster_id: &str,
        _application_name: &str,
        cancellation: &CancellationToken,
    ) -> Option<AuroraDatabase> {
        cancellation.cancelled().await;
        None
    }
}
