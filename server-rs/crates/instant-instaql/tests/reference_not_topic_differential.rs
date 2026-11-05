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
    expected: ExpectedTopic,
}

#[derive(Deserialize)]
struct ExpectedTopic {
    index: String,
    attribute: Uuid,
    component: String,
    excluded: Uuid,
}

#[test]
fn reference_not_topics_match_the_clojure_uuid_contract() {
    let catalog =
        serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
            .unwrap()
            .catalog;
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/reference_not_topic_differential.json"
    ))
    .unwrap();

    for case in fixture.cases {
        let query = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let topics = coarse_invalidation_topics(&query)
            .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
        let excluded = TopicPart::Not(DatalogValue::Uuid(case.expected.excluded));
        let topic = topics
            .iter()
            .find(|topic| {
                topic
                    .indexes
                    .iter()
                    .any(|index| index.to_string() == case.expected.index)
                    && topic.attribute
                        == TopicPart::Values(vec![DatalogValue::Uuid(case.expected.attribute)])
                    && match case.expected.component.as_str() {
                        "entity" => topic.entity == excluded,
                        "value" => topic.value == excluded,
                        component => panic!("{}: unknown component {component}", case.name),
                    }
            })
            .unwrap_or_else(|| panic!("{}: missing typed reference-not topic", case.name));
        match case.expected.component.as_str() {
            "entity" => assert_eq!(topic.entity, excluded, "{}", case.name),
            "value" => assert_eq!(topic.value, excluded, "{}", case.name),
            _ => unreachable!(),
        }
    }
}
