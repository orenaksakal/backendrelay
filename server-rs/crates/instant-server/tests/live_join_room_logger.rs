use instant_server::{JoinRoomLogger, PostgresJoinRoomLogger};
use serde_json::json;
use tokio_postgres::{Client, NoTls, types::Json};
use uuid::Uuid;

async fn client(database_url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps toggle changes and their ordered queue barriers together.
async fn join_room_logger_groups_counts_and_honors_the_live_toggle() {
    let Some(database_url) = instant_test_support::test_database_url(
        "join_room_logger_groups_counts_and_honors_the_live_toggle",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("join_room_logger_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.apps (id uuid PRIMARY KEY);
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );
             CREATE TABLE {schema}.join_room_logs (
               app_id uuid PRIMARY KEY REFERENCES {schema}.apps(id) ON DELETE CASCADE,
               join_count bigint NOT NULL DEFAULT 1,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );"
        ))
        .await
        .unwrap();
    let config_app_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let other_app_id = Uuid::new_v4();
    let setting_attr_id = Uuid::new_v4();
    let toggled_attr_id = Uuid::new_v4();
    let toggle_entity_id = Uuid::new_v4();
    setup
        .execute(
            &format!("INSERT INTO {schema}.apps (id) VALUES ($1), ($2)"),
            &[&app_id, &other_app_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.attrs (id, app_id, etype, label) VALUES
                   ($1, $3, 'toggles', 'setting'),
                   ($2, $3, 'toggles', 'toggled')"
            ),
            &[&setting_attr_id, &toggled_attr_id, &config_app_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value) VALUES
                   ($1, $2, $3, $4), ($1, $2, $5, $6)"
            ),
            &[
                &config_app_id,
                &toggle_entity_id,
                &setting_attr_id,
                &Json(json!("join-room-logging-enabled?")),
                &toggled_attr_id,
                &Json(json!(false)),
            ],
        )
        .await
        .unwrap();
    let logger_client = client(&database_url).await;
    logger_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let logger = PostgresJoinRoomLogger::with_config_app_id(logger_client, config_app_id);

    logger.log_join(app_id);
    logger.log_join(app_id);
    logger.flush().await;
    let rows = setup
        .query(&format!("SELECT app_id FROM {schema}.join_room_logs"), &[])
        .await
        .unwrap();
    assert!(rows.is_empty());

    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[
                &config_app_id,
                &toggle_entity_id,
                &toggled_attr_id,
                &Json(json!(true)),
            ],
        )
        .await
        .unwrap();
    for _ in 0..3 {
        logger.log_join(app_id);
    }
    for _ in 0..2 {
        logger.log_join(other_app_id);
    }
    logger.flush().await;
    let counts = setup
        .query(
            &format!("SELECT app_id, join_count FROM {schema}.join_room_logs ORDER BY app_id"),
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| (row.get::<_, Uuid>(0), row.get::<_, i64>(1)))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(counts.get(&app_id), Some(&3));
    assert_eq!(counts.get(&other_app_id), Some(&2));

    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[
                &config_app_id,
                &toggle_entity_id,
                &toggled_attr_id,
                &Json(json!("enabled")),
            ],
        )
        .await
        .unwrap();
    logger.log_join(app_id);
    logger.flush().await;
    let count: i64 = setup
        .query_one(
            &format!("SELECT join_count FROM {schema}.join_room_logs WHERE app_id = $1"),
            &[&app_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 4);

    drop(logger);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
