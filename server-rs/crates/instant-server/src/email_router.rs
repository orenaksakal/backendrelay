use instant_http::ApiError;

use crate::{
    EmailProvider, PostmarkEmail, PostmarkEmailClient, PostmarkError, SendGridEmailClient,
    SendGridError, ServerEmailConfig,
};

/// JVM-compatible provider selector for structured transactional email.
#[derive(Clone)]
pub struct EmailDeliveryClient {
    provider: EmailProvider,
    cloudflare: Option<crate::cloudflare_email::CloudflareEmailClient>,
    postmark: PostmarkEmailClient,
    sendgrid: SendGridEmailClient,
    resend: crate::ResendEmailClient,
    resend_enabled: bool,
    postmark_enabled: bool,
    sendgrid_enabled: bool,
}

impl EmailDeliveryClient {
    #[must_use]
    pub fn from_config(config: &ServerEmailConfig) -> Self {
        let postmark_token = config.postmark_server_token().map(str::to_owned);
        let sendgrid_token = config.sendgrid_token().map(str::to_owned);
        Self {
            provider: config.provider(),
            cloudflare: config
                .cloudflare_account_id()
                .zip(config.cloudflare_email_api_token())
                .map(|(account, token)| {
                    crate::cloudflare_email::CloudflareEmailClient::new(account, token)
                }),
            postmark_enabled: postmark_token.is_some(),
            sendgrid_enabled: sendgrid_token.is_some(),
            postmark: PostmarkEmailClient::new(postmark_token),
            sendgrid: SendGridEmailClient::new(sendgrid_token),
            resend: crate::ResendEmailClient::new(config.resend_token().map(str::to_owned)),
            resend_enabled: config.resend_token().is_some(),
        }
    }

    #[must_use]
    pub fn new(
        provider: EmailProvider,
        postmark: PostmarkEmailClient,
        sendgrid: SendGridEmailClient,
        postmark_enabled: bool,
        sendgrid_enabled: bool,
    ) -> Self {
        Self {
            provider,
            cloudflare: None,
            resend: crate::ResendEmailClient::new(None),
            resend_enabled: false,
            postmark,
            sendgrid,
            postmark_enabled,
            sendgrid_enabled,
        }
    }

    #[must_use]
    pub const fn delivery_enabled(&self) -> bool {
        match self.provider {
            EmailProvider::Cloudflare => true,
            EmailProvider::Postmark => self.postmark_enabled,
            EmailProvider::SendGrid => self.sendgrid_enabled,
            EmailProvider::Resend => self.resend_enabled,
            EmailProvider::Auto => {
                self.postmark_enabled || self.sendgrid_enabled || self.resend_enabled
            }
        }
    }

    /// Sends through explicit provider selection or token autodetection.
    ///
    /// # Errors
    ///
    /// Returns the selected provider's typed failure.
    pub async fn send(&self, email: &PostmarkEmail) -> Result<(), EmailDeliveryError> {
        let result = self.send_with_provider(email).await;
        crate::metrics::record_email(
            self.provider,
            if result.is_ok() {
                crate::metrics::EmailOutcome::Sent
            } else {
                crate::metrics::EmailOutcome::Failed
            },
        );
        result
    }

    async fn send_with_provider(&self, email: &PostmarkEmail) -> Result<(), EmailDeliveryError> {
        if self.provider == EmailProvider::Cloudflare {
            return self
                .cloudflare
                .as_ref()
                .ok_or(EmailDeliveryError::Cloudflare(
                    crate::cloudflare_email::CloudflareEmailError::NotConfigured,
                ))?
                .send(email)
                .await
                .map_err(EmailDeliveryError::Cloudflare);
        }
        if self.provider == EmailProvider::Resend
            || (self.provider == EmailProvider::Auto
                && self.resend_enabled
                && !self.postmark_enabled
                && !self.sendgrid_enabled)
        {
            return self
                .resend
                .send(email)
                .await
                .map_err(EmailDeliveryError::Resend);
        }
        if self.use_sendgrid() {
            self.sendgrid
                .send(email)
                .await
                .map_err(EmailDeliveryError::SendGrid)
        } else {
            self.postmark
                .send(email)
                .await
                .map_err(EmailDeliveryError::Postmark)
        }
    }

    /// Cloudflare app mail requires a domain verified for this app; Postmark
    /// sender signatures do not establish Cloudflare domain ownership.
    pub(crate) const fn requires_verified_domain(&self) -> bool {
        matches!(self.provider, EmailProvider::Cloudflare)
    }

    /// Selects the app template's custom sender only when the provider has
    /// verified it for this app. Without verification the platform sender is
    /// used.
    pub(crate) fn custom_sender_allowed(
        &self,
        custom_email: Option<&str>,
        ownership_verified: bool,
        domain_verified: bool,
    ) -> bool {
        custom_email.is_some_and(|email| !email.trim().is_empty())
            && if self.requires_verified_domain() {
                domain_verified
            } else {
                ownership_verified
            }
    }

    const fn use_sendgrid(&self) -> bool {
        match self.provider {
            EmailProvider::SendGrid => true,
            EmailProvider::Postmark | EmailProvider::Cloudflare | EmailProvider::Resend => false,
            EmailProvider::Auto => self.sendgrid_enabled && !self.postmark_enabled,
        }
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum EmailDeliveryError {
    #[error(transparent)]
    Cloudflare(#[from] crate::cloudflare_email::CloudflareEmailError),
    #[error(transparent)]
    Postmark(#[from] PostmarkError),
    #[error(transparent)]
    SendGrid(#[from] SendGridError),
    #[error(transparent)]
    Resend(#[from] crate::ResendError),
}

impl EmailDeliveryError {
    #[must_use]
    pub const fn is_sender_signature_problem(&self) -> bool {
        matches!(
            self,
            Self::Postmark(PostmarkError::SenderSignature { .. })
                | Self::Cloudflare(
                    crate::cloudflare_email::CloudflareEmailError::SenderNotVerified
                )
        )
    }

    pub(crate) fn into_api_error(self, recipient: &str) -> ApiError {
        match self {
            Self::Cloudflare(_) => ApiError::new(
                instant_http::ApiErrorType::EmailSendFailed,
                "We weren't able to send the email.",
            ),
            Self::Postmark(error) => error.into_api_error(recipient),
            Self::SendGrid(error) => error.into_api_error(recipient),
            Self::Resend(error) => error.into_api_error(recipient),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EmailSender;

    fn email(from: &str) -> PostmarkEmail {
        PostmarkEmail::html(
            &EmailSender {
                name: Some("Instant".to_owned()),
                email: from.to_owned(),
            },
            "user@example.com",
            "Subject",
            "Body",
        )
    }

    fn router(provider: EmailProvider, postmark: bool, sendgrid: bool) -> EmailDeliveryClient {
        EmailDeliveryClient::new(
            provider,
            PostmarkEmailClient::new(None),
            SendGridEmailClient::new(None),
            postmark,
            sendgrid,
        )
    }

    #[test]
    fn resend_explicit_and_token_only_configs_enable_delivery_without_changing_precedence() {
        for provider in [None, Some("resend"), Some("postmark"), Some("sendgrid")] {
            let mut pairs = vec![("RESEND_TOKEN", "resend-test-token")];
            if let Some(provider) = provider {
                pairs.push(("INSTANT_EMAIL_PROVIDER", provider));
            }
            let config = crate::ServerConfig::from_pairs(pairs).unwrap();
            let router = EmailDeliveryClient::from_config(config.email());
            assert_eq!(
                router.delivery_enabled(),
                provider.is_none() || provider == Some("resend")
            );
            assert!(!router.requires_verified_domain());
        }
    }

    #[test]
    fn app_sender_selection_requires_ownership_and_a_verified_domain_for_cloudflare() {
        let cloudflare = router(EmailProvider::Cloudflare, true, true);
        assert!(cloudflare.requires_verified_domain());
        assert!(!cloudflare.custom_sender_allowed(None, true, true));
        assert!(!cloudflare.custom_sender_allowed(Some(" "), true, true));
        assert!(!cloudflare.custom_sender_allowed(Some("custom@example.com"), true, false));
        assert!(cloudflare.custom_sender_allowed(Some("custom@example.com"), false, true));

        let postmark = router(EmailProvider::Postmark, true, false);
        assert!(!postmark.requires_verified_domain());
        assert!(!postmark.custom_sender_allowed(Some("custom@example.com"), false, true));
        assert!(postmark.custom_sender_allowed(Some("custom@example.com"), true, false));
    }

    #[tokio::test]
    async fn cloudflare_never_falls_through_to_disabled_legacy_delivery() {
        let router = router(EmailProvider::Cloudflare, true, true);
        assert!(router.requires_verified_domain());
        assert!(router.delivery_enabled());
        assert!(matches!(
            router.send(&email("transactional@example.com")).await,
            Err(EmailDeliveryError::Cloudflare(
                crate::cloudflare_email::CloudflareEmailError::NotConfigured
            ))
        ));
    }

    #[test]
    fn explicit_provider_wins_and_auto_detects_sendgrid_only_configuration() {
        assert!(router(EmailProvider::SendGrid, true, true).use_sendgrid());
        assert!(!router(EmailProvider::Postmark, false, true).use_sendgrid());
        assert!(router(EmailProvider::Auto, false, true).use_sendgrid());
        assert!(!router(EmailProvider::Auto, true, true).use_sendgrid());
        assert!(!router(EmailProvider::Auto, false, false).delivery_enabled());
        assert!(router(EmailProvider::Auto, true, false).delivery_enabled());
        assert!(router(EmailProvider::Auto, false, true).delivery_enabled());
    }
}
