use std::collections::BTreeSet;

use instant_datalog::{
    DataType, DatalogValue, MatchCteOptions, TopicPart, render_nested_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality,
    collect_query_invalidation_topics, compile_query, plan_nested_query, shape_response,
};
use instant_pg::{query, query_instaql, query_instaql_with_typed_parameters};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::{NoTls, Transaction, types::Json};
use uuid::Uuid;

mod support;

const APP_ID: Uuid = Uuid::from_u128(0xa1000000_0000_4000_8000_000000000001);
const CREATOR_ID: Uuid = Uuid::from_u128(0xd1000000_0000_4000_8000_000000000001);
const USER_ID_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000001);
const HANDLE_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000002);
const ACTIVE_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000003);
const SHELVES_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000004);
const SHELF_ID_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000005);
const SHELF_NAME_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000006);
const SHELF_ORDER_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000007);
const CONVERSATION_ID_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000008);
const CONVERSATION_ORDER_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_000000000009);
const MESSAGE_ID_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_00000000000a);
const MESSAGE_TIME_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_00000000000b);
const MESSAGE_CONVERSATION_ATTR: Uuid = Uuid::from_u128(0xe1000000_0000_4000_8000_00000000000c);

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    first_query: Value,
    expected_first: Value,
    expected_after: Value,
    conversation_query: Value,
    expected_conversations: Value,
}

#[derive(Clone, Copy)]
struct AttrSeed {
    id: Uuid,
    forward_ident: Uuid,
    value_type: &'static str,
    cardinality: &'static str,
    unique: bool,
    indexed: bool,
    checked: Option<&'static str>,
    required: bool,
    etype: &'static str,
    label: &'static str,
    reverse_ident: Option<Uuid>,
    reverse_etype: Option<&'static str>,
    reverse_label: Option<&'static str>,
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "One migration-backed fixture executes its complete cursor and nested-result corpus."
)]
async fn migrated_postgres_executes_nested_boolean_pagination_and_large_relationship_graph_like_clojure()
 {
    let Some(database_url) = instant_test_support::test_database_url(
        "migrated_postgres_executes_nested_boolean_pagination_and_large_relationship_graph_like_clojure",
    ) else {
        return;
    };
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/live_migrated_nested_instaql.json")).unwrap();
    assert!(fixture.oracle.contains("45d3ffbb"));

    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let transaction = client.transaction().await.unwrap();
    assert_migrated_schema(&transaction).await;
    seed_catalog(&transaction).await;
    seed_triples(&transaction).await;

    let catalog = catalog();
    let compiled = compile_query(&fixture.first_query, &catalog, false).unwrap();
    let results = query_instaql(
        &transaction,
        &compiled,
        APP_ID,
        &MatchCteOptions {
            ensure_empty_symbol_values_for_topics: Some(true),
            ..MatchCteOptions::default()
        },
    )
    .await
    .unwrap();
    let typed_results = query_instaql_with_typed_parameters(
        &transaction,
        &compiled,
        APP_ID,
        &MatchCteOptions {
            ensure_empty_symbol_values_for_topics: Some(true),
            ..MatchCteOptions::default()
        },
        instant_pg::DEFAULT_QUERY_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(
        results, typed_results,
        "typed protocol must preserve rows, page info and topics"
    );
    let response = shape_response(&compiled, &results, &catalog, false).unwrap();
    assert_eq!(
        serde_json::to_value(&response.data).unwrap(),
        fixture.expected_first
    );
    let page = &response.page_info["users"];
    assert!(page.has_next_page);
    assert!(!page.has_previous_page);
    let start = page.start_cursor.as_ref().unwrap();
    let end = page.end_cursor.as_ref().unwrap();
    assert_eq!(start.entity_id, user_id(1));
    assert_eq!(start.value, Some(DatalogValue::Json(json!("alex"))));
    assert_eq!(end.entity_id, user_id(2));
    assert_eq!(end.value, Some(DatalogValue::Json(json!("nicole"))));
    assert_exact_topic_attributes(
        &compiled,
        &results,
        &[
            USER_ID_ATTR,
            HANDLE_ATTR,
            ACTIVE_ATTR,
            SHELVES_ATTR,
            SHELF_ID_ATTR,
            SHELF_NAME_ATTR,
            SHELF_ORDER_ATTR,
        ],
    );

    let mut after_query = fixture.first_query;
    after_query["users"]["$"]["after"] = json!([
        end.entity_id,
        end.attr_id,
        datalog_json(end.value.as_ref().unwrap()),
        end.created_at
    ]);
    let after_compiled = compile_query(&after_query, &catalog, false).unwrap();
    let after_results = query_instaql(
        &transaction,
        &after_compiled,
        APP_ID,
        &MatchCteOptions {
            ensure_empty_symbol_values_for_topics: Some(true),
            ..MatchCteOptions::default()
        },
    )
    .await
    .unwrap();
    let after_response = shape_response(&after_compiled, &after_results, &catalog, false).unwrap();
    assert_eq!(
        serde_json::to_value(&after_response.data).unwrap(),
        fixture.expected_after
    );
    let after_page = &after_response.page_info["users"];
    assert!(!after_page.has_next_page);
    assert!(after_page.has_previous_page);
    assert_eq!(
        after_page.start_cursor.as_ref().unwrap().entity_id,
        user_id(3)
    );
    assert_exact_topic_attributes(
        &after_compiled,
        &after_results,
        &[
            USER_ID_ATTR,
            HANDLE_ATTR,
            ACTIVE_ATTR,
            SHELVES_ATTR,
            SHELF_ID_ATTR,
            SHELF_NAME_ATTR,
            SHELF_ORDER_ATTR,
        ],
    );

    let conversation_compiled =
        compile_query(&fixture.conversation_query, &catalog, false).unwrap();
    let conversation_results = query_instaql(
        &transaction,
        &conversation_compiled,
        APP_ID,
        &MatchCteOptions {
            ensure_empty_symbol_values_for_topics: Some(true),
            ..MatchCteOptions::default()
        },
    )
    .await
    .unwrap();
    let conversation_response = shape_response(
        &conversation_compiled,
        &conversation_results,
        &catalog,
        false,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&conversation_response.data).unwrap(),
        fixture.expected_conversations
    );
    let conversation_page = &conversation_response.page_info["conversations"];
    assert!(conversation_page.has_next_page);
    assert!(!conversation_page.has_previous_page);
    assert_eq!(
        conversation_page.start_cursor.as_ref().unwrap().entity_id,
        conversation_id(3)
    );
    assert_eq!(
        conversation_page.end_cursor.as_ref().unwrap().entity_id,
        conversation_id(2)
    );
    assert_exact_topic_attributes(
        &conversation_compiled,
        &conversation_results,
        &[
            CONVERSATION_ID_ATTR,
            CONVERSATION_ORDER_ATTR,
            MESSAGE_ID_ATTR,
            MESSAGE_TIME_ATTR,
            MESSAGE_CONVERSATION_ATTR,
        ],
    );

    transaction.rollback().await.unwrap();
    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn migrated_optional_numeric_order_hints_preserve_nulls_cursors_pages_topics_and_fresh_rows()
{
    let Some(url) = instant_test_support::test_database_url(
        "migrated_optional_numeric_order_hints_preserve_nulls_cursors_pages_topics_and_fresh_rows",
    ) else {
        return;
    };
    let (mut client, driver) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let driver = tokio::spawn(driver);
    let tx = client.transaction().await.unwrap();
    assert_migrated_schema(&tx).await;
    seed_catalog(&tx).await;
    seed_triples(&tx).await;
    for (n, value) in [(900, None), (901, Some(Value::Null))] {
        let id = message_id(0, n);
        insert_triple(
            &tx,
            id,
            MESSAGE_ID_ATTR,
            json!(id),
            Flags::ea(),
            99_000,
            None,
        )
        .await;
        if let Some(value) = value {
            insert_triple(
                &tx,
                id,
                MESSAGE_TIME_ATTR,
                value,
                Flags::checked(),
                99_001,
                Some("number"),
            )
            .await;
        }
    }
    let catalog = catalog();
    for direction in ["asc", "desc"] {
        let query =
            json!({"messages":{"$":{"order":{"time":direction},"limit":3,"fields":["id","time"]}}});
        let first = assert_numeric_hint_query(&tx, &catalog, &query).await;
        for change in [
            json!({"offset":2}),
            json!({"last":3}),
            json!({"where":{"time":{"$gt":2}}}),
        ] {
            let mut variant = query.clone();
            if change.get("last").is_some() {
                variant["messages"]["$"]
                    .as_object_mut()
                    .unwrap()
                    .remove("limit");
            }
            variant["messages"]["$"]
                .as_object_mut()
                .unwrap()
                .extend(change.as_object().unwrap().clone());
            assert_numeric_hint_query(&tx, &catalog, &variant).await;
        }
        for boundary in ["before", "after"] {
            let cursor = first.page_info["messages"].end_cursor.as_ref().unwrap();
            let mut variant = query.clone();
            variant["messages"]["$"][boundary] = json!([
                cursor.entity_id,
                cursor.attr_id,
                cursor.value.as_ref().map(datalog_json),
                cursor.created_at
            ]);
            assert_numeric_hint_query(&tx, &catalog, &variant).await;
        }
    }
    let query =
        json!({"messages":{"$":{"order":{"time":"desc"},"limit":3,"fields":["id","time"]}}});
    let before = assert_numeric_hint_query(&tx, &catalog, &query).await;
    tx.execute("UPDATE triples SET value='999999'::jsonb,value_md5=md5('999999') WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3", &[&APP_ID,&message_id(0,0),&MESSAGE_TIME_ATTR]).await.unwrap();
    let after = assert_numeric_hint_query(&tx, &catalog, &query).await;
    assert_ne!(before.data, after.data);
    assert_eq!(after.data["messages"][0]["id"], json!(message_id(0, 0)));
    tx.rollback().await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
}

async fn assert_numeric_hint_query(
    tx: &Transaction<'_>,
    catalog: &AttributeCatalog,
    query: &Value,
) -> instant_instaql::InstaqlResponse {
    let compiled = compile_query(query, catalog, false).unwrap();
    let mut variants = Vec::new();
    for enabled in [None, Some(true), Some(false)] {
        let options = MatchCteOptions {
            optional_number_order_scan_hints: enabled,
            ensure_empty_symbol_values_for_topics: Some(true),
            ..MatchCteOptions::default()
        };
        let results = query_instaql(tx, &compiled, APP_ID, &options)
            .await
            .unwrap();
        variants.push((
            shape_response(&compiled, &results, catalog, false).unwrap(),
            collect_query_invalidation_topics(&compiled, &results).unwrap(),
        ));
    }
    assert_eq!(variants[0], variants[1]);
    assert_eq!(variants[0], variants[2]);
    variants.remove(0).0
}

#[tokio::test]
async fn migrated_small_page_jit_hint_is_statement_only_and_keeps_results_and_topics() {
    let Some(url) = instant_test_support::test_database_url(
        "migrated_small_page_jit_hint_is_statement_only_and_keeps_results_and_topics",
    ) else {
        return;
    };
    let (mut client, driver) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let driver = tokio::spawn(driver);
    let tx = client.transaction().await.unwrap();
    assert_migrated_schema(&tx).await;
    seed_catalog(&tx).await;
    seed_triples(&tx).await;
    tx.batch_execute("SET LOCAL jit=on; SET LOCAL jit_above_cost=0; SET LOCAL jit_inline_above_cost=-1; SET LOCAL jit_optimize_above_cost=-1;").await.unwrap();
    let jit_available = tx
        .query_one("SELECT pg_jit_available()", &[])
        .await
        .unwrap()
        .get::<_, bool>(0);
    if !jit_available {
        eprintln!(
            "pg_jit_available() is false; skipping JIT function counters for \
             migrated_small_page_jit_hint_is_statement_only_and_keeps_results_and_topics"
        );
    }
    let catalog = catalog();
    let compiled = compile_query(&json!({"shelves":{"$":{"where":{"name":{"$ne":"Fiction"}},"order":{"order":"desc"},"limit":2,"fields":["id","name","order"]}}}), &catalog, false).unwrap();
    let mut expected = None;
    let mut reference_plan = None;
    for enabled in [false, true, false] {
        let options = MatchCteOptions {
            small_page_jit_off: Some(enabled),
            optional_number_order_scan_hints: Some(false),
            ensure_empty_symbol_values_for_topics: Some(true),
            predicate_options: instant_datalog::PredicateOptions {
                forward_is_null_exists: true,
                ..instant_datalog::PredicateOptions::default()
            },
            ..MatchCteOptions::default()
        };
        let plan = plan_nested_query(&compiled, APP_ID, &options).unwrap();
        let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
        assert_eq!(
            rendered.sql.matches("Set(jit off)").count(),
            usize::from(enabled)
        );
        if let Some((ctes, groups, parameters)) = &reference_plan {
            assert_eq!(&plan.ctes, ctes);
            assert_eq!(&plan.groups, groups);
            assert_eq!(&rendered.parameters, parameters);
        } else {
            reference_plan = Some((
                plan.ctes.clone(),
                plan.groups.clone(),
                rendered.parameters.clone(),
            ));
        }
        let explain = instant_datalog::RenderedQuery {
            sql: format!("EXPLAIN (ANALYZE,FORMAT JSON) {}", rendered.sql),
            parameters: rendered.parameters,
        };
        let rows = query(&tx, &explain).await.unwrap();
        let Json(document) = rows[0].try_get::<_, Json<Value>>(0).unwrap();
        if jit_available {
            let functions = document[0]["JIT"]["Functions"].as_u64().unwrap_or(0);
            if enabled {
                assert_eq!(functions, 0);
            } else {
                assert!(functions > 0);
            }
        }
        let results = query_instaql(&tx, &compiled, APP_ID, &options)
            .await
            .unwrap();
        let output = (
            shape_response(&compiled, &results, &catalog, false).unwrap(),
            collect_query_invalidation_topics(&compiled, &results).unwrap(),
        );
        if let Some(expected) = &expected {
            assert_eq!(&output, expected);
        } else {
            expected = Some(output);
        }
        let jit = tx
            .query_one(
                "SELECT current_setting('jit'),current_setting('jit_above_cost')",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(jit.get::<_, String>(0), "on");
        assert_eq!(jit.get::<_, String>(1), "0");
    }
    tx.rollback().await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
}

#[tokio::test]
async fn migrated_forward_null_exists_preserves_missing_null_ne_links_pages_and_topics() {
    let Some(url) = instant_test_support::test_database_url(
        "migrated_forward_null_exists_preserves_missing_null_ne_links_pages_and_topics",
    ) else {
        return;
    };
    let (mut client, driver) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let driver = tokio::spawn(driver);
    let tx = client.transaction().await.unwrap();
    assert_migrated_schema(&tx).await;
    seed_catalog(&tx).await;
    seed_triples(&tx).await;
    for attr in [SHELF_NAME_ATTR, SHELF_ORDER_ATTR, HANDLE_ATTR, ACTIVE_ATTR] {
        let (one, two) = if [HANDLE_ATTR, ACTIVE_ATTR].contains(&attr) {
            (user_id(1), user_id(2))
        } else {
            (shelf_id(1), shelf_id(2))
        };
        tx.execute("UPDATE triples SET value='null'::jsonb,value_md5=md5('null') WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3", &[&APP_ID,&one,&attr]).await.unwrap();
        tx.execute(
            "DELETE FROM triples WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3",
            &[&APP_ID, &two, &attr],
        )
        .await
        .unwrap();
    }
    tx.execute("UPDATE triples SET value='\"Fiction\"'::jsonb,value_md5=md5('\"Fiction\"') WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3", &[&APP_ID,&shelf_id(3),&SHELF_NAME_ATTR]).await.unwrap();
    tx.execute(
        "DELETE FROM triples WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3",
        &[&APP_ID, &user_id(4), &SHELVES_ATTR],
    )
    .await
    .unwrap();
    let catalog = catalog();
    for (root, field) in [
        ("shelves", "name"),
        ("shelves", "order"),
        ("users", "handle"),
        ("users", "active"),
        ("users", "shelves"),
        ("shelves", "owner"),
    ] {
        for nil in [false, true] {
            let query = json!({root:{"$":{"where":{field:{"$isNull":nil}},"limit":10}}});
            let response = assert_null_exists_query(&tx, &catalog, &query).await;
            let count = response.data[root].as_array().unwrap().len();
            if field == "shelves" || field == "owner" {
                assert_eq!(
                    count,
                    if nil {
                        1
                    } else if field == "shelves" {
                        3
                    } else {
                        4
                    }
                );
            } else {
                assert_eq!(count, if nil || root == "users" { 2 } else { 3 });
            }
        }
    }
    let query = json!({"shelves":{"$":{"where":{"name":{"$ne":"Fiction"}},"order":{"order":"desc"},"limit":2,"fields":["id","name","order"]}}});
    assert_null_exists_pages(&tx, &catalog, &query).await;
    let first = assert_null_exists_query(&tx, &catalog, &query).await;
    assert_eq!(first.data["shelves"][0]["id"], json!(shelf_id(9)));
    let nested = json!({"users":{"$":{"where":{"shelves.name":{"$ne":"Fiction"}}},"shelves":{"$":{"where":{"name":{"$ne":"Fiction"}},"order":{"order":"asc"},"limit":2}}}});
    assert_null_exists_query(&tx, &catalog, &nested).await;
    tx.execute("UPDATE triples SET value='\"Fiction\"'::jsonb,value_md5=md5('\"Fiction\"') WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3", &[&APP_ID,&shelf_id(9),&SHELF_NAME_ATTR]).await.unwrap();
    let fresh = assert_null_exists_query(&tx, &catalog, &query).await;
    assert_ne!(first.data, fresh.data);
    assert_eq!(fresh.data["shelves"][0]["id"], json!(shelf_id(4)));
    tx.rollback().await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
}

async fn assert_null_exists_pages(tx: &Transaction<'_>, catalog: &AttributeCatalog, query: &Value) {
    for direction in ["asc", "desc"] {
        let mut query = query.clone();
        query["shelves"]["$"]["order"]["order"] = json!(direction);
        let first = assert_null_exists_query(tx, catalog, &query).await;
        for change in [json!({"offset":1}), json!({"last":2})] {
            let mut variant = query.clone();
            if change.get("last").is_some() {
                variant["shelves"]["$"]
                    .as_object_mut()
                    .unwrap()
                    .remove("limit");
            }
            variant["shelves"]["$"]
                .as_object_mut()
                .unwrap()
                .extend(change.as_object().unwrap().clone());
            assert_null_exists_query(tx, catalog, &variant).await;
        }
        for boundary in ["before", "after"] {
            let cursor = first.page_info["shelves"].end_cursor.as_ref().unwrap();
            let mut variant = query.clone();
            variant["shelves"]["$"][boundary] = json!([
                cursor.entity_id,
                cursor.attr_id,
                cursor.value.as_ref().map(datalog_json),
                cursor.created_at
            ]);
            assert_null_exists_query(tx, catalog, &variant).await;
        }
    }
}

async fn assert_null_exists_query(
    tx: &Transaction<'_>,
    catalog: &AttributeCatalog,
    query: &Value,
) -> instant_instaql::InstaqlResponse {
    let compiled = compile_query(query, catalog, false).unwrap();
    let mut variants = Vec::new();
    let mut original_plan = None;
    for enabled in [false, true, false] {
        let options = MatchCteOptions {
            predicate_options: instant_datalog::PredicateOptions {
                forward_is_null_exists: enabled,
                ..instant_datalog::PredicateOptions::default()
            },
            optional_number_order_scan_hints: Some(false),
            ensure_empty_symbol_values_for_topics: Some(true),
            ..MatchCteOptions::default()
        };
        let plan = plan_nested_query(&compiled, APP_ID, &options).unwrap();
        let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
        let metadata = serde_json::to_value(&plan.groups).unwrap();
        if let Some((parameters, groups)) = &original_plan {
            assert_eq!(&rendered.parameters, parameters);
            assert_eq!(&metadata, groups);
        } else {
            original_plan = Some((rendered.parameters, metadata));
        }
        let results = query_instaql(tx, &compiled, APP_ID, &options)
            .await
            .unwrap();
        variants.push((
            shape_response(&compiled, &results, catalog, false).unwrap(),
            collect_query_invalidation_topics(&compiled, &results).unwrap(),
        ));
    }
    assert_eq!(variants[0], variants[1]);
    assert_eq!(variants[0], variants[2]);
    variants.remove(0).0
}

#[tokio::test]
async fn migrated_postgres_preserves_two_count_roots_through_the_query_envelope() {
    let Some(database_url) = instant_test_support::test_database_url(
        "migrated_postgres_preserves_two_count_roots_through_the_query_envelope",
    ) else {
        return;
    };

    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let transaction = client.transaction().await.unwrap();
    assert_migrated_schema(&transaction).await;
    seed_catalog(&transaction).await;
    seed_triples(&transaction).await;

    let catalog = catalog();
    let compiled = compile_query(
        &json!({
            "users": {"$": {"aggregate": "count"}},
            "shelves": {"$": {"aggregate": "count"}}
        }),
        &catalog,
        true,
    )
    .unwrap();
    let plan = plan_nested_query(&compiled, APP_ID, &MatchCteOptions::default()).unwrap();
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    let rows = query(&transaction, &rendered).await.unwrap();
    assert_eq!(rows.len(), 1);
    let Json(envelope) = rows[0].try_get::<_, Json<Value>>(0).unwrap();
    let executed = support::decode_envelope(&plan, &envelope).unwrap();

    let counts = compiled
        .roots
        .iter()
        .zip(&executed)
        .map(|(root, result)| (root.key.as_str(), result.aggregate))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(counts.get("users"), Some(&Some(4)));
    assert_eq!(counts.get("shelves"), Some(&Some(5)));

    let response = shape_response(&compiled, &executed, &catalog, true).unwrap();
    assert_eq!(
        serde_json::to_value(response.aggregates).unwrap(),
        json!({
            "users": {"count": 4},
            "shelves": {"count": 5}
        })
    );

    transaction.rollback().await.unwrap();
    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "Opt-in local preparation microbenchmark, not a capacity acceptance gate"]
async fn benchmark_native_statement_reuse_on_migrated_nested_fixture() {
    struct Prepared(tokio_postgres::Statement);
    impl instant_pg::QueryStatementPreparer for Prepared {
        fn prepare(
            &self,
            _: &str,
        ) -> impl std::future::Future<
            Output = Result<Option<tokio_postgres::Statement>, tokio_postgres::Error>,
        > + Send {
            std::future::ready(Ok(Some(self.0.clone())))
        }
    }
    let Some(url) = instant_test_support::test_database_url(
        "benchmark_native_statement_reuse_on_migrated_nested_fixture",
    ) else {
        return;
    };
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/live_migrated_nested_instaql.json")).unwrap();
    let (mut client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let tx = client.transaction().await.unwrap();
    assert_migrated_schema(&tx).await;
    seed_catalog(&tx).await;
    seed_triples(&tx).await;
    let compiled = compile_query(&fixture.first_query, &catalog(), false).unwrap();
    let plan = plan_nested_query(&compiled, APP_ID, &MatchCteOptions::default()).unwrap();
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    assert!(
        rendered.sql.len() <= 64 * 1024,
        "benchmark must fit the proposed SQL admission bound"
    );
    let prepared = Prepared(tx.prepare(&rendered.sql).await.unwrap());
    let mut expected = None;
    let mut uncached = Vec::new();
    let mut reused = Vec::new();
    for cached in [false, true, true, false] {
        for i in 0..80 {
            let start = std::time::Instant::now();
            let rows = if cached {
                instant_pg::query_with_timeout_and_preparer(
                    &tx,
                    &rendered,
                    std::time::Duration::from_secs(30),
                    &prepared,
                )
                .await
                .unwrap()
            } else {
                query(&tx, &rendered).await.unwrap()
            };
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            let envelope = rows[0].get::<_, Json<Value>>(0).0;
            let decoded = support::decode_envelope(&plan, &envelope).unwrap();
            let shaped = shape_response(&compiled, &decoded, &catalog(), false).unwrap();
            let value = serde_json::to_value(shaped.data).unwrap();
            assert_eq!(value, fixture.expected_first);
            if let Some(expected) = &expected {
                assert_eq!(&value, expected);
            } else {
                expected = Some(value);
            }
            if i >= 16 {
                if cached {
                    reused.push(elapsed);
                } else {
                    uncached.push(elapsed);
                }
            }
        }
    }
    let mean = |samples: &[f64]| {
        samples.iter().sum::<f64>() / f64::from(u32::try_from(samples.len()).unwrap())
    };
    println!(
        "{}",
        json!({"sqlBytes":rendered.sql.len(),"callsPerProtocol":uncached.len(),"uncachedMeanMs":mean(&uncached),"preparedMeanMs":mean(&reused),"rowsEqual":true,"scope":"Local migrated synthetic nested-query fixture, ABBA blocks with 16 warmups per block; not WAN latency, five-app acceptance or a whole-project CPU prediction"})
    );
    tx.rollback().await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
}

async fn assert_migrated_schema(transaction: &Transaction<'_>) {
    let function = transaction
        .query_one(
            "SELECT to_regprocedure('public.json_uuid_to_uuid(jsonb)')::text",
            &[],
        )
        .await
        .unwrap()
        .get::<_, Option<String>>(0);
    assert_eq!(function.as_deref(), Some("json_uuid_to_uuid(jsonb)"));
    let table = transaction
        .query_one(
            "SELECT relpersistence::text FROM pg_class
             WHERE oid = 'public.triples'::regclass",
            &[],
        )
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(table, "p", "fixture must use the migrated permanent table");
}

async fn seed_catalog(transaction: &Transaction<'_>) {
    transaction
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&CREATOR_ID, &"rust-live-query@instant.test"],
        )
        .await
        .unwrap();
    transaction
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, $3)",
            &[&APP_ID, &CREATOR_ID, &"Rust live nested query"],
        )
        .await
        .unwrap();
    for seed in attr_seeds() {
        transaction
            .execute(
                "INSERT INTO attrs
                 (id, app_id, value_type, cardinality, is_unique, is_indexed,
                  forward_ident, reverse_ident, checked_data_type, is_required,
                  etype, label, reverse_etype, reverse_label)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                         $9::text::checked_data_type, $10, $11, $12, $13, $14)",
                &[
                    &seed.id,
                    &APP_ID,
                    &seed.value_type,
                    &seed.cardinality,
                    &seed.unique,
                    &seed.indexed,
                    &seed.forward_ident,
                    &seed.reverse_ident,
                    &seed.checked,
                    &seed.required,
                    &seed.etype,
                    &seed.label,
                    &seed.reverse_etype,
                    &seed.reverse_label,
                ],
            )
            .await
            .unwrap();
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "The fixture keeps all migrated triples together so its source corpus is auditable."
)]
async fn seed_triples(transaction: &Transaction<'_>) {
    let users = [
        (user_id(1), "alex", true, 1_000),
        (user_id(2), "nicole", false, 2_000),
        (user_id(3), "stopa", true, 3_000),
        (user_id(4), "zara", false, 4_000),
    ];
    for (id, handle, active, created_at) in users {
        insert_triple(
            transaction,
            id,
            USER_ID_ATTR,
            json!(id),
            Flags::ea(),
            created_at,
            None,
        )
        .await;
        insert_triple(
            transaction,
            id,
            HANDLE_ATTR,
            json!(handle),
            Flags::checked(),
            created_at + 1,
            Some("string"),
        )
        .await;
        insert_triple(
            transaction,
            id,
            ACTIVE_ATTR,
            json!(active),
            Flags::checked(),
            created_at + 2,
            Some("boolean"),
        )
        .await;
    }
    let shelves = [
        (user_id(1), shelf_id(9), "Essays", 9, 1_900),
        (user_id(1), shelf_id(1), "Nonfiction", 1, 1_100),
        (user_id(2), shelf_id(2), "Fiction", 2, 2_100),
        (user_id(3), shelf_id(3), "Nonfiction", 3, 3_100),
        (user_id(4), shelf_id(4), "Nonfiction", 4, 4_100),
    ];
    for (owner, id, name, order, created_at) in shelves {
        insert_triple(
            transaction,
            owner,
            SHELVES_ATTR,
            json!(id),
            Flags::reference(),
            created_at,
            None,
        )
        .await;
        insert_triple(
            transaction,
            id,
            SHELF_ID_ATTR,
            json!(id),
            Flags::ea(),
            created_at,
            None,
        )
        .await;
        insert_triple(
            transaction,
            id,
            SHELF_NAME_ATTR,
            json!(name),
            Flags::ea(),
            created_at + 1,
            None,
        )
        .await;
        insert_triple(
            transaction,
            id,
            SHELF_ORDER_ATTR,
            json!(order),
            Flags::checked(),
            created_at + 2,
            Some("number"),
        )
        .await;
    }

    for conversation_index in 0..3 {
        let conversation = conversation_id(conversation_index + 1);
        let created_at = 10_000 + i64::try_from(conversation_index).unwrap() * 100;
        insert_triple(
            transaction,
            conversation,
            CONVERSATION_ID_ATTR,
            json!(conversation),
            Flags::ea(),
            created_at,
            None,
        )
        .await;
        insert_triple(
            transaction,
            conversation,
            CONVERSATION_ORDER_ATTR,
            json!(conversation_index + 1),
            Flags::checked(),
            created_at + 1,
            Some("number"),
        )
        .await;

        for message_index in 0..20 {
            let message = message_id(conversation_index, message_index);
            insert_triple(
                transaction,
                message,
                MESSAGE_ID_ATTR,
                json!(message),
                Flags::ea(),
                created_at + 10 + i64::try_from(message_index).unwrap() * 3,
                None,
            )
            .await;
            insert_triple(
                transaction,
                message,
                MESSAGE_TIME_ATTR,
                json!(conversation_index + message_index),
                Flags::checked(),
                created_at + 11 + i64::try_from(message_index).unwrap() * 3,
                Some("number"),
            )
            .await;
            insert_triple(
                transaction,
                message,
                MESSAGE_CONVERSATION_ATTR,
                json!(conversation),
                Flags::reference(),
                created_at + 12 + i64::try_from(message_index).unwrap() * 3,
                None,
            )
            .await;
        }
    }
}

#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // Mirrors the five physical triples index flags.
struct Flags {
    ea: bool,
    eav: bool,
    av: bool,
    ave: bool,
    vae: bool,
}

impl Flags {
    const fn ea() -> Self {
        Self {
            ea: true,
            eav: false,
            av: false,
            ave: false,
            vae: false,
        }
    }

    const fn checked() -> Self {
        Self {
            ea: true,
            eav: false,
            av: false,
            ave: true,
            vae: false,
        }
    }

    const fn reference() -> Self {
        Self {
            ea: false,
            eav: true,
            av: false,
            ave: false,
            vae: true,
        }
    }
}

async fn insert_triple(
    transaction: &Transaction<'_>,
    entity: Uuid,
    attribute: Uuid,
    value: Value,
    flags: Flags,
    created_at: i64,
    checked: Option<&str>,
) {
    transaction
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, value_md5,
              ea, eav, av, ave, vae, created_at, checked_data_type)
             VALUES ($1, $2, $3, $4, md5(($4::jsonb)::text),
                     $5, $6, $7, $8, $9, $10, $11::text::checked_data_type)",
            &[
                &APP_ID,
                &entity,
                &attribute,
                &Json(value),
                &flags.ea,
                &flags.eav,
                &flags.av,
                &flags.ave,
                &flags.vae,
                &created_at,
                &checked,
            ],
        )
        .await
        .unwrap();
}

fn assert_exact_topic_attributes(
    query: &instant_instaql::CompiledQuery,
    results: &[instant_instaql::ExecutedNode],
    expected: &[Uuid],
) {
    let topics = collect_query_invalidation_topics(query, results).unwrap();
    let actual = topics
        .iter()
        .flat_map(|topic| match &topic.attribute {
            TopicPart::Values(values) => values
                .iter()
                .filter_map(|value| match value {
                    DatalogValue::Uuid(value) => Some(*value),
                    DatalogValue::Json(_) => None,
                })
                .collect::<Vec<_>>(),
            TopicPart::Any | TopicPart::Not(_) => Vec::new(),
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected.iter().copied().collect::<BTreeSet<_>>());
}

fn datalog_json(value: &DatalogValue) -> Value {
    match value {
        DatalogValue::Json(value) => value.clone(),
        DatalogValue::Uuid(value) => json!(value),
    }
}

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: attr_seeds()
            .into_iter()
            .map(|seed| Attribute {
                id: seed.id,
                wire: None,
                etype: seed.etype.to_owned(),
                label: seed.label.to_owned(),
                value_type: if seed.value_type == "ref" {
                    AttributeValueType::Ref
                } else {
                    AttributeValueType::Blob
                },
                cardinality: if seed.cardinality == "many" {
                    Cardinality::Many
                } else {
                    Cardinality::One
                },
                unique: seed.unique,
                setting_unique: false,
                indexed: seed.indexed,
                required: seed.required,
                checked_data_type: seed.checked.map(|value| match value {
                    "string" => DataType::String,
                    "boolean" => DataType::Boolean,
                    "number" => DataType::Number,
                    _ => unreachable!(),
                }),
                indexing: false,
                checking_data_type: false,
                reverse_etype: seed.reverse_etype.map(str::to_owned),
                reverse_label: seed.reverse_label.map(str::to_owned),
                on_delete: None,
                on_delete_reverse: None,
            })
            .collect(),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "The fixture's source-pinned catalog is easiest to audit as one ordered definition."
)]
fn attr_seeds() -> Vec<AttrSeed> {
    vec![
        attr_seed(
            USER_ID_ATTR,
            1,
            "blob",
            "one",
            true,
            true,
            None,
            true,
            "users",
            "id",
        ),
        attr_seed(
            HANDLE_ATTR,
            2,
            "blob",
            "one",
            false,
            true,
            Some("string"),
            false,
            "users",
            "handle",
        ),
        attr_seed(
            ACTIVE_ATTR,
            3,
            "blob",
            "one",
            false,
            true,
            Some("boolean"),
            false,
            "users",
            "active",
        ),
        AttrSeed {
            reverse_ident: Some(ident(8)),
            reverse_etype: Some("shelves"),
            reverse_label: Some("owner"),
            ..attr_seed(
                SHELVES_ATTR,
                4,
                "ref",
                "many",
                false,
                false,
                None,
                false,
                "users",
                "shelves",
            )
        },
        attr_seed(
            SHELF_ID_ATTR,
            5,
            "blob",
            "one",
            true,
            true,
            None,
            true,
            "shelves",
            "id",
        ),
        attr_seed(
            SHELF_NAME_ATTR,
            6,
            "blob",
            "one",
            false,
            false,
            None,
            false,
            "shelves",
            "name",
        ),
        attr_seed(
            SHELF_ORDER_ATTR,
            7,
            "blob",
            "one",
            false,
            true,
            Some("number"),
            false,
            "shelves",
            "order",
        ),
        attr_seed(
            CONVERSATION_ID_ATTR,
            9,
            "blob",
            "one",
            true,
            true,
            None,
            true,
            "conversations",
            "id",
        ),
        attr_seed(
            CONVERSATION_ORDER_ATTR,
            10,
            "blob",
            "one",
            true,
            true,
            Some("number"),
            false,
            "conversations",
            "order",
        ),
        attr_seed(
            MESSAGE_ID_ATTR,
            11,
            "blob",
            "one",
            true,
            true,
            None,
            true,
            "messages",
            "id",
        ),
        attr_seed(
            MESSAGE_TIME_ATTR,
            12,
            "blob",
            "one",
            false,
            true,
            Some("number"),
            false,
            "messages",
            "time",
        ),
        AttrSeed {
            reverse_ident: Some(ident(14)),
            reverse_etype: Some("conversations"),
            reverse_label: Some("messages"),
            ..attr_seed(
                MESSAGE_CONVERSATION_ATTR,
                13,
                "ref",
                "one",
                false,
                false,
                None,
                false,
                "messages",
                "conversation",
            )
        },
    ]
}

#[allow(clippy::too_many_arguments)]
const fn attr_seed(
    id: Uuid,
    ident_index: u128,
    value_type: &'static str,
    cardinality: &'static str,
    unique: bool,
    indexed: bool,
    checked: Option<&'static str>,
    required: bool,
    etype: &'static str,
    label: &'static str,
) -> AttrSeed {
    AttrSeed {
        id,
        forward_ident: ident(ident_index),
        value_type,
        cardinality,
        unique,
        indexed,
        checked,
        required,
        etype,
        label,
        reverse_ident: None,
        reverse_etype: None,
        reverse_label: None,
    }
}

const fn ident(index: u128) -> Uuid {
    Uuid::from_u128(0xf1000000_0000_4000_8000_000000000000 + index)
}

const fn user_id(index: u128) -> Uuid {
    Uuid::from_u128(0xb1000000_0000_4000_8000_000000000000 + index)
}

const fn shelf_id(index: u128) -> Uuid {
    Uuid::from_u128(0xc1000000_0000_4000_8000_000000000000 + index)
}

const fn conversation_id(index: usize) -> Uuid {
    Uuid::from_u128(0xc2000000_0000_4000_8000_000000000000 + index as u128)
}

const fn message_id(conversation_index: usize, message_index: usize) -> Uuid {
    Uuid::from_u128(
        0xc3000000_0000_4000_8000_000000000000
            + (conversation_index as u128) * 100
            + message_index as u128,
    )
}
