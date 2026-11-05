use instant_http::{WalHealthFuture, WalHealthSource};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_postgres::Client;

use crate::postgres_pool::PostgresClientSource;
use crate::postgres_realtime_cluster::MACHINE_HEALTH_GRACE;
use crate::{PostgresPool, replication::InvalidationHealthObserver};

type WalPersistenceFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>;

trait WalHealthPersistence: Send + Sync + 'static {
    fn persist_healthy(&self, healthy: bool) -> WalPersistenceFuture<'_>;
}

/// Fleet-wide WAL readiness stored in the shared `config` table.
#[derive(Clone, Debug)]
pub struct PostgresWalHealth {
    persistence: Arc<PostgresWalHealthPersistence>,
    local_healthy: Arc<AtomicBool>,
}

#[derive(Debug)]
struct PostgresWalHealthPersistence {
    client: PostgresClientSource,
    process_id: String,
}

impl PostgresWalHealth {
    #[must_use]
    pub fn new(client: Client, process_id: impl Into<String>) -> Self {
        Self {
            persistence: Arc::new(PostgresWalHealthPersistence {
                client: PostgresClientSource::fixed(client),
                process_id: process_id.into(),
            }),
            local_healthy: Arc::new(AtomicBool::new(true)),
        }
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool, process_id: impl Into<String>) -> Self {
        Self {
            persistence: Arc::new(PostgresWalHealthPersistence {
                client: PostgresClientSource::pooled(pool),
                process_id: process_id.into(),
            }),
            local_healthy: Arc::new(AtomicBool::new(true)),
        }
    }

    fn set_local_healthy(&self, healthy: bool) {
        self.local_healthy.store(healthy, Ordering::Release);
    }

    #[must_use]
    pub fn local_healthy(&self) -> bool {
        self.local_healthy.load(Ordering::Acquire)
    }

    /// Publishes this process's WAL failure marker.
    ///
    /// # Errors
    ///
    /// Returns an error when PostgreSQL checkout or marker persistence fails.
    pub async fn mark_unhealthy(&self) -> Result<(), String> {
        self.set_local_healthy(false);
        self.persistence.persist_unhealthy().await
    }

    /// Removes only this process's WAL failure marker.
    ///
    /// # Errors
    ///
    /// Returns an error when PostgreSQL checkout or marker persistence fails.
    pub async fn mark_healthy(&self) -> Result<(), String> {
        self.persistence.persist_healthy_marker().await?;
        self.set_local_healthy(true);
        Ok(())
    }

    async fn shared_healthy(&self) -> Result<bool, String> {
        self.persistence.shared_healthy().await
    }
}

impl PostgresWalHealthPersistence {
    async fn persist_unhealthy(&self) -> Result<(), String> {
        let client = self
            .client
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        client
            .execute(
                "INSERT INTO config (k, v)
                 VALUES ('wal-errors', jsonb_build_object($1::text, now()::text)::json)
                 ON CONFLICT (k) DO UPDATE
                 SET v = (config.v::jsonb || excluded.v::jsonb)::json",
                &[&self.process_id],
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn persist_healthy_marker(&self) -> Result<(), String> {
        let client = self
            .client
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        client
            .execute(
                "UPDATE config
                 SET v = (v::jsonb - $1::text)::json
                 WHERE k = 'wal-errors'",
                &[&self.process_id],
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn shared_healthy(&self) -> Result<bool, String> {
        let client = self
            .client
            .acquire()
            .await
            .map_err(|error| error.to_string())?;
        let grace = i64::try_from(MACHINE_HEALTH_GRACE.as_secs()).unwrap_or(i64::MAX);
        let unhealthy = client
            .query_one(
                "SELECT coalesce(
                   bool_or(
                     CASE
                       WHEN machine.machine_id IS NOT NULL
                         THEN machine.updated_at
                           > now() - $1::bigint * interval '1 second'
                       WHEN pg_input_is_valid(entry.value #>> '{}', 'timestamptz')
                         THEN (entry.value #>> '{}')::timestamptz
                           > now() - $1::bigint * interval '1 second'
                       ELSE true
                     END
                   ),
                   false
                 )
                 FROM config
                 CROSS JOIN LATERAL jsonb_each(config.v::jsonb) AS entry
                 LEFT JOIN rust_realtime_machines AS machine
                   ON machine.machine_id = CASE
                        WHEN pg_input_is_valid(entry.key, 'uuid') THEN entry.key::uuid
                      END
                 WHERE config.k = 'wal-errors'",
                &[&grace],
            )
            .await
            .map_err(|error| error.to_string())?
            .get::<_, bool>(0);
        Ok(!unhealthy)
    }
}

impl WalHealthPersistence for PostgresWalHealthPersistence {
    fn persist_healthy(&self, healthy: bool) -> WalPersistenceFuture<'_> {
        Box::pin(async move {
            if healthy {
                self.persist_healthy_marker().await
            } else {
                self.persist_unhealthy().await
            }
        })
    }
}

async fn effective_shared_health<F>(local_healthy: &AtomicBool, shared: F) -> Result<bool, String>
where
    F: std::future::Future<Output = Result<bool, String>>,
{
    if !local_healthy.load(Ordering::Acquire) {
        return Ok(false);
    }
    let shared_healthy = shared.await?;
    // A disconnect may be observed while the shared query is awaiting a pool
    // checkout or PostgreSQL. Re-read after the await so that stale shared
    // state cannot override the synchronously published local failure.
    Ok(shared_healthy && local_healthy.load(Ordering::Acquire))
}

impl WalHealthSource for PostgresWalHealth {
    fn wal_healthy(&self) -> WalHealthFuture<'_> {
        Box::pin(effective_shared_health(
            self.local_healthy.as_ref(),
            self.shared_healthy(),
        ))
    }
}

/// Non-blocking, ordered observer. PostgreSQL failures are intentionally
/// best-effort and never terminate or delay replication recovery.
#[derive(Clone)]
pub struct PostgresInvalidationHealthObserver {
    sender: tokio::sync::mpsc::UnboundedSender<WalHealthCommand>,
    health: PostgresWalHealth,
}

enum WalHealthCommand {
    Set(bool),
    Graceful(tokio::sync::oneshot::Sender<()>),
}

fn spawn_persistence_worker(
    persistence: Arc<dyn WalHealthPersistence>,
) -> tokio::sync::mpsc::UnboundedSender<WalHealthCommand> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(command) = receiver.recv().await {
            match command {
                WalHealthCommand::Set(healthy) => {
                    let _best_effort = persistence.persist_healthy(healthy).await;
                }
                WalHealthCommand::Graceful(done) => {
                    let _best_effort = persistence.persist_healthy(true).await;
                    let _waiting_supervisor_may_be_gone = done.send(());
                }
            }
        }
    });
    sender
}

impl PostgresInvalidationHealthObserver {
    #[must_use]
    pub fn spawn(health: PostgresWalHealth) -> Self {
        let persistence: Arc<dyn WalHealthPersistence> = health.persistence.clone();
        let sender = spawn_persistence_worker(persistence);
        Self { sender, health }
    }
}

impl InvalidationHealthObserver for PostgresInvalidationHealthObserver {
    fn set_healthy(&self, healthy: bool) {
        self.health.set_local_healthy(healthy);
        let _worker_may_be_shutting_down = self.sender.send(WalHealthCommand::Set(healthy));
    }

    fn graceful_shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.health.set_local_healthy(true);
            let (done, waiting) = tokio::sync::oneshot::channel();
            if self.sender.send(WalHealthCommand::Graceful(done)).is_ok() {
                let _worker_may_stop = waiting.await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::{Notify, oneshot};

    use super::*;

    struct BlockingPersistence {
        healthy_started: Notify,
        healthy_release: Mutex<Option<oneshot::Receiver<()>>>,
        unhealthy_persisted: Notify,
    }

    impl WalHealthPersistence for BlockingPersistence {
        fn persist_healthy(&self, healthy: bool) -> WalPersistenceFuture<'_> {
            Box::pin(async move {
                if healthy {
                    self.healthy_started.notify_one();
                    let waiting = self
                        .healthy_release
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    if let Some(waiting) = waiting {
                        let _released = waiting.await;
                    }
                } else {
                    self.unhealthy_persisted.notify_one();
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn stale_queued_healthy_persistence_cannot_overwrite_newer_disconnect() {
        let (release, waiting) = oneshot::channel();
        let persistence = Arc::new(BlockingPersistence {
            healthy_started: Notify::new(),
            healthy_release: Mutex::new(Some(waiting)),
            unhealthy_persisted: Notify::new(),
        });
        let sender = spawn_persistence_worker(persistence.clone());
        let local_healthy = AtomicBool::new(false);

        local_healthy.store(true, Ordering::Release);
        sender.send(WalHealthCommand::Set(true)).unwrap();
        persistence.healthy_started.notified().await;
        local_healthy.store(false, Ordering::Release);
        sender.send(WalHealthCommand::Set(false)).unwrap();
        assert!(!local_healthy.load(Ordering::Acquire));

        release.send(()).unwrap();
        persistence.unhealthy_persisted.notified().await;
        assert!(!local_healthy.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn disconnect_during_shared_read_is_fail_closed() {
        let local_healthy = Arc::new(AtomicBool::new(true));
        let (query_started, query_waiting) = oneshot::channel();
        let (query_release, released) = oneshot::channel();
        let read_local = Arc::clone(&local_healthy);
        let read = tokio::spawn(async move {
            effective_shared_health(read_local.as_ref(), async move {
                query_started.send(()).unwrap();
                let _released = released.await;
                Ok(true)
            })
            .await
        });

        query_waiting.await.unwrap();
        local_healthy.store(false, Ordering::Release);
        query_release.send(()).unwrap();
        assert_eq!(read.await.unwrap(), Ok(false));
    }
}
