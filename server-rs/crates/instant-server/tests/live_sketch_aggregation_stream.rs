use std::time::Duration;

use instant_pg::{
    ReplicationCancellationToken, Wal2JsonReplicationConnection, create_logical_replication_slot,
    drop_logical_replication_slot, initialize_sketch_aggregator_status,
    sketch_aggregator_checkpoint,
};
use instant_server::run_sketch_aggregation_stream;
use instant_wal::{ByopWal2JsonReplicationDecoder, LogicalReplicationOptions, Lsn, SlotPurpose};
use serde_json::json;
use tokio::time::{Instant, sleep, timeout};
use tokio_postgres::{NoTls, types::Json};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // The regression requires a real slot, feedback, reconnect, and commit.
async fn filtered_writes_preserve_durable_feedback_and_restart() {
    let Some(database_url) = instant_test_support::test_database_url(
        "filtered_writes_preserve_durable_feedback_and_restart",
    ) else {
        return;
    };
    let (writer, writer_connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = writer_connection.await;
    });

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let forward_ident = Uuid::new_v4();
    let slot_name = format!("sketch_loop_{}", Uuid::new_v4().simple());
    writer
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[
                &creator_id,
                &format!("sketch-loop-{creator_id}@example.test"),
            ],
        )
        .await
        .unwrap();
    writer
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'sketch loop fixture')",
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

    let slot = create_logical_replication_slot(&writer, &slot_name, false)
        .await
        .unwrap();
    initialize_sketch_aggregator_status(
        &writer,
        &slot_name,
        "live-sketch-loop",
        slot.consistent_point,
    )
    .await
    .unwrap();
    let durable_checkpoint = slot.consistent_point;
    let status_before = status_row(&writer, &slot_name).await;

    let first_cancellation = ReplicationCancellationToken::new();
    let first_runner = start_stream(
        &database_url,
        &slot_name,
        durable_checkpoint,
        first_cancellation.clone(),
    )
    .await;
    writer
        .execute("UPDATE apps SET title = title WHERE id = $1", &[&app_id])
        .await
        .unwrap();
    sleep(Duration::from_secs(1)).await;
    let confirmed: Lsn = writer
        .query_one(
            "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot_name],
        )
        .await
        .unwrap()
        .get::<_, String>(0)
        .parse()
        .unwrap();
    assert!(
        confirmed <= durable_checkpoint,
        "filtered feedback must remain recoverable"
    );
    assert_eq!(status_row(&writer, &slot_name).await, status_before);

    // Exceed the filtered WAL budget in a single committed, non-triples batch.
    writer.batch_execute(
        "CREATE TABLE sketch_filtered_wal_fixture (payload text);          INSERT INTO sketch_filtered_wal_fixture          SELECT repeat(md5(g::text), 32) FROM generate_series(1, 20000) g;",
    ).await.unwrap();
    let durable_checkpoint = wait_for_sketch_checkpoint(&writer, &slot_name, durable_checkpoint)
        .await
        .expect("large filtered batch must persist progress");
    assert!(wait_for_confirmed_lsn(&writer, &slot_name, durable_checkpoint).await);
    let settled_status = status_row(&writer, &slot_name).await;
    sleep(Duration::from_secs(1)).await;
    assert_eq!(
        status_row(&writer, &slot_name).await,
        settled_status,
        "checkpoint writes must not sustain a feedback loop"
    );

    first_cancellation.cancel();
    timeout(Duration::from_secs(5), first_runner)
        .await
        .expect("first stream must stop")
        .unwrap()
        .unwrap();
    assert!(wait_for_slot_inactive(&writer, &slot_name).await);

    let restart_slot = instant_pg::get_logical_replication_slot(&writer, &slot_name)
        .await
        .unwrap()
        .unwrap();
    instant_pg::validate_replication_start(
        &restart_slot,
        durable_checkpoint,
        SlotPurpose::Aggregator,
    )
    .expect("production startup guard must accept filtered progress");

    let second_cancellation = ReplicationCancellationToken::new();
    let second_runner = start_stream(
        &database_url,
        &slot_name,
        durable_checkpoint,
        second_cancellation.clone(),
    )
    .await;
    writer
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae)
             VALUES ($1, $2, $3, $4, md5(($4::jsonb)::text), true, false, false, false, false)",
            &[&app_id, &entity_id, &attr_id, &Json(json!("after-replay"))],
        )
        .await
        .unwrap();
    let checkpoint = wait_for_sketch_checkpoint(&writer, &slot_name, durable_checkpoint)
        .await
        .expect("triple batch must persist a checkpoint");
    assert!(wait_for_confirmed_lsn(&writer, &slot_name, checkpoint).await);
    let sketch = writer
        .query_one(
            "SELECT total, max_lsn::text FROM attr_sketches WHERE app_id = $1 AND attr_id = $2",
            &[&app_id, &attr_id],
        )
        .await
        .unwrap();
    assert_eq!(
        sketch.get::<_, i64>("total"),
        1,
        "replay must not double count"
    );
    assert_eq!(sketch.get::<_, String>("max_lsn"), checkpoint.to_string());

    second_cancellation.cancel();
    timeout(Duration::from_secs(5), second_runner)
        .await
        .expect("second stream must stop")
        .unwrap()
        .unwrap();
    assert!(wait_for_slot_inactive(&writer, &slot_name).await);
    writer
        .batch_execute("DROP TABLE sketch_filtered_wal_fixture")
        .await
        .unwrap();
    drop_logical_replication_slot(&writer, &slot_name)
        .await
        .unwrap();
    writer
        .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
        .await
        .unwrap();
}

async fn start_stream(
    database_url: &str,
    slot_name: &str,
    checkpoint: Lsn,
    cancellation: ReplicationCancellationToken,
) -> tokio::task::JoinHandle<Result<(), instant_server::SketchAggregatorRuntimeError>> {
    let options =
        LogicalReplicationOptions::new(slot_name, checkpoint, SlotPurpose::Aggregator).unwrap();
    let replication_url = database_url.to_owned();
    let connection = tokio::task::spawn_blocking(move || {
        Wal2JsonReplicationConnection::connect(&replication_url, &options)
    })
    .await
    .unwrap()
    .unwrap();
    let (mut database, database_connection) =
        tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = database_connection.await;
    });
    let slot_name = slot_name.to_owned();
    tokio::spawn(async move {
        let mut connection = connection;
        let mut decoder = ByopWal2JsonReplicationDecoder::new(checkpoint);
        let mut stream_checkpoint = checkpoint;
        run_sketch_aggregation_stream(
            &mut connection,
            &mut decoder,
            &mut database,
            &slot_name,
            "live-sketch-loop",
            &mut stream_checkpoint,
            &cancellation,
            Duration::from_millis(20),
        )
        .await
    })
}

async fn status_row(client: &tokio_postgres::Client, slot_name: &str) -> (String, String) {
    let row = client
        .query_one(
            "SELECT lsn::text, xmin::text FROM wal_aggregator_status WHERE slot_name = $1",
            &[&slot_name],
        )
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn wait_for_confirmed_lsn(
    client: &tokio_postgres::Client,
    slot_name: &str,
    expected: Lsn,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let confirmed = client
            .query_one(
                "SELECT confirmed_flush_lsn::text FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot_name],
            )
            .await
            .unwrap()
            .get::<_, String>(0)
            .parse::<Lsn>()
            .unwrap();
        if confirmed >= expected {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_sketch_checkpoint(
    client: &tokio_postgres::Client,
    slot_name: &str,
    previous: Lsn,
) -> Option<Lsn> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(checkpoint) = sketch_aggregator_checkpoint(client, slot_name)
            .await
            .unwrap()
            && checkpoint > previous
        {
            return Some(checkpoint);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_slot_inactive(client: &tokio_postgres::Client, slot_name: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if client
            .query_one(
                "SELECT NOT active FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot_name],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(25)).await;
    }
}
