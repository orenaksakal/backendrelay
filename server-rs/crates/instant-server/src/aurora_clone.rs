use chrono::{DateTime, Utc};
use thiserror::Error;

#[derive(Clone, Eq, PartialEq)]
pub struct AuroraClone {
    pub cluster_id: String,
    pub instance_id: String,
    pub snapshot_time: DateTime<Utc>,
    pub database_url: String,
}

impl std::fmt::Debug for AuroraClone {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuroraClone")
            .field("cluster_id", &self.cluster_id)
            .field("instance_id", &self.instance_id)
            .field("snapshot_time", &self.snapshot_time)
            .field("database_url", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum AuroraCloneError {
    #[error("Aurora snapshots are unavailable in the self-hosted instant-server build")]
    AwsHostedFeatureDisabled,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AwsAuroraCloneProvider;

impl AwsAuroraCloneProvider {
    #[must_use]
    pub fn new(_config: &()) -> Self {
        Self
    }
}

#[derive(Clone)]
pub struct DirectPostgresSnapshotProvider {
    database_url: String,
}

impl DirectPostgresSnapshotProvider {
    #[must_use]
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
        }
    }
}

#[derive(Clone)]
pub enum ProductionSnapshotProvider {
    Aurora(AwsAuroraCloneProvider),
    DirectPostgres(DirectPostgresSnapshotProvider),
}

pub trait AuroraCloneLifecycle: Send + Sync + 'static {
    fn create(
        &self,
        source_cluster_id: String,
    ) -> impl std::future::Future<Output = Result<AuroraClone, AuroraCloneError>> + Send;
    fn delete(
        &self,
        clone: AuroraClone,
    ) -> impl std::future::Future<Output = Result<(), AuroraCloneError>> + Send;
    fn logical_start_lsn_query(&self) -> &'static str {
        "SELECT aurora_volume_logical_start_lsn()::text"
    }
    fn requires_exported_snapshot(&self) -> bool {
        false
    }
}

impl AuroraCloneLifecycle for AwsAuroraCloneProvider {
    fn create(
        &self,
        _source_cluster_id: String,
    ) -> impl std::future::Future<Output = Result<AuroraClone, AuroraCloneError>> + Send {
        std::future::ready(Err(AuroraCloneError::AwsHostedFeatureDisabled))
    }
    fn delete(
        &self,
        _clone: AuroraClone,
    ) -> impl std::future::Future<Output = Result<(), AuroraCloneError>> + Send {
        std::future::ready(Err(AuroraCloneError::AwsHostedFeatureDisabled))
    }
    fn logical_start_lsn_query(&self) -> &'static str {
        "SELECT aurora_volume_logical_start_lsn()::text"
    }
}

impl AuroraCloneLifecycle for DirectPostgresSnapshotProvider {
    fn create(
        &self,
        source_cluster_id: String,
    ) -> impl std::future::Future<Output = Result<AuroraClone, AuroraCloneError>> + Send {
        std::future::ready(Ok(AuroraClone {
            cluster_id: source_cluster_id,
            instance_id: "direct-postgres".to_owned(),
            snapshot_time: Utc::now(),
            database_url: self.database_url.clone(),
        }))
    }
    fn delete(
        &self,
        _clone: AuroraClone,
    ) -> impl std::future::Future<Output = Result<(), AuroraCloneError>> + Send {
        std::future::ready(Ok(()))
    }
    fn logical_start_lsn_query(&self) -> &'static str {
        "SELECT pg_current_wal_lsn()::text"
    }
    fn requires_exported_snapshot(&self) -> bool {
        true
    }
}

impl AuroraCloneLifecycle for ProductionSnapshotProvider {
    async fn create(&self, source_cluster_id: String) -> Result<AuroraClone, AuroraCloneError> {
        match self {
            Self::Aurora(provider) => provider.create(source_cluster_id).await,
            Self::DirectPostgres(provider) => provider.create(source_cluster_id).await,
        }
    }
    async fn delete(&self, clone: AuroraClone) -> Result<(), AuroraCloneError> {
        match self {
            Self::Aurora(provider) => provider.delete(clone).await,
            Self::DirectPostgres(provider) => provider.delete(clone).await,
        }
    }
    fn logical_start_lsn_query(&self) -> &'static str {
        match self {
            Self::Aurora(provider) => provider.logical_start_lsn_query(),
            Self::DirectPostgres(provider) => provider.logical_start_lsn_query(),
        }
    }
    fn requires_exported_snapshot(&self) -> bool {
        match self {
            Self::Aurora(provider) => provider.requires_exported_snapshot(),
            Self::DirectPostgres(provider) => provider.requires_exported_snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_postgres_provider_requires_a_shared_exported_snapshot() {
        let provider = DirectPostgresSnapshotProvider::new("postgresql://db/instant");
        let snapshot = provider.create("self-hosted".to_owned()).await.unwrap();

        assert_eq!(snapshot.database_url, "postgresql://db/instant");
        assert_eq!(
            provider.logical_start_lsn_query(),
            "SELECT pg_current_wal_lsn()::text"
        );
        assert!(provider.requires_exported_snapshot());
        provider.delete(snapshot).await.unwrap();
    }
}
