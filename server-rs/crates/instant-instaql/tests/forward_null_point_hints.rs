use instant_datalog::{
    DataType, MatchCteOptions, PredicateOptions, QueryHint, render_nested_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query, plan_nested_query,
};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "ownerId", "revision"]
            .into_iter()
            .enumerate()
            .map(|(n, label)| Attribute {
                id: Uuid::from_u128(n as u128 + 1),
                wire: None,
                etype: "todos".into(),
                label: label.into(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: label == "id",
                setting_unique: false,
                indexed: label != "revision",
                required: false,
                checked_data_type: Some(DataType::String),
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

fn options(estimate: Option<u64>) -> MatchCteOptions {
    let mut options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        pg_hints_enabled: Some(true),
        predicate_options: PredicateOptions {
            forward_is_null_exists: true,
            ..Default::default()
        },
        attribute_cardinalities: BTreeMap::from([
            (Uuid::from_u128(1), 5000),
            (Uuid::from_u128(2), 5000),
            (Uuid::from_u128(3), 5000),
        ]),
        ..Default::default()
    };
    if let Some(estimate) = estimate {
        options.attribute_value_cardinalities.insert(
            Uuid::from_u128(2),
            BTreeMap::from([(json!("subject").to_string(), estimate)]),
        );
    }
    options
}

fn plan(options: &MatchCteOptions) -> instant_instaql::PlannedNestedQuery {
    let catalog = catalog();
    let query=compile_query(&json!({"todos":{"$":{"where":{"ownerId":"subject","revision":{"$ne":"nonce"}},"limit":10}}}), &catalog,false).unwrap();
    plan_nested_query(&query, Uuid::from_u128(100), options).unwrap()
}

fn point_hints(plan: &instant_instaql::PlannedNestedQuery) -> Vec<&QueryHint> {
    plan.hints
        .iter()
        .filter(|hint| {
            matches!(hint,QueryHint::IndexScan {table_alias,index}
        if table_alias.ends_with("-subquery") && index=="triples_pkey")
        })
        .collect()
}

#[test]
fn known_small_forward_null_checks_use_entity_attribute_point_probes() {
    for estimate in [0, 10, 1024] {
        let plan = plan(&options(Some(estimate)));
        assert_eq!(point_hints(&plan).len(), 1, "estimate {estimate}");
        let sql = render_nested_match_query(&plan.sql_plan()).unwrap();
        assert!(sql.sql.contains("triples_pkey"));
    }
}

#[test]
fn unknown_broad_and_disabled_paths_keep_the_previous_hints() {
    let mut cases = vec![options(None), options(Some(1025)), options(Some(5000))];
    let mut disabled = options(Some(10));
    disabled.pg_hints_enabled = Some(false);
    cases.push(disabled);
    let mut disabled = options(Some(10));
    disabled.candidate_entity_scans = Some(false);
    cases.push(disabled);
    let mut legacy = options(Some(10));
    legacy.predicate_options.disable_is_null_fix = true;
    cases.push(legacy);
    let mut legacy = options(Some(10));
    legacy.predicate_options.forward_is_null_exists = false;
    cases.push(legacy);
    for (n, options) in cases.iter().enumerate() {
        assert!(point_hints(&plan(options)).is_empty(), "case {n}");
    }
}

#[test]
fn only_the_new_hint_changes_not_sql_parameters_ctes_or_candidate_join_shape() {
    let current = plan(&options(Some(10)));
    assert_eq!(point_hints(&current).len(), 1);
    let mut prior = current.clone();
    prior.hints.retain(|hint| {
        !matches!(hint,QueryHint::IndexScan {table_alias,index}
        if table_alias.ends_with("-subquery") && index=="triples_pkey")
    });
    assert_eq!(current.ctes, prior.ctes);
    assert_eq!(current.groups, prior.groups);
    let old = render_nested_match_query(&prior.sql_plan()).unwrap();
    let new = render_nested_match_query(&current.sql_plan()).unwrap();
    assert_eq!(old.parameters, new.parameters);
    let without_hints = |sql: &str| {
        let value = if let Some(start) = sql.find("/*+") {
            let end = start + sql[start..].find("*/").unwrap() + 2;
            format!("{}{}", &sql[..start], &sql[end..])
        } else {
            sql.to_owned()
        };
        value.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    assert_eq!(without_hints(&old.sql), without_hints(&new.sql));
}
