//! Local structured logging, optional loopback benchmark counters and optional
//! Sentry-protocol error reporting.
//!
//! No trace exporter or background collector is installed. When `SENTRY_DSN` is
//! configured, [`crate::init_error_reporting`] initializes the client and this
//! module adds the `sentry-tracing` layer that forwards `error!` events and
//! `warn!` breadcrumbs.
use std::sync::{Once, OnceLock};

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::http::header::HeaderName;
use axum::middleware::Next;
use axum::response::Response;
use thiserror::Error;
use tracing::Instrument as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Registry, reload};
use uuid::Uuid;

use crate::ServerConfig;
use crate::version::{GIT_SHA, VERSION};

static MACHINE_ID: OnceLock<String> = OnceLock::new();
static LOG_FILTER: OnceLock<LogFilterControl> = OnceLock::new();

/// Replaces the process log filter in place, so changing verbosity does not
/// restart the process (which would drop every live socket).
#[derive(Clone)]
pub(crate) struct LogFilterControl {
    handle: reload::Handle<EnvFilter, Registry>,
}

impl LogFilterControl {
    pub(crate) fn install(filter: EnvFilter) -> (reload::Layer<EnvFilter, Registry>, Self) {
        let (layer, handle) = reload::Layer::new(filter);
        (layer, Self { handle })
    }

    pub(crate) fn current(&self) -> String {
        self.handle
            .with_current(ToString::to_string)
            .unwrap_or_else(|_| DEFAULT_LOG_FILTER.to_owned())
    }

    pub(crate) fn update(&self, directive: &str) -> Result<String, String> {
        let filter = EnvFilter::try_new(directive).map_err(|error| error.to_string())?;
        let rendered = filter.to_string();
        self.handle
            .reload(filter)
            .map_err(|error| error.to_string())?;
        Ok(rendered)
    }
}

pub(crate) fn log_filter_control() -> Option<LogFilterControl> {
    LOG_FILTER.get().cloned()
}

fn machine_id() -> &'static str {
    MACHINE_ID.get().map_or("unknown", String::as_str)
}

#[derive(Debug, Error)]
pub enum TelemetryError {
    #[error("could not install local process logging: {0}")]
    Subscriber(String),
}

/// Used when `RUST_LOG` is unset. `pg_walstream` logs every standby status
/// update at info (one per stream per second, and thousands per second if
/// PostgreSQL ever spins on reply requests), so it is held at warn.
const DEFAULT_LOG_FILTER: &str = "info,pg_walstream=warn";

/// Installs local JSON logging. Legacy exporter/DSN variables have no effect.
///
/// # Errors
///
/// Returns an error when a process subscriber is already installed.
pub fn init_telemetry(_config: &ServerConfig, machine_id: Uuid) -> Result<(), TelemetryError> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    let (filter, control) = LogFilterControl::install(filter);
    let sentry_layer = sentry::Hub::current().client().map(|_| {
        sentry_tracing::layer().event_filter(|metadata| match *metadata.level() {
            tracing::Level::ERROR => sentry_tracing::EventFilter::Event,
            tracing::Level::WARN => sentry_tracing::EventFilter::Breadcrumb,
            _ => sentry_tracing::EventFilter::Ignore,
        })
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false),
        )
        .with(sentry_layer)
        .try_init()
        .map_err(|error| TelemetryError::Subscriber(error.to_string()))?;
    let _ = LOG_FILTER.set(control);
    let _ = MACHINE_ID.set(machine_id.to_string());
    tracing::info!(
        machine_id = %machine_id,
        version = VERSION,
        git_sha = GIT_SHA,
        "local telemetry initialized"
    );
    Ok(())
}

static PANIC_HOOK: Once = Once::new();

/// Installs a process panic hook that logs one structured event, increments
/// `backendrelay_panics_total`, and then defers to the previously installed
/// hook. The hook never aborts or exits on its own.
pub fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            crate::metrics::record_panic();
            let location = info.location().map_or_else(
                || "<unknown>".to_owned(),
                |location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                },
            );
            let thread = std::thread::current();
            let thread = thread.name().unwrap_or("<unnamed>");
            let message = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string payload>");
            let backtrace = std::env::var_os("RUST_BACKTRACE")
                .is_some_and(|value| value != "0")
                .then(|| std::backtrace::Backtrace::force_capture().to_string());
            tracing::error!(
                panic.location = %location,
                panic.thread = %thread,
                panic.message = %message,
                panic.backtrace = backtrace.as_deref(),
                "process panic"
            );
            previous(info);
        }));
    });
}

/// Counts response-header latency only when private native metrics are enabled.
/// No request-scoped hub, trace context, request copy or network export is created.
///
/// Accepts or mints `x-request-id`, echoes it on the response, and runs the
/// request inside a span and task-local scope so error bodies can report it.
///
/// # Panics
///
/// Panics only if [`instant_http::request_id::resolve_request_id`] returns a
/// value outside its own header-safe allowlist, which cannot happen.
pub async fn trace_http_request(request: Request, next: Next) -> Response {
    let started = crate::metrics::enabled().then(std::time::Instant::now);
    let group = crate::metrics::request_group(request.uri().path());
    let request_id = instant_http::request_id::resolve_request_id(
        request
            .headers()
            .get(instant_http::request_id::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
    );
    let header = HeaderValue::from_str(&request_id)
        .expect("resolved request ids only contain header-safe ASCII");
    let method = request.method().clone();
    let span = tracing::info_span!(
        "request",
        request_id = %request_id,
        method = %method,
        route = crate::metrics::request_group_name(group),
        machine_id = machine_id(),
        version = VERSION,
    );
    let mut response = instant_http::request_id::REQUEST_ID
        .scope(request_id, next.run(request).instrument(span))
        .await;
    response.headers_mut().insert(
        HeaderName::from_static(instant_http::request_id::REQUEST_ID_HEADER),
        header,
    );
    if let Some(started) = started {
        crate::metrics::record_request(group, response.status().as_u16(), started.elapsed());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt as _;

    #[test]
    fn default_log_filter_parses_and_quiets_the_replication_client() {
        let filter = tracing_subscriber::EnvFilter::try_new(DEFAULT_LOG_FILTER).unwrap();
        assert!(filter.to_string().contains("pg_walstream=warn"));
    }

    #[test]
    fn panic_hook_increments_the_process_counter_without_aborting() {
        install_panic_hook();
        let before = crate::metrics::panics();
        std::thread::spawn(|| {
            let panicked = std::panic::catch_unwind(|| panic!("panic hook counter test"));
            assert!(panicked.is_err());
        })
        .join()
        .unwrap();
        assert!(crate::metrics::panics() > before);
    }

    #[tokio::test]
    async fn local_counter_middleware_preserves_success_and_error_responses() {
        let router = Router::new()
            .route("/ok", get(|| async { ([("x-test", "preserved")], "ok") }))
            .route(
                "/failed",
                get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "unavailable") }),
            )
            .layer(axum::middleware::from_fn(trace_http_request));
        for (path, expected_status, expected_body) in [
            ("/ok", StatusCode::OK, "ok"),
            ("/failed", StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("authorization", "Bearer private-test-credential")
                        .header(
                            "traceparent",
                            "00-11111111111111111111111111111111-1111111111111111-01",
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status);
            if path == "/ok" {
                assert_eq!(response.headers()["x-test"], "preserved");
            }
            assert!(!response.headers().contains_key("traceparent"));
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(body.as_ref(), expected_body.as_bytes());
        }
    }

    fn request_router() -> Router {
        Router::new()
            .route("/ok", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(trace_http_request))
    }

    async fn response_request_id(header: Option<&str>) -> String {
        let mut builder = Request::builder().uri("/ok");
        if let Some(header) = header {
            builder = builder.header("x-request-id", header);
        }
        let response = request_router()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn middleware_generates_a_uuid_v7_request_id() {
        let request_id = response_request_id(None).await;
        assert_eq!(Uuid::parse_str(&request_id).unwrap().get_version_num(), 7);
    }

    #[tokio::test]
    async fn middleware_preserves_a_valid_incoming_request_id() {
        assert_eq!(
            response_request_id(Some("client-request_1:2.3")).await,
            "client-request_1:2.3"
        );
    }

    #[tokio::test]
    async fn middleware_replaces_an_invalid_incoming_request_id() {
        for invalid in ["has space".to_owned(), "a".repeat(200)] {
            let request_id = response_request_id(Some(&invalid)).await;
            assert_ne!(request_id, invalid);
            assert_eq!(Uuid::parse_str(&request_id).unwrap().get_version_num(), 7);
        }
    }

    #[tokio::test]
    async fn api_errors_report_the_request_id_from_the_response_header() {
        async fn failed() -> instant_http::ApiError {
            instant_http::ApiError::new(instant_http::ApiErrorType::Unknown, "broken")
        }
        let router = Router::new()
            .route("/failed", get(failed))
            .layer(axum::middleware::from_fn(trace_http_request));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/failed")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["trace-id"], request_id);
    }
}
