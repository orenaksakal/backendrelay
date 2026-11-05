use instant_auth::oauth::{fetch_discovery, parse_safe_url};
use instant_http::ApiError;
use serde_json::Value;

use crate::{DashboardControlHookOperation, DashboardControlHooks};

/// Network-backed validation for dashboard OIDC client settings.
pub struct OAuthSettingsHooks<D> {
    inner: D,
}

impl<D> OAuthSettingsHooks<D> {
    #[must_use]
    pub const fn new(inner: D) -> Self {
        Self { inner }
    }
}

impl<D> DashboardControlHooks for OAuthSettingsHooks<D>
where
    D: DashboardControlHooks,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::ValidateOAuthDiscovery { endpoint } => {
                let valid = match parse_safe_url(&endpoint) {
                    Ok(endpoint) => fetch_discovery(&endpoint).await.is_ok(),
                    Err(_) => false,
                };
                Ok(Value::Bool(valid))
            }
            other => self.inner.execute(other).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoopDashboardControlHooks;

    #[tokio::test]
    async fn unsafe_discovery_endpoints_fail_before_network_io() {
        let hooks = OAuthSettingsHooks::new(NoopDashboardControlHooks);
        for endpoint in ["file:///etc/passwd", "http://127.0.0.1/openid"] {
            assert_eq!(
                hooks
                    .execute(DashboardControlHookOperation::ValidateOAuthDiscovery {
                        endpoint: endpoint.to_owned(),
                    })
                    .await
                    .unwrap(),
                Value::Bool(false)
            );
        }
    }
}
