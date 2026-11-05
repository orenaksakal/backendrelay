//! Shared mapping for database failures surfaced through `ApiError`.
use instant_http::{ApiError, ApiErrorType};

/// Longest database error text kept in one log line, including any ellipsis.
const MAX_LOG_MESSAGE_LEN: usize = 500;

/// Strips PostgreSQL `DETAIL:` lines (they can contain row values) and caps
/// the remaining message so one failure cannot dominate the log stream.
#[must_use]
pub(crate) fn scrub_database_error(message: &str) -> String {
    let head = message
        .split("\nDETAIL:")
        .next()
        .unwrap_or(message)
        .trim_end();
    if head.chars().count() <= MAX_LOG_MESSAGE_LEN {
        return head.to_owned();
    }
    let mut truncated: String = head.chars().take(MAX_LOG_MESSAGE_LEN - 1).collect();
    truncated.push('…');
    truncated
}

/// Logs one scrubbed `error`-level event for a database failure.
pub(crate) fn log_database_error(message: &str) {
    let scrubbed = scrub_database_error(message);
    tracing::error!(
        target: "backendrelay::db",
        { error.message = %scrubbed },
        "database error"
    );
}

/// Logs one scrubbed `error`-level event for an unexpected internal failure.
pub(crate) fn log_internal_error(message: &str) {
    let scrubbed = scrub_database_error(message);
    tracing::error!(
        target: "backendrelay::internal",
        { error.message = %scrubbed },
        "internal error"
    );
}

fn log_postgres_error(error: &tokio_postgres::Error) {
    let message = error
        .as_db_error()
        .map_or_else(|| error.to_string(), ToString::to_string);
    let scrubbed = scrub_database_error(&message);
    let sqlstate = error
        .code()
        .map_or("none", tokio_postgres::error::SqlState::code);
    tracing::error!(
        target: "backendrelay::db",
        { error.message = %scrubbed, error.sqlstate = %sqlstate },
        "database error"
    );
}

/// Maps a database failure to the public generic error while logging the
/// scrubbed cause once. Callers must not append the driver message.
#[must_use]
pub(crate) fn database_error(error: impl std::fmt::Display) -> ApiError {
    log_database_error(&error.to_string());
    ApiError::new(ApiErrorType::Unknown, "Database error")
}

/// Variant for concrete PostgreSQL errors that also records the SQLSTATE.
#[allow(clippy::needless_pass_by_value)] // `map_err` hands over the owned error.
#[must_use]
pub(crate) fn database_error_pg(error: tokio_postgres::Error) -> ApiError {
    log_postgres_error(&error);
    ApiError::new(ApiErrorType::Unknown, "Database error")
}

/// Maps an unexpected internal failure to the public generic error while
/// logging the scrubbed cause once. Callers must not append the cause.
#[must_use]
pub(crate) fn internal_error(error: impl std::fmt::Display) -> ApiError {
    log_internal_error(&error.to_string());
    ApiError::new(ApiErrorType::Unknown, "Internal error")
}

/// Maps an `InstaQL` validation failure (for example a number that does not
/// fit a finite `f64`) to a client-facing validation error.
#[allow(clippy::needless_pass_by_value)] // `map_err` hands over the owned error.
#[must_use]
pub(crate) fn query_validation_error(error: instant_instaql::InstaqlError) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbing_removes_postgres_detail_values() {
        let message = "ERROR: duplicate key value violates unique constraint\nDETAIL: Key (email)=(user@example.com) already exists.";
        assert_eq!(
            scrub_database_error(message),
            "ERROR: duplicate key value violates unique constraint"
        );
    }

    #[test]
    fn scrubbing_caps_long_messages() {
        let message = "x".repeat(MAX_LOG_MESSAGE_LEN * 2);
        let scrubbed = scrub_database_error(&message);
        assert_eq!(scrubbed.chars().count(), MAX_LOG_MESSAGE_LEN);
        assert!(scrubbed.ends_with('…'));
    }

    #[test]
    fn database_errors_hide_the_driver_message() {
        let error = database_error("relation \"secret_table\" does not exist");
        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Database error");
    }

    #[test]
    fn internal_errors_hide_the_cause() {
        let error = internal_error("join task panicked: secret_token=abc123");
        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Internal error");
    }
}
