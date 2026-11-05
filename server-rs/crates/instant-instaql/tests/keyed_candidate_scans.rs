use instant_datalog::{
    MatchCteOptions, PredicateOptions, QueryHint, SqlParameter, render_nested_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query, plan_nested_query,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "room", "revision", "updatedAt"]
            .into_iter()
            .enumerate()
            .map(|(i, label)| Attribute {
                id: Uuid::from_u128(i as u128 + 1),
                wire: None,
                etype: "messages".into(),
                label: label.into(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: label == "id",
                setting_unique: false,
                indexed: matches!(label, "room" | "updatedAt"),
                required: false,
                checked_data_type: (label == "updatedAt")
                    .then_some(instant_datalog::DataType::Number),
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

fn options() -> MatchCteOptions {
    let mut options = MatchCteOptions {
        pg_hints_enabled: Some(true),
        candidate_entity_scans: Some(true),
        keyed_small_page_scans: Some(true),
        optional_number_order_scan_hints: Some(false),
        predicate_options: PredicateOptions {
            forward_is_null_exists: true,
            ..Default::default()
        },
        ..Default::default()
    };
    for attr in catalog().attributes {
        options.attribute_cardinalities.insert(attr.id, 5000);
    }
    options
        .attribute_value_cardinalities
        .entry(Uuid::from_u128(2))
        .or_default()
        .extend([
            (json!("small").to_string(), 10),
            (json!("popular").to_string(), 5000),
        ]);
    options
        .attribute_value_cardinalities
        .entry(Uuid::from_u128(3))
        .or_default()
        .insert(json!("nonce").to_string(), 0);
    options
        .attribute_is_null_cardinalities
        .entry(Uuid::from_u128(1))
        .or_default()
        .insert(
            Uuid::from_u128(3),
            instant_datalog::IsNullCardinalities {
                null: 0,
                not_null: 5000,
            },
        );
    options
}

fn plan(query: Value, options: &MatchCteOptions) -> instant_instaql::PlannedNestedQuery {
    plan_nested_query(
        &compile_query(&query, &catalog(), false).unwrap(),
        Uuid::from_u128(100),
        options,
    )
    .unwrap()
}

fn private(room: &str) -> Value {
    json!({"messages":{"$":{"where":{"room":room,"revision":{"$ne":"nonce"}},"limit":10}}})
}

#[test]
fn keyed_native_pages_keep_metadata_and_every_data_parameter() {
    let options = options();
    let mut baseline = options.clone();
    baseline.keyed_small_page_scans = None;
    let old = plan(private("small"), &baseline);
    let new = plan(private("small"), &options);
    assert_eq!(new.groups, old.groups);
    let old = render_nested_match_query(&old.sql_plan()).unwrap();
    let new = render_nested_match_query(&new.sql_plan()).unwrap();
    assert_eq!(new.sql.matches("LATERAL").count(), 3);
    assert!(new.sql.contains("OFFSET 0) AS \"t0\""));
    let data = |parameters: Vec<SqlParameter>| {
        parameters
            .into_iter()
            .filter_map(|p| {
                if let SqlParameter::Json(v) = p {
                    Some(v.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(data(old.parameters), data(new.parameters));
    assert!(!new.sql.contains("small"));
    assert!(!new.sql.contains("nonce"));
}

#[test]
fn broad_unknown_and_disjunctive_only_equalities_do_not_add_keyed_probes() {
    let broad = plan(private("popular"), &options());
    assert!(
        !render_nested_match_query(&broad.sql_plan())
            .unwrap()
            .sql
            .contains("LATERAL")
    );
    let mut unknown = options();
    unknown
        .attribute_value_cardinalities
        .remove(&Uuid::from_u128(2));
    assert!(
        !render_nested_match_query(&plan(private("small"), &unknown).sql_plan())
            .unwrap()
            .sql
            .contains("LATERAL")
    );
    let input = json!({"messages":{"$":{"where":{"or":[{"room":"small"},{"revision":"nonce"}]},"limit":10}}});
    assert!(
        !render_nested_match_query(&plan(input, &options()).sql_plan())
            .unwrap()
            .sql
            .contains("LATERAL")
    );
}

#[test]
fn numeric_candidate_membership_keeps_its_existing_path_and_costed_seed_hint() {
    let input = json!({"messages":{"$":{"where":{"room":"small"},"order":{"updatedAt":"desc"},"limit":20}}});
    let planned = plan(input, &options());
    let sql = render_nested_match_query(&planned.sql_plan()).unwrap().sql;
    assert!(sql.contains(" IN (SELECT "));
    assert!(!sql.contains("LATERAL"));
    assert!(planned.hints.iter().any(|hint|matches!(hint,QueryHint::IndexScan{table_alias,index} if table_alias.ends_with("-seed")&&index=="ave_with_e_index")));
}

#[test]
fn explicit_opt_out_keeps_the_reference_candidate_plan() {
    let mut disabled = options();
    disabled.keyed_small_page_scans = Some(false);
    let mut default = disabled.clone();
    default.keyed_small_page_scans = None;
    assert_eq!(
        plan(private("small"), &disabled),
        plan(private("small"), &default)
    );
}
