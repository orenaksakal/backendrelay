//! Property coverage for the public transaction step parser.
//!
//! The parser consumes unauthenticated JSON from the write endpoint, so it must
//! reject arbitrary input without panicking while preserving every field of a
//! valid step.

use instant_instaql::AttributeCatalog;
use instant_transact::{
    DeleteEntityStep, EntityLookup, RuleParamsStep, TransactionMode, TransactionStep,
    TripleOptions, TripleStep, compile_transaction, compile_transaction_all_errors,
};
use proptest::prelude::{BoxedStrategy, Just, Strategy, any, prop_assert, prop_assert_eq};
use proptest::proptest;
use proptest::string::string_regex;
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn leaf_json() -> impl Strategy<Value = Value> {
    proptest::prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        proptest::num::f64::NORMAL.prop_map(Value::from),
        string_regex("[ -~]{0,12}")
            .expect("valid regex")
            .prop_map(Value::String),
    ]
}

fn arb_json(depth: u32) -> BoxedStrategy<Value> {
    if depth == 0 {
        return leaf_json().boxed();
    }
    proptest::prop_oneof![
        leaf_json(),
        proptest::collection::vec(arb_json(depth - 1), 0..4).prop_map(Value::Array),
        proptest::collection::btree_map(
            string_regex("[A-Za-z0-9_$]{0,8}").expect("valid regex"),
            arb_json(depth - 1),
            0..4,
        )
        .prop_map(|entries| Value::Object(entries.into_iter().collect())),
    ]
    .boxed()
}

fn uuid_value() -> impl Strategy<Value = Uuid> {
    any::<u128>().prop_map(Uuid::from_u128)
}

fn etype_value() -> impl Strategy<Value = Option<String>> {
    proptest::option::of(string_regex("[A-Za-z0-9_]{0,6}").expect("valid regex"))
}

fn mode_value() -> impl Strategy<Value = Option<TransactionMode>> {
    proptest::prop_oneof![
        Just(None),
        Just(Some(TransactionMode::Create)),
        Just(Some(TransactionMode::Update)),
        Just(Some(TransactionMode::Upsert)),
    ]
}

fn mode_name(mode: TransactionMode) -> &'static str {
    match mode {
        TransactionMode::Create => "create",
        TransactionMode::Update => "update",
        TransactionMode::Upsert => "upsert",
    }
}

fn add_triple_step() -> impl Strategy<Value = (Value, TransactionStep)> {
    (uuid_value(), uuid_value(), arb_json(2), mode_value()).prop_map(
        |(entity, attr_id, value, mode)| {
            let step = TripleStep {
                entity: EntityLookup::Id(entity),
                etype: None,
                attr_id,
                reverse_etype: None,
                value: value.clone(),
                options: TripleOptions { mode },
            };
            let mut input = vec![json!("add-triple"), json!(entity), json!(attr_id), value];
            if let Some(mode) = mode {
                input.push(json!({ "mode": mode_name(mode) }));
            }
            (Value::Array(input), TransactionStep::AddTriple(step))
        },
    )
}

fn retract_triple_step() -> impl Strategy<Value = (Value, TransactionStep)> {
    (uuid_value(), uuid_value(), arb_json(2)).prop_map(|(entity, attr_id, value)| {
        let step = TripleStep {
            entity: EntityLookup::Id(entity),
            etype: None,
            attr_id,
            reverse_etype: None,
            value: value.clone(),
            options: TripleOptions::default(),
        };
        let input = json!(["retract-triple", entity, attr_id, value]);
        (input, TransactionStep::RetractTriple(step))
    })
}

fn delete_entity_step() -> impl Strategy<Value = (Value, TransactionStep)> {
    (uuid_value(), etype_value()).prop_map(|(entity, etype)| {
        let step = DeleteEntityStep {
            entity: EntityLookup::Id(entity),
            etype: etype.clone(),
        };
        let input = match &etype {
            Some(etype) => json!(["delete-entity", entity, etype]),
            None => json!(["delete-entity", entity]),
        };
        (input, TransactionStep::DeleteEntity(step))
    })
}

fn rule_params_step() -> impl Strategy<Value = (Value, TransactionStep)> {
    let params = proptest::collection::btree_map(
        string_regex("[a-z]{1,4}").expect("valid regex"),
        arb_json(1),
        0..3,
    )
    .prop_map(|entries| entries.into_iter().collect::<Map<String, Value>>());
    (uuid_value(), etype_value(), params).prop_map(|(entity, etype, params)| {
        let step = RuleParamsStep {
            entity: EntityLookup::Id(entity),
            etype: etype.clone(),
            params: params.clone(),
        };
        let input = match &etype {
            Some(etype) => json!(["rule-params", entity, etype, params]),
            None => json!(["rule-params", entity, params]),
        };
        (input, TransactionStep::RuleParams(step))
    })
}

fn attribute_id_step() -> impl Strategy<Value = (Value, TransactionStep)> {
    (any::<bool>(), uuid_value()).prop_map(|(delete, id)| {
        let (operation, step) = if delete {
            ("delete-attr", TransactionStep::DeleteAttr(id))
        } else {
            ("restore-attr", TransactionStep::RestoreAttr(id))
        };
        (json!([operation, id]), step)
    })
}

fn valid_step() -> impl Strategy<Value = (Value, TransactionStep)> {
    proptest::prop_oneof![
        add_triple_step(),
        retract_triple_step(),
        delete_entity_step(),
        rule_params_step(),
        attribute_id_step(),
    ]
}

proptest! {
    #[test]
    fn valid_generated_steps_round_trip(input_and_step in valid_step()) {
        let (input, expected) = input_and_step;
        let compiled = compile_transaction(&Value::Array(vec![input]), &AttributeCatalog::default());
        match compiled {
            Ok(steps) => prop_assert_eq!(steps, vec![expected], "generated step did not round-trip"),
            Err(_) => prop_assert!(false, "generated valid step failed to parse"),
        }
    }

    #[test]
    fn arbitrary_json_never_panics_transaction_parsing(input in arb_json(3)) {
        let _ = compile_transaction(&input, &AttributeCatalog::default());
        let _ = compile_transaction_all_errors(&input, &AttributeCatalog::default());
    }

    #[test]
    fn arbitrary_step_arrays_never_panic_transaction_parsing(
        steps in proptest::collection::vec(arb_json(3), 0..5)
    ) {
        let input = Value::Array(steps);
        let _ = compile_transaction(&input, &AttributeCatalog::default());
        let _ = compile_transaction_all_errors(&input, &AttributeCatalog::default());
    }
}
