use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::PostmarkEmail;

/// Cloudflare reports a sender the account has not enabled for sending.
const SENDER_DOMAIN_ERROR_CODES: [&str; 2] =
    ["E_SENDER_NOT_VERIFIED", "E_SENDER_DOMAIN_NOT_AVAILABLE"];

/// Cloudflare Email Sending REST adapter. Credentials are never included in Debug output.
#[derive(Clone)]
pub(crate) struct CloudflareEmailClient {
    http: reqwest::Client,
    endpoint: String,
    token: String,
}

impl CloudflareEmailClient {
    pub(crate) fn new(account: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: format!(
                "https://api.cloudflare.com/client/v4/accounts/{account}/email/sending/send"
            ),
            token: token.to_owned(),
        }
    }

    pub(crate) async fn send(&self, email: &PostmarkEmail) -> Result<(), CloudflareEmailError> {
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .json(&payload(email))
            .send()
            .await
            .map_err(|_| CloudflareEmailError::Transport)?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|_| CloudflareEmailError::InvalidResponse)?;
        if !status.is_success() {
            if is_sender_domain_rejection(&body) {
                return Err(CloudflareEmailError::SenderNotVerified);
            }
            return Err(CloudflareEmailError::Provider(status.as_u16()));
        }
        let result = serde_json::from_slice::<SendResponse>(&body)
            .map_err(|_| CloudflareEmailError::InvalidResponse)?;
        result.accepted(&email.to)
    }
}

fn is_sender_domain_rejection(body: &[u8]) -> bool {
    let Ok(response) = serde_json::from_slice::<ApiErrorEnvelope>(body) else {
        return false;
    };
    response.errors.iter().any(|error| {
        error
            .code
            .as_str()
            .is_some_and(|code| SENDER_DOMAIN_ERROR_CODES.contains(&code))
    })
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    #[serde(default)]
    errors: Vec<ApiErrorEntry>,
}

#[derive(Deserialize)]
struct ApiErrorEntry {
    #[serde(default)]
    code: Value,
}

/// Cloudflare's answer for one sender domain.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EmailDomainStatus {
    pub zone_id: Option<String>,
    pub enabled: bool,
}

/// Checks whether a sender domain is enabled for Cloudflare Email Sending.
/// Implemented through [`CloudflareEmailDomainVerifier`] in production and
/// mocked in tests.
pub trait EmailDomainVerifier: Send + Sync + 'static {
    fn verify_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<EmailDomainStatus, CloudflareEmailError>> + Send + 'a>>;
}

/// Zone-scoped Cloudflare Email Sending domain lookup.
#[derive(Clone)]
pub struct CloudflareEmailDomainVerifier {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl CloudflareEmailDomainVerifier {
    #[must_use]
    pub fn new(token: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: "https://api.cloudflare.com/client/v4".to_owned(),
            token: token.to_owned(),
        }
    }

    #[must_use]
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        let mut base = base.into();
        base.truncate(base.trim_end_matches('/').len());
        self.base = base;
        self
    }

    async fn find_zone(&self, domain: &str) -> Result<Option<String>, CloudflareEmailError> {
        for candidate in zone_candidates(domain) {
            let url = format!("{}/zones?name={candidate}", self.base);
            let zone = self
                .get::<ZoneEnvelope>(&url)
                .await?
                .result
                .into_iter()
                .next();
            if let Some(zone) = zone {
                return Ok(Some(zone.id));
            }
        }
        Ok(None)
    }

    async fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T, CloudflareEmailError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| CloudflareEmailError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(CloudflareEmailError::Provider(status.as_u16()));
        }
        let envelope = response
            .json::<T>()
            .await
            .map_err(|_| CloudflareEmailError::InvalidResponse)?;
        Ok(envelope)
    }
}

impl EmailDomainVerifier for CloudflareEmailDomainVerifier {
    fn verify_domain<'a>(
        &'a self,
        domain: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<EmailDomainStatus, CloudflareEmailError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(zone_id) = self.find_zone(domain).await? else {
                return Ok(EmailDomainStatus::default());
            };
            let url = format!(
                "{}/zones/{zone_id}/email/sending/subdomains?per_page=50",
                self.base
            );
            let subdomains = self.get::<SubdomainEnvelope>(&url).await?;
            let enabled = subdomains
                .result
                .iter()
                .any(|subdomain| subdomain.name.eq_ignore_ascii_case(domain) && subdomain.enabled);
            Ok(EmailDomainStatus {
                zone_id: Some(zone_id),
                enabled,
            })
        })
    }
}

/// Cloudflare zones are registered domains, so try the full sender domain and
/// then each parent until the lookup finds the owning zone.
fn zone_candidates(domain: &str) -> Vec<String> {
    let labels = domain
        .trim()
        .trim_matches('.')
        .split('.')
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();
    (0..labels.len().saturating_sub(1))
        .map(|start| labels[start..].join("."))
        .collect()
}

#[derive(Deserialize)]
struct ZoneEnvelope {
    result: Vec<ZoneEntry>,
}

#[derive(Deserialize)]
struct ZoneEntry {
    id: String,
}

#[derive(Deserialize)]
struct SubdomainEnvelope {
    #[serde(default)]
    result: Vec<SubdomainEntry>,
}

#[derive(Deserialize)]
struct SubdomainEntry {
    name: String,
    #[serde(default)]
    enabled: bool,
}

/// Cloudflare failures deliberately omit response bodies, recipients, and tokens.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum CloudflareEmailError {
    #[error("Cloudflare email is not configured")]
    NotConfigured,
    #[error("Cloudflare email transport failed")]
    Transport,
    #[error("Cloudflare email request failed with HTTP {0}")]
    Provider(u16),
    #[error("Cloudflare email returned an invalid response")]
    InvalidResponse,
    #[error("Cloudflare did not accept the email recipient")]
    NotAccepted,
    #[error("Cloudflare has not verified the sender domain")]
    SenderNotVerified,
}

fn address(value: &str) -> Value {
    value
        .strip_suffix('>')
        .and_then(|v| v.rsplit_once(" <"))
        .map_or_else(
            || json!(value),
            |(name, address)| json!({"name": name, "address": address}),
        )
}

fn payload(email: &PostmarkEmail) -> Value {
    let mut body = json!({
        "from": address(&email.from),
        "to": email.to,
        "reply_to": address(&email.reply_to),
        "subject": email.subject,
        "html": email.html_body,
    });
    if let Some(text) = &email.text_body {
        body["text"] = json!(text);
    }
    body
}

#[derive(Deserialize)]
struct SendResponse {
    success: bool,
    result: Option<DeliveryResult>,
}

#[derive(Deserialize)]
struct DeliveryResult {
    delivered: Vec<String>,
    queued: Vec<String>,
    permanent_bounces: Vec<String>,
}

impl SendResponse {
    fn accepted(self, recipient: &str) -> Result<(), CloudflareEmailError> {
        let Some(result) = self.result.filter(|_| self.success) else {
            return Err(CloudflareEmailError::NotAccepted);
        };
        let matches = |address: &String| address.eq_ignore_ascii_case(recipient);
        if result.permanent_bounces.iter().any(matches)
            || !result.delivered.iter().chain(&result.queued).any(matches)
        {
            return Err(CloudflareEmailError::NotAccepted);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmailSender;

    #[test]
    fn rest_payload_preserves_app_name_default_sender_and_both_bodies() {
        let mut email = PostmarkEmail::html(
            &EmailSender {
                name: Some("My App".into()),
                email: "transactional@example.com".into(),
            },
            "user@example.com",
            "Login",
            "<p>123456</p>",
        );
        email.text_body = Some("123456".into());
        assert_eq!(
            payload(&email),
            json!({
                "from": {"name":"My App", "address":"transactional@example.com"},
                "to":"user@example.com", "reply_to":"transactional@example.com",
                "subject":"Login", "html":"<p>123456</p>", "text":"123456"
            })
        );
    }

    #[test]
    fn only_delivery_or_queue_acceptance_for_the_recipient_succeeds() {
        for (success, delivered, queued, bounced, expected) in [
            (true, vec!["user@example.com"], vec![], vec![], true),
            (true, vec![], vec!["user@example.com"], vec![], true),
            (true, vec![], vec![], vec!["user@example.com"], false),
            (true, vec!["other@example.com"], vec![], vec![], false),
            (true, vec![], vec![], vec![], false),
            (false, vec!["user@example.com"], vec![], vec![], false),
        ] {
            let response: SendResponse = serde_json::from_value(json!({
                "success":success, "result": {"delivered":delivered,
                "queued":queued, "permanent_bounces":bounced}
            }))
            .unwrap();
            assert_eq!(response.accepted("user@example.com").is_ok(), expected);
        }
    }

    #[tokio::test]
    async fn http_request_uses_bearer_auth_and_rejects_provider_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body, expected) in [
            (
                "200 OK",
                r#"{"success":true,"result":{"delivered":[],"queued":["user@example.com"],"permanent_bounces":[]}}"#,
                true,
            ),
            ("200 OK", r#"{"success":false,"result":null}"#, false),
            ("401 Unauthorized", "{}", false),
            ("200 OK", "{}", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/send", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .unwrap()
                            .parse()
                            .unwrap();
                        if bytes.len() >= end + 4 + length {
                            assert!(headers.contains("authorization: bearer test-token"));
                            let request: Value = serde_json::from_slice(&bytes[end + 4..]).unwrap();
                            assert_eq!(request["from"], "transactional@example.com");
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let mut client = CloudflareEmailClient::new("account", "test-token");
            client.endpoint = endpoint;
            let email = PostmarkEmail::html(
                &EmailSender {
                    name: None,
                    email: "transactional@example.com".into(),
                },
                "user@example.com",
                "Login",
                "123456",
            );
            assert_eq!(client.send(&email).await.is_ok(), expected);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn unverified_sender_rejections_are_distinct_from_other_provider_failures() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/send", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 4096];
            let _ = socket.read(&mut buffer).await.unwrap();
            let body =
                r#"{"success":false,"errors":[{"code":"E_SENDER_NOT_VERIFIED","message":"no"}]}"#;
            let response = format!(
                "HTTP/1.1 422 Unprocessable Entity\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let mut client = CloudflareEmailClient::new("account", "test-token");
        client.endpoint = endpoint;
        let email = PostmarkEmail::html(
            &EmailSender {
                name: None,
                email: "custom@example.com".into(),
            },
            "user@example.com",
            "Login",
            "123456",
        );
        assert_eq!(
            client.send(&email).await,
            Err(CloudflareEmailError::SenderNotVerified)
        );
        server.await.unwrap();
        assert!(is_sender_domain_rejection(
            br#"{"errors":[{"code":"E_SENDER_DOMAIN_NOT_AVAILABLE"}]}"#
        ));
        assert!(!is_sender_domain_rejection(
            br#"{"errors":[{"code":"E_OTHER"}]}"#
        ));
        assert!(!is_sender_domain_rejection(b"not json"));
    }

    #[test]
    fn zone_candidates_include_each_parent_domain() {
        assert_eq!(
            zone_candidates("Mail.Example.COM"),
            vec!["Mail.Example.COM", "Example.COM"]
        );
        assert_eq!(zone_candidates("example.com"), vec!["example.com"]);
        assert!(zone_candidates("localhost").is_empty());
    }

    #[tokio::test]
    async fn verifier_finds_the_zone_and_reads_the_enabled_subdomain() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (path, body) in [
                (
                    "/client/v4/zones?name=mail.example.com",
                    r#"{"success":true,"result":[]}"#,
                ),
                (
                    "/client/v4/zones?name=example.com",
                    r#"{"success":true,"result":[{"id":"zone-1"}]}"#,
                ),
                (
                    "/client/v4/zones/zone-1/email/sending/subdomains?per_page=50",
                    r#"{"success":true,"result":[{"name":"mail.example.com","enabled":true}]}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();
                assert!(request.starts_with(&format!("get {path} ")), "{request}");
                assert!(request.contains("authorization: bearer test-token"));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let verifier = CloudflareEmailDomainVerifier::new("test-token")
            .with_base_url(format!("{base}/client/v4"));
        assert_eq!(
            verifier.verify_domain("mail.example.com").await.unwrap(),
            EmailDomainStatus {
                zone_id: Some("zone-1".to_owned()),
                enabled: true,
            }
        );
        server.await.unwrap();
    }
}
