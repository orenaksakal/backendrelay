use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use instant_datalog::{
    Clause, DataType, DatalogResult, DatalogValue, Index, IndexKey, MatchCteOptions,
    MatchQueryContext, MatchQueryPlan, NormalizedClause, OrderDirection, PageCursor, PageInfo,
    PageOrderType, RawPattern, RawTerm, SqlParameter, Symbol, TopicPart, plan_match_query,
    render_batch_match_query, render_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query, compile_roots,
    shape_response,
};
use instant_pg::{
    DatalogBatchRequest, MixedDatalogBatchRequest, MixedDatalogBatchResult, PgQueryError,
    execute_datalog_requests, execute_instaql_requests, execute_mixed_datalog_requests,
    explain_instaql, mixed_datalog_batch_loader, query_datalog, query_datalog_batch,
    query_datalog_count, query_instaql, query_instaql_with_timeout, query_with_timeout,
};
use serde::Deserialize;
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

fn normalized_clauses(attr_id: Uuid) -> Vec<NormalizedClause> {
    let clauses: Vec<Clause> = serde_json::from_value(json!([{
        "kind": "pattern",
        "pattern": {
            "index": {"key": "ea"},
            "components": [
                {"kind": "variable", "value": "?entity"},
                {"kind": "constant", "value": attr_id},
                {"kind": "variable", "value": "?title"}
            ]
        }
    }]))
    .unwrap();
    clauses
        .iter()
        .map(Clause::normalize)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn match_plan(prefix: &str, app_id: Uuid, attr_id: Uuid) -> MatchQueryPlan {
    let clauses = normalized_clauses(attr_id);
    plan_match_query(
        &MatchQueryContext {
            prefix,
            app_id,
            additional_joins: &HashMap::new(),
            initial_symbol_cardinalities: &HashMap::new(),
            options: &MatchCteOptions::default(),
        },
        &clauses,
    )
    .unwrap()
}

#[derive(Deserialize)]
struct ProductionResultFixture {
    app_id: Uuid,
    cases: Vec<ProductionResultCase>,
}

#[derive(Deserialize)]
struct ProductionResultCase {
    name: String,
    clauses: Vec<Clause>,
    #[serde(default)]
    expected_entity_lookup: bool,
    rows: Vec<ProductionFixtureRow>,
    expected: serde_json::Value,
}

#[derive(Deserialize)]
struct ProductionFixtureRow {
    triples: Vec<ProductionFixtureTriple>,
}

#[derive(Deserialize)]
struct ProductionFixtureTriple {
    entity: Uuid,
    attribute: Uuid,
    value: serde_json::Value,
    reference: bool,
    created_at: i64,
}

fn canonical_datalog_result(result: &DatalogResult) -> serde_json::Value {
    let symbols = result
        .symbol_values
        .iter()
        .map(|(symbol, values)| {
            (
                symbol.as_str().to_owned(),
                serde_json::Value::Array(values.iter().map(canonical_datalog_value).collect()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    json!({
        "topics": result.topics.iter().map(|topic| json!({
            "indexes": topic.indexes,
            "entity": canonical_topic_part(&topic.entity),
            "attribute": canonical_topic_part(&topic.attribute),
            "value": canonical_topic_part(&topic.value),
        })).collect::<Vec<_>>(),
        "symbol_values": symbols,
        "join_rows": result.join_rows.iter().map(|row| row.iter().map(|triple| json!({
            "entity": triple.entity_id,
            "attribute": triple.attr_id,
            "value": canonical_datalog_value(&triple.value),
            "created_at": triple.created_at,
        })).collect::<Vec<_>>()).collect::<Vec<_>>(),
    })
}

fn canonical_topic_part(part: &TopicPart) -> serde_json::Value {
    match part {
        TopicPart::Any => json!({"kind": "any"}),
        TopicPart::Values(values) => json!({
            "kind": "values",
            "values": values.iter().map(canonical_datalog_value).collect::<Vec<_>>()
        }),
        TopicPart::Not(value) => {
            json!({"kind": "not", "value": canonical_datalog_value(value)})
        }
    }
}

fn canonical_datalog_value(value: &DatalogValue) -> serde_json::Value {
    match value {
        DatalogValue::Uuid(value) => json!({"kind": "uuid", "value": value}),
        DatalogValue::Json(value) => json!({"kind": "json", "value": value}),
    }
}

async fn insert_triple(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    value: &str,
    created_at: i64,
) {
    client
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, ea, eav, av, ave, vae, created_at)
             VALUES ($1, $2, $3, $4, true, false, false, false, false, $5)",
            &[
                &app_id,
                &entity_id,
                &attr_id,
                &tokio_postgres::types::Json(json!(value)),
                &created_at,
            ],
        )
        .await
        .unwrap();
}

async fn insert_checked_string(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    value: &str,
    created_at: i64,
) {
    insert_checked_value(
        client,
        app_id,
        entity_id,
        attr_id,
        json!(value),
        DataType::String,
        created_at,
    )
    .await;
}

async fn insert_checked_value(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    value: serde_json::Value,
    data_type: DataType,
    created_at: i64,
) {
    let data_type = match data_type {
        DataType::String => "string",
        DataType::Number => "number",
        DataType::Boolean => "boolean",
        DataType::Date => "date",
    };
    client
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, ea, eav, av, ave, vae, created_at, checked_data_type)
             VALUES ($1, $2, $3, $4, true, false, false, true, false, $5,
                     $6::text::checked_data_type)",
            &[
                &app_id,
                &entity_id,
                &attr_id,
                &tokio_postgres::types::Json(value),
                &created_at,
                &data_type,
            ],
        )
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn checked_page_request(
    app_id: Uuid,
    base_attr: Uuid,
    order_attr: Uuid,
    order_type: DataType,
    direction: OrderDirection,
    before: Option<PageCursor>,
    before_inclusive: bool,
    after: Option<PageCursor>,
    after_inclusive: bool,
) -> DatalogBatchRequest {
    let order_symbol = Symbol::new("?checked-order").unwrap();
    DatalogBatchRequest {
        app_id,
        clauses: normalized_clauses(base_attr),
        options: MatchCteOptions::default(),
        page: Some(PageInfo {
            limit: Some(10),
            last: false,
            offset: None,
            direction,
            order_symbol: order_symbol.clone(),
            order_type: PageOrderType::Checked(order_type),
            order_required: true,
            attr_id: order_attr,
            pattern: RawPattern::new(
                Index::checked(IndexKey::Ave, order_type),
                vec![
                    RawTerm::Variable(Symbol::new("?entity").unwrap()),
                    RawTerm::Constant(json!(order_attr)),
                    RawTerm::Variable(order_symbol),
                ],
            )
            .normalize()
            .unwrap(),
            before,
            before_inclusive,
            after,
            after_inclusive,
        }),
    }
}

fn result_entities(result: &DatalogResult) -> Vec<Uuid> {
    result
        .join_rows
        .iter()
        .map(|row| row[0].entity_id)
        .collect()
}

async fn insert_reference(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    target_id: Uuid,
    created_at: i64,
) {
    client
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, ea, eav, av, ave, vae, created_at)
             VALUES ($1, $2, $3, $4, true, true, false, false, true, $5)",
            &[
                &app_id,
                &entity_id,
                &attr_id,
                &tokio_postgres::types::Json(json!(target_id)),
                &created_at,
            ],
        )
        .await
        .unwrap();
}

fn assert_direct_result(
    result: &DatalogResult,
    entity_id: Uuid,
    attr_id: Uuid,
    value: &str,
    created_at: i64,
) {
    assert_eq!(result.join_rows.len(), 1);
    assert_eq!(result.join_rows[0][0].entity_id, entity_id);
    assert_eq!(result.join_rows[0][0].attr_id, attr_id);
    assert_eq!(
        result.join_rows[0][0].value,
        DatalogValue::Json(json!(value))
    );
    assert_eq!(result.join_rows[0][0].created_at, created_at);
    assert_eq!(
        result.symbol_values[&Symbol::new("?entity").unwrap()],
        vec![DatalogValue::Uuid(entity_id)]
    );
    assert_eq!(
        result.symbol_values[&Symbol::new("?title").unwrap()],
        vec![DatalogValue::Json(json!(value))]
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One loop proves every pinned case through both SQL transports.
async fn source_pinned_production_result_corpus_executes_against_postgres() {
    let Some(database_url) = instant_test_support::test_database_url(
        "source_pinned_production_result_corpus_executes_against_postgres",
    ) else {
        return;
    };
    let fixture: ProductionResultFixture = serde_json::from_str(include_str!(
        "../../instant-datalog/tests/fixtures/production_query_result_differential.json"
    ))
    .unwrap();
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TEMP TABLE triples (
                app_id uuid NOT NULL,
                entity_id uuid NOT NULL,
                attr_id uuid NOT NULL,
                value jsonb NOT NULL,
                ea boolean NOT NULL,
                eav boolean NOT NULL,
                av boolean NOT NULL,
                ave boolean NOT NULL,
                vae boolean NOT NULL,
                created_at bigint NOT NULL,
                checked_data_type checked_data_type
             );",
        )
        .await
        .unwrap();

    assert!(
        fixture.cases.len() >= 4,
        "the complete source-pinned production-result corpus must remain present"
    );
    for (index, case) in fixture.cases.into_iter().enumerate() {
        client.batch_execute("TRUNCATE triples").await.unwrap();
        if case.expected_entity_lookup {
            client
                .execute(
                    "INSERT INTO triples
                     (app_id, entity_id, attr_id, value, ea, eav, av, ave, vae, created_at)
                     VALUES ($1, $2, $3, $4, true, true, true, true, false, 0)",
                    &[
                        &fixture.app_id,
                        &uuid("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee"),
                        &uuid("77777777-7777-4777-8777-777777777777"),
                        &tokio_postgres::types::Json(json!("alex")),
                    ],
                )
                .await
                .unwrap();
        }
        let mut inserted = HashSet::new();
        for triple in case.rows.iter().flat_map(|row| &row.triples) {
            let key = json!([
                triple.entity,
                triple.attribute,
                triple.value,
                triple.reference,
                triple.created_at
            ])
            .to_string();
            if !inserted.insert(key) {
                continue;
            }
            client
                .execute(
                    "INSERT INTO triples
                     (app_id, entity_id, attr_id, value, ea, eav, av, ave, vae, created_at)
                     VALUES ($1, $2, $3, $4, true, $5, false, false, $5, $6)",
                    &[
                        &fixture.app_id,
                        &triple.entity,
                        &triple.attribute,
                        &tokio_postgres::types::Json(triple.value.clone()),
                        &triple.reference,
                        &triple.created_at,
                    ],
                )
                .await
                .unwrap();
        }
        let clauses = case
            .clauses
            .iter()
            .map(Clause::normalize)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let additional_joins = HashMap::new();
        let symbol_cardinalities = HashMap::new();
        let options = MatchCteOptions::default();
        let prefix = format!("production-live-{index}-");
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: &prefix,
                app_id: fixture.app_id,
                additional_joins: &additional_joins,
                initial_symbol_cardinalities: &symbol_cardinalities,
                options: &options,
            },
            &clauses,
        )
        .unwrap();
        let direct_query = render_match_query(&plan).unwrap();
        let direct = query_datalog(&client, &direct_query, &plan).await.unwrap();
        let batch_query = render_batch_match_query(std::slice::from_ref(&plan)).unwrap();
        let batch = query_datalog_batch(&client, &batch_query, std::slice::from_ref(&plan))
            .await
            .unwrap()
            .remove(0);

        assert_eq!(direct, batch, "{} direct/batch drift", case.name);
        assert_eq!(
            canonical_datalog_result(&direct),
            case.expected,
            "{}",
            case.name
        );
    }

    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One transaction exercises direct, batch, loader, and page paths.
async fn planned_query_executes_and_decodes_against_postgres() {
    let Some(database_url) = instant_test_support::test_database_url(
        "planned_query_executes_and_decodes_against_postgres",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TEMP TABLE triples (
                app_id uuid NOT NULL,
                entity_id uuid NOT NULL,
                attr_id uuid NOT NULL,
                value jsonb NOT NULL,
                ea boolean NOT NULL,
                eav boolean NOT NULL,
                av boolean NOT NULL,
                ave boolean NOT NULL,
                vae boolean NOT NULL,
                created_at bigint NOT NULL,
                checked_data_type checked_data_type
            );
             CREATE TEMP TABLE attr_sketches (
                app_id uuid NOT NULL,
                attr_id uuid NOT NULL,
                total bigint NOT NULL,
                width integer,
                depth integer,
                total_not_binned bigint,
                bins bytea,
                reverse_width integer,
                reverse_depth integer,
                reverse_total bigint,
                reverse_bins bytea
            )",
        )
        .await
        .unwrap();

    let app_id = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let entity_id = uuid("11111111-1111-1111-1111-111111111111");
    let attr_id = uuid("22222222-2222-2222-2222-222222222222");
    insert_triple(&client, app_id, entity_id, attr_id, "RoboCop", 1234).await;
    let plan = match_plan("match-0-", app_id, attr_id);
    let query = render_match_query(&plan).unwrap();
    assert!(matches!(query.parameters[0], SqlParameter::Uuid(value) if value == app_id));

    let result = query_datalog(&client, &query, &plan).await.unwrap();
    assert_direct_result(&result, entity_id, attr_id, "RoboCop", 1234);
    assert_eq!(query_datalog_count(&client, &plan).await.unwrap(), 1);

    let handle_attr = uuid("99999999-9999-9999-9999-999999999999");
    insert_checked_string(&client, app_id, entity_id, handle_attr, "RoboCop", 1235).await;
    let catalog = AttributeCatalog {
        attributes: vec![
            Attribute {
                id: attr_id,
                wire: None,
                etype: "users".to_owned(),
                label: "id".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: true,
                setting_unique: false,
                indexed: true,
                required: true,
                checked_data_type: None,
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            },
            Attribute {
                id: handle_attr,
                wire: None,
                etype: "users".to_owned(),
                label: "handle".to_owned(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: false,
                setting_unique: false,
                indexed: true,
                required: false,
                checked_data_type: Some(instant_datalog::DataType::String),
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            },
        ],
    };
    let compiled = compile_roots(
        &json!({"users": {"$": {"where": {"handle": "RoboCop"}}}}),
        &catalog,
        false,
    )
    .unwrap();
    let explain_query = compile_query(
        &json!({"users": {"$": {"where": {"handle": "RoboCop"}}}}),
        &catalog,
        false,
    )
    .unwrap();
    let explain = explain_instaql(
        &client,
        &explain_query,
        app_id,
        &MatchCteOptions::default(),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(explain.time_ms >= 0.0);
    assert!(explain.execution_time_ms >= 0.0);
    assert!(explain.planning_time_ms >= 0.0);
    let additional = HashMap::new();
    let options = MatchCteOptions::default();
    let where_context = MatchQueryContext {
        prefix: "where-live-",
        app_id,
        additional_joins: &additional,
        initial_symbol_cardinalities: &HashMap::new(),
        options: &options,
    };
    let where_plan = compiled[0].plan(&where_context).unwrap();
    let where_query = render_match_query(&where_plan).unwrap();
    let where_result = query_datalog(&client, &where_query, &where_plan)
        .await
        .unwrap();
    assert_eq!(where_result.join_rows[0][0].entity_id, entity_id);
    assert_eq!(
        where_result.join_rows[0][0].value,
        DatalogValue::Json(json!("RoboCop"))
    );

    let missing_handle_entity = uuid("12121212-1212-1212-1212-121212121212");
    insert_triple(
        &client,
        app_id,
        missing_handle_entity,
        attr_id,
        "No handle",
        1200,
    )
    .await;
    let order_symbol = Symbol::new("?handle-order").unwrap();
    let optional_order_pattern = RawPattern::new(
        Index::checked(IndexKey::Ave, instant_datalog::DataType::String),
        vec![
            RawTerm::Variable(Symbol::new("?entity").unwrap()),
            RawTerm::Constant(json!(handle_attr)),
            RawTerm::Variable(order_symbol.clone()),
        ],
    )
    .normalize()
    .unwrap();
    let missing_order_page = execute_datalog_requests(
        &client,
        &[DatalogBatchRequest {
            app_id,
            clauses: normalized_clauses(attr_id),
            options: MatchCteOptions::default(),
            page: Some(PageInfo {
                limit: Some(10),
                last: false,
                offset: None,
                direction: OrderDirection::Asc,
                order_symbol,
                order_type: PageOrderType::Checked(instant_datalog::DataType::String),
                order_required: false,
                attr_id: handle_attr,
                pattern: optional_order_pattern,
                before: None,
                before_inclusive: false,
                after: None,
                after_inclusive: false,
            }),
        }],
    )
    .await
    .unwrap();
    let page_info = missing_order_page[0].page_info.as_ref().unwrap();
    let missing_cursor = [
        page_info.start_cursor.as_ref(),
        page_info.end_cursor.as_ref(),
    ]
    .into_iter()
    .flatten()
    .find(|cursor| cursor.entity_id == missing_handle_entity)
    .expect("the page boundary should retain the entity missing the order attribute");
    assert_eq!(missing_cursor.value, None);
    assert_eq!(missing_cursor.created_at, 1200);

    let second_app_id = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let second_entity_id = uuid("33333333-3333-3333-3333-333333333333");
    let second_attr_id = uuid("44444444-4444-4444-4444-444444444444");
    insert_triple(
        &client,
        second_app_id,
        second_entity_id,
        second_attr_id,
        "Predator",
        5678,
    )
    .await;
    let second_plan = match_plan("match-1-", second_app_id, second_attr_id);
    let batch_query = render_batch_match_query(&[plan.clone(), second_plan.clone()]).unwrap();
    let batch_results =
        query_datalog_batch(&client, &batch_query, &[plan.clone(), second_plan.clone()])
            .await
            .unwrap();
    assert_eq!(batch_results.len(), 2);
    assert_eq!(batch_results[0].join_rows[0][0].entity_id, entity_id);
    assert_eq!(batch_results[1].join_rows[0][0].entity_id, second_entity_id);
    assert_eq!(
        batch_results[1].join_rows[0][0].value,
        DatalogValue::Json(json!("Predator"))
    );

    let loader_results = execute_datalog_requests(
        &client,
        &[
            DatalogBatchRequest {
                app_id,
                clauses: normalized_clauses(attr_id),
                options: MatchCteOptions::default(),
                page: None,
            },
            DatalogBatchRequest {
                app_id: second_app_id,
                clauses: normalized_clauses(second_attr_id),
                options: MatchCteOptions::default(),
                page: None,
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(loader_results[0].join_rows[0][0].entity_id, entity_id);
    assert_eq!(
        loader_results[1].join_rows[0][0].entity_id,
        second_entity_id
    );
    let mixed_results = execute_mixed_datalog_requests(
        &client,
        &[
            MixedDatalogBatchRequest::Rows(DatalogBatchRequest {
                app_id,
                clauses: normalized_clauses(attr_id),
                options: MatchCteOptions::default(),
                page: None,
            }),
            MixedDatalogBatchRequest::Count(DatalogBatchRequest {
                app_id: second_app_id,
                clauses: normalized_clauses(second_attr_id),
                options: MatchCteOptions::default(),
                page: None,
            }),
        ],
    )
    .await
    .unwrap();
    assert!(matches!(
        &mixed_results[0],
        MixedDatalogBatchResult::Rows(result)
            if result.join_rows[0][0].entity_id == entity_id
    ));
    assert_eq!(mixed_results[1], MixedDatalogBatchResult::Count(1));
    assert_eq!(
        execute_mixed_datalog_requests(
            &client,
            &[MixedDatalogBatchRequest::Count(DatalogBatchRequest {
                app_id,
                clauses: normalized_clauses(attr_id),
                options: MatchCteOptions::default(),
                page: None,
            })],
        )
        .await
        .unwrap(),
        vec![MixedDatalogBatchResult::Count(2)]
    );
    let single_loader_result = execute_datalog_requests(
        &client,
        &[DatalogBatchRequest {
            app_id,
            clauses: normalized_clauses(attr_id),
            options: MatchCteOptions::default(),
            page: None,
        }],
    )
    .await
    .unwrap();
    assert_eq!(single_loader_result[0].join_rows[0][0].entity_id, entity_id);

    let later_entity_id = uuid("55555555-5555-5555-5555-555555555555");
    insert_triple(&client, app_id, later_entity_id, attr_id, "Arrival", 2345).await;
    let order_symbol = Symbol::new("?created").unwrap();
    let page_pattern = RawPattern::new(
        Index::new(IndexKey::Ea),
        vec![
            RawTerm::Variable(Symbol::new("?entity").unwrap()),
            RawTerm::Constant(json!(attr_id)),
            RawTerm::Any,
            RawTerm::Variable(order_symbol.clone()),
        ],
    )
    .normalize()
    .unwrap();
    let paged = execute_datalog_requests(
        &client,
        &[DatalogBatchRequest {
            app_id,
            clauses: normalized_clauses(attr_id),
            options: MatchCteOptions::default(),
            page: Some(PageInfo {
                limit: Some(1),
                last: false,
                offset: None,
                direction: OrderDirection::Asc,
                order_symbol,
                order_type: PageOrderType::CreatedAtTimestamp,
                order_required: true,
                attr_id,
                pattern: page_pattern,
                before: None,
                before_inclusive: false,
                after: None,
                after_inclusive: false,
            }),
        }],
    )
    .await
    .unwrap();
    let page_info = paged[0].page_info.as_ref().unwrap();
    assert_eq!(
        page_info.start_cursor.as_ref().unwrap().entity_id,
        missing_handle_entity
    );
    assert_eq!(page_info.end_cursor.as_ref().unwrap().created_at, 1200);
    assert!(page_info.has_next_page);
    assert!(!page_info.has_previous_page);

    let nested_app_id = uuid("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee");
    let user_id = uuid("aaaaaaaa-1111-1111-1111-111111111111");
    let shelf_id = uuid("bbbbbbbb-1111-1111-1111-111111111111");
    let user_id_attr = uuid("aaaaaaaa-2222-2222-2222-222222222222");
    let nested_handle_attr = uuid("aaaaaaaa-3333-3333-3333-333333333333");
    let shelves_attr = uuid("aaaaaaaa-4444-4444-4444-444444444444");
    let shelf_id_attr = uuid("bbbbbbbb-2222-2222-2222-222222222222");
    let shelf_name_attr = uuid("bbbbbbbb-3333-3333-3333-333333333333");
    insert_triple(
        &client,
        nested_app_id,
        user_id,
        user_id_attr,
        &user_id.to_string(),
        3000,
    )
    .await;
    insert_checked_string(
        &client,
        nested_app_id,
        user_id,
        nested_handle_attr,
        "alex",
        3001,
    )
    .await;
    insert_reference(
        &client,
        nested_app_id,
        user_id,
        shelves_attr,
        shelf_id,
        3002,
    )
    .await;
    let stored_reference = client
        .query_one(
            "SELECT json_uuid_to_uuid(value) FROM triples
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&nested_app_id, &user_id, &shelves_attr],
        )
        .await
        .unwrap()
        .get::<_, Option<Uuid>>(0);
    assert_eq!(stored_reference, Some(shelf_id));
    insert_triple(
        &client,
        nested_app_id,
        shelf_id,
        shelf_id_attr,
        &shelf_id.to_string(),
        4000,
    )
    .await;
    insert_triple(
        &client,
        nested_app_id,
        shelf_id,
        shelf_name_attr,
        "Nonfiction",
        4001,
    )
    .await;
    let nested_catalog = AttributeCatalog {
        attributes: vec![
            attribute(user_id_attr, "users", "id", AttributeValueType::Blob),
            Attribute {
                checked_data_type: Some(instant_datalog::DataType::String),
                indexed: true,
                ..attribute(
                    nested_handle_attr,
                    "users",
                    "handle",
                    AttributeValueType::Blob,
                )
            },
            Attribute {
                cardinality: Cardinality::Many,
                reverse_etype: Some("shelves".to_owned()),
                reverse_label: Some("owner".to_owned()),
                ..attribute(shelves_attr, "users", "shelves", AttributeValueType::Ref)
            },
            attribute(shelf_id_attr, "shelves", "id", AttributeValueType::Blob),
            attribute(shelf_name_attr, "shelves", "name", AttributeValueType::Blob),
        ],
    };
    let nested_query = compile_query(
        &json!({
            "users": {
                "$": {"where": {"handle": "alex"}},
                "shelves": {},
                "zippy": {}
            },
            "zippy": {}
        }),
        &nested_catalog,
        false,
    )
    .unwrap();
    let nested_results = query_instaql(
        &client,
        &nested_query,
        nested_app_id,
        &MatchCteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(nested_results.len(), 2);
    let users_index = nested_query
        .roots
        .iter()
        .position(|root| root.key == "users")
        .unwrap();
    let shelves_index = nested_query.roots[users_index]
        .children
        .iter()
        .position(|child| child.key == "shelves")
        .unwrap();
    let users_result = &nested_results[users_index];
    assert_eq!(users_result.entities.len(), 1);
    assert_eq!(users_result.entities[0].children.len(), 2);
    assert_eq!(
        users_result.entities[0].children[shelves_index]
            .entities
            .len(),
        1,
        "{nested_results:#?}"
    );
    let response = shape_response(&nested_query, &nested_results, &nested_catalog, true).unwrap();
    assert_eq!(response.data["users"][0]["handle"], "alex");
    assert_eq!(
        response.data["users"][0]["shelves"][0]["name"],
        "Nonfiction"
    );
    assert_eq!(response.data["users"][0]["zippy"], json!([]));
    assert_eq!(response.data["zippy"], json!([]));

    let nested_batch = execute_instaql_requests(
        &client,
        &[
            nested_query.clone(),
            instant_instaql::CompiledQuery { roots: Vec::new() },
            nested_query.clone(),
        ],
        nested_app_id,
        &MatchCteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(nested_batch.len(), 3);
    assert_eq!(nested_batch[0], nested_results);
    assert!(nested_batch[1].is_empty());
    assert_eq!(nested_batch[2], nested_results);

    let client = Arc::new(client);
    let loader = mixed_datalog_batch_loader(Arc::clone(&client));
    let row_request = MixedDatalogBatchRequest::Rows(DatalogBatchRequest {
        app_id,
        clauses: normalized_clauses(attr_id),
        options: MatchCteOptions::default(),
        page: None,
    });
    let count_request = MixedDatalogBatchRequest::Count(DatalogBatchRequest {
        app_id: second_app_id,
        clauses: normalized_clauses(second_attr_id),
        options: MatchCteOptions::default(),
        page: None,
    });
    let (loaded_rows, loaded_count) =
        tokio::join!(loader.load(row_request), loader.load(count_request));
    assert!(matches!(
        loaded_rows.unwrap(),
        MixedDatalogBatchResult::Rows(result)
            if result.join_rows[0][0].entity_id == entity_id
    ));
    assert_eq!(loaded_count.unwrap(), MixedDatalogBatchResult::Count(1));

    drop(loader);
    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn generic_array_binding_supports_schema_defined_enum_arrays() {
    let Some(database_url) = instant_test_support::test_database_url(
        "generic_array_binding_supports_schema_defined_enum_arrays",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute("CREATE TYPE pg_temp.array_status AS ENUM ('draft', 'published')")
        .await
        .unwrap();

    let query = instant_datalog::RenderedQuery {
        sql: "SELECT to_jsonb($1::array_status[]) AS result".to_owned(),
        parameters: vec![SqlParameter::Array {
            values: vec![json!("draft"), json!("published")],
            element_type: instant_datalog::PgType::CheckedDataType,
        }],
    };
    let rows = instant_pg::query(&client, &query).await.unwrap();
    let result: tokio_postgres::types::Json<serde_json::Value> = rows[0].get("result");
    assert_eq!(result.0, json!(["draft", "published"]));

    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One fixture exercises the complete typed cursor matrix.
async fn typed_pagination_matches_the_jvm_contract_against_postgres() {
    let Some(database_url) = instant_test_support::test_database_url(
        "typed_pagination_matches_the_jvm_contract_against_postgres",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TEMP TABLE triples (
                app_id uuid NOT NULL,
                entity_id uuid NOT NULL,
                attr_id uuid NOT NULL,
                value jsonb NOT NULL,
                ea boolean NOT NULL,
                eav boolean NOT NULL,
                av boolean NOT NULL,
                ave boolean NOT NULL,
                vae boolean NOT NULL,
                created_at bigint NOT NULL,
                checked_data_type checked_data_type
            )",
        )
        .await
        .unwrap();

    let app_id = uuid("dddddddd-dddd-dddd-dddd-dddddddddddd");
    let base_attr = uuid("eeeeeeee-eeee-eeee-eeee-eeeeeeeeeeee");
    let entities = [
        uuid("11111111-1111-1111-1111-111111111111"),
        uuid("22222222-2222-2222-2222-222222222222"),
        uuid("33333333-3333-3333-3333-333333333333"),
    ];
    for (index, entity_id) in entities.into_iter().enumerate() {
        insert_triple(
            &client,
            app_id,
            entity_id,
            base_attr,
            &format!("entity-{index}"),
            1000 + i64::try_from(index).unwrap(),
        )
        .await;
    }

    let typed_cases = [
        (
            uuid("aaaaaaaa-0000-0000-0000-000000000001"),
            DataType::String,
            [json!("alpha"), json!("bravo"), json!("charlie")],
        ),
        (
            uuid("aaaaaaaa-0000-0000-0000-000000000002"),
            DataType::Number,
            [json!(10), json!(20), json!(30)],
        ),
        (
            uuid("aaaaaaaa-0000-0000-0000-000000000003"),
            DataType::Boolean,
            [json!(false), json!(true), json!(true)],
        ),
        (
            uuid("aaaaaaaa-0000-0000-0000-000000000004"),
            DataType::Date,
            [
                json!("2024-01-01T00:00:00Z"),
                json!("2024-02-01T00:00:00Z"),
                json!("2024-03-01T00:00:00Z"),
            ],
        ),
    ];
    for (order_attr, order_type, values) in &typed_cases {
        for (index, (entity_id, value)) in entities.into_iter().zip(values).enumerate() {
            insert_checked_value(
                &client,
                app_id,
                entity_id,
                *order_attr,
                value.clone(),
                *order_type,
                2000 + i64::try_from(index).unwrap(),
            )
            .await;
        }
        let result = execute_datalog_requests(
            &client,
            &[checked_page_request(
                app_id,
                base_attr,
                *order_attr,
                *order_type,
                OrderDirection::Asc,
                None,
                false,
                None,
                false,
            )],
        )
        .await
        .unwrap();
        assert_eq!(result_entities(&result[0]), entities);
    }

    let number_attr = typed_cases[1].0;
    let middle = PageCursor {
        entity_id: entities[1],
        order_value: json!(20),
    };
    let cursor_cases = [
        (
            OrderDirection::Asc,
            Some(middle.clone()),
            false,
            None,
            false,
            vec![entities[0]],
        ),
        (
            OrderDirection::Asc,
            Some(middle.clone()),
            true,
            None,
            false,
            vec![entities[0], entities[1]],
        ),
        (
            OrderDirection::Asc,
            None,
            false,
            Some(middle.clone()),
            false,
            vec![entities[2]],
        ),
        (
            OrderDirection::Asc,
            None,
            false,
            Some(middle.clone()),
            true,
            vec![entities[1], entities[2]],
        ),
        (
            OrderDirection::Desc,
            Some(middle.clone()),
            false,
            None,
            false,
            vec![entities[2]],
        ),
        (
            OrderDirection::Desc,
            None,
            false,
            Some(middle),
            false,
            vec![entities[0]],
        ),
    ];
    for (direction, before, before_inclusive, after, after_inclusive, expected) in cursor_cases {
        let result = execute_datalog_requests(
            &client,
            &[checked_page_request(
                app_id,
                base_attr,
                number_attr,
                DataType::Number,
                direction,
                before,
                before_inclusive,
                after,
                after_inclusive,
            )],
        )
        .await
        .unwrap();
        assert_eq!(result_entities(&result[0]), expected);
    }

    let tied_boolean_cursor = PageCursor {
        entity_id: entities[1],
        order_value: json!(true),
    };
    let result = execute_datalog_requests(
        &client,
        &[checked_page_request(
            app_id,
            base_attr,
            typed_cases[2].0,
            DataType::Boolean,
            OrderDirection::Asc,
            None,
            false,
            Some(tied_boolean_cursor),
            false,
        )],
    )
    .await
    .unwrap();
    assert_eq!(result_entities(&result[0]), vec![entities[2]]);

    drop(client);
    connection_task.await.unwrap().unwrap();
}

/// Exercises the complete render/bind/execute/decode path with a production-sized
/// result set.  The generated rows intentionally use the same hot columns as the
/// real `triples` table, so this catches accidental result truncation and parameter
/// or JSON-envelope limits that tiny fixtures cannot expose.
#[tokio::test]
async fn production_scale_datalog_results_are_complete_and_batchable() {
    let Some(database_url) = instant_test_support::test_database_url(
        "production_scale_datalog_results_are_complete_and_batchable",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TEMP TABLE triples (
                app_id uuid NOT NULL,
                entity_id uuid NOT NULL,
                attr_id uuid NOT NULL,
                value jsonb NOT NULL,
                ea boolean NOT NULL,
                eav boolean NOT NULL,
                av boolean NOT NULL,
                ave boolean NOT NULL,
                vae boolean NOT NULL,
                created_at bigint NOT NULL
            );
            CREATE INDEX triples_app_attr ON triples (app_id, attr_id);",
        )
        .await
        .unwrap();

    let first_app = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let second_app = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let attr_id = uuid("cccccccc-cccc-cccc-cccc-cccccccccccc");
    // Ten thousand rows per app is large enough to exercise the normal production
    // result path while remaining quick on the PostgreSQL CI service.
    for app_id in [first_app, second_app] {
        client
            .execute(
                "INSERT INTO triples
                 (app_id, entity_id, attr_id, value, ea, eav, av, ave, vae, created_at)
                 SELECT $1::uuid,
                        md5(format('%s:%s', $1::uuid::text, series))::uuid,
                        $2::uuid,
                        to_jsonb(format('entity-%s', series)),
                        true, false, false, false, false, series::bigint
                 FROM generate_series(0, 9999) AS rows(series)",
                &[&app_id, &attr_id],
            )
            .await
            .unwrap();
    }

    let first_plan = match_plan("scale-first-", first_app, attr_id);
    let second_plan = match_plan("scale-second-", second_app, attr_id);
    let first_query = render_match_query(&first_plan).unwrap();
    let first_result = query_datalog(&client, &first_query, &first_plan)
        .await
        .unwrap();
    assert_eq!(first_result.join_rows.len(), 10_000);
    let first_entities: HashSet<_> = result_entities(&first_result).into_iter().collect();
    assert_eq!(first_entities.len(), 10_000);
    assert!(
        first_entities.contains(&Uuid::parse_str("73a08317-72cb-89f7-282f-c4420468ada8").unwrap())
    );

    let batch_query = render_batch_match_query(&[first_plan.clone(), second_plan.clone()]).unwrap();
    let batch_results = query_datalog_batch(&client, &batch_query, &[first_plan, second_plan])
        .await
        .unwrap();
    assert_eq!(batch_results.len(), 2);
    assert_eq!(batch_results[0].join_rows.len(), 10_000);
    assert_eq!(batch_results[1].join_rows.len(), 10_000);
    assert_eq!(
        query_datalog_count(&client, &match_plan("scale-count-", first_app, attr_id))
            .await
            .unwrap(),
        10_000
    );

    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn timed_out_query_is_cancelled_and_releases_the_connection() {
    let Some(database_url) = instant_test_support::test_database_url(
        "timed_out_query_is_cancelled_and_releases_the_connection",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let slow_query = instant_datalog::RenderedQuery {
        sql: "SELECT pg_sleep(10)".to_owned(),
        parameters: Vec::new(),
    };

    let error = query_with_timeout(&client, &slow_query, Duration::from_millis(10))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PgQueryError::QueryTimeout { milliseconds: 10 }
    ));
    let row = client.query_one("SELECT 1::bigint", &[]).await.unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);

    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn nested_query_deadline_cancels_blocked_cardinality_read() {
    let Some(database_url) = instant_test_support::test_database_url(
        "nested_query_deadline_cancels_blocked_cardinality_read",
    ) else {
        return;
    };
    let schema = format!("instaql_timeout_{}", Uuid::new_v4().simple());
    let (query_client, query_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let query_connection_task = tokio::spawn(query_connection);
    let (locker_client, locker_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let locker_connection_task = tokio::spawn(locker_connection);

    locker_client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attr_sketches (
                 app_id uuid NOT NULL,
                 attr_id uuid NOT NULL,
                 total bigint NOT NULL
             );"
        ))
        .await
        .unwrap();
    query_client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    locker_client
        .batch_execute(&format!(
            "BEGIN; LOCK TABLE {schema}.attr_sketches IN ACCESS EXCLUSIVE MODE"
        ))
        .await
        .unwrap();

    let app_id = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![attribute(attr_id, "goals", "id", AttributeValueType::Blob)],
    };
    let compiled = compile_query(&json!({"goals": {}}), &catalog, false).unwrap();
    let error = query_instaql_with_timeout(
        &query_client,
        &compiled,
        app_id,
        &MatchCteOptions::default(),
        Duration::from_millis(10),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PgQueryError::QueryTimeout { milliseconds: 10 }
    ));

    locker_client.batch_execute("ROLLBACK").await.unwrap();
    let row = query_client
        .query_one("SELECT 1::bigint", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
    locker_client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();

    drop(query_client);
    drop(locker_client);
    query_connection_task.await.unwrap().unwrap();
    locker_connection_task.await.unwrap().unwrap();
}

fn attribute(id: Uuid, etype: &str, label: &str, value_type: AttributeValueType) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type,
        cardinality: Cardinality::One,
        unique: label == "id",
        setting_unique: false,
        indexed: label == "id",
        required: label == "id",
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}
