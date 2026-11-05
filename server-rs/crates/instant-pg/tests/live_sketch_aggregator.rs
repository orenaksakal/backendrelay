use std::collections::HashMap;

use instant_pg::{
    AttrSketchDeltas, SketchDatum, SketchDeltaBatch, SketchKey, SketchPersistenceError,
    initialize_sketch_aggregator_status, persist_sketch_delta_batch, persist_sketch_delta_batches,
    sketch_aggregator_checkpoint,
};
use instant_wal::Lsn;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)] // One disposable schema verifies commit and rollback together.
async fn sketch_rows_and_checkpoint_commit_atomically() {
    let Some(database_url) =
        instant_test_support::test_database_url("sketch_rows_and_checkpoint_commit_atomically")
    else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TEMP TABLE apps (id uuid PRIMARY KEY);
             CREATE TEMP TABLE attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL);
             CREATE TEMP TABLE attr_sketches (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               width integer NOT NULL,
               depth integer NOT NULL,
               total bigint NOT NULL,
               total_not_binned bigint NOT NULL,
               bins bytea NOT NULL,
               max_lsn pg_lsn,
               reverse_width integer,
               reverse_depth integer,
               reverse_total bigint,
               reverse_bins bytea,
               UNIQUE (app_id, attr_id)
             );
             CREATE TEMP TABLE wal_aggregator_status (
               slot_name text PRIMARY KEY,
               lsn pg_lsn,
               process_id text
             );",
        )
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    client
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO attrs (id, app_id) VALUES ($1, $2)",
            &[&attr_id, &app_id],
        )
        .await
        .unwrap();
    initialize_sketch_aggregator_status(&client, "aggregator_test", "worker-a", Lsn::from_u64(10))
        .await
        .unwrap();

    let key = SketchKey { app_id, attr_id };
    let mut deltas = AttrSketchDeltas {
        max_lsn: Lsn::from_u64(20),
        ..AttrSketchDeltas::default()
    };
    deltas
        .records
        .insert(SketchDatum::String("native".to_owned()), 1);
    deltas
        .reverse_records
        .insert(SketchDatum::String(Uuid::new_v4().to_string()), 1);
    let batch = SketchDeltaBatch {
        changes: HashMap::from([(key, deltas)]),
        lsn: Lsn::from_u64(20),
        next_lsn: Lsn::from_u64(21),
        tx_bytes: 100,
    };
    persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(10),
        &batch,
    )
    .await
    .unwrap();
    assert_eq!(
        sketch_aggregator_checkpoint(&client, "aggregator_test")
            .await
            .unwrap(),
        Some(Lsn::from_u64(20))
    );
    let row = client
        .query_one(
            "SELECT total, total_not_binned, reverse_total, max_lsn::text
               FROM attr_sketches WHERE app_id = $1 AND attr_id = $2",
            &[&app_id, &attr_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>("total"), 1);
    assert_eq!(row.get::<_, i64>("total_not_binned"), 0);
    assert_eq!(row.get::<_, i64>("reverse_total"), 1);
    assert_eq!(
        row.get::<_, String>("max_lsn"),
        Lsn::from_u64(20).to_string()
    );

    let error = persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-b",
        Lsn::from_u64(10),
        &batch,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        SketchPersistenceError::CheckpointConflict { .. }
    ));
    let total: i64 = client
        .query_one("SELECT total FROM attr_sketches", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(total, 1, "checkpoint conflict must roll back sketch writes");

    let mut plus = batch.clone();
    plus.lsn = Lsn::from_u64(30);
    plus.next_lsn = Lsn::from_u64(31);
    plus.changes.get_mut(&key).unwrap().max_lsn = plus.lsn;
    let mut minus = plus.clone();
    minus.lsn = Lsn::from_u64(40);
    minus.next_lsn = Lsn::from_u64(41);
    let delta = minus.changes.get_mut(&key).unwrap();
    delta.max_lsn = minus.lsn;
    for count in delta
        .records
        .values_mut()
        .chain(delta.reverse_records.values_mut())
    {
        *count = -1;
    }
    persist_sketch_delta_batches(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(20),
        &[plus.clone(), minus.clone()],
    )
    .await
    .unwrap();
    let row = client
        .query_one(
            "SELECT total, reverse_total, max_lsn::text FROM attr_sketches",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, i64>(1), 1);
    assert_eq!(row.get::<_, String>(2), Lsn::from_u64(40).to_string());
    assert_eq!(
        sketch_aggregator_checkpoint(&client, "aggregator_test")
            .await
            .unwrap(),
        Some(Lsn::from_u64(40))
    );

    // Netting these two commits would hide a bad intermediate count. Ordered
    // application must reject it and preserve the entire durable boundary.
    let mut bad = minus.clone();
    bad.lsn = Lsn::from_u64(50);
    bad.next_lsn = Lsn::from_u64(51);
    let delta = bad.changes.get_mut(&key).unwrap();
    delta.max_lsn = bad.lsn;
    for count in delta
        .records
        .values_mut()
        .chain(delta.reverse_records.values_mut())
    {
        *count = -2;
    }
    let mut restore = plus.clone();
    restore.lsn = Lsn::from_u64(60);
    restore.next_lsn = Lsn::from_u64(61);
    let delta = restore.changes.get_mut(&key).unwrap();
    delta.max_lsn = restore.lsn;
    for count in delta
        .records
        .values_mut()
        .chain(delta.reverse_records.values_mut())
    {
        *count = 2;
    }
    let error = persist_sketch_delta_batches(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(40),
        &[bad.clone(), restore.clone()],
    )
    .await
    .unwrap_err();
    assert!(matches!(error, SketchPersistenceError::Mutation(_)));
    assert_eq!(
        sketch_aggregator_checkpoint(&client, "aggregator_test")
            .await
            .unwrap(),
        Some(Lsn::from_u64(40))
    );
    assert_eq!(
        client
            .query_one("SELECT total FROM attr_sketches", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    let error = persist_sketch_delta_batches(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(40),
        &[restore, bad],
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        SketchPersistenceError::InvalidBatchSequence
    ));

    // An alternate valid zstd representation makes an accidental decode/encode
    // round trip observable, even when its decoded sketch is exactly identical.
    fn alternate_blob(original: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let raw = zstd::bulk::decompress(original, 8 * 20_000 * 10).unwrap();
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(&raw).unwrap();
        let encoded = encoder.finish().unwrap();
        assert_ne!(encoded, original);
        encoded
    }
    let row = client
        .query_one("SELECT bins, reverse_bins FROM attr_sketches", &[])
        .await
        .unwrap();
    let old_forward = alternate_blob(&row.get::<_, Vec<u8>>(0));
    let old_reverse = alternate_blob(&row.get::<_, Vec<u8>>(1));
    client
        .execute(
            "UPDATE attr_sketches SET bins=$1, reverse_bins=$2",
            &[&old_forward, &old_reverse],
        )
        .await
        .unwrap();

    let mut forward_only = plus.clone();
    forward_only.lsn = Lsn::from_u64(70);
    forward_only.next_lsn = Lsn::from_u64(71);
    let delta = forward_only.changes.get_mut(&key).unwrap();
    delta.max_lsn = forward_only.lsn;
    delta.reverse_records.clear();
    persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(40),
        &forward_only,
    )
    .await
    .unwrap();
    let row = client
        .query_one(
            "SELECT total, reverse_total, bins, reverse_bins, max_lsn::text FROM attr_sketches",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 2);
    assert_eq!(row.get::<_, i64>(1), 1);
    assert_eq!(
        row.get::<_, Vec<u8>>(3),
        old_reverse,
        "untouched reverse representation must remain byte-for-byte intact"
    );
    assert_eq!(row.get::<_, String>(4), Lsn::from_u64(70).to_string());
    let preserved_forward = alternate_blob(&row.get::<_, Vec<u8>>(2));
    client
        .execute("UPDATE attr_sketches SET bins=$1", &[&preserved_forward])
        .await
        .unwrap();

    let mut reverse_only = plus.clone();
    reverse_only.lsn = Lsn::from_u64(80);
    reverse_only.next_lsn = Lsn::from_u64(81);
    let delta = reverse_only.changes.get_mut(&key).unwrap();
    delta.max_lsn = reverse_only.lsn;
    delta.records.clear();
    persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(70),
        &reverse_only,
    )
    .await
    .unwrap();
    let row = client
        .query_one(
            "SELECT total, reverse_total, bins, reverse_bins FROM attr_sketches",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 2);
    assert_eq!(row.get::<_, i64>(1), 2);
    assert_eq!(
        row.get::<_, Vec<u8>>(2),
        preserved_forward,
        "untouched forward representation must remain byte-for-byte intact"
    );
    let preserved_reverse = alternate_blob(&row.get::<_, Vec<u8>>(3));
    client
        .execute(
            "UPDATE attr_sketches SET reverse_bins=$1",
            &[&preserved_reverse],
        )
        .await
        .unwrap();

    let noop = SketchDeltaBatch {
        changes: HashMap::from([(
            key,
            AttrSketchDeltas {
                max_lsn: Lsn::from_u64(90),
                ..AttrSketchDeltas::default()
            },
        )]),
        lsn: Lsn::from_u64(90),
        next_lsn: Lsn::from_u64(91),
        tx_bytes: 1,
    };
    persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(80),
        &noop,
    )
    .await
    .unwrap();
    let row = client
        .query_one(
            "SELECT bins, reverse_bins, max_lsn::text FROM attr_sketches",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, Vec<u8>>(0), preserved_forward);
    assert_eq!(row.get::<_, Vec<u8>>(1), preserved_reverse);
    assert_eq!(row.get::<_, String>(2), Lsn::from_u64(90).to_string());
    assert_eq!(
        sketch_aggregator_checkpoint(&client, "aggregator_test")
            .await
            .unwrap(),
        Some(Lsn::from_u64(90))
    );

    let error = persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-b",
        Lsn::from_u64(80),
        &noop,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        SketchPersistenceError::CheckpointConflict { .. }
    ));
    assert_eq!(
        sketch_aggregator_checkpoint(&client, "aggregator_test")
            .await
            .unwrap(),
        Some(Lsn::from_u64(90))
    );

    // One bulk group spans apps, attributes and nullable reverse directions.
    // Include an invalid cross-tenant attr pairing: it must never create a row.
    let second_app = Uuid::new_v4();
    client
        .execute("INSERT INTO apps(id) VALUES ($1)", &[&second_app])
        .await
        .unwrap();
    let mut changes = HashMap::new();
    let mut inserted = Vec::new();
    for n in 0..24 {
        let owner = if n % 2 == 0 { app_id } else { second_app };
        let attr = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO attrs(id,app_id) VALUES ($1,$2)",
                &[&attr, &owner],
            )
            .await
            .unwrap();
        let mut delta = AttrSketchDeltas {
            max_lsn: Lsn::from_u64(100),
            ..Default::default()
        };
        delta
            .records
            .insert(SketchDatum::String(format!("bulk-{n}")), n + 1);
        if n % 3 == 0 {
            delta.reverse_records.insert(SketchDatum::Nil, 1);
        }
        changes.insert(
            SketchKey {
                app_id: owner,
                attr_id: attr,
            },
            delta,
        );
        inserted.push(attr);
    }
    changes.insert(
        SketchKey {
            app_id: second_app,
            attr_id,
        },
        AttrSketchDeltas {
            records: HashMap::from([(SketchDatum::Nil, 1)]),
            max_lsn: Lsn::from_u64(100),
            ..Default::default()
        },
    );
    let bulk = SketchDeltaBatch {
        changes,
        lsn: Lsn::from_u64(100),
        next_lsn: Lsn::from_u64(101),
        tx_bytes: 1000,
    };
    persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-a",
        Lsn::from_u64(90),
        &bulk,
    )
    .await
    .unwrap();
    let rows=client.query("SELECT total,reverse_total FROM attr_sketches WHERE attr_id=ANY($1::uuid[]) ORDER BY total",&[&inserted]).await.unwrap();
    assert_eq!(rows.len(), 24);
    for (n, row) in rows.iter().enumerate() {
        assert_eq!(row.get::<_, i64>(0), n as i64 + 1);
        assert_eq!(
            row.get::<_, Option<i64>>(1),
            if n % 3 == 0 { Some(1) } else { None }
        );
    }
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM attr_sketches WHERE app_id=$1 AND attr_id=$2",
                &[&second_app, &attr_id]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    let mut conflicting = bulk.clone();
    conflicting.lsn = Lsn::from_u64(110);
    let error = persist_sketch_delta_batch(
        &mut client,
        "aggregator_test",
        "worker-b",
        Lsn::from_u64(90),
        &conflicting,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        SketchPersistenceError::CheckpointConflict { .. }
    ));
    assert_eq!(
        client
            .query_one(
                "SELECT sum(total)::bigint FROM attr_sketches WHERE attr_id=ANY($1::uuid[])",
                &[&inserted]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        300,
        "all bulk writes roll back together on ownership conflict"
    );
    assert_eq!(
        sketch_aggregator_checkpoint(&client, "aggregator_test")
            .await
            .unwrap(),
        Some(Lsn::from_u64(100))
    );

    drop(client);
    connection_task.await.unwrap().unwrap();
}
