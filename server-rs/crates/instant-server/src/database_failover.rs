use std::str::FromStr;
use std::time::Duration;

use thiserror::Error;
use tokio_postgres::Client;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseFailoverPhase {
    WaitForFollowers,
    PauseWrites,
    CancelInProgress,
    VerifyCatchUp,
    Resume,
    Completed,
    Rollback,
    Failed,
}

impl DatabaseFailoverPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WaitForFollowers => "wait-for-followers",
            Self::PauseWrites => "pause-writes",
            Self::CancelInProgress => "cancel-in-progress",
            Self::VerifyCatchUp => "verify-catch-up",
            Self::Resume => "resume",
            Self::Completed => "completed",
            Self::Rollback => "rollback",
            Self::Failed => "failed",
        }
    }

    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::WaitForFollowers, Self::PauseWrites)
                | (Self::PauseWrites, Self::CancelInProgress)
                | (Self::CancelInProgress, Self::VerifyCatchUp)
                | (Self::VerifyCatchUp, Self::Resume)
                | (Self::Resume, Self::Completed)
                | (
                    Self::WaitForFollowers
                        | Self::PauseWrites
                        | Self::CancelInProgress
                        | Self::VerifyCatchUp,
                    Self::Rollback
                )
                | (
                    Self::WaitForFollowers
                        | Self::PauseWrites
                        | Self::CancelInProgress
                        | Self::VerifyCatchUp
                        | Self::Resume,
                    Self::Failed
                )
        )
    }
}

impl FromStr for DatabaseFailoverPhase {
    type Err = DatabaseFailoverError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "wait-for-followers" => Ok(Self::WaitForFollowers),
            "pause-writes" => Ok(Self::PauseWrites),
            "cancel-in-progress" => Ok(Self::CancelInProgress),
            "verify-catch-up" => Ok(Self::VerifyCatchUp),
            "resume" => Ok(Self::Resume),
            "completed" => Ok(Self::Completed),
            "rollback" => Ok(Self::Rollback),
            "failed" => Ok(Self::Failed),
            _ => Err(DatabaseFailoverError::InvalidPhase(value.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseFailoverParticipantStatus {
    Pending,
    Ready,
    PauseWrites,
    CancelInProgress,
    Resume,
    Rollback,
    Failed,
}

impl DatabaseFailoverParticipantStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::PauseWrites => "pause-writes",
            Self::CancelInProgress => "cancel-in-progress",
            Self::Resume => "resume",
            Self::Rollback => "rollback",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseFailover {
    pub id: Uuid,
    pub leader_machine_id: Uuid,
    pub source_database_id: String,
    pub target_database_id: String,
    pub phase: DatabaseFailoverPhase,
    pub marker_transaction_id: Option<i64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseFailoverParticipant {
    pub machine_id: Uuid,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DatabaseFailoverParticipantProgress {
    pub total: i64,
    pub matching: i64,
    pub failed: Vec<Uuid>,
}

impl DatabaseFailoverParticipantProgress {
    #[must_use]
    pub fn complete(&self) -> bool {
        self.total > 0 && self.total == self.matching && self.failed.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum DatabaseFailoverError {
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
    #[error("invalid database failover phase: {0}")]
    InvalidPhase(String),
    #[error("database failover phase cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: DatabaseFailoverPhase,
        to: DatabaseFailoverPhase,
    },
    #[error("another database failover is already active")]
    AlreadyActive,
    #[error("database failover state changed concurrently")]
    ConcurrentTransition,
    #[error("database failover identifiers must be non-secret names, not connection URLs")]
    InvalidDatabaseIdentity,
    #[error("database failover cannot start without active Rust server machines")]
    NoActiveMachines,
    #[error("machine {machine_id} is not an expected participant in failover {failover_id}")]
    NotParticipant { failover_id: Uuid, machine_id: Uuid },
    #[error("target database readiness check failed: {0}")]
    TargetConnection(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseFailoverTimings {
    pub ready_timeout: Duration,
    pub pause_timeout: Duration,
    pub transaction_drain: Duration,
    pub cancel_timeout: Duration,
    pub catch_up_timeout: Duration,
    pub resume_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for DatabaseFailoverTimings {
    fn default() -> Self {
        Self {
            ready_timeout: Duration::from_millis(500),
            pause_timeout: Duration::from_millis(500),
            transaction_drain: Duration::from_millis(2_500),
            cancel_timeout: Duration::from_secs(1),
            catch_up_timeout: Duration::from_millis(5_050),
            resume_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(50),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseFailoverReport {
    pub failover_id: Uuid,
    pub participants: i64,
    pub marker_transaction_id: i64,
}

#[derive(Debug, Error)]
pub enum DatabaseFailoverLeaderError {
    #[error(transparent)]
    Coordinator(#[from] DatabaseFailoverError),
    #[error("database failover was cancelled")]
    Cancelled,
    #[error("database failover timed out waiting for {phase:?}")]
    Timeout { phase: DatabaseFailoverPhase },
    #[error("database failover participants failed during {phase:?}: {machines:?}")]
    ParticipantsFailed {
        phase: DatabaseFailoverPhase,
        machines: Vec<Uuid>,
    },
    #[error("replication marker {transaction_id} did not reach the target database")]
    MarkerNotVisible { transaction_id: i64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DatabaseFailoverParticipantOutcome {
    Cancelled,
    Resume(DatabaseFailover),
    Rollback(DatabaseFailover),
    Failed(DatabaseFailover),
}

/// Durable replacement for the JVM failover Hazelcast map. Every transition
/// is compare-and-set in PostgreSQL and each machine acknowledgement is
/// independently restart-safe.
pub struct PostgresDatabaseFailoverCoordinator {
    client: Client,
}

impl PostgresDatabaseFailoverCoordinator {
    #[must_use]
    pub const fn new(client: Client) -> Self {
        Self { client }
    }

    /// Creates the single active fleet switchover.
    ///
    /// # Errors
    ///
    /// Returns `AlreadyActive` when another non-terminal operation exists.
    pub async fn begin(
        &self,
        leader_machine_id: Uuid,
        source_database_id: &str,
        target_database_id: &str,
    ) -> Result<DatabaseFailover, DatabaseFailoverError> {
        if !safe_database_identity(source_database_id)
            || !safe_database_identity(target_database_id)
        {
            return Err(DatabaseFailoverError::InvalidDatabaseIdentity);
        }
        let id = Uuid::new_v4();
        let row = self
            .client
            .query_opt(
                "INSERT INTO rust_database_failovers (
                   id, leader_machine_id, source_database_id, target_database_id, phase
                 ) VALUES ($1, $2, $3, $4, 'wait-for-followers')
                 ON CONFLICT ((true))
                   WHERE phase NOT IN ('completed', 'rollback', 'failed')
                 DO NOTHING
                 RETURNING id, leader_machine_id, source_database_id, target_database_id,
                           phase, marker_transaction_id, error",
                &[
                    &id,
                    &leader_machine_id,
                    &source_database_id,
                    &target_database_id,
                ],
            )
            .await?
            .ok_or(DatabaseFailoverError::AlreadyActive)?;
        decode_failover(&row)
    }

    /// Creates the operation and atomically snapshots every machine with a
    /// heartbeat in the last 30 seconds as an expected participant.
    ///
    /// # Errors
    ///
    /// Returns when identities are unsafe, another operation owns the unique
    /// active slot, or no live Rust servers are registered.
    pub async fn begin_for_active_machines(
        &self,
        leader_machine_id: Uuid,
        source_database_id: &str,
        target_database_id: &str,
    ) -> Result<(DatabaseFailover, i64), DatabaseFailoverError> {
        if !safe_database_identity(source_database_id)
            || !safe_database_identity(target_database_id)
        {
            return Err(DatabaseFailoverError::InvalidDatabaseIdentity);
        }
        let id = Uuid::new_v4();
        let row = self
            .client
            .query_opt(
                "WITH live_machines AS MATERIALIZED (
                   SELECT machine_id FROM rust_realtime_machines
                   WHERE updated_at >= now() - interval '30 seconds'
                 ), operation AS (
                   INSERT INTO rust_database_failovers (
                     id, leader_machine_id, source_database_id, target_database_id, phase
                   )
                   SELECT $1, $2, $3, $4, 'wait-for-followers'
                   WHERE EXISTS (SELECT 1 FROM live_machines)
                   ON CONFLICT ((true))
                     WHERE phase NOT IN ('completed', 'rollback', 'failed')
                   DO NOTHING
                   RETURNING id, leader_machine_id, source_database_id, target_database_id,
                             phase, marker_transaction_id, error
                 ), participants AS (
                   INSERT INTO rust_database_failover_participants
                     (failover_id, machine_id, status)
                   SELECT operation.id, live_machines.machine_id, 'pending'
                   FROM operation CROSS JOIN live_machines
                   RETURNING machine_id
                 )
                 SELECT operation.id, operation.leader_machine_id,
                        operation.source_database_id, operation.target_database_id,
                        operation.phase, operation.marker_transaction_id, operation.error,
                        (SELECT count(*)::bigint FROM participants)
                 FROM operation",
                &[
                    &id,
                    &leader_machine_id,
                    &source_database_id,
                    &target_database_id,
                ],
            )
            .await?;
        let Some(row) = row else {
            return if self.active().await?.is_some() {
                Err(DatabaseFailoverError::AlreadyActive)
            } else {
                Err(DatabaseFailoverError::NoActiveMachines)
            };
        };
        Ok((decode_failover(&row)?, row.get(7)))
    }

    /// Returns the current non-terminal operation, if any.
    ///
    /// # Errors
    ///
    /// Returns database or corrupt phase errors.
    pub async fn active(&self) -> Result<Option<DatabaseFailover>, DatabaseFailoverError> {
        self.client
            .query_opt(
                "SELECT id, leader_machine_id, source_database_id, target_database_id,
                        phase, marker_transaction_id, error
                 FROM rust_database_failovers
                 WHERE phase NOT IN ('completed', 'rollback', 'failed')
                 ORDER BY created_at DESC LIMIT 1",
                &[],
            )
            .await?
            .as_ref()
            .map(decode_failover)
            .transpose()
    }

    /// Loads one operation including terminal rollback/failure phases.
    ///
    /// # Errors
    ///
    /// Returns database or corrupt phase errors.
    pub async fn get(&self, id: Uuid) -> Result<Option<DatabaseFailover>, DatabaseFailoverError> {
        self.client
            .query_opt(
                "SELECT id, leader_machine_id, source_database_id, target_database_id,
                        phase, marker_transaction_id, error
                 FROM rust_database_failovers WHERE id = $1",
                &[&id],
            )
            .await?
            .as_ref()
            .map(decode_failover)
            .transpose()
    }

    /// Compare-and-sets the phase so stale leaders cannot skip or overwrite a
    /// transition. Marker/error fields are carried only when supplied.
    ///
    /// # Errors
    ///
    /// Returns for an illegal edge or concurrent state change.
    pub async fn transition(
        &self,
        id: Uuid,
        from: DatabaseFailoverPhase,
        to: DatabaseFailoverPhase,
        marker_transaction_id: Option<i64>,
        error: Option<&str>,
    ) -> Result<DatabaseFailover, DatabaseFailoverError> {
        if !from.can_transition_to(to) {
            return Err(DatabaseFailoverError::InvalidTransition { from, to });
        }
        let row = self
            .client
            .query_opt(
                "UPDATE rust_database_failovers
                 SET phase = $3,
                     marker_transaction_id = coalesce($4, marker_transaction_id),
                     error = coalesce($5, error)
                 WHERE id = $1 AND phase = $2
                 RETURNING id, leader_machine_id, source_database_id, target_database_id,
                           phase, marker_transaction_id, error",
                &[
                    &id,
                    &from.as_str(),
                    &to.as_str(),
                    &marker_transaction_id,
                    &error,
                ],
            )
            .await?
            .ok_or(DatabaseFailoverError::ConcurrentTransition)?;
        decode_failover(&row)
    }

    /// Upserts this machine's acknowledgement for the active operation.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn acknowledge(
        &self,
        failover_id: Uuid,
        machine_id: Uuid,
        status: DatabaseFailoverParticipantStatus,
        error: Option<&str>,
    ) -> Result<(), DatabaseFailoverError> {
        let updated = self
            .client
            .execute(
                "UPDATE rust_database_failover_participants
                 SET status = $3, error = $4
                 WHERE failover_id = $1 AND machine_id = $2",
                &[&failover_id, &machine_id, &status.as_str(), &error],
            )
            .await?;
        if updated == 0 {
            return Err(DatabaseFailoverError::NotParticipant {
                failover_id,
                machine_id,
            });
        }
        Ok(())
    }

    /// Lists fleet acknowledgements in deterministic machine order.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn participants(
        &self,
        failover_id: Uuid,
    ) -> Result<Vec<DatabaseFailoverParticipant>, DatabaseFailoverError> {
        Ok(self
            .client
            .query(
                "SELECT machine_id, status, error
                 FROM rust_database_failover_participants
                 WHERE failover_id = $1 ORDER BY machine_id",
                &[&failover_id],
            )
            .await?
            .into_iter()
            .map(|row| DatabaseFailoverParticipant {
                machine_id: row.get(0),
                status: row.get(1),
                error: row.get(2),
            })
            .collect())
    }

    /// Counts acknowledgements for a required fleet phase and identifies any
    /// participant that has reported failure.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn participant_progress(
        &self,
        failover_id: Uuid,
        expected: DatabaseFailoverParticipantStatus,
    ) -> Result<DatabaseFailoverParticipantProgress, DatabaseFailoverError> {
        let rows = self
            .client
            .query(
                "SELECT machine_id, status
                 FROM rust_database_failover_participants
                 WHERE failover_id = $1",
                &[&failover_id],
            )
            .await?;
        let mut progress = DatabaseFailoverParticipantProgress::default();
        for row in rows {
            progress.total += 1;
            let machine_id = row.get::<_, Uuid>(0);
            let status = row.get::<_, String>(1);
            if status == expected.as_str() {
                progress.matching += 1;
            } else if status == DatabaseFailoverParticipantStatus::Failed.as_str() {
                progress.failed.push(machine_id);
            }
        }
        progress.failed.sort_unstable();
        Ok(progress)
    }

    /// Inserts the same transaction-row replication marker used by the JVM
    /// switchover procedure.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn create_replication_marker(
        &self,
        app_id: Uuid,
    ) -> Result<i64, DatabaseFailoverError> {
        Ok(self
            .client
            .query_one(
                "INSERT INTO transactions (app_id) VALUES ($1) RETURNING id",
                &[&app_id],
            )
            .await?
            .get(0))
    }

    /// Checks whether the exact marker has replicated to this database.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn replication_marker_visible(
        &self,
        app_id: Uuid,
        transaction_id: i64,
    ) -> Result<bool, DatabaseFailoverError> {
        Ok(self
            .client
            .query_opt(
                "SELECT 1 FROM transactions WHERE app_id = $1 AND id = $2",
                &[&app_id, &transaction_id],
            )
            .await?
            .is_some())
    }

    /// Moves sequences beyond values that could still arrive from the old
    /// primary, preserving the JVM's 1,000/100,000 collision headroom.
    ///
    /// # Errors
    ///
    /// Returns database failures.
    pub async fn advance_failover_sequences(
        &self,
        marker_transaction_id: i64,
    ) -> Result<(), DatabaseFailoverError> {
        self.client
            .query_one(
                "SELECT setval('transactions_id_seq', $1::bigint, true)",
                &[&marker_transaction_id.saturating_add(1_000)],
            )
            .await?;
        self.client
            .query_one(
                "SELECT setval(
                   'triples_size_updates_id_seq',
                   nextval('triples_size_updates_id_seq') + 100000,
                   true
                 )",
                &[],
            )
            .await?;
        Ok(())
    }
}

/// Polls durable fleet state for one running server. `pause_requests` stops
/// accepting work, while `cancel_database` drops in-flight PostgreSQL work at
/// the later cancellation phase. The caller rebuilds against source or target
/// after the returned terminal instruction.
///
/// # Errors
///
/// Returns database, membership, or corrupt phase failures.
pub async fn run_database_failover_participant(
    coordinator: &PostgresDatabaseFailoverCoordinator,
    machine_id: Uuid,
    target_database_url: &str,
    pause_requests: &CancellationToken,
    cancel_database: &CancellationToken,
    cancellation: &CancellationToken,
) -> Result<DatabaseFailoverParticipantOutcome, DatabaseFailoverError> {
    let mut operation_id = None;
    let mut acknowledged = None;
    loop {
        tokio::select! {
            () = cancellation.cancelled() => {
                return Ok(DatabaseFailoverParticipantOutcome::Cancelled);
            }
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        let operation = match operation_id {
            Some(id) => coordinator.get(id).await?,
            None => coordinator.active().await?,
        };
        let Some(operation) = operation else {
            continue;
        };
        if operation_id != Some(operation.id) {
            operation_id = Some(operation.id);
            acknowledged = None;
        }
        match operation.phase {
            DatabaseFailoverPhase::WaitForFollowers => {
                acknowledge_once(
                    coordinator,
                    operation.id,
                    machine_id,
                    DatabaseFailoverParticipantStatus::Ready,
                    &mut acknowledged,
                )
                .await?;
            }
            DatabaseFailoverPhase::PauseWrites => {
                pause_requests.cancel();
                acknowledge_once(
                    coordinator,
                    operation.id,
                    machine_id,
                    DatabaseFailoverParticipantStatus::PauseWrites,
                    &mut acknowledged,
                )
                .await?;
            }
            DatabaseFailoverPhase::CancelInProgress => {
                pause_requests.cancel();
                cancel_database.cancel();
                acknowledge_once(
                    coordinator,
                    operation.id,
                    machine_id,
                    DatabaseFailoverParticipantStatus::CancelInProgress,
                    &mut acknowledged,
                )
                .await?;
            }
            DatabaseFailoverPhase::VerifyCatchUp | DatabaseFailoverPhase::Completed => {}
            DatabaseFailoverPhase::Resume => {
                if let Err(error) = validate_target_database(target_database_url).await {
                    let message = error.to_string();
                    let _ = coordinator
                        .acknowledge(
                            operation.id,
                            machine_id,
                            DatabaseFailoverParticipantStatus::Failed,
                            Some(&message),
                        )
                        .await;
                    return Err(error);
                }
                coordinator
                    .acknowledge(
                        operation.id,
                        machine_id,
                        DatabaseFailoverParticipantStatus::Resume,
                        None,
                    )
                    .await?;
                return Ok(DatabaseFailoverParticipantOutcome::Resume(operation));
            }
            DatabaseFailoverPhase::Rollback => {
                coordinator
                    .acknowledge(
                        operation.id,
                        machine_id,
                        DatabaseFailoverParticipantStatus::Rollback,
                        None,
                    )
                    .await?;
                return Ok(DatabaseFailoverParticipantOutcome::Rollback(operation));
            }
            DatabaseFailoverPhase::Failed => {
                return Ok(DatabaseFailoverParticipantOutcome::Failed(operation));
            }
        }
    }
}

async fn validate_target_database(database_url: &str) -> Result<(), DatabaseFailoverError> {
    let (mut clients, drivers) = crate::PostgresConnectionDrivers::connect_many(database_url, 1)
        .await
        .map_err(|error| DatabaseFailoverError::TargetConnection(error.to_string()))?;
    let client = clients
        .pop()
        .ok_or_else(|| DatabaseFailoverError::TargetConnection("missing client".to_owned()))?;
    let cancellation = CancellationToken::new();
    let driver = tokio::spawn(drivers.supervise(cancellation.child_token()));
    let readiness = client
        .query_one(
            "SELECT NOT pg_is_in_recovery()
                    AND current_setting('transaction_read_only') = 'off'",
            &[],
        )
        .await
        .map_err(|error| DatabaseFailoverError::TargetConnection(error.to_string()))
        .map(|row| row.get::<_, bool>(0));
    drop(client);
    cancellation.cancel();
    let driver_result = driver
        .await
        .map_err(|error| DatabaseFailoverError::TargetConnection(error.to_string()))?
        .map_err(|error| DatabaseFailoverError::TargetConnection(error.to_string()));
    if !readiness? {
        return Err(DatabaseFailoverError::TargetConnection(
            "target is still read-only or in recovery".to_owned(),
        ));
    }
    driver_result
}

async fn acknowledge_once(
    coordinator: &PostgresDatabaseFailoverCoordinator,
    failover_id: Uuid,
    machine_id: Uuid,
    status: DatabaseFailoverParticipantStatus,
    acknowledged: &mut Option<DatabaseFailoverParticipantStatus>,
) -> Result<(), DatabaseFailoverError> {
    if *acknowledged != Some(status) {
        coordinator
            .acknowledge(failover_id, machine_id, status, None)
            .await?;
        *acknowledged = Some(status);
    }
    Ok(())
}

/// Executes the leader side of the planned cross-cluster switchover. Running
/// server participants must acknowledge each phase; no resume occurs until
/// the marker is visible and collision-prone sequences are advanced.
///
/// # Errors
///
/// Returns coordinator, fleet, cancellation, timeout, or replication errors.
/// Before `resume`, failures best-effort transition the durable operation to
/// `rollback`; after `resume`, they mark it failed for operator intervention.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn run_database_failover(
    source: &PostgresDatabaseFailoverCoordinator,
    target: &PostgresDatabaseFailoverCoordinator,
    leader_machine_id: Uuid,
    source_database_id: &str,
    target_database_id: &str,
    marker_app_id: Uuid,
    timings: DatabaseFailoverTimings,
    cancellation: &CancellationToken,
) -> Result<DatabaseFailoverReport, DatabaseFailoverLeaderError> {
    let (operation, participants) = source
        .begin_for_active_machines(leader_machine_id, source_database_id, target_database_id)
        .await?;
    let mut phase = DatabaseFailoverPhase::WaitForFollowers;
    let result = async {
        wait_for_participants(
            source,
            operation.id,
            DatabaseFailoverParticipantStatus::Ready,
            phase,
            timings.ready_timeout,
            timings.poll_interval,
            cancellation,
        )
        .await?;
        source
            .transition(
                operation.id,
                phase,
                DatabaseFailoverPhase::PauseWrites,
                None,
                None,
            )
            .await?;
        phase = DatabaseFailoverPhase::PauseWrites;
        wait_for_participants(
            source,
            operation.id,
            DatabaseFailoverParticipantStatus::PauseWrites,
            phase,
            timings.pause_timeout,
            timings.poll_interval,
            cancellation,
        )
        .await?;
        tokio::select! {
            () = cancellation.cancelled() => return Err(DatabaseFailoverLeaderError::Cancelled),
            () = tokio::time::sleep(timings.transaction_drain) => {}
        }
        source
            .transition(
                operation.id,
                phase,
                DatabaseFailoverPhase::CancelInProgress,
                None,
                None,
            )
            .await?;
        phase = DatabaseFailoverPhase::CancelInProgress;
        wait_for_participants(
            source,
            operation.id,
            DatabaseFailoverParticipantStatus::CancelInProgress,
            phase,
            timings.cancel_timeout,
            timings.poll_interval,
            cancellation,
        )
        .await?;
        let marker_transaction_id = source.create_replication_marker(marker_app_id).await?;
        source
            .transition(
                operation.id,
                phase,
                DatabaseFailoverPhase::VerifyCatchUp,
                Some(marker_transaction_id),
                None,
            )
            .await?;
        phase = DatabaseFailoverPhase::VerifyCatchUp;
        wait_for_marker(
            target,
            marker_app_id,
            marker_transaction_id,
            timings.catch_up_timeout,
            timings.poll_interval,
            cancellation,
        )
        .await?;
        target
            .advance_failover_sequences(marker_transaction_id)
            .await?;
        source
            .transition(
                operation.id,
                phase,
                DatabaseFailoverPhase::Resume,
                None,
                None,
            )
            .await?;
        phase = DatabaseFailoverPhase::Resume;
        wait_for_participants(
            source,
            operation.id,
            DatabaseFailoverParticipantStatus::Resume,
            phase,
            timings.resume_timeout,
            timings.poll_interval,
            cancellation,
        )
        .await?;
        source
            .transition(
                operation.id,
                phase,
                DatabaseFailoverPhase::Completed,
                None,
                None,
            )
            .await?;
        Ok(DatabaseFailoverReport {
            failover_id: operation.id,
            participants,
            marker_transaction_id,
        })
    }
    .await;
    if let Err(error) = &result {
        let terminal = if phase == DatabaseFailoverPhase::Resume {
            DatabaseFailoverPhase::Failed
        } else {
            DatabaseFailoverPhase::Rollback
        };
        let _ = source
            .transition(
                operation.id,
                phase,
                terminal,
                None,
                Some(&error.to_string()),
            )
            .await;
    }
    result
}

async fn wait_for_participants(
    coordinator: &PostgresDatabaseFailoverCoordinator,
    failover_id: Uuid,
    expected: DatabaseFailoverParticipantStatus,
    phase: DatabaseFailoverPhase,
    timeout: Duration,
    poll_interval: Duration,
    cancellation: &CancellationToken,
) -> Result<(), DatabaseFailoverLeaderError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let progress = coordinator
            .participant_progress(failover_id, expected)
            .await?;
        if !progress.failed.is_empty() {
            return Err(DatabaseFailoverLeaderError::ParticipantsFailed {
                phase,
                machines: progress.failed,
            });
        }
        if progress.complete() {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(DatabaseFailoverLeaderError::Timeout { phase });
        }
        tokio::select! {
            () = cancellation.cancelled() => return Err(DatabaseFailoverLeaderError::Cancelled),
            () = tokio::time::sleep(poll_interval.min(deadline - now)) => {}
        }
    }
}

async fn wait_for_marker(
    target: &PostgresDatabaseFailoverCoordinator,
    app_id: Uuid,
    transaction_id: i64,
    timeout: Duration,
    poll_interval: Duration,
    cancellation: &CancellationToken,
) -> Result<(), DatabaseFailoverLeaderError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if target
            .replication_marker_visible(app_id, transaction_id)
            .await?
        {
            return Ok(());
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(DatabaseFailoverLeaderError::MarkerNotVisible { transaction_id });
        }
        tokio::select! {
            () = cancellation.cancelled() => return Err(DatabaseFailoverLeaderError::Cancelled),
            () = tokio::time::sleep(poll_interval.min(deadline - now)) => {}
        }
    }
}

fn decode_failover(row: &tokio_postgres::Row) -> Result<DatabaseFailover, DatabaseFailoverError> {
    Ok(DatabaseFailover {
        id: row.get(0),
        leader_machine_id: row.get(1),
        source_database_id: row.get(2),
        target_database_id: row.get(3),
        phase: row.get::<_, String>(4).parse()?,
        marker_transaction_id: row.get(5),
        error: row.get(6),
    })
}

fn safe_database_identity(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 255
        && !value.contains("://")
        && !value.contains('@')
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_graph_forbids_skips_and_terminal_reentry() {
        assert!(
            DatabaseFailoverPhase::WaitForFollowers
                .can_transition_to(DatabaseFailoverPhase::PauseWrites)
        );
        assert!(
            DatabaseFailoverPhase::VerifyCatchUp.can_transition_to(DatabaseFailoverPhase::Rollback)
        );
        assert!(DatabaseFailoverPhase::Resume.can_transition_to(DatabaseFailoverPhase::Completed));
        assert!(
            !DatabaseFailoverPhase::WaitForFollowers
                .can_transition_to(DatabaseFailoverPhase::Resume)
        );
        assert!(
            !DatabaseFailoverPhase::Completed
                .can_transition_to(DatabaseFailoverPhase::WaitForFollowers)
        );
        assert!(
            !DatabaseFailoverPhase::Rollback.can_transition_to(DatabaseFailoverPhase::Completed)
        );
    }

    #[test]
    fn phase_strings_round_trip_the_database_constraints() {
        for phase in [
            DatabaseFailoverPhase::WaitForFollowers,
            DatabaseFailoverPhase::PauseWrites,
            DatabaseFailoverPhase::CancelInProgress,
            DatabaseFailoverPhase::VerifyCatchUp,
            DatabaseFailoverPhase::Resume,
            DatabaseFailoverPhase::Completed,
            DatabaseFailoverPhase::Rollback,
            DatabaseFailoverPhase::Failed,
        ] {
            assert_eq!(
                phase.as_str().parse::<DatabaseFailoverPhase>().unwrap(),
                phase
            );
        }
    }

    #[test]
    fn durable_database_identifiers_cannot_contain_connection_secrets() {
        assert!(safe_database_identity("instant-production"));
        assert!(safe_database_identity("db.internal:5432/instant"));
        assert!(!safe_database_identity(""));
        assert!(!safe_database_identity(
            "postgresql://user:secret@db.internal/instant"
        ));
        assert!(!safe_database_identity("user@db.internal"));
    }

    #[test]
    fn leader_timings_preserve_the_reference_safety_windows() {
        let timings = DatabaseFailoverTimings::default();
        assert_eq!(timings.ready_timeout, Duration::from_millis(500));
        assert_eq!(timings.pause_timeout, Duration::from_millis(500));
        assert_eq!(timings.transaction_drain, Duration::from_millis(2_500));
        assert_eq!(timings.cancel_timeout, Duration::from_secs(1));
        assert_eq!(timings.catch_up_timeout, Duration::from_millis(5_050));
        assert_eq!(timings.poll_interval, Duration::from_millis(50));
    }
}
