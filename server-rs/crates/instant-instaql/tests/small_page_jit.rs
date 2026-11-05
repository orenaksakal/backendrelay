use instant_datalog::{MatchCteOptions, QueryHint, render_nested_match_query};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, CompiledQuery,
    PlannedNestedQuery, compile_query, plan_nested_query,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn attribute(id: u128, etype: &str, label: &str) -> Attribute {
    Attribute {
        id: Uuid::from_u128(id),
        wire: None,
        etype: etype.into(),
        label: label.into(),
        value_type: AttributeValueType::Blob,
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

fn compiled(query: &Value) -> CompiledQuery {
    let mut friends = attribute(4, "users", "friends");
    friends.value_type = AttributeValueType::Ref;
    friends.cardinality = Cardinality::Many;
    friends.reverse_etype = Some("users".into());
    friends.reverse_label = Some("followers".into());
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(1, "users", "id"),
            attribute(2, "other", "id"),
            attribute(3, "third", "id"),
            friends,
        ],
    };
    compile_query(query, &catalog, true).unwrap()
}

fn planned(
    query: &CompiledQuery,
    enabled: Option<bool>,
    hints: Option<bool>,
) -> PlannedNestedQuery {
    plan_nested_query(
        query,
        Uuid::from_u128(10),
        &MatchCteOptions {
            small_page_jit_off: enabled,
            pg_hints_enabled: hints,
            ..MatchCteOptions::default()
        },
    )
    .unwrap()
}

#[test]
fn small_page_jit_hint_is_opt_in_and_preserves_all_sql_body_parameters_and_metadata() {
    let query =
        compiled(&json!({"users":{"$":{"limit":20,"fields":["id"]}},"other":{"$":{"first":10}}}));
    let reference = planned(&query, None, None);
    assert_eq!(reference, planned(&query, Some(false), None));
    let mut candidate = planned(&query, Some(true), None);
    assert_eq!(candidate.ctes, reference.ctes);
    assert_eq!(candidate.groups, reference.groups);
    assert_eq!(
        candidate
            .hints
            .iter()
            .filter(|h| **h == QueryHint::DisableJit)
            .count(),
        1
    );
    assert_eq!(
        candidate
            .hints
            .iter()
            .filter(|h| **h != QueryHint::DisableJit)
            .cloned()
            .collect::<Vec<_>>(),
        reference.hints
    );
    candidate
        .hints
        .extend([QueryHint::DisableJit, QueryHint::DisableJit]);
    let old = render_nested_match_query(&reference.sql_plan()).unwrap();
    let new = render_nested_match_query(&candidate.sql_plan()).unwrap();
    assert_eq!(new.parameters, old.parameters);
    assert_eq!(new.sql.matches("Set(jit off)\n").count(), 1);
    let without_jit_hint = new.sql.replace("Set(jit off)\n", "");
    let without_jit_hint = if reference.hints.is_empty() {
        without_jit_hint.replace("/*+\n*/ ", "")
    } else {
        without_jit_hint
    };
    assert_eq!(without_jit_hint, old.sql);
}

#[test]
fn jit_hint_does_not_apply_to_unbounded_bulk_nested_ref_aggregate_or_large_offset_reads() {
    for query in [
        json!({}),
        json!({"users":{}}),
        json!({"users":{"$":{"limit":101}}}),
        json!({"users":{"$":{"limit":10,"offset":101}}}),
        json!({"users":{"$":{"aggregate":"count"}}}),
        json!({"users":{"$":{"limit":2}},"other":{}}),
        json!({"users":{"$":{"limit":2}},"other":{"$":{"limit":2}},"third":{"$":{"limit":2}}}),
        json!({"users":{"$":{"limit":2}},"missing":{"$":{"limit":2}}}),
        json!({"users":{"$":{"limit":2},"friends":{"$":{"limit":2}}}}),
        json!({"users":{"$":{"limit":2,"where":{"friends.id":Uuid::from_u128(20)}}}}),
    ] {
        assert!(
            !planned(&compiled(&query), Some(true), None)
                .hints
                .contains(&QueryHint::DisableJit)
        );
    }
    let mut query = compiled(&json!({"users":{"$":{"limit":2}}}));
    query.roots[0].selector_clauses = vec![query.roots[0].selector_clauses[0].clone(); 9];
    assert!(
        !planned(&query, Some(true), None)
            .hints
            .contains(&QueryHint::DisableJit)
    );
}

#[test]
fn disabling_pg_hints_also_disables_the_statement_only_jit_hint() {
    let query = compiled(&json!({"users":{"$":{"limit":20}}}));
    let candidate = planned(&query, Some(true), Some(false));
    assert!(!candidate.hints.contains(&QueryHint::DisableJit));
    assert!(
        !render_nested_match_query(&candidate.sql_plan())
            .unwrap()
            .sql
            .contains("Set(jit off)")
    );
}
