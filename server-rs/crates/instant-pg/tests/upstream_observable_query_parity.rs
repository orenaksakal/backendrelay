use instant_datalog::{DataType, MatchCteOptions, render_nested_match_query};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query, plan_nested_query,
    shape_response,
};
use instant_pg::query;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::{NoTls, Transaction, types::Json};
use uuid::Uuid;

mod support;

const APP_ID: Uuid = Uuid::from_u128(0xaaaaaaaa_aaaa_4aaa_8aaa_aaaaaaaaaaaa);
const CREATOR_ID: Uuid = Uuid::from_u128(0xaaaaaaaa_aaaa_4aaa_8aaa_aaaaaaaaaaab);
const USER_ID: Uuid = Uuid::from_u128(0x10000000_0000_4000_8000_000000000001);
const SHELF_ID: Uuid = Uuid::from_u128(0x20000000_0000_4000_8000_000000000001);
const SHARED_ID: Uuid = Uuid::from_u128(0x30000000_0000_4000_8000_000000000001);

const USER_ID_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111111);
const HANDLE_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111112);
const AGE_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111113);
const SHELVES_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111114);
const SHELF_ID_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111115);
const SHELF_NAME_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111116);
const SHELF_ORDER_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111117);
const BOOK_ID_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111118);
const BOOK_FIELD_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111119);
const AUTHOR_ID_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_11111111111a);
const AUTHOR_FIELD_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_11111111111b);
const ITEM_ID_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_11111111111c);
const LABEL_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_11111111111d);
const SCORE_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_11111111111e);
const ACTIVE_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_11111111111f);
const WHEN_ATTR: Uuid = Uuid::from_u128(0x11111111_1111_4111_8111_111111111120);

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    fields_query: Value,
    fields_expected: Value,
    shared_id_query: Value,
    shared_id_expected: Value,
    order_cases: Vec<OrderCase>,
}

#[derive(Deserialize)]
struct OrderCase {
    name: String,
    query: Value,
    expected_ids: Vec<Uuid>,
}

#[derive(Clone)]
struct Triple {
    entity: Uuid,
    attribute: Uuid,
    value: Value,
    created_at: i64,
}

#[tokio::test]
async fn source_adapted_observable_queries_execute_against_migrated_postgres() {
    let Some(database_url) = instant_test_support::test_database_url(
        "source_adapted_observable_queries_execute_against_migrated_postgres",
    ) else {
        return;
    };
    let fixture = fixture();
    assert_source_adapted(&fixture);

    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let transaction = client.transaction().await.unwrap();
    assert_migrated_schema(&transaction).await;
    let catalog = catalog();
    seed_database(&transaction, &catalog).await;

    let fields = execute_and_shape(&transaction, &fixture.fields_query, &catalog).await;
    assert_eq!(fields, fixture.fields_expected);

    let shared_id = execute_and_shape(&transaction, &fixture.shared_id_query, &catalog).await;
    assert_eq!(shared_id, fixture.shared_id_expected);

    for case in fixture.order_cases {
        let response = execute_and_shape(&transaction, &case.query, &catalog).await;
        let ids = response["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| Uuid::parse_str(item["id"].as_str().unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids, case.expected_ids, "{}", case.name);
    }

    transaction.rollback().await.unwrap();
    drop(client);
    connection_task.await.unwrap().unwrap();
}

async fn execute_and_shape(
    transaction: &Transaction<'_>,
    query_value: &Value,
    catalog: &AttributeCatalog,
) -> Value {
    let compiled = compile_query(query_value, catalog, false).unwrap();
    let plan = plan_nested_query(&compiled, APP_ID, &MatchCteOptions::default()).unwrap();
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    assert!(!rendered.sql.is_empty());
    let rows = query(transaction, &rendered).await.unwrap();
    assert_eq!(rows.len(), 1);
    let Json(envelope) = rows[0].try_get::<_, Json<Value>>(0).unwrap();
    let executed = support::decode_envelope(&plan, &envelope).unwrap();
    let response = shape_response(&compiled, &executed, catalog, false).unwrap();
    serde_json::to_value(response.data).unwrap()
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
    let persistence = transaction
        .query_one(
            "SELECT relpersistence::text FROM pg_class
             WHERE oid = 'public.triples'::regclass",
            &[],
        )
        .await
        .unwrap()
        .get::<_, String>(0);
    assert_eq!(persistence, "p", "fixture must use the migrated table");
}

async fn seed_database(transaction: &Transaction<'_>, catalog: &AttributeCatalog) {
    transaction
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&CREATOR_ID, &"observable-query-parity@instant.test"],
        )
        .await
        .unwrap();
    transaction
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, $3)",
            &[&APP_ID, &CREATOR_ID, &"Observable query parity"],
        )
        .await
        .unwrap();

    for (index, attribute) in catalog.attributes.iter().enumerate() {
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        let cardinality = match attribute.cardinality {
            Cardinality::One => "one",
            Cardinality::Many => "many",
        };
        let checked = attribute.checked_data_type.map(data_type_name);
        let forward_ident = ident(index, false);
        let reverse_ident = attribute.reverse_etype.as_ref().map(|_| ident(index, true));
        transaction
            .execute(
                "INSERT INTO attrs
                 (id, app_id, value_type, cardinality, is_unique, is_indexed,
                  forward_ident, reverse_ident, checked_data_type, is_required,
                  etype, label, reverse_etype, reverse_label)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                         $9::text::checked_data_type, $10, $11, $12, $13, $14)",
                &[
                    &attribute.id,
                    &APP_ID,
                    &value_type,
                    &cardinality,
                    &attribute.unique,
                    &attribute.indexed,
                    &forward_ident,
                    &reverse_ident,
                    &checked,
                    &attribute.required,
                    &attribute.etype,
                    &attribute.label,
                    &attribute.reverse_etype,
                    &attribute.reverse_label,
                ],
            )
            .await
            .unwrap();
    }

    for triple in triples() {
        let attribute = catalog
            .attributes
            .iter()
            .find(|attribute| attribute.id == triple.attribute)
            .unwrap();
        insert_triple(transaction, &triple, attribute).await;
    }
}

async fn insert_triple(transaction: &Transaction<'_>, triple: &Triple, attribute: &Attribute) {
    let reference = attribute.value_type == AttributeValueType::Ref;
    let checked = attribute.checked_data_type.map(data_type_name);
    let ea = !reference;
    let eav = reference;
    let ave = checked.is_some();
    let vae = reference;
    transaction
        .execute(
            "INSERT INTO triples
             (app_id, entity_id, attr_id, value, value_md5,
              ea, eav, av, ave, vae, created_at, checked_data_type)
             VALUES ($1, $2, $3, $4, md5(($4::jsonb)::text),
                     $5, $6, false, $7, $8, $9, $10::text::checked_data_type)",
            &[
                &APP_ID,
                &triple.entity,
                &triple.attribute,
                &Json(&triple.value),
                &ea,
                &eav,
                &ave,
                &vae,
                &triple.created_at,
                &checked,
            ],
        )
        .await
        .unwrap();
}

const fn data_type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::String => "string",
        DataType::Number => "number",
        DataType::Boolean => "boolean",
        DataType::Date => "date",
    }
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!(
        "fixtures/upstream_observable_query_parity.json"
    ))
    .unwrap()
}

fn assert_source_adapted(fixture: &Fixture) {
    assert!(fixture.oracle.contains("instantdb/instant@45d3ffbb"));
    assert!(
        fixture
            .oracle
            .contains("not JVM-captured production results")
    );
}

fn triples() -> Vec<Triple> {
    let mut triples = vec![
        triple(USER_ID, USER_ID_ATTR, json!(USER_ID), 1),
        triple(USER_ID, AGE_ATTR, json!(42), 2),
        triple(USER_ID, HANDLE_ATTR, json!("alex"), 3),
        triple(USER_ID, SHELVES_ATTR, json!(SHELF_ID), 4),
        triple(SHELF_ID, SHELF_ID_ATTR, json!(SHELF_ID), 4),
        triple(SHELF_ID, SHELF_NAME_ATTR, json!("Nonfiction"), 5),
        triple(SHELF_ID, SHELF_ORDER_ATTR, json!(1), 6),
        triple(SHARED_ID, BOOK_ID_ATTR, json!(SHARED_ID), 7),
        triple(SHARED_ID, BOOK_FIELD_ATTR, json!("book"), 8),
        triple(SHARED_ID, AUTHOR_ID_ATTR, json!(SHARED_ID), 9),
        triple(SHARED_ID, AUTHOR_FIELD_ATTR, json!("author"), 10),
    ];
    triples.extend(item_triples().into_iter().flatten());
    triples
}

fn item_triples() -> Vec<Vec<Triple>> {
    [
        (
            1,
            Some("same"),
            Some(2),
            Some(true),
            Some("2024-01-02T00:00:00Z"),
        ),
        (
            2,
            Some("same"),
            Some(1),
            Some(false),
            Some("2024-01-01T00:00:00Z"),
        ),
        (
            3,
            Some("zulu"),
            Some(3),
            Some(true),
            Some("2024-01-03T00:00:00Z"),
        ),
        (4, None, None, Some(false), None),
    ]
    .into_iter()
    .map(|(index, label, score, active, when)| {
        let entity = Uuid::from_u128(0x40000000_0000_4000_8000_000000000000 + index);
        let created_at = i64::try_from(index).unwrap() + 20;
        let mut triples = vec![triple(entity, ITEM_ID_ATTR, json!(entity), created_at)];
        for (attribute, value) in [
            (LABEL_ATTR, label.map(Value::from)),
            (SCORE_ATTR, score.map(Value::from)),
            (ACTIVE_ATTR, active.map(Value::from)),
            (WHEN_ATTR, when.map(Value::from)),
        ] {
            if let Some(value) = value {
                triples.push(triple(entity, attribute, value, created_at + 10));
            }
        }
        triples
    })
    .collect()
}

fn triple(entity: Uuid, attribute: Uuid, value: Value, created_at: i64) -> Triple {
    Triple {
        entity,
        attribute,
        value,
        created_at,
    }
}

const fn ident(index: usize, reverse: bool) -> Uuid {
    Uuid::from_u128(0x12121212_1212_4212_8212_000000000000 + index as u128 * 2 + reverse as u128)
}

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: vec![
            attribute(USER_ID_ATTR, "users", "id", None),
            attribute(HANDLE_ATTR, "users", "handle", Some(DataType::String)),
            attribute(AGE_ATTR, "users", "age", Some(DataType::Number)),
            Attribute {
                value_type: AttributeValueType::Ref,
                cardinality: Cardinality::Many,
                reverse_etype: Some("bookshelves".to_owned()),
                reverse_label: Some("owner".to_owned()),
                ..attribute(SHELVES_ATTR, "users", "bookshelves", None)
            },
            attribute(SHELF_ID_ATTR, "bookshelves", "id", None),
            attribute(SHELF_NAME_ATTR, "bookshelves", "name", None),
            attribute(
                SHELF_ORDER_ATTR,
                "bookshelves",
                "order",
                Some(DataType::Number),
            ),
            attribute(BOOK_ID_ATTR, "books", "id", None),
            attribute(BOOK_FIELD_ATTR, "books", "field", None),
            attribute(AUTHOR_ID_ATTR, "authors", "id", None),
            attribute(AUTHOR_FIELD_ATTR, "authors", "field", None),
            attribute(ITEM_ID_ATTR, "items", "id", Some(DataType::String)),
            attribute(LABEL_ATTR, "items", "label", Some(DataType::String)),
            attribute(SCORE_ATTR, "items", "score", Some(DataType::Number)),
            attribute(ACTIVE_ATTR, "items", "active", Some(DataType::Boolean)),
            attribute(WHEN_ATTR, "items", "when", Some(DataType::Date)),
        ],
    }
}

fn attribute(id: Uuid, etype: &str, label: &str, checked: Option<DataType>) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique: label == "id",
        setting_unique: false,
        indexed: label == "id" || checked.is_some(),
        required: label == "id",
        checked_data_type: checked,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}
