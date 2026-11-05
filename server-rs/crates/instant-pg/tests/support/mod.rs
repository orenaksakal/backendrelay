//! Shared helpers for envelope-level decoder parity checks.
//!
//! Every fixture that decodes a planned nested `InstaQL` envelope also decodes
//! the same document through the production raw-text path. The two decoders must
//! agree exactly, including malformed-input errors.

use instant_instaql::{ExecutedNode, PlannedNestedQuery};
use instant_pg::{
    PgQueryError, decode_planned_instaql_envelope, decode_planned_instaql_envelope_text,
};
use serde_json::Value;

/// Decodes one envelope through both transports and asserts identical outcomes.
pub fn decode_envelope(
    plan: &PlannedNestedQuery,
    envelope: &Value,
) -> Result<Vec<ExecutedNode>, PgQueryError> {
    let text = serde_json::to_string(envelope).expect("fixture envelopes serialize as JSON");
    let raw =
        decode_planned_instaql_envelope_text(plan, &text).map_err(|error| format!("{error:?}"));
    let value =
        decode_planned_instaql_envelope(plan, envelope).map_err(|error| format!("{error:?}"));
    assert_eq!(raw, value, "raw-text and Value envelope decoders diverged");
    decode_planned_instaql_envelope(plan, envelope)
}
