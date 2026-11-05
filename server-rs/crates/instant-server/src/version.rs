//! Build metadata embedded by `build.rs`.
//!
//! Tagged builds receive `BACKENDRELAY_VERSION` (the date-based tag, for
//! example `v2026.11.1`) and `BACKENDRELAY_GIT_SHA`; local builds fall back to
//! `<crate version>-dev` and the checked-out revision.

/// Release version, or `<crate version>-dev` for an untagged build.
pub const VERSION: &str = env!("BACKENDRELAY_VERSION");

/// Short git revision the binary was built from, or `unknown`.
pub const GIT_SHA: &str = env!("BACKENDRELAY_GIT_SHA");

/// Latest schema migration embedded in this binary.
#[must_use]
pub fn migration_version() -> u64 {
    crate::migrations::latest_embedded_version()
}

/// The one-line banner printed by `instant-server --version`.
#[must_use]
pub fn display_version() -> String {
    format!("instant-server {VERSION} ({GIT_SHA})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_version_names_the_binary_and_embedded_revision() {
        let line = display_version();
        assert!(line.starts_with("instant-server "));
        assert!(line.ends_with(&format!(" ({GIT_SHA})")));
        assert!(line.contains(VERSION));
    }

    #[test]
    fn build_metadata_is_never_empty() {
        assert!(!VERSION.trim().is_empty());
        assert!(!GIT_SHA.trim().is_empty());
        assert!(migration_version() > 0);
    }
}
