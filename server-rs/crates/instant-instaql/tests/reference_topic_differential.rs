use instant_datalog::{DatalogValue, TopicPart};
use instant_instaql::{AttributeCatalog, coarse_invalidation_topics, compile_query};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    expected: Vec<ExpectedTopic>,
}

#[derive(Deserialize)]
struct ExpectedTopic {
    index: String,
    attribute: Uuid,
    values: Option<Vec<Uuid>>,
}

#[test]
fn reference_filter_topics_match_the_clojure_uuid_contract() {
    let catalog =
        serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
            .unwrap()
            .catalog;
    let fixture =
        serde_json::from_str::<Fixture>(include_str!("fixtures/reference_topic_differential.json"))
            .unwrap();

    for case in fixture.cases {
        let query = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let topics = coarse_invalidation_topics(&query)
            .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
        for expected in case.expected {
            let topic = topics
                .iter()
                .find(|topic| {
                    topic
                        .indexes
                        .iter()
                        .any(|index| index.to_string() == expected.index)
                        && topic.attribute
                            == TopicPart::Values(vec![DatalogValue::Uuid(expected.attribute)])
                })
                .unwrap_or_else(|| {
                    panic!("{}: missing topic for {}", case.name, expected.attribute)
                });
            let expected_values = expected.values.map_or(TopicPart::Any, |values| {
                TopicPart::Values(values.into_iter().map(DatalogValue::Uuid).collect())
            });
            assert_eq!(topic.value, expected_values, "{}", case.name);
        }
    }
}
