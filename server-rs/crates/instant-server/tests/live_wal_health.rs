use instant_http::WalHealthSource;
use instant_server::{
    InvalidationHealthObserver, PostgresInvalidationHealthObserver, PostgresWalHealth,
};
use serde_json::Value;
use tokio_postgres::NoTls;
use uuid::Uuid;

async fn health(database_url: &str, schema: &str, process: &str) -> PostgresWalHealth {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    PostgresWalHealth::new(client, process)
}

async fn create_schema(setup: &tokio_postgres::Client, schema: &str) {
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.config (
               k text PRIMARY KEY NOT NULL,
               v json NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_machines (
               machine_id uuid PRIMARY KEY,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );"
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn shared_wal_health_merges_processes_and_clears_only_its_owner() {
    let Some(database_url) = instant_test_support::test_database_url(
        "shared_wal_health_merges_processes_and_clears_only_its_owner",
    ) else {
        return;
    };
    let (setup, setup_connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { setup_connection.await.unwrap() });
    let schema = format!("wal_health_{}", Uuid::new_v4().simple());
    create_schema(&setup, &schema).await;

    let first = health(&database_url, &schema, "first").await;
    let second = health(&database_url, &schema, "second").await;
    let reader = health(&database_url, &schema, "reader").await;
    let (first_mark, second_mark) = tokio::join!(first.mark_unhealthy(), second.mark_unhealthy());
    first_mark.unwrap();
    second_mark.unwrap();
    assert!(!reader.wal_healthy().await.unwrap());
    let value: Value = setup
        .query_one(
            &format!("SELECT v::jsonb FROM {schema}.config WHERE k = 'wal-errors'"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["first", "second"]
    );

    first.mark_healthy().await.unwrap();
    assert!(!reader.wal_healthy().await.unwrap());
    second.mark_healthy().await.unwrap();
    assert!(reader.wal_healthy().await.unwrap());

    let observer = PostgresInvalidationHealthObserver::spawn(first.clone());
    observer.set_healthy(false);
    // Local readiness changes synchronously; it does not wait for the
    // best-effort shared-row writer.
    assert!(!first.wal_healthy().await.unwrap());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !reader.wal_healthy().await.unwrap() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let value: Value = setup
        .query_one(
            &format!("SELECT v::jsonb FROM {schema}.config WHERE k = 'wal-errors'"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(value.as_object().unwrap().contains_key("first"));

    observer.graceful_shutdown().await;
    assert!(first.wal_healthy().await.unwrap());
    assert!(reader.wal_healthy().await.unwrap());
    let value: Value = setup
        .query_one(
            &format!("SELECT v::jsonb FROM {schema}.config WHERE k = 'wal-errors'"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(value, serde_json::json!({}));

    drop((observer, first, second, reader));
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
async fn stale_machine_wal_errors_do_not_take_the_fleet_unhealthy() {
    let Some(database_url) = instant_test_support::test_database_url(
        "stale_machine_wal_errors_do_not_take_the_fleet_unhealthy",
    ) else {
        return;
    };
    let (setup, setup_connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move { setup_connection.await.unwrap() });
    let schema = format!("wal_health_{}", Uuid::new_v4().simple());
    create_schema(&setup, &schema).await;

    let machine = Uuid::new_v4();
    let process = machine.simple().to_string();
    setup
        .execute(
            &format!("INSERT INTO {schema}.rust_realtime_machines (machine_id) VALUES ($1)"),
            &[&machine],
        )
        .await
        .unwrap();

    let crashed = health(&database_url, &schema, &process).await;
    crashed.mark_unhealthy().await.unwrap();
    let survivor = health(&database_url, &schema, "survivor").await;
    assert!(
        !survivor.wal_healthy().await.unwrap(),
        "a fresh machine marker must keep the fleet unhealthy"
    );

    setup
        .execute(
            &format!(
                "UPDATE {schema}.rust_realtime_machines
                 SET updated_at = now() - interval '1 minute'
                 WHERE machine_id = $1"
            ),
            &[&machine],
        )
        .await
        .unwrap();
    assert!(
        survivor.wal_healthy().await.unwrap(),
        "a dead machine marker must be ignored after the grace period"
    );

    let legacy = health(&database_url, &schema, "legacy-jvm").await;
    legacy.mark_unhealthy().await.unwrap();
    assert!(
        !survivor.wal_healthy().await.unwrap(),
        "a fresh non-UUID marker must count as unhealthy"
    );

    setup
        .execute(
            &format!(
                "UPDATE {schema}.config
                 SET v = jsonb_build_object(
                   'legacy-jvm', (now() - interval '1 minute')::text
                 )::json
                 WHERE k = 'wal-errors'"
            ),
            &[],
        )
        .await
        .unwrap();
    assert!(
        survivor.wal_healthy().await.unwrap(),
        "a stale non-UUID marker must be ignored"
    );

    drop((crashed, survivor, legacy));
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
