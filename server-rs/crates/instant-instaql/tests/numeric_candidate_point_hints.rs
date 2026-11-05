use std::collections::BTreeMap;

use instant_datalog::{
    DataType, FromTable, IndexKey, MatchCteOptions, Materialization, PatternMetadataNode,
    PlannedCte, PredicateOperator, QueryHint, WhereColumn, WhereExpression, WherePredicate,
    render_nested_match_query,
};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, compile_query, plan_nested_query,
};
use serde_json::json;
use uuid::Uuid;

fn catalog() -> AttributeCatalog {
    AttributeCatalog {
        attributes: ["id", "room", "revision", "updatedAt"]
            .into_iter()
            .enumerate()
            .map(|(index, label)| Attribute {
                id: Uuid::from_u128(index as u128 + 1),
                wire: None,
                etype: "messages".into(),
                label: label.into(),
                value_type: AttributeValueType::Blob,
                cardinality: Cardinality::One,
                unique: label == "id",
                setting_unique: false,
                indexed: label != "revision",
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

fn numeric_tables(metadata: &[PatternMetadataNode], result: &mut Vec<String>) {
    for node in metadata {
        match node {
            PatternMetadataNode::Pattern(meta)
                if meta.pattern.index.key == IndexKey::Ave
                    && meta.pattern.index.data_type == Some(DataType::Number) =>
            {
                result.push(meta.cte_columns[0].trim_end_matches("_entity_id").into());
            }
            PatternMetadataNode::Or(branches) => {
                for branch in branches {
                    numeric_tables(branch, result);
                }
            }
            _ => {}
        }
    }
}

fn small_options() -> MatchCteOptions {
    MatchCteOptions {
        candidate_entity_scans: Some(true),
        attribute_cardinalities: BTreeMap::from([(Uuid::from_u128(2), 20_000)]),
        attribute_value_cardinalities: BTreeMap::from([(
            Uuid::from_u128(2),
            BTreeMap::from([("\"never-inline-room\"".into(), 86)]),
        )]),
        ..Default::default()
    }
}

fn is_candidate_membership(predicate: &WherePredicate) -> bool {
    match predicate {
        WherePredicate::In { query, .. } => query.table.contains("-candidate-"),
        WherePredicate::And(values) | WherePredicate::Or(values) => {
            values.iter().any(is_candidate_membership)
        }
        _ => false,
    }
}

#[test]
fn small_numeric_membership_changes_only_the_distinct_candidate_binding() {
    let compiled = compile_query(
        &json!({"messages":{"$":{
            "where":{"room":"never-inline-room","revision":{"$ne":"never-inline-nonce"},"updatedAt":{"$lt":8}},
            "order":{"updatedAt":"desc"},"limit":20,"fields":["id","room"]
        }}}),
        &catalog(),
        false,
    )
    .unwrap();
    let plan = plan_nested_query(&compiled, Uuid::from_u128(100), &small_options()).unwrap();
    let mut reference = plan.clone();
    let mut numeric = Vec::new();
    numeric_tables(&plan.groups[0].metadata, &mut numeric);
    let mut checked = 0;
    let mut non_materialized_order_relations = 0;
    for cte in &mut reference.ctes {
        let PlannedCte::Match(cte) = cte else {
            continue;
        };
        if !numeric.contains(&cte.name.replace('-', "_")) {
            continue;
        }
        assert_eq!(
            cte.from.len(),
            1,
            "do not multiply the numeric relation by its candidates"
        );
        if cte.materialization == Materialization::NotMaterialized {
            non_materialized_order_relations += 1;
        }
        let target = cte.from[0].alias.as_ref().unwrap();
        let WherePredicate::And(predicates) = &mut cte.predicate else {
            panic!("retain the original numeric predicate before its binding")
        };
        assert_eq!(predicates.len(), 2);
        let WherePredicate::In {
            negated,
            expression,
            query,
        } = &predicates[1]
        else {
            panic!("known-small numeric relations need candidate membership")
        };
        assert!(!negated);
        assert_eq!(
            *expression,
            WhereExpression::qualified_column(target, WhereColumn::EntityId)
        );
        assert_eq!(
            query.select,
            vec![WhereExpression::NamedColumn("bound-entity-id".into())]
        );
        assert_eq!(*query.predicate, WherePredicate::And(Vec::new()));
        assert!(plan.ctes.iter().any(|cte| matches!(cte,
            PlannedCte::Select(seed) if seed.name == query.table
                && seed.materialization == Materialization::Materialized
                && seed.statement.distinct_on.len() == 1
                && seed.statement.limit.is_none() && seed.statement.offset.is_none())));
        // Reconstruct exactly the former comma-join binding, not a different
        // query/options baseline. Everything else in the cloned plan is kept.
        let table = query.table.clone();
        let left = expression.clone();
        cte.from.push(FromTable {
            table: table.clone(),
            alias: None,
        });
        predicates[1] = WherePredicate::Compare {
            operator: PredicateOperator::Equal,
            left,
            right: WhereExpression::NamedColumn(format!("{table}.bound-entity-id")),
        };
        checked += 1;
    }
    assert_eq!(
        checked, 2,
        "cover numeric filters and the optional order relation"
    );
    assert_eq!(
        non_materialized_order_relations, 1,
        "keep the optional-order relation non-materialized"
    );
    assert_eq!(plan.groups, reference.groups);
    assert_eq!(plan.hints, reference.hints);
    let new = render_nested_match_query(&plan.sql_plan()).unwrap();
    let old = render_nested_match_query(&reference.sql_plan()).unwrap();
    assert_eq!(
        new.parameters, old.parameters,
        "no new bindings or inlined user values"
    );
    assert_eq!(
        new.sql.matches(" IN (SELECT \"bound_entity_id\"").count(),
        2
    );
    assert!(!new.sql.contains("never-inline-room"));
    assert!(!new.sql.contains("never-inline-nonce"));
}

#[test]
fn optional_order_seed_binding_drops_only_the_redundant_numeric_membership() {
    let compiled = compile_query(
        &json!({"messages":{"$":{
            "where":{"room":"never-inline-room","revision":{"$ne":"never-inline-nonce"},"updatedAt":{"$lt":8}},
            "order":{"updatedAt":"desc"},"limit":20
        }}}),
        &catalog(),
        false,
    )
    .unwrap();
    let mut options = small_options();
    let rollback = plan_nested_query(
        &compiled,
        Uuid::from_u128(100),
        &MatchCteOptions {
            optional_order_seed_binding: Some(false),
            ..options.clone()
        },
    )
    .unwrap();
    options.optional_order_seed_binding = Some(true);
    let plan = plan_nested_query(&compiled, Uuid::from_u128(100), &options).unwrap();
    assert_eq!(plan.groups, rollback.groups);
    let memberships = |plan: &instant_instaql::PlannedNestedQuery| {
        plan.ctes
            .iter()
            .filter_map(|cte| match cte {
                PlannedCte::Match(cte) if is_candidate_membership(&cte.predicate) => {
                    Some((cte.name.clone(), cte.materialization))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let before = memberships(&rollback);
    let after = memberships(&plan);
    assert_eq!(after.len(), 1, "only the numeric filter keeps membership");
    assert_eq!(before.len(), 2);
    let removed = before
        .iter()
        .find(|(name, _)| after.iter().all(|(kept, _)| kept != name))
        .expect("the optional-order relation loses its membership");
    assert_eq!(
        removed.1,
        Materialization::NotMaterialized,
        "the optional-order relation is the non-materialized numeric target"
    );
    let target = plan
        .ctes
        .iter()
        .find_map(|cte| match cte {
            PlannedCte::Match(cte) if cte.name == removed.0 => cte.from[0].alias.clone(),
            _ => None,
        })
        .unwrap();
    assert!(plan.hints.contains(&QueryHint::IndexScan {
        table_alias: target,
        index: "triples_pkey".into(),
    }));
    assert_eq!(
        render_nested_match_query(&plan.sql_plan())
            .unwrap()
            .sql
            .matches(" IN (SELECT \"bound_entity_id\"")
            .count(),
        1
    );
}

#[test]
fn numeric_membership_keeps_unknown_broad_and_other_type_fallbacks_and_ignores_hint_controls() {
    let input = json!({"messages":{"$":{
        "where":{"room":"never-inline-room","revision":{"$ne":"never-inline-nonce"},"updatedAt":{"$lt":8}},
        "order":{"updatedAt":"asc"},"limit":20
    }}});
    for dtype in [DataType::Number, DataType::String] {
        let mut catalog = catalog();
        catalog.attributes[3].checked_data_type = Some(dtype);
        let mut input = input.clone();
        if dtype == DataType::String {
            input["messages"]["$"]["where"]["updatedAt"] = json!({"$lt":"synthetic-bound"});
        }
        let compiled = compile_query(&input, &catalog, false).unwrap();
        for estimate in [
            None,
            Some(0),
            Some(1),
            Some(1_024),
            Some(1_025),
            Some(20_000),
        ] {
            for hints in [false, true] {
                let mut options = small_options();
                options.pg_hints_enabled = Some(hints);
                options.attribute_cardinalities.clear();
                options.attribute_value_cardinalities.clear();
                if let Some(rows) = estimate {
                    options
                        .attribute_cardinalities
                        .insert(Uuid::from_u128(2), 20_000);
                    options.attribute_value_cardinalities.insert(
                        Uuid::from_u128(2),
                        BTreeMap::from([("\"never-inline-room\"".into(), rows)]),
                    );
                }
                let plan = plan_nested_query(&compiled, Uuid::from_u128(100), &options).unwrap();
                let count = plan
                    .ctes
                    .iter()
                    .filter(|cte| {
                        matches!(cte,
                    PlannedCte::Match(cte) if is_candidate_membership(&cte.predicate))
                    })
                    .count();
                assert_eq!(
                    count,
                    if dtype == DataType::Number && estimate.is_some_and(|rows| rows <= 1_024) {
                        2
                    } else {
                        0
                    },
                    "scope stays numeric and known-small, independent of hint enablement"
                );
            }
        }
    }
}

#[test]
fn known_small_candidates_probe_numeric_filters_and_order_without_changing_query_data() {
    let input = json!({"messages":{"$":{
        "where":{"room":"never-inline-room","revision":{"$ne":"never-inline-nonce"},"updatedAt":{"$lt":8}},
        "order":{"updatedAt":"desc"},"limit":20,"fields":["id","room"]
    }}});
    let catalog = catalog();
    let compiled = compile_query(&input, &catalog, false).unwrap();
    let options = MatchCteOptions {
        candidate_entity_scans: Some(true),
        attribute_cardinalities: BTreeMap::from([(Uuid::from_u128(2), 20_000)]),
        attribute_value_cardinalities: BTreeMap::from([(
            Uuid::from_u128(2),
            BTreeMap::from([("\"never-inline-room\"".into(), 86)]),
        )]),
        ..Default::default()
    };
    let app = Uuid::from_u128(100);
    let plan = plan_nested_query(&compiled, app, &options).unwrap();
    let mut tables = Vec::new();
    numeric_tables(&plan.groups[0].metadata, &mut tables);
    assert_eq!(
        tables.len(),
        2,
        "cover the numeric filter and optional order"
    );
    let mut aliases = Vec::new();
    for cte in &plan.ctes {
        let PlannedCte::Match(cte) = cte else {
            continue;
        };
        if tables.contains(&cte.name.replace('-', "_")) {
            assert!(is_candidate_membership(&cte.predicate));
            assert_eq!(cte.from.len(), 1);
            let alias = cte.from[0].alias.clone().unwrap();
            assert!(plan.hints.contains(&QueryHint::IndexScan {
                table_alias: alias.clone(),
                index: "triples_pkey".into(),
            }));
            aliases.push(alias);
        }
    }
    assert_eq!(aliases.len(), 2);
    let mut unhinted = plan.clone();
    unhinted.hints.retain(|hint| {
        !matches!(hint,
        QueryHint::IndexScan { table_alias, index }
            if aliases.contains(table_alias) && index == "triples_pkey")
    });
    let rendered = render_nested_match_query(&plan.sql_plan()).unwrap();
    let reference = render_nested_match_query(&unhinted.sql_plan()).unwrap();
    assert_eq!(rendered.parameters, reference.parameters);
    let mut without_point_hints = rendered.sql;
    for alias in &aliases {
        without_point_hints =
            without_point_hints.replace(&format!("IndexScan({alias} triples_pkey)\n"), "");
    }
    assert_eq!(without_point_hints, reference.sql);
    assert!(!reference.sql.contains("never-inline-room"));
    assert!(!reference.sql.contains("never-inline-nonce"));

    let disabled = plan_nested_query(
        &compiled,
        app,
        &MatchCteOptions {
            pg_hints_enabled: Some(false),
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(plan.groups, disabled.groups);
    assert!(!disabled.hints.iter().any(|hint| matches!(
        hint,
        QueryHint::IndexScan { .. } | QueryHint::BitmapScan { .. }
    )));
    let rollback = plan_nested_query(
        &compiled,
        app,
        &MatchCteOptions {
            candidate_entity_scans: Some(false),
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(plan.groups, rollback.groups);
    assert!(!rollback.hints.iter().any(|hint| matches!(hint,
        QueryHint::IndexScan { table_alias, index }
            if aliases.contains(table_alias) && index == "triples_pkey")));

    // Keep the existing fallback when the seed is unknown or known broad. An
    // ascending page avoids the separate, existing broad descending-window path.
    let mut ascending = input.clone();
    ascending["messages"]["$"]["order"]["updatedAt"] = json!("asc");
    let compiled = compile_query(&ascending, &catalog, false).unwrap();
    for estimate in [None, Some(1_024), Some(1_025), Some(20_000)] {
        let mut fallback = options.clone();
        fallback.attribute_cardinalities.clear();
        fallback.attribute_value_cardinalities.clear();
        if let Some(rows) = estimate {
            fallback
                .attribute_cardinalities
                .insert(Uuid::from_u128(2), rows);
            fallback.attribute_value_cardinalities.insert(
                Uuid::from_u128(2),
                BTreeMap::from([("\"never-inline-room\"".into(), rows)]),
            );
        }
        let fallback = plan_nested_query(&compiled, app, &fallback).unwrap();
        for alias in &aliases {
            assert_eq!(
                fallback.hints.contains(&QueryHint::IndexScan {
                    table_alias: alias.clone(),
                    index: "triples_pkey".into(),
                }),
                estimate == Some(1_024),
                "probe only known-small seeds, including the exact threshold"
            );
        }
    }

    let mut strings = catalog.clone();
    strings.attributes[3].checked_data_type = Some(DataType::String);
    let mut string_input = input;
    string_input["messages"]["$"]["where"]["updatedAt"] = json!({"$lt":"synthetic-bound"});
    let compiled = compile_query(&string_input, &strings, false).unwrap();
    let string_plan = plan_nested_query(&compiled, app, &options).unwrap();
    assert!(!string_plan.hints.iter().any(|hint| matches!(hint,
        QueryHint::IndexScan { table_alias, index }
            if aliases.contains(table_alias) && index == "triples_pkey")));
}
