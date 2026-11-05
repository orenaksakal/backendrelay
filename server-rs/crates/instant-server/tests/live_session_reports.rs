use instant_server::{PostgresRuntimeSessionReportCollector, RuntimeSessionReportCollector};
use serde_json::json;
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

async fn client(database_url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

#[derive(Clone, Copy)]
struct ConfigFlag {
    app: Uuid,
    setting_attr: Uuid,
    value_attr: Uuid,
    entity: Uuid,
    proxy_entity: Uuid,
}

async fn create_schema(setup: &Client, schema: &str, flag: ConfigFlag) {
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );
             CREATE TABLE {schema}.rust_runtime_session_reports (
               machine_id uuid PRIMARY KEY,
               report jsonb,
               session_count bigint,
               query_count bigint,
               proxied_connections jsonb,
               report_updated_at timestamptz,
               totals_updated_at timestamptz,
               proxy_updated_at timestamptz,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );"
        ))
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.attrs (id, app_id, etype, label)
                 VALUES ($1, $2, 'flags', 'setting'), ($3, $2, 'flags', 'value')"
            ),
            &[&flag.setting_attr, &flag.app, &flag.value_attr],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.triples (app_id, entity_id, attr_id, value)
                  VALUES ($1, $2, $3, to_jsonb('session-reports-cache-ttl'::text)),
                         ($1, $2, $4, to_jsonb('60000'::text)),
                         ($1, $5, $3, to_jsonb('proxied-connections-overview-enabled'::text)),
                         ($1, $5, $4, 'true'::jsonb)"
            ),
            &[
                &flag.app,
                &flag.entity,
                &flag.setting_attr,
                &flag.value_attr,
                &flag.proxy_entity,
            ],
        )
        .await
        .unwrap();
}

async fn expire(setup: &Client, schema: &str, column: &str, machine_id: Uuid) {
    assert!(matches!(
        column,
        "report_updated_at" | "totals_updated_at" | "proxy_updated_at"
    ));
    setup
        .execute(
            &format!(
                "UPDATE {schema}.rust_runtime_session_reports
                 SET {column} = now() - interval '10 seconds'
                 WHERE machine_id = $1"
            ),
            &[&machine_id],
        )
        .await
        .unwrap();
}

async fn set_report_ttl(setup: &Client, schema: &str, flag: ConfigFlag, ttl: &str) {
    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = to_jsonb($4::text)
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[&flag.app, &flag.entity, &flag.value_attr, &ttl],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn postgres_session_reports_merge_independent_rust_machines() {
    let Some(url) = instant_test_support::test_database_url(
        "postgres_session_reports_merge_independent_rust_machines",
    ) else {
        return;
    };
    let setup = client(&url).await;
    let schema = format!("session_reports_{}", Uuid::new_v4().simple());
    let flag = ConfigFlag {
        app: Uuid::new_v4(),
        setting_attr: Uuid::new_v4(),
        value_attr: Uuid::new_v4(),
        entity: Uuid::new_v4(),
        proxy_entity: Uuid::new_v4(),
    };
    create_schema(&setup, &schema, flag).await;
    let first_client = client(&url).await;
    let second_client = client(&url).await;
    first_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    second_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let first =
        PostgresRuntimeSessionReportCollector::new(first_client).with_config_app_id(flag.app);
    let second =
        PostgresRuntimeSessionReportCollector::new(second_client).with_config_app_id(flag.app);
    let first_machine = Uuid::new_v4();
    let second_machine = Uuid::new_v4();

    first
        .collect_session_reports(first_machine, json!({"app-a": {"count": 2}}))
        .await
        .unwrap();
    let reports = second
        .collect_session_reports(second_machine, json!({"app-b": {"count": 3}}))
        .await
        .unwrap();
    assert_eq!(reports.as_object().unwrap().len(), 2);
    assert_eq!(reports[&first_machine.to_string()]["app-a"]["count"], 2);
    assert_eq!(reports[&second_machine.to_string()]["app-b"]["count"], 3);

    expire(&setup, &schema, "report_updated_at", first_machine).await;
    set_report_ttl(&setup, &schema, flag, "5000").await;
    let reports = second
        .collect_session_reports(second_machine, json!({"app-b": {"count": 3}}))
        .await
        .unwrap();
    assert_eq!(reports.as_object().unwrap().len(), 1);
    assert!(reports.get(first_machine.to_string()).is_none());

    assert_eq!(
        first.collect_totals(first_machine, 2, 4).await.unwrap(),
        (2, 4)
    );
    assert_eq!(
        second.collect_totals(second_machine, 3, 5).await.unwrap(),
        (5, 9)
    );
    expire(&setup, &schema, "totals_updated_at", first_machine).await;
    assert_eq!(
        second.collect_totals(second_machine, 3, 5).await.unwrap(),
        (3, 5)
    );
    first
        .record_proxied_connections(first_machine, json!({"remote": 7}))
        .await
        .unwrap();
    assert_eq!(
        second.collect_proxied_connections().await.unwrap().unwrap()[&first_machine.to_string()]["remote"],
        7
    );
    expire(&setup, &schema, "proxy_updated_at", first_machine).await;
    assert!(
        second
            .collect_proxied_connections()
            .await
            .unwrap()
            .is_none()
    );

    drop(first);
    drop(second);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
