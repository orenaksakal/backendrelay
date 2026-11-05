use instant_instaql::normalized_query_hash;
use instant_server::{
    PostgresFailoverGate, PostgresPool, PostgresQuerySetting, QueryModifierPolicy,
    QueryPlannerPolicy, QueryPreparationPolicy, QuerySettingsPolicy, apply_postgres_query_settings,
};
use serde_json::json;
use tokio_postgres::{NoTls, types::Json};
use uuid::Uuid;

async fn create_config_tables(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            "CREATE TEMP TABLE attrs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
             );
             CREATE TEMP TABLE triples (
               app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );",
        )
        .await
        .unwrap();
}

async fn insert_flag(
    client: &tokio_postgres::Client,
    config_app_id: Uuid,
    entity_id: Uuid,
    setting_attr: Uuid,
    value_attr: Uuid,
    setting: &str,
    value: serde_json::Value,
) {
    client
        .execute(
            "INSERT INTO triples (app_id, entity_id, attr_id, value)
             VALUES ($1, $2, $3, $4), ($1, $2, $5, $6)",
            &[
                &config_app_id,
                &entity_id,
                &setting_attr,
                &Json(json!(setting)),
                &value_attr,
                &Json(value),
            ],
        )
        .await
        .unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One live config-app fixture covers flags, toggles, and planner policy.
async fn failover_gate_reads_live_config_app_toggle_truthiness() {
    let Some(url) = instant_test_support::test_database_url(
        "failover_gate_reads_live_config_app_toggle_truthiness",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    create_config_tables(&client).await;
    let config_app_id = Uuid::new_v4();
    let setting_attr = Uuid::new_v4();
    let toggled_attr = Uuid::new_v4();
    let flag_setting_attr = Uuid::new_v4();
    let flag_value_attr = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let batch_size_entity_id = Uuid::new_v4();
    let idle_sleep_entity_id = Uuid::new_v4();
    let materialize_entity_id = Uuid::new_v4();
    let threshold_entity_id = Uuid::new_v4();
    let disable_hint_entity_id = Uuid::new_v4();
    let planner_toggle_entity_id = Uuid::new_v4();
    let result_toggle_entity_id = Uuid::new_v4();
    let materialized_attr_id = Uuid::new_v4();
    let query = json!({"users": {}});
    client
        .execute(
            "INSERT INTO attrs (id, app_id, etype, label)
             VALUES ($1, $5, 'toggles', 'setting'), ($2, $5, 'toggles', 'toggled'),
                    ($3, $5, 'flags', 'setting'), ($4, $5, 'flags', 'value')",
            &[
                &setting_attr,
                &toggled_attr,
                &flag_setting_attr,
                &flag_value_attr,
                &config_app_id,
            ],
        )
        .await
        .unwrap();
    for (entity_id, setting, value) in [
        (
            materialize_entity_id,
            "always-materialize-attr-ids",
            json!([materialized_attr_id, "invalid"]),
        ),
        (threshold_entity_id, "row-hint-threshold", json!(321)),
        (
            disable_hint_entity_id,
            "disable-hint-query-hashes",
            json!([normalized_query_hash(&query).unwrap()]),
        ),
        (batch_size_entity_id, "custodian-batch-size", json!(250)),
        (idle_sleep_entity_id, "custodian-idle-sleep-ms", json!(75)),
    ] {
        insert_flag(
            &client,
            config_app_id,
            entity_id,
            flag_setting_attr,
            flag_value_attr,
            setting,
            value,
        )
        .await;
    }
    client
        .execute(
            "INSERT INTO triples (app_id, entity_id, attr_id, value)
             VALUES ($1, $2, $3, $4), ($1, $2, $5, $6),
                    ($1, $7, $3, $8), ($1, $7, $5, $9),
                    ($1, $10, $3, $11), ($1, $10, $5, $12)",
            &[
                &config_app_id,
                &entity_id,
                &setting_attr,
                &Json(json!("failing-over")),
                &toggled_attr,
                &Json(json!(true)),
                &planner_toggle_entity_id,
                &Json(json!("disable-like-count-optimization")),
                &Json(json!(true)),
                &result_toggle_entity_id,
                &Json(json!("ensure-empty-symbol-values-for-topics")),
                &Json(json!(true)),
            ],
        )
        .await
        .unwrap();

    let modifier_app_attr = Uuid::new_v4();
    let modifier_hash_attr = Uuid::new_v4();
    let modifier_etype_attr = Uuid::new_v4();
    let modifier_params_attr = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs (id, app_id, etype, label)
             VALUES ($1, $5, 'query-modifiers', 'app-id'),
                    ($2, $5, 'query-modifiers', 'query-hash'),
                    ($3, $5, 'query-modifiers', 'etype'),
                    ($4, $5, 'query-modifiers', 'dollar-params')",
            &[
                &modifier_app_attr,
                &modifier_hash_attr,
                &modifier_etype_attr,
                &modifier_params_attr,
                &config_app_id,
            ],
        )
        .await
        .unwrap();
    let target_app_id = Uuid::new_v4();
    let query_hash = normalized_query_hash(&query).unwrap();
    for (entity_id, params) in [
        (Uuid::from_u128(1), json!({"limit": 50, "offset": 2})),
        (Uuid::from_u128(2), json!({"limit": 10})),
    ] {
        client
            .execute(
                "INSERT INTO triples (app_id, entity_id, attr_id, value)
                 VALUES ($1, $2, $3, $7), ($1, $2, $4, $8),
                        ($1, $2, $5, $9), ($1, $2, $6, $10)",
                &[
                    &config_app_id,
                    &entity_id,
                    &modifier_app_attr,
                    &modifier_hash_attr,
                    &modifier_etype_attr,
                    &modifier_params_attr,
                    &Json(json!(target_app_id)),
                    &Json(json!(query_hash)),
                    &Json(json!("users")),
                    &Json(params),
                ],
            )
            .await
            .unwrap();
    }

    let query_flag_hash_attr = Uuid::new_v4();
    let query_flag_setting_attr = Uuid::new_v4();
    let query_flag_value_attr = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO attrs (id, app_id, etype, label)
             VALUES ($1, $4, 'query-flags', 'query-hash'),
                    ($2, $4, 'query-flags', 'setting'),
                    ($3, $4, 'query-flags', 'value')",
            &[
                &query_flag_hash_attr,
                &query_flag_setting_attr,
                &query_flag_value_attr,
                &config_app_id,
            ],
        )
        .await
        .unwrap();
    for (entity_id, setting, value) in [
        (Uuid::from_u128(3), "enable_nestloop", "off"),
        (Uuid::from_u128(4), "enable_hashjoin", "off"),
    ] {
        client
            .execute(
                "INSERT INTO triples (app_id, entity_id, attr_id, value)
                 VALUES ($1, $2, $3, $6), ($1, $2, $4, $7), ($1, $2, $5, $8)",
                &[
                    &config_app_id,
                    &entity_id,
                    &query_flag_hash_attr,
                    &query_flag_setting_attr,
                    &query_flag_value_attr,
                    &Json(json!(query_hash)),
                    &Json(json!(setting)),
                    &Json(json!(value)),
                ],
            )
            .await
            .unwrap();
    }

    let original_application_name: String = client
        .query_one("SELECT current_setting('application_name')", &[])
        .await
        .unwrap()
        .get(0);
    let transaction = client.transaction().await.unwrap();
    apply_postgres_query_settings(
        &transaction,
        &[PostgresQuerySetting {
            setting: "application_name".to_owned(),
            value: "instant-query-flags-live".to_owned(),
        }],
    )
    .await
    .unwrap();
    let configured: String = transaction
        .query_one("SELECT current_setting('application_name')", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(configured, "instant-query-flags-live");
    transaction.rollback().await.unwrap();
    let reset: String = client
        .query_one("SELECT current_setting('application_name')", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(reset, original_application_name);

    let invalid_transaction = client.transaction().await.unwrap();
    assert!(
        apply_postgres_query_settings(
            &invalid_transaction,
            &[PostgresQuerySetting {
                setting: "not_a_postgres_setting".to_owned(),
                value: "off".to_owned(),
            }],
        )
        .await
        .is_err()
    );
    assert!(invalid_transaction.simple_query("SELECT 1").await.is_err());
    invalid_transaction.rollback().await.unwrap();

    let gate = PostgresFailoverGate::new(client).with_config_app_id(config_app_id);
    assert!(gate.is_failing_over().await.unwrap());
    assert!(!gate.is_toggled("missing").await.unwrap());
    assert_eq!(
        gate.flag_values(&["custodian-batch-size", "custodian-idle-sleep-ms", "missing",])
            .await
            .unwrap(),
        [
            ("custodian-batch-size".to_owned(), json!(250)),
            ("custodian-idle-sleep-ms".to_owned(), json!(75)),
        ]
        .into_iter()
        .collect()
    );
    let options = gate.options(&query).await.unwrap();
    assert_eq!(
        options.always_materialize_attr_ids,
        vec![materialized_attr_id]
    );
    assert_eq!(options.row_hint_threshold, Some(321));
    assert_eq!(options.pg_hints_enabled, Some(false));
    assert_eq!(options.row_hints_enabled, Some(true));
    assert_eq!(options.like_count_optimization, Some(false));
    assert_eq!(options.ensure_empty_symbol_values_for_topics, Some(true));
    assert_eq!(
        gate.settings(query_hash).await.unwrap(),
        vec![
            PostgresQuerySetting {
                setting: "enable_nestloop".to_owned(),
                value: "off".to_owned(),
            },
            PostgresQuerySetting {
                setting: "enable_hashjoin".to_owned(),
                value: "off".to_owned(),
            },
        ]
    );
    assert_eq!(
        gate.apply(target_app_id, query_hash, &query).await.unwrap(),
        json!({"users": {"$": {"limit": 10, "offset": 2}}})
    );
    assert_eq!(
        gate.apply(Uuid::new_v4(), query_hash, &query)
            .await
            .unwrap(),
        query
    );
}

#[tokio::test]
async fn pooled_controls_reuse_statements_but_read_live_values() {
    let Some(url) = instant_test_support::test_database_url(
        "pooled_controls_reuse_statements_but_read_live_values",
    ) else {
        return;
    };
    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    let config_app_id = Uuid::new_v4();
    let setting_attr = Uuid::new_v4();
    let value_attr = Uuid::new_v4();
    let client = pool.acquire().await.unwrap();
    create_config_tables(&client).await;
    client
        .execute(
            "INSERT INTO attrs VALUES ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')",
            &[&setting_attr, &value_attr, &config_app_id],
        )
        .await
        .unwrap();
    insert_flag(
        &client,
        config_app_id,
        Uuid::new_v4(),
        setting_attr,
        value_attr,
        "live",
        json!(1),
    )
    .await;
    drop(client);
    let gate = PostgresFailoverGate::from_pool(pool.clone()).with_config_app_id(config_app_id);
    for value in [1, 2] {
        let client = pool.acquire().await.unwrap();
        client
            .execute(
                "UPDATE triples SET value = $1 WHERE attr_id = $2",
                &[&Json(json!(value)), &value_attr],
            )
            .await
            .unwrap();
        drop(client);
        for _ in 0..3 {
            assert_eq!(gate.flag_value("live").await.unwrap(), Some(json!(value)));
            assert_eq!(
                gate.flag_values(&["live"]).await.unwrap()["live"],
                json!(value)
            );
            assert!(!gate.is_failing_over().await.unwrap());
            let query = json!({"users": {}});
            gate.options(&query).await.unwrap();
            gate.settings(42).await.unwrap();
            gate.apply(config_app_id, 42, &query).await.unwrap();
            gate.prepare(config_app_id, &query).await.unwrap();
        }
    }
    let client = pool.acquire().await.unwrap();
    let count: i64 = client.query_one(
        "SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE '%FROM attrs %' AND statement NOT LIKE '%pg_prepared_statements%'",
        &[],
    ).await.unwrap().get(0);
    assert_eq!(
        count, 7,
        "one statement per control shape must survive repeated reads"
    );
    drop(client);
    let gate = gate.with_config_app_id(Uuid::new_v4());
    assert_eq!(gate.flag_value("live").await.unwrap(), None);
    let gate = gate.with_config_app_id(config_app_id);
    assert_eq!(gate.flag_value("live").await.unwrap(), Some(json!(2)));
    let client = pool.acquire().await.unwrap();
    let old_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    drop(client);
    let (observer, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    observer
        .query_one("SELECT pg_terminate_backend($1)", &[&old_pid])
        .await
        .unwrap();
    let client = pool.acquire().await.unwrap();
    let new_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(old_pid, new_pid);
    create_config_tables(&client).await;
    drop(client);
    assert_eq!(gate.flag_value("live").await.unwrap(), None);
    assert!(!gate.is_failing_over().await.unwrap());
    gate.prepare(config_app_id, &json!({"users": {}}))
        .await
        .unwrap();
}

/// Opt-in local control-path microbenchmark; these are burst durations, not user latency.
#[tokio::test]
async fn benchmark_live_control_preparation() {
    if std::env::var_os("BENCHMARK_CONTROL_PREPARATION").is_none() {
        return;
    }
    let url =
        instant_test_support::test_database_url("benchmark_live_control_preparation").unwrap();
    let config_app_id = Uuid::new_v4();
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    create_config_tables(&client).await;
    let uncached = PostgresFailoverGate::new(client).with_config_app_id(config_app_id);
    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    let client = pool.acquire().await.unwrap();
    create_config_tables(&client).await;
    drop(client);
    let cached = PostgresFailoverGate::from_pool(pool).with_config_app_id(config_app_id);
    let query = json!({"users": {}});
    for round in 0..4 {
        let variants = if round % 2 == 0 {
            [("uncached", &uncached), ("cached", &cached)]
        } else {
            [("cached", &cached), ("uncached", &uncached)]
        };
        for (variant, gate) in variants {
            for _ in 0..10 {
                gate.prepare(config_app_id, &query).await.unwrap();
            }
            let start = std::time::Instant::now();
            for _ in 0..1000 {
                gate.prepare(config_app_id, &query).await.unwrap();
            }
            println!(
                "control_preparation variant={variant} round={round} calls=1000 elapsed_ms={:.3}",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}

#[tokio::test]
async fn config_identity_allows_plan_reuse_with_skewed_app_statistics() {
    let Some(url) = instant_test_support::test_database_url(
        "config_identity_allows_plan_reuse_with_skewed_app_statistics",
    ) else {
        return;
    };
    let pool = PostgresPool::connect(&url, 1).await.unwrap();
    let client = pool.acquire().await.unwrap();
    create_config_tables(&client).await;
    client.batch_execute("CREATE INDEX ON attrs(app_id); CREATE INDEX ON triples(app_id, attr_id);
        INSERT INTO attrs SELECT md5(i::text)::uuid, '00000000-0000-0000-0000-000000000001', 'flags', 'value' FROM generate_series(1,10000) i;
        INSERT INTO triples SELECT '00000000-0000-0000-0000-000000000001', md5(i::text)::uuid, md5(((i % 10000)+1)::text)::uuid, 'null'::jsonb FROM generate_series(1,100000) i;
        ANALYZE attrs; ANALYZE triples; SET plan_cache_mode=auto;").await.unwrap();
    drop(client);
    let gate = PostgresFailoverGate::from_pool(pool.clone()).with_config_app_id(Uuid::new_v4());
    for _ in 0..20 {
        gate.prepare(Uuid::new_v4(), &json!({"users": {}}))
            .await
            .unwrap();
    }
    let client = pool.acquire().await.unwrap();
    let row = client.query_one("SELECT generic_plans, custom_plans FROM pg_prepared_statements WHERE statement LIKE 'WITH control_values AS MATERIALIZED%'", &[]).await.unwrap();
    let generic: i64 = row.get(0);
    let custom: i64 = row.get(1);
    println!("control_plan_reuse generic={generic} custom={custom}");
    if std::env::var_os("BENCHMARK_CONTROL_PREPARATION").is_some() {
        let specialized: String = client.query_one("SELECT statement FROM pg_prepared_statements WHERE statement LIKE 'WITH control_values AS MATERIALIZED%'", &[]).await.unwrap().get(0);
        // Recover the prior parameterized shape from the actual prepared SQL,
        // avoiding a separately maintained copy of the production query.
        let mut original = specialized.clone();
        for parameter in (1..=4).rev() {
            original = original.replace(&format!("${parameter}"), &format!("${}", parameter + 1));
        }
        let config_literal = specialized
            .split("WHERE a.app_id = ")
            .nth(1)
            .unwrap()
            .split(" AND")
            .next()
            .unwrap();
        original = original.replace(config_literal, "$1");
        let config_id =
            Uuid::parse_str(config_literal.trim_end_matches("::uuid").trim_matches('\'')).unwrap();
        let original = client.prepare_cached(&original).await.unwrap();
        let specialized = client.prepare_cached(&specialized).await.unwrap();
        let target = Uuid::new_v4().to_string();
        let flags: &[&str] = &["row-hint-threshold"];
        let toggles: &[&str] = &["disable-pg-hints"];
        let old_params: &[&(dyn tokio_postgres::types::ToSql + Sync)] =
            &[&config_id, &target, &0_i32, &flags, &toggles];
        let new_params: &[&(dyn tokio_postgres::types::ToSql + Sync)] =
            &[&target, &0_i32, &flags, &toggles];
        for round in 0..4 {
            let old = ("parameterized_identity", &original, old_params);
            let new = ("specialized_identity", &specialized, new_params);
            for (variant, statement, params) in if round % 2 == 0 {
                [old, new]
            } else {
                [new, old]
            } {
                for _ in 0..10 {
                    assert!(client.query(statement, params).await.unwrap().is_empty());
                }
                let start = std::time::Instant::now();
                for _ in 0..1000 {
                    assert!(client.query(statement, params).await.unwrap().is_empty());
                }
                println!(
                    "skewed_control_sql variant={variant} round={round} calls=1000 elapsed_ms={:.3}",
                    start.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
    }

    assert!(
        generic >= 10,
        "control reads repeatedly replanned under skewed app statistics: generic={generic}, custom={custom}"
    );
}
