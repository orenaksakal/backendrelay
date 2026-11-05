//! Property coverage for the public `InstaQL` parsers: query coercion and hashing.
//!
//! These suites exercise unauthenticated input the realtime and HTTP handlers
//! accept: arbitrary JSON, deeply nested forms, and numbers that only
//! `serde_json`'s `arbitrary_precision` feature can represent.

use instant_instaql::{
    AttributeCatalog, Form, coerce_forms, normalized_query_hash, validate_query_numbers,
};
use proptest::prelude::{
    BoxedStrategy, Just, Strategy, any, prop_assert, prop_assert_eq, prop_assert_ne,
};
use proptest::proptest;
use proptest::string::string_regex;
use serde_json::{Map, Value, json};

/// Builds a small catalog so `where`/`order` coercion sees known attributes.
fn catalog() -> AttributeCatalog {
    let attributes = json!([
        {
            "id": "11111111-1111-1111-1111-111111111111",
            "etype": "users",
            "label": "id",
            "value_type": "blob",
            "cardinality": "one",
            "unique": true,
            "indexed": true,
            "required": true,
            "checked_data_type": null,
            "indexing": false,
            "checking_data_type": false,
            "reverse_etype": null,
            "reverse_label": null
        },
        {
            "id": "22222222-2222-2222-2222-222222222222",
            "etype": "users",
            "label": "handle",
            "value_type": "blob",
            "cardinality": "one",
            "unique": true,
            "indexed": true,
            "required": false,
            "checked_data_type": "string",
            "indexing": false,
            "checking_data_type": false,
            "reverse_etype": null,
            "reverse_label": null
        }
    ]);
    serde_json::from_value(json!({ "attributes": attributes })).expect("fixture catalog")
}

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

/// Arbitrary JSON no deeper than `depth` nested arrays/objects.
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

/// Nested plain-object queries with no option maps, which always coerce.
fn arb_structural_query(depth: u32) -> BoxedStrategy<Value> {
    if depth == 0 {
        return Just(Value::Object(Map::new())).boxed();
    }
    proptest::collection::btree_map(
        string_regex("[A-Za-z][A-Za-z0-9_]{0,5}").expect("valid regex"),
        arb_structural_query(depth - 1),
        0..3,
    )
    .prop_map(|entries| Value::Object(entries.into_iter().collect()))
    .boxed()
}

/// Rebuilds the public query shape from coerced forms, keeping child order.
fn query_from_forms(forms: &[Form]) -> Value {
    let mut object = Map::new();
    for form in forms {
        object.insert(form.key.clone(), query_from_forms(&form.children));
    }
    Value::Object(object)
}

proptest! {
    #[test]
    fn arbitrary_json_never_panics_instaql_coercion(query in arb_json(4)) {
        let _ = coerce_forms(&query, &catalog());
    }

    #[test]
    fn coerced_structural_queries_are_idempotent(query in arb_structural_query(4)) {
        let catalog = catalog();
        let forms = coerce_forms(&query, &catalog)
            .expect("plain nested objects carry no coercible options");
        let rebuilt = query_from_forms(&forms);
        let recoerced = coerce_forms(&rebuilt, &catalog)
            .expect("coerced structural form must coerce again");
        prop_assert_eq!(recoerced, forms, "re-coercing a coerced query changed the forms");
    }

    #[test]
    fn arbitrary_json_never_panics_query_hashing(value in arb_json(4)) {
        let validated = validate_query_numbers(&value);
        let hashed = normalized_query_hash(&value);
        if validated.is_ok() {
            prop_assert!(
                hashed.is_ok(),
                "a validated query failed to hash"
            );
        }
    }

    #[test]
    fn query_hashes_are_deterministic_and_key_order_independent(value in arb_json(4)) {
        let first = normalized_query_hash(&value);
        prop_assert_eq!(normalized_query_hash(&value), first.clone(), "hashing is not deterministic");
        if let Value::Object(entries) = &value {
            let reordered = entries
                .iter()
                .rev()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<Map<_, _>>();
            prop_assert_eq!(
                normalized_query_hash(&Value::Object(reordered)),
                first,
                "object key order changed the hash"
            );
        }
    }

    #[test]
    fn structurally_different_queries_do_not_collide(
        key in string_regex("[a-z]{1,6}").expect("valid regex"),
        other in string_regex("[a-z]{1,6}").expect("valid regex"),
    ) {
        if key == other {
            return Ok(());
        }
        let mut smaller = Map::new();
        smaller.insert(key.clone(), json!({}));
        let mut larger = smaller.clone();
        larger.insert(other, json!({}));
        prop_assert_ne!(
            normalized_query_hash(&Value::Object(smaller)),
            normalized_query_hash(&Value::Object(larger)),
            "adding a distinct form did not change the hash"
        );
    }

    #[test]
    fn oversized_exponents_are_rejected_without_panicking(exponent in 309_u32..400) {
        let source = format!(r#"{{"users": {{"$": {{"limit": 1e{exponent}}}}}}}"#);
        let query: Value = serde_json::from_str(&source).expect("arbitrary precision parses");
        prop_assert!(validate_query_numbers(&query).is_err());
        prop_assert!(normalized_query_hash(&query).is_err());
    }
}
