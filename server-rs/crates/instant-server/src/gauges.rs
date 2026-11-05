use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::{ScheduledTask, ServerEnvironment, TaskSchedule};

const GAUGE_INTERVAL: Duration = Duration::from_secs(30);
const GAUGE_TIMEOUT: Duration = Duration::from_secs(5);

pub trait GaugeProvider: Send + Sync + 'static {
    /// Returns the provider's current path/value pairs.
    ///
    /// # Errors
    ///
    /// Returns a provider-local collection failure. The registry converts it
    /// into `instant.gauges.metric-fn-error` and continues collecting.
    fn gauges(&self) -> Result<Vec<(String, Value)>, String>;
}

impl<F> GaugeProvider for F
where
    F: Fn() -> Result<Vec<(String, Value)>, String> + Send + Sync + 'static,
{
    fn gauges(&self) -> Result<Vec<(String, Value)>, String> {
        self()
    }
}

/// Extensible Rust-native replacement for the JVM gauge registry.
#[derive(Default)]
pub struct NativeGaugeRegistry {
    next_id: AtomicU64,
    providers: Mutex<BTreeMap<u64, Arc<dyn GaugeProvider>>>,
}

impl NativeGaugeRegistry {
    #[must_use]
    pub fn register<P>(self: &Arc<Self>, provider: P) -> GaugeRegistration
    where
        P: GaugeProvider,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Arc::new(provider));
        GaugeRegistration {
            registry: Arc::downgrade(self),
            id,
        }
    }

    #[must_use]
    pub fn collect(&self) -> BTreeMap<String, Value> {
        let mut gauges = base_gauges();
        let providers = self
            .providers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for provider in providers {
            match provider.gauges() {
                Ok(values) => gauges.extend(values),
                Err(error) => {
                    gauges.insert("instant.gauges.metric-fn-error".to_owned(), json!(error));
                }
            }
        }
        gauges
    }

    #[must_use]
    pub fn scheduled_task(self: &Arc<Self>, environment: ServerEnvironment) -> ScheduledTask {
        let registry = Arc::clone(self);
        ScheduledTask::new(
            "native-gauges",
            TaskSchedule::Fixed {
                every: GAUGE_INTERVAL,
                start_immediately: true,
            },
            GAUGE_TIMEOUT,
            move || {
                let registry = Arc::clone(&registry);
                async move {
                    let gauges = registry.collect();
                    if environment.is_hosted() {
                        let stdout = io::stdout();
                        write_gauge_event(stdout.lock(), &gauges)?;
                    }
                    Ok::<(), io::Error>(())
                }
            },
        )
    }
}

pub struct GaugeRegistration {
    registry: Weak<NativeGaugeRegistry>,
    id: u64,
}

impl Drop for GaugeRegistration {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .providers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.id);
        }
    }
}

fn write_gauge_event(mut writer: impl Write, gauges: &BTreeMap<String, Value>) -> io::Result<()> {
    serde_json::to_writer(
        &mut writer,
        &json!({
            "name": "gauges",
            "attributes": gauges,
        }),
    )
    .map_err(io::Error::other)?;
    writer.write_all(b"\n")
}

fn base_gauges() -> BTreeMap<String, Value> {
    let mut gauges = BTreeMap::new();
    gauges.insert(
        "instant.gauges.calculated.ms".to_owned(),
        json!(unix_millis()),
    );
    collect_linux_process_gauges(&mut gauges);
    collect_tokio_gauges(&mut gauges);
    gauges
}

fn unix_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn collect_tokio_gauges(gauges: &mut BTreeMap<String, Value>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let metrics = handle.metrics();
    gauges.insert(
        "rust.runtime.worker.count".to_owned(),
        json!(metrics.num_workers()),
    );
    gauges.insert(
        "rust.runtime.task.alive.count".to_owned(),
        json!(metrics.num_alive_tasks()),
    );
    gauges.insert(
        "rust.runtime.queue.global.depth".to_owned(),
        json!(metrics.global_queue_depth()),
    );
    let busy_millis = (0..metrics.num_workers())
        .map(|worker| metrics.worker_total_busy_duration(worker).as_millis())
        .sum::<u128>();
    gauges.insert(
        "rust.runtime.worker.busy.ms".to_owned(),
        json!(u64::try_from(busy_millis).unwrap_or(u64::MAX)),
    );
}

#[cfg(target_os = "linux")]
fn collect_linux_process_gauges(gauges: &mut BTreeMap<String, Value>) {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        collect_status_gauges(&status, gauges);
    }
    if let Ok(process_io) = fs::read_to_string("/proc/self/io") {
        collect_io_gauges(&process_io, gauges);
    }
    if let Ok(entries) = fs::read_dir("/proc/self/fd") {
        gauges.insert(
            "process.fd.open.count".to_owned(),
            json!(entries.filter_map(Result::ok).count()),
        );
    }
    collect_cgroup_value(
        "/sys/fs/cgroup/memory.current",
        "process.cgroup.memory.current.bytes",
        gauges,
    );
    collect_cgroup_value(
        "/sys/fs/cgroup/memory.max",
        "process.cgroup.memory.max.bytes",
        gauges,
    );
}

#[cfg(not(target_os = "linux"))]
fn collect_linux_process_gauges(_gauges: &mut BTreeMap<String, Value>) {}

#[cfg(target_os = "linux")]
fn collect_status_gauges(status: &str, gauges: &mut BTreeMap<String, Value>) {
    for line in status.lines() {
        let Some((name, raw)) = line.split_once(':') else {
            continue;
        };
        let Some(value) = raw.split_ascii_whitespace().next() else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match name {
            "VmRSS" => insert_kib(gauges, "process.memory.resident.bytes", value),
            "VmSize" => insert_kib(gauges, "process.memory.virtual.bytes", value),
            "VmSwap" => insert_kib(gauges, "process.memory.swap.bytes", value),
            "Threads" => {
                gauges.insert("process.thread.count".to_owned(), json!(value));
            }
            "voluntary_ctxt_switches" => {
                gauges.insert(
                    "process.context-switch.voluntary.count".to_owned(),
                    json!(value),
                );
            }
            "nonvoluntary_ctxt_switches" => {
                gauges.insert(
                    "process.context-switch.involuntary.count".to_owned(),
                    json!(value),
                );
            }
            _ => {}
        }
    }
}

#[cfg(target_os = "linux")]
fn insert_kib(gauges: &mut BTreeMap<String, Value>, path: &str, value: u64) {
    gauges.insert(path.to_owned(), json!(value.saturating_mul(1024)));
}

#[cfg(target_os = "linux")]
fn collect_io_gauges(process_io: &str, gauges: &mut BTreeMap<String, Value>) {
    for line in process_io.lines() {
        let Some((name, raw)) = line.split_once(':') else {
            continue;
        };
        let Ok(value) = raw.trim().parse::<u64>() else {
            continue;
        };
        let path = match name {
            "rchar" => Some("process.io.read.requested.bytes"),
            "wchar" => Some("process.io.write.requested.bytes"),
            "read_bytes" => Some("process.io.read.storage.bytes"),
            "write_bytes" => Some("process.io.write.storage.bytes"),
            _ => None,
        };
        if let Some(path) = path {
            gauges.insert(path.to_owned(), json!(value));
        }
    }
}

#[cfg(target_os = "linux")]
fn collect_cgroup_value(file: &str, path: &str, gauges: &mut BTreeMap<String, Value>) {
    let Ok(raw) = fs::read_to_string(file) else {
        return;
    };
    if let Ok(value) = raw.trim().parse::<u64>() {
        gauges.insert(path.to_owned(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_status_and_io_are_converted_to_native_byte_and_count_paths() {
        let mut gauges = BTreeMap::new();
        collect_status_gauges(
            "VmSize:\t100 kB\nVmRSS:\t40 kB\nVmSwap:\t3 kB\nThreads:\t7\n\
             voluntary_ctxt_switches:\t11\nnonvoluntary_ctxt_switches:\t2\n",
            &mut gauges,
        );
        collect_io_gauges(
            "rchar: 10\nwchar: 20\nread_bytes: 30\nwrite_bytes: 40\n",
            &mut gauges,
        );
        assert_eq!(gauges["process.memory.virtual.bytes"], 102_400);
        assert_eq!(gauges["process.memory.resident.bytes"], 40_960);
        assert_eq!(gauges["process.memory.swap.bytes"], 3_072);
        assert_eq!(gauges["process.thread.count"], 7);
        assert_eq!(gauges["process.context-switch.voluntary.count"], 11);
        assert_eq!(gauges["process.context-switch.involuntary.count"], 2);
        assert_eq!(gauges["process.io.read.requested.bytes"], 10);
        assert_eq!(gauges["process.io.write.storage.bytes"], 40);
    }

    #[tokio::test]
    async fn registry_is_extensible_cleans_up_and_emits_the_gauges_event_shape() {
        let registry = Arc::new(NativeGaugeRegistry::default());
        assert_eq!(
            registry.scheduled_task(ServerEnvironment::Staging).name(),
            "native-gauges"
        );
        assert_eq!(GAUGE_INTERVAL, Duration::from_secs(30));
        let registration =
            registry.register(|| Ok(vec![("instant.test.depth".to_owned(), json!(9))]));
        let gauges = registry.collect();
        assert_eq!(gauges["instant.test.depth"], 9);
        assert!(gauges.contains_key("rust.runtime.worker.count"));
        assert!(gauges.contains_key("rust.runtime.task.alive.count"));
        let mut encoded = Vec::new();
        write_gauge_event(&mut encoded, &gauges).unwrap();
        let event: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(event["name"], "gauges");
        assert_eq!(event["attributes"]["instant.test.depth"], 9);

        drop(registration);
        assert!(!registry.collect().contains_key("instant.test.depth"));
    }

    #[tokio::test]
    async fn provider_failures_become_a_gauge_instead_of_stopping_collection() {
        let registry = Arc::new(NativeGaugeRegistry::default());
        let _registration = registry.register(|| Err("broken provider".to_owned()));
        let gauges = registry.collect();
        assert_eq!(gauges["instant.gauges.metric-fn-error"], "broken provider");
        assert!(gauges.contains_key("instant.gauges.calculated.ms"));
    }
}
