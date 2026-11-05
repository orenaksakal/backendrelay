use instant_datalog::{FunctionClause, NormalizedClause, Term};
use instant_instaql::{AttributeCatalog, compile_query};
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct Fixture {
    oracle: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: Value,
    #[serde(default)]
    expected_attrs: Vec<Uuid>,
    expected_error: Option<String>,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
        .unwrap()
        .catalog
}

#[test]
fn relationship_entity_prefix_paths_match_the_upstream_id_pattern_graph() {
    let catalog = catalog();
    let fixture = serde_json::from_str::<Fixture>(include_str!(
        "fixtures/entity_prefix_path_differential.json"
    ))
    .unwrap();
    assert!(fixture.oracle.starts_with("4e0a1bd8 "));

    for case in fixture.cases {
        if let Some(expected_error) = case.expected_error {
            let error = compile_query(&case.query, &catalog, false)
                .expect_err(&format!("{}: expected validation error", case.name));
            assert!(
                error.to_string().contains(&expected_error),
                "{}: {error}",
                case.name
            );
            continue;
        }
        let query = compile_query(&case.query, &catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let mut attrs = Vec::new();
        let mut prefixes = 0;
        inspect(&query.roots[0].selector_clauses, &mut attrs, &mut prefixes);
        attrs.sort_unstable();
        let mut expected_attrs = case.expected_attrs;
        expected_attrs.sort_unstable();
        assert_eq!(attrs, expected_attrs, "{}", case.name);
        assert_eq!(prefixes, 1, "{}", case.name);
    }
}

fn inspect(clauses: &[NormalizedClause], attrs: &mut Vec<Uuid>, prefixes: &mut usize) {
    for clause in clauses {
        match clause {
            NormalizedClause::Pattern { pattern } => {
                let Term::Constants(values) = &pattern.attribute else {
                    panic!("compiled where attribute must be constant");
                };
                attrs.push(Uuid::parse_str(values[0].as_str().unwrap()).unwrap());
                *prefixes += usize::from(matches!(
                    pattern.entity,
                    Term::Function(FunctionClause::EntityIdStartsWith(_))
                ));
                *prefixes += usize::from(matches!(
                    pattern.value,
                    Term::Function(FunctionClause::EntityIdStartsWith(_))
                ));
            }
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                inspect(clauses, attrs, prefixes);
            }
        }
    }
}
