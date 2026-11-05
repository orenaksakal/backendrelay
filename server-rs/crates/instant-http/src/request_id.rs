//! Request correlation IDs shared by the HTTP middleware and error responses.
use tokio::task_local;
use uuid::Uuid;

/// Header carrying the per-request correlation ID in both directions.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

const MAX_REQUEST_ID_LEN: usize = 128;

task_local! {
    /// Request ID installed by the middleware for the current request task.
    pub static REQUEST_ID: String;
}

/// Returns the request ID installed for the current task, if any.
#[must_use]
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok()
}

/// Keeps a caller-supplied request ID when it is safe to echo, otherwise mints
/// a `UUIDv7`.
#[must_use]
pub fn resolve_request_id(header: Option<&str>) -> String {
    header
        .filter(|value| is_valid_request_id(value))
        .map_or_else(|| Uuid::now_v7().to_string(), ToOwned::to_owned)
}

fn is_valid_request_id(value: &str) -> bool {
    (1..=MAX_REQUEST_ID_LEN).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_uuid_v7() {
        let id = resolve_request_id(None);
        assert_eq!(Uuid::parse_str(&id).unwrap().get_version_num(), 7);
    }

    #[test]
    fn caller_ids_are_kept_only_when_safe() {
        assert_eq!(resolve_request_id(Some("abc.DEF_1:2-3")), "abc.DEF_1:2-3");
        for invalid in ["", "has space", "line\nbreak", "semi;colon"] {
            assert_ne!(resolve_request_id(Some(invalid)), invalid);
        }
        let too_long = "a".repeat(MAX_REQUEST_ID_LEN + 1);
        assert_ne!(resolve_request_id(Some(&too_long)), too_long);
    }
}
