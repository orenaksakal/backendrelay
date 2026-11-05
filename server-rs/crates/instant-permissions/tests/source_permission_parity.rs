use instant_permissions::{
    EvaluationBindings, PermissionAction, PermissionError, RefCache, RuleProgram, RuleSet,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    provenance: Provenance,
    cel: Vec<CelCase>,
    persisted: Vec<PersistedCase>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Provenance {
    expectation: String,
    production_capture: String,
    instant: String,
    cel_java: String,
}

#[derive(Deserialize)]
struct CelCase {
    name: String,
    source: String,
    expected: String,
}

#[derive(Deserialize)]
struct PersistedCase {
    name: String,
    rules: Value,
    expected: String,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/source_permission_parity.json"))
        .expect("source permission fixture must parse")
}

#[test]
fn fixture_distinguishes_source_expectations_from_production_captures() {
    let provenance = fixture().provenance;
    assert_eq!(provenance.expectation, "source-derived");
    assert_eq!(provenance.production_capture, "unavailable");
    assert!(provenance.instant.contains("instantdb/instant@45d3ffbb"));
    assert!(provenance.cel_java.contains("cel-java@v0.11.1"));
}

#[test]
fn cel_compile_runtime_and_macro_edges_match_pinned_sources() {
    for case in fixture().cel {
        let compiled = RuleProgram::compile(PermissionAction::View, &case.source);
        if case.expected == "compile-error" {
            assert!(compiled.is_err(), "{} compiled unexpectedly", case.name);
            continue;
        }

        let result = compiled
            .unwrap_or_else(|error| panic!("{} did not compile: {error}", case.name))
            .evaluate_bool(&EvaluationBindings::default());
        match case.expected.as_str() {
            "true" => assert_eq!(result, Ok(true), "{}", case.name),
            "false" => assert_eq!(result, Ok(false), "{}", case.name),
            "evaluation-error" => assert!(
                matches!(result, Err(PermissionError::Evaluation(_))),
                "{} returned {result:?}",
                case.name
            ),
            expected => panic!("unknown fixture expectation {expected}"),
        }
    }
}

#[test]
fn persisted_rule_selection_and_failures_match_pinned_instant_source() {
    for case in fixture().persisted {
        let rules = RuleSet::new(case.rules);
        let program = rules.program("posts", PermissionAction::View);
        if case.expected == "compile-error" {
            assert!(program.is_err(), "{} compiled unexpectedly", case.name);
            continue;
        }

        let program = program
            .unwrap_or_else(|error| panic!("{} did not compile: {error}", case.name))
            .unwrap_or_else(|| panic!("{} did not select a rule", case.name));
        let bindings = EvaluationBindings {
            data: json!({"ownerId": "user-1"}),
            ..EvaluationBindings::default()
        };
        let result = program.evaluate_bool(&bindings);
        match case.expected.as_str() {
            "true" => assert_eq!(result, Ok(true), "{}", case.name),
            "false" => assert_eq!(result, Ok(false), "{}", case.name),
            "evaluation-error" => assert!(
                matches!(result, Err(PermissionError::Evaluation(_))),
                "{} returned {result:?}",
                case.name
            ),
            "invalid-return-type" => {
                assert_eq!(
                    result,
                    Err(PermissionError::InvalidReturnType),
                    "{}",
                    case.name
                );
            }
            expected => panic!("unknown fixture expectation {expected}"),
        }
    }
}

#[test]
fn null_persisted_fields_and_binds_are_absent() {
    let rules = RuleSet::new(json!({
        "posts": {
            "bind": null,
            "allow": {"view": true, "update": null},
            "fields": {"secret": null}
        }
    }));

    assert!(rules.validation_errors().is_empty());
    assert!(rules.field_program("posts", "secret").unwrap().is_none());
    assert_eq!(
        rules
            .filter_view_entities(
                "posts",
                &[json!({"id": "one", "secret": "visible"})],
                &EvaluationBindings::default(),
                &mut RefCache::new(),
                |_| Ok(RefCache::new()),
            )
            .unwrap(),
        vec![json!({"id": "one", "secret": "visible"})]
    );
}

#[test]
fn null_persisted_id_field_is_absent_when_rules_are_loaded() {
    let rules = RuleSet::new(json!({
        "posts": {"fields": {"id": null}}
    }));

    assert!(rules.validation_errors().is_empty());
    assert!(RuleSet::try_new(rules.code().clone()).is_ok());
    assert!(rules.field_program("posts", "id").unwrap().is_none());
}
