//! Per-app cap on concurrent interactive database work.
//!
//! One tenant's query burst must not occupy every interactive connection and
//! push other apps behind it. Permits are created lazily per app; only live
//! holders and waiters keep a semaphore alive, so the map stays bounded without
//! a background reaper.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

/// Small pools, and small apps on large pools, always keep this many
/// concurrent interactive queries before any fair share applies.
const MIN_PERMITS_PER_APP: usize = 4;
/// Dead entries are pruned once the map reaches this many apps.
const SWEEP_THRESHOLD: usize = 1024;

pub(crate) struct AppInteractiveAdmission {
    permits_per_app: usize,
    wait_timeout: Duration,
    apps: Mutex<HashMap<Uuid, Weak<Semaphore>>>,
}

/// One held interactive slot for an app. Dropping it wakes the next waiter.
pub(crate) struct AppInteractivePermit {
    _permit: OwnedSemaphorePermit,
}

impl AppInteractiveAdmission {
    /// Reserves `max(4, interactive_connections / 3)` permits per app so a
    /// single app can never take the whole interactive pool.
    pub(crate) fn new(interactive_connections: usize, wait_timeout: Duration) -> Self {
        Self {
            permits_per_app: (interactive_connections / 3).max(MIN_PERMITS_PER_APP),
            wait_timeout,
            apps: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) const fn permits_per_app(&self) -> usize {
        self.permits_per_app
    }

    /// Waits for this app's slot up to the admission timeout. `None` means the
    /// app stayed over its share and the caller should fail the request.
    pub(crate) async fn acquire(&self, app_id: Uuid) -> Option<AppInteractivePermit> {
        let semaphore = self.semaphore(app_id);
        let permit = tokio::time::timeout(self.wait_timeout, semaphore.acquire_owned())
            .await
            .ok()?
            .ok()?;
        Some(AppInteractivePermit { _permit: permit })
    }

    fn semaphore(&self, app_id: Uuid) -> Arc<Semaphore> {
        let mut apps = self
            .apps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(semaphore) = apps.get(&app_id).and_then(Weak::upgrade) {
            return semaphore;
        }
        // The previous semaphore for this app has no holders and no waiters.
        apps.remove(&app_id);
        if apps.len() >= SWEEP_THRESHOLD {
            apps.retain(|_, semaphore| semaphore.strong_count() > 0);
        }
        let semaphore = Arc::new(Semaphore::new(self.permits_per_app));
        apps.insert(app_id, Arc::downgrade(&semaphore));
        semaphore
    }

    #[cfg(test)]
    fn tracked_apps(&self) -> usize {
        self.apps.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn admission(interactive_connections: usize, wait: Duration) -> AppInteractiveAdmission {
        AppInteractiveAdmission::new(interactive_connections, wait)
    }

    #[test]
    fn a_small_pool_keeps_four_permits_but_a_large_one_keeps_a_third() {
        assert_eq!(admission(6, Duration::from_millis(10)).permits_per_app(), 4);
        assert_eq!(admission(9, Duration::from_millis(10)).permits_per_app(), 4);
        assert_eq!(
            admission(30, Duration::from_millis(10)).permits_per_app(),
            10
        );
    }

    #[tokio::test]
    async fn one_app_is_capped_while_another_app_keeps_its_own_permits() {
        let admission = admission(6, Duration::from_millis(50));
        let busy_app = Uuid::new_v4();
        let other_app = Uuid::new_v4();
        let held = {
            let mut held = Vec::new();
            for _ in 0..4 {
                held.push(admission.acquire(busy_app).await.unwrap());
            }
            held
        };
        let started = Instant::now();
        assert!(admission.acquire(busy_app).await.is_none());
        assert!(started.elapsed() >= Duration::from_millis(50));
        let other = admission.acquire(other_app).await.unwrap();
        drop(other);
        drop(held);
        let next = admission.acquire(busy_app).await.unwrap();
        drop(next);
    }

    #[tokio::test]
    async fn idle_semaphores_are_evicted_when_the_map_reaches_its_threshold() {
        let admission = admission(6, Duration::from_millis(50));
        for index in 0..SWEEP_THRESHOLD {
            drop(admission.acquire(Uuid::from_u128(index as u128)).await);
        }
        assert_eq!(admission.tracked_apps(), SWEEP_THRESHOLD);
        // Acquiring one more app prunes every semaphore nothing retains.
        drop(admission.acquire(Uuid::new_v4()).await);
        assert_eq!(admission.tracked_apps(), 1);
    }

    #[tokio::test]
    async fn live_waiters_keep_the_semaphore_alive_during_a_sweep() {
        let admission = Arc::new(admission(6, Duration::from_secs(1)));
        let app = Uuid::new_v4();
        let held = admission.acquire(app).await.unwrap();
        let waiting = {
            let admission = Arc::clone(&admission);
            tokio::spawn(async move {
                let _permit = admission.acquire(app).await.unwrap();
            })
        };
        tokio::task::yield_now().await;
        for index in 0..SWEEP_THRESHOLD {
            drop(admission.acquire(Uuid::from_u128(index as u128)).await);
        }
        drop(held);
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("sweeping the map dropped a live waiter")
            .unwrap();
    }
}
