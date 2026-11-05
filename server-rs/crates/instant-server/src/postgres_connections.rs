use std::future::Future;

use thiserror::Error;
use tokio::task::{JoinError, JoinSet};
use tokio_postgres::Client;
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum PostgresConnectionError {
    #[error("failed to open PostgreSQL connection {index}: {source}")]
    Connect {
        index: usize,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("PostgreSQL connection driver {index} failed: {message}")]
    Driver { index: usize, message: String },
    #[error("PostgreSQL connection driver {index} stopped unexpectedly")]
    DriverStopped { index: usize },
    #[error("PostgreSQL connection driver task failed: {0}")]
    DriverTask(#[source] JoinError),
}

/// Owns the asynchronous drivers behind a fixed set of PostgreSQL clients.
///
/// `tokio_postgres::Client` values do not make progress by themselves: each one
/// has a corresponding connection future that must remain polled. This owner
/// turns an early driver exit into a process-fatal error and aborts every driver
/// when process cancellation begins.
pub struct PostgresConnectionDrivers {
    drivers: JoinSet<(usize, Result<(), String>)>,
}

impl PostgresConnectionDrivers {
    /// Opens `count` independent clients and starts each connection driver.
    ///
    /// # Errors
    ///
    /// Returns the failing one-based connection index and closes any clients
    /// already opened when PostgreSQL cannot establish the complete set.
    pub async fn connect_many(
        database_url: &str,
        count: usize,
    ) -> Result<(Vec<Client>, Self), PostgresConnectionError> {
        Self::connect_urls(std::iter::repeat_n(database_url, count)).await
    }

    /// Opens independently driven clients against an ordered set of URLs.
    /// This keeps role ownership explicit while allowing selected read roles
    /// to use a replica without creating an unsupervised second driver set.
    ///
    /// # Errors
    ///
    /// Returns the failing one-based connection index and closes every client
    /// opened earlier in the same startup attempt.
    pub async fn connect_urls<'a>(
        database_urls: impl IntoIterator<Item = &'a str>,
    ) -> Result<(Vec<Client>, Self), PostgresConnectionError> {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let tls = MakeRustlsConnect::with_webpki_roots();
        let database_urls = database_urls.into_iter();
        let mut clients = Vec::with_capacity(database_urls.size_hint().0);
        let mut owner = Self {
            drivers: JoinSet::new(),
        };
        for (offset, database_url) in database_urls.enumerate() {
            let index = offset + 1;
            let (client, connection) =
                match tokio_postgres::connect(database_url, tls.clone()).await {
                    Ok(pair) => pair,
                    Err(source) => {
                        owner.drivers.abort_all();
                        while owner.drivers.join_next().await.is_some() {}
                        return Err(PostgresConnectionError::Connect { index, source });
                    }
                };
            owner.spawn_driver(index, async move {
                connection.await.map_err(|error| error.to_string())
            });
            clients.push(client);
        }
        Ok((clients, owner))
    }

    /// Polls every connection driver until cancellation or the first failure.
    ///
    /// # Errors
    ///
    /// Returns if a driver fails, panics, or stops while the process is active.
    pub async fn supervise(
        mut self,
        cancellation: CancellationToken,
    ) -> Result<(), PostgresConnectionError> {
        if self.drivers.is_empty() {
            cancellation.cancelled().await;
            return Ok(());
        }

        let outcome = tokio::select! {
            () = cancellation.cancelled() => None,
            result = self.drivers.join_next() => result,
        };
        self.drivers.abort_all();
        while self.drivers.join_next().await.is_some() {}

        match outcome {
            None => Ok(()),
            Some(Ok((index, Ok(())))) => Err(PostgresConnectionError::DriverStopped { index }),
            Some(Ok((index, Err(message)))) => {
                Err(PostgresConnectionError::Driver { index, message })
            }
            Some(Err(error)) => Err(PostgresConnectionError::DriverTask(error)),
        }
    }

    fn spawn_driver<F>(&mut self, index: usize, driver: F)
    where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        self.drivers.spawn(async move { (index, driver.await) });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    #[tokio::test]
    async fn cancellation_stops_all_connection_drivers() {
        let mut drivers = PostgresConnectionDrivers {
            drivers: JoinSet::new(),
        };
        drivers.spawn_driver(1, std::future::pending());
        drivers.spawn_driver(2, std::future::pending());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        tokio::time::timeout(Duration::from_secs(1), drivers.supervise(cancellation))
            .await
            .expect("supervisor should stop promptly")
            .expect("process cancellation is successful");
    }

    #[tokio::test]
    async fn an_early_driver_failure_is_process_fatal_and_identifies_the_client() {
        let mut drivers = PostgresConnectionDrivers {
            drivers: JoinSet::new(),
        };
        drivers.spawn_driver(7, async { Err("database connection closed".to_owned()) });
        drivers.spawn_driver(8, std::future::pending());

        let error = drivers
            .supervise(CancellationToken::new())
            .await
            .expect_err("driver failure must not be hidden");
        assert!(matches!(
            error,
            PostgresConnectionError::Driver { index: 7, message }
                if message == "database connection closed"
        ));
    }

    #[tokio::test]
    async fn a_clean_early_driver_exit_is_still_process_fatal() {
        let mut drivers = PostgresConnectionDrivers {
            drivers: JoinSet::new(),
        };
        drivers.spawn_driver(3, std::future::ready(Ok(())));

        let error = drivers
            .supervise(CancellationToken::new())
            .await
            .expect_err("an active database driver may not disappear");
        assert!(matches!(
            error,
            PostgresConnectionError::DriverStopped { index: 3 }
        ));
    }

    #[tokio::test]
    async fn required_tls_connections_send_the_postgres_ssl_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mock_server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8];
            socket.read_exact(&mut request).await.unwrap();
            assert_eq!(request, [0, 0, 0, 8, 4, 210, 22, 47]);
            socket.write_all(b"N").await.unwrap();
        });
        let database_url =
            format!("postgresql://instant@{address}/instant?sslmode=require&connect_timeout=1");

        let error = PostgresConnectionDrivers::connect_many(&database_url, 1)
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            PostgresConnectionError::Connect { index: 1, .. }
        ));
        mock_server.await.unwrap();
    }

    #[tokio::test]
    async fn ordered_urls_keep_clients_and_drivers_in_the_same_role_order() {
        let Some(database_url) = instant_test_support::test_database_url(
            "ordered_urls_keep_clients_and_drivers_in_the_same_role_order",
        ) else {
            return;
        };
        let (clients, drivers) =
            PostgresConnectionDrivers::connect_urls([database_url.as_str(), database_url.as_str()])
                .await
                .unwrap();

        assert_eq!(clients.len(), 2);
        assert_eq!(
            clients[0]
                .query_one("SELECT 1::int", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            1
        );
        assert_eq!(
            clients[1]
                .query_one("SELECT 2::int", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            2
        );
        drop(clients);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        drivers.supervise(cancellation).await.unwrap();
    }
}
