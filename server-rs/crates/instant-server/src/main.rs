#![recursion_limit = "256"]

mod native_allocator;

use std::io::{Read as _, Write as _};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use instant_server::{
    AuroraDatabase, AuroraDatabaseError, AwsAuroraDatabaseProvider, AwsKmsTinkKeysetLoader,
    ConfigAppError, DatabaseFailoverError, DatabaseFailoverLeaderError,
    DatabaseFailoverParticipantOutcome, DatabaseFailoverTimings, FailoverGateError,
    INSTANT_CONFIG_APP_ID, MigrationError, PostgresConnectionDrivers, PostgresConnectionError,
    PostgresDatabaseFailoverCoordinator, PostgresFailoverGate, ProductionDatabaseClients,
    ProductionRouteAssemblyError, ProductionServerProcessError, ProductionServerSecrets,
    ProductionServerSecretsError, RuntimeSyncHistory, ServerConfig, ServerConfigError,
    ShutdownSignals, SystemCatalogMigrationError, TelemetryError, audit_database,
    build_production_routes_with_realtime_history, ensure_system_catalog, force_migration_version,
    generate_self_hosted_secrets_env, get_config_flag, init_error_reporting, init_telemetry,
    install_panic_hook, list_config_flags, migrate_database, reencrypt_stored_secrets,
    rollback_database, rotate_self_hosted_secrets_env, run_database_failover,
    run_database_failover_participant, run_production_server_process, set_config_flag,
    unset_config_flag, verify_database_migrations,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Error)]
enum LaunchError {
    #[error("could not create the Tokio runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error(transparent)]
    Config(#[from] ServerConfigError),
    #[error(transparent)]
    Telemetry(#[from] TelemetryError),
    #[error(transparent)]
    ErrorReporting(#[from] instant_server::ErrorReportingError),
    #[error(transparent)]
    Secrets(#[from] ProductionServerSecretsError),
    #[error(transparent)]
    Keyset(#[from] instant_server::TinkKeysetLoadError),
    #[error(transparent)]
    SecretCipher(#[from] instant_server::OAuthCredentialError),
    #[error("could not read the BYOP connection string from standard input: {0}")]
    ByopInput(#[source] std::io::Error),
    #[error("the BYOP connection string must be a PostgreSQL URL")]
    InvalidByopUrl,
    #[error("app {0} does not exist")]
    UnknownByopApp(Uuid),
    #[error("could not update app BYOP configuration: {0}")]
    ByopDatabase(#[source] tokio_postgres::Error),
    #[error(transparent)]
    Database(#[from] instant_server::ProductionDatabaseBootstrapError),
    #[error(transparent)]
    DatabaseDriver(#[from] PostgresConnectionError),
    #[error(transparent)]
    AuroraDatabase(#[from] AuroraDatabaseError),
    #[error(transparent)]
    DatabaseFailover(#[from] DatabaseFailoverError),
    #[error(transparent)]
    FailoverGate(#[from] FailoverGateError),
    #[error("database failover participant task failed: {0}")]
    DatabaseFailoverTask(#[source] tokio::task::JoinError),
    #[error("database failover requested unknown target {0}")]
    DatabaseFailoverTarget(String),
    #[error("database failover entered failed state: {0}")]
    DatabaseFailoverFailed(String),
    #[error(transparent)]
    DatabaseFailoverLeader(#[from] DatabaseFailoverLeaderError),
    #[error("database failover requires both failing-over and disable-aggregator flags")]
    DatabaseFailoverFlags,
    #[error(transparent)]
    Migration(#[from] MigrationError),
    #[error(transparent)]
    SystemCatalog(#[from] SystemCatalogMigrationError),
    #[error(transparent)]
    ConfigApp(#[from] ConfigAppError),
    #[error("invalid JSON flag value: {0}")]
    InvalidFlagValue(#[source] serde_json::Error),
    #[error("could not audit app rules: {0}")]
    Audit(#[source] tokio_postgres::Error),
    #[error("config app flag `{0}` is not set")]
    UnknownFlag(String),
    #[error(
        "usage: instant-server [serve | healthcheck | failover | generate-secrets [--rotate secrets.env] | reencrypt [--dry-run] | configure-byop app-id | flags list | get name | set name json | unset name | audit | migrate [up | down [steps] | force version]]"
    )]
    InvalidArguments,
    #[error(transparent)]
    Assembly(#[from] ProductionRouteAssemblyError),
    #[error("could not register process shutdown signals: {0}")]
    Signals(#[source] std::io::Error),
    #[error("could not bind the HTTP listener: {0}")]
    Bind(#[source] std::io::Error),
    #[error("health check timed out after {0:?}")]
    HealthcheckTimeout(Duration),
    #[error("health check request failed: {0}")]
    HealthcheckIo(#[source] std::io::Error),
    #[error("health check returned an unhealthy response")]
    HealthcheckUnhealthy,
    #[error(transparent)]
    Process(#[from] ProductionServerProcessError),
    #[error(transparent)]
    SecretGeneration(#[from] instant_server::SelfHostedSecretGenerationError),
    #[error(transparent)]
    SecretRotation(#[from] instant_server::SelfHostedSecretRotationError),
    #[error("could not read the secrets file {path}: {source}")]
    SecretsFile {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    SecretReencryption(#[from] instant_server::SecretReencryptionError),
}

#[derive(Clone)]
struct ResolvedDatabase {
    identity: String,
    url: String,
    aurora: Option<AuroraDatabase>,
}

type FailoverParticipantRuntime = (
    tokio::task::JoinHandle<Result<DatabaseFailoverParticipantOutcome, DatabaseFailoverError>>,
    CancellationToken,
    tokio::task::JoinHandle<Result<(), PostgresConnectionError>>,
);

const WORKER_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;
const USAGE: &str = "usage: instant-server [serve | healthcheck | failover | generate-secrets [--rotate secrets.env] | reencrypt [--dry-run] | configure-byop app-id | flags list | get name | set name json | unset name | audit | migrate [up | down [steps] | force version]]";
const HEALTHCHECK_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    if let Err(error) = run() {
        // Also reached when a running server stops on a fatal subsystem error,
        // so don't call it a startup failure; log it too so it lands in the
        // structured logs next to the shutdown, not only on stderr.
        tracing::error!(%error, "instant-server exited with an error");
        eprintln!("instant-server failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), LaunchError> {
    // The production dashboard is a compile-time adapter chain with more than
    // 200 handlers. Dispatching an operation that reaches its terminal polls
    // that nested future on one worker stack, so Tokio's 2 MiB default is too
    // small even for an empty ephemeral app created by the official SDK suite.
    // Tokio applies this size to blocking-pool threads too. Permission rules
    // recurse through the CEL parser and validators, whose limits are sized for
    // this stack (see `instant_permissions::MAX_RULE_NESTING`).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_THREAD_STACK_SIZE)
        .build()
        .map_err(LaunchError::Runtime)?;
    runtime.block_on(Box::pin(launch()))
}

async fn launch() -> Result<(), LaunchError> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() == ["--help"] || arguments.as_slice() == ["-h"] {
        println!("{USAGE}");
        return Ok(());
    }
    if arguments.as_slice() == ["--version"] || arguments.as_slice() == ["-V"] {
        println!("{}", instant_server::display_version());
        return Ok(());
    }
    if arguments.as_slice() == ["generate-secrets"] {
        print!("{}", generate_self_hosted_secrets_env()?);
        return Ok(());
    }
    if let [command, action, path] = arguments.as_slice()
        && command == "generate-secrets"
        && action == "--rotate"
    {
        let path = path.clone();
        let input = std::fs::read_to_string(&path).map_err(|source| LaunchError::SecretsFile {
            path: path.clone(),
            source,
        })?;
        print!("{}", rotate_self_hosted_secrets_env(&input)?);
        return Ok(());
    }
    if arguments.as_slice() == ["healthcheck"] {
        return healthcheck().await;
    }
    let config = ServerConfig::from_env()?;
    if let [command] = arguments.as_slice()
        && command == "reencrypt"
    {
        return reencrypt(config, false).await;
    }
    if let [command, flag] = arguments.as_slice()
        && command == "reencrypt"
        && flag == "--dry-run"
    {
        return reencrypt(config, true).await;
    }
    if let [command, app_id] = arguments.as_slice()
        && command == "configure-byop"
    {
        let app_id = app_id.parse().map_err(|_| LaunchError::InvalidArguments)?;
        return configure_byop(config, app_id).await;
    }
    // Keep stdout machine-readable: the audit reports JSON lines, so it runs
    // before telemetry initialization like the other one-shot commands.
    if let [command] = arguments.as_slice()
        && command == "audit"
    {
        return audit(config).await;
    }
    let machine_id = Uuid::new_v4();
    let error_reporting = init_error_reporting(&config)?;
    init_telemetry(&config, machine_id)?;
    install_panic_hook();
    let result = match arguments.as_slice() {
        [] => Box::pin(serve(config, machine_id)).await,
        [command] if command == "serve" => Box::pin(serve(config, machine_id)).await,
        [command] if command == "failover" => Box::pin(failover(config)).await,
        [command, action] if command == "flags" && action == "list" => {
            flags(config, FlagsCommand::List).await
        }
        [command, action, name] if command == "flags" && action == "get" => {
            flags(config, FlagsCommand::Get(name.clone())).await
        }
        [command, action, name, value] if command == "flags" && action == "set" => {
            flags(config, FlagsCommand::Set(name.clone(), value.clone())).await
        }
        [command, action, name] if command == "flags" && action == "unset" => {
            flags(config, FlagsCommand::Unset(name.clone())).await
        }
        [command] if command == "migrate" => migrate(config, MigrationDirection::Up).await,
        [command, direction] if command == "migrate" && direction == "up" => {
            migrate(config, MigrationDirection::Up).await
        }
        [command, direction] if command == "migrate" && direction == "down" => {
            migrate(config, MigrationDirection::Down(1)).await
        }
        [command, direction, steps] if command == "migrate" && direction == "down" => {
            let steps = steps.parse().map_err(|_| LaunchError::InvalidArguments)?;
            migrate(config, MigrationDirection::Down(steps)).await
        }
        [command, direction, version] if command == "migrate" && direction == "force" => {
            let version = version.parse().map_err(|_| LaunchError::InvalidArguments)?;
            migrate(config, MigrationDirection::Force(version)).await
        }
        _ => Err(LaunchError::InvalidArguments),
    };
    if let Some(reporting) = error_reporting.as_ref() {
        reporting.flush();
    }
    result?;
    Ok(())
}

async fn healthcheck() -> Result<(), LaunchError> {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8888);
    healthcheck_port_with_timeout(port, HEALTHCHECK_TIMEOUT).await
}

async fn healthcheck_port_with_timeout(port: u16, timeout: Duration) -> Result<(), LaunchError> {
    tokio::time::timeout(timeout, healthcheck_port(port))
        .await
        .map_err(|_| LaunchError::HealthcheckTimeout(timeout))?
}

async fn healthcheck_port(port: u16) -> Result<(), LaunchError> {
    let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(LaunchError::HealthcheckIo)?;
    stream
        .write_all(b"GET /health/system HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .map_err(LaunchError::HealthcheckIo)?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(LaunchError::HealthcheckIo)?;
    let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err(LaunchError::HealthcheckUnhealthy);
    };
    let headers = &response[..header_end];
    let body = &response[header_end + 4..];
    if !headers.starts_with(b"HTTP/1.1 200 ") && !headers.starts_with(b"HTTP/1.0 200 ") {
        return Err(LaunchError::HealthcheckUnhealthy);
    }
    let payload: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| LaunchError::HealthcheckUnhealthy)?;
    if payload.get("wal").and_then(serde_json::Value::as_str) != Some("ok") {
        return Err(LaunchError::HealthcheckUnhealthy);
    }
    Ok(())
}

/// Installs an encrypted BYOP URL without placing database credentials in the
/// process argument list. Operators pipe exactly one URL on standard input.
async fn configure_byop(config: ServerConfig, app_id: Uuid) -> Result<(), LaunchError> {
    let mut connection_string = String::new();
    std::io::stdin()
        .read_to_string(&mut connection_string)
        .map_err(LaunchError::ByopInput)?;
    let connection_string = connection_string.trim_end_matches(['\r', '\n']);
    let driver_url = connection_string
        .strip_prefix("jdbc:")
        .unwrap_or(connection_string);
    let parsed = url::Url::parse(driver_url).map_err(|_| LaunchError::InvalidByopUrl)?;
    if connection_string.is_empty()
        || !matches!(parsed.scheme(), "postgres" | "postgresql")
        || parsed.host_str().is_none()
    {
        return Err(LaunchError::InvalidByopUrl);
    }

    let secrets = ProductionServerSecrets::from_env()?;
    let cipher = AwsKmsTinkKeysetLoader::new()
        .load_oauth_cipher(&secrets.oauth_keyset)
        .await?;
    let encrypted = cipher.encrypt_bytes(app_id, connection_string)?;
    let database_url = effective_database_url(&config, Uuid::new_v4()).await?;
    let (mut clients, drivers) = PostgresConnectionDrivers::connect_many(&database_url, 1).await?;
    let client = clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let cancellation = CancellationToken::new();
    let driver_token = cancellation.child_token();
    let driver = tokio::spawn(drivers.supervise(driver_token));
    let updated = client
        .execute(
            "UPDATE apps SET connection_string = $2 WHERE id = $1",
            &[&app_id, &encrypted],
        )
        .await
        .map_err(LaunchError::ByopDatabase);
    drop(client);
    cancellation.cancel();
    let driver_result = driver.await.map_err(PostgresConnectionError::DriverTask)?;
    let updated = updated?;
    driver_result?;
    if updated != 1 {
        return Err(LaunchError::UnknownByopApp(app_id));
    }
    println!("configured BYOP database for app {app_id}");
    Ok(())
}

/// Re-encrypts stored secrets with the primary AEAD key after a rotation and
/// prints per-table counts. `--dry-run` reports what would change without
/// committing updates.
async fn reencrypt(config: ServerConfig, dry_run: bool) -> Result<(), LaunchError> {
    let secrets = ProductionServerSecrets::from_env()?;
    let cipher = AwsKmsTinkKeysetLoader::new()
        .load_oauth_cipher(&secrets.oauth_keyset)
        .await?;
    let database_url = effective_database_url(&config, Uuid::new_v4()).await?;
    let (mut clients, drivers) = PostgresConnectionDrivers::connect_many(&database_url, 1).await?;
    let mut client = clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let cancellation = CancellationToken::new();
    let driver_token = cancellation.child_token();
    let driver = tokio::spawn(drivers.supervise(driver_token));
    let result = reencrypt_stored_secrets(&mut client, &cipher, dry_run).await;
    cancellation.cancel();
    let driver_result = driver.await.map_err(PostgresConnectionError::DriverTask)?;
    drop(client);
    let report = result?;
    driver_result?;
    for table in &report.tables {
        println!(
            "{}\tscanned={}\treencrypted={}",
            table.table, table.scanned, table.reencrypted
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn serve(config: ServerConfig, machine_id: Uuid) -> Result<(), LaunchError> {
    let _start_span = tracing::info_span!("server/start").entered();
    if config.dashboard_signup_open() {
        tracing::warn!(
            "INSTANT_DASHBOARD_SIGNUP=open: anyone who can receive email can create a dashboard account"
        );
    }
    let signals = ShutdownSignals::new().map_err(LaunchError::Signals)?;
    let process_shutdown = CancellationToken::new();
    let signal_shutdown = process_shutdown.clone();
    std::mem::drop(tokio::spawn(async move {
        signals.wait().await;
        signal_shutdown.cancel();
    }));
    let application_name = format!("instant-server, {machine_id}");
    let aurora = aurora_provider(&config).await;
    let mut database = resolve_database(
        config.database_cluster_id(),
        Some(config.database_url()),
        &application_name,
        aurora.as_deref(),
    )
    .await?
    .expect("the primary database URL always exists");
    let target_database = resolve_database(
        config.next_database_cluster_id(),
        config.next_database_url(),
        &application_name,
        aurora.as_deref(),
    )
    .await?;
    // The JVM sync-table transaction queue is process-global. Keep the Rust
    // catch-up window alive when failover rebuilds the production route tree.
    let realtime_sync_history = Arc::new(RuntimeSyncHistory::default());

    loop {
        let secrets = ProductionServerSecrets::from_env()?;
        let (clients, drivers) =
            ProductionDatabaseClients::connect_url(&database.url, config.environment()).await?;
        let assembly = Box::pin(build_production_routes_with_realtime_history(
            &config,
            &database.url,
            target_database.as_ref().map(|target| target.url.as_str()),
            clients,
            secrets,
            machine_id,
            Arc::clone(&realtime_sync_history),
        ))
        .await?;
        let push_runtime = Arc::clone(&assembly.runtime_realtime);
        let push_admin = Arc::clone(&assembly.admin_realtime);
        let metrics_snapshot: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
            instant_server::metrics::render(
                &push_runtime.metrics_snapshot(),
                push_admin.session_count(),
                &push_admin.sdk_sessions(),
            )
        });
        let _metrics = if let Ok(address) = std::env::var("INSTANT_METRICS_ADDR") {
            let address =
                instant_server::metrics::parse_address(&address).map_err(LaunchError::Bind)?;
            Some(
                instant_server::metrics::start(
                    address,
                    Arc::clone(&metrics_snapshot),
                    process_shutdown.clone(),
                )
                .await
                .map_err(LaunchError::Bind)?,
            )
        } else {
            None
        };
        let listener = TcpListener::bind(config.bind_address())
            .await
            .map_err(LaunchError::Bind)?;
        let watcher_cancellation = CancellationToken::new();
        let database_cancellation = CancellationToken::new();
        let failover_pause = CancellationToken::new();
        let participant_cancellation = CancellationToken::new();
        let participant = if let Some(target) = &target_database {
            let (mut controls, control_drivers) =
                PostgresConnectionDrivers::connect_many(&database.url, 1).await?;
            let control = controls.pop().ok_or(LaunchError::InvalidArguments)?;
            let control_cancellation = CancellationToken::new();
            let control_token = control_cancellation.child_token();
            let control_driver = tokio::spawn(control_drivers.supervise(control_token));
            let pause = failover_pause.clone();
            let cancel_database = database_cancellation.clone();
            let target_database_url = target.url.clone();
            let cancellation = participant_cancellation.clone();
            let handle = tokio::spawn(async move {
                let coordinator = PostgresDatabaseFailoverCoordinator::new(control);
                run_database_failover_participant(
                    &coordinator,
                    machine_id,
                    &target_database_url,
                    &pause,
                    &cancel_database,
                    &cancellation,
                )
                .await
            });
            Some((handle, control_cancellation, control_driver))
        } else {
            None
        };
        let replacement = Arc::new(Mutex::new(None));
        let replacement_slot = Arc::clone(&replacement);
        let stop_watcher = watcher_cancellation.clone();
        let shutdown = process_shutdown.clone();
        let provider = aurora.clone();
        let current_aurora = database.aurora.clone();
        let application_name = application_name.clone();
        let pause = failover_pause.clone();
        let coordinated_shutdown = async move {
            let aurora_change = async {
                let Some((provider, current)) = provider.zip(current_aurora) else {
                    std::future::pending().await
                };
                provider
                    .wait_for_writer_change(
                        &current.instance_id,
                        &current.cluster_id,
                        &application_name,
                        &stop_watcher,
                    )
                    .await
            };
            tokio::select! {
                () = shutdown.cancelled() => {}
                () = pause.cancelled() => {}
                next = aurora_change => {
                    if let Some(next) = next {
                        *replacement_slot
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(next);
                    }
                }
            }
        };
        let result = Box::pin(run_production_server_process(
            listener,
            assembly,
            drivers,
            database_cancellation,
            coordinated_shutdown,
            // The realtime drain (up to INSTANT_DRAIN_BUDGET_SECONDS, default 30 s)
            // runs first; 10 s more keeps the worst case under compose's 45 s
            // stop_grace_period.
            Duration::from_secs(10),
        ))
        .await;
        watcher_cancellation.cancel();
        let paused_for_failover = failover_pause.is_cancelled();
        if !paused_for_failover || process_shutdown.is_cancelled() {
            participant_cancellation.cancel();
        }
        let participant_outcome = finish_failover_participant(participant).await?;
        if process_shutdown.is_cancelled() {
            return result.map(|_| ()).map_err(Into::into);
        }
        if let Some(next) = replacement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            result?;
            database = resolved_aurora(next);
            continue;
        }
        if paused_for_failover {
            result?;
            match participant_outcome {
                Some(DatabaseFailoverParticipantOutcome::Resume(operation)) => {
                    let target = target_database.as_ref().ok_or_else(|| {
                        LaunchError::DatabaseFailoverTarget(operation.target_database_id.clone())
                    })?;
                    if operation.target_database_id != target.identity {
                        return Err(LaunchError::DatabaseFailoverTarget(
                            operation.target_database_id,
                        ));
                    }
                    database = target.clone();
                    continue;
                }
                Some(DatabaseFailoverParticipantOutcome::Rollback(_)) => continue,
                Some(DatabaseFailoverParticipantOutcome::Failed(operation)) => {
                    return Err(LaunchError::DatabaseFailoverFailed(
                        operation
                            .error
                            .unwrap_or_else(|| "unknown failure".to_owned()),
                    ));
                }
                Some(DatabaseFailoverParticipantOutcome::Cancelled) | None => {
                    return Err(LaunchError::DatabaseFailoverFailed(
                        "participant stopped during an active failover".to_owned(),
                    ));
                }
            }
        }
        return result.map(|_| ()).map_err(Into::into);
    }
}

async fn failover(config: ServerConfig) -> Result<(), LaunchError> {
    let leader_id = Uuid::new_v4();
    let application_name = format!("instant-failover, {leader_id}");
    let aurora = aurora_provider(&config).await;
    let source = resolve_database(
        config.database_cluster_id(),
        Some(config.database_url()),
        &application_name,
        aurora.as_deref(),
    )
    .await?
    .expect("the primary database URL always exists");
    let target = resolve_database(
        config.next_database_cluster_id(),
        config.next_database_url(),
        &application_name,
        aurora.as_deref(),
    )
    .await?
    .ok_or(LaunchError::InvalidArguments)?;
    if source.identity == target.identity {
        return Err(LaunchError::DatabaseFailoverTarget(target.identity));
    }
    let (mut source_clients, source_drivers) =
        PostgresConnectionDrivers::connect_many(&source.url, 2).await?;
    let (mut target_clients, target_drivers) =
        PostgresConnectionDrivers::connect_many(&target.url, 1).await?;
    let mut source_client = source_clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let failover_gate_client = source_clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let mut target_client = target_clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let source_cancellation = CancellationToken::new();
    let target_cancellation = CancellationToken::new();
    let source_driver = tokio::spawn(source_drivers.supervise(source_cancellation.child_token()));
    let target_driver = tokio::spawn(target_drivers.supervise(target_cancellation.child_token()));
    let result: Result<_, LaunchError> = async {
        verify_database_migrations(&mut source_client).await?;
        verify_database_migrations(&mut target_client).await?;
        let failover_gate = PostgresFailoverGate::new(failover_gate_client);
        if !failover_gate.is_failing_over().await?
            || !failover_gate.is_toggled("disable-aggregator").await?
        {
            return Err(LaunchError::DatabaseFailoverFlags);
        }
        let signals = ShutdownSignals::new().map_err(LaunchError::Signals)?;
        let cancellation = CancellationToken::new();
        let signal_cancellation = cancellation.clone();
        std::mem::drop(tokio::spawn(async move {
            signals.wait().await;
            signal_cancellation.cancel();
        }));
        let source_coordinator = PostgresDatabaseFailoverCoordinator::new(source_client);
        let target_coordinator = PostgresDatabaseFailoverCoordinator::new(target_client);
        Ok(run_database_failover(
            &source_coordinator,
            &target_coordinator,
            leader_id,
            &source.identity,
            &target.identity,
            INSTANT_CONFIG_APP_ID,
            DatabaseFailoverTimings::default(),
            &cancellation,
        )
        .await?)
    }
    .await;
    source_cancellation.cancel();
    target_cancellation.cancel();
    source_driver
        .await
        .map_err(PostgresConnectionError::DriverTask)??;
    target_driver
        .await
        .map_err(PostgresConnectionError::DriverTask)??;
    let report = result?;
    println!(
        "database failover {} completed for {} machines at marker transaction {}",
        report.failover_id, report.participants, report.marker_transaction_id
    );
    Ok(())
}

enum MigrationDirection {
    Up,
    Down(usize),
    Force(u64),
}

enum FlagsCommand {
    List,
    Get(String),
    Set(String, String),
    Unset(String),
}

async fn flags(config: ServerConfig, command: FlagsCommand) -> Result<(), LaunchError> {
    let database_url = effective_database_url(&config, Uuid::new_v4()).await?;
    let (mut clients, drivers) = PostgresConnectionDrivers::connect_many(&database_url, 1).await?;
    let mut client = clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let driver_token = cancellation.child_token();
    let driver = tokio::spawn(drivers.supervise(driver_token));
    let result: Result<(), LaunchError> = async {
        match command {
            FlagsCommand::List => {
                for (setting, value) in list_config_flags(&client).await? {
                    println!(
                        "{setting}\t{}",
                        serde_json::to_string(&value).expect("flag value serializes")
                    );
                }
            }
            FlagsCommand::Get(name) => {
                let value = get_config_flag(&client, &name)
                    .await?
                    .ok_or(LaunchError::UnknownFlag(name))?;
                println!(
                    "{}",
                    serde_json::to_string(&value).expect("flag values serialize")
                );
            }
            FlagsCommand::Set(name, value) => {
                let value = serde_json::from_str(&value).map_err(LaunchError::InvalidFlagValue)?;
                set_config_flag(&mut client, &name, value).await?;
            }
            FlagsCommand::Unset(name) => {
                unset_config_flag(&mut client, &name).await?;
            }
        }
        Ok(())
    }
    .await;
    cancellation.cancel();
    let driver_result = driver.await.map_err(PostgresConnectionError::DriverTask)?;
    drop(client);
    result?;
    driver_result?;
    Ok(())
}

async fn audit(config: ServerConfig) -> Result<(), LaunchError> {
    let database_url = effective_database_url(&config, Uuid::new_v4()).await?;
    let (mut clients, drivers) = PostgresConnectionDrivers::connect_many(&database_url, 1).await?;
    let client = clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let driver_token = cancellation.child_token();
    let driver = tokio::spawn(drivers.supervise(driver_token));
    let result = audit_database(&client).await;
    cancellation.cancel();
    let driver_result = driver.await.map_err(PostgresConnectionError::DriverTask)?;
    drop(client);
    let report = result.map_err(LaunchError::Audit)?;
    driver_result?;
    for finding in &report.findings {
        println!(
            "{}",
            serde_json::to_string(finding).expect("audit findings serialize")
        );
    }
    println!(
        "{}",
        serde_json::to_string(&report.summary_json()).expect("audit summary serializes")
    );
    if report.exit_code() != 0 {
        let _ = std::io::stdout().flush();
        std::process::exit(report.exit_code());
    }
    Ok(())
}

async fn migrate(config: ServerConfig, direction: MigrationDirection) -> Result<(), LaunchError> {
    let database_url = effective_database_url(&config, Uuid::new_v4()).await?;
    let (mut clients, drivers) = PostgresConnectionDrivers::connect_many(&database_url, 1).await?;
    let mut client = clients.pop().ok_or(LaunchError::InvalidArguments)?;
    let cancellation = tokio_util::sync::CancellationToken::new();
    let driver_token = cancellation.child_token();
    let driver = tokio::spawn(drivers.supervise(driver_token));
    let result: Result<_, LaunchError> = async {
        match direction {
            MigrationDirection::Up => {
                let outcome = migrate_database(&mut client).await?;
                ensure_system_catalog(&mut client).await?;
                Ok(outcome)
            }
            MigrationDirection::Down(steps) => Ok(rollback_database(&mut client, steps).await?),
            MigrationDirection::Force(version) => {
                Ok(force_migration_version(&mut client, version).await?)
            }
        }
    }
    .await;
    cancellation.cancel();
    let driver_result = driver.await.map_err(PostgresConnectionError::DriverTask)?;
    drop(client);
    let outcome = result?;
    driver_result?;
    println!(
        "database schema {} -> {}; migration versions: {:?}",
        outcome.previous_version, outcome.current_version, outcome.applied_versions
    );
    Ok(())
}

async fn effective_database_url(
    config: &ServerConfig,
    machine_id: Uuid,
) -> Result<String, AuroraDatabaseError> {
    let application_name = format!("instant-server, {machine_id}");
    let provider = aurora_provider(config).await;
    Ok(resolve_database(
        config.database_cluster_id(),
        Some(config.database_url()),
        &application_name,
        provider.as_deref(),
    )
    .await?
    .expect("the primary database URL always exists")
    .url)
}

#[allow(clippy::unused_async)]
async fn aurora_provider(config: &ServerConfig) -> Option<Arc<AwsAuroraDatabaseProvider>> {
    if config.database_cluster_id().is_none() && config.next_database_cluster_id().is_none() {
        return None;
    }
    Some(Arc::new(AwsAuroraDatabaseProvider::new(&())))
}

async fn resolve_database(
    cluster_id: Option<&str>,
    database_url: Option<&str>,
    application_name: &str,
    aurora: Option<&AwsAuroraDatabaseProvider>,
) -> Result<Option<ResolvedDatabase>, AuroraDatabaseError> {
    if let Some(cluster_id) = cluster_id {
        let provider = aurora.expect("cluster ID guarantees an Aurora provider");
        return Ok(Some(resolved_aurora(
            provider.discover(cluster_id, application_name).await?,
        )));
    }
    Ok(database_url.map(|url| ResolvedDatabase {
        identity: database_identity_from_url(url),
        url: url.to_owned(),
        aurora: None,
    }))
}

fn resolved_aurora(database: AuroraDatabase) -> ResolvedDatabase {
    ResolvedDatabase {
        identity: database.cluster_id.clone(),
        url: database.database_url.clone(),
        aurora: Some(database),
    }
}

fn database_identity_from_url(database_url: &str) -> String {
    let Ok(url) = url::Url::parse(database_url) else {
        return "configured-database".to_owned();
    };
    let host = url.host_str().unwrap_or("configured-database");
    let port = url
        .port()
        .map_or_else(String::new, |port| format!(":{port}"));
    format!("{host}{port}{}", url.path())
}

async fn finish_failover_participant(
    participant: Option<FailoverParticipantRuntime>,
) -> Result<Option<DatabaseFailoverParticipantOutcome>, LaunchError> {
    let Some((participant, driver_cancellation, driver)) = participant else {
        return Ok(None);
    };
    let outcome = participant
        .await
        .map_err(LaunchError::DatabaseFailoverTask)??;
    driver_cancellation.cancel();
    driver
        .await
        .map_err(PostgresConnectionError::DriverTask)??;
    Ok(Some(outcome))
}

#[cfg(test)]
mod healthcheck_tests {
    use super::*;

    async fn serve_response(response: &'static [u8]) -> u16 {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 512];
            let read = stream.read(&mut request).await.unwrap();
            assert!(request[..read].starts_with(b"GET /health/system HTTP/1.1\r\n"));
            stream.write_all(response).await.unwrap();
        });
        port
    }

    #[tokio::test]
    async fn native_healthcheck_accepts_only_healthy_wal_response() {
        let port = serve_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n{\"wal\":\"ok\"}",
        )
        .await;
        healthcheck_port(port).await.unwrap();

        let port = serve_response(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"wal\":\"failed\"}",
        )
        .await;
        assert!(matches!(
            healthcheck_port(port).await,
            Err(LaunchError::HealthcheckUnhealthy)
        ));
    }

    #[tokio::test]
    async fn native_healthcheck_times_out_a_stalled_endpoint() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        });
        assert!(matches!(
            healthcheck_port_with_timeout(port, Duration::from_millis(20)).await,
            Err(LaunchError::HealthcheckTimeout(timeout))
                if timeout == Duration::from_millis(20)
        ));
    }
}
