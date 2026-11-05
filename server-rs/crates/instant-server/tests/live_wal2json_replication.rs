use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use instant_pg::{
    ByopTopic, ByopWalRecord, ReplicationCancellationToken, Wal2JsonReplicationConnection,
    create_logical_replication_slot, drop_logical_replication_slot, introspect_byop_schema,
};
use instant_realtime::{RealtimeStore, ReplicationPipelineOutcome, WalInvalidationPipeline};
use instant_server::{
    DurableInvalidationSink, ReconnectPolicy, run_invalidation_stream,
    supervise_byop_invalidation_stream,
};
use instant_wal::{Isn, LogicalReplicationOptions, Lsn, SlotPurpose, WalCacheEviction, WalRecord};
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Instant, sleep, timeout};
use tokio_postgres::{NoTls, types::Json};
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
struct ObservedWal {
    records: Vec<WalRecord>,
    byop_records: Vec<ByopWalRecord>,
    evictions: Vec<WalCacheEviction>,
    max_seen_isns: Vec<Isn>,
}

struct RecordingSink {
    observed: Arc<Mutex<ObservedWal>>,
    changed: Arc<Notify>,
}

impl DurableInvalidationSink for RecordingSink {
    type Error = Infallible;

    fn persist(
        &mut self,
        outcome: &ReplicationPipelineOutcome,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        if let Some(record) = &outcome.record {
            self.observed.lock().unwrap().records.push(record.clone());
            self.changed.notify_waiters();
        }
        std::future::ready(Ok(()))
    }

    fn persist_byop(
        &mut self,
        _app_id: Uuid,
        record: &ByopWalRecord,
        _topics: &[ByopTopic],
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.observed
            .lock()
            .unwrap()
            .byop_records
            .push(record.clone());
        self.changed.notify_waiters();
        std::future::ready(Ok(()))
    }

    fn evict_cache(
        &mut self,
        eviction: &WalCacheEviction,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.observed
            .lock()
            .unwrap()
            .evictions
            .push(eviction.clone());
        self.changed.notify_waiters();
        std::future::ready(Ok(()))
    }

    fn observe_max_seen_isn(
        &mut self,
        isn: Isn,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.observed.lock().unwrap().max_seen_isns.push(isn);
        self.changed.notify_waiters();
        std::future::ready(Ok(()))
    }
}

async fn wait_for_fixture(
    observed: &Arc<Mutex<ObservedWal>>,
    changed: &Notify,
    app_id: Uuid,
) -> Option<ObservedWal> {
    timeout(Duration::from_secs(15), async {
        loop {
            let notified = changed.notified();
            let snapshot = observed.lock().unwrap().clone();
            let has_record = snapshot
                .records
                .iter()
                .any(|record| record.app_id == app_id && record.triple_changes.len() == 1);
            let has_tx = snapshot.evictions.iter().any(|eviction| {
                matches!(
                    eviction,
                    WalCacheEviction::TransactionMaxSeen { tx_id: Some(_) }
                )
            });
            let has_filtered_rule = snapshot.evictions.iter().any(|eviction| {
                matches!(eviction, WalCacheEviction::Rules { app_id: Some(id) } if *id == app_id)
            });
            if has_record && has_tx && has_filtered_rule && snapshot.max_seen_isns.len() >= 2 {
                return snapshot;
            }
            notified.await;
        }
    })
    .await
    .ok()
}

async fn wait_for_confirmed_lsn(
    client: &tokio_postgres::Client,
    slot_name: &str,
    expected: Lsn,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let Ok(row) = client
            .query_opt(
                "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot_name],
            )
            .await
        else {
            return false;
        };
        if row
            .and_then(|row| row.get::<_, Option<String>>(0))
            .and_then(|lsn| lsn.parse::<Lsn>().ok())
            .is_some_and(|confirmed| confirmed >= expected)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_active_slot_pid(
    client: &tokio_postgres::Client,
    slot_name: &str,
    previous: Option<i32>,
) -> Option<i32> {
    timeout(Duration::from_secs(15), async {
        loop {
            if let Some(pid) = client
                .query_opt(
                    "SELECT active_pid FROM pg_replication_slots WHERE slot_name = $1",
                    &[&slot_name],
                )
                .await
                .ok()
                .flatten()
                .and_then(|row| row.get::<_, Option<i32>>(0))
                .filter(|pid| Some(*pid) != previous)
            {
                return pid;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .ok()
}

async fn wait_for_slot_inactive(client: &tokio_postgres::Client, slot_name: &str) -> bool {
    timeout(Duration::from_secs(5), async {
        loop {
            let inactive = client
                .query_opt(
                    "SELECT NOT active FROM pg_replication_slots WHERE slot_name = $1",
                    &[&slot_name],
                )
                .await
                .ok()
                .flatten()
                .is_some_and(|row| row.get::<_, bool>(0));
            if inactive {
                return;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .is_ok()
}

async fn wait_for_byop_marker(
    observed: &Arc<Mutex<ObservedWal>>,
    changed: &Notify,
    marker: &str,
) -> Option<Lsn> {
    timeout(Duration::from_secs(15), async {
        loop {
            let notified = changed.notified();
            let lsn = observed
                .lock()
                .unwrap()
                .byop_records
                .iter()
                .find_map(|record| {
                    record.changes.iter().find_map(|change| {
                        change
                            .columns
                            .iter()
                            .any(|column| column.name == "marker" && column.value == json!(marker))
                            .then_some(record.next_lsn)
                    })
                });
            if lsn.is_some() {
                return lsn;
            }
            notified.await;
        }
    })
    .await
    .ok()
    .flatten()
}

async fn wait_for_byop_record_count(
    observed: &Arc<Mutex<ObservedWal>>,
    changed: &Notify,
    expected: usize,
) -> bool {
    timeout(Duration::from_secs(15), async {
        loop {
            let notified = changed.notified();
            if observed.lock().unwrap().byop_records.len() >= expected {
                return;
            }
            notified.await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn real_wal2json_stream_delivers_cache_filtered_and_durable_commits() {
    let Some(database_url) = instant_test_support::test_database_url(
        "real_wal2json_stream_delivers_cache_filtered_and_durable_commits",
    ) else {
        return;
    };
    let (mut writer, writer_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = writer_connection.await;
    });

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let forward_ident = Uuid::new_v4();
    let slot_name = format!("rust_wal_fixture_{}", Uuid::new_v4().simple());

    writer
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("wal-{creator_id}@example.test")],
        )
        .await
        .unwrap();
    writer
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'wal fixture')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();
    writer
        .execute(
            "INSERT INTO attrs
             (id, app_id, value_type, cardinality, is_unique, is_indexed,
              forward_ident, etype, label)
             VALUES ($1, $2, 'blob', 'one', false, false, $3, 'goals', 'title')",
            &[&attr_id, &app_id, &forward_ident],
        )
        .await
        .unwrap();

    let created_slot = create_logical_replication_slot(&writer, &slot_name, false)
        .await
        .unwrap();
    let options = LogicalReplicationOptions::new(
        &slot_name,
        created_slot.consistent_point,
        SlotPurpose::Invalidator,
    )
    .unwrap();
    let replication_url = database_url.clone();
    let connection_result = tokio::task::spawn_blocking(move || {
        Wal2JsonReplicationConnection::connect(&replication_url, &options)
    })
    .await;
    let connection = match connection_result {
        Ok(Ok(connection)) => connection,
        failed => {
            let _ = drop_logical_replication_slot(&writer, &slot_name).await;
            let _ = writer
                .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
                .await;
            panic!("could not open live wal2json stream: {failed:?}");
        }
    };

    let observed = Arc::new(Mutex::new(ObservedWal::default()));
    let changed = Arc::new(Notify::new());
    let cancellation = ReplicationCancellationToken::new();
    let run_cancellation = cancellation.clone();
    let run_observed = Arc::clone(&observed);
    let run_changed = Arc::clone(&changed);
    let runner = tokio::spawn(async move {
        let mut connection = connection;
        let mut pipeline = WalInvalidationPipeline::new(2, None, created_slot.consistent_point);
        let mut store = RealtimeStore::default();
        let mut sink = RecordingSink {
            observed: run_observed,
            changed: run_changed,
        };
        let result = run_invalidation_stream(
            &mut connection,
            &mut pipeline,
            &mut store,
            &run_cancellation,
            Duration::from_millis(20),
            &mut sink,
        )
        .await;
        (result, pipeline.feedback_state())
    });

    let transaction = writer.transaction().await.unwrap();
    let tx_id: i64 = transaction
        .query_one(
            "INSERT INTO transactions (app_id) VALUES ($1) RETURNING id",
            &[&app_id],
        )
        .await
        .unwrap()
        .get(0);
    transaction
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae)
             VALUES ($1, $2, $3, $4, md5(($4::jsonb)::text), true, false, false, false, false)",
            &[&app_id, &entity_id, &attr_id, &Json(json!("native-wal"))],
        )
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    writer
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[
                &app_id,
                &Json(json!({"goals": {"allow": {"view": "true"}}})),
            ],
        )
        .await
        .unwrap();

    let snapshot = wait_for_fixture(&observed, &changed, app_id).await;
    let filtered_isn = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.max_seen_isns.iter().max().copied());
    let confirmed = if let Some(isn) = filtered_isn {
        wait_for_confirmed_lsn(&writer, &slot_name, isn.lsn()).await
    } else {
        false
    };
    cancellation.cancel();
    let runner_result = runner.await;

    // The replication connection must be gone before PostgreSQL permits slot cleanup.
    let slot_cleanup = drop_logical_replication_slot(&writer, &slot_name).await;
    let row_cleanup = writer
        .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
        .await;

    slot_cleanup.unwrap();
    row_cleanup.unwrap();
    let (run_result, feedback) = runner_result.unwrap();
    run_result.unwrap();
    let snapshot = snapshot.expect("timed out waiting for real wal2json transactions");
    let filtered_isn = filtered_isn.expect("fixture produced no committed ISN");
    assert!(
        confirmed,
        "standby feedback did not confirm the filtered commit"
    );
    assert!(feedback.flushed() >= filtered_isn.lsn());
    assert_eq!(snapshot.records.len(), 1);
    let record = &snapshot.records[0];
    assert_eq!(record.app_id, app_id);
    assert_eq!(record.tx_id, tx_id);
    assert_eq!(record.triple_changes.len(), 1);
    assert!(
        snapshot
            .evictions
            .contains(&WalCacheEviction::TransactionMaxSeen { tx_id: Some(tx_id) })
    );
    assert!(snapshot.evictions.contains(&WalCacheEviction::Rules {
        app_id: Some(app_id),
    }));
    assert!(snapshot.max_seen_isns.len() >= 2);
    assert!(
        snapshot
            .max_seen_isns
            .windows(2)
            .all(|isns| isns[0] < isns[1])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn byop_supervisor_reconnects_from_confirmed_checkpoint_after_backend_termination() {
    const RECORDS_PER_EPOCH: usize = 64;

    let Some(database_url) = instant_test_support::test_database_url(
        "byop_supervisor_reconnects_from_confirmed_checkpoint_after_backend_termination",
    ) else {
        return;
    };
    let (writer, writer_connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = writer_connection.await;
    });

    let app_id = Uuid::new_v4();
    let suffix = Uuid::new_v4().simple().to_string();
    let namespace = format!("rust_byop_reconnect_{suffix}");
    let slot_name = format!("rust_byop_reconnect_{suffix}");
    writer
        .batch_execute(&format!(
            "CREATE SCHEMA {namespace};
             CREATE TABLE {namespace}.events (
               id uuid PRIMARY KEY,
               marker text NOT NULL
             );"
        ))
        .await
        .unwrap();
    let schema = introspect_byop_schema(&writer, &namespace).await.unwrap();
    let created_slot = create_logical_replication_slot(&writer, &slot_name, false)
        .await
        .unwrap();

    let observed = Arc::new(Mutex::new(ObservedWal::default()));
    let changed = Arc::new(Notify::new());
    let cancellation = ReplicationCancellationToken::new();
    let run_cancellation = cancellation.clone();
    let run_observed = Arc::clone(&observed);
    let run_changed = Arc::clone(&changed);
    let run_url = database_url.clone();
    let run_slot = slot_name.clone();
    let run_namespace = namespace.clone();
    let runner = tokio::spawn(async move {
        let mut sink = RecordingSink {
            observed: run_observed,
            changed: run_changed,
        };
        supervise_byop_invalidation_stream(
            &run_url,
            &run_slot,
            app_id,
            &run_namespace,
            schema,
            created_slot.consistent_point,
            &run_cancellation,
            Duration::from_millis(20),
            ReconnectPolicy::new(Duration::from_secs(2), Duration::from_secs(2)),
            &mut sink,
        )
        .await
    });

    let first_pid = wait_for_active_slot_pid(&writer, &slot_name, None)
        .await
        .expect("BYOP supervisor did not acquire the logical slot");
    for index in 0..RECORDS_PER_EPOCH {
        writer
            .execute(
                &format!("INSERT INTO {namespace}.events (id, marker) VALUES ($1, $2)"),
                &[&Uuid::new_v4(), &format!("before-{index}")],
            )
            .await
            .unwrap();
    }
    let first_lsn = wait_for_byop_marker(
        &observed,
        &changed,
        &format!("before-{}", RECORDS_PER_EPOCH - 1),
    )
    .await
    .expect("first BYOP epoch was not delivered");
    assert!(wait_for_byop_record_count(&observed, &changed, RECORDS_PER_EPOCH).await);
    assert!(wait_for_confirmed_lsn(&writer, &slot_name, first_lsn).await);

    assert!(
        writer
            .query_one("SELECT pg_terminate_backend($1)", &[&first_pid])
            .await
            .unwrap()
            .get::<_, bool>(0),
        "PostgreSQL did not terminate the active BYOP replication backend"
    );
    assert!(wait_for_slot_inactive(&writer, &slot_name).await);
    for index in 0..RECORDS_PER_EPOCH {
        writer
            .execute(
                &format!("INSERT INTO {namespace}.events (id, marker) VALUES ($1, $2)"),
                &[&Uuid::new_v4(), &format!("after-{index}")],
            )
            .await
            .unwrap();
    }
    assert!(
        writer
            .query_one(
                "SELECT NOT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot_name],
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        "the entire second epoch must commit while the slot is disconnected"
    );
    let replacement_pid = wait_for_active_slot_pid(&writer, &slot_name, Some(first_pid))
        .await
        .expect("BYOP supervisor did not replace the terminated replication connection");
    assert_ne!(replacement_pid, first_pid);
    let second_lsn = wait_for_byop_marker(
        &observed,
        &changed,
        &format!("after-{}", RECORDS_PER_EPOCH - 1),
    )
    .await
    .expect("BYOP epoch after reconnect was not delivered");
    assert!(
        wait_for_byop_record_count(&observed, &changed, RECORDS_PER_EPOCH * 2).await,
        "all records after the reconnect must be delivered"
    );
    assert!(second_lsn > first_lsn);
    assert!(wait_for_confirmed_lsn(&writer, &slot_name, second_lsn).await);

    cancellation.cancel();
    runner.await.unwrap().unwrap();
    drop_logical_replication_slot(&writer, &slot_name)
        .await
        .unwrap();
    writer
        .batch_execute(&format!("DROP SCHEMA {namespace} CASCADE"))
        .await
        .unwrap();

    let observed = observed.lock().unwrap();
    assert_eq!(observed.byop_records.len(), RECORDS_PER_EPOCH * 2);
    let markers = observed
        .byop_records
        .iter()
        .flat_map(|record| &record.changes)
        .flat_map(|change| &change.columns)
        .filter(|column| column.name == "marker")
        .filter_map(|column| column.value.as_str())
        .collect::<Vec<_>>();
    let expected_markers = ["before", "after"]
        .into_iter()
        .flat_map(|epoch| (0..RECORDS_PER_EPOCH).map(move |index| format!("{epoch}-{index}")))
        .collect::<Vec<_>>();
    assert_eq!(
        markers,
        expected_markers
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert!(
        observed
            .byop_records
            .windows(2)
            .all(|records| records[0].next_lsn < records[1].next_lsn)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn byop_supervisor_fails_closed_when_its_durable_slot_disappears() {
    let Some(database_url) = instant_test_support::test_database_url(
        "byop_supervisor_fails_closed_when_its_durable_slot_disappears",
    ) else {
        return;
    };
    let (writer, writer_connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = writer_connection.await;
    });

    let app_id = Uuid::new_v4();
    let suffix = Uuid::new_v4().simple().to_string();
    let namespace = format!("rust_byop_slot_loss_{suffix}");
    let slot_name = format!("rust_byop_slot_loss_{suffix}");
    writer
        .batch_execute(&format!(
            "CREATE SCHEMA {namespace};
             CREATE TABLE {namespace}.events (
               id uuid PRIMARY KEY,
               marker text NOT NULL
             );"
        ))
        .await
        .unwrap();
    let schema = introspect_byop_schema(&writer, &namespace).await.unwrap();
    let created_slot = create_logical_replication_slot(&writer, &slot_name, false)
        .await
        .unwrap();

    let observed = Arc::new(Mutex::new(ObservedWal::default()));
    let changed = Arc::new(Notify::new());
    let cancellation = ReplicationCancellationToken::new();
    let run_cancellation = cancellation.clone();
    let run_observed = Arc::clone(&observed);
    let run_changed = Arc::clone(&changed);
    let run_url = database_url.clone();
    let run_slot = slot_name.clone();
    let run_namespace = namespace.clone();
    let runner = tokio::spawn(async move {
        let mut sink = RecordingSink {
            observed: run_observed,
            changed: run_changed,
        };
        supervise_byop_invalidation_stream(
            &run_url,
            &run_slot,
            app_id,
            &run_namespace,
            schema,
            created_slot.consistent_point,
            &run_cancellation,
            Duration::from_millis(20),
            ReconnectPolicy::new(Duration::from_secs(2), Duration::from_secs(2)),
            &mut sink,
        )
        .await
    });

    let active_pid = wait_for_active_slot_pid(&writer, &slot_name, None)
        .await
        .expect("BYOP supervisor did not acquire the logical slot");
    writer
        .execute(
            &format!("INSERT INTO {namespace}.events (id, marker) VALUES ($1, 'durable')"),
            &[&Uuid::new_v4()],
        )
        .await
        .unwrap();
    let durable_lsn = wait_for_byop_marker(&observed, &changed, "durable")
        .await
        .expect("BYOP commit before slot loss was not delivered");
    assert!(wait_for_confirmed_lsn(&writer, &slot_name, durable_lsn).await);

    assert!(
        writer
            .query_one("SELECT pg_terminate_backend($1)", &[&active_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert!(
        wait_for_slot_inactive(&writer, &slot_name).await,
        "terminated BYOP slot did not become inactive"
    );
    drop_logical_replication_slot(&writer, &slot_name)
        .await
        .unwrap();

    let result = timeout(Duration::from_secs(8), runner)
        .await
        .expect("BYOP supervisor retried a missing durable slot indefinitely")
        .unwrap()
        .expect_err("BYOP supervisor recreated or ignored a missing durable slot");
    let error = result.to_string();
    assert!(
        error.contains("does not exist"),
        "unexpected slot-loss error: {error}"
    );
    assert!(
        error.contains("42704"),
        "slot-loss SQLSTATE was not retained: {error}"
    );

    writer
        .batch_execute(&format!("DROP SCHEMA {namespace} CASCADE"))
        .await
        .unwrap();
    assert_eq!(observed.lock().unwrap().byop_records.len(), 1);
}
