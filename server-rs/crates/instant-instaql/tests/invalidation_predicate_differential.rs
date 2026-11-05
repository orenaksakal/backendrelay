use std::collections::HashMap;

use instant_instaql::{
    AttributeCatalog, InvalidationEntity, InvalidationPredicateOutcome, coerce_forms,
    compile_invalidation_predicate,
};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct Fixture {
    catalog: AttributeCatalog,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Expected {
    Supported { entities: Vec<EntityCase> },
    Unsupported { reason: String },
}

#[derive(Deserialize)]
struct EntityCase {
    etype: String,
    attrs: Map<String, Value>,
    matches: bool,
    #[serde(default)]
    fail_open: bool,
}

#[test]
fn entity_invalidation_predicates_match_the_upstream_clojure_corpus() {
    let fixture: Fixture = serde_json::from_str(include_str!(
        "fixtures/invalidation_predicate_differential.json"
    ))
    .unwrap();

    for case in fixture.cases {
        let forms = coerce_forms(&case.query, &fixture.catalog)
            .unwrap_or_else(|error| panic!("{} coercion: {error}", case.name));
        let result = compile_invalidation_predicate(&forms, &fixture.catalog);
        match case.expected {
            Expected::Supported { entities } => {
                let predicate = result.unwrap_or_else(|error| panic!("{}: {error}", case.name));
                for entity in entities {
                    let attrs = entity
                        .attrs
                        .into_iter()
                        .map(|(label, value)| {
                            let attribute = fixture
                                .catalog
                                .forward(&entity.etype, &label)
                                .or_else(|| {
                                    fixture
                                        .catalog
                                        .attributes
                                        .iter()
                                        .find(|attribute| attribute.label == label)
                                })
                                .unwrap_or_else(|| {
                                    panic!("{}: unknown fixture attr {label}", case.name)
                                });
                            (attribute.id, value)
                        })
                        .collect::<HashMap<_, _>>();
                    let expected_match = entity.matches;
                    let expected_fail_open = entity.fail_open;
                    let entity = InvalidationEntity {
                        etype: entity.etype,
                        attrs,
                    };
                    assert_eq!(
                        predicate.matches(&entity),
                        expected_match,
                        "{}: {entity:?}",
                        case.name,
                    );
                    assert_eq!(
                        predicate.match_outcome(&entity),
                        if expected_fail_open {
                            InvalidationPredicateOutcome::FailOpen
                        } else if expected_match {
                            InvalidationPredicateOutcome::Match
                        } else {
                            InvalidationPredicateOutcome::NoMatch
                        },
                        "{}: {entity:?}",
                        case.name,
                    );
                }
            }
            Expected::Unsupported { reason } => {
                let error = result.unwrap_err();
                assert_eq!(error.0, reason, "{}", case.name);
            }
        }
    }
}
