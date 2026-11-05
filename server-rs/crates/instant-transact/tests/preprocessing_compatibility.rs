use instant_instaql::{Attribute, AttributeCatalog, DeleteAction};
use instant_transact::{
    EntityLookup, PathSegment, TransactionMode, TransactionOperation, TransactionStep,
    compile_admin_transaction, compile_transaction, compile_transaction_all_errors,
    compile_transaction_with_order, resolve_delete_etypes, resolve_delete_lookups,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct PreprocessingFixture {
    source: String,
    cases: Vec<ErrorCase>,
}

#[derive(Deserialize)]
struct ErrorCase {
    name: String,
    input: Value,
    path: Vec<PathSegment>,
    message: String,
}

#[derive(Deserialize)]
struct AggregateFixture {
    source: String,
    cases: Vec<AggregateCase>,
}

#[derive(Deserialize)]
struct ArityFixture {
    source: String,
    cases: Vec<AggregateCase>,
    accepted: Vec<Value>,
}

#[derive(Deserialize)]
struct LookupShapeFixture {
    source: String,
    cases: Vec<AggregateCase>,
    accepted: Vec<Value>,
}

#[derive(Deserialize)]
struct IdentityPredicateFixture {
    source: String,
    cases: Vec<AggregateCase>,
    accepted: Vec<Value>,
}

#[derive(Deserialize)]
struct ObjectReverseIdentityFixture {
    source: String,
    cases: Vec<ObjectReverseIdentityCase>,
}

#[derive(Deserialize)]
struct ObjectReverseIdentityCase {
    name: String,
    input: Value,
    #[serde(default)]
    catalog_append: Vec<Attribute>,
    expected_attr_id: Uuid,
    expected_etype: String,
    expected_reverse_etype: Option<String>,
    expected_value: Value,
}

#[derive(Deserialize)]
struct OptimisticUpdateFixture {
    source: String,
    rejected: Vec<AggregateCase>,
    accepted: Vec<OptimisticUpdateAcceptedCase>,
}

#[derive(Deserialize)]
struct OptimisticUpdateAcceptedCase {
    name: String,
    input: Value,
    attr_id: Uuid,
    etype: Option<String>,
    reverse_etype: Option<String>,
}

#[derive(Deserialize)]
struct AggregateCase {
    name: String,
    input: Value,
    errors: Vec<ExpectedIssue>,
}

#[derive(Deserialize)]
struct ExpectedIssue {
    path: Vec<PathSegment>,
    message: String,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!(
        "../../instant-instaql/tests/fixtures/where_compilation.json"
    ))
    .unwrap()
    .catalog
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

#[test]
fn validation_errors_match_the_clojure_preprocessing_contract() {
    let fixture: PreprocessingFixture =
        serde_json::from_str(include_str!("fixtures/preprocessing.json")).unwrap();
    assert!(fixture.source.contains("transaction.clj"));
    assert!(fixture.source.contains("4e0a1bd8"));

    for case in fixture.cases {
        let error = match compile_transaction(&case.input, &catalog()) {
            Ok(steps) => panic!("{} unexpectedly compiled as {steps:?}", case.name),
            Err(error) => error,
        };
        assert_eq!(error.path, case.path, "{}", case.name);
        assert_eq!(error.message, case.message, "{}", case.name);
    }
}

#[test]
fn aggregate_validation_retains_independent_clojure_spec_paths_in_input_order() {
    let fixture: AggregateFixture =
        serde_json::from_str(include_str!("fixtures/preprocessing_multi.json")).unwrap();
    assert!(fixture.source.contains("transaction.clj"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }
}

#[test]
fn invalid_id_lookups_keep_clojure_values_paths_and_multi_error_order() {
    let fixture: AggregateFixture =
        serde_json::from_str(include_str!("fixtures/invalid_id_lookup.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("transaction_test.clj:3636-3662"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }
}

#[test]
fn core_predicates_keep_exact_wording_paths_and_multi_error_order() {
    let fixture: AggregateFixture =
        serde_json::from_str(include_str!("fixtures/core_predicates.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("attr.clj:57-97"));
    assert!(fixture.source.contains("transaction.clj:25-48"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }
}

#[test]
fn enum_set_and_missing_key_predicates_match_pinned_spec_order() {
    let fixture: AggregateFixture =
        serde_json::from_str(include_str!("fixtures/enum_missing_predicates.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("clojure.spec.alpha"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }
}

#[test]
fn operation_arity_predicates_and_attr_coercion_match_pinned_spec_order() {
    let fixture: ArityFixture =
        serde_json::from_str(include_str!("fixtures/arity_predicates.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("Clojure 1.12.0"));
    assert!(fixture.source.contains("after coerce!"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }

    let steps = compile_transaction(&Value::Array(fixture.accepted), &catalog()).unwrap();
    assert_eq!(steps.len(), 2);
    assert!(matches!(steps[0], TransactionStep::AddAttr(_)));
    assert!(matches!(steps[1], TransactionStep::UpdateAttr(_)));
}

#[test]
fn optimistic_updates_replace_catalog_entries_request_wide_like_upstream_assoc() {
    let fixture: OptimisticUpdateFixture =
        serde_json::from_str(include_str!("fixtures/optimistic_update_replacement.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("optimistic-attrs"));
    assert!(fixture.source.contains("assoc!"));

    for case in fixture.rejected {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }

    for case in fixture.accepted {
        let steps = compile_transaction(&case.input, &catalog())
            .unwrap_or_else(|error| panic!("{} failed: {error}", case.name));
        let triple = steps
            .iter()
            .find_map(|step| match step {
                TransactionStep::AddTriple(triple) if triple.attr_id == case.attr_id => {
                    Some(triple)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{} did not use the replacement identity", case.name));
        assert_eq!(triple.etype, case.etype, "{}", case.name);
        assert_eq!(triple.reverse_etype, case.reverse_etype, "{}", case.name);
        assert!(
            steps
                .iter()
                .any(|step| matches!(step, TransactionStep::UpdateAttr(_))),
            "{}",
            case.name
        );
    }
}

#[test]
fn malformed_lookup_shapes_preserve_spec_and_custom_error_precedence() {
    let fixture: LookupShapeFixture =
        serde_json::from_str(include_str!("fixtures/malformed_lookup_predicates.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("check-for-invalid-entity-ids!"));
    assert!(fixture.source.contains("Clojure 1.12.0"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }

    let steps = compile_transaction(&Value::Array(fixture.accepted), &catalog()).unwrap();
    assert!(
        steps
            .iter()
            .all(|step| matches!(step, TransactionStep::RuleParams(_)))
    );
}

#[test]
fn attribute_identity_shapes_and_nested_predicates_match_pinned_spec() {
    let fixture: IdentityPredicateFixture =
        serde_json::from_str(include_str!("fixtures/identity_predicates.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("attr.clj:57-97"));
    assert!(fixture.source.contains("Clojure 1.12.0"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }

    let steps = compile_transaction(&Value::Array(fixture.accepted), &catalog()).unwrap();
    let TransactionStep::AddAttr(definition) = &steps[0] else {
        panic!("expected accepted blob attribute")
    };
    assert!(definition.reverse_identity.is_none());
}

#[test]
fn direct_attribute_id_positions_use_the_upstream_uuid_predicate() {
    let fixture: AggregateFixture =
        serde_json::from_str(include_str!("fixtures/attribute_id_predicates.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("triple.clj:29-38"));
    assert!(fixture.source.contains("Clojure 1.12.0"));

    for case in fixture.cases {
        let errors = compile_transaction_all_errors(&case.input, &catalog()).unwrap_err();
        assert_eq!(errors.len(), case.errors.len(), "{}", case.name);
        for (actual, expected) in errors.iter().zip(&case.errors) {
            assert_eq!(actual.path, expected.path, "{}", case.name);
            assert_eq!(actual.message, expected.message, "{}", case.name);
        }

        let first = compile_transaction(&case.input, &catalog()).unwrap_err();
        assert_eq!(first.path, case.errors[0].path, "{}", case.name);
        assert_eq!(first.message, case.errors[0].message, "{}", case.name);
    }
}

#[test]
fn object_writes_resolve_reverse_identities_with_forward_first_precedence() {
    let fixture: ObjectReverseIdentityFixture =
        serde_json::from_str(include_str!("fixtures/object_reverse_identity.json")).unwrap();
    assert!(fixture.source.contains("4e0a1bd8"));
    assert!(fixture.source.contains("resolve-attr-id"));
    assert!(fixture.source.contains("expand-maps"));

    for case in fixture.cases {
        let mut case_catalog = catalog();
        case_catalog.attributes.extend(case.catalog_append);
        let steps = compile_transaction(&case.input, &case_catalog)
            .unwrap_or_else(|error| panic!("{} failed: {error}", case.name));
        let triple = steps
            .iter()
            .find_map(|step| match step {
                TransactionStep::AddTriple(triple)
                    if triple.attr_id == case.expected_attr_id
                        && triple.value == case.expected_value =>
                {
                    Some(triple)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{} did not emit the expected triple", case.name));
        assert_eq!(
            triple.etype.as_deref(),
            Some(case.expected_etype.as_str()),
            "{}",
            case.name
        );
        assert_eq!(
            triple.reverse_etype, case.expected_reverse_etype,
            "{}",
            case.name
        );
    }
}

#[test]
fn every_operation_rejects_noncanonical_uuid_strings_like_the_jvm_coercer() {
    let entity = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let compact = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let attribute = json!({
        "id": compact,
        "forward-identity": ["bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "users", "name"],
        "value-type": "blob",
        "cardinality": "one",
        "unique?": false,
        "index?": false
    });
    let cases = [
        (
            "add-triple",
            json!([["add-triple", entity, compact, "value"]]),
            vec![0.into(), 2.into()],
            "uuid?",
        ),
        (
            "deep-merge-triple",
            json!([["deep-merge-triple", entity, compact, {}]]),
            vec![0.into(), 2.into()],
            "uuid?",
        ),
        (
            "retract-triple",
            json!([["retract-triple", entity, compact, "value"]]),
            vec![0.into(), 2.into()],
            "uuid?",
        ),
        (
            "add-attr",
            json!([["add-attr", attribute]]),
            vec![0.into(), 1.into(), "id".into()],
            "uuid?",
        ),
        (
            "update-attr",
            json!([["update-attr", {"id": compact, "index?": true}]]),
            vec![0.into(), 1.into(), "id".into()],
            "uuid?",
        ),
        (
            "delete-attr",
            json!([["delete-attr", compact]]),
            vec![0.into(), 1.into()],
            "uuid?",
        ),
        (
            "restore-attr",
            json!([["restore-attr", compact]]),
            vec![0.into(), 1.into()],
            "uuid?",
        ),
        (
            "delete-entity",
            json!([["delete-entity", compact]]),
            vec![0.into(), 1.into()],
            "Invalid entity ID 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'. Entity IDs must be UUIDs. Use id() or lookup() to generate a valid UUID.",
        ),
        (
            "rule-params",
            json!([["rule-params", compact, {}]]),
            vec![0.into(), 1.into()],
            "vector?",
        ),
    ];

    for (operation, input, path, message) in cases {
        let error = match compile_transaction(&input, &catalog()) {
            Ok(steps) => panic!("{operation} unexpectedly compiled as {steps:?}"),
            Err(error) => error,
        };
        assert_eq!(error.path, path, "{operation}");
        assert_eq!(error.message, message, "{operation}");
    }
}

#[test]
fn unknown_transaction_options_are_ignored_like_clojure_spec_keys() {
    let entity = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let attr = "22222222-2222-2222-2222-222222222222";
    let steps = compile_transaction(
        &json!([
            [
                "add-triple",
                entity,
                attr,
                "one",
                {"mode": "create", "future-option": {"enabled": true}}
            ],
            [
                "deep-merge-triple",
                entity,
                "99999999-9999-9999-9999-999999999999",
                {"nested": true},
                {"client-metadata": "retained by caller only"}
            ]
        ]),
        &catalog(),
    )
    .unwrap();

    let TransactionStep::AddTriple(add) = &steps[0] else {
        panic!("expected add-triple first");
    };
    assert_eq!(add.options.mode, Some(TransactionMode::Create));
    let TransactionStep::DeepMergeTriple(merge) = &steps[1] else {
        panic!("expected deep-merge-triple second");
    };
    assert_eq!(merge.options.mode, None);
}

#[test]
fn every_transaction_operation_compiles_into_a_typed_step() {
    let entity = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let input = json!([
        ["add-triple", entity, "22222222-2222-2222-2222-222222222222", "alex", {"mode": "create"}],
        ["deep-merge-triple", entity, "99999999-9999-9999-9999-999999999999", {"theme": {"dark": true}}, {"mode": "upsert"}],
        ["retract-triple", entity, "22222222-2222-2222-2222-222222222222", "old"],
        ["add-attr", {
            "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "forward-identity": ["cccccccc-cccc-cccc-cccc-cccccccccccc", "users", "city"],
            "value-type": "blob", "cardinality": "one", "unique?": false, "index?": false
        }],
        ["update-attr", {"id": "22222222-2222-2222-2222-222222222222", "unique?": false}],
        ["delete-attr", "33333333-3333-3333-3333-333333333333"],
        ["restore-attr", "33333333-3333-3333-3333-333333333333"],
        ["delete-entity", ["22222222-2222-2222-2222-222222222222", "alex"], "users"],
        ["rule-params", ["22222222-2222-2222-2222-222222222222", "alex"], "users", {"localId": "one"}]
    ]);
    let steps = compile_transaction(&input, &catalog()).unwrap();
    let operations = steps
        .iter()
        .map(TransactionStep::operation)
        .collect::<Vec<_>>();

    assert_eq!(
        operations,
        vec![
            TransactionOperation::AddTriple,
            TransactionOperation::DeepMergeTriple,
            TransactionOperation::RetractTriple,
            TransactionOperation::AddAttr,
            TransactionOperation::UpdateAttr,
            TransactionOperation::DeleteAttr,
            TransactionOperation::RestoreAttr,
            TransactionOperation::DeleteEntity,
            TransactionOperation::RuleParams,
        ]
    );
}

#[test]
fn ordered_compilation_preserves_source_checks_and_grouped_execution() {
    let entity = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let compiled = compile_transaction_with_order(
        &json!([
            ["add-triple", entity, "22222222-2222-2222-2222-222222222222", "first"],
            ["update-attr", {"id": "22222222-2222-2222-2222-222222222222", "index?": true}],
            ["add-triple", entity, "33333333-3333-3333-3333-333333333333", 42]
        ]),
        &catalog(),
    )
    .unwrap();
    assert_eq!(
        compiled
            .source_steps
            .iter()
            .map(TransactionStep::operation)
            .collect::<Vec<_>>(),
        [
            TransactionOperation::AddTriple,
            TransactionOperation::UpdateAttr,
            TransactionOperation::AddTriple,
        ]
    );
    assert_eq!(
        compiled
            .grouped_steps
            .iter()
            .map(TransactionStep::operation)
            .collect::<Vec<_>>(),
        [
            TransactionOperation::AddTriple,
            TransactionOperation::AddTriple,
            TransactionOperation::UpdateAttr,
        ]
    );
}

#[test]
fn optimistic_attributes_support_same_transaction_object_expansion_and_grouping() {
    let entity = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    let input = json!([
        ["add-attr", {
            "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            "forward-identity": ["cccccccc-cccc-cccc-cccc-cccccccccccc", "users", "city"],
            "value-type": "blob", "cardinality": "one", "unique?": false, "index?": false
        }],
        {"id": entity, "etype": "users", "city": "Edmonton"},
        ["delete-attr", "33333333-3333-3333-3333-333333333333"],
        ["add-triple", entity, "22222222-2222-2222-2222-222222222222", "alex"]
    ]);
    let steps = compile_transaction(&input, &catalog()).unwrap();
    let operations = steps
        .iter()
        .map(TransactionStep::operation)
        .collect::<Vec<_>>();

    assert_eq!(operations[0], TransactionOperation::AddAttr);
    assert_eq!(
        operations[1..4],
        [
            TransactionOperation::AddTriple,
            TransactionOperation::AddTriple,
            TransactionOperation::AddTriple,
        ]
    );
    assert_eq!(operations[4], TransactionOperation::DeleteAttr);
    let city = steps
        .iter()
        .find_map(|step| match step {
            TransactionStep::AddTriple(triple)
                if triple.attr_id == uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb") =>
            {
                Some(triple)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(city.value, "Edmonton");
}

#[test]
fn compatible_value_lookup_refs_retain_the_reference_namespace() {
    let steps = compile_transaction(
        &json!([[
            "add-triple",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "44444444-4444-4444-4444-444444444444",
            [
                "55555555-5555-5555-5555-555555555555",
                "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
            ]
        ]]),
        &catalog(),
    )
    .unwrap();
    let TransactionStep::AddTriple(triple) = &steps[0] else {
        panic!("expected add triple");
    };
    assert_eq!(triple.etype.as_deref(), Some("users"));
    assert_eq!(triple.reverse_etype.as_deref(), Some("bookshelves"));
}

#[test]
fn value_lookup_with_unknown_attribute_reports_namespace_error() {
    let error = compile_transaction(
        &json!([[
            "add-triple",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "44444444-4444-4444-4444-444444444444",
            ["aaaaaaaa-9999-9999-9999-999999999999", "missing"]
        ]]),
        &catalog(),
    )
    .unwrap_err();
    assert_eq!(error.path, vec![0.into(), 3.into()]);
    assert_eq!(
        error.message,
        "Invalid lookup. Could not determine namespace from lookup attribute."
    );
}

#[test]
fn delete_lookup_and_etype_resolution_match_the_jvm_preprocessor() {
    let input = json!([
        ["rule-params", ["22222222-2222-2222-2222-222222222222", "alex"], "users", {"localId": "one"}],
        ["delete-entity", ["22222222-2222-2222-2222-222222222222", "alex"]],
        ["delete-entity", ["22222222-2222-2222-2222-222222222222", "missing"]]
    ]);
    let steps = compile_transaction(&input, &catalog()).unwrap();
    let lookup = steps
        .iter()
        .find_map(|step| match step {
            TransactionStep::DeleteEntity(delete) => match &delete.entity {
                EntityLookup::Lookup(lookup) if lookup.value == "alex" => Some(lookup.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    let entity_id = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let resolved = resolve_delete_lookups(&steps, &[(lookup, entity_id)]);
    assert_eq!(
        resolved
            .iter()
            .filter(|step| matches!(step, TransactionStep::DeleteEntity(_)))
            .count(),
        1,
        "the unresolved delete is omitted"
    );
    let resolved = resolve_delete_etypes(
        &resolved,
        &[(entity_id, vec!["users".to_owned(), "profiles".to_owned()])],
    );
    let etypes = resolved
        .iter()
        .filter_map(|step| match step {
            TransactionStep::DeleteEntity(delete) => delete.etype.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(etypes, ["users", "profiles"]);
    let rule_entity = resolved.iter().find_map(|step| match step {
        TransactionStep::RuleParams(params) => Some(&params.entity),
        _ => None,
    });
    assert_eq!(rule_entity, Some(&EntityLookup::Id(entity_id)));
}

#[test]
fn attribute_updates_preserve_required_and_nullable_cascade_fields() {
    let attr_id = uuid("44444444-4444-4444-4444-444444444444");
    let steps = compile_transaction(
        &json!([["update-attr", {
            "id": attr_id,
            "required?": true,
            "on-delete": "cascade",
            "on-delete-reverse": null
        }]]),
        &catalog(),
    )
    .unwrap();
    let TransactionStep::UpdateAttr(update) = &steps[0] else {
        panic!("expected update-attr");
    };
    assert_eq!(update.required, Some(true));
    assert_eq!(update.on_delete, Some(Some(DeleteAction::Cascade)));
    assert_eq!(update.on_delete_reverse, Some(None));
}

#[test]
fn admin_update_and_link_tuples_infer_schema_and_lower_to_triples() {
    let post_id = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let comment_id = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let steps = compile_admin_transaction(
        &json!([
            ["update", "posts", post_id, {"title": "Hello"}],
            ["update", "comments", comment_id, {"text": "Great"}],
            ["link", "posts", post_id, {"comments": comment_id}]
        ]),
        &AttributeCatalog::default(),
        false,
    )
    .unwrap();

    let added_attributes = steps
        .iter()
        .filter_map(|step| match step {
            TransactionStep::AddAttr(definition) => Some(definition.query_attribute()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(added_attributes.len(), 5);
    assert!(
        added_attributes
            .iter()
            .any(|attr| { attr.etype == "posts" && attr.label == "id" && attr.unique })
    );
    assert!(
        added_attributes
            .iter()
            .any(|attr| { attr.etype == "comments" && attr.label == "id" && attr.unique })
    );
    let comments = added_attributes
        .iter()
        .find(|attr| attr.etype == "posts" && attr.label == "comments")
        .unwrap();
    assert_eq!(comments.reverse_etype.as_deref(), Some("comments"));
    assert_eq!(comments.reverse_label.as_deref(), Some("posts"));

    let triples = steps
        .iter()
        .filter_map(|step| match step {
            TransactionStep::AddTriple(triple) => Some(triple),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(triples.len(), 5);
    assert!(triples.iter().any(|triple| {
        triple.entity == EntityLookup::Id(post_id)
            && triple.value == json!(comment_id)
            && triple.reverse_etype.as_deref() == Some("comments")
    }));
}

#[test]
fn admin_tuples_reuse_declared_schema_and_honor_missing_attribute_mode() {
    let user_id = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let shelf_id = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let declared = catalog();
    let steps = compile_admin_transaction(
        &json!([
            ["update", "users", user_id, {"handle": "alex"}],
            ["link", "users", user_id, {"bookshelves": shelf_id}]
        ]),
        &declared,
        true,
    )
    .unwrap();
    assert!(
        steps
            .iter()
            .all(|step| !matches!(step, TransactionStep::AddAttr(_)))
    );
    assert_eq!(
        steps
            .iter()
            .filter(|step| matches!(step, TransactionStep::AddTriple(_)))
            .count(),
        3
    );

    let error = compile_admin_transaction(
        &json!([["update", "users", user_id, {"missing": true}]]),
        &declared,
        true,
    )
    .unwrap_err();
    assert!(error.message.contains("users.missing"));
}

#[test]
fn admin_frontend_lowers_every_official_data_action() {
    let post_id = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let comment_id = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let steps = compile_admin_transaction(
        &json!([
            ["create", "posts", post_id, {"title": "Hello"}],
            ["link", "posts", post_id, {"comments": comment_id}],
            ["unlink", "posts", post_id, {"comments": comment_id}],
            ["merge", "posts", post_id, {"settings": {"theme": "dark"}}],
            ["ruleParams", "posts", post_id, {"role": "editor"}],
            ["delete", "posts", post_id]
        ]),
        &AttributeCatalog::default(),
        false,
    )
    .unwrap();

    let create_triples = steps
        .iter()
        .filter_map(|step| match step {
            TransactionStep::AddTriple(triple)
                if triple.options.mode == Some(TransactionMode::Create) =>
            {
                Some(triple)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(create_triples.len(), 2);
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TransactionStep::DeepMergeTriple(_)))
    );
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TransactionStep::RetractTriple(_)))
    );
    let rule_params = steps.iter().find_map(|step| match step {
        TransactionStep::RuleParams(params) => Some(params),
        _ => None,
    });
    assert_eq!(rule_params.unwrap().params["role"], "editor");
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TransactionStep::DeleteEntity(_)))
    );
}

#[test]
fn admin_lookup_id_markers_preserve_lookup_values_and_all_wire_forms() {
    let declared = catalog();
    let handle_attr = declared.forward("users", "handle").unwrap().id;
    let id_attr = declared.forward("users", "id").unwrap().id;
    for lookup in [
        json!(["handle", "alex"]),
        json!({"handle": "alex"}),
        json!("lookup__handle__\"alex\""),
    ] {
        let steps = compile_admin_transaction(
            &json!([["update", "users", lookup, {"name": "Alex"}]]),
            &declared,
            false,
        )
        .unwrap();
        let id_marker = steps.iter().find_map(|step| match step {
            TransactionStep::AddTriple(triple) if triple.attr_id == id_attr => Some(triple),
            _ => None,
        });
        let id_marker = id_marker.unwrap();
        assert_eq!(
            id_marker.entity,
            EntityLookup::Lookup(instant_transact::LookupRef {
                attr_id: handle_attr,
                value: json!("alex"),
            })
        );
        assert_eq!(id_marker.value, json!([handle_attr, "alex"]));
    }

    let target = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let linked = compile_admin_transaction(
        &json!([["link", "users", ["handle", "alex"], {"bookshelves": target}]]),
        &declared,
        true,
    )
    .unwrap();
    assert!(linked.iter().any(|step| match step {
        TransactionStep::AddTriple(triple) => {
            triple.attr_id == id_attr && triple.value == json!([handle_attr, "alex"])
        }
        _ => false,
    }));
}

#[test]
fn admin_lookup_schema_is_request_wide_and_link_lists_remain_distinct_targets() {
    let source = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let steps = compile_admin_transaction(
        &json!([
            ["update", "users", source, {"email": "first@example.com"}],
            ["update", "users", ["email", "first@example.com"], {"name": "First"}],
            ["link", "users", source, {"friends": [
                "lookup__email__\"a@example.com\"",
                "lookup__email__\"b@example.com\""
            ]}]
        ]),
        &AttributeCatalog::default(),
        false,
    )
    .unwrap();
    let attributes = steps
        .iter()
        .filter_map(|step| match step {
            TransactionStep::AddAttr(definition) => Some(definition.query_attribute()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let email = attributes
        .iter()
        .find(|attribute| attribute.etype == "users" && attribute.label == "email")
        .unwrap();
    assert!(email.unique);
    assert!(email.indexed);
    let friends = attributes
        .iter()
        .find(|attribute| attribute.etype == "users" && attribute.label == "friends")
        .unwrap();
    assert!(friends.unique);
    assert_eq!(friends.cardinality, instant_instaql::Cardinality::One);
    let linked_targets = steps
        .iter()
        .filter(|step| match step {
            TransactionStep::AddTriple(triple) => triple.attr_id == friends.id,
            _ => false,
        })
        .count();
    assert_eq!(linked_targets, 2);
}

#[test]
fn admin_unlink_does_not_create_target_lookup_schema_and_id_lookup_is_indexed() {
    let source = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let error = compile_admin_transaction(
        &json!([[
            "unlink",
            "users",
            source,
            {"friends": "lookup__email__\"missing@example.com\""}
        ]]),
        &AttributeCatalog::default(),
        false,
    )
    .unwrap_err();
    assert!(error.message.contains("unknown lookup attribute"));

    let looked_up_id = uuid("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
    let steps = compile_admin_transaction(
        &json!([[
            "update",
            "users",
            ["id", looked_up_id],
            {"name": "Lookup"},
            {"upsert": null}
        ]]),
        &AttributeCatalog::default(),
        false,
    )
    .unwrap();
    let id_attribute = steps.iter().find_map(|step| match step {
        TransactionStep::AddAttr(definition)
            if definition.forward_identity.etype == "users"
                && definition.forward_identity.label == "id" =>
        {
            Some(definition.query_attribute())
        }
        _ => None,
    });
    let id_attribute = id_attribute.unwrap();
    assert!(id_attribute.unique);
    assert!(id_attribute.indexed);
}

#[test]
fn admin_rule_params_can_precede_a_later_lookup_creation() {
    let steps = compile_admin_transaction(
        &json!([
            ["ruleParams", "users", ["email", "later@example.com"], {"role": "owner"}],
            ["create", "users", ["email", "later@example.com"], {"name": "Later"}]
        ]),
        &AttributeCatalog::default(),
        false,
    )
    .unwrap();
    assert!(matches!(steps.last(), Some(TransactionStep::AddTriple(_))));
    assert!(
        steps
            .iter()
            .any(|step| matches!(step, TransactionStep::RuleParams(_)))
    );
}

#[test]
fn admin_update_upsert_parameter_uses_clojure_truthiness() {
    const ORACLE: &str =
        "instantdb/instant@45d3ffbb server/src/instant/admin/model.clj:159-167 convert-opts";
    assert!(ORACLE.contains("45d3ffbb"));
    let entity = uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
    let cases = [
        (json!({}), None),
        (json!({"upsert": null}), None),
        (json!({"upsert": false}), Some(TransactionMode::Update)),
        (json!({"upsert": true}), Some(TransactionMode::Upsert)),
        (json!({"upsert": "false"}), Some(TransactionMode::Upsert)),
        (json!({"upsert": 0}), Some(TransactionMode::Upsert)),
    ];

    for (options, expected) in cases {
        let steps = compile_admin_transaction(
            &json!([["update", "users", entity, {"name": "Alex"}, options]]),
            &catalog(),
            false,
        )
        .unwrap_or_else(|error| panic!("{options} failed: {error}"));
        let modes = steps
            .iter()
            .filter_map(|step| match step {
                TransactionStep::AddTriple(triple) => Some(triple.options.mode),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!modes.is_empty(), "{options}");
        assert!(modes.iter().all(|mode| *mode == expected), "{options}");
    }
}

#[test]
fn blob_arrays_are_values_not_lookup_references() {
    const ORACLE: &str = "instantdb/instant@4e0a1bd8 server/test/instant/db/transaction_test.clj:5663-5678 array-as-lookup";
    assert!(ORACLE.contains("4e0a1bd8"));
    let array = json!([
        "2473c57f-6a58-4167-96b7-cf9e034a670a",
        "7758b4b1-7c0b-4c90-9532-f41a54812f76"
    ]);
    let steps = compile_transaction(
        &json!([[
            "add-triple",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            "22222222-2222-2222-2222-222222222222",
            array
        ]]),
        &catalog(),
    )
    .unwrap();
    let TransactionStep::AddTriple(triple) = &steps[0] else {
        panic!("expected add-triple");
    };
    assert_eq!(triple.value, array);
}
