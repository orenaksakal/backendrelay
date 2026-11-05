use std::time::Duration;

use instant_server::{
    PostgresPool, PostgresReceiveTimeoutPolicy, PostgresStatementCancelWaitPolicy,
    ReceiveHandlerTimeoutPolicy, ReceiveHandlerTimeouts, ReceiveTimeoutPolicy,
    StatementCancelWaitPolicy,
};
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
async fn receive_timeout_policy_follows_the_per_app_config_entity() {
    let Some(database_url) = instant_test_support::test_database_url(
        "receive_timeout_policy_follows_the_per_app_config_entity",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("receive_timeout_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );"
        ))
        .await
        .unwrap();
    let config_app_id = Uuid::new_v4();
    let target_app_id = Uuid::new_v4();
    let app_attr_id = Uuid::new_v4();
    let timeout_attr_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.attrs (id, app_id, etype, label) VALUES
                   ($1, $3, 'handle-receive-timeout', 'appId'),
                   ($2, $3, 'handle-receive-timeout', 'timeoutMs')"
            ),
            &[&app_attr_id, &timeout_attr_id, &config_app_id],
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
                &entity_id,
                &app_attr_id,
                &Json(json!(target_app_id.to_string())),
                &timeout_attr_id,
                &Json(json!(25)),
            ],
        )
        .await
        .unwrap();
    let policy_client = client(&database_url).await;
    policy_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let pool = PostgresPool::connect(&database_url, 1).await.unwrap();
    pool.acquire()
        .await
        .unwrap()
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policies = [
        PostgresReceiveTimeoutPolicy::new(policy_client).with_config_app_id(config_app_id),
        PostgresReceiveTimeoutPolicy::from_pool(pool.clone()).with_config_app_id(config_app_id),
    ];
    for policy in &policies {
        assert_eq!(policy.timeout(None).await.unwrap(), Duration::from_secs(5));
        for _ in 0..8 {
            assert_eq!(
                policy.timeout(Some(target_app_id)).await.unwrap(),
                Duration::from_millis(25)
            );
        }
        assert_eq!(
            policy.timeout(Some(Uuid::new_v4())).await.unwrap(),
            Duration::from_secs(5)
        );
    }
    assert_eq!(pool.acquire().await.unwrap().query_one("SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE 'SELECT timeout_value.value%'", &[]).await.unwrap().get::<_, i64>(0), 1);
    setup
        .execute(
            &format!(
                "UPDATE {schema}.triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3"
            ),
            &[
                &config_app_id,
                &entity_id,
                &timeout_attr_id,
                &Json(json!(40)),
            ],
        )
        .await
        .unwrap();
    for policy in &policies {
        assert_eq!(
            policy.timeout(Some(target_app_id)).await.unwrap(),
            Duration::from_millis(40)
        );
    }
    setup
        .execute(
            &format!("UPDATE {schema}.triples SET value='null'::jsonb WHERE attr_id=$1"),
            &[&timeout_attr_id],
        )
        .await
        .unwrap();
    for policy in &policies {
        assert!(
            policy.timeout(Some(target_app_id)).await.is_err(),
            "a cached SQL plan must not cache a previous valid config result"
        );
    }
    drop(policies);
    drop(pool);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
async fn combined_handler_limits_are_one_fresh_statement_with_scope_defaults_and_error_priority() {
    let Some(url) = instant_test_support::test_database_url(
        "combined_handler_limits_are_one_fresh_statement_with_scope_defaults_and_error_priority",
    ) else {
        return;
    };
    let setup = client(&url).await;
    let schema = format!("handler_limits_{}", Uuid::new_v4().simple());
    setup.batch_execute(&format!("CREATE SCHEMA {schema};
        CREATE TABLE {schema}.attrs(id uuid PRIMARY KEY,app_id uuid NOT NULL,etype text NOT NULL,label text NOT NULL);
        CREATE TABLE {schema}.triples(app_id uuid NOT NULL,entity_id uuid NOT NULL,attr_id uuid NOT NULL,value jsonb NOT NULL)")).await.unwrap();
    let config = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let app_attr = Uuid::new_v4();
    let timeout_attr = Uuid::new_v4();
    let setting_attr = Uuid::new_v4();
    let value_attr = Uuid::new_v4();
    let flag = Uuid::new_v4();
    let attrs = [
        (app_attr, "handle-receive-timeout", "appId"),
        (timeout_attr, "handle-receive-timeout", "timeoutMs"),
        (setting_attr, "flags", "setting"),
        (value_attr, "flags", "value"),
    ];
    for (id, etype, label) in attrs {
        setup
            .execute(
                &format!("INSERT INTO {schema}.attrs VALUES($1,$2,$3,$4)"),
                &[&id, &config, &etype, &label],
            )
            .await
            .unwrap();
    }
    for scope in [foreign, config] {
        for (entity, attr, value) in [
            (first, app_attr, json!(first)),
            (first, timeout_attr, json!(25)),
            (second, app_attr, json!(second)),
            (second, timeout_attr, json!(70)),
            (flag, setting_attr, json!("statement-cancel-wait-ms")),
            (flag, value_attr, json!(9)),
        ] {
            let value = if scope == foreign && attr == timeout_attr {
                json!(999)
            } else if scope == foreign && attr == value_attr {
                json!(-1)
            } else {
                value
            };
            setup
                .execute(
                    &format!("INSERT INTO {schema}.triples VALUES($1,$2,$3,$4)"),
                    &[&scope, &entity, &attr, &Json(value)],
                )
                .await
                .unwrap();
        }
    }
    let fixed = client(&url).await;
    fixed
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    pool.acquire()
        .await
        .unwrap()
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    let policies = [
        PostgresReceiveTimeoutPolicy::new(fixed).with_config_app_id(config),
        PostgresReceiveTimeoutPolicy::from_pool(pool.clone()).with_config_app_id(config),
    ];
    let cancel =
        PostgresStatementCancelWaitPolicy::from_pool(pool.clone()).with_config_app_id(config);
    for policy in &policies {
        for (app, ms) in [
            (None, 5000),
            (Some(first), 25),
            (Some(second), 70),
            (Some(Uuid::new_v4()), 5000),
            (Some(first), 25),
        ] {
            assert_eq!(
                policy.handler_timeouts(app).await.unwrap(),
                ReceiveHandlerTimeouts {
                    timeout: Duration::from_millis(ms),
                    cancel_wait: Duration::from_millis(9)
                }
            );
            assert_eq!(
                policy.timeout(app).await.unwrap(),
                Duration::from_millis(ms)
            );
        }
    }
    // Count a warmed pooled call: exactly one fresh SELECT and at most one
    // checkout SET (an unchanged session mode is not re-asserted).
    if std::env::var("TEST_DATABASE_STATEMENT_COUNTS").as_deref() == Ok("1") {
        let counts = "SELECT coalesce(sum(calls) FILTER (WHERE query LIKE 'SELECT (%' AND query LIKE '%AS receive_timeout%' AND query LIKE '%AS cancel_wait%'),0)::bigint, coalesce(sum(calls) FILTER (WHERE query LIKE 'SET default_transaction_read_only%'),0)::bigint FROM pg_stat_statements WHERE dbid=(SELECT oid FROM pg_database WHERE datname=current_database())";
        let before = setup.query_one(counts, &[]).await.unwrap();
        policies[1].handler_timeouts(Some(first)).await.unwrap();
        let after = setup.query_one(counts, &[]).await.unwrap();
        assert_eq!(after.get::<_, i64>(0) - before.get::<_, i64>(0), 1);
        let set_delta = after.get::<_, i64>(1) - before.get::<_, i64>(1);
        assert!(
            (0..=1).contains(&set_delta),
            "a checkout must issue at most one mode statement, issued {set_delta}"
        );
    }
    setup.execute(&format!("UPDATE {schema}.triples SET value=CASE WHEN attr_id=$2 THEN 'null'::jsonb ELSE '-1'::jsonb END WHERE app_id=$1 AND attr_id IN ($2,$3)"),&[&config,&timeout_attr,&value_attr]).await.unwrap();
    for policy in &policies {
        let error = policy.handler_timeouts(Some(first)).await.unwrap_err();
        assert!(error.body().message.contains("timeoutMs"));
        assert_eq!(
            error.body().message,
            policy
                .timeout(Some(first))
                .await
                .unwrap_err()
                .body()
                .message
        );
        let init_error = policy.handler_timeouts(None).await.unwrap_err();
        assert_eq!(
            init_error.body().message,
            cancel.wait_duration().await.unwrap_err().body().message
        );
    }
    setup.execute(&format!("UPDATE {schema}.triples SET value='0'::jsonb WHERE app_id=$1 AND attr_id IN ($2,$3)"),&[&config,&timeout_attr,&value_attr]).await.unwrap();
    for policy in &policies {
        assert_eq!(
            policy.handler_timeouts(Some(first)).await.unwrap(),
            ReceiveHandlerTimeouts {
                timeout: Duration::ZERO,
                cancel_wait: Duration::ZERO
            }
        );
    }
    setup
        .execute(
            &format!("DELETE FROM {schema}.triples WHERE app_id=$1"),
            &[&config],
        )
        .await
        .unwrap();
    for policy in &policies {
        assert_eq!(
            policy.handler_timeouts(Some(first)).await.unwrap(),
            ReceiveHandlerTimeouts {
                timeout: Duration::from_secs(5),
                cancel_wait: Duration::from_millis(500)
            }
        );
    }
    drop((policies, cancel, pool));
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
