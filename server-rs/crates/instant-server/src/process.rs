use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use instant_http::HealthState;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::{BackgroundSupervisor, ScheduledTask, ShutdownReport};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServerShutdownReport {
    pub workers: ShutdownReport,
    pub http_aborted: bool,
    pub invalidator_aborted: bool,
}

#[derive(Debug, Error)]
pub enum ServerProcessError {
    #[error("HTTP server failed: {0}")]
    Http(#[source] std::io::Error),
    #[error("HTTP server task failed: {0}")]
    HttpTask(#[source] JoinError),
    #[error("WAL invalidator failed: {0}")]
    Invalidator(String),
    #[error("WAL invalidator stopped before process shutdown")]
    InvalidatorStopped,
    #[error("WAL invalidator task failed: {0}")]
    InvalidatorTask(#[source] JoinError),
}

/// Process termination signals registered before the HTTP server starts.
///
/// Registration is explicit so a launcher can fail before binding its listener
/// when the operating system cannot install signal handlers.
pub struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    terminate: tokio::signal::windows::CtrlBreak,
}

impl ShutdownSignals {
    /// Registers the platform's normal interactive and service-stop signals.
    ///
    /// # Errors
    ///
    /// Returns an operating-system error if signal registration fails.
    #[cfg(any(unix, windows))]
    pub fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                interrupt: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::interrupt(),
                )?,
                terminate: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                )?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
                terminate: tokio::signal::windows::ctrl_break()?,
            })
        }
    }

    /// Resolves once the process receives SIGINT/SIGTERM on Unix or
    /// Ctrl-C/Ctrl-Break on Windows.
    #[cfg(any(unix, windows))]
    pub async fn wait(mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

/// Runs the public HTTP server, WAL invalidator, and periodic workers as one process.
///
/// The invalidator receives a child cancellation token and is expected to remain alive
/// until that token is cancelled. An early invalidator exit marks WAL health unhealthy
/// and shuts down the rest of the process. External shutdown cancels every subsystem and
/// waits up to `shutdown_grace` before aborting tasks that have not stopped.
///
/// # Errors
///
/// Returns when the HTTP server or invalidator exits unexpectedly or either task panics.
#[allow(clippy::too_many_arguments)]
pub async fn run_server_process<I, IFut, IE, Shutdown>(
    listener: TcpListener,
    router: Router,
    health: HealthState,
    tasks: Vec<ScheduledTask>,
    invalidator: I,
    shutdown: Shutdown,
    shutdown_grace: Duration,
) -> Result<ServerShutdownReport, ServerProcessError>
where
    I: FnOnce(CancellationToken) -> IFut + Send + 'static,
    IFut: Future<Output = Result<(), IE>> + Send + 'static,
    IE: std::error::Error + Send + Sync + 'static,
    Shutdown: Future<Output = ()> + Send,
{
    coordinate_server_process(
        health,
        tasks,
        invalidator,
        move |cancellation| async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(cancellation.cancelled_owned())
            .await
        },
        shutdown,
        shutdown_grace,
    )
    .await
}

async fn coordinate_server_process<I, IFut, IE, H, HFut, Shutdown>(
    health: HealthState,
    tasks: Vec<ScheduledTask>,
    invalidator: I,
    http_server: H,
    shutdown: Shutdown,
    shutdown_grace: Duration,
) -> Result<ServerShutdownReport, ServerProcessError>
where
    I: FnOnce(CancellationToken) -> IFut + Send + 'static,
    IFut: Future<Output = Result<(), IE>> + Send + 'static,
    IE: std::error::Error + Send + Sync + 'static,
    H: FnOnce(CancellationToken) -> HFut + Send + 'static,
    HFut: Future<Output = Result<(), std::io::Error>> + Send + 'static,
    Shutdown: Future<Output = ()> + Send,
{
    let cancellation = CancellationToken::new();
    let invalidator_cancellation = cancellation.child_token();
    let mut invalidator_handle = tokio::spawn(async move {
        invalidator(invalidator_cancellation)
            .await
            .map_err(|error| error.to_string())
    });
    let http_cancellation = cancellation.child_token();
    let mut http_handle = tokio::spawn(http_server(http_cancellation));
    let mut workers = BackgroundSupervisor::start(tasks);
    tokio::pin!(shutdown);

    let mut http_finished = false;
    let mut invalidator_finished = false;
    let primary_error = tokio::select! {
        () = &mut shutdown => None,
        result = &mut http_handle => {
            http_finished = true;
            Some(http_result(result))
        }
        result = &mut invalidator_handle => {
            invalidator_finished = true;
            health.set_wal_healthy(false);
            Some(invalidator_result(result))
        }
    };

    tracing::info!(name = "shut-down.start", "process shutdown started");
    let (
        workers_report,
        (http_aborted, http_shutdown_error),
        (invalidator_aborted, invalidator_shutdown_error),
    ) = async {
        cancellation.cancel();
        let deadline = Instant::now() + shutdown_grace;
        let workers_report = workers.shutdown(shutdown_grace).await;
        let http_result = if http_finished {
            (false, None)
        } else {
            finish_http(&mut http_handle, deadline)
                .instrument(tracing::info_span!("stop-server"))
                .await
        };
        let invalidator_result = if invalidator_finished {
            (false, None)
        } else {
            finish_invalidator(&mut invalidator_handle, deadline)
                .instrument(tracing::info_span!("stop-invalidator"))
                .await
        };
        (workers_report, http_result, invalidator_result)
    }
    .instrument(tracing::info_span!("shut-down"))
    .await;

    if let Some(error) = primary_error
        .or(http_shutdown_error)
        .or(invalidator_shutdown_error)
    {
        return Err(error);
    }
    Ok(ServerShutdownReport {
        workers: workers_report,
        http_aborted,
        invalidator_aborted,
    })
}

fn http_result(result: Result<Result<(), std::io::Error>, JoinError>) -> ServerProcessError {
    match result {
        Ok(Ok(())) => ServerProcessError::Http(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "HTTP server stopped before process shutdown",
        )),
        Ok(Err(error)) => ServerProcessError::Http(error),
        Err(error) => ServerProcessError::HttpTask(error),
    }
}

fn invalidator_result(result: Result<Result<(), String>, JoinError>) -> ServerProcessError {
    match result {
        Ok(Ok(())) => ServerProcessError::InvalidatorStopped,
        Ok(Err(error)) => ServerProcessError::Invalidator(error),
        Err(error) => ServerProcessError::InvalidatorTask(error),
    }
}

async fn finish_http(
    handle: &mut JoinHandle<Result<(), std::io::Error>>,
    deadline: Instant,
) -> (bool, Option<ServerProcessError>) {
    match tokio::time::timeout_at(deadline, &mut *handle).await {
        Ok(Ok(Ok(()))) => (false, None),
        Ok(Ok(Err(error))) => (false, Some(ServerProcessError::Http(error))),
        Ok(Err(error)) => (false, Some(ServerProcessError::HttpTask(error))),
        Err(_) => {
            handle.abort();
            (true, None)
        }
    }
}

async fn finish_invalidator(
    handle: &mut JoinHandle<Result<(), String>>,
    deadline: Instant,
) -> (bool, Option<ServerProcessError>) {
    match tokio::time::timeout_at(deadline, &mut *handle).await {
        Ok(Ok(Ok(()))) => (false, None),
        Ok(Ok(Err(error))) => (false, Some(ServerProcessError::Invalidator(error))),
        Ok(Err(error)) => (false, Some(ServerProcessError::InvalidatorTask(error))),
        Err(_) => {
            handle.abort();
            (true, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use axum::extract::ConnectInfo;
    use axum::routing::get;
    use tokio::sync::oneshot;

    use super::*;
    use crate::TaskSchedule;

    #[tokio::test]
    async fn process_stops_http_invalidator_and_workers_together() {
        let health = HealthState::new(true);
        let invalidator_stopped = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&invalidator_stopped);
        let http_stopped = Arc::new(AtomicBool::new(false));
        let http_stop = Arc::clone(&http_stopped);
        let worker_runs = Arc::new(AtomicUsize::new(0));
        let runs = Arc::clone(&worker_runs);
        let task = ScheduledTask::new(
            "process-test-worker",
            TaskSchedule::Fixed {
                every: Duration::from_secs(60),
                start_immediately: true,
            },
            Duration::from_secs(1),
            move || {
                runs.fetch_add(1, Ordering::Relaxed);
                std::future::ready(Ok::<_, io::Error>(()))
            },
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let process = tokio::spawn(coordinate_server_process(
            health,
            vec![task],
            move |cancellation| async move {
                cancellation.cancelled().await;
                stopped.store(true, Ordering::Release);
                Ok::<_, io::Error>(())
            },
            move |cancellation| async move {
                cancellation.cancelled().await;
                http_stop.store(true, Ordering::Release);
                Ok(())
            },
            async move {
                let _ = shutdown_rx.await;
            },
            Duration::from_secs(1),
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            while worker_runs.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        shutdown_tx.send(()).unwrap();
        let report = process.await.unwrap().unwrap();

        assert_eq!(
            report,
            ServerShutdownReport {
                workers: ShutdownReport {
                    completed: 1,
                    aborted: 0,
                },
                http_aborted: false,
                invalidator_aborted: false,
            }
        );
        assert!(invalidator_stopped.load(Ordering::Acquire));
        assert!(http_stopped.load(Ordering::Acquire));
        assert!(worker_runs.load(Ordering::Relaxed) >= 1);
    }

    #[tokio::test]
    async fn invalidator_failure_marks_health_unhealthy_and_terminates_process() {
        let health = HealthState::new(true);
        let error = coordinate_server_process(
            health.clone(),
            Vec::new(),
            |_| std::future::ready(Err(io::Error::other("replication failed"))),
            |cancellation| async move {
                cancellation.cancelled().await;
                Ok(())
            },
            std::future::pending(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, ServerProcessError::Invalidator(message) if message == "replication failed")
        );
        assert!(!health.wal_healthy());
    }

    #[tokio::test]
    async fn public_server_attaches_the_socket_peer_to_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route(
            "/peer",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }),
        );
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let process = tokio::spawn(run_server_process(
            listener,
            router,
            HealthState::new(true),
            Vec::new(),
            |cancellation| async move {
                cancellation.cancelled().await;
                Ok::<_, io::Error>(())
            },
            async move {
                let _ = shutdown_rx.await;
            },
            Duration::from_secs(1),
        ));

        let response = reqwest::get(format!("http://{address}/peer"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(response, "127.0.0.1");

        shutdown_tx.send(()).unwrap();
        assert!(!process.await.unwrap().unwrap().http_aborted);
    }
}
