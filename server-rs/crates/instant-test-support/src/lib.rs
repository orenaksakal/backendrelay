//! Shared helpers for tests that need a live PostgreSQL database or migration database.

/// Only `REQUIRE_TEST_DATABASE` turns a skip into a failure. `CI` is not
/// consulted: CI providers set it for every job, including the lanes that run
/// unit tests without a database.
fn database_required() -> bool {
    std::env::var("REQUIRE_TEST_DATABASE").is_ok_and(|value| {
        let value = value.trim();
        !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

fn lookup(variable: &str, test_name: &str) -> Option<String> {
    if let Ok(url) = std::env::var(variable) {
        if !url.trim().is_empty() {
            return Some(url);
        }
    }
    if database_required() {
        panic!("{variable} is required (REQUIRE_TEST_DATABASE set) for {test_name}");
    }
    eprintln!("{variable} is not set; {test_name} skipped");
    None
}

/// Returns the disposable test database URL, or skips the calling test.
///
/// The caller is expected to `return` from the test when this is `None`.
#[must_use]
pub fn test_database_url(test_name: &str) -> Option<String> {
    lookup("TEST_DATABASE_URL", test_name)
}

/// Returns the disposable migration database URL, or skips the calling test.
///
/// The caller is expected to `return` from the test when this is `None`.
#[must_use]
pub fn test_migration_database_url(test_name: &str) -> Option<String> {
    lookup("TEST_MIGRATION_DATABASE_URL", test_name)
}
