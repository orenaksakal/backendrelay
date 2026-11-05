//! Optional Sentry-protocol error reporting.
//!
//! Reporting stays off unless `SENTRY_DSN` is set. When enabled, the
//! `sentry-tracing` layer turns `error!` events into Sentry events and `warn!`
//! events into breadcrumbs; the panic integration reports panics alongside the
//! local panic hook. No request bodies, cookies, headers, query strings or
//! process environment are sent.
use std::time::Duration;

use thiserror::Error;

use crate::ServerConfig;

/// How long the process waits for queued events when it shuts down.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// Keeps the Sentry client alive for the process lifetime and flushes it on
/// shutdown.
pub struct ErrorReporting {
    guard: sentry::ClientInitGuard,
}

impl ErrorReporting {
    /// Drains queued events, giving up after [`FLUSH_TIMEOUT`].
    pub fn flush(&self) {
        self.guard.flush(Some(FLUSH_TIMEOUT));
    }
}

#[derive(Debug, Error)]
pub enum ErrorReportingError {
    #[error("SENTRY_DSN is not a valid Sentry DSN")]
    InvalidDsn,
}

/// Initializes Sentry when `SENTRY_DSN` is configured. Returns `None` when the
/// variable is unset or blank, leaving the client and the tracing layer disabled.
///
/// # Errors
///
/// Returns [`ErrorReportingError::InvalidDsn`] when the configured value is not
/// a Sentry DSN.
pub fn init_error_reporting(
    config: &ServerConfig,
) -> Result<Option<ErrorReporting>, ErrorReportingError> {
    let Some(dsn) = config.sentry_dsn() else {
        return Ok(None);
    };
    if dsn.parse::<sentry::types::Dsn>().is_err() {
        return Err(ErrorReportingError::InvalidDsn);
    }
    let options = sentry::ClientOptions::new()
        .dsn(dsn)
        .release(env!("CARGO_PKG_VERSION"))
        .environment(config.sentry_environment().to_owned())
        .send_default_pii(false)
        .shutdown_timeout(FLUSH_TIMEOUT)
        .before_send(|event| Some(scrub_event(event)));
    Ok(Some(ErrorReporting {
        guard: sentry::init(options),
    }))
}

/// Removes request data that can carry credentials or personal information
/// before an event leaves the process.
fn scrub_event(mut event: sentry::protocol::Event<'static>) -> sentry::protocol::Event<'static> {
    if let Some(request) = event.request.as_mut() {
        request.data = None;
        request.cookies = None;
        request.query_string = None;
        request.headers.clear();
        request.env.clear();
    }
    event
}

#[cfg(test)]
mod tests {
    use sentry::protocol::{Event, Request};

    use super::*;

    #[test]
    fn blank_or_absent_dsn_leaves_sentry_disabled() {
        for pairs in [
            Vec::<(&str, &str)>::new(),
            vec![("SENTRY_DSN", "")],
            vec![("SENTRY_DSN", "   ")],
        ] {
            let config = ServerConfig::from_pairs(pairs).unwrap();
            assert!(init_error_reporting(&config).unwrap().is_none());
        }
    }

    #[test]
    fn invalid_dsn_is_rejected_without_initialising_sentry() {
        let config = ServerConfig::from_pairs([("SENTRY_DSN", "not-a-dsn")]).unwrap();
        assert!(matches!(
            init_error_reporting(&config),
            Err(ErrorReportingError::InvalidDsn)
        ));
    }

    #[test]
    fn before_send_drops_request_bodies_cookies_headers_and_query_strings() {
        let request = Request {
            url: Some("https://api.example.test/dash/apps".parse().unwrap()),
            method: Some("POST".to_owned()),
            data: Some(r#"{"email":"owner@example.test"}"#.to_owned()),
            cookies: Some("session=private".to_owned()),
            query_string: Some("token=private".to_owned()),
            headers: [(
                "authorization".to_owned(),
                "Bearer private-credential".to_owned(),
            )]
            .into(),
            env: [("REMOTE_ADDR".to_owned(), "203.0.113.7".to_owned())].into(),
        };
        let event = Event {
            request: Some(request),
            ..Event::default()
        };

        let request = scrub_event(event)
            .request
            .expect("the request itself is kept");

        assert_eq!(request.data, None);
        assert_eq!(request.cookies, None);
        assert_eq!(request.query_string, None);
        assert!(request.headers.is_empty());
        assert!(request.env.is_empty());
        assert_eq!(request.method.as_deref(), Some("POST"));
    }
}
