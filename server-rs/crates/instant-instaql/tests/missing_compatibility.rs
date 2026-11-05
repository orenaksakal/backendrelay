use instant_instaql::{AttributeCatalog, CompiledNode, compile_query};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct MissingFixture {
    source: String,
    cases: Vec<MissingCase>,
}

#[derive(Deserialize)]
struct MissingCase {
    name: String,
    query: Value,
    admin: bool,
    missing_paths: Vec<String>,
}

#[test]
fn missing_schema_cases_match_the_guarded_clojure_contract() {
    let catalog =
        serde_json::from_str::<CatalogFixture>(include_str!("fixtures/where_compilation.json"))
            .unwrap()
            .catalog;
    let fixture: MissingFixture =
        serde_json::from_str(include_str!("fixtures/missing_attributes.json")).unwrap();
    assert!(fixture.source.contains("instaql_test.clj:3672"));

    for case in fixture.cases {
        let query = compile_query(&case.query, &catalog, case.admin)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let mut missing_paths = Vec::new();
        for root in &query.roots {
            collect_missing(root, "", &mut missing_paths);
        }
        assert_eq!(missing_paths, case.missing_paths, "{}", case.name);

        let groups = query
            .pattern_groups()
            .unwrap_or_else(|error| panic!("{}: {error:?}", case.name));
        assert_eq!(
            count_missing_groups(&groups),
            case.missing_paths.len(),
            "{}",
            case.name
        );
    }
}

fn collect_missing(node: &CompiledNode, parent: &str, paths: &mut Vec<String>) {
    let path = if parent.is_empty() {
        node.key.clone()
    } else {
        format!("{parent}.{}", node.key)
    };
    if node.missing {
        paths.push(path);
        return;
    }
    for child in &node.children {
        collect_missing(child, &path, paths);
    }
}

fn count_missing_groups(groups: &[instant_instaql::NestedPatternGroup]) -> usize {
    groups
        .iter()
        .map(|group| {
            usize::from(group.missing)
                + group
                    .children
                    .as_ref()
                    .map_or(0, |children| count_missing_groups(&children.groups))
        })
        .sum()
}
