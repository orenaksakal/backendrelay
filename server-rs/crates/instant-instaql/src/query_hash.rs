use std::cmp::Ordering;

use serde_json::{Map, Number, Value};

use crate::{InstaqlError, QueryPathSegment, ValidationError};

const KEYWORD_HASH_SALT: i32 = 0x9e37_79b9_u32.cast_signed();
const MURMUR_C1: u32 = 0xcc9e_2d51;
const MURMUR_C2: u32 = 0x1b87_3593;

#[derive(Clone, Copy)]
enum HashMode {
    Normal,
    Options,
    Where,
}

/// Computes Clojure 1.12's `hash` of `normalized-forms` for a JSON `InstaQL` query.
/// Object keys are Ring's keywordized JSON keys; scalar values retain their
/// JSON types except inside `where`, where the reference replaces them with
/// type keywords. Page cursors are replaced with `:cursor`.
///
/// # Errors
///
/// Returns [`InstaqlError::Validation`] when a number has no finite,
/// representable `i64`, `u64`, or `f64` form. `serde_json`'s
/// `arbitrary_precision` parsing accepts literals such as `1e400`, and hashing
/// them would otherwise panic on untrusted input.
pub fn normalized_query_hash(query: &Value) -> Result<i32, InstaqlError> {
    value_hasheq(query, HashMode::Normal)
}

/// Rejects JSON numbers with no finite, representable form.
///
/// Realtime and HTTP handlers run this before compiling or hashing a query so
/// a literal such as `1e400` becomes a validation error instead of a panic in
/// query hashing or SQL parameter rendering.
///
/// # Errors
///
/// Returns [`InstaqlError::Validation`] naming the first offending path.
pub fn validate_query_numbers(query: &Value) -> Result<(), InstaqlError> {
    validate_numbers_at(query, &mut Vec::new())
}

fn validate_numbers_at(
    value: &Value,
    path: &mut Vec<QueryPathSegment>,
) -> Result<(), InstaqlError> {
    match value {
        Value::Number(value) => {
            if !representable_number(value) {
                return Err(unrepresentable_number_error(value, path));
            }
            Ok(())
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                path.push(QueryPathSegment::Index(index));
                validate_numbers_at(value, path)?;
                path.pop();
            }
            Ok(())
        }
        Value::Object(values) => {
            for (key, value) in values {
                path.push(QueryPathSegment::Field(key.clone()));
                validate_numbers_at(value, path)?;
                path.pop();
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
    }
}

fn value_hasheq(value: &Value, mode: HashMode) -> Result<i32, InstaqlError> {
    if matches!(mode, HashMode::Where) {
        match value {
            Value::String(_) => return Ok(keyword_hasheq("string")),
            Value::Number(_) => return Ok(keyword_hasheq("number")),
            Value::Bool(_) => return Ok(keyword_hasheq("boolean")),
            Value::Null | Value::Array(_) | Value::Object(_) => {}
        }
    }
    match value {
        Value::Null => Ok(0),
        Value::Bool(value) => Ok(if *value { 1231 } else { 1237 }),
        Value::Number(value) => number_hasheq(value),
        Value::String(value) => Ok(murmur_hash_int(java_string_hash(value))),
        Value::Array(values) => {
            let mut hashed = Vec::with_capacity(values.len());
            for value in values {
                hashed.push(value_hasheq(value, mode)?);
            }
            Ok(ordered_hasheq(hashed))
        }
        Value::Object(values) => object_hasheq(values, mode),
    }
}

fn object_hasheq(values: &Map<String, Value>, mode: HashMode) -> Result<i32, InstaqlError> {
    let mut hash = 0_i32;
    for (key, value) in values {
        let value_hash = match mode {
            HashMode::Where => value_hasheq(value, HashMode::Where)?,
            HashMode::Options if key == "where" => value_hasheq(value, HashMode::Where)?,
            HashMode::Options if matches!(key.as_str(), "before" | "after") => {
                keyword_hasheq("cursor")
            }
            HashMode::Options | HashMode::Normal => {
                let child_mode = if key == "$" && value.is_object() {
                    HashMode::Options
                } else {
                    HashMode::Normal
                };
                value_hasheq(value, child_mode)?
            }
        };
        hash = hash.wrapping_add(ordered_hasheq([keyword_hasheq(key), value_hash]));
    }
    Ok(mix_collection_hash(hash, values.len()))
}

fn ordered_hasheq(values: impl IntoIterator<Item = i32>) -> i32 {
    let mut hash = 1_i32;
    let mut count = 0_usize;
    for value in values {
        hash = hash.wrapping_mul(31).wrapping_add(value);
        count += 1;
    }
    mix_collection_hash(hash, count)
}

pub(crate) fn keyword_hasheq(value: &str) -> i32 {
    let (namespace, name) = value
        .split_once('/')
        .filter(|_| value != "/")
        .map_or((None, value), |(namespace, name)| (Some(namespace), name));
    let name_hash = murmur_hash_unencoded_chars(name);
    let namespace_hash = namespace.map_or(0, java_string_hash);
    hash_combine(name_hash, namespace_hash).wrapping_add(KEYWORD_HASH_SALT)
}

/// Returns JSON-object entries in Clojure `PersistentHashMap` sequence order.
/// Cheshire 6 parses every object by associating into `(transient {})`, so
/// traversal follows the keyword hash-array-mapped trie rather than JSON wire
/// order or lexical key order.
pub(crate) fn clojure_map_entries(values: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries = values.iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| clojure_keyword_order(left, right));
    entries
}

fn clojure_keyword_order(left: &str, right: &str) -> Ordering {
    let left_hash = keyword_hasheq(left).cast_unsigned();
    let right_hash = keyword_hasheq(right).cast_unsigned();
    for shift in (0..32).step_by(5) {
        let left_partition = (left_hash >> shift) & 0x1f;
        let right_partition = (right_hash >> shift) & 0x1f;
        match left_partition.cmp(&right_partition) {
            Ordering::Equal => {}
            order => return order,
        }
    }
    // Distinct keys with a full keyword-hash collision occupy one collision
    // node. JSON object member ordering is not a public protocol contract, so
    // lexical order gives legacy `Value` callers a stable deterministic tie-break.
    left.cmp(right)
}

fn number_hasheq(value: &Number) -> Result<i32, InstaqlError> {
    if let Some(value) = value.as_i64() {
        return Ok(murmur_hash_long(value.cast_unsigned()));
    }
    if let Some(value) = value.as_u64() {
        if let Ok(value) = i64::try_from(value) {
            return Ok(murmur_hash_long(value.cast_unsigned()));
        }
        return Ok(high_u32(value)
            .cast_signed()
            .wrapping_mul(31)
            .wrapping_add(low_u32(value).cast_signed()));
    }
    let Some(value) = value.as_f64().filter(|value| value.is_finite()) else {
        return Err(unrepresentable_number_error(value, &[]));
    };
    let bits = value.to_bits();
    Ok(low_u32(bits ^ (bits >> 32)).cast_signed())
}

fn representable_number(value: &Number) -> bool {
    value.as_i64().is_some()
        || value.as_u64().is_some()
        || value.as_f64().is_some_and(f64::is_finite)
}

fn unrepresentable_number_error(value: &Number, path: &[QueryPathSegment]) -> InstaqlError {
    InstaqlError::Validation(vec![ValidationError {
        expected: "finite-number?".to_owned(),
        path: path.to_vec(),
        message: format!("The JSON number {value} is not finite and cannot be hashed."),
    }])
}

fn java_string_hash(value: &str) -> i32 {
    value.encode_utf16().fold(0_i32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(i32::from(unit))
    })
}

fn hash_combine(seed: i32, hash: i32) -> i32 {
    seed ^ hash
        .wrapping_add(KEYWORD_HASH_SALT)
        .wrapping_add(seed.wrapping_shl(6))
        .wrapping_add(seed >> 2)
}

fn murmur_hash_int(input: i32) -> i32 {
    if input == 0 {
        return 0;
    }
    fmix(mix_h1(0, mix_k1(input.cast_unsigned())), 4).cast_signed()
}

fn murmur_hash_long(input: u64) -> i32 {
    if input == 0 {
        return 0;
    }
    let hash = mix_h1(0, mix_k1(low_u32(input)));
    fmix(mix_h1(hash, mix_k1(high_u32(input))), 8).cast_signed()
}

fn murmur_hash_unencoded_chars(input: &str) -> i32 {
    let units = input.encode_utf16().collect::<Vec<_>>();
    let mut hash = 0_u32;
    let (pairs, remainder) = units.as_chunks::<2>();
    for pair in pairs {
        let value = u32::from(pair[0]) | (u32::from(pair[1]) << 16);
        hash = mix_h1(hash, mix_k1(value));
    }
    if let Some(unit) = remainder.first() {
        hash ^= mix_k1(u32::from(*unit));
    }
    let byte_length = units
        .len()
        .checked_mul(2)
        .and_then(|length| u32::try_from(length).ok())
        .expect("an InstaQL key is smaller than four gigabytes");
    fmix(hash, byte_length).cast_signed()
}

fn mix_collection_hash(hash: i32, count: usize) -> i32 {
    let count = u32::try_from(count).expect("a JSON collection has fewer than four billion items");
    fmix(mix_h1(0, mix_k1(hash.cast_unsigned())), count).cast_signed()
}

fn low_u32(value: u64) -> u32 {
    let bytes = value.to_le_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn high_u32(value: u64) -> u32 {
    let bytes = value.to_le_bytes();
    u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]])
}

fn mix_k1(mut value: u32) -> u32 {
    value = value.wrapping_mul(MURMUR_C1);
    value = value.rotate_left(15);
    value.wrapping_mul(MURMUR_C2)
}

fn mix_h1(mut hash: u32, value: u32) -> u32 {
    hash ^= value;
    hash = hash.rotate_left(13);
    hash.wrapping_mul(5).wrapping_add(0xe654_6b64)
}

fn fmix(mut hash: u32, length: u32) -> u32 {
    hash ^= length;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2_ae35);
    hash ^ (hash >> 16)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn primitive_hasheq_matches_clojure_1_12() {
        assert_eq!(value_hasheq(&Value::Null, HashMode::Normal), Ok(0));
        assert_eq!(value_hasheq(&json!(false), HashMode::Normal), Ok(1237));
        assert_eq!(value_hasheq(&json!(true), HashMode::Normal), Ok(1231));
        assert_eq!(value_hasheq(&json!(0), HashMode::Normal), Ok(0));
        assert_eq!(value_hasheq(&json!(1), HashMode::Normal), Ok(1_392_991_556));
        assert_eq!(
            value_hasheq(&json!(1.5), HashMode::Normal),
            Ok(1_073_217_536)
        );
        assert_eq!(
            value_hasheq(&json!("users"), HashMode::Normal),
            Ok(634_031_600)
        );
        assert_eq!(keyword_hasheq("users"), -713_552_705);
        assert_eq!(
            value_hasheq(&json!([]), HashMode::Normal),
            Ok(-2_017_569_654)
        );
        assert_eq!(value_hasheq(&json!({}), HashMode::Normal), Ok(-15_128_758));
    }

    #[test]
    fn unrepresentable_numbers_are_rejected_instead_of_panicking() {
        // `serde_json`'s `arbitrary_precision` feature accepts a literal whose
        // `f64` conversion overflows to infinity. Values inside `where` hash as
        // a type keyword, so hash a number that is hashed by value.
        let query: Value = serde_json::from_str(r#"{"users": {"$": {"limit": 1e400}}}"#).unwrap();
        let error = normalized_query_hash(&query).unwrap_err();
        let InstaqlError::Validation(errors) = &error else {
            panic!("expected a validation error, got {error}");
        };
        assert_eq!(errors[0].expected, "finite-number?");
        assert!(errors[0].message.contains("not finite"));

        // Query ingress rejects it even where hashing itself would not need it.
        let in_where: Value =
            serde_json::from_str(r#"{"users": {"$": {"where": {"age": {"$gt": 1e400}}}}}"#)
                .unwrap();
        let error = validate_query_numbers(&in_where).unwrap_err();
        let InstaqlError::Validation(errors) = &error else {
            panic!("expected a validation error, got {error}");
        };
        assert_eq!(errors[0].expected, "finite-number?");
        assert_eq!(
            errors[0].path,
            [
                QueryPathSegment::Field("users".to_owned()),
                QueryPathSegment::Field("$".to_owned()),
                QueryPathSegment::Field("where".to_owned()),
                QueryPathSegment::Field("age".to_owned()),
                QueryPathSegment::Field("$gt".to_owned()),
            ]
        );

        // Integers wider than u64 still hash through their finite `f64` form,
        // exactly as the reference implementation's double coercion does.
        let wide: Value = serde_json::from_str(
            r#"{"users": {"$": {"where": {"age": 123456789012345678901234567890}}}}"#,
        )
        .unwrap();
        let wide_limit: Value =
            serde_json::from_str(r#"{"users": {"$": {"limit": 123456789012345678901234567890}}}"#)
                .unwrap();
        assert!(normalized_query_hash(&wide_limit).is_ok());
        assert!(validate_query_numbers(&wide).is_ok());

        // Ordinary large integers and floats still hash.
        assert!(validate_query_numbers(&json!({"n": u64::MAX, "f": 1.5})).is_ok());
        assert!(normalized_query_hash(&json!({"n": u64::MAX})).is_ok());
    }

    #[test]
    fn normalized_public_query_hashes_match_clojure_1_12_oracle() {
        let fixtures = [
            (json!({"users": {}}), -1_199_196_952),
            (
                json!({"users": {"$": {"where": {
                    "handle": "alex", "age": 42, "active": true
                }}}}),
                -1_592_566_014,
            ),
            (
                json!({"users": {"$": {
                    "where": {
                        "id": "123e4567-e89b-12d3-a456-426614174000",
                        "handle": {"$in": ["a", "b"]}
                    },
                    "limit": 25,
                    "before": ["x", "y", "z", "w"]
                }}}),
                -1_705_203_576,
            ),
            (
                json!({"users": {"posts": {"$": {"where": {
                    "title": {"$like": "%rust%"}, "published": false
                }}}}}),
                50_618_862,
            ),
            (
                json!({"users": {"$": {
                    "where": {"deleted": null, "score": {"$gt": 1.5}},
                    "order": {"serverCreatedAt": "desc"},
                    "after": [1, 2, 3, 4]
                }}}),
                204_029_082,
            ),
        ];
        for (query, expected) in fixtures {
            assert_eq!(normalized_query_hash(&query), Ok(expected), "{query}");
        }
    }

    #[test]
    fn where_values_and_page_cursors_do_not_change_the_hash() {
        let first = json!({"users": {"$": {
            "where": {"handle": "alex", "age": 1, "active": true},
            "before": ["a", "b", "c", "d"]
        }}});
        let second = json!({"users": {"$": {
            "where": {"handle": "bea", "age": 99, "active": false},
            "before": ["w", "x", "y", "z"]
        }}});
        assert_eq!(
            normalized_query_hash(&first).unwrap(),
            normalized_query_hash(&second).unwrap()
        );
    }

    #[test]
    fn cheshire_object_iteration_uses_keyword_hamt_order() {
        let value: Value = serde_json::from_str(
            r#"{"k0":0,"k1":1,"k2":2,"k3":3,"k4":4,"k5":5,"k6":6,"k7":7,"k8":8,"k9":9}"#,
        )
        .unwrap();
        let serialized = value.to_string();
        let hash = normalized_query_hash(&value).unwrap();
        let keys = clojure_map_entries(value.as_object().unwrap())
            .into_iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            ["k8", "k5", "k7", "k9", "k6", "k0", "k3", "k4", "k1", "k2"]
        );
        assert_eq!(value.to_string(), serialized);
        assert_eq!(normalized_query_hash(&value), Ok(hash));

        let first: Value = serde_json::from_str(r#"{"zeta":{},"alpha":{}}"#).unwrap();
        let second: Value = serde_json::from_str(r#"{"alpha":{},"zeta":{}}"#).unwrap();
        let ordered = |value: &Value| {
            clojure_map_entries(value.as_object().unwrap())
                .into_iter()
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ordered(&first), ["zeta", "alpha"]);
        assert_eq!(ordered(&first), ordered(&second));

        let children: Value = serde_json::from_str(r#"{"bookshelves":{},"alpha":{}}"#).unwrap();
        assert_eq!(ordered(&children), ["bookshelves", "alpha"]);
    }
}
