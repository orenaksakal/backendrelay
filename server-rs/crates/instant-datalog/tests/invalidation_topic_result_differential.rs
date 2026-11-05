use std::collections::HashMap;

use instant_datalog::{
    Clause, MatchCteOptions, MatchQueryContext, PagePlanMetadata, PatternMetadataNode,
    ResultDecodeOptions, TopicPart, decode_paged_query_rows_with_options,
    decode_query_rows_with_options, plan_match_query,
};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    clauses: Vec<Clause>,
    expected_entity_parts: Vec<String>,
    paged: bool,
    empty_page_has_previous: bool,
}

#[test]
fn empty_and_paged_topics_match_the_clojure_result_state_machine() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/invalidation_topic_results.json")).unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        let clauses = case
            .clauses
            .iter()
            .map(Clause::normalize)
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let plan = plan_match_query(
            &MatchQueryContext {
                prefix: "match-",
                app_id: Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap(),
                additional_joins: &HashMap::new(),
                initial_symbol_cardinalities: &HashMap::new(),
                options: &MatchCteOptions::default(),
            },
            &clauses,
        )
        .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let options = ResultDecodeOptions {
            ensure_empty_symbol_values_for_topics: true,
        };
        let result = if case.paged {
            let cursor_columns = plan
                .pattern_metadata
                .iter()
                .find_map(|node| match node {
                    PatternMetadataNode::Pattern(metadata) => Some(metadata.cte_columns.clone()),
                    PatternMetadataNode::Or(_) => None,
                })
                .expect("fixture has a page cursor pattern");
            let page = PagePlanMetadata {
                table: "page".to_owned(),
                has_next_table: "page-has-next".to_owned(),
                has_previous_table: "page-has-previous".to_owned(),
                cursor_columns,
                fallback_entity_column: "page-entity".to_owned(),
                fallback_created_at_column: "page-created-at".to_owned(),
                attr_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap(),
                reverse_results: false,
                empty_page_has_previous: case.empty_page_has_previous,
            };
            let result = decode_paged_query_rows_with_options(
                &[],
                &plan.pattern_metadata,
                &page,
                false,
                false,
                options,
            )
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
            assert_eq!(
                result.page_info.as_ref().unwrap().has_previous_page,
                case.empty_page_has_previous,
                "{}",
                case.name
            );
            result
        } else {
            decode_query_rows_with_options(&[], &plan.pattern_metadata, options)
                .unwrap_or_else(|error| panic!("{}: {error}", case.name))
        };

        assert_eq!(
            result.topics.len(),
            case.expected_entity_parts.len(),
            "{}",
            case.name
        );
        for (topic, expected) in result.topics.iter().zip(&case.expected_entity_parts) {
            match expected.as_str() {
                "any" => assert_eq!(topic.entity, TopicPart::Any, "{}", case.name),
                "empty" => assert_eq!(topic.entity, TopicPart::Values(Vec::new()), "{}", case.name),
                expected => panic!("{}: unknown expected topic part {expected}", case.name),
            }
        }
    }
}
