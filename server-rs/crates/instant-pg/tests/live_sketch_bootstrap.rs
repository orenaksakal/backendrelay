use instant_pg::{
    bootstrap_attr_sketches, create_exported_snapshot_slot, sketch_aggregator_checkpoint,
};
use serde_json::json;
use tokio_postgres::NoTls;
use tokio_postgres::types::Json;
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)] // One isolated schema verifies the complete snapshot boundary.
async fn exported_slot_snapshot_bootstraps_only_pre_slot_triples() {
    let Some(database_url) = instant_test_support::test_database_url(
        "exported_slot_snapshot_bootstraps_only_pre_slot_triples",
    ) else {
        return;
    };
    let (mut snapshot_client, snapshot_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let snapshot_task = tokio::spawn(snapshot_connection);
    let (write_client, write_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let write_task = tokio::spawn(write_connection);
    let namespace = format!("sketch_bootstrap_{}", Uuid::new_v4().simple());
    let slot_name = format!(
        "aggregator_test_{}",
        &Uuid::new_v4().simple().to_string()[..20]
    );
    write_client
        .batch_execute(&format!(
            "CREATE SCHEMA {namespace};
             CREATE TYPE {namespace}.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TABLE {namespace}.apps (id uuid PRIMARY KEY);
             CREATE TABLE {namespace}.attrs (id uuid PRIMARY KEY);
             CREATE TABLE {namespace}.triples (
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               value jsonb NOT NULL,
               checked_data_type {namespace}.checked_data_type,
               created_at bigint NOT NULL,
               eav boolean NOT NULL,
               ea boolean NOT NULL
             );
             CREATE TABLE {namespace}.attr_sketches (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, attr_id uuid NOT NULL,
               width integer NOT NULL, depth integer NOT NULL, total bigint NOT NULL,
               total_not_binned bigint NOT NULL, bins bytea NOT NULL, max_lsn pg_lsn,
               reverse_width integer, reverse_depth integer, reverse_total bigint,
               reverse_bins bytea, UNIQUE (app_id, attr_id)
             );
             CREATE TABLE {namespace}.wal_aggregator_status (
               slot_name text PRIMARY KEY, lsn pg_lsn, process_id text
             );
             SET search_path TO {namespace}, public;"
        ))
        .await
        .unwrap();
    snapshot_client
        .batch_execute(&format!("SET search_path TO {namespace}, public"))
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    write_client
        .execute("INSERT INTO apps (id) VALUES ($1)", &[&app_id])
        .await
        .unwrap();
    write_client
        .execute("INSERT INTO attrs (id) VALUES ($1)", &[&attr_id])
        .await
        .unwrap();
    insert_triple(&write_client, app_id, attr_id, "before-slot").await;

    let replication_url = if database_url.contains('?') {
        format!("{database_url}&replication=database")
    } else {
        format!("{database_url}?replication=database")
    };
    let create_url = replication_url.clone();
    let create_name = slot_name.clone();
    let snapshot_slot = tokio::task::spawn_blocking(move || {
        create_exported_snapshot_slot(&create_url, &create_name)
    })
    .await
    .unwrap()
    .unwrap()
    .expect("unique test slot must be created");
    insert_triple(&write_client, app_id, attr_id, "after-slot").await;

    assert_eq!(
        bootstrap_attr_sketches(
            &mut snapshot_client,
            &write_client,
            &snapshot_slot,
            &slot_name,
            "bootstrap-test",
        )
        .await
        .unwrap(),
        1
    );
    let total: i64 = write_client
        .query_one("SELECT total FROM attr_sketches", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(total, 1, "snapshot must exclude the post-slot insert");
    assert_eq!(
        sketch_aggregator_checkpoint(&write_client, &slot_name)
            .await
            .unwrap(),
        Some(snapshot_slot.consistent_point)
    );

    drop(snapshot_slot);
    write_client
        .execute("SELECT pg_drop_replication_slot($1)", &[&slot_name])
        .await
        .unwrap();
    write_client
        .batch_execute(&format!(
            "SET search_path TO public; DROP SCHEMA {namespace} CASCADE"
        ))
        .await
        .unwrap();
    drop(snapshot_client);
    drop(write_client);
    snapshot_task.await.unwrap().unwrap();
    write_task.await.unwrap().unwrap();
}

async fn insert_triple(client: &tokio_postgres::Client, app_id: Uuid, attr_id: Uuid, value: &str) {
    client
        .execute(
            "INSERT INTO triples (app_id, attr_id, entity_id, value, created_at, eav, ea)
             VALUES ($1, $2, $3, $4, 1700000000000, false, true)",
            &[&app_id, &attr_id, &Uuid::new_v4(), &Json(json!(value))],
        )
        .await
        .unwrap();
}
