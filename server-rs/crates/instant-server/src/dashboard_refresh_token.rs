//! Dashboard (`instant_users`) refresh-token storage and lookup.
//!
//! Tokens are compared by the SHA-256 of their canonical UUID text, and a
//! session that goes unused for 30 days stops authenticating. Use is stamped at
//! most once per hour so a busy dashboard does not write on every request.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Hashes a dashboard refresh token exactly as the `token_hash` column stores
/// it: SHA-256 over the canonical UUID text.
#[must_use]
pub(crate) fn hash_token(token: Uuid) -> Vec<u8> {
    Sha256::digest(token.to_string().as_bytes()).to_vec()
}

/// Inserts a refresh token under both the plaintext key and its hash.
///
/// The plaintext column is only dropped by a later migration, so both must stay
/// populated until then.
pub(crate) const INSERT_SQL: &str =
    "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id) VALUES ($1, $2, $3)";

/// Loads the dashboard user for a live refresh token.
///
/// Columns are `(id, email, created_at)`. The data-modifying CTE refreshes
/// `last_used_at` at most once per hour; tokens idle for 30 days match no row.
pub(crate) const LOAD_SESSION_SQL: &str = "\
WITH touched AS (
    UPDATE instant_user_refresh_tokens
       SET last_used_at = now()
     WHERE token_hash = $1
       AND last_used_at > now() - interval '30 days'
       AND last_used_at < now() - interval '1 hour'
    RETURNING id
)
SELECT users.id, users.email, users.created_at
  FROM instant_users users
  JOIN instant_user_refresh_tokens tokens ON tokens.user_id = users.id
 WHERE tokens.token_hash = $1
   AND tokens.last_used_at > now() - interval '30 days'";

/// Loads `(id, email)` for a live refresh token on management routes, which do
/// not project the account creation time.
pub(crate) const LOAD_USER_SQL: &str = "\
WITH touched AS (
    UPDATE instant_user_refresh_tokens
       SET last_used_at = now()
     WHERE token_hash = $1
       AND last_used_at > now() - interval '30 days'
       AND last_used_at < now() - interval '1 hour'
    RETURNING id
)
SELECT users.id, users.email
  FROM instant_users users
  JOIN instant_user_refresh_tokens tokens ON tokens.user_id = users.id
 WHERE tokens.token_hash = $1
   AND tokens.last_used_at > now() - interval '30 days'";
