use instant_datalog::{DataType, MatchCteOptions, QueryHint, render_nested_match_query};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query, plan_nested_query,
};
use serde_json::json;
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "room", "revision", "updatedAt"]
            .into_iter()
            .enumerate()
            .map(|(index, label)| Attribute {
                id: Uuid::from_u128(index as u128 + 1),
                wire: None,
                etype: "messages".into(),
                label: label.into(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: label == "id",
                setting_unique: false,
                indexed: label != "revision",
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

#[test]
fn nonce_read_uses_entity_probes_but_compound_page_and_hint_controls_keep_their_scope() {
    let input = json!({"messages":{"$":{
        "where":{"room":"never-inline-room","revision":{"$ne":"never-inline-nonce"}},
        "order":{"updatedAt":"desc"},"limit":20,"fields":["id","room"]
    }}});
    let catalog = catalog();
    let app = Uuid::from_u128(100);
    let options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        attribute_cardinalities: BTreeMap::from([(Uuid::from_u128(2), 20_000)]),
        attribute_value_cardinalities: BTreeMap::from([(
            Uuid::from_u128(2),
            BTreeMap::from([("\"never-inline-room\"".into(), 86)]),
        )]),
        ..Default::default()
    };
    let compiled = compile_query(&input, &catalog, false).unwrap();
    let new = plan_nested_query(&compiled, app, &options).unwrap();
    assert!(
        new.hints.contains(&QueryHint::BitmapScan {
            table_alias: "t3".into(),
            index: "triples_pkey".into(),
        }),
        "{:?}",
        new.hints
    );
    let reference = plan_nested_query(
        &compiled,
        app,
        &MatchCteOptions {
            candidate_entity_scans: Some(false),
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(
        new.groups, reference.groups,
        "preserve pages and row/topic metadata"
    );
    let rendered = render_nested_match_query(&new.sql_plan()).unwrap();
    assert!(!rendered.sql.contains("never-inline-room"));
    assert!(!rendered.sql.contains("never-inline-nonce"));
    assert!(rendered.sql.contains("BitmapScan(t3 triples_pkey)"));

    let mut page = input;
    page["messages"]["$"]["where"]["updatedAt"] = json!({"$lt":8});
    let compiled = compile_query(&page, &catalog, false).unwrap();
    let compound = plan_nested_query(&compiled, app, &options).unwrap();
    assert!(!compound.hints.iter().any(|hint| matches!(hint,
        QueryHint::BitmapScan { index, .. } if index == "triples_pkey")));
    let without_hints = plan_nested_query(
        &compiled,
        app,
        &MatchCteOptions {
            pg_hints_enabled: Some(false),
            ..options
        },
    )
    .unwrap();
    assert_eq!(compound.groups, without_hints.groups);
    assert!(!without_hints.hints.iter().any(|hint| matches!(
        hint,
        QueryHint::BitmapScan { .. } | QueryHint::IndexScan { .. }
    )));
}
use std::collections::BTreeMap;
