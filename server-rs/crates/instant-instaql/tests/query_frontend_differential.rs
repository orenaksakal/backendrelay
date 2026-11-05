use instant_datalog::{NormalizedClause, PageOrderType};
use instant_instaql::{AttributeCatalog, WhereCondition, WhereValue, coerce_forms, compile_roots};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixture {
    catalog: AttributeCatalog,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    where_shape: Vec<String>,
    limit: Option<u64>,
    offset: Option<u64>,
    has_order: bool,
    has_page: bool,
    clause_shape: String,
}

#[test]
fn query_frontend_matches_clojure_null_and_boolean_normalization_corpus() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/query_frontend_differential.json")).unwrap();

    for case in fixture.cases {
        let forms = coerce_forms(&case.query, &fixture.catalog)
            .unwrap_or_else(|error| panic!("{} coercion: {error}", case.name));
        let form = forms
            .first()
            .unwrap_or_else(|| panic!("{}: missing form", case.name));
        assert_eq!(
            form.options
                .where_clause
                .iter()
                .map(where_shape)
                .collect::<Vec<_>>(),
            case.where_shape,
            "{}",
            case.name
        );
        assert_eq!(
            form.options.order.is_some(),
            case.has_order,
            "{}",
            case.name
        );

        let roots = compile_roots(&case.query, &fixture.catalog, false)
            .unwrap_or_else(|error| panic!("{} compilation: {error}", case.name));
        let root = roots.first().unwrap();
        assert_eq!(root.page.is_some(), case.has_page, "{}", case.name);
        assert_eq!(
            root.page.as_ref().and_then(|page| page.limit),
            case.limit,
            "{}",
            case.name
        );
        assert_eq!(
            root.page.as_ref().and_then(|page| page.offset),
            case.offset,
            "{}",
            case.name
        );
        if let Some(page) = &root.page {
            assert_eq!(page.order_type, PageOrderType::CreatedAtTimestamp);
        }
        assert_eq!(
            clauses_shape(&root.clauses),
            case.clause_shape,
            "{}",
            case.name
        );
    }
}

fn where_shape(condition: &WhereCondition) -> String {
    match condition {
        WhereCondition::Condition { path, value } => {
            format!("{}={}", path.join("."), value_shape(value))
        }
        WhereCondition::And { conditions } => format!(
            "A({})",
            conditions
                .iter()
                .map(where_shape)
                .collect::<Vec<_>>()
                .join(",")
        ),
        WhereCondition::Or { conditions } => format!(
            "O({})",
            conditions
                .iter()
                .map(where_shape)
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

const fn value_shape(value: &WhereValue) -> &'static str {
    match value {
        WhereValue::Scalar(_) => "scalar",
        WhereValue::In(_) => "in",
        WhereValue::Not(_) => "not",
        WhereValue::IsNull(_) => "is-null",
        WhereValue::Comparator { .. } => "comparator",
        WhereValue::EntityIdStartsWith(_) => "entity-prefix",
    }
}

fn clauses_shape(clauses: &[NormalizedClause]) -> String {
    clauses
        .iter()
        .map(|clause| match clause {
            NormalizedClause::Pattern { pattern } => format!("P({})", pattern.index.key),
            NormalizedClause::And { clauses } => format!("A({})", clauses_shape(clauses)),
            NormalizedClause::Or { clauses, .. } => format!("O({})", clauses_shape(clauses)),
        })
        .collect::<Vec<_>>()
        .join(",")
}
