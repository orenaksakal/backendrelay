use instant_instaql::{AttributeCatalog, Cursor, coerce_forms, compile_query};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    kind: String,
    query: Value,
    expected_entity: Option<Uuid>,
    expected_attribute: Option<Uuid>,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

#[test]
fn public_uuid_coercion_matches_the_trimmed_36_character_jvm_boundary() {
    let catalog = catalog();
    let fixture =
        serde_json::from_str::<Fixture>(include_str!("fixtures/uuid_coercion_differential.json"))
            .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        match case.kind.as_str() {
            "compile-ok" => {
                compile_query(&case.query, &catalog, false)
                    .unwrap_or_else(|error| panic!("{}: {error}", case.name));
            }
            "compile-error" => {
                let error = compile_query(&case.query, &catalog, false)
                    .expect_err(&format!("{}: expected rejection", case.name));
                assert!(error.to_string().contains("uuid"), "{}: {error}", case.name);
            }
            "normal-cursor" => {
                let query = compile_query(&case.query, &catalog, false)
                    .unwrap_or_else(|error| panic!("{}: {error}", case.name));
                let Some(Cursor::Normal {
                    entity_id, attr_id, ..
                }) = &query.roots[0].options.after
                else {
                    panic!("{}: expected normal cursor", case.name);
                };
                assert_eq!(Some(*entity_id), case.expected_entity, "{}", case.name);
                assert_eq!(Some(*attr_id), case.expected_attribute, "{}", case.name);
            }
            "byop-cursor" => {
                let forms = coerce_forms(&case.query, &catalog)
                    .unwrap_or_else(|error| panic!("{}: {error}", case.name));
                let Some(Cursor::Byop {
                    entity_id, attr_id, ..
                }) = &forms[0].options.after
                else {
                    panic!("{}: expected BYOP cursor", case.name);
                };
                assert_eq!(
                    entity_id
                        .as_str()
                        .and_then(|value| Uuid::parse_str(value).ok()),
                    case.expected_entity,
                    "{}",
                    case.name
                );
                assert_eq!(Some(*attr_id), case.expected_attribute, "{}", case.name);
            }
            kind => panic!("{}: unknown fixture kind {kind}", case.name),
        }
    }
}
