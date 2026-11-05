use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use instant_datalog::{DataType, MatchCteOptions, PredicateOptions, TopicPart};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality,
    collect_query_invalidation_topics, compile_query, shape_response,
};
use instant_pg::{
    AttrSketch, AttrSketchInput, PgQueryError, QueryStatementPreparer, encode_attr_sketch_bins,
    query_instaql, query_instaql_with_structural_preparer,
};
use serde_json::{Value, json};
use tokio_postgres::{Client, NoTls, Statement};
use uuid::Uuid;

struct Preparer<'a> {
    client: &'a Client,
    statements: Mutex<BTreeMap<String, Statement>>,
}

impl QueryStatementPreparer for Preparer<'_> {
    async fn prepare(&self, sql: &str) -> Result<Option<Statement>, tokio_postgres::Error> {
        if let Some(statement) = self.statements.lock().unwrap().get(sql).cloned() {
            return Ok(Some(statement));
        }
        let statement = self.client.prepare(sql).await?;
        self.statements
            .lock()
            .unwrap()
            .insert(sql.into(), statement.clone());
        Ok(Some(statement))
    }
}

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "owner", "revision", "updatedAt", "children"]
            .into_iter()
            .enumerate()
            .map(|(n, label)| Attribute {
                id: Uuid::from_u128(n as u128 + 1),
                wire: None,
                etype: "items".into(),
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
                indexed: matches!(label, "id" | "owner" | "updatedAt"),
                required: false,
                checked_data_type: (label == "updatedAt").then_some(DataType::Number),
                indexing: false,
                checking_data_type: false,
                reverse_etype: (label == "children").then(|| "items".into()),
                reverse_label: (label == "children").then(|| "parents".into()),
                on_delete: None,
                on_delete_reverse: None,
            })
            .collect(),
    }
}

fn topics(
    query: &instant_instaql::CompiledQuery,
    rows: &[instant_instaql::ExecutedNode],
) -> BTreeSet<String> {
    collect_query_invalidation_topics(query, rows)
        .unwrap()
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

async fn refresh_sketches(client: &tokio_postgres::Transaction<'_>) {
    let mut sketches = BTreeMap::<(Uuid, Uuid), AttrSketch>::new();
    for row in client
        .query("SELECT app_id,attr_id,value FROM triples", &[])
        .await
        .unwrap()
    {
        let value: Value = row.get(2);
        let input = match &value {
            Value::Null => Some(AttrSketchInput::Nil),
            Value::String(value) => Some(AttrSketchInput::String(value)),
            Value::Bool(value) => Some(AttrSketchInput::Boolean(*value)),
            Value::Number(value) => Some(AttrSketchInput::Long(value.as_i64().unwrap())),
            Value::Array(_) | Value::Object(_) => None,
        };
        sketches
            .entry((row.get(0), row.get(1)))
            .or_insert_with(AttrSketch::empty_default)
            .apply_delta(input, 1)
            .unwrap();
    }
    client
        .execute("DELETE FROM attr_sketches", &[])
        .await
        .unwrap();
    for ((app, attr), sketch) in sketches {
        client.execute("INSERT INTO attr_sketches(app_id,attr_id,total,width,depth,total_not_binned,bins) VALUES($1,$2,$3,20000,10,$4,$5)",
            &[&app,&attr,&(sketch.total as i64),&(sketch.total_not_binned as i64),&encode_attr_sketch_bins(&sketch).unwrap()]).await.unwrap();
    }
}

#[tokio::test]
async fn structural_queries_preserve_fresh_rows_pages_topics_and_non_native_fallbacks() {
    let Some(url) = instant_test_support::test_database_url(
        "structural_queries_preserve_fresh_rows_pages_topics_and_non_native_fallbacks",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let driver = tokio::spawn(connection);
    let tx = client.transaction().await.unwrap();
    let schema = format!("structural_{}", Uuid::new_v4().simple());
    tx.batch_execute(&format!("CREATE SCHEMA {schema}; SET LOCAL search_path={schema},pg_catalog;
        CREATE TYPE checked_data_type AS ENUM ('number','string','boolean','date');
        CREATE TABLE triples(app_id uuid,entity_id uuid,attr_id uuid,value jsonb,ea boolean,eav boolean,av boolean,ave boolean,vae boolean,created_at bigint,checked_data_type checked_data_type);
        CREATE TABLE attr_sketches(app_id uuid,attr_id uuid,total bigint,width int,depth int,total_not_binned bigint,bins bytea,reverse_width int,reverse_depth int,reverse_total bigint,reverse_bins bytea);
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
        INSERT INTO triples SELECT a.app,md5(n::text)::uuid,v.attr,v.value,true,false,v.av,v.ave,false,n,v.dtype
        FROM (VALUES ('00000000-0000-0000-0000-000000000064'::uuid),('00000000-0000-0000-0000-000000000065'::uuid)) a(app)
        CROSS JOIN generate_series(1,200) n CROSS JOIN LATERAL (VALUES
          ('00000000-0000-0000-0000-000000000001'::uuid,to_jsonb(md5(n::text)),true,true,NULL::checked_data_type),
          ('00000000-0000-0000-0000-000000000002'::uuid,to_jsonb('owner-'||(n%2)),false,true,NULL::checked_data_type),
          ('00000000-0000-0000-0000-000000000003'::uuid,CASE WHEN n%7=0 THEN 'null'::jsonb ELSE to_jsonb('revision-'||(n%3)) END,false,false,NULL::checked_data_type),
          ('00000000-0000-0000-0000-000000000004'::uuid,CASE WHEN n%11=0 THEN 'null'::jsonb ELSE to_jsonb(n) END,false,true,'number'::checked_data_type)
        ) v(attr,value,av,ave,dtype) WHERE v.attr<>'00000000-0000-0000-0000-000000000003'::uuid OR n%5<>0;
        ANALYZE triples;")).await.unwrap();
    refresh_sketches(&tx).await;
    let catalog = catalog();
    let preparer = Preparer {
        client: tx.client(),
        statements: Mutex::new(BTreeMap::new()),
    };
    let options = MatchCteOptions {
        pg_hints_enabled: Some(true),
        candidate_entity_scans: Some(true),
        predicate_options: PredicateOptions {
            forward_is_null_exists: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let queries = [
        json!({"items":{"$":{"where":{"owner":"owner-0","revision":{"$ne":"nonce-a"}},"limit":10}}}),
        json!({"items":{"$":{"where":{"owner":"owner-1","revision":{"$ne":"nonce-b'"}},"limit":10}}}),
        json!({"items":{"$":{"where":{"owner":"owner-0"},"order":{"updatedAt":"desc"},"limit":20}}}),
        json!({"items":{"$":{"where":{"owner":"owner-1"},"order":{"updatedAt":"asc"},"last":5,"offset":2}}}),
        json!({"items":{"$":{"where":{"or":[{"owner":"owner-0"},{"revision":"revision-1"}]},"limit":10}}}),
        json!({"items":{"$":{"where":{"owner":"missing"},"limit":10}}}),
        json!({"items":{"$":{"where":{"owner":"owner-0"},"limit":101}}}),
        json!({"items":{"$":{"where":{"owner":"owner-0"},"aggregate":"count"}}}),
        json!({"items":{"$":{"limit":10},"children":{"$":{"limit":3}}}}),
        json!({"items":{"$":{"where":{"owner":"owner-0","revision":{"$ne":"nonce-shared"}},"order":{"updatedAt":"desc"},"limit":20}}}),
    ];
    for repetition in 0..8 {
        for (index, input) in queries.iter().enumerate() {
            let compiled = compile_query(input, &catalog, index == 7).unwrap();
            let app = Uuid::from_u128(100 + (repetition % 2));
            let expected = query_instaql(&tx, &compiled, app, &options).await.unwrap();
            let cached_before = preparer.statements.lock().unwrap().len();
            let actual = query_instaql_with_structural_preparer(
                &tx,
                &compiled,
                app,
                &options,
                Duration::from_secs(10),
                &preparer,
            )
            .await
            .unwrap();
            if (6..=8).contains(&index) {
                assert_eq!(
                    preparer.statements.lock().unwrap().len(),
                    cached_before,
                    "large, aggregate and nested queries must keep the old execution path"
                );
            }
            assert_eq!(
                serde_json::to_value(shape_response(&compiled, &actual, &catalog, false).unwrap())
                    .unwrap(),
                serde_json::to_value(
                    shape_response(&compiled, &expected, &catalog, false).unwrap()
                )
                .unwrap(),
                "case {index} repetition {repetition}"
            );
            assert_eq!(topics(&compiled, &actual), topics(&compiled, &expected));
            if index < 6 || index == 9 {
                let response = shape_response(&compiled, &expected, &catalog, false).unwrap();
                for (boundary, cursor) in
                    response
                        .page_info
                        .get("items")
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
                    let mut next = input.clone();
                    next["items"]["$"][boundary] =
                        json!([cursor.entity_id, cursor.attr_id, value, cursor.created_at]);
                    let compiled = compile_query(&next, &catalog, false).unwrap();
                    let old = query_instaql(&tx, &compiled, app, &options).await.unwrap();
                    let new = query_instaql_with_structural_preparer(
                        &tx,
                        &compiled,
                        app,
                        &options,
                        Duration::from_secs(10),
                        &preparer,
                    )
                    .await
                    .unwrap();
                    assert_eq!(
                        serde_json::to_value(
                            shape_response(&compiled, &new, &catalog, false).unwrap()
                        )
                        .unwrap(),
                        serde_json::to_value(
                            shape_response(&compiled, &old, &catalog, false).unwrap()
                        )
                        .unwrap(),
                        "case {index} {boundary} cursor"
                    );
                    assert_eq!(topics(&compiled, &new), topics(&compiled, &old));
                }
            }
        }
        tx.execute("UPDATE triples SET value=to_jsonb($1::text) WHERE attr_id='00000000-0000-0000-0000-000000000003'",&[&format!("fresh-{repetition}")]).await.unwrap();
        refresh_sketches(&tx).await;
    }
    assert!(!preparer.statements.lock().unwrap().is_empty());
    assert!(
        preparer
            .statements
            .lock()
            .unwrap()
            .keys()
            .any(|sql| sql.contains("LATERAL") && sql.contains("ea_index")),
        "real sketch loading must exercise keyed selectors/pages and projected-field costing"
    );
    let compiled = compile_query(&queries[0], &catalog, false).unwrap();
    struct Blocked;
    impl QueryStatementPreparer for Blocked {
        async fn prepare(&self, _: &str) -> Result<Option<Statement>, tokio_postgres::Error> {
            std::future::pending().await
        }
    }
    let error = query_instaql_with_structural_preparer(
        &tx,
        &compiled,
        Uuid::from_u128(100),
        &options,
        Duration::from_millis(20),
        &Blocked,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PgQueryError::QueryTimeout { .. }));
    // Discard this timed-out transaction just as the production owner does.
    drop(preparer);
    tx.rollback().await.unwrap();
    client.query_one("SELECT 1", &[]).await.unwrap();
    drop(client);
    driver.await.unwrap().unwrap();
}
