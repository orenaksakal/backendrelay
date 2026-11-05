use thiserror::Error;

use crate::{PostgresConnectionDrivers, PostgresConnectionError, ServerConfig, ServerEnvironment};

#[derive(Debug, Error)]
pub enum ProductionDatabaseBootstrapError {
    #[error(transparent)]
    Connection(#[from] PostgresConnectionError),
}

/// Empty compatibility marker for the former fixed production client
/// inventory. Every process-owned database path now uses a bounded reconnecting
/// pool; clone-local exported-snapshot sessions are created only for one run.
pub struct ProductionDatabaseClients;

impl ProductionDatabaseClients {
    /// Returns an empty fixed-driver owner. The process API retains this marker
    /// while failover shutdown is migrated independently from the old fixed
    /// connection inventory.
    ///
    /// # Errors
    ///
    /// Retains the historical result type for process-composition callers.
    pub async fn connect(
        config: &ServerConfig,
    ) -> Result<(Self, PostgresConnectionDrivers), ProductionDatabaseBootstrapError> {
        Self::connect_url(config.database_url(), config.environment()).await
    }

    /// Builds the empty production inventory against an already resolved
    /// writer URL. Aurora deployments retain the same process-composition API.
    ///
    /// # Errors
    ///
    /// Retains the historical result type for process-composition callers.
    pub async fn connect_url(
        database_url: &str,
        _environment: ServerEnvironment,
    ) -> Result<(Self, PostgresConnectionDrivers), ProductionDatabaseBootstrapError> {
        let (clients, drivers) = PostgresConnectionDrivers::connect_many(database_url, 0).await?;
        debug_assert!(clients.is_empty());
        Ok((Self, drivers))
    }

    /// Verifies the zero-client compatibility marker.
    ///
    /// # Errors
    ///
    /// Retains the historical result type for process-composition callers.
    pub fn finish(self) -> Result<(), ProductionDatabaseBootstrapError> {
        Ok(())
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_environment_has_zero_fixed_production_database_clients() {
        for environment in [
            ServerEnvironment::Production,
            ServerEnvironment::Development,
            ServerEnvironment::Test,
            ServerEnvironment::Staging,
        ] {
            let (clients, drivers) = ProductionDatabaseClients::connect_url(
                "an invalid URL is never opened for the zero-client inventory",
                environment,
            )
            .await
            .unwrap();
            assert_eq!(clients.remaining(), 0);
            let cancellation = tokio_util::sync::CancellationToken::new();
            cancellation.cancel();
            drivers.supervise(cancellation).await.unwrap();
        }
    }
}
