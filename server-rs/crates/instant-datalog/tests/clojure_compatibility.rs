use instant_datalog::{
    Index, IndexKey, NormalizedClause, PatternError, RawPattern, RawTerm, Symbol,
    plan_join_conditions, validate_clause_joinable, validate_joinable,
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct JoinFixture {
    name: String,
    source: String,
    patterns: Vec<RawPattern>,
    expected: Vec<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
struct ValidationFixture {
    name: String,
    source: String,
    patterns: Vec<RawPattern>,
    valid: bool,
    invalid_pattern_index: Option<usize>,
}

#[test]
fn join_conditions_match_the_clojure_test_corpus() {
    let fixtures: Vec<JoinFixture> =
        serde_json::from_str(include_str!("fixtures/join_conditions.json")).unwrap();

    for fixture in fixtures {
        let patterns = fixture
            .patterns
            .into_iter()
            .map(RawPattern::normalize)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let actual: Vec<Vec<Value>> = plan_join_conditions("match-", &patterns)
            .into_iter()
            .map(|conditions| {
                conditions
                    .into_iter()
                    .map(|condition| condition.to_honey_sql_json())
                    .collect()
            })
            .collect();

        assert_eq!(
            actual, fixture.expected,
            "fixture `{}` derived from {}",
            fixture.name, fixture.source
        );
    }
}

#[test]
fn joinability_matches_the_clojure_rules() {
    let fixtures: Vec<ValidationFixture> =
        serde_json::from_str(include_str!("fixtures/join_validation.json")).unwrap();

    for fixture in fixtures {
        let patterns = fixture
            .patterns
            .into_iter()
            .map(RawPattern::normalize)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let result = validate_joinable(&patterns);

        if fixture.valid {
            assert!(
                result.is_ok(),
                "fixture `{}` derived from {} returned {result:?}",
                fixture.name,
                fixture.source
            );
        } else {
            assert_eq!(
                result,
                Err(PatternError::PatternNotJoinable {
                    pattern_index: fixture.invalid_pattern_index.unwrap()
                }),
                "fixture `{}` derived from {}",
                fixture.name,
                fixture.source
            );
        }
    }
}

#[test]
fn an_empty_query_is_rejected() {
    assert_eq!(validate_joinable(&[]), Err(PatternError::EmptyQuery));
}

fn variable(name: &str) -> RawTerm {
    RawTerm::Variable(Symbol::new(name).unwrap())
}

fn normalized_pattern(index: IndexKey, terms: Vec<RawTerm>) -> NormalizedClause {
    NormalizedClause::Pattern {
        pattern: RawPattern::new(Index::new(index), terms)
            .normalize()
            .unwrap(),
    }
}

#[test]
fn nested_joinability_flattens_boolean_groups_and_anchors_the_parent_symbol() {
    let parent = Symbol::new("parent").unwrap();
    let clauses = vec![NormalizedClause::And {
        clauses: vec![NormalizedClause::Or {
            join_symbol: parent.clone(),
            clauses: vec![
                normalized_pattern(
                    IndexKey::Ea,
                    vec![variable("parent"), variable("attribute")],
                ),
                normalized_pattern(
                    IndexKey::Eav,
                    vec![variable("child"), variable("attribute"), variable("value")],
                ),
            ],
        }],
    }];

    assert_eq!(
        validate_clause_joinable(&clauses, &[]),
        Err(PatternError::PatternNotJoinable { pattern_index: 0 })
    );
    assert_eq!(validate_clause_joinable(&clauses, &[parent]), Ok(()));
}

#[test]
fn parent_anchoring_does_not_hide_an_isolated_nested_branch() {
    let parent = Symbol::new("parent").unwrap();
    let clauses = vec![NormalizedClause::Or {
        join_symbol: parent.clone(),
        clauses: vec![
            normalized_pattern(
                IndexKey::Ea,
                vec![variable("parent"), variable("attribute")],
            ),
            normalized_pattern(
                IndexKey::Eav,
                vec![variable("other"), variable("other-attr"), variable("value")],
            ),
        ],
    }];

    assert_eq!(
        validate_clause_joinable(&clauses, &[parent]),
        Err(PatternError::PatternNotJoinable { pattern_index: 1 })
    );
}
