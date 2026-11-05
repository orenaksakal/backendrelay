use instant_instaql::{AttributeCatalog, InstaqlError, coerce_forms, compile_roots};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    catalog: AttributeCatalog,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    fields: Vec<Value>,
    expected_attr_ids: Vec<Uuid>,
}

/// Pinned to `coerce-option-map!` and `etype-attr-ids` in upstream
/// `instaql.clj` at 4e0a1bd8: `fields` must be sequential, but its members are
/// retained without a string predicate and unmatched values select no attr.
#[test]
fn heterogeneous_field_members_match_clojure_projection_coercion() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/fields_member_differential.json")).unwrap();

    for case in fixture.cases {
        let query = json!({"users": {"$": {"fields": case.fields}}});
        let forms = coerce_forms(&query, &fixture.catalog)
            .unwrap_or_else(|error| panic!("{} coercion: {error}", case.name));
        assert_eq!(
            forms[0].options.fields.as_ref(),
            query["users"]["$"]["fields"].as_array(),
            "{} normalized members",
            case.name
        );

        let roots = compile_roots(&query, &fixture.catalog, false)
            .unwrap_or_else(|error| panic!("{} compilation: {error}", case.name));
        assert_eq!(
            roots[0].selected_attr_ids, case.expected_attr_ids,
            "{} selected attrs",
            case.name
        );
    }

    for fields in [json!("handle"), json!({"0": "handle"})] {
        let InstaqlError::Validation(errors) = compile_roots(
            &json!({"users": {"$": {"fields": fields}}}),
            &fixture.catalog,
            false,
        )
        .unwrap_err() else {
            panic!("non-sequential fields must fail validation");
        };
        assert_eq!(errors[0].expected, "array?");
        assert_eq!(errors[0].path, ["users", "$", "fields"]);
    }
}
