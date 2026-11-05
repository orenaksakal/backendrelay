#[path = "../src/native_allocator.rs"]
mod native_allocator;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use instant_permissions::{EvaluationBindings, PermissionAction, RuleProgram};
use serde_json::{Value, json};

#[test]
fn vectors_preserve_zeroing_growth_shrink_and_overaligned_elements() {
    for size in [1, 7, 4096, 65537] {
        let mut bytes = vec![0_u8; size];
        assert!(bytes.iter().all(|value| *value == 0));
        for (index, value) in bytes.iter_mut().enumerate() {
            *value = u8::try_from(index % 251).unwrap();
        }
        let expected = bytes.clone();
        bytes.resize(size * 3, 29);
        assert_eq!(&bytes[..size], expected.as_slice());
        assert!(bytes[size..].iter().all(|value| *value == 29));
        bytes.truncate(size);
        bytes.shrink_to_fit();
        assert_eq!(bytes, expected);
    }
    #[derive(Clone, Debug, Eq, PartialEq)]
    #[repr(align(4096))]
    struct Aligned([u8; 4096]);
    let mut values = Vec::with_capacity(1);
    for index in 0..32 {
        values.push(Aligned([index; 4096]));
        assert_eq!(values.as_ptr().addr() % std::mem::align_of::<Aligned>(), 0);
    }
    values.shrink_to_fit();
    assert_eq!(values.as_ptr().addr() % std::mem::align_of::<Aligned>(), 0);
    for (index, value) in values.iter().enumerate() {
        assert_eq!(value.0, [u8::try_from(index).unwrap(); 4096]);
    }
    let empty = Vec::<u8>::new();
    assert!(empty.is_empty());
    let zero_sized = vec![(); 4096];
    assert_eq!(zero_sized.len(), 4096);
}

#[test]
fn concurrent_permission_evaluations_keep_every_subject_and_denial_fresh() {
    let program = RuleProgram::compile(
        PermissionAction::View,
        "auth.id != null && auth.id == data.ownerId && data.title.lowerAscii() == 'private'",
    )
    .unwrap();
    std::thread::scope(|scope| {
        for worker in 0..16 {
            let program = &program;
            scope.spawn(move || {
                for iteration in 0..64 {
                    let subject = format!("allocator-proof-{worker}-{iteration}");
                    let mut bindings = EvaluationBindings {
                        auth: json!({"id": subject}),
                        data: json!({"ownerId": subject, "title": "PRIVATE"}),
                        ..Default::default()
                    };
                    assert!(program.evaluate_bool(&bindings).unwrap());
                    bindings.auth = json!({"id": "different-subject"});
                    assert!(!program.evaluate_bool(&bindings).unwrap());
                    bindings.auth = Value::Null;
                    assert!(!program.evaluate_bool(&bindings).unwrap());
                }
            });
        }
    });
}

#[test]
fn cross_thread_owned_json_round_trips_and_frees_without_lost_rows() {
    let (sender, receiver) = std::sync::mpsc::sync_channel(32);
    std::thread::scope(|scope| {
        for worker in 0..16 {
            let sender = sender.clone();
            scope.spawn(move || {
                for iteration in 0..64 {
                    let value = json!({
                        "worker": worker,
                        "iteration": iteration,
                        "payload": "Unicode λ; quotes \" and newlines\n".repeat(128),
                        "records": (0..20).map(|row| json!({"row": row, "null": null, "flag": row%2==0})).collect::<Vec<_>>()
                    });
                    let bytes = serde_json::to_vec(&value).unwrap();
                    sender.send((worker, iteration, value, bytes)).unwrap();
                }
            });
        }
        drop(sender);
        let mut seen = BTreeSet::new();
        for (worker, iteration, value, bytes) in receiver {
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), value);
            assert_eq!(value["worker"], worker);
            assert_eq!(value["iteration"], iteration);
            assert!(seen.insert((worker, iteration)));
        }
        assert_eq!(seen.len(), 16 * 64);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_tasks_release_all_owned_payloads_across_workers() {
    struct OwnedPayload {
        bytes: Vec<u8>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for OwnedPayload {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let (sender, mut receiver) = tokio::sync::mpsc::channel(128);
    let mut tasks = Vec::new();
    for _ in 0..128 {
        let sender = sender.clone();
        let dropped = dropped.clone();
        tasks.push(tokio::spawn(async move {
            let owned = OwnedPayload {
                bytes: vec![37; 64 * 1024],
                dropped,
            };
            assert_eq!(owned.bytes.len(), 64 * 1024);
            sender.send(()).await.unwrap();
            std::future::pending::<()>().await;
            drop(owned);
        }));
    }
    drop(sender);
    for _ in 0..128 {
        receiver.recv().await.unwrap();
    }
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 128);
    assert_eq!(Arc::strong_count(&dropped), 1);
}
