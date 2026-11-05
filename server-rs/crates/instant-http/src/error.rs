use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable public error discriminator used by the JVM HTTP middleware.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApiErrorType {
    RecordNotFound,
    RecordExpired,
    RecordNotUnique,
    RecordForeignKeyInvalid,
    RecordCheckViolation,
    SqlRaise,
    SqlException,
    Timeout,
    RateLimited,
    PermissionDenied,
    PermissionEvaluationFailed,
    ParameterLimitExceeded,
    AppReadOnly,
    AppDisabled,
    AppCreationDisabled,
    SignupsClosed,
    BillingClosed,
    ParamMissing,
    ParamMalformed,
    ValidationFailed,
    EmailSendFailed,
    SessionMissing,
    MemberMissing,
    ConnectionClosed,
    OauthError,
    OperationTimedOut,
    Unknown,
}

/// JSON body returned for errors other than OAuth-provider callbacks.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApiErrorBody {
    #[serde(rename = "type")]
    pub kind: ApiErrorType,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<Value>,
    #[serde(rename = "trace-id", skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// HTTP-ready application failure preserving Instant's status mapping.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    body: ApiErrorBody,
    unauthorized: bool,
    status: Option<StatusCode>,
}

impl ApiError {
    #[must_use]
    pub fn new(kind: ApiErrorType, message: impl Into<String>) -> Self {
        Self {
            body: ApiErrorBody {
                kind,
                message: message.into(),
                hint: None,
                trace_id: None,
            },
            unauthorized: false,
            status: None,
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: Value) -> Self {
        self.body.hint = Some(hint);
        self
    }

    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.body.trace_id = Some(trace_id.into());
        self
    }

    #[must_use]
    pub const fn unauthorized(mut self) -> Self {
        self.unauthorized = true;
        self
    }

    #[must_use]
    pub const fn with_status(mut self, status: StatusCode) -> Self {
        self.status = Some(status);
        self
    }

    #[must_use]
    pub const fn status(&self) -> StatusCode {
        if let Some(status) = self.status {
            return status;
        }
        if self.unauthorized {
            return StatusCode::UNAUTHORIZED;
        }
        match self.body.kind {
            ApiErrorType::Timeout | ApiErrorType::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            ApiErrorType::EmailSendFailed
            | ApiErrorType::OperationTimedOut
            | ApiErrorType::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    #[must_use]
    pub const fn body(&self) -> &ApiErrorBody {
        &self.body
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.body.kind == ApiErrorType::OauthError {
            let body = serde_json::json!({
                "type": "oauth-error",
                "error": self.body.message,
            });
            return (self.status(), Json(body)).into_response();
        }
        let status = self.status();
        let mut body = self.body;
        if body.trace_id.is_none() {
            body.trace_id = crate::request_id::current_request_id();
        }
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_matches_wrap_errors() {
        assert_eq!(
            ApiError::new(ApiErrorType::ValidationFailed, "bad").status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::new(ApiErrorType::PermissionDenied, "no")
                .unauthorized()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::new(ApiErrorType::Timeout, "slow").status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ApiError::new(ApiErrorType::Unknown, "broken").status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::new(ApiErrorType::SqlException, "database rejected statement").status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::new(ApiErrorType::EmailSendFailed, "provider rejected email").status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            ApiError::new(ApiErrorType::OperationTimedOut, "receive timed out").status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn body_uses_kebab_case_wire_keys() {
        let body = ApiError::new(ApiErrorType::RecordNotFound, "missing")
            .with_hint(serde_json::json!({"record-type": "app"}))
            .with_trace_id("trace")
            .body;
        assert_eq!(
            serde_json::to_value(body).unwrap(),
            serde_json::json!({
                "type": "record-not-found",
                "message": "missing",
                "hint": {"record-type": "app"},
                "trace-id": "trace"
            })
        );
    }

    #[test]
    fn sql_exception_uses_the_jvm_wire_discriminator() {
        assert_eq!(
            serde_json::to_value(ApiErrorType::SqlException).unwrap(),
            serde_json::json!("sql-exception")
        );
    }

    #[tokio::test]
    async fn trace_id_stays_unset_outside_a_request_scope() {
        let response = ApiError::new(ApiErrorType::ValidationFailed, "bad").into_response();
        assert_eq!(crate::request_id::current_request_id(), None);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(body.get("trace-id").is_none());
    }
}
