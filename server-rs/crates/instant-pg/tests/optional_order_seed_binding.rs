use std::collections::{BTreeMap, BTreeSet};

use instant_datalog::{
    DataType, InvalidationTopic, MatchCteOptions, PlannedCte, PredicateOptions, TopicPart,
    WherePredicate, render_nested_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality,
    collect_query_invalidation_topics, compile_query, plan_nested_query, shape_response,
};
use serde_json::json;
use tokio_postgres::NoTls;
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "room", "updatedAt"]
            .into_iter()
            .enumerate()
            .map(|(n, label)| Attribute {
                id: Uuid::from_u128(n as u128 + 1),
                wire: None,
                etype: "messages".into(),
                label: label.into(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: label == "id",
                setting_unique: false,
                indexed: matches!(label, "id" | "room" | "updatedAt"),
                required: label == "id",
                checked_data_type: (label == "updatedAt").then_some(DataType::Number),
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            })
            .collect(),
    }
}

fn canonical_topics(topics: Vec<InvalidationTopic>) -> BTreeSet<String> {
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

fn membership_count(plan: &instant_instaql::PlannedNestedQuery) -> usize {
    fn membership(predicate: &WherePredicate) -> bool {
        match predicate {
            WherePredicate::In { query, .. } => query.table.contains("-candidate-"),
            WherePredicate::And(values) | WherePredicate::Or(values) => {
                values.iter().any(membership)
            }
            _ => false,
        }
    }
    plan.ctes
        .iter()
        .filter(|cte| matches!(cte, PlannedCte::Match(cte) if membership(&cte.predicate)))
        .count()
}

#[tokio::test]
async fn optional_order_seed_binding_preserves_room_pages_but_drops_redundant_membership() {
    let Some(url) = instant_test_support::test_database_url(
        "optional_order_seed_binding_preserves_room_pages_but_drops_redundant_membership",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    let task = tokio::spawn(connection);
    let tx = client.transaction().await.unwrap();
    let schema = format!("optional_order_{}", Uuid::new_v4().simple());
    tx.batch_execute(&format!(
        "CREATE SCHEMA {schema}; SET LOCAL search_path={schema},pg_catalog;
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
         SET LOCAL statement_timeout='10s';"
    ))
    .await
    .unwrap();
    let app = Uuid::from_u128(500);
    // 300 room messages plus 300 messages in other rooms. The app-wide
    // updated-at partition is twice the room's, so a whole-partition scan is
    // distinguishable from the seed join by its row count.
    tx.execute(
        "INSERT INTO triples SELECT $1,md5(n::text)::uuid,v.attr,v.value,true,false,v.av,v.ave,false,n::bigint,v.dtype
         FROM generate_series(1,600) n CROSS JOIN LATERAL (VALUES
           ('00000000-0000-0000-0000-000000000001'::uuid,to_jsonb(md5(n::text)::uuid::text),true,true,NULL::checked_data_type),
           ('00000000-0000-0000-0000-000000000002'::uuid,to_jsonb(CASE WHEN n<=300 THEN 'hot' ELSE 'other-'||(n%4) END),true,true,NULL::checked_data_type),
           ('00000000-0000-0000-0000-000000000003'::uuid,to_jsonb(n),true,true,'number'::checked_data_type)
         )v(attr,value,av,ave,dtype)",
        &[&app],
    )
    .await
    .unwrap();
    tx.batch_execute("ANALYZE triples").await.unwrap();
    let hot: i64 = tx
        .query_one(
            "SELECT count(*) FROM triples WHERE app_id=$1 AND attr_id='00000000-0000-0000-0000-000000000002'::uuid AND value='\"hot\"'::jsonb",
            &[&app],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(hot, 300);

    let catalog = catalog();
    let base = MatchCteOptions {
        candidate_entity_scans: Some(true),
        pg_hints_enabled: Some(false),
        predicate_options: PredicateOptions {
            forward_is_null_exists: true,
            ..Default::default()
        },
        attribute_cardinalities: catalog
            .attributes
            .iter()
            .map(|attribute| (attribute.id, 600))
            .collect(),
        attribute_value_cardinalities: BTreeMap::from([(
            Uuid::from_u128(2),
            BTreeMap::from([("\"hot\"".into(), 300)]),
        )]),
        ..Default::default()
    };
    let on = MatchCteOptions {
        optional_order_seed_binding: Some(true),
        ..base.clone()
    };
    let off = MatchCteOptions {
        optional_order_seed_binding: Some(false),
        ..base
    };
    let input = json!({"messages":{"$":{"where":{"room":"hot"},"order":{"updatedAt":"desc"},"limit":20,"fields":["id","room","updatedAt"]}}});
    let compiled = compile_query(&input, &catalog, false).unwrap();
    let on_plan = plan_nested_query(&compiled, app, &on).unwrap();
    let off_plan = plan_nested_query(&compiled, app, &off).unwrap();
    assert_eq!(membership_count(&off_plan), 1);
    assert_eq!(
        membership_count(&on_plan),
        0,
        "the optional-order relation relies on its seed join"
    );
    assert_eq!(on_plan.groups, off_plan.groups);
    assert_eq!(
        render_nested_match_query(&on_plan.sql_plan())
            .unwrap()
            .parameters,
        render_nested_match_query(&off_plan.sql_plan())
            .unwrap()
            .parameters,
        "physical plan selection adds no data parameter"
    );

    let a = instant_pg::query_instaql(&tx, &compiled, app, &off)
        .await
        .unwrap();
    let b = instant_pg::query_instaql(&tx, &compiled, app, &on)
        .await
        .unwrap();
    let a_response = shape_response(&compiled, &a, &catalog, false).unwrap();
    let b_response = shape_response(&compiled, &b, &catalog, false).unwrap();
    assert_eq!(
        serde_json::to_value(&a_response).unwrap(),
        serde_json::to_value(&b_response).unwrap(),
        "rows, order, cursor metadata and has-next flags"
    );
    assert_eq!(a_response.data["messages"].as_array().unwrap().len(), 20);
    assert_eq!(a_response.data["messages"][0]["updatedAt"], 300);
    assert_eq!(a_response.data["messages"][19]["updatedAt"], 281);
    assert_eq!(
        canonical_topics(collect_query_invalidation_topics(&compiled, &a).unwrap()),
        canonical_topics(collect_query_invalidation_topics(&compiled, &b).unwrap())
    );
    tx.rollback().await.unwrap();
    drop(client);
    task.await.unwrap().unwrap();
}
