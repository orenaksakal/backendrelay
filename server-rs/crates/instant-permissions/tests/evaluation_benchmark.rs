//! Manual CPU characterization with synthetic bindings; not a timing gate.
use std::hint::black_box;
use std::time::Instant;

use instant_permissions::{EvaluationBindings, PermissionAction, RuleProgram};
use serde_json::json;

#[test]
#[ignore = "manual CPU benchmark; compare identical release builds before and after a change"]
fn repeated_permission_evaluation() {
    let bindings = EvaluationBindings {
        auth: json!({"id": "synthetic-user-0"}),
        data: json!({
            "ownerId": "synthetic-user-0",
            "memberIds": (0..20).map(|n| format!("synthetic-user-{n}")).collect::<Vec<_>>(),
        }),
        ..EvaluationBindings::default()
    };
    let iterations = 20_000;
    for (name, source) in [
        ("private", "auth.id != null && auth.id == data.ownerId"),
        ("shared", "auth.id != null && auth.id in data.memberIds"),
    ] {
        let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
        assert!(program.evaluate_bool(&bindings).unwrap());
        let started = Instant::now();
        for _ in 0..iterations {
            assert!(black_box(
                program.evaluate_bool(black_box(&bindings)).unwrap()
            ));
        }
        let elapsed = started.elapsed();
        println!(
            "permission-evaluation {name}: {iterations} iterations, {} ns/evaluation",
            elapsed.as_nanos() / iterations
        );
    }
}
