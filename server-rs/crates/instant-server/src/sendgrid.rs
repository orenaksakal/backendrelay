use instant_http::{ApiError, ApiErrorType};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::Instrument as _;

use crate::PostmarkEmail;

const SENDGRID_EMAIL_ENDPOINT: &str = "https://api.sendgrid.com/v3/mail/send";

/// `SendGrid` v3 mail adapter. A missing token preserves the JVM self-hosted
/// disabled-delivery behavior and succeeds without network I/O.
#[derive(Clone)]
pub struct SendGridEmailClient {
    http: reqwest::Client,
    endpoint: String,
    token: Option<String>,
}

impl SendGridEmailClient {
    #[must_use]
    pub fn new(token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: SENDGRID_EMAIL_ENDPOINT.to_owned(),
            token: non_blank(token),
        }
    }

    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Sends the same structured message accepted by the Postmark adapter.
    ///
    /// # Errors
    ///
    /// Returns a transport failure or a `SendGrid` provider rejection.
    pub async fn send(&self, email: &PostmarkEmail) -> Result<(), SendGridError> {
        async {
            let Some(token) = &self.token else {
                tracing::info!("SendGrid delivery is disabled");
                return Ok(());
            };
            let response = self
                .http
                .post(&self.endpoint)
                .bearer_auth(token)
                .json(&SendGridRequest::from(email))
                .send()
                .await
                .map_err(|error| SendGridError::Transport(error.to_string()))?;
            let status = response.status();
            if status.is_success() {
                return Ok(());
            }
            let body = response.bytes().await.unwrap_or_default();
            let message = serde_json::from_slice::<SendGridErrors>(&body)
                .ok()
                .and_then(|errors| errors.errors.into_iter().next())
                .map(|error| error.message);
            Err(SendGridError::Provider {
                status: status.as_u16(),
                message,
            })
        }
        .instrument(sendgrid_send_span(self.token.is_some()))
        .await
    }
}

fn sendgrid_send_span(enabled: bool) -> tracing::Span {
    if enabled {
        tracing::info_span!("sendgrid/send", delivery_enabled = true)
    } else {
        tracing::info_span!("sendgrid/send-disabled", delivery_enabled = false)
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SendGridError {
    #[error("SendGrid provider error {status}: {message:?}")]
    Provider {
        status: u16,
        message: Option<String>,
    },
    #[error("SendGrid transport error: {0}")]
    Transport(String),
}

impl SendGridError {
    #[must_use]
    pub fn into_api_error(self, recipient: &str) -> ApiError {
        ApiError::new(
            ApiErrorType::EmailSendFailed,
            "We weren't able to send the email.",
        )
        .with_hint(json!({"recipient": recipient}))
    }
}

#[derive(Serialize)]
struct SendGridRequest<'a> {
    personalizations: [SendGridPersonalization<'a>; 1],
    from: SendGridAddress<'a>,
    reply_to: SendGridAddress<'a>,
    subject: &'a str,
    content: [SendGridContent<'a>; 1],
}

impl<'a> From<&'a PostmarkEmail> for SendGridRequest<'a> {
    fn from(email: &'a PostmarkEmail) -> Self {
        Self {
            personalizations: [SendGridPersonalization {
                to: [SendGridAddress {
                    email: &email.to,
                    name: None,
                }],
            }],
            from: formatted_address(&email.from),
            reply_to: SendGridAddress {
                email: &email.reply_to,
                name: None,
            },
            subject: &email.subject,
            content: [SendGridContent {
                content_type: "text/html",
                value: &email.html_body,
            }],
        }
    }
}

#[derive(Serialize)]
struct SendGridPersonalization<'a> {
    to: [SendGridAddress<'a>; 1],
}

#[derive(Serialize)]
struct SendGridAddress<'a> {
    email: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

#[derive(Serialize)]
struct SendGridContent<'a> {
    #[serde(rename = "type")]
    content_type: &'static str,
    value: &'a str,
}

#[derive(Deserialize)]
struct SendGridErrors {
    #[serde(default)]
    errors: Vec<SendGridErrorMessage>,
}

#[derive(Deserialize)]
struct SendGridErrorMessage {
    message: String,
}

fn formatted_address(value: &str) -> SendGridAddress<'_> {
    value
        .strip_suffix('>')
        .and_then(|value| value.rsplit_once(" <"))
        .map_or(
            SendGridAddress {
                email: value,
                name: None,
            },
            |(name, email)| SendGridAddress {
                email,
                name: Some(name),
            },
        )
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.and_then(|value| (!value.trim().is_empty()).then_some(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmailSender;

    #[test]
    fn provider_spans_match_the_jvm_names_without_email_or_secret_fields() {
        tracing::subscriber::with_default(tracing_subscriber::registry(), || {
            for (span, expected_name) in [
                (sendgrid_send_span(true), "sendgrid/send"),
                (sendgrid_send_span(false), "sendgrid/send-disabled"),
            ] {
                let metadata = span.metadata().expect("registry enables info spans");
                assert_eq!(metadata.name(), expected_name);
                assert_eq!(
                    metadata
                        .fields()
                        .iter()
                        .map(|field| field.name())
                        .collect::<Vec<_>>(),
                    vec!["delivery_enabled"]
                );
            }
        });
    }

    #[test]
    fn payload_matches_the_jvm_sendgrid_v3_contract() {
        let email = PostmarkEmail::html(
            &EmailSender {
                name: Some("Instant".to_owned()),
                email: "verify@example.com".to_owned(),
            },
            "user@example.com",
            "Verification",
            "<p>123456</p>",
        );
        assert_eq!(
            serde_json::to_value(SendGridRequest::from(&email)).unwrap(),
            json!({
                "personalizations": [{"to": [{"email": "user@example.com"}]}],
                "from": {"email": "verify@example.com", "name": "Instant"},
                "reply_to": {"email": "verify@example.com"},
                "subject": "Verification",
                "content": [{"type": "text/html", "value": "<p>123456</p>"}]
            })
        );
    }

    #[tokio::test]
    async fn a_missing_token_disables_delivery_without_network_io() {
        let client = SendGridEmailClient::new(None).with_endpoint("http://127.0.0.1:1");
        let email = PostmarkEmail::html(
            &EmailSender {
                name: None,
                email: "verify@example.com".to_owned(),
            },
            "user@example.com",
            "Subject",
            "Body",
        );
        client.send(&email).await.unwrap();
    }
}
