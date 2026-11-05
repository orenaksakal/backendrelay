//! Fixed-category observations only. Never retain session data or change admission.
use std::sync::atomic::{AtomicU64, Ordering};

const LABELS: [&str; 13] = [
    "loop-dropped",
    "message-worker-ended",
    "outbound-stream-ended",
    "outbound-send-error",
    "outbound-send-timeout",
    "inbound-receive-error",
    "inbound-eof",
    "inbound-handler-ended",
    "peer-close",
    "idle-unrevived",
    "ping-send-error",
    "ping-send-timeout",
    "service-restart",
];

#[derive(Clone, Copy)]
#[repr(usize)]
pub(super) enum WebsocketExit {
    LoopDropped,
    MessageWorkerEnded,
    OutboundStreamEnded,
    OutboundSendError,
    OutboundSendTimeout,
    InboundReceiveError,
    InboundEof,
    InboundHandlerEnded,
    PeerClose,
    IdleUnrevived,
    PingSendError,
    PingSendTimeout,
    ServiceRestart,
}

struct ExitCounters([AtomicU64; LABELS.len()]);

impl ExitCounters {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; LABELS.len()])
    }

    fn snapshot(&self) -> [(&'static str, u64); LABELS.len()] {
        std::array::from_fn(|index| (LABELS[index], self.0[index].load(Ordering::Relaxed)))
    }
}

static EXITS: ExitCounters = ExitCounters::new();

/// Process-wide WebSocket loop exit observations, not client error allowances.
///
/// `loop-dropped` means no normal branch was selected, including task abortion
/// or unwinding. `inbound-handler-ended` includes peer receive overload or an
/// unsuccessful control-frame reply; it is not a more specific inferred cause.
/// Counters are individually atomic; the returned collection is not one snapshot.
#[must_use]
pub fn runtime_websocket_exit_metrics() -> [(&'static str, u64); LABELS.len()] {
    EXITS.snapshot()
}

pub(super) struct WebsocketExitGuard<'a> {
    counters: &'a ExitCounters,
    reason: WebsocketExit,
}

impl WebsocketExitGuard<'static> {
    pub(super) fn global() -> Self {
        Self {
            counters: &EXITS,
            reason: WebsocketExit::LoopDropped,
        }
    }
}

impl WebsocketExitGuard<'_> {
    pub(super) fn finish(mut self, reason: WebsocketExit) {
        self.reason = reason;
    }
}

impl Drop for WebsocketExitGuard<'_> {
    fn drop(&mut self) {
        self.counters.0[self.reason as usize].fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn every_exit_has_one_fixed_label_and_is_counted_exactly_once() {
        let counters = ExitCounters::new();
        let reasons = [
            WebsocketExit::LoopDropped,
            WebsocketExit::MessageWorkerEnded,
            WebsocketExit::OutboundStreamEnded,
            WebsocketExit::OutboundSendError,
            WebsocketExit::OutboundSendTimeout,
            WebsocketExit::InboundReceiveError,
            WebsocketExit::InboundEof,
            WebsocketExit::InboundHandlerEnded,
            WebsocketExit::PeerClose,
            WebsocketExit::IdleUnrevived,
            WebsocketExit::PingSendError,
            WebsocketExit::PingSendTimeout,
            WebsocketExit::ServiceRestart,
        ];
        for reason in reasons {
            WebsocketExitGuard {
                counters: &counters,
                reason: WebsocketExit::LoopDropped,
            }
            .finish(reason);
        }
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.len(), 13);
        for (index, (label, count)) in snapshot.into_iter().enumerate() {
            assert_eq!(label, LABELS[index]);
            assert_eq!(count, 1);
            assert!(
                label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
            );
        }
    }

    #[test]
    fn uncompleted_guard_counts_drop_without_inventing_a_specific_cause() {
        let counters = ExitCounters::new();
        drop(WebsocketExitGuard {
            counters: &counters,
            reason: WebsocketExit::LoopDropped,
        });
        assert_eq!(counters.snapshot()[0], ("loop-dropped", 1));
        assert!(
            counters.snapshot()[1..]
                .iter()
                .all(|(_, count)| *count == 0)
        );
    }

    #[test]
    fn independent_threads_keep_exact_exit_counts_without_retaining_sessions() {
        let counters = Arc::new(ExitCounters::new());
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let counters = Arc::clone(&counters);
                scope.spawn(move || {
                    for _ in 0..1000 {
                        WebsocketExitGuard {
                            counters: &counters,
                            reason: WebsocketExit::LoopDropped,
                        }
                        .finish(WebsocketExit::PeerClose);
                    }
                });
            }
        });
        assert_eq!(
            counters.snapshot()[WebsocketExit::PeerClose as usize].1,
            16_000
        );
        assert_eq!(
            counters
                .snapshot()
                .iter()
                .map(|(_, count)| count)
                .sum::<u64>(),
            16_000
        );
        assert_eq!(Arc::strong_count(&counters), 1);
    }

    #[tokio::test]
    async fn aborted_loop_counts_drop_once_without_retaining_the_task() {
        let counters = Arc::new(ExitCounters::new());
        let owned = Arc::clone(&counters);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let guard = WebsocketExitGuard {
                counters: &owned,
                reason: WebsocketExit::LoopDropped,
            };
            sender.send(()).unwrap();
            std::future::pending::<()>().await;
            drop(guard);
        });
        receiver.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(counters.snapshot()[0], ("loop-dropped", 1));
        assert_eq!(Arc::strong_count(&counters), 1);
    }
}
