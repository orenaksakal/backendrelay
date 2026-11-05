use std::time::Duration;

use instant_http::{ApiError, ApiErrorType};
use serde_json::json;

use crate::PostmarkEmail;

/// Optional Resend delivery. Credentials and message contents are never logged.
#[derive(Clone)]
pub struct ResendEmailClient {
    http: Option<reqwest::Client>,
    endpoint: String,
    token: Option<String>,
}

impl ResendEmailClient {
    #[must_use]
    pub fn new(token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .ok(),
            endpoint: "https://api.resend.com/emails".to_owned(),
            token: token.filter(|value| !value.trim().is_empty()),
        }
    }

    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// # Errors
    /// Returns a transport error or provider rejection; never retries a send.
    pub async fn send(&self, email: &PostmarkEmail) -> Result<(), ResendError> {
        let Some(token) = &self.token else {
            return Ok(());
        };
        let mut body = json!({
            "from": email.from, "to": [&email.to], "subject": email.subject,
            "html": email.html_body, "reply_to": email.reply_to,
        });
        if let Some(text) = &email.text_body {
            body["text"] = json!(text);
        }
        let response = self
            .http
            .as_ref()
            .ok_or(ResendError::Transport)?
            .post(&self.endpoint)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|_| ResendError::Transport)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(ResendError::Provider(response.status().as_u16()))
        }
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ResendError {
    #[error("Resend transport failed")]
    Transport,
    #[error("Resend provider rejected email (HTTP {0})")]
    Provider(u16),
}

impl ResendError {
    pub(crate) fn into_api_error(self, recipient: &str) -> ApiError {
        ApiError::new(
            ApiErrorType::EmailSendFailed,
            "We weren't able to send the email.",
        )
        .with_hint(json!({"recipient": recipient}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmailSender;
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn resend_delivery_contract_success_rejection_and_redirect() {
        for status in [
            StatusCode::OK,
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::TEMPORARY_REDIRECT,
        ] {
            let captured = Arc::new(Mutex::new(None));
            let received = captured.clone();
            let app = Router::new().route(
                "/emails",
                post(
                    move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                        let received = received.clone();
                        async move {
                            *received.lock().unwrap() = Some((headers, body));
                            (
                                status,
                                [("location", "http://127.0.0.1:1/never-follow")],
                                "provider response",
                            )
                        }
                    },
                ),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let client = ResendEmailClient::new(Some("test-token".into()))
                .with_endpoint(format!("http://{address}/emails"));
            let email = PostmarkEmail::html(
                &EmailSender {
                    name: Some("App".into()),
                    email: "verify@example.test".into(),
                },
                "user@example.test",
                "Subject",
                "<p>code</p>",
            );
            let result = client.send(&email).await;
            if status.is_success() {
                assert_eq!(result, Ok(()));
            } else {
                assert_eq!(result, Err(ResendError::Provider(status.as_u16())));
            }
            let (headers, body) = captured.lock().unwrap().take().unwrap();
            assert_eq!(headers["authorization"], "Bearer test-token");
            assert_eq!(
                body,
                json!({"from":"App <verify@example.test>","to":["user@example.test"],"subject":"Subject","html":"<p>code</p>","reply_to":"verify@example.test"})
            );
            server.abort();
        }
    }
}
