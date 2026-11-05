use std::collections::HashMap;

use instant_datalog::{
    MatchCteOptions, MatchQueryContext, NormalizedClause, Term, render_match_query,
};
use instant_instaql::{AttributeCatalog, compile_roots};
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
    query: Value,
    indexes: Vec<String>,
    attrs: Vec<Uuid>,
    functions: usize,
    or_groups: usize,
}

#[test]
fn where_conditions_match_attribute_pattern_and_boolean_planning() {
    let fixture: Fixture =
        serde_json::from_str(include_str!("fixtures/where_compilation.json")).unwrap();
    for (case_index, case) in fixture.cases.into_iter().enumerate() {
        let roots = compile_roots(&case.query, &fixture.catalog, false)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let root = roots.first().unwrap();
        let mut indexes = Vec::new();
        let mut attrs = Vec::new();
        let mut functions = 0;
        let mut or_groups = 0;
        inspect(
            &root.clauses,
            &mut indexes,
            &mut attrs,
            &mut functions,
            &mut or_groups,
        );
        assert_eq!(indexes, case.indexes, "{}", case.name);
        assert_eq!(attrs, case.attrs, "{}", case.name);
        assert_eq!(functions, case.functions, "{}", case.name);
        assert_eq!(or_groups, case.or_groups, "{}", case.name);

        let prefix = format!("where-{case_index}-");
        let additional = HashMap::new();
        let options = MatchCteOptions::default();
        let context = MatchQueryContext {
            prefix: &prefix,
            app_id: Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap(),
            additional_joins: &additional,
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        };
        let plan = root
            .plan(&context)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let rendered = render_match_query(&plan).unwrap();
        assert!(rendered.sql.starts_with("WITH "), "{}", case.name);
    }
}

fn inspect(
    clauses: &[NormalizedClause],
    indexes: &mut Vec<String>,
    attrs: &mut Vec<Uuid>,
    functions: &mut usize,
    or_groups: &mut usize,
) {
    for clause in clauses {
        match clause {
            NormalizedClause::Pattern { pattern } => {
                indexes.push(pattern.index.key.to_string());
                let Term::Constants(values) = &pattern.attribute else {
                    panic!("compiled where attribute must be constant");
                };
                attrs.push(Uuid::parse_str(values[0].as_str().unwrap()).unwrap());
                *functions += usize::from(matches!(pattern.entity, Term::Function(_)))
                    + usize::from(matches!(pattern.value, Term::Function(_)));
            }
            NormalizedClause::And { clauses } => {
                inspect(clauses, indexes, attrs, functions, or_groups);
            }
            NormalizedClause::Or { clauses, .. } => {
                *or_groups += 1;
                inspect(clauses, indexes, attrs, functions, or_groups);
            }
        }
    }
}
