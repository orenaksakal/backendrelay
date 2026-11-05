use std::collections::{BTreeMap, HashMap};

use instant_datalog::{
    BindingPath, BindingTree, Component, Index, IndexKey, MatchCteContext, MatchCteOptions,
    MatchCtePlan, NestedSymbolMap, QueryHint, RawPattern, RawTerm, Symbol, build_match_cte,
};
use serde_json::json;
use uuid::Uuid;

fn entity() -> Symbol {
    Symbol::new("?entity").unwrap()
}

fn path(index: usize, component: Component) -> BindingTree {
    BindingTree::Path(BindingPath::new(index, component, IndexKey::Eav))
}

fn alternatives() -> BindingTree {
    BindingTree::Or(vec![
        BindingTree::And(vec![path(0, Component::Entity)]),
        BindingTree::And(vec![path(1, Component::Entity)]),
    ])
}

fn plan(bindings: Vec<BindingTree>, candidates: Option<bool>, hints: bool) -> MatchCtePlan {
    let attribute = Uuid::from_u128(9);
    let pattern = RawPattern::new(
        Index::new(IndexKey::Ave),
        vec![
            RawTerm::Variable(entity()),
            RawTerm::Constant(json!(attribute)),
            RawTerm::Constant(json!("synthetic-room")),
        ],
    )
    .normalize()
    .unwrap();
    let options = MatchCteOptions {
        candidate_entity_scans: candidates,
        pg_hints_enabled: Some(hints),
        attribute_cardinalities: BTreeMap::from([(attribute, 20_000)]),
        attribute_value_cardinalities: BTreeMap::from([(
            attribute,
            BTreeMap::from([("\"synthetic-room\"".into(), 86)]),
        )]),
        ..Default::default()
    };
    let symbols = NestedSymbolMap::from([(entity(), bindings)]);
    build_match_cte(
        &MatchCteContext {
            prefix: "m-",
            app_id: Uuid::from_u128(100),
            pattern_index: 3,
            start_of_group: false,
            symbol_map: &symbols,
            symbol_cardinalities: &HashMap::new(),
            additional_joins: &HashMap::new(),
            options: &options,
            row_estimate: None,
            suppress_row_hint: false,
        },
        &pattern,
    )
    .unwrap()
}

#[test]
fn one_entity_disjunction_uses_primary_key_bitmap_probes_without_rewriting_the_cte() {
    let old = plan(vec![alternatives()], Some(false), true);
    assert!(old.hints.contains(&QueryHint::IndexScan {
        table_alias: "t3".into(),
        index: "ave_with_e_index".into(),
    }));
    let new = plan(vec![alternatives()], Some(true), true);
    assert!(new.hints.contains(&QueryHint::BitmapScan {
        table_alias: "t3".into(),
        index: "triples_pkey".into(),
    }));
    assert_eq!(new.cte, old.cte, "retain every predicate, column and fence");
    assert_eq!(new.pattern, old.pattern);
    let mut reference = new.hints;
    for hint in &mut reference {
        if matches!(hint, QueryHint::BitmapScan { table_alias, index }
            if table_alias == "t3" && index == "triples_pkey")
        {
            *hint = QueryHint::IndexScan {
                table_alias: "t3".into(),
                index: "ave_with_e_index".into(),
            };
        }
    }
    assert_eq!(reference, old.hints, "retain unrelated row/index hints");
}

#[test]
fn rollback_hint_disable_and_other_binding_shapes_keep_the_existing_plan() {
    let default = plan(vec![alternatives()], None, true);
    assert_eq!(default, plan(vec![alternatives()], Some(false), true));
    assert_eq!(
        plan(vec![alternatives()], Some(true), false),
        plan(vec![alternatives()], Some(false), false),
        "disabled hints cannot introduce a scan preference"
    );
    for bindings in [
        vec![path(0, Component::Entity)],
        vec![BindingTree::And(vec![
            path(0, Component::Entity),
            path(1, Component::Entity),
        ])],
        vec![BindingTree::Or(vec![path(0, Component::Entity)])],
        vec![BindingTree::Or(vec![
            path(0, Component::Entity),
            path(1, Component::Value),
        ])],
        vec![path(2, Component::Entity), alternatives()],
    ] {
        assert_eq!(
            plan(bindings.clone(), Some(true), true),
            plan(bindings, Some(false), true),
            "ordinary, reference and compound joins retain their legacy hints"
        );
    }
}
