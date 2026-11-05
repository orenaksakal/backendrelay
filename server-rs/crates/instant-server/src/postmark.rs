use std::sync::Arc;

use chrono::{DateTime, Utc};
use instant_http::{ApiError, ApiErrorType};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_postgres::Client;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    DashboardControlHookOperation, DashboardControlHooks, DashboardMagicCodeDelivery,
    EmailDeliveryClient, MagicCodeDelivery, PostgresPool, ServerEmailConfig,
};

const POSTMARK_EMAIL_ENDPOINT: &str = "https://api.postmarkapp.com/email";
const POSTMARK_SENDERS_ENDPOINT: &str = "https://api.postmarkapp.com/senders";
const POSTMARK_USER_NOTE: &str =
    "Instant partners with Postmark to send emails.  Please verify your custom sender address.";
/// A Cloudflare sender domain is usable for this long after its last successful
/// check; the daily worker refreshes it before it goes stale.
const SENDER_DOMAIN_VERIFICATION_HOURS: i64 = 25;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmailSender {
    pub name: Option<String>,
    pub email: String,
}

impl EmailSender {
    #[must_use]
    pub fn formatted(&self) -> String {
        self.name
            .as_deref()
            .map(sanitized_sender_name)
            .filter(|name| !name.is_empty())
            .map_or_else(
                || self.email.clone(),
                |name| format!("{name} <{}>", self.email),
            )
    }
}

/// A display name is a header value, never markup: drop line breaks, angle
/// brackets and quotes, then bound its length.
fn sanitized_sender_name(name: &str) -> String {
    name.chars()
        .filter(|character| !matches!(character, '\r' | '\n' | '<' | '>' | '"'))
        .collect::<String>()
        .trim()
        .chars()
        .take(64)
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PostmarkEmail {
    pub from: String,
    pub to: String,
    pub reply_to: String,
    pub subject: String,
    pub message_stream: &'static str,
    #[serde(rename = "HTMLBody")]
    pub html_body: String,
    #[serde(rename = "TextBody", skip_serializing_if = "Option::is_none")]
    pub text_body: Option<String>,
}

impl PostmarkEmail {
    #[must_use]
    pub fn html(
        sender: &EmailSender,
        to: impl Into<String>,
        subject: impl Into<String>,
        html_body: impl Into<String>,
    ) -> Self {
        Self {
            from: sender.formatted(),
            to: to.into(),
            reply_to: sender.email.clone(),
            subject: subject.into(),
            message_stream: "outbound",
            html_body: html_body.into(),
            text_body: None,
        }
    }
}

/// Minimal Postmark server API adapter. A missing token preserves the JVM's
/// self-hosted disabled-delivery behavior and succeeds without network I/O.
#[derive(Clone)]
pub struct PostmarkEmailClient {
    http: reqwest::Client,
    endpoint: String,
    server_token: Option<String>,
}

impl PostmarkEmailClient {
    #[must_use]
    pub fn new(server_token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: POSTMARK_EMAIL_ENDPOINT.to_owned(),
            server_token: non_blank(server_token),
        }
    }

    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Sends one message or returns the provider's typed failure.
    ///
    /// # Errors
    ///
    /// Returns a transport failure, a Postmark provider rejection, or the
    /// distinct sender-signature rejection used for default-sender fallback.
    pub async fn send(&self, email: &PostmarkEmail) -> Result<(), PostmarkError> {
        async {
            let Some(server_token) = &self.server_token else {
                tracing::info!("Postmark delivery is disabled");
                return Ok(());
            };
            let response = self
                .http
                .post(&self.endpoint)
                .header("X-Postmark-Server-Token", server_token)
                .json(email)
                .send()
                .await
                .map_err(|error| PostmarkError::Transport(error.to_string()))?;
            let status = response.status();
            let bytes = response
                .bytes()
                .await
                .map_err(|error| PostmarkError::Transport(error.to_string()))?;
            let result = serde_json::from_slice::<PostmarkResponse>(&bytes).unwrap_or_default();
            if status.is_success() && result.error_code == 0 {
                return Ok(());
            }
            match result.error_code {
                400 | 401 => Err(PostmarkError::SenderSignature {
                    code: result.error_code,
                    message: result.message,
                }),
                code => Err(PostmarkError::Provider {
                    code,
                    message: result.message,
                }),
            }
        }
        .instrument(postmark_send_span(self.server_token.is_some()))
        .await
    }
}

fn postmark_send_span(enabled: bool) -> tracing::Span {
    if enabled {
        tracing::info_span!("postmark/send", delivery_enabled = true)
    } else {
        tracing::info_span!("postmark/send-disabled", delivery_enabled = false)
    }
}

/// Postmark account API adapter for sender signature administration.
#[derive(Clone)]
pub struct PostmarkAccountClient {
    http: reqwest::Client,
    endpoint: String,
    account_token: Option<String>,
}

impl PostmarkAccountClient {
    #[must_use]
    pub fn new(account_token: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            endpoint: POSTMARK_SENDERS_ENDPOINT.to_owned(),
            account_token: non_blank(account_token),
        }
    }

    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self.endpoint
            .truncate(self.endpoint.trim_end_matches('/').len());
        self
    }

    const fn is_configured(&self) -> bool {
        self.account_token.is_some()
    }

    /// Creates or renames the sender matching `email` and returns its Postmark ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the account token is absent or Postmark rejects an
    /// account request.
    pub async fn sync_sender(&self, email: &str, name: &str) -> Result<i32, ApiError> {
        if let Some(sender) = self.find_sender(email).await? {
            let id = sender_id(&sender)?;
            self.request(
                reqwest::Method::PUT,
                &format!("{}/{id}", self.endpoint),
                Some(json!({
                    "Name": name,
                    "ConfirmationPersonalNote": POSTMARK_USER_NOTE,
                })),
            )
            .await?;
            return Ok(id);
        }
        let response = self
            .request(
                reqwest::Method::POST,
                &self.endpoint,
                Some(json!({
                    "FromEmail": email,
                    "ReplyToEmail": email,
                    "Name": name,
                    "ConfirmationPersonalNote": POSTMARK_USER_NOTE,
                })),
            )
            .await?;
        sender_id(&response)
    }

    /// Loads a sender signature by its Postmark ID.
    ///
    /// # Errors
    ///
    /// Returns an error when account administration is disabled or the request fails.
    pub async fn get_sender(&self, id: i32) -> Result<Value, ApiError> {
        self.request(
            reqwest::Method::GET,
            &format!("{}/{id}", self.endpoint),
            None,
        )
        .await
    }

    async fn find_sender(&self, email: &str) -> Result<Option<Value>, ApiError> {
        let response = self
            .request(
                reqwest::Method::GET,
                &format!("{}?count=500&offset=0", self.endpoint),
                None,
            )
            .await?;
        Ok(response
            .get("SenderSignatures")
            .and_then(Value::as_array)
            .and_then(|senders| {
                senders
                    .iter()
                    .find(|sender| {
                        sender.get("EmailAddress").and_then(Value::as_str) == Some(email)
                    })
                    .cloned()
            }))
    }

    async fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value, ApiError> {
        let token = self.account_token.as_deref().ok_or_else(|| {
            ApiError::new(
                ApiErrorType::Unknown,
                "POSTMARK_ACCOUNT_TOKEN is required for sender administration",
            )
        })?;
        let mut request = self
            .http
            .request(method, url)
            .header("X-Postmark-Account-Token", token)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|_| {
            ApiError::new(
                ApiErrorType::Unknown,
                "Postmark sender administration request failed",
            )
        })?;
        let status = response.status();
        let body = response.json::<Value>().await.map_err(|_| {
            ApiError::new(
                ApiErrorType::Unknown,
                "Postmark sender administration returned malformed JSON",
            )
        })?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(ApiError::new(
                ApiErrorType::Unknown,
                "Postmark sender administration was rejected",
            ))
        }
    }
}

/// Dashboard hook middleware that owns Postmark-specific behavior and delegates
/// every unrelated control-plane operation to `inner`.
pub struct PostmarkDashboardControlHooks<D> {
    inner: D,
    email: EmailDeliveryClient,
    account: PostmarkAccountClient,
    email_domain_verifier: Option<Arc<dyn crate::cloudflare_email::EmailDomainVerifier>>,
    default_sender: Option<EmailSender>,
    team_sender: Option<EmailSender>,
    dashboard_origin: String,
    verification_code_expiry_minutes: i32,
}

/// Production email adapters consumed by the runtime/admin/dashboard services.
pub struct PostmarkAdapters<D> {
    pub magic_code: Arc<PostgresMagicCodeDelivery>,
    pub dashboard_magic_code: PostmarkDashboardMagicCodeDelivery,
    pub dashboard_hooks: PostmarkDashboardControlHooks<D>,
    pub email_domain_verifier: Option<Arc<dyn crate::cloudflare_email::EmailDomainVerifier>>,
}

impl<D> PostmarkAdapters<D> {
    #[must_use]
    pub fn from_config(
        config: &ServerEmailConfig,
        dashboard_origin: &str,
        template_client: Client,
        inner_hooks: D,
    ) -> Self {
        Self::from_config_source(
            config,
            dashboard_origin,
            PostgresClientSource::fixed(template_client),
            inner_hooks,
        )
    }

    pub(crate) fn from_config_source(
        config: &ServerEmailConfig,
        dashboard_origin: &str,
        template_client: PostgresClientSource,
        inner_hooks: D,
    ) -> Self {
        let email = EmailDeliveryClient::from_config(config);
        let account =
            PostmarkAccountClient::new(config.postmark_account_token().map(str::to_owned));
        let (app_sender_name, app_sender_email) = config.app_sender();
        let app_sender = app_sender_email.map(|email| EmailSender {
            name: app_sender_name.map(str::to_owned),
            email: email.to_owned(),
        });
        let (dashboard_sender_name, dashboard_sender_email) = config.dashboard_sender();
        let dashboard_sender = dashboard_sender_email.map(|email| EmailSender {
            name: Some(dashboard_sender_name.to_owned()),
            email: email.to_owned(),
        });
        let (team_sender_name, team_sender_email) = config.team_sender();
        let team_sender = team_sender_email.map(|email| EmailSender {
            name: Some(team_sender_name.to_owned()),
            email: email.to_owned(),
        });
        let email_domain_verifier = (config.provider() == crate::EmailProvider::Cloudflare)
            .then(|| config.cloudflare_email_api_token())
            .flatten()
            .map(|token| {
                Arc::new(crate::cloudflare_email::CloudflareEmailDomainVerifier::new(
                    token,
                )) as Arc<dyn crate::cloudflare_email::EmailDomainVerifier>
            });
        Self {
            magic_code: Arc::new(PostgresMagicCodeDelivery::with_email_client_source(
                template_client,
                email.clone(),
                app_sender,
            )),
            dashboard_magic_code: PostmarkDashboardMagicCodeDelivery::with_email_client(
                email.clone(),
                dashboard_sender.clone(),
            ),
            dashboard_hooks: PostmarkDashboardControlHooks::with_email_client(
                inner_hooks,
                email,
                account,
                dashboard_sender,
                team_sender,
                dashboard_origin.to_owned(),
                1_440,
            )
            .with_email_domain_verifier(email_domain_verifier.clone()),
            email_domain_verifier,
        }
    }
}

impl<T> MagicCodeDelivery for Arc<T>
where
    T: MagicCodeDelivery,
{
    async fn deliver(
        &self,
        app_id: Uuid,
        email: &str,
        code: &str,
        expiry_minutes: u64,
    ) -> Result<(), ApiError> {
        T::deliver(self, app_id, email, code, expiry_minutes).await
    }
}

impl<D> PostmarkDashboardControlHooks<D> {
    #[must_use]
    pub const fn with_email_client(
        inner: D,
        email: EmailDeliveryClient,
        account: PostmarkAccountClient,
        default_sender: Option<EmailSender>,
        team_sender: Option<EmailSender>,
        dashboard_origin: String,
        verification_code_expiry_minutes: i32,
    ) -> Self {
        Self {
            inner,
            email,
            account,
            email_domain_verifier: None,
            default_sender,
            team_sender,
            dashboard_origin,
            verification_code_expiry_minutes,
        }
    }

    /// Adds the Cloudflare sender-domain verifier used by the dashboard flow
    /// and the daily re-check worker.
    #[must_use]
    pub fn with_email_domain_verifier(
        mut self,
        verifier: Option<Arc<dyn crate::cloudflare_email::EmailDomainVerifier>>,
    ) -> Self {
        self.email_domain_verifier = verifier;
        self
    }
}

impl<D> DashboardControlHooks for PostmarkDashboardControlHooks<D>
where
    D: DashboardControlHooks,
{
    #[allow(clippy::too_many_lines)] // Keeps provider-specific operations in wire order.
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::DefaultEmailTemplate => {
                let sender = self
                    .default_sender
                    .as_ref()
                    .ok_or_else(|| missing_sender_email("INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL"))?;
                Ok(json!({
                    "sender-email": sender.email,
                    "subject": "{code} is your verification code for {app_title}",
                    "body": default_magic_code_body("{app_title}", "{code}", "{expiration}"),
                    "verification-code-expiry-minutes": self.verification_code_expiry_minutes,
                }))
            }
            DashboardControlHookOperation::SyncEmailSender { email, name, .. } => {
                if self.email.requires_verified_domain() {
                    // Cloudflare sender domains are registered with the zone,
                    // not created through Postmark's account API.
                    Ok(Value::Null)
                } else {
                    self.account
                        .sync_sender(&email, &name)
                        .await
                        .map(|id| json!(id))
                }
            }
            DashboardControlHookOperation::VerifyEmailDomain { domain, .. } => {
                let Some(verifier) = self.email_domain_verifier.as_ref() else {
                    return Ok(Value::Null);
                };
                Ok(match verifier.verify_domain(&domain).await {
                    Ok(status) => json!({
                        "enabled": status.enabled,
                        "zone-id": status.zone_id,
                        "error": Value::Null,
                    }),
                    Err(error) => json!({
                        "enabled": false,
                        "zone-id": Value::Null,
                        "error": error.to_string(),
                    }),
                })
            }
            DashboardControlHookOperation::GetEmailSender { postmark_id } => {
                if self.account.is_configured() {
                    self.account.get_sender(postmark_id).await
                } else {
                    Ok(Value::Null)
                }
            }
            DashboardControlHookOperation::DashboardAuthDefaultSender { .. } => {
                let sender = self
                    .default_sender
                    .as_ref()
                    .ok_or_else(|| missing_sender_email("INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL"))?;
                Ok(Value::String(sender.email.clone()))
            }
            DashboardControlHookOperation::DeliverDashboardEmail {
                to,
                sender_email,
                sender_name,
                subject,
                body,
                ..
            } => {
                let custom_sender = sender_email.is_some();
                let sender_address = sender_email
                    .or_else(|| {
                        self.default_sender
                            .as_ref()
                            .map(|sender| sender.email.clone())
                    })
                    .ok_or_else(|| missing_sender_email("INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL"))?;
                let sender = EmailSender {
                    name: Some(sender_name),
                    email: sender_address,
                };
                let message = PostmarkEmail::html(&sender, &to, subject, body);
                match self.email.send(&message).await {
                    Ok(()) => Ok(Value::Bool(true)),
                    Err(error) if custom_sender && error.is_sender_signature_problem() => {
                        let Some(default_sender) = self.default_sender.as_ref() else {
                            return Err(error.into_api_error(&to));
                        };
                        let fallback = PostmarkEmail {
                            from: EmailSender {
                                name: sender.name,
                                email: default_sender.email.clone(),
                            }
                            .formatted(),
                            reply_to: default_sender.email.clone(),
                            ..message
                        };
                        self.email
                            .send(&fallback)
                            .await
                            .map(|()| Value::Bool(true))
                            .map_err(|error| error.into_api_error(&to))
                    }
                    Err(error) => Err(error.into_api_error(&to)),
                }
            }
            DashboardControlHookOperation::DeliverInvite {
                target,
                inviter_email,
                invitee_email,
                title,
                role,
                ..
            } => {
                let team_sender = self
                    .team_sender
                    .as_ref()
                    .ok_or_else(|| missing_sender_email("INSTANT_TEAM_EMAIL_SENDER_EMAIL"))?;
                let message = invite_email(
                    team_sender,
                    &self.dashboard_origin,
                    target,
                    &role,
                    &inviter_email,
                    &invitee_email,
                    &title,
                );
                self.email
                    .send(&message)
                    .await
                    .map(|()| Value::Bool(true))
                    .map_err(|error| error.into_api_error(&invitee_email))
            }
            other => self.inner.execute(other).await,
        }
    }
}

fn missing_sender_email(variable: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::Unknown,
        format!("Email sender is not configured; set {variable}"),
    )
}

fn invite_email(
    sender: &EmailSender,
    dashboard_origin: &str,
    target: &str,
    role: &str,
    inviter: &str,
    recipient: &str,
    title: &str,
) -> PostmarkEmail {
    let ownership_transfer = target == "app" && role == "creator";
    let subject = if ownership_transfer {
        format!("[Instant] You've been asked to take ownership of {title}")
    } else {
        format!("[Instant] You've been invited to collaborate on {title}")
    };
    let invitation = if ownership_transfer {
        format!(
            "{} invited you to become the new owner of {}.",
            escape_html(inviter),
            escape_html(title)
        )
    } else {
        format!(
            "{} invited you to collaborate on their {} {}.",
            escape_html(inviter),
            if target == "org" {
                "organization"
            } else {
                "app"
            },
            escape_html(title)
        )
    };
    let invite_url = format!("{}/dash?s=invites", dashboard_origin.trim_end_matches('/'));
    let body = standard_body(&format!(
        "<p><strong>Hey there!</strong></p><p>{invitation}</p><p>Navigate to <a href=\"{}\">Instant</a> to accept the invite.</p><p>Note: this invite will expire in 3 days. If you don't know the user inviting you, please reply to this email.</p>",
        escape_html(&invite_url)
    ));
    PostmarkEmail::html(sender, recipient, subject, body)
}

fn sender_id(value: &Value) -> Result<i32, ApiError> {
    value
        .get("ID")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok())
        .ok_or_else(|| {
            ApiError::new(
                ApiErrorType::Unknown,
                "Postmark sender response did not include a valid ID",
            )
        })
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum PostmarkError {
    #[error("Postmark sender signature error {code}: {message}")]
    SenderSignature { code: i32, message: String },
    #[error("Postmark provider error {code}: {message}")]
    Provider { code: i32, message: String },
    #[error("Postmark transport error: {0}")]
    Transport(String),
}

impl PostmarkError {
    pub(crate) fn into_api_error(self, recipient: &str) -> ApiError {
        match self {
            Self::Provider { code: 406, .. } => ApiError::new(
                ApiErrorType::EmailSendFailed,
                format!(
                    "We couldn't deliver the email to {recipient}. The address has been marked as inactive and can't receive mail."
                ),
            )
            .with_hint(json!({"recipient": recipient, "recipient-problem?": true})),
            Self::Provider { code: 412 | 413, .. } => ApiError::new(
                ApiErrorType::EmailSendFailed,
                "The Postmark account isn't approved for sending yet.",
            )
            .with_hint(json!({"recipient": recipient})),
            _ => ApiError::new(
                ApiErrorType::EmailSendFailed,
                "We weren't able to send the email.",
            )
            .with_hint(json!({"recipient": recipient})),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PostmarkResponse {
    error_code: i32,
    #[serde(default)]
    message: String,
}

/// PostgreSQL template resolver plus Postmark delivery for runtime/admin codes.
pub struct PostgresMagicCodeDelivery {
    client: PostgresClientSource,
    email: EmailDeliveryClient,
    default_sender: Option<EmailSender>,
}

impl PostgresMagicCodeDelivery {
    #[must_use]
    pub fn new(client: Client, postmark: PostmarkEmailClient, default_sender: EmailSender) -> Self {
        Self::with_email_client(
            client,
            EmailDeliveryClient::new(
                crate::EmailProvider::Postmark,
                postmark,
                crate::SendGridEmailClient::new(None),
                true,
                false,
            ),
            Some(default_sender),
        )
    }

    #[must_use]
    pub fn with_email_client(
        client: Client,
        email: EmailDeliveryClient,
        default_sender: Option<EmailSender>,
    ) -> Self {
        Self::with_email_client_source(PostgresClientSource::fixed(client), email, default_sender)
    }

    /// Uses a reconnecting pool for template and sender resolution.
    #[must_use]
    pub fn with_email_client_pool(
        pool: PostgresPool,
        email: EmailDeliveryClient,
        default_sender: Option<EmailSender>,
    ) -> Self {
        Self::with_email_client_source(PostgresClientSource::pooled(pool), email, default_sender)
    }

    fn with_email_client_source(
        client: PostgresClientSource,
        email: EmailDeliveryClient,
        default_sender: Option<EmailSender>,
    ) -> Self {
        Self {
            client,
            email,
            default_sender,
        }
    }

    async fn rendered_email(
        &self,
        default_sender: &EmailSender,
        app_id: Uuid,
        recipient: &str,
        code: &str,
        expiry_minutes: u64,
    ) -> Result<(PostmarkEmail, Option<String>), ApiError> {
        let client = self.client.acquire().await.map_err(database_error)?;
        let row = client
            .query_opt(
                "SELECT a.title, t.body, t.name, t.subject, s.email,
                        COALESCE(v.verified, false),
                        (SELECT d.verified_at FROM app_sender_domains d
                         WHERE d.app_id = a.id
                           AND d.domain = lower(split_part(s.email, '@', 2)))
                 FROM apps a
                 LEFT JOIN app_email_templates t
                   ON t.app_id = a.id AND t.email_type = 'magic-code'
                 LEFT JOIN app_email_senders s ON s.id = t.sender_id
                 LEFT JOIN app_email_verifications v
                   ON v.app_id = a.id AND v.sender_id = s.id
                 WHERE a.id = $1 AND a.deletion_marked_at IS NULL",
                &[&app_id],
            )
            .await
            .map_err(database_error)?
            .ok_or_else(|| ApiError::new(ApiErrorType::RecordNotFound, "Record not found: app"))?;
        let app_title: String = row.get(0);
        let template_body: Option<String> = row.get(1);
        let template_name: Option<String> = row.get(2);
        let template_subject: Option<String> = row.get(3);
        let custom_email: Option<String> = row.get(4);
        let ownership_verified: bool = row.get(5);
        let domain_verified = domain_verification_is_fresh(row.get(6), Utc::now());
        let expiration = friendly_expiration(expiry_minutes);
        let parameters = [
            ("user_email", recipient),
            ("code", code),
            ("app_title", app_title.as_str()),
            ("expiration", expiration.as_str()),
        ];
        let custom_domain = custom_email
            .as_deref()
            .filter(|_| {
                self.email.custom_sender_allowed(
                    custom_email.as_deref(),
                    ownership_verified,
                    domain_verified,
                )
            })
            .map(email_domain);
        let sender = if custom_domain.is_some() {
            EmailSender {
                name: template_name.clone().or_else(|| Some(app_title.clone())),
                email: custom_email.expect("verified custom sender has an email"),
            }
        } else {
            EmailSender {
                name: Some(app_title.clone()),
                email: default_sender.email.clone(),
            }
        };
        let (subject, body) = match (template_subject, template_body) {
            (Some(subject), Some(body)) => (
                replace_template(&subject, &parameters, false),
                replace_template(&body, &parameters, true),
            ),
            _ => (
                format!("{code} is your verification code for {app_title}"),
                default_magic_code_body(&app_title, code, &expiration),
            ),
        };
        Ok((
            PostmarkEmail::html(&sender, recipient, subject, body),
            custom_domain,
        ))
    }

    async fn mark_domain_unverified(&self, app_id: Uuid, domain: &str) {
        let result = async {
            let client = self.client.acquire().await.map_err(database_error)?;
            client
                .execute(
                    "UPDATE app_sender_domains
                     SET verified_at = NULL, last_checked_at = now(), last_error = $3
                     WHERE app_id = $1 AND domain = $2",
                    &[&app_id, &domain, &"Cloudflare rejected the sender domain"],
                )
                .await
                .map_err(database_error)?;
            Ok::<(), ApiError>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%app_id, domain, error = %error.body().message,
                "could not mark the sender domain unverified");
        }
    }
}

impl MagicCodeDelivery for PostgresMagicCodeDelivery {
    async fn deliver(
        &self,
        app_id: Uuid,
        email: &str,
        code: &str,
        expiry_minutes: u64,
    ) -> Result<(), ApiError> {
        if !self.email.delivery_enabled() {
            tracing::warn!(
                %app_id,
                recipient = email,
                verification_code = code,
                "Application login code generated because email delivery is disabled"
            );
            return Ok(());
        }
        let Some(default_sender) = self.default_sender.as_ref() else {
            tracing::warn!(
                %app_id,
                recipient = email,
                verification_code = code,
                "Application login code generated because no sender email is configured; set INSTANT_APP_EMAIL_SENDER_EMAIL"
            );
            return Ok(());
        };
        let (message, custom_domain) = self
            .rendered_email(default_sender, app_id, email, code, expiry_minutes)
            .await?;
        match self.email.send(&message).await {
            Ok(()) => Ok(()),
            Err(error) if custom_domain.is_some() && error.is_sender_signature_problem() => {
                let custom_domain = custom_domain.expect("custom sender domain");
                if self.email.requires_verified_domain() {
                    self.mark_domain_unverified(app_id, &custom_domain).await;
                }
                let fallback = PostmarkEmail {
                    from: EmailSender {
                        name: message
                            .from
                            .split_once(" <")
                            .map(|(name, _)| name.to_owned()),
                        email: default_sender.email.clone(),
                    }
                    .formatted(),
                    reply_to: default_sender.email.clone(),
                    ..message
                };
                self.email
                    .send(&fallback)
                    .await
                    .map_err(|error| error.into_api_error(email))
            }
            Err(error) => Err(error.into_api_error(email)),
        }
    }
}

#[derive(Clone)]
pub struct PostmarkDashboardMagicCodeDelivery {
    email: EmailDeliveryClient,
    sender: Option<EmailSender>,
}

impl PostmarkDashboardMagicCodeDelivery {
    #[must_use]
    pub fn new(postmark: PostmarkEmailClient, sender: EmailSender) -> Self {
        Self::with_email_client(
            EmailDeliveryClient::new(
                crate::EmailProvider::Postmark,
                postmark,
                crate::SendGridEmailClient::new(None),
                true,
                false,
            ),
            Some(sender),
        )
    }

    #[must_use]
    pub const fn with_email_client(
        email: EmailDeliveryClient,
        sender: Option<EmailSender>,
    ) -> Self {
        Self { email, sender }
    }
}

impl DashboardMagicCodeDelivery for PostmarkDashboardMagicCodeDelivery {
    async fn deliver(&self, email: String, code: String) -> Result<(), ApiError> {
        if !self.email.delivery_enabled() {
            tracing::warn!(
                recipient = email,
                verification_code = code,
                "Dashboard login code generated because email delivery is disabled"
            );
            return Ok(());
        }
        let Some(sender) = self.sender.as_ref() else {
            tracing::warn!(
                recipient = email,
                verification_code = code,
                "Dashboard login code generated because no sender email is configured; set INSTANT_DASHBOARD_EMAIL_SENDER_EMAIL"
            );
            return Ok(());
        };
        let title = sender.name.as_deref().unwrap_or("Instant");
        let message = PostmarkEmail::html(
            sender,
            &email,
            format!("{code} is your verification code for {title}"),
            default_magic_code_body(title, &code, "10 minutes"),
        );
        self.email
            .send(&message)
            .await
            .map_err(|error| error.into_api_error(&email))
    }
}

fn friendly_expiration(minutes: u64) -> String {
    if minutes >= 60 {
        let hours = minutes / 60;
        format!("{hours} hour{}", if hours > 1 { "s" } else { "" })
    } else {
        format!("{minutes} minute{}", if minutes > 1 { "s" } else { "" })
    }
}

fn replace_template(template: &str, parameters: &[(&str, &str)], escape: bool) -> String {
    parameters
        .iter()
        .fold(template.to_owned(), |rendered, (name, value)| {
            let value = if escape {
                escape_html(value)
            } else {
                (*value).to_owned()
            };
            rendered.replace(&format!("{{{name}}}"), &value)
        })
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn default_magic_code_body(app_title: &str, code: &str, expiration: &str) -> String {
    standard_body(&format!(
        "<p><strong>Welcome,</strong></p><p>You asked to join {}. To complete your registration, use this verification code:</p><h2 style=\"text-align: center\"><strong>{}</strong></h2><p>Copy and paste this into the confirmation box, and you'll be on your way.</p><p>Note: This code will expire in {}, and can only be used once. If you didn't request this code, please reply to this email.</p>",
        escape_html(app_title),
        escape_html(code),
        escape_html(expiration),
    ))
}

fn standard_body(body: &str) -> String {
    format!(
        "<div style='background:#f6f6f6;font-family:Helvetica,Arial,sans-serif;line-height:1.6;font-size:18px'><div style='max-width:650px;margin:0 auto;background:white;padding:20px'>{body}</div></div>"
    )
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.and_then(|value| (!value.trim().is_empty()).then_some(value))
}

/// The lowercase domain of a sender address.
pub(crate) fn email_domain(email: &str) -> String {
    email
        .trim()
        .rsplit_once('@')
        .map_or_else(String::new, |(_, domain)| {
            domain.trim().to_ascii_lowercase()
        })
}

pub(crate) fn domain_verification_is_fresh(
    verified_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    verified_at.is_some_and(|verified_at| {
        now - verified_at < chrono::Duration::hours(SENDER_DOMAIN_VERIFICATION_HOURS)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopDashboardControlHooks;

    #[test]
    fn provider_spans_match_the_jvm_names_without_email_or_secret_fields() {
        tracing::subscriber::with_default(tracing_subscriber::registry(), || {
            for (span, expected_name) in [
                (postmark_send_span(true), "postmark/send"),
                (postmark_send_span(false), "postmark/send-disabled"),
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
    fn sender_display_names_are_header_safe_and_bounded() {
        let sender = EmailSender {
            name: Some("Instant\r\nBcc: <evil@example.com> \"quoted\"".to_owned()),
            email: "verify@example.com".to_owned(),
        };
        assert_eq!(
            sender.formatted(),
            "InstantBcc: evil@example.com quoted <verify@example.com>"
        );
        let long = EmailSender {
            name: Some("a".repeat(100)),
            email: "verify@example.com".to_owned(),
        };
        assert_eq!(
            long.formatted(),
            format!("{} <verify@example.com>", "a".repeat(64))
        );
        let empty = EmailSender {
            name: Some("<>\r\n".to_owned()),
            email: "verify@example.com".to_owned(),
        };
        assert_eq!(empty.formatted(), "verify@example.com");
    }

    #[test]
    fn app_titles_with_script_tags_are_escaped_in_rendered_bodies() {
        let title = "<script>alert(1)</script>";
        for body in [
            default_magic_code_body(title, "123456", "1 hour"),
            replace_template(
                "<p>{app_title}: {code}</p>",
                &[("app_title", title), ("code", "123456")],
                true,
            ),
        ] {
            assert!(!body.contains("<script>"), "unescaped markup in {body}");
            assert!(body.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        }
    }

    #[test]
    fn templates_escape_html_but_not_subjects_and_expiry_matches_jvm() {
        let parameters = [("app_title", "A&B <demo>")];
        assert_eq!(
            replace_template("Join {app_title}", &parameters, false),
            "Join A&B <demo>"
        );
        assert_eq!(
            replace_template("<p>{app_title}</p>", &parameters, true),
            "<p>A&amp;B &lt;demo&gt;</p>"
        );
        assert_eq!(friendly_expiration(59), "59 minutes");
        assert_eq!(friendly_expiration(60), "1 hour");
        assert_eq!(friendly_expiration(125), "2 hours");
    }

    #[test]
    fn postmark_payload_uses_the_jvm_wire_keys() {
        let email = PostmarkEmail::html(
            &EmailSender {
                name: Some("Instant".to_owned()),
                email: "verify@example.com".to_owned(),
            },
            "user@example.com",
            "Subject",
            "<p>Body</p>",
        );
        assert_eq!(
            serde_json::to_value(email).unwrap(),
            json!({
                "From": "Instant <verify@example.com>",
                "To": "user@example.com",
                "ReplyTo": "verify@example.com",
                "Subject": "Subject",
                "MessageStream": "outbound",
                "HTMLBody": "<p>Body</p>"
            })
        );
    }

    #[test]
    fn team_and_ownership_invites_match_the_jvm_copy_and_escape_values() {
        let sender = EmailSender {
            name: Some("Instant".to_owned()),
            email: "teams@pm.instantdb.com".to_owned(),
        };
        let team = invite_email(
            &sender,
            "https://www.instantdb.com/",
            "org",
            "admin",
            "owner+test@example.com",
            "invitee@example.com",
            "R&D <Team>",
        );
        assert_eq!(
            team.subject,
            "[Instant] You've been invited to collaborate on R&D <Team>"
        );
        assert!(
            team.html_body
                .contains("their organization R&amp;D &lt;Team&gt;")
        );
        assert!(
            team.html_body
                .contains("https://www.instantdb.com/dash?s=invites")
        );

        let transfer = invite_email(
            &sender,
            "https://www.instantdb.com",
            "app",
            "creator",
            "owner@example.com",
            "invitee@example.com",
            "Alpha",
        );
        assert_eq!(
            transfer.subject,
            "[Instant] You've been asked to take ownership of Alpha"
        );
        assert!(transfer.html_body.contains("become the new owner of Alpha"));
        assert_eq!(transfer.reply_to, "teams@pm.instantdb.com");
    }

    #[test]
    fn sender_domain_verification_expires_after_one_day_of_slack() {
        let now = DateTime::parse_from_rfc3339("2026-10-26T12:00:00Z")
            .unwrap()
            .to_utc();
        assert!(!domain_verification_is_fresh(None, now));
        assert!(domain_verification_is_fresh(
            Some(now - chrono::Duration::hours(24)),
            now
        ));
        assert!(!domain_verification_is_fresh(
            Some(now - chrono::Duration::hours(26)),
            now
        ));
        assert_eq!(
            email_domain(" Custom@Mail.Example.COM "),
            "mail.example.com"
        );
        assert_eq!(email_domain("not-an-email"), "");
    }

    #[tokio::test]
    async fn dashboard_middleware_owns_email_operations_and_delegates_the_rest() {
        let hooks = PostmarkDashboardControlHooks::with_email_client(
            NoopDashboardControlHooks,
            EmailDeliveryClient::new(
                crate::EmailProvider::Postmark,
                PostmarkEmailClient::new(None),
                crate::SendGridEmailClient::new(None),
                true,
                false,
            ),
            PostmarkAccountClient::new(None),
            Some(EmailSender {
                name: Some("Instant".to_owned()),
                email: "verify@example.com".to_owned(),
            }),
            Some(EmailSender {
                name: Some("Instant".to_owned()),
                email: "teams@example.com".to_owned(),
            }),
            "https://example.test".to_owned(),
            1_440,
        );
        let template = hooks
            .execute(DashboardControlHookOperation::DefaultEmailTemplate)
            .await
            .unwrap();
        assert_eq!(template["sender-email"], "verify@example.com");
        assert!(template["body"].as_str().unwrap().contains("{code}"));
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::DashboardAuthDefaultSender {
                    app_id: Uuid::nil(),
                })
                .await
                .unwrap(),
            Value::String("verify@example.com".to_owned())
        );
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::DeliverDashboardEmail {
                    kind: "test",
                    app_id: Uuid::nil(),
                    to: "user@example.com".to_owned(),
                    sender_email: None,
                    sender_name: "Instant".to_owned(),
                    subject: "Subject".to_owned(),
                    body: "Body".to_owned(),
                })
                .await
                .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::AuthorizeBackup {
                    app_id: Uuid::nil(),
                    creator_id: None,
                    work_estimate: 0,
                    client_ip: None,
                })
                .await
                .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::GetEmailSender { postmark_id: 42 })
                .await
                .unwrap(),
            Value::Null
        );
        let missing_account_token = hooks
            .execute(DashboardControlHookOperation::SyncEmailSender {
                app_id: Uuid::nil(),
                email: "sender@example.com".to_owned(),
                name: "Sender".to_owned(),
            })
            .await
            .unwrap_err();
        assert_eq!(
            missing_account_token.body().message,
            "POSTMARK_ACCOUNT_TOKEN is required for sender administration"
        );
    }

    struct EnabledDomains;

    impl crate::cloudflare_email::EmailDomainVerifier for EnabledDomains {
        fn verify_domain<'a>(
            &'a self,
            domain: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::cloudflare_email::EmailDomainStatus,
                            crate::cloudflare_email::CloudflareEmailError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(crate::cloudflare_email::EmailDomainStatus {
                    zone_id: Some(format!("zone-{domain}")),
                    enabled: true,
                })
            })
        }
    }

    #[tokio::test]
    async fn cloudflare_sender_operations_delegate_to_the_domain_verifier() {
        let hooks = PostmarkDashboardControlHooks::with_email_client(
            NoopDashboardControlHooks,
            EmailDeliveryClient::new(
                crate::EmailProvider::Cloudflare,
                PostmarkEmailClient::new(None),
                crate::SendGridEmailClient::new(None),
                false,
                false,
            ),
            PostmarkAccountClient::new(None),
            None,
            None,
            "https://example.test".to_owned(),
            1_440,
        );
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::SyncEmailSender {
                    app_id: Uuid::nil(),
                    email: "sender@example.com".to_owned(),
                    name: "Sender".to_owned(),
                })
                .await
                .unwrap(),
            Value::Null
        );
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::VerifyEmailDomain {
                    app_id: Uuid::nil(),
                    domain: "example.com".to_owned(),
                })
                .await
                .unwrap(),
            Value::Null
        );

        let hooks = hooks.with_email_domain_verifier(Some(Arc::new(EnabledDomains)));
        assert_eq!(
            hooks
                .execute(DashboardControlHookOperation::VerifyEmailDomain {
                    app_id: Uuid::nil(),
                    domain: "example.com".to_owned(),
                })
                .await
                .unwrap(),
            json!({"enabled": true, "zone-id": "zone-example.com", "error": null})
        );
    }
}
