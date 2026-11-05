use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::JoinError;
use tokio_util::sync::CancellationToken;

use crate::{
    BackupWorkerError, NightlyBackupSupervisorError, PostgresConnectionDrivers,
    PostgresConnectionError, ProductionRouteAssembly, ServerProcessError, ServerShutdownReport,
    run_server_process,
};

#[derive(Debug, Error)]
pub enum ProductionServerProcessError {
    #[error(transparent)]
    Server(#[from] ServerProcessError),
    #[error(transparent)]
    Database(#[from] PostgresConnectionError),
    #[error("PostgreSQL connection supervisor task failed: {0}")]
    DatabaseTask(#[from] JoinError),
    #[error(transparent)]
    BackupWorker(#[from] BackupWorkerError),
    #[error("backup worker pool task failed: {0}")]
    BackupWorkerTask(JoinError),
    #[error(transparent)]
    NightlyBackup(#[from] NightlyBackupSupervisorError),
    #[error("nightly backup supervisor task failed: {0}")]
    NightlyBackupTask(JoinError),
}

async fn drain_realtime_connections(
    runtime: &crate::ProductionRuntimeRealtime,
    admin: &crate::production_routes::ProductionAdminRealtime,
    drain_budget: Duration,
) {
    tokio::join!(
        runtime.drain_all_connections(drain_budget, Duration::from_secs(1)),
        admin.drain_all_connections(drain_budget, Duration::from_secs(1)),
    );
}

/// Default time a terminating process gives live transports to reconnect
/// elsewhere before the HTTP server is aborted.
const DEFAULT_DRAIN_BUDGET: Duration = Duration::from_secs(30);

/// Reads the drain budget. Malformed or zero values fall back to the default so
/// a bad environment variable cannot skip the drain entirely.
fn drain_budget() -> Duration {
    std::env::var("INSTANT_DRAIN_BUDGET_SECONDS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map_or(DEFAULT_DRAIN_BUDGET, Duration::from_secs)
}

/// Runs HTTP, WAL invalidation, periodic workers, and every PostgreSQL
/// connection driver under one failure domain.
///
/// # Errors
///
/// Returns when HTTP, WAL invalidation, or a PostgreSQL driver fails, including
/// task panics and bounded-shutdown failures.
#[allow(clippy::too_many_lines)] // One owner keeps startup, failure, and shutdown ordering visible.
pub async fn run_production_server_process<Shutdown>(
    listener: TcpListener,
    assembly: ProductionRouteAssembly,
    drivers: PostgresConnectionDrivers,
    database_cancellation: CancellationToken,
    shutdown: Shutdown,
    shutdown_grace: Duration,
) -> Result<ServerShutdownReport, ProductionServerProcessError>
where
    Shutdown: Future<Output = ()> + Send,
{
    let ProductionRouteAssembly {
        router,
        health,
        tasks,
        runtime_realtime,
        admin_realtime,
        invalidator,
        backup_pool,
        nightly_backup,
    } = assembly;
    let database_token = database_cancellation.child_token();
    let mut database_handle = tokio::spawn(drivers.supervise(database_token));
    let early_database_result = Arc::new(Mutex::new(None));
    let captured_result = Arc::clone(&early_database_result);
    let backup_shutdown = CancellationToken::new();
    let backup_shutdown_signal = backup_shutdown.clone();
    let backup_handle = tokio::spawn(async move {
        backup_shutdown_signal.cancelled().await;
        backup_pool.shutdown(shutdown_grace).await
    });
    let nightly_handle = nightly_backup.map(|nightly| {
        let shutdown = backup_shutdown.clone();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            nightly.shutdown(shutdown_grace).await
        })
    });
    let request_backup_shutdown = backup_shutdown.clone();
    let drain_health = health.clone();
    let drain_realtime = Arc::clone(&runtime_realtime);
    let drain_admin_realtime = Arc::clone(&admin_realtime);
    let coordinated_shutdown = async {
        tokio::select! {
            () = shutdown => {}
            result = &mut database_handle => {
                *captured_result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            }
        }
        request_backup_shutdown.cancel();
        // `/health/system` is the portable readiness contract. Withdraw the
        // process first, then pace existing client and upstream proxy closures
        // before the HTTP listener stops. Orchestrators only need to deliver
        // their normal termination signal; no cloud event bus is required.
        drain_health.set_wal_healthy(false);
        drain_realtime_connections(&drain_realtime, &drain_admin_realtime, drain_budget()).await;
    };

    let server_result = run_server_process(
        listener,
        router,
        health,
        tasks,
        move |cancellation| invalidator.run(cancellation),
        coordinated_shutdown,
        shutdown_grace,
    )
    .await;
    backup_shutdown.cancel();
    let backup_result = backup_handle
        .await
        .map_err(ProductionServerProcessError::BackupWorkerTask);
    let nightly_result = match nightly_handle {
        Some(handle) => Some(
            handle
                .await
                .map_err(ProductionServerProcessError::NightlyBackupTask),
        ),
        None => None,
    };
    database_cancellation.cancel();
    let early = early_database_result
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let database_result = match early {
        Some(result) => result,
        None => database_handle.await,
    }?;
    backup_result??;
    if let Some(result) = nightly_result {
        result??;
    }
    database_result?;
    Ok(server_result?)
}
