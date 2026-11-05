use std::any::Any;
use std::error::Error;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Days, NaiveTime, TimeZone as _, Utc};
use chrono_tz::Tz;
use futures_util::FutureExt as _;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::metrics::{self, TaskOutcome};

type TaskFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type TaskFn = Arc<dyn Fn() -> TaskFuture + Send + Sync>;

const TASK_EVENT_BUFFER_CAPACITY: usize = 128;
const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 1_024;
const TRUNCATED_DIAGNOSTIC_SUFFIX: &str = " [truncated]";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskSchedule {
    Fixed {
        every: Duration,
        start_immediately: bool,
    },
    Daily {
        timezone: Tz,
        hour: u32,
        minute: u32,
    },
}

impl TaskSchedule {
    /// Computes the next daily execution instant, preserving local wall time
    /// across daylight-saving transitions.
    #[must_use]
    pub fn next_daily_run(self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let Self::Daily {
            timezone,
            hour,
            minute,
        } = self
        else {
            return None;
        };
        let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
        let local_now = now.with_timezone(&timezone);
        for day_offset in 0..=2 {
            let date = local_now
                .date_naive()
                .checked_add_days(Days::new(day_offset))?;
            let local = date.and_time(time);
            let candidate = match timezone.from_local_datetime(&local) {
                chrono::LocalResult::Single(value) => value,
                chrono::LocalResult::Ambiguous(first, second) => first.min(second),
                chrono::LocalResult::None => continue,
            };
            let candidate = candidate.with_timezone(&Utc);
            if candidate > now {
                return Some(candidate);
            }
        }
        None
    }
}

/// A supervised task. Names are trusted, stable diagnostic identifiers, not
/// user-provided data.
#[derive(Clone)]
pub struct ScheduledTask {
    name: Arc<str>,
    schedule: TaskSchedule,
    timeout: Option<Duration>,
    run: TaskFn,
}

impl ScheduledTask {
    /// Builds a supervised task whose errors are reported without terminating
    /// its schedule.
    #[must_use]
    pub fn new<F, Fut, E>(
        name: impl Into<Arc<str>>,
        schedule: TaskSchedule,
        timeout: Duration,
        run: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Error + Send + Sync + 'static,
    {
        Self {
            name: bounded_task_name(name),
            schedule,
            timeout: Some(timeout),
            run: Arc::new(move || {
                let future = run();
                Box::pin(async move { future.await.map_err(|error| error.to_string()) })
            }),
        }
    }

    /// Builds supervised periodic work without a per-run deadline. Process
    /// cancellation still drops the active run immediately. This matches JVM
    /// daemons whose bounded database loop is itself the termination control.
    #[must_use]
    pub fn new_unbounded<F, Fut, E>(
        name: impl Into<Arc<str>>,
        schedule: TaskSchedule,
        run: F,
    ) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Error + Send + Sync + 'static,
    {
        Self {
            name: bounded_task_name(name),
            schedule,
            timeout: None,
            run: Arc::new(move || {
                let future = run();
                Box::pin(async move { future.await.map_err(|error| error.to_string()) })
            }),
        }
    }

    /// Builds a supervised persistent task whose future runs until process cancellation.
    /// Unlike periodic work, it has no per-run timeout; shutdown still drops the future
    /// immediately through the supervisor cancellation token.
    #[must_use]
    pub fn new_persistent<F, Fut, E>(name: impl Into<Arc<str>>, run: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: Error + Send + Sync + 'static,
    {
        Self {
            name: bounded_task_name(name),
            schedule: TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: true,
            },
            timeout: None,
            run: Arc::new(move || {
                let future = run();
                Box::pin(async move { future.await.map_err(|error| error.to_string()) })
            }),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEvent {
    pub task: Arc<str>,
    pub kind: TaskEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskEventKind {
    Succeeded,
    /// A UTF-8-safe diagnostic message capped at 1,024 bytes. Truncated
    /// messages end with ` [truncated]`.
    Failed(String),
    TimedOut,
    /// A UTF-8-safe diagnostic message capped at 1,024 bytes. Truncated
    /// messages end with ` [truncated]`.
    Panicked(String),
}

/// Counters for one configured scheduled task. They remain available even when
/// the bounded recent-event buffer discards an event.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaskDiagnosticCounters {
    pub succeeded: u64,
    pub failed: u64,
    pub timed_out: u64,
    pub panicked: u64,
    pub discarded_succeeded: u64,
    pub discarded_failed: u64,
    pub discarded_timed_out: u64,
    pub discarded_panicked: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDiagnostics {
    pub task: Arc<str>,
    pub counters: TaskDiagnosticCounters,
}

struct TaskDiagnosticCountersAtomic {
    task: Arc<str>,
    succeeded: AtomicU64,
    failed: AtomicU64,
    timed_out: AtomicU64,
    panicked: AtomicU64,
    discarded_succeeded: AtomicU64,
    discarded_failed: AtomicU64,
    discarded_timed_out: AtomicU64,
    discarded_panicked: AtomicU64,
}

impl TaskDiagnosticCountersAtomic {
    fn new(task: Arc<str>) -> Self {
        Self {
            task,
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            panicked: AtomicU64::new(0),
            discarded_succeeded: AtomicU64::new(0),
            discarded_failed: AtomicU64::new(0),
            discarded_timed_out: AtomicU64::new(0),
            discarded_panicked: AtomicU64::new(0),
        }
    }

    fn record(&self, kind: &TaskEventKind) {
        counter_for_kind(self, kind, false).fetch_add(1, Ordering::Relaxed);
    }

    fn record_discarded(&self, kind: &TaskEventKind) {
        counter_for_kind(self, kind, true).fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> TaskDiagnostics {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        TaskDiagnostics {
            task: self.task.clone(),
            counters: TaskDiagnosticCounters {
                succeeded: load(&self.succeeded),
                failed: load(&self.failed),
                timed_out: load(&self.timed_out),
                panicked: load(&self.panicked),
                discarded_succeeded: load(&self.discarded_succeeded),
                discarded_failed: load(&self.discarded_failed),
                discarded_timed_out: load(&self.discarded_timed_out),
                discarded_panicked: load(&self.discarded_panicked),
            },
        }
    }
}

fn counter_for_kind<'a>(
    counters: &'a TaskDiagnosticCountersAtomic,
    kind: &TaskEventKind,
    discarded: bool,
) -> &'a AtomicU64 {
    match (kind, discarded) {
        (TaskEventKind::Succeeded, false) => &counters.succeeded,
        (TaskEventKind::Failed(_), false) => &counters.failed,
        (TaskEventKind::TimedOut, false) => &counters.timed_out,
        (TaskEventKind::Panicked(_), false) => &counters.panicked,
        (TaskEventKind::Succeeded, true) => &counters.discarded_succeeded,
        (TaskEventKind::Failed(_), true) => &counters.discarded_failed,
        (TaskEventKind::TimedOut, true) => &counters.discarded_timed_out,
        (TaskEventKind::Panicked(_), true) => &counters.discarded_panicked,
    }
}

struct DiagnosticEvents {
    capacity: usize,
    events: Mutex<std::collections::VecDeque<QueuedTaskEvent>>,
    active_senders: AtomicUsize,
    available: Notify,
}

impl DiagnosticEvents {
    fn new(capacity: usize, active_senders: usize) -> Self {
        Self {
            capacity,
            events: Mutex::new(std::collections::VecDeque::with_capacity(capacity)),
            active_senders: AtomicUsize::new(active_senders),
            available: Notify::new(),
        }
    }

    fn send(&self, event: TaskEvent, counters: Arc<TaskDiagnosticCountersAtomic>) {
        let Ok(mut events) = self.events.try_lock() else {
            counters.record_discarded(&event.kind);
            return;
        };
        if events.len() == self.capacity {
            if matches!(event.kind, TaskEventKind::Succeeded) {
                counters.record_discarded(&event.kind);
                return;
            }
            if let Some(index) = events
                .iter()
                .position(|queued| matches!(queued.event.kind, TaskEventKind::Succeeded))
            {
                if let Some(discarded) = events.remove(index) {
                    discarded.counters.record_discarded(&discarded.event.kind);
                }
            } else {
                counters.record_discarded(&event.kind);
                return;
            }
        }
        events.push_back(QueuedTaskEvent { event, counters });
        drop(events);
        self.available.notify_one();
    }

    async fn recv(&self) -> Option<TaskEvent> {
        loop {
            let notified = self.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(event) = self.events.lock().ok()?.pop_front() {
                return Some(event.event);
            }
            if self.active_senders.load(Ordering::Acquire) == 0 {
                return None;
            }
            notified.await;
        }
    }
}

struct QueuedTaskEvent {
    event: TaskEvent,
    counters: Arc<TaskDiagnosticCountersAtomic>,
}

struct EventSender {
    events: Arc<DiagnosticEvents>,
}

impl Drop for EventSender {
    fn drop(&mut self) {
        self.events.active_senders.fetch_sub(1, Ordering::Release);
        self.events.available.notify_waiters();
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShutdownReport {
    pub completed: usize,
    pub aborted: usize,
}

/// JVM-derived schedule inventory for native background workers.
pub struct JvmTaskSchedules;

impl JvmTaskSchedules {
    pub const INDEXING_RECOVERY_PRODUCTION: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(60),
        start_immediately: true,
    };
    pub const INDEXING_RECOVERY_NON_PRODUCTION: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(600),
        start_immediately: true,
    };
    pub const BACKUP_RECOVERY: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(600),
        start_immediately: true,
    };
    pub const WEBHOOK_KICK_PRODUCTION: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(120),
        start_immediately: true,
    };
    pub const WEBHOOK_KICK_DEVELOPMENT: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(600),
        start_immediately: true,
    };
    pub const HOURLY_SWEEP: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(3_600),
        start_immediately: true,
    };
    pub const WAL_LOG_TRUNCATION: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(3_600),
        start_immediately: false,
    };
    pub const REPLICATION_SLOT_CLEANUP: TaskSchedule = Self::HOURLY_SWEEP;
    pub const STORAGE_SWEEP: TaskSchedule = Self::HOURLY_SWEEP;
    pub const RATE_LIMIT_KEY_SWEEP: TaskSchedule = Self::HOURLY_SWEEP;
    pub const CUSTODIAN_REAPER: TaskSchedule = Self::HOURLY_SWEEP;
    pub const CUSTODIAN_LAG_SAMPLE: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_secs(60),
        start_immediately: true,
    };
    pub const TRIPLES_SIZE_COLLECTION_HOSTED: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_mins(5),
        start_immediately: true,
    };
    pub const TRIPLES_SIZE_COLLECTION_DEVELOPMENT: TaskSchedule = TaskSchedule::Fixed {
        every: Duration::from_hours(1),
        start_immediately: true,
    };
    pub const EPHEMERAL_APP_SWEEP: TaskSchedule = TaskSchedule::Daily {
        timezone: chrono_tz::US::Eastern,
        hour: 5,
        minute: 0,
    };
    pub const HARD_DELETION_SWEEP: TaskSchedule = TaskSchedule::Daily {
        timezone: chrono_tz::US::Pacific,
        hour: 10,
        minute: 0,
    };
    pub const DAILY_BACKUP: TaskSchedule = TaskSchedule::Daily {
        timezone: chrono_tz::US::Pacific,
        hour: 20,
        minute: 0,
    };
    pub const DAILY_METRICS: TaskSchedule = TaskSchedule::Daily {
        timezone: chrono_tz::US::Pacific,
        hour: 9,
        minute: 0,
    };
    pub const HISTORY_TRUNCATION: TaskSchedule = TaskSchedule::Daily {
        timezone: chrono_tz::US::Pacific,
        hour: 16,
        minute: 0,
    };
    pub const SENDER_DOMAIN_RECHECK: TaskSchedule = TaskSchedule::Daily {
        timezone: chrono_tz::US::Pacific,
        hour: 3,
        minute: 0,
    };
}

/// Owns periodic tasks and coordinates cancellation plus bounded shutdown.
pub struct BackgroundSupervisor {
    cancellation: CancellationToken,
    handles: Vec<JoinHandle<()>>,
    events: Arc<DiagnosticEvents>,
    diagnostics: Vec<Arc<TaskDiagnosticCountersAtomic>>,
}

impl BackgroundSupervisor {
    #[must_use]
    pub fn start(tasks: impl IntoIterator<Item = ScheduledTask>) -> Self {
        let cancellation = CancellationToken::new();
        let tasks = tasks
            .into_iter()
            .map(|task| {
                let diagnostics = Arc::new(TaskDiagnosticCountersAtomic::new(task.name.clone()));
                (task, diagnostics)
            })
            .collect::<Vec<_>>();
        let events = Arc::new(DiagnosticEvents::new(
            TASK_EVENT_BUFFER_CAPACITY,
            tasks.len(),
        ));
        let diagnostics = tasks
            .iter()
            .map(|(_, diagnostics)| Arc::clone(diagnostics))
            .collect();
        let handles = tasks
            .into_iter()
            .map(|(task, diagnostics)| {
                let sender = EventSender {
                    events: Arc::clone(&events),
                };
                tokio::spawn(run_scheduled_task(
                    task,
                    cancellation.child_token(),
                    Arc::clone(&events),
                    diagnostics,
                    sender,
                ))
            })
            .collect();
        Self {
            cancellation,
            handles,
            events,
            diagnostics,
        }
    }

    pub async fn next_event(&mut self) -> Option<TaskEvent> {
        self.events.recv().await
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<TaskDiagnostics> {
        self.diagnostics
            .iter()
            .map(|counter| counter.snapshot())
            .collect()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn shutdown(&mut self, grace: Duration) -> ShutdownReport {
        self.cancel();
        let deadline = Instant::now() + grace;
        let mut report = ShutdownReport::default();
        for mut handle in self.handles.drain(..) {
            if tokio::time::timeout_at(deadline, &mut handle).await.is_ok() {
                report.completed += 1;
            } else {
                handle.abort();
                report.aborted += 1;
            }
        }
        report
    }
}

impl Drop for BackgroundSupervisor {
    fn drop(&mut self) {
        self.cancellation.cancel();
        for handle in &self.handles {
            handle.abort();
        }
    }
}

async fn run_scheduled_task(
    task: ScheduledTask,
    cancellation: CancellationToken,
    events: Arc<DiagnosticEvents>,
    diagnostics: Arc<TaskDiagnosticCountersAtomic>,
    _sender: EventSender,
) {
    match task.schedule {
        TaskSchedule::Fixed {
            every,
            start_immediately,
        } => {
            let every = every.max(Duration::from_millis(1));
            let start = Instant::now()
                + if start_immediately {
                    Duration::ZERO
                } else {
                    every
                };
            let mut interval = tokio::time::interval_at(start, every);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => return,
                    _ = interval.tick() => {}
                }
                if !execute_task(&task, &cancellation, &events, &diagnostics).await {
                    return;
                }
            }
        }
        TaskSchedule::Daily { .. } => loop {
            let Some(next) = task.schedule.next_daily_run(Utc::now()) else {
                send_event(
                    &events,
                    &diagnostics,
                    &task,
                    TaskEventKind::Failed("invalid daily schedule".to_owned()),
                );
                return;
            };
            let delay = (next - Utc::now()).to_std().unwrap_or(Duration::ZERO);
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            if !execute_task(&task, &cancellation, &events, &diagnostics).await {
                return;
            }
        },
    }
}

async fn execute_task(
    task: &ScheduledTask,
    cancellation: &CancellationToken,
    events: &DiagnosticEvents,
    diagnostics: &Arc<TaskDiagnosticCountersAtomic>,
) -> bool {
    let future = match catch_unwind(AssertUnwindSafe(|| (task.run)())) {
        Ok(future) => future,
        Err(payload) => {
            send_event(
                events,
                diagnostics,
                task,
                TaskEventKind::Panicked(panic_message(payload.as_ref())),
            );
            return true;
        }
    };
    let future = AssertUnwindSafe(future).catch_unwind();
    let outcome = if let Some(timeout) = task.timeout {
        tokio::select! {
            () = cancellation.cancelled() => return false,
            outcome = tokio::time::timeout(timeout.max(Duration::from_millis(1)), future) => outcome,
        }
    } else {
        tokio::select! {
            () = cancellation.cancelled() => return false,
            outcome = future => Ok(outcome),
        }
    };
    let kind = match outcome {
        Err(_) => TaskEventKind::TimedOut,
        Ok(Err(payload)) => TaskEventKind::Panicked(panic_message(payload.as_ref())),
        Ok(Ok(Err(error))) => TaskEventKind::Failed(error),
        Ok(Ok(Ok(()))) => TaskEventKind::Succeeded,
    };
    send_event(events, diagnostics, task, kind);
    true
}

fn send_event(
    events: &DiagnosticEvents,
    diagnostics: &Arc<TaskDiagnosticCountersAtomic>,
    task: &ScheduledTask,
    kind: TaskEventKind,
) {
    diagnostics.record(&kind);
    let kind = bounded_task_event_kind(kind);
    let outcome = match &kind {
        TaskEventKind::Succeeded => TaskOutcome::Success,
        TaskEventKind::Failed(_) => TaskOutcome::Failure,
        TaskEventKind::TimedOut => TaskOutcome::Timeout,
        TaskEventKind::Panicked(_) => TaskOutcome::Panic,
    };
    metrics::record_task_run(task.name.as_ref(), outcome);
    match &kind {
        TaskEventKind::Succeeded => {}
        TaskEventKind::Failed(message) => {
            tracing::error!(
                task = %task.name,
                outcome = "failure",
                error = %crate::db_error::scrub_database_error(message),
                "scheduled task failed"
            );
        }
        TaskEventKind::TimedOut => tracing::error!(
            task = %task.name,
            outcome = "timeout",
            "scheduled task timed out"
        ),
        TaskEventKind::Panicked(message) => {
            tracing::error!(
                task = %task.name,
                outcome = "panic",
                error = %crate::db_error::scrub_database_error(message),
                "scheduled task panicked"
            );
        }
    }
    events.send(
        TaskEvent {
            task: task.name.clone(),
            kind,
        },
        Arc::clone(diagnostics),
    );
}

fn bounded_task_name(name: impl Into<Arc<str>>) -> Arc<str> {
    Arc::from(bounded_diagnostic_message(name.into().as_ref()))
}

fn bounded_task_event_kind(kind: TaskEventKind) -> TaskEventKind {
    match kind {
        TaskEventKind::Failed(message) => {
            TaskEventKind::Failed(bounded_diagnostic_message(&message))
        }
        TaskEventKind::Panicked(message) => {
            TaskEventKind::Panicked(bounded_diagnostic_message(&message))
        }
        kind => kind,
    }
}

// Always allocate a new string so an oversized source allocation is not
// retained by the bounded event buffer.
fn bounded_diagnostic_message(message: &str) -> String {
    if message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_DIAGNOSTIC_MESSAGE_BYTES - TRUNCATED_DIAGNOSTIC_SUFFIX.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = String::with_capacity(MAX_DIAGNOSTIC_MESSAGE_BYTES);
    bounded.push_str(&message[..end]);
    bounded.push_str(TRUNCATED_DIAGNOSTIC_SUFFIX);
    bounded
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic".to_owned())
        },
        |message| (*message).to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::Notify;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedLogs {
        type Writer = LogBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            LogBuffer(Arc::clone(&self.0))
        }
    }

    #[tokio::test]
    async fn failures_and_panics_are_isolated_from_later_runs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let task_calls = calls.clone();
        let task = ScheduledTask::new(
            "recoverable",
            TaskSchedule::Fixed {
                every: Duration::from_millis(5),
                start_immediately: true,
            },
            Duration::from_secs(1),
            move || {
                let call = task_calls.fetch_add(1, Ordering::Relaxed);
                async move {
                    match call {
                        0 => Err(io::Error::other("first failure")),
                        1 => panic!("second panic"),
                        _ => Ok(()),
                    }
                }
            },
        );
        let mut supervisor = BackgroundSupervisor::start([task]);
        assert!(matches!(
            supervisor.next_event().await.unwrap().kind,
            TaskEventKind::Failed(_)
        ));
        assert!(matches!(
            supervisor.next_event().await.unwrap().kind,
            TaskEventKind::Panicked(_)
        ));
        assert_eq!(
            supervisor.next_event().await.unwrap().kind,
            TaskEventKind::Succeeded
        );
        assert_eq!(
            supervisor.shutdown(Duration::from_secs(1)).await,
            ShutdownReport {
                completed: 1,
                aborted: 0
            }
        );
    }

    #[tokio::test]
    async fn timeout_is_reported_and_shutdown_cancels_active_work() {
        let task = ScheduledTask::new(
            "slow",
            TaskSchedule::Fixed {
                every: Duration::from_secs(60),
                start_immediately: true,
            },
            Duration::from_millis(5),
            || async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok::<_, io::Error>(())
            },
        );
        let mut supervisor = BackgroundSupervisor::start([task]);
        assert_eq!(
            supervisor.next_event().await.unwrap().kind,
            TaskEventKind::TimedOut
        );
        let report = supervisor.shutdown(Duration::from_secs(1)).await;
        assert_eq!(report.completed, 1);
        assert_eq!(report.aborted, 0);
    }

    #[tokio::test]
    async fn saturated_success_events_are_discarded_but_failures_remain_visible() {
        let task = ScheduledTask::new_unbounded(
            "diagnostic-buffer",
            TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: false,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let counters = Arc::new(TaskDiagnosticCountersAtomic::new(task.name.clone()));
        let events = DiagnosticEvents::new(2, 1);

        for _ in 0..3 {
            send_event(&events, &counters, &task, TaskEventKind::Succeeded);
        }
        send_event(
            &events,
            &counters,
            &task,
            TaskEventKind::Failed("visible despite saturation".to_owned()),
        );

        assert_eq!(events.recv().await.unwrap().kind, TaskEventKind::Succeeded);
        assert!(matches!(
            events.recv().await.unwrap().kind,
            TaskEventKind::Failed(_)
        ));
        assert_eq!(
            counters.snapshot().counters,
            TaskDiagnosticCounters {
                succeeded: 3,
                failed: 1,
                discarded_succeeded: 2,
                ..TaskDiagnosticCounters::default()
            }
        );
    }

    #[test]
    fn discarded_failures_remain_counted_when_the_recent_buffer_is_full() {
        let task = ScheduledTask::new_unbounded(
            "failure-counter",
            TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: false,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let counters = Arc::new(TaskDiagnosticCountersAtomic::new(task.name.clone()));
        let events = DiagnosticEvents::new(1, 1);

        send_event(
            &events,
            &counters,
            &task,
            TaskEventKind::Failed("first failure".to_owned()),
        );
        send_event(
            &events,
            &counters,
            &task,
            TaskEventKind::Failed("second failure".to_owned()),
        );

        assert_eq!(
            counters.snapshot().counters,
            TaskDiagnosticCounters {
                failed: 2,
                discarded_failed: 1,
                ..TaskDiagnosticCounters::default()
            }
        );
    }

    #[tokio::test]
    async fn evicted_success_is_counted_for_its_originating_task() {
        let schedule = TaskSchedule::Fixed {
            every: Duration::from_secs(1),
            start_immediately: false,
        };
        let success_task = ScheduledTask::new_unbounded("success-task", schedule, || async {
            Ok::<_, io::Error>(())
        });
        let failure_task = ScheduledTask::new_unbounded("failure-task", schedule, || async {
            Ok::<_, io::Error>(())
        });
        let success_counters =
            Arc::new(TaskDiagnosticCountersAtomic::new(success_task.name.clone()));
        let failure_counters =
            Arc::new(TaskDiagnosticCountersAtomic::new(failure_task.name.clone()));
        let events = DiagnosticEvents::new(1, 2);

        send_event(
            &events,
            &success_counters,
            &success_task,
            TaskEventKind::Succeeded,
        );
        send_event(
            &events,
            &failure_counters,
            &failure_task,
            TaskEventKind::Failed("replacement failure".to_owned()),
        );

        assert_eq!(success_counters.snapshot().counters.discarded_succeeded, 1);
        assert_eq!(failure_counters.snapshot().counters.failed, 1);
        assert_eq!(failure_counters.snapshot().counters.discarded_succeeded, 0);
        assert!(matches!(
            events.recv().await.unwrap().kind,
            TaskEventKind::Failed(_)
        ));
    }

    #[tokio::test]
    async fn next_event_closes_after_all_tasks_exit() {
        let task = ScheduledTask::new_unbounded(
            "invalid-daily",
            TaskSchedule::Daily {
                timezone: chrono_tz::US::Eastern,
                hour: 24,
                minute: 0,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let mut supervisor = BackgroundSupervisor::start([task]);
        assert!(matches!(
            supervisor.next_event().await.unwrap().kind,
            TaskEventKind::Failed(_)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), supervisor.next_event())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn abort_before_first_poll_closes_the_event_stream() {
        let task = ScheduledTask::new_unbounded(
            "aborted-before-poll",
            TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: false,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let events = Arc::new(DiagnosticEvents::new(1, 1));
        let diagnostics = Arc::new(TaskDiagnosticCountersAtomic::new(task.name.clone()));
        let sender = EventSender {
            events: Arc::clone(&events),
        };
        let handle = tokio::spawn(run_scheduled_task(
            task,
            CancellationToken::new(),
            Arc::clone(&events),
            diagnostics,
            sender,
        ));
        handle.abort();

        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn saturated_diagnostics_do_not_delay_cancellation_or_shutdown() {
        let task = ScheduledTask::new_unbounded(
            "fast-success",
            TaskSchedule::Fixed {
                every: Duration::from_millis(1),
                start_immediately: true,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let mut supervisor = BackgroundSupervisor::start([task]);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if supervisor.diagnostics()[0].counters.discarded_succeeded > 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        supervisor.cancel();
        assert_eq!(
            supervisor.shutdown(Duration::from_millis(100)).await,
            ShutdownReport {
                completed: 1,
                aborted: 0,
            }
        );
    }

    #[tokio::test]
    async fn persistent_tasks_run_until_supervisor_cancellation_without_a_timeout() {
        let started = Arc::new(Notify::new());
        let task_started = Arc::clone(&started);
        let task = ScheduledTask::new_persistent("persistent", move || {
            let started = Arc::clone(&task_started);
            async move {
                started.notify_one();
                std::future::pending::<Result<(), io::Error>>().await
            }
        });
        let mut supervisor = BackgroundSupervisor::start([task]);
        started.notified().await;

        assert_eq!(
            supervisor.shutdown(Duration::from_millis(100)).await,
            ShutdownReport {
                completed: 1,
                aborted: 0,
            }
        );
    }

    #[tokio::test]
    async fn retained_event_messages_are_utf8_safe_byte_bounded_and_freshly_allocated() {
        let task = ScheduledTask::new_unbounded(
            "bounded-message",
            TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: false,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let counters = Arc::new(TaskDiagnosticCountersAtomic::new(task.name.clone()));
        let events = DiagnosticEvents::new(1, 1);
        let original_capacity = MAX_DIAGNOSTIC_MESSAGE_BYTES * 2;
        let mut message = String::with_capacity(original_capacity);
        message.push_str(&"é".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES));

        send_event(&events, &counters, &task, TaskEventKind::Failed(message));

        let Some(TaskEvent {
            kind: TaskEventKind::Failed(message),
            ..
        }) = events.recv().await
        else {
            panic!("expected retained failure event");
        };
        assert!(message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES);
        assert!(message.ends_with(TRUNCATED_DIAGNOSTIC_SUFFIX));
        assert!(message.capacity() <= MAX_DIAGNOSTIC_MESSAGE_BYTES);
    }

    #[tokio::test]
    async fn task_names_are_bounded_before_diagnostic_registry_retention() {
        let task = ScheduledTask::new_unbounded(
            "é".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES),
            TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: false,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let mut supervisor = BackgroundSupervisor::start([task]);

        let task = &supervisor.diagnostics()[0].task;
        assert!(task.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES);
        assert!(task.ends_with(TRUNCATED_DIAGNOSTIC_SUFFIX));
        supervisor.shutdown(Duration::from_secs(1)).await;
    }

    #[test]
    fn automatic_task_logs_include_scrubbed_failure_and_panic_details() {
        let task = ScheduledTask::new_unbounded(
            "safe-task-name",
            TaskSchedule::Fixed {
                every: Duration::from_secs(1),
                start_immediately: false,
            },
            || async { Ok::<_, io::Error>(()) },
        );
        let counters = Arc::new(TaskDiagnosticCountersAtomic::new(task.name.clone()));
        let events = DiagnosticEvents::new(2, 1);
        let logs = CapturedLogs(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(logs.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            send_event(
                &events,
                &counters,
                &task,
                TaskEventKind::Failed(
                    "duplicate key value violates unique constraint\nDETAIL: Key (email)=(user@example.com) already exists."
                        .to_owned(),
                ),
            );
            send_event(
                &events,
                &counters,
                &task,
                TaskEventKind::Panicked("worker crashed while opening a connection".to_owned()),
            );
        });

        let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("safe-task-name"));
        assert!(logs.contains("outcome=\"failure\""), "{logs}");
        assert!(logs.contains("outcome=\"panic\""), "{logs}");
        assert!(logs.contains("duplicate key value violates unique constraint"));
        assert!(logs.contains("worker crashed while opening a connection"));
        assert!(!logs.contains("user@example.com"));
    }

    #[test]
    fn triples_size_schedules_and_unbounded_runs_match_the_jvm_daemon() {
        assert_eq!(
            JvmTaskSchedules::TRIPLES_SIZE_COLLECTION_HOSTED,
            TaskSchedule::Fixed {
                every: Duration::from_mins(5),
                start_immediately: true,
            }
        );
        assert_eq!(
            JvmTaskSchedules::TRIPLES_SIZE_COLLECTION_DEVELOPMENT,
            TaskSchedule::Fixed {
                every: Duration::from_hours(1),
                start_immediately: true,
            }
        );
        let task = ScheduledTask::new_unbounded(
            "triples-size-collection",
            JvmTaskSchedules::TRIPLES_SIZE_COLLECTION_HOSTED,
            || async { Ok::<_, io::Error>(()) },
        );
        assert!(task.timeout.is_none());
    }

    #[test]
    fn partition_retention_schedules_match_the_jvm_daemons() {
        assert_eq!(
            JvmTaskSchedules::WAL_LOG_TRUNCATION,
            TaskSchedule::Fixed {
                every: Duration::from_hours(1),
                start_immediately: false,
            }
        );
        let before = "2026-10-31T20:00:00Z".parse().unwrap();
        let history = JvmTaskSchedules::HISTORY_TRUNCATION
            .next_daily_run(before)
            .unwrap();
        assert_eq!(history.to_rfc3339(), "2026-10-31T23:00:00+00:00");
    }

    #[test]
    fn daily_schedules_preserve_jvm_wall_times_across_dst() {
        let before_spring = "2026-03-07T12:00:00Z".parse().unwrap();
        let spring = JvmTaskSchedules::EPHEMERAL_APP_SWEEP
            .next_daily_run(before_spring)
            .unwrap();
        assert_eq!(spring.to_rfc3339(), "2026-03-08T09:00:00+00:00");
        let daily_metrics = JvmTaskSchedules::DAILY_METRICS
            .next_daily_run(before_spring)
            .unwrap();
        assert_eq!(daily_metrics.to_rfc3339(), "2026-03-07T17:00:00+00:00");

        let before_fall = "2026-10-31T20:00:00Z".parse().unwrap();
        let fall = JvmTaskSchedules::HARD_DELETION_SWEEP
            .next_daily_run(before_fall)
            .unwrap();
        assert_eq!(fall.to_rfc3339(), "2026-11-01T18:00:00+00:00");
        let daily_metrics = JvmTaskSchedules::DAILY_METRICS
            .next_daily_run(before_fall)
            .unwrap();
        assert_eq!(daily_metrics.to_rfc3339(), "2026-11-01T17:00:00+00:00");
    }
}
