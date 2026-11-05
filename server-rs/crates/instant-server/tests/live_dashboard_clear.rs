use instant_instaql::SYSTEM_CATALOG_APP_ID;
use instant_server::{
    DashboardControlHookOperation, DashboardControlHooks, NoopDashboardControlHooks,
    PostgresDashboardCoreHooks,
};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn clear_app_atomically_brands_attributes_idents_and_resets_rules() {
    let Some(database_url) = instant_test_support::test_database_url(
        "clear_app_atomically_brands_attributes_idents_and_resets_rules",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls)
        .await
        .expect("connect to PostgreSQL");
    tokio::spawn(async move { connection.await.expect("PostgreSQL driver") });
    let schema = format!("dashboard_clear_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema};
             CREATE TABLE transactions (id bigserial PRIMARY KEY, app_id uuid NOT NULL);
             CREATE TABLE rules (app_id uuid PRIMARY KEY, code jsonb NOT NULL);
             CREATE TABLE idents (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text, label text
             );
             CREATE TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL,
               forward_ident uuid, reverse_ident uuid, reverse_etype text, reverse_label text,
               is_indexed boolean NOT NULL DEFAULT false,
               is_required boolean NOT NULL DEFAULT false,
               metadata jsonb, deletion_marked_at timestamptz
             );"
        ))
        .await
        .expect("create migrations-shaped schema");
    let app_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    let forward_ident = Uuid::new_v4();
    let reverse_ident = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs (id, app_id, etype, label)
             VALUES ($1, $2, 'books', 'id')",
            &[&id_attr, &SYSTEM_CATALOG_APP_ID],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO idents (id, app_id, etype, label) VALUES
             ($1, $3, 'books', 'author'), ($2, $3, 'authors', 'books')",
            &[&forward_ident, &reverse_ident, &app_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO attrs
             (id, app_id, etype, label, forward_ident, reverse_ident,
              reverse_etype, reverse_label, is_indexed, is_required, metadata)
             VALUES ($1, $2, 'books', 'author', $3, $4,
                     'authors', 'books', true, true, '{\"existing\":true}')",
            &[&attr_id, &app_id, &forward_ident, &reverse_ident],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO rules (app_id, code) VALUES ($1, $2)",
            &[&app_id, &json!({"books": {"allow": {"view": "true"}}})],
        )
        .await
        .unwrap();

    let hooks = PostgresDashboardCoreHooks::new(client, NoopDashboardControlHooks);
    assert_eq!(
        hooks
            .execute(DashboardControlHookOperation::ClearApp { app_id })
            .await
            .unwrap(),
        serde_json::Value::Bool(true)
    );

    let client = hooks_test_client(&database_url, &schema).await;
    let attr = client
        .query_one(
            "SELECT etype, label, reverse_etype, reverse_label, is_indexed, is_required,
                    metadata #>> '{soft_delete_snapshot,id_attr_id}', deletion_marked_at IS NOT NULL
             FROM attrs WHERE id = $1",
            &[&attr_id],
        )
        .await
        .unwrap();
    assert!(!attr.get::<_, bool>(4));
    assert!(!attr.get::<_, bool>(5));
    assert_eq!(attr.get::<_, String>(6), id_attr.to_string());
    assert!(attr.get::<_, bool>(7));
    for column in 0..4 {
        assert!(
            attr.get::<_, String>(column)
                .starts_with(&format!("{attr_id}_deleted$"))
        );
    }
    let ident_count: i64 = client
        .query_one(
            "SELECT count(*) FROM idents
             WHERE app_id = $1 AND etype LIKE $2",
            &[&app_id, &format!("{attr_id}_deleted$%")],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(ident_count, 2);
    let rules: serde_json::Value = client
        .query_one("SELECT code FROM rules WHERE app_id = $1", &[&app_id])
        .await
        .unwrap()
        .get(0);
    assert_eq!(rules, json!({}));
    let transactions: i64 = client
        .query_one(
            "SELECT count(*) FROM transactions WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(transactions, 1);
}

async fn hooks_test_client(database_url: &str, schema: &str) -> tokio_postgres::Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    client
}
