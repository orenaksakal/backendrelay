use instant_datalog::{DataType, PageOrderType};
use instant_instaql::{Aggregate, AttributeCatalog, InstaqlError, compile_roots};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
struct Fixture {
    catalog: AttributeCatalog,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    admin: bool,
    query: Value,
    expected: Expected,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Expected {
    Success {
        limit: Option<u64>,
        offset: Option<u64>,
        order_attr: Option<Uuid>,
        order_type: Option<String>,
        aggregate: Option<String>,
    },
    Error {
        expected: String,
        path: Vec<String>,
        message_contains: String,
    },
}

#[test]
fn root_form_compilation_matches_authoritative_instaql_cases() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/root_compilation.json")).unwrap();
    for case in fixture.cases {
        let result = compile_roots(&case.query, &fixture.catalog, case.admin);
        match case.expected {
            Expected::Success {
                limit,
                offset,
                order_attr,
                order_type,
                aggregate,
            } => {
                let roots = result.unwrap_or_else(|error| panic!("{}: {error}", case.name));
                let root = roots
                    .first()
                    .unwrap_or_else(|| panic!("{}: no root", case.name));
                assert_eq!(
                    root.page.as_ref().and_then(|page| page.limit),
                    limit,
                    "{}",
                    case.name
                );
                assert_eq!(
                    root.page.as_ref().and_then(|page| page.offset),
                    offset,
                    "{}",
                    case.name
                );
                assert_eq!(
                    root.page.as_ref().map(|page| page.attr_id),
                    order_attr,
                    "{}",
                    case.name
                );
                let actual_type = root.page.as_ref().map(|page| match page.order_type {
                    PageOrderType::CreatedAtTimestamp => "created-at",
                    PageOrderType::Checked(DataType::String) => "string",
                    PageOrderType::Checked(DataType::Number) => "number",
                    PageOrderType::Checked(DataType::Boolean) => "boolean",
                    PageOrderType::Checked(DataType::Date) => "date",
                });
                assert_eq!(actual_type, order_type.as_deref(), "{}", case.name);
                assert_eq!(
                    root.aggregate.map(|aggregate| match aggregate {
                        Aggregate::Count => "count",
                    }),
                    aggregate.as_deref(),
                    "{}",
                    case.name
                );
            }
            Expected::Error {
                expected,
                path,
                message_contains,
            } => {
                let InstaqlError::Validation(errors) = result.unwrap_err() else {
                    panic!("{}: expected validation error", case.name);
                };
                let error = errors.first().unwrap();
                assert_eq!(error.expected, expected, "{}", case.name);
                assert_eq!(error.path, path, "{}", case.name);
                assert!(
                    error.message.contains(&message_contains),
                    "{}: {}",
                    case.name,
                    error.message
                );
            }
        }
    }
}
