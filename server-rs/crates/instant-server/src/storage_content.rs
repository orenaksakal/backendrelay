//! Content policy for stored app files.
//!
//! Uploads may carry any client-declared type, but only a small allowlist of
//! media is stored and served inline. Everything else is persisted as
//! `application/octet-stream` with `Content-Disposition: attachment`, so
//! `text/html`, `image/svg+xml` and JavaScript documents can never execute on
//! the storage origin. Both headers are decided once, at upload time, and
//! presigned downloads simply serve the stored object.

/// MIME types that browsers may render inline.
pub const INLINE_CONTENT_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "video/mp4",
    "video/webm",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "application/pdf",
];

/// Type persisted for client types that are not on the inline allowlist.
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

fn content_type_essence(value: &str) -> &str {
    value.split(';').next().unwrap_or(value).trim()
}

/// Whether `content_type` is on the inline allowlist, ignoring parameters and
/// case.
#[must_use]
pub fn is_inline_content_type(content_type: &str) -> bool {
    let essence = content_type_essence(content_type);
    INLINE_CONTENT_TYPES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(essence))
}

/// The metadata type to persist for a client-declared type.
///
/// Allowlisted types are stored as their normalized MIME essence; every other
/// type, including none at all, is stored as `application/octet-stream`.
#[must_use]
pub fn stored_content_type(requested: Option<&str>) -> String {
    requested
        .filter(|value| is_inline_content_type(value))
        .map_or_else(
            || DEFAULT_CONTENT_TYPE.to_owned(),
            |value| content_type_essence(value).to_ascii_lowercase(),
        )
}

/// The disposition to store for an upload. A client may always ask for
/// `attachment` (for example `attachment; filename="report.pdf"`), which only
/// makes serving stricter; otherwise allowlisted types are served inline and
/// everything else is forced to `attachment`.
#[must_use]
pub fn stored_content_disposition(stored_content_type: &str, requested: Option<&str>) -> String {
    if let Some(requested) = requested.map(str::trim)
        && requested.len() <= 512
        && requested.bytes().all(|byte| (0x20..0x7f).contains(&byte))
        && requested
            .get(..10)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("attachment"))
    {
        return requested.to_owned();
    }
    if is_inline_content_type(stored_content_type) {
        "inline".to_owned()
    } else {
        "attachment".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_may_only_tighten_the_stored_disposition() {
        assert_eq!(
            stored_content_disposition(
                "application/pdf",
                Some("attachment; filename=\"report.pdf\"")
            ),
            "attachment; filename=\"report.pdf\""
        );
        assert_eq!(
            stored_content_disposition("image/png", Some("Attachment")),
            "Attachment"
        );
        assert_eq!(
            stored_content_disposition("application/octet-stream", Some("inline")),
            "attachment"
        );
        assert_eq!(
            stored_content_disposition("image/png", Some("inline")),
            "inline"
        );
        assert_eq!(
            stored_content_disposition("image/png", Some("attachment\r\nX-Evil: 1")),
            "inline"
        );
    }

    #[test]
    fn inline_allowlist_matches_the_essence_case_insensitively() {
        assert!(is_inline_content_type("image/png"));
        assert!(is_inline_content_type(" IMAGE/PNG ; charset=binary"));
        assert!(is_inline_content_type("application/pdf"));
        for value in [
            "text/html",
            "image/svg+xml",
            "application/xhtml+xml",
            "application/javascript",
            "text/ecmascript",
            "text/xml",
            "application/xml",
            "",
        ] {
            assert!(!is_inline_content_type(value), "accepted {value}");
        }
    }

    #[test]
    fn non_allowlisted_types_are_stored_as_octet_stream() {
        assert_eq!(stored_content_type(None), "application/octet-stream");
        assert_eq!(
            stored_content_type(Some("text/html")),
            "application/octet-stream"
        );
        assert_eq!(
            stored_content_type(Some("image/svg+xml")),
            "application/octet-stream"
        );
        assert_eq!(
            stored_content_type(Some("application/javascript")),
            "application/octet-stream"
        );
        assert_eq!(
            stored_content_type(Some("Image/PNG; charset=binary")),
            "image/png"
        );
    }

    #[test]
    fn dispositions_force_attachment_outside_the_allowlist() {
        assert_eq!(stored_content_disposition("image/png", None), "inline");
        assert_eq!(
            stored_content_disposition("application/pdf", None),
            "inline"
        );
        assert_eq!(
            stored_content_disposition("application/octet-stream", None),
            "attachment"
        );
        assert_eq!(stored_content_disposition("text/html", None), "attachment");
        assert_eq!(stored_content_disposition("", None), "attachment");
    }
}
