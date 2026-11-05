use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use instant_http::{ApiError, ApiErrorType};
use serde_json::Value;
use tokio_postgres::Client;

use crate::db_error::database_error;
use crate::postgres_pool::PostgresClientSource;
use crate::{
    DashboardIdentityPolicy, INSTANT_CONFIG_APP_ID, PostgresPool, ScheduledTask, TaskSchedule,
};

const DEFAULT_MAGIC_CODE_RATE_LIMIT: u32 = 20;
const MAX_VERIFY_FAILURES: u32 = 5;
const VERIFY_FAILURE_WINDOW: Duration = Duration::from_mins(15);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SignupMode {
    Open,
    Restricted,
    #[default]
    Closed,
}

#[derive(Clone, Debug)]
struct IdentityPolicySnapshot {
    /// Mode set by the config-app `dashboard-signups` flag. `None` means the
    /// flag is unset or unrecognised, so the operator default applies; it is
    /// resolved at check time so the default can never be stale.
    signup_mode: Option<SignupMode>,
    allowed_emails: HashSet<String>,
    magic_code_rate_limit: u32,
}

impl Default for IdentityPolicySnapshot {
    fn default() -> Self {
        Self {
            signup_mode: None,
            allowed_emails: HashSet::new(),
            magic_code_rate_limit: DEFAULT_MAGIC_CODE_RATE_LIMIT,
        }
    }
}

#[derive(Clone, Debug)]
struct Bucket {
    tokens: f64,
    updated_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum MagicCodeAction {
    Send,
    Verify,
}

struct IdentityPolicyInner {
    client: PostgresClientSource,
    snapshot: RwLock<IdentityPolicySnapshot>,
    buckets: StdMutex<HashMap<(MagicCodeAction, String), Bucket>>,
    verify_failures: StdMutex<HashMap<String, (u32, Instant)>>,
}

/// Refreshable config-app dashboard signup and magic-code rate policy.
#[derive(Clone)]
pub struct PostgresDashboardIdentityPolicy {
    inner: Arc<IdentityPolicyInner>,
    allowed_emails: Option<Arc<HashSet<String>>>,
    superuser_email: Option<String>,
    default_signup_mode: SignupMode,
}

impl PostgresDashboardIdentityPolicy {
    /// Loads the initial policy before it can authorize a request.
    ///
    /// # Errors
    ///
    /// Returns a database error when the config-app flags cannot be read.
    pub async fn load(client: Client) -> Result<Self, ApiError> {
        let policy = Self {
            inner: Arc::new(IdentityPolicyInner {
                client: PostgresClientSource::fixed(client),
                snapshot: RwLock::new(IdentityPolicySnapshot::default()),
                buckets: StdMutex::new(HashMap::new()),
                verify_failures: StdMutex::new(HashMap::new()),
            }),
            allowed_emails: None,
            superuser_email: None,
            default_signup_mode: SignupMode::default(),
        };
        policy.refresh().await?;
        Ok(policy)
    }

    /// Loads the initial policy from a read-only pooled checkout.
    ///
    /// # Errors
    ///
    /// Returns a database error when a connection cannot be acquired or the
    /// config-app flags cannot be read.
    pub async fn load_pool(pool: PostgresPool) -> Result<Self, ApiError> {
        let policy = Self {
            inner: Arc::new(IdentityPolicyInner {
                client: PostgresClientSource::pooled_read_only(pool),
                snapshot: RwLock::new(IdentityPolicySnapshot::default()),
                buckets: StdMutex::new(HashMap::new()),
                verify_failures: StdMutex::new(HashMap::new()),
            }),
            allowed_emails: None,
            superuser_email: None,
            default_signup_mode: SignupMode::default(),
        };
        policy.refresh().await?;
        Ok(policy)
    }

    /// A static operator allowlist is an additional restriction. Dynamic flags
    /// can restrict it further, but refreshing an "open" flag cannot remove it.
    #[must_use]
    pub fn with_allowed_emails(mut self, allowed_emails: Option<HashSet<String>>) -> Self {
        self.allowed_emails = allowed_emails.map(Arc::new);
        self
    }

    /// The deployment superuser may always sign in and sign up.
    #[must_use]
    pub fn with_superuser_email(mut self, superuser_email: Option<String>) -> Self {
        self.superuser_email = superuser_email.map(|email| email.trim().to_ascii_lowercase());
        self
    }

    /// Signup is closed unless the config-app flag opens it or the operator
    /// explicitly sets `INSTANT_DASHBOARD_SIGNUP=open`.
    #[must_use]
    pub const fn with_signup_open(mut self, open: bool) -> Self {
        self.default_signup_mode = if open {
            SignupMode::Open
        } else {
            SignupMode::Closed
        };
        self
    }

    fn check_email(&self, email: &str) -> Result<(), ApiError> {
        check_dashboard_email(
            self.allowed_emails.as_deref(),
            self.superuser_email.as_deref(),
            email,
        )
    }

    /// Reloads dynamic signup and capacity flags atomically.
    ///
    /// # Errors
    ///
    /// Returns a database error without replacing the last good snapshot.
    pub async fn refresh(&self) -> Result<(), ApiError> {
        let client = self.inner.client.acquire().await.map_err(database_error)?;
        let rows = client
            .query(
                "SELECT flag_setting.value #>> '{}', flag_value.value
                 FROM attrs setting_attr
                 JOIN triples flag_setting
                   ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
                 JOIN attrs value_attr
                   ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
                  AND value_attr.label = 'value'
                 JOIN triples flag_value
                   ON flag_value.app_id = $1
                  AND flag_value.entity_id = flag_setting.entity_id
                  AND flag_value.attr_id = value_attr.id
                 WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
                   AND setting_attr.label = 'setting'
                   AND flag_setting.value IN (
                     '\"dashboard-signups\"'::jsonb,
                     '\"magic-code-rate-limit-per-hour\"'::jsonb
                   )",
                &[&INSTANT_CONFIG_APP_ID],
            )
            .await
            .map_err(database_error)?;
        let mut snapshot = IdentityPolicySnapshot::default();
        for row in rows {
            let setting: String = row.get(0);
            let value: Value = row.get(1);
            match setting.as_str() {
                "dashboard-signups" => apply_signup_flag(&mut snapshot, &value),
                "magic-code-rate-limit-per-hour" => {
                    snapshot.magic_code_rate_limit = value
                        .as_u64()
                        .and_then(|value| u32::try_from(value).ok())
                        .filter(|value| *value > 0)
                        .unwrap_or(DEFAULT_MAGIC_CODE_RATE_LIMIT);
                }
                _ => {}
            }
        }
        *self
            .inner
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        Ok(())
    }

    /// Creates the hourly JVM flag-refresh worker.
    #[must_use]
    pub fn refresh_task(self) -> ScheduledTask {
        ScheduledTask::new(
            "dashboard-identity-policy-refresh",
            TaskSchedule::Fixed {
                every: Duration::from_secs(3_600),
                start_immediately: false,
            },
            Duration::from_secs(30),
            move || {
                let policy = self.clone();
                async move {
                    policy
                        .refresh()
                        .await
                        .map_err(|error| PolicyRefreshError(error.body().message.clone()))
                }
            },
        )
    }

    fn consume(&self, action: MagicCodeAction, email: &str) -> Result<(), ApiError> {
        let capacity = self
            .inner
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .magic_code_rate_limit;
        let now = Instant::now();
        let normalized = email.trim().to_ascii_lowercase();
        let mut buckets = self
            .inner
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buckets
            .retain(|_, bucket| now.duration_since(bucket.updated_at) < Duration::from_secs(7_200));
        let bucket = buckets
            .entry((action, normalized))
            .or_insert_with(|| Bucket {
                tokens: f64::from(capacity),
                updated_at: now,
            });
        let elapsed = now.duration_since(bucket.updated_at).as_secs_f64();
        bucket.tokens =
            (bucket.tokens + elapsed * f64::from(capacity) / 3_600.0).min(f64::from(capacity));
        bucket.updated_at = now;
        if bucket.tokens < 1.0 {
            return Err(ApiError::new(
                ApiErrorType::RateLimited,
                "Too many magic code attempts for this email",
            ));
        }
        bucket.tokens -= 1.0;
        Ok(())
    }
}

impl DashboardIdentityPolicy for PostgresDashboardIdentityPolicy {
    fn check_send(&self, email: &str) -> Result<(), ApiError> {
        self.check_email(email)?;
        self.consume(MagicCodeAction::Send, email)
    }

    fn check_verify(&self, email: &str) -> Result<(), ApiError> {
        self.check_email(email)?;
        self.consume(MagicCodeAction::Verify, email)
    }

    fn record_verify_failure(&self, email: &str) -> bool {
        let normalized = email.trim().to_ascii_lowercase();
        let now = Instant::now();
        let mut failures = self
            .inner
            .verify_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        failures.retain(|_, (_, at)| now.duration_since(*at) < VERIFY_FAILURE_WINDOW);
        let (count, at) = failures.entry(normalized).or_insert((0, now));
        *count = count.saturating_add(1);
        *at = now;
        *count >= MAX_VERIFY_FAILURES
    }

    fn reset_verify_failures(&self, email: &str) {
        self.inner
            .verify_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&email.trim().to_ascii_lowercase());
    }

    fn allow_signup(&self, email: &str) -> Result<(), ApiError> {
        self.check_email(email)?;
        let snapshot = self
            .inner
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let allowed = signup_allowed(
            snapshot.signup_mode.unwrap_or(self.default_signup_mode),
            &snapshot.allowed_emails,
            self.allowed_emails.as_deref(),
            self.superuser_email.as_deref(),
            email,
        );
        if allowed {
            Ok(())
        } else {
            Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "This email is not allowed to sign up for this Instant deployment.",
            ))
        }
    }
}

/// A configured operator allowlist and the deployment superuser always pass;
/// otherwise the flag mode decides.
fn signup_allowed(
    mode: SignupMode,
    snapshot_allowed: &HashSet<String>,
    operator_allowlist: Option<&HashSet<String>>,
    superuser_email: Option<&str>,
    email: &str,
) -> bool {
    let normalized = email.trim().to_ascii_lowercase();
    if is_superuser_email(superuser_email, &normalized)
        || is_listed_email(operator_allowlist, &normalized)
    {
        return true;
    }
    if operator_allowlist.is_some_and(|emails| !emails.contains(&normalized)) {
        return false;
    }
    match mode {
        SignupMode::Open => true,
        SignupMode::Restricted => snapshot_allowed.contains(&normalized),
        SignupMode::Closed => false,
    }
}

/// The superuser always passes the dashboard allowlist.
pub(crate) fn is_superuser_email(superuser_email: Option<&str>, email: &str) -> bool {
    let email = email.trim();
    superuser_email.is_some_and(|superuser| superuser.eq_ignore_ascii_case(email))
}

/// Whether the email is listed in the operator allowlist. An unset allowlist
/// does not restrict anything.
pub(crate) fn is_listed_email(allowed: Option<&HashSet<String>>, email: &str) -> bool {
    let email = email.trim().to_ascii_lowercase();
    allowed.is_some_and(|emails| emails.contains(&email))
}

/// Shared dashboard sign-in check: the allowlist when configured, plus the
/// deployment superuser.
pub(crate) fn check_dashboard_email(
    allowed: Option<&HashSet<String>>,
    superuser_email: Option<&str>,
    email: &str,
) -> Result<(), ApiError> {
    if is_superuser_email(superuser_email, email) {
        return Ok(());
    }
    check_allowed_email(allowed, email)
}

fn check_allowed_email(allowed: Option<&HashSet<String>>, email: &str) -> Result<(), ApiError> {
    if allowed.is_none_or(|emails| emails.contains(&email.trim().to_ascii_lowercase())) {
        Ok(())
    } else {
        Err(ApiError::new(
            ApiErrorType::PermissionDenied,
            "This email is not allowed to sign in to this dashboard.",
        ))
    }
}

fn apply_signup_flag(snapshot: &mut IdentityPolicySnapshot, value: &Value) {
    // Unknown or missing modes keep the configured default instead of opening signup.
    let mode = value.get("mode").and_then(Value::as_str).map(str::trim);
    snapshot.signup_mode = match mode {
        Some(mode) if mode.eq_ignore_ascii_case("open") => Some(SignupMode::Open),
        Some(mode) if mode.eq_ignore_ascii_case("restricted") => Some(SignupMode::Restricted),
        Some(mode) if mode.eq_ignore_ascii_case("closed") => Some(SignupMode::Closed),
        _ => snapshot.signup_mode,
    };
    snapshot.allowed_emails = value
        .get("allowedEmails")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|email| !email.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
}

#[derive(Debug, thiserror::Error)]
#[error("dashboard identity policy refresh failed: {0}")]
struct PolicyRefreshError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_allowlist_is_case_insensitive_and_defaults_to_upstream_behavior() {
        let emails = HashSet::from(["owner@example.com".to_owned()]);
        assert!(check_allowed_email(None, "other@example.com").is_ok());
        assert!(check_allowed_email(Some(&emails), " Owner@Example.COM ").is_ok());
        assert!(check_allowed_email(Some(&emails), "other@example.com").is_err());
        assert!(check_allowed_email(Some(&HashSet::new()), "owner@example.com").is_err());
    }

    #[test]
    fn shared_dashboard_email_check_honors_allowlist_and_superuser() {
        let emails = HashSet::from(["owner@example.com".to_owned()]);
        assert!(check_dashboard_email(Some(&emails), None, "OWNER@example.com").is_ok());
        assert!(check_dashboard_email(Some(&emails), None, "other@example.com").is_err());
        assert!(
            check_dashboard_email(Some(&emails), Some("root@example.com"), "Root@Example.com")
                .is_ok()
        );
        assert!(
            check_dashboard_email(Some(&emails), Some("root@example.com"), "other@example.com")
                .is_err()
        );
        assert!(check_dashboard_email(None, None, "anyone@example.com").is_ok());
    }

    #[test]
    fn signup_flag_normalizes_modes_and_emails() {
        let mut snapshot = IdentityPolicySnapshot::default();
        apply_signup_flag(
            &mut snapshot,
            &serde_json::json!({
                "mode": "restricted",
                "allowedEmails": [" A@Example.com ", null, ""]
            }),
        );
        assert_eq!(snapshot.signup_mode, Some(SignupMode::Restricted));
        assert_eq!(
            snapshot.allowed_emails,
            HashSet::from(["a@example.com".to_owned()])
        );

        apply_signup_flag(&mut snapshot, &serde_json::json!({"mode": "invalid"}));
        assert_eq!(snapshot.signup_mode, Some(SignupMode::Restricted));

        apply_signup_flag(&mut snapshot, &serde_json::json!({"mode": " Open "}));
        assert_eq!(snapshot.signup_mode, Some(SignupMode::Open));

        let mut closed = IdentityPolicySnapshot::default();
        apply_signup_flag(
            &mut closed,
            &serde_json::json!({"allowedEmails": ["a@example.com"]}),
        );
        // No recognised mode: the operator default decides at check time.
        assert_eq!(closed.signup_mode, None);
    }

    #[test]
    fn signup_helper_enforces_closed_default_and_allowlists() {
        let empty = HashSet::new();
        let restricted = HashSet::from(["member@example.com".to_owned()]);
        let operator = HashSet::from(["owner@example.com".to_owned()]);

        assert!(!signup_allowed(
            SignupMode::Closed,
            &empty,
            None,
            None,
            "new@example.com"
        ));
        assert!(signup_allowed(
            SignupMode::Closed,
            &empty,
            Some(&operator),
            None,
            "Owner@Example.com"
        ));
        assert!(signup_allowed(
            SignupMode::Closed,
            &empty,
            None,
            Some("root@example.com"),
            "ROOT@example.com"
        ));
        assert!(signup_allowed(
            SignupMode::Open,
            &empty,
            None,
            None,
            "new@example.com"
        ));
        assert!(signup_allowed(
            SignupMode::Restricted,
            &restricted,
            None,
            None,
            "Member@Example.com"
        ));
        assert!(!signup_allowed(
            SignupMode::Restricted,
            &restricted,
            None,
            None,
            "new@example.com"
        ));
        assert!(!signup_allowed(
            SignupMode::Open,
            &empty,
            Some(&operator),
            None,
            "new@example.com"
        ));
    }
}
