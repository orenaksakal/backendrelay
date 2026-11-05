use instant_server::{
    DatabaseFailoverError, DatabaseFailoverParticipantStatus, DatabaseFailoverPhase,
    DatabaseFailoverTimings, PostgresDatabaseFailoverCoordinator, run_database_failover,
    run_database_failover_participant,
};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn wait_for_participant_status(
    coordinator: &PostgresDatabaseFailoverCoordinator,
    failover_id: Uuid,
    status: DatabaseFailoverParticipantStatus,
) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let participants = coordinator.participants(failover_id).await.unwrap();
            if !participants.is_empty()
                && participants
                    .iter()
                    .all(|participant| participant.status == status.as_str())
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("every failover participant should acknowledge the phase");
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn durable_failover_state_is_single_owner_compare_and_set_and_acknowledged() {
    let Some(url) = instant_test_support::test_database_url(
        "durable_failover_state_is_single_owner_compare_and_set_and_acknowledged",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    std::mem::drop(tokio::spawn(async move {
        let _ = connection.await;
    }));
    client
        .batch_execute(
            "CREATE TEMP TABLE rust_realtime_machines (
               machine_id uuid PRIMARY KEY,
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE rust_database_failovers (
               id uuid PRIMARY KEY,
               leader_machine_id uuid NOT NULL,
               source_database_id text NOT NULL,
               target_database_id text NOT NULL,
               phase text NOT NULL,
               marker_transaction_id bigint,
               error text,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE UNIQUE INDEX rust_database_failovers_one_active_idx
               ON rust_database_failovers ((true))
               WHERE phase NOT IN ('completed', 'rollback', 'failed');
             CREATE TEMP TABLE rust_database_failover_participants (
               failover_id uuid NOT NULL REFERENCES rust_database_failovers (id),
               machine_id uuid NOT NULL,
               status text NOT NULL,
               error text,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (failover_id, machine_id)
             );
             CREATE TEMP SEQUENCE transactions_id_seq;
             CREATE TEMP TABLE transactions (
               id bigint PRIMARY KEY DEFAULT nextval('transactions_id_seq'),
               app_id uuid NOT NULL
             );
             CREATE TEMP SEQUENCE triples_size_updates_id_seq;",
        )
        .await
        .unwrap();

    let leader = Uuid::new_v4();
    let follower = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO rust_realtime_machines (machine_id) VALUES ($1), ($2)",
            &[&leader, &follower],
        )
        .await
        .unwrap();
    let coordinator = PostgresDatabaseFailoverCoordinator::new(client);
    let (failover, expected) = coordinator
        .begin_for_active_machines(leader, "primary", "replica")
        .await
        .unwrap();
    assert_eq!(expected, 2);
    assert_eq!(
        coordinator.active().await.unwrap().unwrap(),
        failover.clone()
    );
    assert!(matches!(
        coordinator.begin(follower, "primary", "other").await,
        Err(DatabaseFailoverError::AlreadyActive)
    ));
    coordinator
        .acknowledge(
            failover.id,
            follower,
            DatabaseFailoverParticipantStatus::Ready,
            None,
        )
        .await
        .unwrap();
    coordinator
        .acknowledge(
            failover.id,
            leader,
            DatabaseFailoverParticipantStatus::Ready,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        coordinator.participants(failover.id).await.unwrap().len(),
        2
    );
    assert!(
        coordinator
            .participant_progress(failover.id, DatabaseFailoverParticipantStatus::Ready)
            .await
            .unwrap()
            .complete()
    );
    let paused = coordinator
        .transition(
            failover.id,
            DatabaseFailoverPhase::WaitForFollowers,
            DatabaseFailoverPhase::PauseWrites,
            Some(41),
            None,
        )
        .await
        .unwrap();
    assert_eq!(paused.marker_transaction_id, Some(41));
    assert!(matches!(
        coordinator
            .transition(
                failover.id,
                DatabaseFailoverPhase::WaitForFollowers,
                DatabaseFailoverPhase::PauseWrites,
                None,
                None,
            )
            .await,
        Err(DatabaseFailoverError::ConcurrentTransition)
    ));
    coordinator
        .transition(
            failover.id,
            DatabaseFailoverPhase::PauseWrites,
            DatabaseFailoverPhase::Rollback,
            None,
            Some("operator rollback"),
        )
        .await
        .unwrap();
    assert!(coordinator.active().await.unwrap().is_none());
    let cancellation = CancellationToken::new();
    let timings = DatabaseFailoverTimings {
        ready_timeout: std::time::Duration::from_secs(2),
        pause_timeout: std::time::Duration::from_secs(2),
        transaction_drain: std::time::Duration::from_millis(10),
        cancel_timeout: std::time::Duration::from_secs(2),
        catch_up_timeout: std::time::Duration::from_secs(2),
        resume_timeout: std::time::Duration::from_secs(2),
        poll_interval: std::time::Duration::from_millis(5),
    };
    let leader_run = run_database_failover(
        &coordinator,
        &coordinator,
        Uuid::new_v4(),
        "primary",
        "replica",
        Uuid::new_v4(),
        timings,
        &cancellation,
    );
    let first_pause = CancellationToken::new();
    let first_database = CancellationToken::new();
    let first = run_database_failover_participant(
        &coordinator,
        leader,
        &url,
        &first_pause,
        &first_database,
        &cancellation,
    );
    let second_pause = CancellationToken::new();
    let second_database = CancellationToken::new();
    let second = run_database_failover_participant(
        &coordinator,
        follower,
        &url,
        &second_pause,
        &second_database,
        &cancellation,
    );
    let (report, first, second) = tokio::join!(leader_run, first, second);
    let report = report.unwrap();
    assert_eq!(report.participants, 2);
    assert!(matches!(
        first.unwrap(),
        instant_server::DatabaseFailoverParticipantOutcome::Resume(_)
    ));
    assert!(matches!(
        second.unwrap(),
        instant_server::DatabaseFailoverParticipantOutcome::Resume(_)
    ));
    assert!(first_pause.is_cancelled());
    assert!(first_database.is_cancelled());

    let rollback_leader_cancellation = CancellationToken::new();
    let rollback_participant_cancellation = CancellationToken::new();
    let first_rollback_pause = CancellationToken::new();
    let first_rollback_database = CancellationToken::new();
    let rollback_leader = run_database_failover(
        &coordinator,
        &coordinator,
        leader,
        "replica",
        "primary",
        Uuid::new_v4(),
        DatabaseFailoverTimings {
            transaction_drain: std::time::Duration::from_secs(2),
            ..timings
        },
        &rollback_leader_cancellation,
    );
    let first_rollback = run_database_failover_participant(
        &coordinator,
        leader,
        &url,
        &first_rollback_pause,
        &first_rollback_database,
        &rollback_participant_cancellation,
    );
    let second_rollback_pause = CancellationToken::new();
    let second_rollback_database = CancellationToken::new();
    let second_rollback = run_database_failover_participant(
        &coordinator,
        follower,
        &url,
        &second_rollback_pause,
        &second_rollback_database,
        &rollback_participant_cancellation,
    );
    let trigger_rollback = async {
        let rollback = loop {
            if let Some(operation) = coordinator.active().await.unwrap() {
                break operation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        wait_for_participant_status(
            &coordinator,
            rollback.id,
            DatabaseFailoverParticipantStatus::PauseWrites,
        )
        .await;
        rollback_leader_cancellation.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let operation = coordinator.get(rollback.id).await.unwrap().unwrap();
                if operation.phase == DatabaseFailoverPhase::Rollback {
                    return operation;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("leader cancellation should durably roll back the failover")
    };
    let (rollback_result, first_rollback, second_rollback, terminal) = tokio::join!(
        rollback_leader,
        first_rollback,
        second_rollback,
        trigger_rollback
    );
    assert!(matches!(
        rollback_result,
        Err(instant_server::DatabaseFailoverLeaderError::Cancelled)
    ));
    assert_eq!(terminal.phase, DatabaseFailoverPhase::Rollback);
    assert!(matches!(
        first_rollback.unwrap(),
        instant_server::DatabaseFailoverParticipantOutcome::Rollback(operation)
            if operation.id == terminal.id
    ));
    assert!(matches!(
        second_rollback.unwrap(),
        instant_server::DatabaseFailoverParticipantOutcome::Rollback(operation)
            if operation.id == terminal.id
    ));
    wait_for_participant_status(
        &coordinator,
        terminal.id,
        DatabaseFailoverParticipantStatus::Rollback,
    )
    .await;
    assert!(first_rollback_pause.is_cancelled());
    assert!(!first_rollback_database.is_cancelled());
    assert!(second_rollback_pause.is_cancelled());
    assert!(!second_rollback_database.is_cancelled());
}
