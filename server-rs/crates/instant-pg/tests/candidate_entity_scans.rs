use std::collections::{BTreeMap, BTreeSet};

use instant_datalog::{
    DataType, FromTable, InvalidationTopic, MatchCteOptions, PlannedCte, PredicateOperator,
    PredicateOptions, TopicPart, WhereExpression, WherePredicate, render_nested_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, PlannedNestedGroup,
    collect_query_invalidation_topics, compile_query, plan_nested_query, shape_response,
};
use serde_json::{Value, json};
use tokio_postgres::{NoTls, types::Json};
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "room", "revision", "updatedAt", "children"]
            .into_iter()
            .enumerate()
            .map(|(n, label)| Attribute {
                id: Uuid::from_u128(n as u128 + 1),
                wire: None,
                etype: "messages".into(),
                label: label.into(),
                value_type: if label == "children" {
                    AttributeValueType::Ref
                } else {
                    AttributeValueType::Blob
                },
                cardinality: if label == "children" {
                    Cardinality::Many
                } else {
                    Cardinality::One
                },
                unique: label == "id",
                setting_unique: false,
                indexed: matches!(label, "id" | "room" | "updatedAt"),
                required: label == "id",
                checked_data_type: (label == "updatedAt").then_some(DataType::Number),
                indexing: false,
                checking_data_type: false,
                reverse_etype: (label == "children").then(|| "messages".into()),
                reverse_label: (label == "children").then(|| "parents".into()),
                on_delete: None,
                on_delete_reverse: None,
            })
            .collect(),
    }
}

fn canonical_topics(topics: Vec<InvalidationTopic>) -> BTreeSet<String> {
    // Topics and their binding values are sets. Changing a physical scan may
    // reorder them without changing which WAL updates match the dependency.
    topics
        .into_iter()
        .map(|mut topic| {
            topic.indexes.sort_by_key(ToString::to_string);
            for part in [&mut topic.entity, &mut topic.attribute, &mut topic.value] {
                if let TopicPart::Values(values) = part {
                    values.sort_by_cached_key(|value| serde_json::to_string(value).unwrap());
                    values.dedup();
                }
            }
            serde_json::to_string(&topic).unwrap()
        })
        .collect()
}

#[tokio::test]
async fn candidate_pages_preserve_results_cursors_topics_and_reduce_database_work() {
    let Some(url) = instant_test_support::test_database_url(
        "candidate_pages_preserve_results_cursors_topics_and_reduce_database_work",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let task = tokio::spawn(connection);
    let tx = client.transaction().await.unwrap();
    let schema = format!("candidate_{}", Uuid::new_v4().simple());
    // Hint-enabled queries read a fresh sketch snapshot even when this fixture
    // supplies its own estimates. Keep the snapshot table in the isolated
    // schema and empty so zero/small/broad test estimates remain deliberate.
    tx.batch_execute(&format!("CREATE SCHEMA {schema}; SET LOCAL search_path={schema},pg_catalog;
      CREATE TYPE checked_data_type AS ENUM ('number','string','boolean','date');
      CREATE TABLE triples(app_id uuid,entity_id uuid,attr_id uuid,value jsonb,ea boolean,eav boolean,av boolean,ave boolean,vae boolean,created_at bigint,checked_data_type checked_data_type);
      CREATE TABLE attr_sketches(app_id uuid NOT NULL,attr_id uuid NOT NULL,total bigint NOT NULL,width integer,depth integer,total_not_binned bigint,bins bytea,reverse_width integer,reverse_depth integer,reverse_total bigint,reverse_bins bytea);
      CREATE INDEX triples_pkey ON triples(app_id,entity_id,attr_id);
      CREATE INDEX ea_index ON triples(app_id,entity_id,attr_id) WHERE ea;
      CREATE INDEX ave_with_e_index ON triples(app_id,attr_id,value,entity_id) WHERE ave;
      CREATE INDEX av_index ON triples(app_id,attr_id,value,entity_id) WHERE av;
      CREATE INDEX triples_created_at_idx ON triples(app_id,attr_id,created_at);
      CREATE FUNCTION triples_extract_number_value(jsonb) RETURNS float8 LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN jsonb_typeof($1)='number' THEN ($1#>>'{{}}')::float8 END $$;
      CREATE FUNCTION json_null_to_null(jsonb) RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$ SELECT NULLIF($1,'null'::jsonb) $$;
      CREATE FUNCTION json_uuid_to_uuid(jsonb) RETURNS uuid LANGUAGE sql IMMUTABLE AS $$ SELECT ($1#>>'{{}}')::uuid $$;
      CREATE INDEX triples_number_type_idx ON triples(app_id,attr_id,triples_extract_number_value(value) NULLS FIRST) WHERE ave AND checked_data_type='number';
      SET LOCAL jit=off;
      SET LOCAL statement_timeout='10s';")).await.unwrap();
    let app = Uuid::from_u128(100);
    tx.execute("WITH entities AS (SELECT n,md5(n::text)::uuid AS id FROM generate_series(1,20000) n)
      INSERT INTO triples SELECT $1,id,v.attr,v.value,true,false,v.av,v.ave,false,n::bigint,v.dtype
      FROM entities CROSS JOIN LATERAL (VALUES
       ('00000000-0000-0000-0000-000000000001'::uuid,to_jsonb(id::text),true,true,NULL::checked_data_type),
       ('00000000-0000-0000-0000-000000000002'::uuid,to_jsonb(CASE WHEN n%997=0 THEN 'small' ELSE 'other' END),false,true,NULL::checked_data_type),
       ('00000000-0000-0000-0000-000000000003'::uuid,CASE WHEN n%7=0 THEN 'null'::jsonb ELSE to_jsonb(CASE WHEN n%3=0 THEN 'skip' ELSE 'keep' END) END,false,false,NULL::checked_data_type),
       ('00000000-0000-0000-0000-000000000004'::uuid,CASE WHEN n%11=0 THEN 'null'::jsonb ELSE to_jsonb(n%10) END,false,true,'number'::checked_data_type)
       )v(attr,value,av,ave,dtype) WHERE v.attr<>'00000000-0000-0000-0000-000000000003'::uuid OR n%5<>0", &[&app]).await.unwrap();
    tx.execute("INSERT INTO triples SELECT $1,md5(n::text)::uuid,'00000000-0000-0000-0000-000000000005'::uuid,
      to_jsonb(md5((n+997*child)::text)),false,true,false,false,true,n::bigint,NULL
      FROM generate_series(1,17000) n CROSS JOIN generate_series(1,2) child WHERE n%997=0", &[&app]).await.unwrap();
    tx.batch_execute("ANALYZE triples").await.unwrap();
    let catalog = catalog();
    let base = MatchCteOptions {
        pg_hints_enabled: Some(false),
        ensure_empty_symbol_values_for_topics: Some(true),
        predicate_options: PredicateOptions {
            forward_is_null_exists: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let candidate = MatchCteOptions {
        candidate_entity_scans: Some(true),
        ..base.clone()
    };
    let queries = [
        json!({"messages":{"$":{"where":{"room":"small"},"order":{"updatedAt":"desc"},"limit":5}}}),
        json!({"messages":{"$":{"where":{"room":"small","revision":{"$ne":"skip"}},"order":{"updatedAt":"desc"},"limit":5}}}),
        json!({"messages":{"$":{"where":{"room":"small","revision":{"$ne":"skip"},"updatedAt":{"$lt":8}},"order":{"updatedAt":"asc"},"last":5}}}),
        json!({"messages":{"$":{"where":{"or":[{"room":"small"},{"revision":"skip"}]},"order":{"updatedAt":"desc"},"limit":5}}}),
        json!({"messages":{"$":{"where":{"room":{"$in":["small","missing"]}},"order":{"updatedAt":"asc"},"limit":5,"offset":3}}}),
        json!({"messages":{"$":{"where":{"room":"missing","revision":{"$ne":"skip"}},"order":{"updatedAt":"desc"},"limit":5}}}),
        json!({"messages":{"$":{"where":{"room":"small"},"order":{"updatedAt":"desc"},"limit":3,"fields":["id","room"]},"children":{"$":{"where":{"room":"small","revision":{"$ne":"skip"}},"order":{"updatedAt":"asc"},"limit":2,"fields":["id","room"]}}}}),
        json!({"messages":{"$":{"where":{"room":"small","revision":{"$ne":"skip"}},"aggregate":"count"}}}),
    ];
    for (index, input) in queries.into_iter().enumerate() {
        let admin = input["messages"]["$"].get("aggregate").is_some();
        let compiled = compile_query(&input, &catalog, admin).unwrap();
        let old = instant_pg::query_instaql(&tx, &compiled, app, &base)
            .await
            .unwrap();
        let new = instant_pg::query_instaql(&tx, &compiled, app, &candidate)
            .await
            .unwrap();
        let old_response = shape_response(&compiled, &old, &catalog, false).unwrap();
        let new_response = shape_response(&compiled, &new, &catalog, false).unwrap();
        if index == 0 {
            assert_eq!(old_response.data["messages"].as_array().unwrap().len(), 5);
        }
        assert_eq!(
            serde_json::to_value(&old_response).unwrap(),
            serde_json::to_value(&new_response).unwrap(),
            "case {index}: data, nulls and cursor metadata"
        );
        assert_eq!(
            canonical_topics(collect_query_invalidation_topics(&compiled, &old).unwrap()),
            canonical_topics(collect_query_invalidation_topics(&compiled, &new).unwrap()),
            "case {index}: invalidation dependencies"
        );
        // Known-small and stale-zero seeds opt forward null probes into the
        // entity/attribute hint. A broad estimate keeps the previous path. The
        // complete rows, missing/JSON-null semantics, pages and topics must not
        // depend on that physical choice (local PostgreSQL may ignore hints).
        for estimate in [0, 20, 2000] {
            let hinted = MatchCteOptions {
                pg_hints_enabled: Some(true),
                attribute_cardinalities: catalog
                    .attributes
                    .iter()
                    .map(|attr| (attr.id, 20_000))
                    .collect(),
                attribute_value_cardinalities: BTreeMap::from([(
                    Uuid::from_u128(2),
                    BTreeMap::from([(json!("small").to_string(), estimate)]),
                )]),
                ..candidate.clone()
            };
            let actual = instant_pg::query_instaql(&tx, &compiled, app, &hinted)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(shape_response(&compiled, &actual, &catalog, false).unwrap())
                    .unwrap(),
                serde_json::to_value(&old_response).unwrap(),
                "case {index}: forward null point estimate {estimate}"
            );
            assert_eq!(
                canonical_topics(collect_query_invalidation_topics(&compiled, &actual).unwrap()),
                canonical_topics(collect_query_invalidation_topics(&compiled, &old).unwrap()),
                "case {index}: point probe topics estimate {estimate}"
            );
            if index == 1 && estimate <= 1024 {
                assert!(
                    plan_nested_query(&compiled, app, &hinted)
                        .unwrap()
                        .hints
                        .iter()
                        .any(|hint| matches!(hint,
                            instant_datalog::QueryHint::IndexScan {table_alias,index}
                            if table_alias.ends_with("-subquery") && index=="triples_pkey"))
                );
            }
        }
        // Exercise both directions of a real public cursor, including ties and
        // missing/JSON-null optional sort values. No field projection is widened.
        for (boundary, cursor) in
            old_response
                .page_info
                .get("messages")
                .into_iter()
                .flat_map(|page| {
                    [
                        ("after", page.end_cursor.as_ref()),
                        ("before", page.start_cursor.as_ref()),
                    ]
                })
        {
            let Some(cursor) = cursor else {
                continue;
            };
            let mut next = input.clone();
            let value = cursor
                .value
                .as_ref()
                .map(|v| match v {
                    instant_datalog::DatalogValue::Json(v) => v.clone(),
                    instant_datalog::DatalogValue::Uuid(v) => json!(v),
                })
                .unwrap_or(Value::Null);
            next["messages"]["$"][boundary] =
                json!([cursor.entity_id, cursor.attr_id, value, cursor.created_at]);
            let compiled = compile_query(&next, &catalog, false).unwrap();
            let a = instant_pg::query_instaql(&tx, &compiled, app, &base)
                .await
                .unwrap();
            let b = instant_pg::query_instaql(&tx, &compiled, app, &candidate)
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(shape_response(&compiled, &a, &catalog, false).unwrap())
                    .unwrap(),
                serde_json::to_value(shape_response(&compiled, &b, &catalog, false).unwrap())
                    .unwrap(),
                "case {index}: {boundary} cursor"
            );
        }
        if index == 1 {
            let mut measurements = Vec::new();
            for options in [&base, &candidate] {
                let plan = plan_nested_query(&compiled, app, options).unwrap();
                let mut rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
                rendered.sql = format!("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) {}", rendered.sql);
                let rows = instant_pg::query(&tx, &rendered).await.unwrap();
                let Json(explain): Json<Value> = rows[0].get(0);
                let root = &explain[0]["Plan"];
                let blocks = root["Shared Hit Blocks"].as_u64().unwrap_or(0)
                    + root["Shared Read Blocks"].as_u64().unwrap_or(0);
                measurements.push((blocks, explain[0]["Execution Time"].as_f64().unwrap()));
            }
            eprintln!(
                "Local 20k-entity query: legacy {:?}, candidate {:?} (buffer accesses, execution ms); not serving capacity evidence",
                measurements[0], measurements[1]
            );
            assert!(
                measurements[1].0 < measurements[0].0,
                "candidate plan must perform fewer buffer accesses on the sparse-room OR workload"
            );
        }
    }
    // Estimates and candidate relations are planning aids, not result caches.
    let query =
        json!({"messages":{"$":{"where":{"room":"small"},"order":{"updatedAt":"desc"},"limit":5}}});
    let compiled = compile_query(&query, &catalog, false).unwrap();
    let before = instant_pg::query_instaql(&tx, &compiled, app, &candidate)
        .await
        .unwrap();
    tx.execute("UPDATE triples SET value='999999'::jsonb WHERE app_id=$1 AND entity_id=md5('997')::uuid AND attr_id='00000000-0000-0000-0000-000000000004'::uuid", &[&app]).await.unwrap();
    let after = instant_pg::query_instaql(&tx, &compiled, app, &candidate)
        .await
        .unwrap();
    let legacy = instant_pg::query_instaql(&tx, &compiled, app, &base)
        .await
        .unwrap();
    assert_ne!(
        shape_response(&compiled, &before, &catalog, false).unwrap(),
        shape_response(&compiled, &after, &catalog, false).unwrap()
    );
    assert_eq!(
        shape_response(&compiled, &legacy, &catalog, false).unwrap(),
        shape_response(&compiled, &after, &catalog, false).unwrap()
    );
    // Broad-room order-first execution and its optional missing/JSON-null tail.
    // Deliberately overestimated small/empty seeds also exercise correctness
    // when a sketch is stale; estimates never become result filters.
    let mut ordered = candidate.clone();
    ordered
        .attribute_cardinalities
        .insert(Uuid::from_u128(2), 20000);
    for value in ["other", "small", "missing"] {
        ordered
            .attribute_value_cardinalities
            .entry(Uuid::from_u128(2))
            .or_default()
            .insert(serde_json::to_string(value).unwrap(), 20000);
    }
    for (index,input) in [
        json!({"messages":{"$":{"where":{"room":"other","revision":{"$ne":"skip"}},"order":{"updatedAt":"desc"},"limit":20}}}),
        json!({"messages":{"$":{"where":{"room":"small"},"order":{"updatedAt":"desc"},"limit":20}}}),
        json!({"messages":{"$":{"where":{"room":"small","revision":{"$ne":"skip"}},"order":{"updatedAt":"desc"},"limit":20}}}),
        json!({"messages":{"$":{"where":{"room":"missing"},"order":{"updatedAt":"desc"},"limit":20}}}),
        json!({"messages":{"$":{"where":{"room":"other","updatedAt":{"$lt":8}},"order":{"updatedAt":"desc"},"limit":20}}}),
        json!({"messages":{"$":{"where":{"room":"other","or":[{"revision":{"$ne":"skip"}},{"updatedAt":{"$lt":7}}]},"order":{"updatedAt":"desc"},"limit":20}}}),
    ].into_iter().enumerate() {
        let compiled=compile_query(&input,&catalog,false).unwrap();
        let plan=plan_nested_query(&compiled,app,&ordered).unwrap();
        assert!(plan.ctes.iter().any(|cte|matches!(cte,instant_datalog::PlannedCte::Select(c) if c.name.ends_with("ordered-nonnull"))),"ordered case {index} must actually exercise the new path");
        let a=instant_pg::query_instaql(&tx,&compiled,app,&base).await.unwrap();
        let b=instant_pg::query_instaql(&tx,&compiled,app,&ordered).await.unwrap();
        assert_eq!(shape_response(&compiled,&a,&catalog,false).unwrap(),shape_response(&compiled,&b,&catalog,false).unwrap(),"ordered case {index}: values, boundaries and nulls");
        assert_eq!(canonical_topics(collect_query_invalidation_topics(&compiled,&a).unwrap()),canonical_topics(collect_query_invalidation_topics(&compiled,&b).unwrap()),"ordered case {index}: dependency sets");
        if index==0 {
            let mut measurements=Vec::new();
            for options in [&base,&ordered] {
                let plan=plan_nested_query(&compiled,app,options).unwrap();
                let mut rendered=render_nested_match_query(&plan.sql_plan()).unwrap();rendered.sql=format!("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) {}",rendered.sql);
                let rows=instant_pg::query(&tx,&rendered).await.unwrap();let Json(explain):Json<Value>=rows[0].get(0);
                measurements.push((explain[0]["Plan"]["Shared Hit Blocks"].as_u64().unwrap_or(0),explain[0]["Execution Time"].as_f64().unwrap()));
            }
            eprintln!("Local broad-room query: legacy {:?}, ordered {:?} (buffer accesses, execution ms); not live capacity evidence",measurements[0],measurements[1]);
        }
    }
    // Default-created-at ordering must probe a selective entity set rather than
    // walking every ID triple just to find a small private page. Include tied
    // creation times and deliberately wrong estimates: estimates never filter.
    tx.execute("UPDATE triples SET created_at=created_at%3 WHERE app_id=$1 AND attr_id='00000000-0000-0000-0000-000000000001'::uuid", &[&app]).await.unwrap();
    let mut narrow = candidate.clone();
    narrow
        .attribute_cardinalities
        .insert(Uuid::from_u128(1), 20_000);
    narrow
        .attribute_cardinalities
        .insert(Uuid::from_u128(2), 20_000);
    for room in ["small", "other", "missing"] {
        narrow
            .attribute_value_cardinalities
            .entry(Uuid::from_u128(2))
            .or_default()
            .insert(serde_json::to_string(room).unwrap(), 20);
    }
    for (index, input) in [
        json!({"messages":{"$":{"where":{"room":"small"},"limit":5}}}),
        json!({"messages":{"$":{"where":{"room":"small","revision":{"$ne":"skip"}},"limit":5}}}),
        json!({"messages":{"$":{"where":{"room":"small"},"limit":5,"order":{"serverCreatedAt":"desc"}}}}),
        json!({"messages":{"$":{"where":{"room":"small"},"last":5}}}),
        json!({"messages":{"$":{"where":{"room":"small"},"limit":5,"offset":3}}}),
        json!({"messages":{"$":{"where":{"room":"missing"},"limit":5}}}),
        json!({"messages":{"$":{"where":{"room":"other"},"limit":5}}}),
    ].into_iter().enumerate() {
        let compiled = compile_query(&input, &catalog, false).unwrap();
        let plan = plan_nested_query(&compiled, app, &narrow).unwrap();
        assert!(plan.ctes.iter().any(|cte| matches!(cte, instant_datalog::PlannedCte::Select(c) if c.name.ends_with("-bound-order"))), "created-at case {index} must exercise the bounded order join");
        let a = instant_pg::query_instaql(&tx, &compiled, app, &base).await.unwrap();
        let b = instant_pg::query_instaql(&tx, &compiled, app, &narrow).await.unwrap();
        let response = shape_response(&compiled, &a, &catalog, false).unwrap();
        assert_eq!(response, shape_response(&compiled, &b, &catalog, false).unwrap(), "created-at case {index}: rows, order, ties, page flags");
        assert_eq!(canonical_topics(collect_query_invalidation_topics(&compiled, &a).unwrap()), canonical_topics(collect_query_invalidation_topics(&compiled, &b).unwrap()), "created-at case {index}: dependency sets");
        if index == 0 {
            let missing = instant_pg::query_instaql(&tx, &compiled, app, &candidate).await.unwrap();
            assert_eq!(response, shape_response(&compiled, &missing, &catalog, false).unwrap(), "missing-sketch created-at fallback retains exact results");
            let mut work = Vec::new();
            for options in [&base, &candidate] {
                let plan = plan_nested_query(&compiled, app, options).unwrap();
                let mut rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
                rendered.sql = format!("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) {}", rendered.sql);
                let rows = instant_pg::query(&tx, &rendered).await.unwrap();
                let Json(explain): Json<Value> = rows[0].get(0);
                work.push((explain[0]["Plan"]["Shared Hit Blocks"].as_u64().unwrap_or(0), explain[0]["Execution Time"].as_f64().unwrap()));
            }
            eprintln!("Local narrow created-at page: legacy {:?}, bounded {:?} (buffer accesses, execution ms); not live capacity evidence", work[0], work[1]);
            // PostgreSQL can already choose the entity-driven plan on this
            // fixture. The fence prevents the bad order-driven alternative;
            // it does not promise fewer buffers than every already-good plan.
            // Structural coverage above requires the fence, while exact
            // result/cursor/topic comparisons below are the correctness gate.
        }
        for (boundary, cursor) in response.page_info.get("messages").into_iter().flat_map(|page| [("after", page.end_cursor.as_ref()), ("before", page.start_cursor.as_ref())]) {
            let Some(cursor) = cursor else { continue };
            let value = cursor.value.as_ref().map(|value| match value {
                instant_datalog::DatalogValue::Json(value) => value.clone(),
                instant_datalog::DatalogValue::Uuid(value) => json!(value),
            }).unwrap_or(Value::Null);
            let mut next = input.clone();
            next["messages"]["$"][boundary] = json!([cursor.entity_id, cursor.attr_id, value, cursor.created_at]);
            let compiled = compile_query(&next, &catalog, false).unwrap();
            let a = instant_pg::query_instaql(&tx, &compiled, app, &base).await.unwrap();
            let b = instant_pg::query_instaql(&tx, &compiled, app, &narrow).await.unwrap();
            assert_eq!(shape_response(&compiled, &a, &catalog, false).unwrap(), shape_response(&compiled, &b, &catalog, false).unwrap(), "created-at case {index}: {boundary} cursor");
            assert_eq!(canonical_topics(collect_query_invalidation_topics(&compiled, &a).unwrap()), canonical_topics(collect_query_invalidation_topics(&compiled, &b).unwrap()), "created-at case {index}: {boundary} topics");
        }
    }
    numeric_membership_preserves_complete_rows_pages_topics_and_fresh_values(&tx, app, &catalog)
        .await;
    tx.rollback().await.unwrap();
    drop(client);
    task.await.unwrap().unwrap();
}

fn canonical_envelope(mut envelope: Value, groups: &[PlannedNestedGroup]) -> Value {
    fn paged_tables(groups: &[PlannedNestedGroup], result: &mut BTreeSet<String>) {
        for group in groups {
            if let (Some(table), Some(page)) = (&group.table, &group.page) {
                result.extend(
                    [table, &page.has_next_table, &page.has_previous_table]
                        .map(|name| name.replace('-', "_")),
                );
            }
            if let Some(children) = &group.children {
                paged_tables(&children.groups, result);
            }
        }
    }
    let mut pages = BTreeSet::new();
    paged_tables(groups, &mut pages);
    for chunk in envelope.as_array_mut().unwrap() {
        for (table, rows) in chunk.as_object_mut().unwrap() {
            if !pages.contains(table)
                && let Some(rows) = rows.as_array_mut()
            {
                // Field gathers have no SQL ordering contract. Compare complete
                // row multisets, not just entity sets or the projected response.
                rows.sort_by_cached_key(|row| serde_json::to_string(row).unwrap());
            }
        }
    }
    envelope
}

async fn compare_numeric_membership(
    tx: &tokio_postgres::Transaction<'_>,
    app: Uuid,
    catalog: &AttributeCatalog,
    input: &Value,
    options: &MatchCteOptions,
) -> instant_instaql::InstaqlResponse {
    let compiled = compile_query(input, catalog, false).unwrap();
    let plan = plan_nested_query(&compiled, app, options).unwrap();
    let mut reference = plan.clone();
    let mut restored = 0;
    for cte in &mut reference.ctes {
        let PlannedCte::Match(cte) = cte else {
            continue;
        };
        let WherePredicate::And(predicates) = &mut cte.predicate else {
            continue;
        };
        for predicate in predicates {
            let WherePredicate::In {
                negated: false,
                expression,
                query,
            } = predicate
            else {
                continue;
            };
            if !query.table.contains("-candidate-") {
                continue;
            }
            let table = query.table.clone();
            let left = expression.clone();
            cte.from.push(FromTable {
                table: table.clone(),
                alias: None,
            });
            *predicate = WherePredicate::Compare {
                operator: PredicateOperator::Equal,
                left,
                right: WhereExpression::NamedColumn(format!("{table}.bound-entity-id")),
            };
            restored += 1;
        }
    }
    assert!(
        restored > 0,
        "the live case must exercise numeric membership"
    );
    assert_eq!(plan.groups, reference.groups);
    assert_eq!(plan.hints, reference.hints);
    let new = render_nested_match_query(&plan.sql_plan()).unwrap();
    let old = render_nested_match_query(&reference.sql_plan()).unwrap();
    assert_eq!(new.parameters, old.parameters);
    let mut results = Vec::new();
    for (label, plan, rendered) in [("former-join", &reference, old), ("membership", &plan, new)] {
        let rows = instant_pg::query(tx, &rendered)
            .await
            .unwrap_or_else(|error| panic!("numeric {label} SQL failed: {error}"));
        let Json(envelope): Json<Value> = rows[0].get(0);
        let nodes = instant_pg::decode_planned_instaql_envelope(plan, &envelope).unwrap();
        let response = shape_response(&compiled, &nodes, catalog, false).unwrap();
        let topics =
            canonical_topics(collect_query_invalidation_topics(&compiled, &nodes).unwrap());
        results.push((canonical_envelope(envelope, &plan.groups), response, topics));
    }
    assert_eq!(
        results[0], results[1],
        "complete envelopes, exact paged sequences, cursors and dependency sets"
    );
    results.pop().unwrap().1
}

async fn numeric_membership_preserves_complete_rows_pages_topics_and_fresh_values(
    tx: &tokio_postgres::Transaction<'_>,
    app: Uuid,
    catalog: &AttributeCatalog,
) {
    // 150 candidates with tied numeric values, JSON nulls, missing attributes,
    // null-aware revision OR branches, and multi-valued duplicate seed matches.
    // Match insert_triple's flags: ea describes cardinality-one, while eav/vae
    // describe references, not cardinality-many blob values.
    tx.execute("UPDATE triples SET ea=false,value=CASE WHEN entity_id IN (SELECT md5(n::text)::uuid FROM generate_series(1,150) n) THEN '\"member\"'::jsonb ELSE value END WHERE app_id=$1 AND attr_id='00000000-0000-0000-0000-000000000002'::uuid", &[&app]).await.unwrap();
    tx.execute("INSERT INTO triples SELECT app_id,entity_id,attr_id,'\"also-member\"'::jsonb,false,false,false,true,false,created_at,checked_data_type FROM triples WHERE app_id=$1 AND attr_id='00000000-0000-0000-0000-000000000002'::uuid AND value='\"member\"'::jsonb", &[&app]).await.unwrap();
    tx.execute("DELETE FROM triples WHERE app_id=$1 AND attr_id='00000000-0000-0000-0000-000000000004'::uuid AND entity_id IN (SELECT md5(n::text)::uuid FROM generate_series(13,150,13) n)", &[&app]).await.unwrap();
    let other_app = Uuid::from_u128(101);
    tx.execute("INSERT INTO triples SELECT $2,entity_id,attr_id,CASE WHEN attr_id='00000000-0000-0000-0000-000000000004'::uuid THEN '999999999'::jsonb ELSE value END,ea,eav,av,ave,vae,created_at,checked_data_type FROM triples WHERE app_id=$1 AND entity_id IN (SELECT md5(n::text)::uuid FROM generate_series(1,150) n)", &[&app,&other_app]).await.unwrap();
    let mut catalog = catalog.clone();
    catalog.attributes[1].cardinality = Cardinality::Many;
    let mut options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        pg_hints_enabled: Some(false),
        ensure_empty_symbol_values_for_topics: Some(true),
        predicate_options: PredicateOptions {
            forward_is_null_exists: true,
            ..Default::default()
        },
        attribute_cardinalities: BTreeMap::from([(Uuid::from_u128(2), 20_000)]),
        attribute_value_cardinalities: BTreeMap::from([(
            Uuid::from_u128(2),
            BTreeMap::from([
                ("\"member\"".into(), 150),
                ("\"also-member\"".into(), 150),
                ("\"empty\"".into(), 0),
            ]),
        )]),
        ..Default::default()
    };
    let queries = [
        json!({"messages":{"$":{"where":{"room":{"$in":["member","also-member"]}},"order":{"updatedAt":"desc"},"limit":20,"fields":["id","room","updatedAt"]}}}),
        json!({"messages":{"$":{"where":{"room":"member","revision":{"$ne":"skip"}},"order":{"updatedAt":"desc"},"limit":20}}}),
        json!({"messages":{"$":{"where":{"room":"member","updatedAt":{"$lt":8}},"order":{"updatedAt":"asc"},"last":10}}}),
        json!({"messages":{"$":{"where":{"room":"member"},"order":{"updatedAt":"asc"},"limit":150,"offset":3}}}),
        json!({"messages":{"$":{"where":{"room":"empty"},"order":{"updatedAt":"desc"},"limit":20}}}),
    ];
    for (index, input) in queries.into_iter().enumerate() {
        eprintln!("Numeric membership fixture case {index}: initial page");
        let response = compare_numeric_membership(tx, app, &catalog, &input, &options).await;
        assert!(
            response.data["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["updatedAt"] != json!(999999999))
        );
        for (boundary, cursor) in response
            .page_info
            .get("messages")
            .into_iter()
            .flat_map(|page| {
                [
                    ("after", page.end_cursor.as_ref()),
                    ("before", page.start_cursor.as_ref()),
                ]
            })
        {
            let Some(cursor) = cursor else { continue };
            let value = cursor
                .value
                .as_ref()
                .map(|value| match value {
                    instant_datalog::DatalogValue::Json(value) => value.clone(),
                    instant_datalog::DatalogValue::Uuid(value) => json!(value),
                })
                .unwrap_or(Value::Null);
            let mut page = input.clone();
            page["messages"]["$"][boundary] =
                json!([cursor.entity_id, cursor.attr_id, value, cursor.created_at]);
            eprintln!("Numeric membership fixture case {index}: {boundary} cursor");
            compare_numeric_membership(tx, app, &catalog, &page, &options).await;
        }
    }
    // A stale zero estimate must never cap a real result set or reuse values.
    options
        .attribute_value_cardinalities
        .get_mut(&Uuid::from_u128(2))
        .unwrap()
        .insert("\"member\"".into(), 0);
    let input = json!({"messages":{"$":{"where":{"room":"member"},"order":{"updatedAt":"desc"},"limit":200}}});
    let before = compare_numeric_membership(tx, app, &catalog, &input, &options).await;
    assert_eq!(before.data["messages"].as_array().unwrap().len(), 150);
    tx.execute("UPDATE triples SET value='999999'::jsonb WHERE app_id=$1 AND entity_id=md5('1')::uuid AND attr_id='00000000-0000-0000-0000-000000000004'::uuid", &[&app]).await.unwrap();
    let after = compare_numeric_membership(tx, app, &catalog, &input, &options).await;
    assert_ne!(before, after);
    assert_eq!(after.data["messages"][0]["updatedAt"], 999999);
    assert_eq!(after.data["messages"].as_array().unwrap().len(), 150);
}
