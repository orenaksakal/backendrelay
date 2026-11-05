//! Static proof of which entity attributes a compiled view rule can read.
//!
//! The analysis walks the same expressions evaluation executes: the selected
//! view program plus every reachable bind body, after `$default` resolution and
//! bind compilation. A label is projected only when every `data` use is a
//! field selection; any other shape falls back to a complete entity load.
use std::collections::BTreeSet;

use cel::common::ast::{EntryExpr, Expr, IdedEntryExpr, IdedExpr};
use instant_instaql::AttributeCatalog;
use uuid::Uuid;

use crate::RuleProgram;

/// Entity attributes the permission evaluator always receives alongside the
/// labels its rule selects. Identity is part of the entity contract, and some
/// system fallbacks (for example `data.id`) depend on it.
const EVALUATION_ENTITY_LABELS: [&str; 1] = ["id"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DataAttributeAccess {
    /// At least one `data` use cannot be reduced to field selections.
    Complete,
    /// Every `data` use is one of these top-level labels.
    Attributes(BTreeSet<String>),
}

/// Collects the top-level `data.<label>` accesses of a compiled rule.
pub(super) fn program_data_attributes(program: &RuleProgram) -> DataAttributeAccess {
    let mut labels = BTreeSet::new();
    for expression in std::iter::once(program.program.expression()).chain(
        program
            .bind_programs
            .iter()
            .map(|(_, bind)| bind.expression()),
    ) {
        if collect_expression(expression, &mut labels).is_none() {
            return DataAttributeAccess::Complete;
        }
    }
    labels.extend(EVALUATION_ENTITY_LABELS.map(str::to_owned));
    DataAttributeAccess::Attributes(labels)
}

/// Maps a view program's labels to this etype's attribute IDs.
///
/// Returns `None` for a complete load: either the expression shape or a label
/// (including the evaluation's implicit `id`) is not in the catalog.
pub(super) fn projected_attribute_ids(
    program: &RuleProgram,
    etype: &str,
    catalog: &AttributeCatalog,
) -> Option<Vec<Uuid>> {
    let DataAttributeAccess::Attributes(labels) = program_data_attributes(program) else {
        return None;
    };
    let mut ids = Vec::with_capacity(labels.len());
    for label in labels {
        ids.push(catalog.forward(etype, &label)?.id);
    }
    ids.sort_unstable();
    ids.dedup();
    Some(ids)
}

/// `None` means the analysis proved nothing and the entity needs the complete
/// load. `Some(ids)` is the exact projection the evaluation can read.
fn collect_expression(expression: &IdedExpr, labels: &mut BTreeSet<String>) -> Option<()> {
    match &expression.expr {
        Expr::Ident(name) => {
            // `size(data)`, `data == {}`, or any other unqualified use needs
            // the whole object.
            (name != "data").then_some(())
        }
        Expr::Select(select) => {
            if rooted_at_data(expression) {
                // `data.a.b` only needs attribute `a`; `has(data.a)` needs `a`.
                // Do not descend into the operand: its root is the bare `data`
                // identifier this branch deliberately consumes.
                labels.insert(first_field_after_data(expression)?.clone());
                Some(())
            } else {
                collect_expression(&select.operand, labels)
            }
        }
        Expr::Call(call) => {
            // Any method or index call on `data` itself (`data.ref(...)`,
            // `data[...]`, comprehensions over `data`, ...) is opaque here.
            if call.target.as_deref().is_some_and(rooted_at_data) {
                return None;
            }
            if let Some(target) = &call.target {
                collect_expression(target, labels)?;
            }
            for argument in &call.args {
                collect_expression(argument, labels)?;
            }
            Some(())
        }
        Expr::Comprehension(comprehension) => {
            for child in [
                &comprehension.iter_range,
                &comprehension.accu_init,
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ] {
                collect_expression(child, labels)?;
            }
            Some(())
        }
        Expr::List(list) => {
            for element in &list.elements {
                collect_expression(element, labels)?;
            }
            Some(())
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                collect_entry(entry, labels)?;
            }
            Some(())
        }
        Expr::Struct(value) => {
            for entry in &value.entries {
                collect_entry(entry, labels)?;
            }
            Some(())
        }
        Expr::Literal(_) => Some(()),
        // Never assume an unknown expression shape is safe to project.
        Expr::Unspecified => None,
    }
}

fn collect_entry(entry: &IdedEntryExpr, labels: &mut BTreeSet<String>) -> Option<()> {
    match &entry.expr {
        EntryExpr::StructField(field) => collect_expression(&field.value, labels),
        EntryExpr::MapEntry(entry) => {
            collect_expression(&entry.key, labels)?;
            collect_expression(&entry.value, labels)
        }
    }
}

fn rooted_at_data(expression: &IdedExpr) -> bool {
    match &expression.expr {
        Expr::Ident(name) => name == "data",
        Expr::Select(select) => rooted_at_data(&select.operand),
        _ => false,
    }
}

/// Innermost selection field directly on `data`, for `data.a`/`data.a.b`.
fn first_field_after_data(expression: &IdedExpr) -> Option<&String> {
    let Expr::Select(select) = &expression.expr else {
        return None;
    };
    match &select.operand.expr {
        Expr::Ident(name) if name == "data" => Some(&select.field),
        _ => first_field_after_data(&select.operand),
    }
}

#[cfg(test)]
mod tests {
    use instant_instaql::{Attribute, AttributeValueType, Cardinality};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::{EvaluationBindings, PermissionAction, RuleSet};

    fn attribute(id: u128, etype: &str, label: &str) -> Attribute {
        Attribute {
            id: Uuid::from_u128(id),
            wire: None,
            etype: etype.to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: label == "id",
            setting_unique: false,
            indexed: label == "id",
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    fn catalog() -> AttributeCatalog {
        AttributeCatalog {
            attributes: vec![
                attribute(1, "todos", "id"),
                attribute(2, "todos", "ownerId"),
                attribute(3, "todos", "title"),
                attribute(4, "messages", "id"),
                attribute(5, "messages", "memberIds"),
                attribute(6, "$users", "id"),
                attribute(7, "$users", "linkedPrimaryUser"),
            ],
        }
    }

    fn view(source: &str) -> RuleProgram {
        RuleProgram::compile(PermissionAction::View, source).unwrap()
    }

    fn labels(source: &str) -> BTreeSet<String> {
        match program_data_attributes(&view(source)) {
            DataAttributeAccess::Attributes(labels) => labels,
            DataAttributeAccess::Complete => panic!("expected projected access for {source}"),
        }
    }

    fn attributes(source: &str, etype: &str) -> Option<Vec<Uuid>> {
        projected_attribute_ids(&view(source), etype, &catalog())
    }

    fn ids(values: &[u128]) -> Option<Vec<Uuid>> {
        let mut values = values
            .iter()
            .copied()
            .map(Uuid::from_u128)
            .collect::<Vec<_>>();
        values.sort_unstable();
        Some(values)
    }

    #[test]
    fn fixture_view_rules_project_only_their_membership_attribute() {
        assert_eq!(
            labels("auth.id != null && auth.id == data.ownerId"),
            BTreeSet::from(["id".to_owned(), "ownerId".to_owned()])
        );
        assert_eq!(
            labels("auth.id != null && auth.id in data.memberIds"),
            BTreeSet::from(["id".to_owned(), "memberIds".to_owned()])
        );
        assert_eq!(attributes("auth.id == data.ownerId", "todos"), ids(&[1, 2]));
        assert_eq!(
            attributes("auth.id in data.memberIds", "messages"),
            ids(&[4, 5])
        );
    }

    #[test]
    fn bind_bodies_are_analyzed_after_expansion() {
        let rules = RuleSet::new(json!({
            "todos": {
                "bind": {"owner": "data.ownerId"},
                "allow": {"view": "auth.id != null && auth.id == owner"}
            }
        }));
        let program = rules
            .program("todos", PermissionAction::View)
            .unwrap()
            .unwrap();
        assert_eq!(
            program_data_attributes(&program),
            DataAttributeAccess::Attributes(BTreeSet::from([
                "id".to_owned(),
                "ownerId".to_owned()
            ]))
        );
        assert_eq!(
            projected_attribute_ids(&program, "todos", &catalog()),
            ids(&[1, 2])
        );
    }

    #[test]
    fn default_rules_are_resolved_before_analysis() {
        let rules = RuleSet::new(json!({
            "$default": {
                "allow": {"view": "auth.id != null && auth.id == data.ownerId"}
            }
        }));
        let program = rules
            .program("todos", PermissionAction::View)
            .unwrap()
            .unwrap();
        assert_eq!(
            projected_attribute_ids(&program, "todos", &catalog()),
            ids(&[1, 2])
        );
        let system = RuleSet::new(json!({}));
        let users = system
            .program("$users", PermissionAction::View)
            .unwrap()
            .unwrap();
        assert_eq!(
            projected_attribute_ids(&users, "$users", &catalog()),
            ids(&[6, 7])
        );
    }

    #[test]
    fn presence_nested_selects_and_function_arguments_project() {
        assert_eq!(
            labels("has(data.ownerId) && data.ownerId != null"),
            BTreeSet::from(["id".to_owned(), "ownerId".to_owned()])
        );
        assert_eq!(
            labels("data.memberIds.revision == 1"),
            BTreeSet::from(["id".to_owned(), "memberIds".to_owned()])
        );
        assert_eq!(
            labels("size(data.memberIds) > 0"),
            BTreeSet::from(["id".to_owned(), "memberIds".to_owned()])
        );
        assert_eq!(
            labels("[data.ownerId].exists(owner, owner == auth.id)"),
            BTreeSet::from(["id".to_owned(), "ownerId".to_owned()])
        );
        assert_eq!(
            labels("{'owner': data.ownerId}['owner'] == auth.id"),
            BTreeSet::from(["id".to_owned(), "ownerId".to_owned()])
        );
    }

    #[test]
    fn opaque_data_forms_fall_back_to_complete_entities() {
        for source in [
            "size(data) > 0",
            "data == {}",
            "data.ref('owner.id') == []",
            "data['ownerId'] == auth.id",
            "data.all(owner, owner == auth.id)",
            "[1].all(owner, owner in data)",
            "auth.id in data",
        ] {
            assert_eq!(
                program_data_attributes(&view(source)),
                DataAttributeAccess::Complete,
                "{source}"
            );
        }
    }

    #[test]
    fn unknown_labels_need_the_complete_entity() {
        assert_eq!(attributes("data.unknown == 1", "todos"), None);
        assert_eq!(attributes("data.ownerId == 1", "unknown-etype"), None);
    }

    #[test]
    fn corpus_decisions_match_between_complete_and_projected_entities() {
        let catalog = catalog();
        let corpus = [
            (
                "auth.id != null && auth.id == data.ownerId",
                vec![],
                "todos",
            ),
            (
                "auth.id != null && auth.id in data.memberIds",
                vec![],
                "messages",
            ),
            ("has(data.ownerId) && data.ownerId == 'u1'", vec![], "todos"),
            ("data.memberIds.revision == 2", vec![], "messages"),
            ("size(data.memberIds) == 2", vec![], "messages"),
            (
                "auth.id == owner && data.title != 'blocked'",
                vec![("owner".to_owned(), "data.ownerId".to_owned())],
                "todos",
            ),
            ("true", vec![], "todos"),
            ("data.ref('owner.id') == []", vec![], "todos"),
            ("size(data) > 0", vec![], "todos"),
            ("data['ownerId'] == auth.id", vec![], "todos"),
            ("[1].all(owner, owner in data)", vec![], "todos"),
        ];
        let entities = [
            json!({"id": "t1", "ownerId": "u1", "title": "one"}),
            json!({"id": "t2", "ownerId": "u2", "title": "blocked"}),
            json!({"id": "t3", "ownerId": null}),
            json!({"id": "m1", "memberIds": {"revision": 2}, "ownerId": "u1"}),
            json!({"id": "m2", "memberIds": [1, 2], "title": "two"}),
            json!({"title": "no identity"}),
        ];
        let auths = [json!({"id": "u1"}), json!({"id": "u2"}), json!({})];
        for (source, binds, etype) in corpus {
            let program =
                RuleProgram::compile_with_binds(PermissionAction::View, source, binds).unwrap();
            let projected = match program_data_attributes(&program) {
                DataAttributeAccess::Complete => None,
                DataAttributeAccess::Attributes(labels) => {
                    Some(projected_ids(&labels, etype, &catalog))
                }
            };
            for entity in &entities {
                let complete_data = entity.clone();
                let projected_data = match &projected {
                    Some(labels) => project(entity, labels),
                    None => entity.clone(),
                };
                for auth in &auths {
                    let complete = bindings(auth, &complete_data);
                    let projected_bindings = bindings(auth, &projected_data);
                    assert_eq!(
                        program
                            .evaluate(&complete)
                            .map_err(|error| error.to_string()),
                        program
                            .evaluate(&projected_bindings)
                            .map_err(|error| error.to_string()),
                        "rule {source:?}, entity {entity}, auth {auth}"
                    );
                }
            }
        }
    }

    fn projected_ids(
        labels: &BTreeSet<String>,
        etype: &str,
        catalog: &AttributeCatalog,
    ) -> Vec<Uuid> {
        let mut ids = labels
            .iter()
            .filter_map(|label| catalog.forward(etype, label).map(|attribute| attribute.id))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn project(entity: &Value, labels: &[Uuid]) -> Value {
        let catalog = catalog();
        let mut object = serde_json::Map::new();
        if let Some(source) = entity.as_object() {
            for attribute in &catalog.attributes {
                if labels.contains(&attribute.id)
                    && let Some(value) = source.get(&attribute.label)
                {
                    object.insert(attribute.label.clone(), value.clone());
                }
            }
        }
        Value::Object(object)
    }

    fn bindings(auth: &Value, data: &Value) -> EvaluationBindings {
        EvaluationBindings {
            auth: auth.clone(),
            data: data.clone(),
            data_etype: Some("todos".to_owned()),
            ..EvaluationBindings::default()
        }
    }
}
