use std::time::Duration;

use instant_pg::{
    ReplicationCancellationToken, create_logical_replication_slot, ensure_logical_replication_slot,
    recreate_invalidated_slot, replication_slot_is_lost,
};
use instant_realtime::RealtimeStore;
use instant_server::{
    NoopInvalidationSink, PostgresPool, PostgresReplicationSlotCleaner, ReconnectPolicy,
    drop_process_invalidator_slot, supervise_invalidation_stream,
};
use instant_wal::SlotPurpose;
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

/// These tests share the cluster-wide `max_slot_wal_keep_size` and the public
/// slot namespace, so they must not run concurrently in one test binary.
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn connect_client(database_url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn create_slot(client: &Client, slot_name: &str) {
    create_logical_replication_slot(client, slot_name, false)
        .await
        .unwrap();
}

async fn wait_for_slot_active(client: &Client, slot_name: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let active = client
                .query_opt(
                    "SELECT active FROM pg_replication_slots WHERE slot_name = $1",
                    &[&slot_name],
                )
                .await
                .unwrap()
                .is_some_and(|row| row.get::<_, bool>(0));
            if active {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("replication consumer never became active");
}

async fn slot_names(client: &Client, prefix: &str) -> Vec<String> {
    client
        .query(
            "SELECT slot_name::text FROM pg_replication_slots
             WHERE slot_name LIKE $1 ORDER BY slot_name",
            &[&format!("{prefix}_%")],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get(0))
        .collect()
}

#[tokio::test]
async fn startup_cleanup_drops_slots_whose_machine_heartbeat_is_stale() {
    let _serial = TEST_LOCK.lock().await;
    let Some(database_url) = instant_test_support::test_database_url(
        "startup_cleanup_drops_slots_whose_machine_heartbeat_is_stale",
    ) else {
        return;
    };
    let setup = connect_client(&database_url).await;
    let stale_machine = Uuid::new_v4();
    let live_machine = Uuid::new_v4();
    let orphan_machine = Uuid::new_v4();
    let stale_slot = format!("invalidator_{}", stale_machine.simple());
    let live_slot = format!("invalidator_{}", live_machine.simple());
    let orphan_slot = format!("invalidator_{}", orphan_machine.simple());
    setup
        .execute(
            "INSERT INTO rust_realtime_machines (machine_id, updated_at)
             VALUES ($1, now() - interval '2 minutes')",
            &[&stale_machine],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO rust_realtime_machines (machine_id, updated_at) VALUES ($1, now())",
            &[&live_machine],
        )
        .await
        .unwrap();
    create_slot(&setup, &stale_slot).await;
    create_slot(&setup, &live_slot).await;
    create_slot(&setup, &orphan_slot).await;

    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let outcome = PostgresReplicationSlotCleaner::from_pool(pool)
        .run_stale_machine_cleanup()
        .await
        .unwrap();

    assert!(outcome.cleaned.contains(&stale_slot), "{outcome:?}");
    assert!(outcome.cleaned.contains(&orphan_slot), "{outcome:?}");
    assert!(!outcome.cleaned.contains(&live_slot), "{outcome:?}");
    assert_eq!(
        slot_names(&setup, "invalidator").await,
        vec![live_slot.clone()]
    );

    setup
        .execute("SELECT pg_drop_replication_slot($1)", &[&live_slot])
        .await
        .unwrap();
    setup
        .execute(
            "DELETE FROM rust_realtime_machines WHERE machine_id = ANY($1)",
            &[&vec![stale_machine, live_machine]],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn ten_quick_restarts_leave_at_most_one_invalidator_slot() {
    let _serial = TEST_LOCK.lock().await;
    let Some(database_url) = instant_test_support::test_database_url(
        "ten_quick_restarts_leave_at_most_one_invalidator_slot",
    ) else {
        return;
    };
    let setup = connect_client(&database_url).await;
    let slot_name = format!("invalidator_{}", Uuid::new_v4().simple());
    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();

    for _ in 0..10 {
        let confirmed_lsn = {
            let client = pool.acquire().await.unwrap();
            ensure_logical_replication_slot(&**client, &slot_name)
                .await
                .unwrap()
                .confirmed_flush_lsn
        };
        let cancellation = ReplicationCancellationToken::new();
        let stream_cancellation = cancellation.clone();
        let stream_url = database_url.clone();
        let stream_slot = slot_name.clone();
        let stream = tokio::spawn(async move {
            let mut store = RealtimeStore::default();
            let mut sink = NoopInvalidationSink;
            supervise_invalidation_stream(
                &stream_url,
                &stream_slot,
                2,
                SlotPurpose::Invalidator,
                None,
                confirmed_lsn,
                &mut store,
                &stream_cancellation,
                Duration::from_secs(10),
                ReconnectPolicy::default(),
                None,
                &mut sink,
            )
            .await
        });
        wait_for_slot_active(&setup, &slot_name).await;
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(15), stream)
            .await
            .expect("replication consumer did not stop after cancellation")
            .unwrap()
            .unwrap();
        drop_process_invalidator_slot(&pool, &slot_name).await;
    }

    let remaining = slot_names(&setup, "invalidator").await;
    assert!(
        remaining.len() <= 1,
        "each of ten graceful restarts must drop its slot, found {remaining:?}"
    );
    for slot in remaining {
        setup
            .execute("SELECT pg_drop_replication_slot($1)", &[&slot])
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn invalidated_slot_is_detected_and_recreated() {
    let _serial = TEST_LOCK.lock().await;
    let Some(database_url) =
        instant_test_support::test_database_url("invalidated_slot_is_detected_and_recreated")
    else {
        return;
    };
    let setup = connect_client(&database_url).await;
    let superuser = setup
        .query_one(
            "SELECT rolsuper FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .unwrap()
        .get::<_, bool>(0);
    if !superuser {
        eprintln!("skipping slot invalidation: the test role cannot ALTER SYSTEM");
        return;
    }
    let suffix = Uuid::new_v4().simple().to_string();
    let slot_name = format!("invalidator_{suffix}");
    let fixture = format!("slot_loss_{suffix}");
    setup
        .execute("ALTER SYSTEM RESET max_slot_wal_keep_size", &[])
        .await
        .unwrap();
    setup.execute("SELECT pg_reload_conf()", &[]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    create_slot(&setup, &slot_name).await;

    setup
        .execute("ALTER SYSTEM SET max_slot_wal_keep_size = '64kB'", &[])
        .await
        .unwrap();
    setup.execute("SELECT pg_reload_conf()", &[]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    setup
        .batch_execute(&format!(
            "CREATE TABLE {fixture} AS
             SELECT g AS id, repeat('x', 1024) AS payload FROM generate_series(1, 20000) g;
             SELECT pg_switch_wal();"
        ))
        .await
        .unwrap();
    setup.execute("CHECKPOINT", &[]).await.unwrap();

    let lost = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if replication_slot_is_lost(&setup, &slot_name).await.unwrap() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    setup
        .execute("ALTER SYSTEM RESET max_slot_wal_keep_size", &[])
        .await
        .unwrap();
    setup.execute("SELECT pg_reload_conf()", &[]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    lost.expect("the slot was not invalidated under a bounded WAL retention cap");
    assert!(replication_slot_is_lost(&setup, &slot_name).await.unwrap());

    let created = recreate_invalidated_slot(&setup, &slot_name).await.unwrap();
    assert_eq!(created.name, slot_name);
    assert!(
        !replication_slot_is_lost(&setup, &slot_name).await.unwrap(),
        "a recreated slot must not stay invalidated"
    );

    setup
        .execute("SELECT pg_drop_replication_slot($1)", &[&slot_name])
        .await
        .unwrap();
    setup
        .batch_execute(&format!("DROP TABLE {fixture}"))
        .await
        .unwrap();
}

#[tokio::test]
async fn cleaner_reconnects_between_snapshot_and_rechecked_cleanup() {
    let _serial = TEST_LOCK.lock().await;
    let Some(database_url) = instant_test_support::test_database_url(
        "cleaner_reconnects_between_snapshot_and_rechecked_cleanup",
    ) else {
        return;
    };
    let setup = connect_client(&database_url).await;
    let suffix = Uuid::new_v4().simple().to_string();
    let abandoned = format!("invalidator_{suffix}");
    let protected = format!("protected_{suffix}");
    create_slot(&setup, &abandoned).await;
    create_slot(&setup, &protected).await;

    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    let first = pool.acquire().await.unwrap();
    let first_pid = first
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    drop(first);
    let cleaner =
        PostgresReplicationSlotCleaner::from_pool(pool.clone()).with_grace(Duration::from_secs(2));
    let cleanup = tokio::spawn(async move { cleaner.run_once().await });

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot_finished = setup
                .query_opt(
                    "SELECT state = 'idle' AND query LIKE '%slot_name LIKE%'
                     FROM pg_stat_activity WHERE pid = $1",
                    &[&first_pid],
                )
                .await
                .unwrap()
                .is_some_and(|row| row.get::<_, bool>(0));
            if snapshot_finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("slot snapshot phase did not finish before the bounded deadline");
    assert!(
        setup
            .query_one("SELECT pg_terminate_backend($1)", &[&first_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );

    let outcome = tokio::time::timeout(Duration::from_secs(10), cleanup)
        .await
        .expect("slot cleanup did not reconnect within the bounded deadline")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.candidates, vec![abandoned.clone()]);
    assert_eq!(outcome.cleaned, vec![abandoned.clone()]);
    assert!(outcome.retained.is_empty());

    let remaining = setup
        .query(
            "SELECT slot_name::text FROM pg_replication_slots
             WHERE slot_name = ANY($1::text[]) ORDER BY slot_name",
            &[&vec![abandoned.clone(), protected.clone()]],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>();
    assert_eq!(remaining, vec![protected.clone()]);

    let replacement = pool.acquire().await.unwrap();
    let replacement_pid = replacement
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    assert_ne!(replacement_pid, first_pid);
    drop(replacement);
    let second = PostgresReplicationSlotCleaner::from_pool(pool.clone())
        .with_grace(Duration::from_millis(1))
        .run_once()
        .await
        .unwrap();
    assert!(second.candidates.is_empty());
    assert!(second.cleaned.is_empty());
    assert!(second.retained.is_empty());

    setup
        .execute("SELECT pg_drop_replication_slot($1)", &[&protected])
        .await
        .unwrap();
}
