use std::collections::BTreeMap;

use instant_datalog::{
    DataType, MatchCteOptions, PlannedCte, QueryHint, WherePredicate, render_nested_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query, plan_nested_query,
};
use serde_json::json;
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "room", "revision", "updatedAt", "unindexed"]
            .into_iter()
            .enumerate()
            .map(|(n, label)| Attribute {
                id: Uuid::from_u128(n as u128 + 1),
                wire: None,
                etype: "messages".into(),
                label: label.into(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: label == "id",
                setting_unique: false,
                indexed: matches!(label, "id" | "room" | "updatedAt"),
                required: label == "id",
                checked_data_type: (label == "updatedAt").then_some(DataType::Number),
                indexing: false,
                checking_data_type: false,
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            })
            .collect(),
    }
}

fn has_candidate_membership(predicate: &WherePredicate) -> bool {
    match predicate {
        WherePredicate::In { query, .. } => query.table.contains("candidate"),
        WherePredicate::And(values) | WherePredicate::Or(values) => {
            values.iter().any(has_candidate_membership)
        }
        _ => false,
    }
}

fn candidate_bound(cte: &instant_datalog::MatchCte) -> bool {
    cte.from
        .iter()
        .any(|source| source.table.contains("candidate"))
        || has_candidate_membership(&cte.predicate)
}

#[test]
fn candidate_stages_preserve_metadata_and_bind_or_and_optional_order_scans() {
    let compiled=compile_query(&json!({"messages":{"$":{"where":{"room":"small","revision":{"$ne":"skip"}},"order":{"updatedAt":"desc"},"limit":20}}}),&catalog(),false).unwrap();
    let base = MatchCteOptions {
        predicate_options: instant_datalog::PredicateOptions {
            forward_is_null_exists: true,
            ..Default::default()
        },
        attribute_cardinalities: BTreeMap::from([(Uuid::from_u128(2), 20_000)]),
        attribute_value_cardinalities: BTreeMap::from([(
            Uuid::from_u128(2),
            BTreeMap::from([("\"small\"".into(), 5)]),
        )]),
        ..Default::default()
    };
    let old = plan_nested_query(&compiled, Uuid::from_u128(10), &base).unwrap();
    let new = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(true),
            optional_order_seed_binding: Some(true),
            ..base.clone()
        },
    )
    .unwrap();
    assert_eq!(
        old.groups, new.groups,
        "physical plans must preserve row layout, pages and topics"
    );
    assert!(new.ctes.iter().any(|cte| matches!(cte,PlannedCte::Select(c) if c.name.contains("candidate") && c.statement.distinct_on.len()==1)));
    let bounded = new
        .ctes
        .iter()
        .filter(|cte| matches!(cte,PlannedCte::Match(c) if candidate_bound(c)))
        .count();
    assert!(
        bounded == 2,
        "both OR arms stay bound; the optional-order relation relies on its seed join"
    );
    let legacy = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(true),
            optional_order_seed_binding: Some(false),
            ..base.clone()
        },
    )
    .unwrap();
    let legacy_bounded = legacy
        .ctes
        .iter()
        .filter(|cte| matches!(cte,PlannedCte::Match(c) if candidate_bound(c)))
        .count();
    assert!(
        legacy_bounded >= 3,
        "the rollback keeps the optional-order membership restriction"
    );
    let old_names = old
        .ctes
        .iter()
        .map(|c| match c {
            PlannedCte::Match(c) => &c.name,
            PlannedCte::OrGather(c) => &c.name,
            PlannedCte::Select(c) => &c.name,
        })
        .collect::<Vec<_>>();
    let new_names = new
        .ctes
        .iter()
        .filter_map(|c| match c {
            PlannedCte::Select(c) if c.name.contains("candidate") => None,
            PlannedCte::Match(c) => Some(&c.name),
            PlannedCte::OrGather(c) => Some(&c.name),
            PlannedCte::Select(c) => Some(&c.name),
        })
        .collect::<Vec<_>>();
    assert_eq!(old_names, new_names, "do not renumber original CTEs");
    let disabled = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(false),
            ..base
        },
    )
    .unwrap();
    assert_eq!(disabled, old);
    assert!(
        render_nested_match_query(&new.sql_plan())
            .unwrap()
            .sql
            .contains("bound_entity_id")
    );
}

#[test]
fn a_filter_inside_or_is_not_a_required_global_seed() {
    for query in [
        json!({"messages":{"$":{"where":{"or":[{"room":"small"},{"revision":"other"}]},"order":{"updatedAt":"desc"},"limit":10}}}),
        json!({"messages":{"$":{"where":{"unindexed":"small"},"limit":10}}}),
    ] {
        let compiled = compile_query(&query, &catalog(), false).unwrap();
        let old =
            plan_nested_query(&compiled, Uuid::from_u128(10), &MatchCteOptions::default()).unwrap();
        let new = plan_nested_query(
            &compiled,
            Uuid::from_u128(10),
            &MatchCteOptions {
                candidate_entity_scans: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(old, new);
    }
}

#[test]
fn candidate_scans_do_not_depend_on_hint_extension_or_literal_inlining() {
    let secret = "never-inline-user-values";
    let query = json!({"messages":{"$":{"where":{"room":{"$in":[secret,"other"]}},"order":{"updatedAt":"desc"},"limit":10}}});
    let compiled = compile_query(&query, &catalog(), false).unwrap();
    let plan = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(true),
            pg_hints_enabled: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!plan.hints.iter().any(|hint| matches!(
        hint,
        QueryHint::IndexScan { .. } | QueryHint::BitmapScan { .. }
    )));
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    assert!(!rendered.sql.contains(secret));
    assert!(rendered.sql.contains("candidate"));
}

#[test]
fn known_broad_filters_keep_the_existing_plan_and_selective_seeds_win() {
    let input = json!({"messages":{"$":{"where":{"room":"popular","updatedAt":7},"order":{"updatedAt":"asc"},"limit":10}}});
    let compiled = compile_query(&input, &catalog(), false).unwrap();
    let mut options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        ..Default::default()
    };
    for (attr, key, rows) in [(2, "\"popular\"", 20000), (4, "7", 20000)] {
        let attr = Uuid::from_u128(attr);
        options.attribute_cardinalities.insert(attr, 20000);
        options
            .attribute_value_cardinalities
            .entry(attr)
            .or_default()
            .insert(key.into(), rows);
    }
    let broad = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
    let legacy = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(false),
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(
        broad, legacy,
        "do not force candidate materialization for known broad filters"
    );
    options
        .attribute_value_cardinalities
        .get_mut(&Uuid::from_u128(4))
        .unwrap()
        .insert("7".into(), 5);
    let selective = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
    assert!(
        selective
            .ctes
            .iter()
            .any(|cte| matches!(cte,PlannedCte::Select(c) if c.name.contains("candidate")))
    );
    let rendered = render_nested_match_query(&selective.sql_plan()).unwrap();
    // The physical seed must be the rare numeric equality, not the first
    // indexed field encountered while walking compiler patterns.
    let PlannedCte::Select(seed) = &selective.ctes[0] else {
        panic!("candidate seed must precede its consumers")
    };
    assert!(
        serde_json::to_string(&seed.statement.predicate)
            .unwrap()
            .contains(&Uuid::from_u128(4).to_string())
    );
    assert!(!rendered.sql.contains("popular"));
}

#[test]
fn broad_descending_first_pages_have_bounded_ordered_windows_and_a_null_tail() {
    let query = json!({"messages":{"$":{"where":{"room":"popular","revision":{"$ne":"skip"}},"order":{"updatedAt":"desc"},"limit":20}}});
    let compiled = compile_query(&query, &catalog(), false).unwrap();
    let mut options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        ..Default::default()
    };
    options
        .attribute_cardinalities
        .insert(Uuid::from_u128(2), 20000);
    options
        .attribute_value_cardinalities
        .entry(Uuid::from_u128(2))
        .or_default()
        .insert("\"popular\"".into(), 15000);
    let plan = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
    let legacy = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(false),
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(plan.groups, legacy.groups);
    assert!(plan.ctes.iter().any(|cte|matches!(cte,PlannedCte::Select(c) if c.name.ends_with("ordered-nonnull") && c.statement.limit==Some(21))));
    assert!(plan.ctes.iter().any(|cte|matches!(cte,PlannedCte::Select(c) if c.name.ends_with("ordered-nulls") && c.statement.limit==Some(21))));
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    assert!(!rendered.sql.contains("popular"));
    assert!(!rendered.sql.contains("skip"));
    for change in [json!({"offset":1}), json!({"order":{"updatedAt":"asc"}})] {
        let mut input = query.clone();
        input["messages"]["$"]
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        let compiled = compile_query(&input, &catalog(), false).unwrap();
        let plan = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
        assert!(
            !plan.ctes.iter().any(
                |cte| matches!(cte,PlannedCte::Select(c) if c.name.contains("ordered-nonnull"))
            )
        );
    }
}

#[test]
fn ordered_windows_preserve_correlated_probes_and_materialize_bounded_relations() {
    use instant_datalog::{Materialization, WherePredicate};
    fn probes(predicate: &WherePredicate) -> usize {
        match predicate {
            WherePredicate::ExistsProbe { .. } => 1,
            WherePredicate::And(values) | WherePredicate::Or(values) => {
                values.iter().map(probes).sum()
            }
            _ => 0,
        }
    }
    let compiled = compile_query(&json!({"messages":{"$":{"where":{"room":"popular","or":[{"revision":{"$ne":"skip"}},{"updatedAt":{"$lt":8}}]},"order":{"updatedAt":"desc"},"limit":20}}}), &catalog(), false).unwrap();
    let mut options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        ..Default::default()
    };
    options
        .attribute_cardinalities
        .insert(Uuid::from_u128(2), 20_000);
    options
        .attribute_value_cardinalities
        .entry(Uuid::from_u128(2))
        .or_default()
        .insert("\"popular\"".into(), 20_000);
    let plan = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
    let dense = plan
        .ctes
        .iter()
        .find_map(|cte| match cte {
            PlannedCte::Select(c) if c.name.ends_with("ordered-nonnull") => Some(c),
            _ => None,
        })
        .unwrap();
    // $ne compiles into both a value inequality and its missing/null branch.
    assert_eq!(
        probes(dense.statement.predicate.as_ref().unwrap()),
        4,
        "required seed and every complete null-aware OR arm retain entity-correlated EXISTS"
    );
    let mut bounded = 0;
    for cte in &plan.ctes {
        let PlannedCte::Match(cte) = cte else {
            continue;
        };
        if cte
            .from
            .iter()
            .any(|source| source.table.ends_with("-ordered"))
        {
            bounded += 1;
            assert_eq!(cte.materialization, Materialization::Materialized);
            let target = cte.from[0].alias.as_ref().unwrap();
            assert!(plan.hints.iter().any(|hint| matches!(hint, QueryHint::IndexScan { table_alias,index } if table_alias==target && index=="triples_pkey")));
        }
    }
    assert!(
        bounded >= 2,
        "include optional order relation as well as selector"
    );
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    assert_eq!(rendered.sql.matches(" OFFSET 0").count(), 4);
    assert!(!rendered.sql.contains("popular") && !rendered.sql.contains("skip"));
    options.pg_hints_enabled = Some(false);
    let no_hints = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
    assert!(!no_hints.hints.iter().any(|hint| matches!(
        hint,
        QueryHint::IndexScan { .. } | QueryHint::BitmapScan { .. }
    )));
    assert_eq!(
        plan.groups, no_hints.groups,
        "hint controls do not change results or dependency metadata"
    );
}

#[test]
fn required_order_comparisons_are_index_bounds_but_or_arms_are_not() {
    use instant_datalog::{PredicateOperator, SqlFunction, WhereExpression, WherePredicate};
    fn has_direct_bound(predicate: &WherePredicate) -> bool {
        match predicate {
            WherePredicate::And(values) | WherePredicate::Or(values) => {
                values.iter().any(has_direct_bound)
            }
            WherePredicate::Compare {
                operator: PredicateOperator::LessThan,
                left:
                    WhereExpression::Function {
                        function: SqlFunction::ExtractNumberValue,
                        ..
                    },
                ..
            } => true,
            _ => false, // Do not count the retained correlated EXISTS predicates.
        }
    }
    let mut options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        ..Default::default()
    };
    options
        .attribute_cardinalities
        .insert(Uuid::from_u128(2), 20000);
    options
        .attribute_value_cardinalities
        .entry(Uuid::from_u128(2))
        .or_default()
        .insert("\"popular\"".into(), 15000);
    for (condition, expected) in [
        (
            json!({"room":"popular","updatedAt":{"$lt":8},"revision":{"$ne":"skip"}}),
            true,
        ),
        (
            json!({"room":"popular","or":[{"updatedAt":{"$lt":8}},{"revision":"keep"}]}),
            false,
        ),
    ] {
        let compiled = compile_query(
            &json!({"messages":{"$":{"where":condition,"order":{"updatedAt":"desc"},"limit":20}}}),
            &catalog(),
            false,
        )
        .unwrap();
        let plan = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
        let dense = plan
            .ctes
            .iter()
            .find_map(|cte| match cte {
                PlannedCte::Select(c) if c.name.ends_with("ordered-nonnull") => Some(c),
                _ => None,
            })
            .expect("exercise the ordered window");
        assert_eq!(
            has_direct_bound(dense.statement.predicate.as_ref().unwrap()),
            expected
        );
    }
}

#[test]
fn narrow_created_at_pages_order_the_filtered_join_without_changing_metadata() {
    let input = json!({"messages":{"$":{"where":{"room":"small"},"limit":10}}});
    let compiled = compile_query(&input, &catalog(), false).unwrap();
    let mut options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        ..Default::default()
    };
    options
        .attribute_cardinalities
        .insert(Uuid::from_u128(2), 20_000);
    options
        .attribute_value_cardinalities
        .entry(Uuid::from_u128(2))
        .or_default()
        .insert("\"small\"".into(), 10);
    let bounded = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
    let legacy = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(false),
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(
        bounded.groups, legacy.groups,
        "preserve row layout, cursors and topics"
    );
    let source = bounded
        .ctes
        .iter()
        .find_map(|cte| match cte {
            PlannedCte::Select(cte) if cte.name.ends_with("-bound-order") => Some(cte),
            _ => None,
        })
        .expect("materialize the narrow default-order join before pagination");
    assert_eq!(
        source.materialization,
        instant_datalog::Materialization::Materialized
    );
    assert!(source.statement.limit.is_none() && source.statement.offset.is_none());
    assert!(source.statement.order_by.is_empty() && source.statement.distinct_on.is_empty());
    assert!(source.statement.predicate.is_some());
    // The first selector is m_0; m_1/t1 is its required created-at order join.
    // A whole-partition forced order scan must not survive inside the fence.
    assert!(!bounded.hints.iter().any(|hint| match hint {
        QueryHint::IndexScan { table_alias, .. } | QueryHint::BitmapScan { table_alias, .. } =>
            table_alias == "t1",
        QueryHint::Rows { tables, .. } => tables.iter().any(|table| table == "t1"),
        QueryHint::DisableJit => false,
    }));
    assert!(
        !legacy
            .ctes
            .iter()
            .any(|cte| matches!(cte, PlannedCte::Select(c) if c.name.ends_with("-bound-order")))
    );
    let missing_estimates = plan_nested_query(
        &compiled,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        missing_estimates
            .ctes
            .iter()
            .any(|cte| matches!(cte, PlannedCte::Select(c) if c.name.ends_with("-bound-order"))),
        "a required indexed equality still bounds the physical join when sketches are missing"
    );

    // A small requested page must not masquerade as a small selector. The
    // estimate must be captured before propagation caps it at the page limit.
    options
        .attribute_value_cardinalities
        .get_mut(&Uuid::from_u128(2))
        .unwrap()
        .insert("\"small\"".into(), 20_000);
    let broad = plan_nested_query(&compiled, Uuid::from_u128(10), &options).unwrap();
    assert!(
        !broad
            .ctes
            .iter()
            .any(|cte| matches!(cte, PlannedCte::Select(c) if c.name.ends_with("-bound-order")))
    );
    let unfiltered =
        compile_query(&json!({"messages":{"$":{"limit":10}}}), &catalog(), false).unwrap();
    let plan = plan_nested_query(&unfiltered, Uuid::from_u128(10), &options).unwrap();
    assert!(
        !plan
            .ctes
            .iter()
            .any(|cte| matches!(cte, PlannedCte::Select(c) if c.name.ends_with("-bound-order")))
    );
    let only_or = compile_query(&json!({"messages":{"$":{"where":{"or":[{"room":"small"},{"revision":"other"}]},"limit":10}}}), &catalog(), false).unwrap();
    let plan = plan_nested_query(
        &only_or,
        Uuid::from_u128(10),
        &MatchCteOptions {
            candidate_entity_scans: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        !plan
            .ctes
            .iter()
            .any(|cte| matches!(cte, PlannedCte::Select(c) if c.name.ends_with("-bound-order"))),
        "an OR arm is not a required equality"
    );
}
