use instant_permissions::{EvaluationBindings, PermissionAction, RuleProgram};
use serde_json::json;

#[test]
fn shared_function_definitions_keep_concurrent_subjects_and_decisions_fresh() {
    let program = RuleProgram::compile(
        PermissionAction::View,
        "auth.id != null && auth.id == data.ownerId && data.title.lowerAscii() == 'private'",
    )
    .unwrap();
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let program = &program;
            scope.spawn(move || {
                for iteration in 0..64 {
                    let subject = format!("synthetic-{worker}-{iteration}");
                    let mut bindings = EvaluationBindings {
                        auth: json!({"id":subject}),
                        data: json!({"ownerId":subject,"title":"PRIVATE"}),
                        ..Default::default()
                    };
                    assert!(program.evaluate_bool(&bindings).unwrap());
                    bindings.auth = json!({"id":"different-subject"});
                    assert!(!program.evaluate_bool(&bindings).unwrap());
                    bindings.auth = json!(null);
                    assert!(!program.evaluate_bool(&bindings).unwrap());
                }
            });
        }
    });
}
