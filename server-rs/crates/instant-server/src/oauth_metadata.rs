use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use instant_auth::oauth::{DiscoveryDocument, fetch_discovery, fetch_jwks};
use jsonwebtoken::jwk::JwkSet;
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;

use crate::{ScheduledTask, TaskSchedule};

const CACHE_SIZE: usize = 32;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const FAILURE_LIFETIME: Duration = Duration::from_secs(60);
const JWKS_LIFETIME: Duration = Duration::from_hours(1);
const JWKS_REFRESH_WINDOW: Duration = Duration::from_secs(120);
const GOOGLE_DISCOVERY_ENDPOINT: &str =
    "https://accounts.google.com/.well-known/openid-configuration";
const GOOGLE_JWKS_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v3/certs";

#[derive(Clone, Debug, Error)]
#[error("OAuth metadata fetch failed: {message}")]
pub struct OAuthMetadataFetchError {
    message: String,
}

impl OAuthMetadataFetchError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Clone)]
struct CacheEntry<T> {
    value: T,
    fetched_at: Instant,
    touched: u64,
}

#[derive(Clone)]
struct FetchFailure {
    message: String,
}

struct BoundedCache<T> {
    values: HashMap<String, CacheEntry<T>>,
    clock: u64,
}

impl<T> Default for BoundedCache<T> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            clock: 0,
        }
    }
}

impl<T> BoundedCache<T>
where
    T: Clone,
{
    fn get(&mut self, key: &str) -> Option<CacheEntry<T>> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.values.get_mut(key)?;
        entry.touched = self.clock;
        Some(entry.clone())
    }

    fn insert(&mut self, key: String, value: T, fetched_at: Instant) {
        self.clock = self.clock.wrapping_add(1);
        if !self.values.contains_key(&key)
            && self.values.len() >= CACHE_SIZE
            && let Some(oldest) = self
                .values
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
        {
            self.values.remove(&oldest);
        }
        self.values.insert(
            key,
            CacheEntry {
                value,
                fetched_at,
                touched: self.clock,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        self.values.remove(key);
    }
}

struct CacheState<T> {
    values: BoundedCache<T>,
    failures: BoundedCache<FetchFailure>,
    inflight: HashMap<String, Arc<Mutex<()>>>,
}

impl<T> Default for CacheState<T> {
    fn default() -> Self {
        Self {
            values: BoundedCache::default(),
            failures: BoundedCache::default(),
            inflight: HashMap::new(),
        }
    }
}

/// Bounded discovery and JWKS caches with per-URL single-flight fetches.
pub struct OAuthMetadataCache {
    discovery: Mutex<CacheState<DiscoveryDocument>>,
    jwks: Mutex<CacheState<JwkSet>>,
}

impl Default for OAuthMetadataCache {
    fn default() -> Self {
        Self {
            discovery: Mutex::new(CacheState::default()),
            jwks: Mutex::new(CacheState::default()),
        }
    }
}

impl OAuthMetadataCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns cached discovery metadata, fetching and validating it on first use.
    ///
    /// # Errors
    ///
    /// Returns the SSRF-safe discovery transport or validation error.
    pub async fn discovery(
        &self,
        endpoint: &Url,
    ) -> Result<DiscoveryDocument, OAuthMetadataFetchError> {
        let key = endpoint.as_str();
        fetch_single_flight(
            &self.discovery,
            key,
            |_| true,
            || async {
                fetch_discovery(endpoint)
                    .await
                    .map_err(|error| OAuthMetadataFetchError::new(error.to_string()))
            },
        )
        .await
    }

    /// Returns cached JWKS, using the JVM's one-hour fallback lifetime.
    ///
    /// # Errors
    ///
    /// Returns the bounded SSRF-safe JWKS transport or parsing error.
    pub async fn jwks(
        &self,
        endpoint: &Url,
        force_refresh: bool,
    ) -> Result<JwkSet, OAuthMetadataFetchError> {
        let key = endpoint.as_str();
        fetch_single_flight(
            &self.jwks,
            key,
            |entry| !force_refresh && entry.fetched_at.elapsed() < JWKS_LIFETIME,
            || async {
                fetch_jwks(endpoint)
                    .await
                    .map_err(|error| OAuthMetadataFetchError::new(error.to_string()))
            },
        )
        .await
    }

    pub(crate) async fn refresh_jwks_on_missing_key(&self, endpoint: &Url) -> bool {
        self.jwks
            .lock()
            .await
            .values
            .get(endpoint.as_str())
            .is_some_and(|entry| {
                entry.fetched_at.elapsed() >= JWKS_LIFETIME.saturating_sub(JWKS_REFRESH_WINDOW)
            })
    }

    /// Refreshes every discovery endpoint already observed by this process.
    /// Individual failures do not discard the last known-good document.
    pub async fn refresh_discovery(&self) -> OAuthMetadataRefreshReport {
        self.refresh_discovery_endpoints(self.discovery_endpoints(false).await)
            .await
    }

    async fn refresh_discovery_endpoints(
        &self,
        endpoints: Vec<String>,
    ) -> OAuthMetadataRefreshReport {
        let mut report = OAuthMetadataRefreshReport::default();
        for endpoint in endpoints {
            if self.refresh_discovery_endpoint(endpoint).await {
                report.refreshed += 1;
            } else {
                report.failed += 1;
            }
        }
        report
    }

    #[tracing::instrument(
        name = "oauth/updating-discovery-endpoint",
        skip_all,
        fields(endpoint = %endpoint)
    )]
    async fn refresh_discovery_endpoint(&self, endpoint: String) -> bool {
        let Ok(url) = Url::parse(&endpoint) else {
            return false;
        };
        let Ok(Ok(value)) = tokio::time::timeout(FETCH_TIMEOUT, fetch_discovery(&url)).await else {
            return false;
        };
        let mut state = self.discovery.lock().await;
        state.values.insert(endpoint.clone(), value, Instant::now());
        state.failures.remove(&endpoint);
        true
    }

    /// Refreshes every JWKS endpoint already observed by this process.
    /// Individual failures do not discard the last known-good key set.
    pub async fn refresh_jwks(&self) -> OAuthMetadataRefreshReport {
        self.refresh_jwks_endpoints(self.jwks_endpoints(false).await)
            .await
    }

    async fn refresh_jwks_endpoints(&self, endpoints: Vec<String>) -> OAuthMetadataRefreshReport {
        let mut report = OAuthMetadataRefreshReport::default();
        for endpoint in endpoints {
            if self.refresh_jwks_endpoint(endpoint).await {
                report.refreshed += 1;
            } else {
                report.failed += 1;
            }
        }
        report
    }

    #[tracing::instrument(name = "jwk/updating-certs", skip_all, fields(endpoint = %endpoint))]
    async fn refresh_jwks_endpoint(&self, endpoint: String) -> bool {
        let Ok(url) = Url::parse(&endpoint) else {
            return false;
        };
        let Ok(Ok(value)) = tokio::time::timeout(FETCH_TIMEOUT, fetch_jwks(&url)).await else {
            return false;
        };
        let mut state = self.jwks.lock().await;
        state.values.insert(endpoint.clone(), value, Instant::now());
        state.failures.remove(&endpoint);
        true
    }

    async fn discovery_endpoints(&self, include_google: bool) -> Vec<String> {
        let state = self.discovery.lock().await;
        let mut endpoints = state.values.values.keys().cloned().collect::<Vec<_>>();
        drop(state);
        if include_google
            && !endpoints
                .iter()
                .any(|value| value == GOOGLE_DISCOVERY_ENDPOINT)
        {
            endpoints.push(GOOGLE_DISCOVERY_ENDPOINT.to_owned());
        }
        endpoints
    }

    async fn jwks_endpoints(&self, include_google: bool) -> Vec<String> {
        let state = self.jwks.lock().await;
        let mut endpoints = state.values.values.keys().cloned().collect::<Vec<_>>();
        drop(state);
        if include_google && !endpoints.iter().any(|value| value == GOOGLE_JWKS_ENDPOINT) {
            endpoints.push(GOOGLE_JWKS_ENDPOINT.to_owned());
        }
        endpoints
    }

    async fn refresh_with_google_defaults(&self) -> OAuthMetadataRefreshReport {
        let mut report = self
            .refresh_discovery_endpoints(self.discovery_endpoints(true).await)
            .await;
        let jwks = self
            .refresh_jwks_endpoints(self.jwks_endpoints(true).await)
            .await;
        report.refreshed += jwks.refreshed;
        report.failed += jwks.failed;
        report
    }

    /// JVM-compatible Google warm-up and hourly discovery/JWKS refresh worker.
    #[must_use]
    pub fn refresh_task(cache: Arc<Self>) -> ScheduledTask {
        ScheduledTask::new(
            "oauth-metadata-refresh",
            TaskSchedule::Fixed {
                every: Duration::from_hours(1),
                start_immediately: true,
            },
            Duration::from_mins(10),
            move || {
                let cache = Arc::clone(&cache);
                async move {
                    let report = cache.refresh_with_google_defaults().await;
                    if report.failed == 0 {
                        Ok(())
                    } else {
                        Err(OAuthMetadataRefreshError(report.failed))
                    }
                }
            },
        )
    }
}

async fn fetch_single_flight<T, F, Fut>(
    cache: &Mutex<CacheState<T>>,
    key: &str,
    accept: impl Fn(&CacheEntry<T>) -> bool,
    fetch: F,
) -> Result<T, OAuthMetadataFetchError>
where
    T: Clone,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, OAuthMetadataFetchError>>,
{
    let gate = {
        let mut state = cache.lock().await;
        if let Some(entry) = state.values.get(key)
            && accept(&entry)
        {
            return Ok(entry.value);
        }
        if let Some(failure) = state.failures.get(key)
            && failure.fetched_at.elapsed() < FAILURE_LIFETIME
        {
            return Err(OAuthMetadataFetchError::new(failure.value.message));
        }
        state.failures.remove(key);
        state
            .inflight
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    };
    let _gate = gate.lock().await;
    {
        let mut state = cache.lock().await;
        if let Some(entry) = state.values.get(key)
            && accept(&entry)
        {
            return Ok(entry.value);
        }
        if let Some(failure) = state.failures.get(key)
            && failure.fetched_at.elapsed() < FAILURE_LIFETIME
        {
            return Err(OAuthMetadataFetchError::new(failure.value.message));
        }
        state.failures.remove(key);
    }
    let result = tokio::time::timeout(FETCH_TIMEOUT, fetch()).await;
    let mut state = cache.lock().await;
    state.inflight.remove(key);
    match result {
        Ok(Ok(value)) => {
            state.failures.remove(key);
            state
                .values
                .insert(key.to_owned(), value.clone(), Instant::now());
            Ok(value)
        }
        Ok(Err(error)) => {
            state.failures.insert(
                key.to_owned(),
                FetchFailure {
                    message: error.message.clone(),
                },
                Instant::now(),
            );
            Err(error)
        }
        Err(_) => {
            let error = OAuthMetadataFetchError::new(format!(
                "timed out after {} seconds",
                FETCH_TIMEOUT.as_secs()
            ));
            state.failures.insert(
                key.to_owned(),
                FetchFailure {
                    message: error.message.clone(),
                },
                Instant::now(),
            );
            Err(error)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OAuthMetadataRefreshReport {
    pub refreshed: usize,
    pub failed: usize,
}

#[derive(Debug, Error)]
#[error("{0} OAuth metadata endpoints failed to refresh")]
pub struct OAuthMetadataRefreshError(usize);

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use serde_json::json;

    fn document(issuer: &str) -> DiscoveryDocument {
        serde_json::from_value(json!({"issuer": issuer})).unwrap()
    }

    #[test]
    fn bounded_cache_is_lru_and_retains_exact_capacity() {
        let mut cache = BoundedCache::default();
        for index in 0..CACHE_SIZE {
            cache.insert(
                format!("key-{index}"),
                document(&format!("issuer-{index}")),
                Instant::now(),
            );
        }
        cache.get("key-0");
        cache.insert("new".to_owned(), document("new"), Instant::now());
        assert_eq!(cache.values.len(), CACHE_SIZE);
        assert!(cache.values.contains_key("key-0"));
        assert!(!cache.values.contains_key("key-1"));
    }

    #[tokio::test]
    async fn empty_refresh_is_a_successful_noop() {
        let cache = OAuthMetadataCache::new();
        assert_eq!(
            cache.refresh_discovery().await,
            OAuthMetadataRefreshReport::default()
        );
        assert_eq!(
            cache.refresh_jwks().await,
            OAuthMetadataRefreshReport::default()
        );
    }

    #[tokio::test]
    async fn refresh_failures_preserve_last_known_good_metadata() {
        let cache = OAuthMetadataCache::new();
        let discovery = document("retained");
        let keys: JwkSet = serde_json::from_value(json!({"keys": []})).unwrap();
        cache.discovery.lock().await.values.insert(
            "not a discovery URL".to_owned(),
            discovery.clone(),
            Instant::now(),
        );
        cache.jwks.lock().await.values.insert(
            "not a JWKS URL".to_owned(),
            keys.clone(),
            Instant::now(),
        );

        assert_eq!(
            cache.refresh_discovery().await,
            OAuthMetadataRefreshReport {
                refreshed: 0,
                failed: 1
            }
        );
        assert_eq!(
            cache.refresh_jwks().await,
            OAuthMetadataRefreshReport {
                refreshed: 0,
                failed: 1
            }
        );
        assert_eq!(
            cache
                .discovery
                .lock()
                .await
                .values
                .get("not a discovery URL")
                .unwrap()
                .value,
            discovery
        );
        assert_eq!(
            cache
                .jwks
                .lock()
                .await
                .values
                .get("not a JWKS URL")
                .unwrap()
                .value
                .keys
                .len(),
            keys.keys.len()
        );
    }

    #[test]
    fn refresh_task_owns_the_combined_metadata_lifecycle() {
        let task = OAuthMetadataCache::refresh_task(Arc::new(OAuthMetadataCache::new()));
        assert_eq!(task.name(), "oauth-metadata-refresh");
    }

    #[tokio::test]
    async fn missing_key_refresh_matches_the_jvm_two_minute_window() {
        let cache = OAuthMetadataCache::new();
        let endpoint = Url::parse("https://issuer.example/keys").unwrap();
        let keys: JwkSet = serde_json::from_value(json!({"keys": []})).unwrap();
        let boundary = JWKS_LIFETIME.checked_sub(JWKS_REFRESH_WINDOW).unwrap();
        cache.jwks.lock().await.values.insert(
            endpoint.to_string(),
            keys.clone(),
            Instant::now()
                .checked_sub(boundary.checked_sub(Duration::from_secs(1)).unwrap())
                .unwrap(),
        );
        assert!(!cache.refresh_jwks_on_missing_key(&endpoint).await);
        cache.jwks.lock().await.values.insert(
            endpoint.to_string(),
            keys,
            Instant::now()
                .checked_sub(boundary + Duration::from_secs(1))
                .unwrap(),
        );
        assert!(cache.refresh_jwks_on_missing_key(&endpoint).await);
    }

    #[tokio::test]
    async fn slow_fetch_for_one_endpoint_does_not_block_another() {
        let cache = Arc::new(Mutex::new(CacheState::<usize>::default()));
        let slow_cache = Arc::clone(&cache);
        let slow = tokio::spawn(async move {
            fetch_single_flight(
                &slow_cache,
                "slow",
                |_| true,
                || async {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(1)
                },
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let started = Instant::now();
        let fast = fetch_single_flight(&cache, "fast", |_| true, || async { Ok(2) })
            .await
            .unwrap();
        assert_eq!(fast, 2);
        assert!(started.elapsed() < Duration::from_millis(250));
        assert_eq!(slow.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn failures_are_cached_for_sixty_seconds() {
        let cache = Mutex::new(CacheState::<usize>::default());
        let attempts = Arc::new(AtomicUsize::new(0));
        let first_attempts = Arc::clone(&attempts);
        let first = fetch_single_flight(
            &cache,
            "endpoint",
            |_| true,
            move || {
                first_attempts.fetch_add(1, Ordering::SeqCst);
                async { Err(OAuthMetadataFetchError::new("boom")) }
            },
        )
        .await;
        assert_eq!(
            first.unwrap_err().to_string(),
            "OAuth metadata fetch failed: boom"
        );
        let second_attempts = Arc::clone(&attempts);
        let second = fetch_single_flight(
            &cache,
            "endpoint",
            |_| true,
            move || {
                second_attempts.fetch_add(1, Ordering::SeqCst);
                async { Ok(2) }
            },
        )
        .await;
        assert!(second.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
