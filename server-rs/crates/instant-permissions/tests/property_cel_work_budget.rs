//! Property coverage for the CEL work budget.
//!
//! Permission rules are user-supplied CEL source. Generated expressions must
//! either evaluate or fail with the deterministic work-budget error instead of
//! hanging or panicking.

use std::time::{Duration, Instant};

use instant_permissions::{
    EvaluationBindings, MAX_RULE_NESTING, MAX_RULE_OPERATOR_CHAIN, PermissionAction,
    PermissionError, RuleProgram, RuleSet,
};
use proptest::prelude::{BoxedStrategy, Just, Strategy, proptest};
use proptest::string::string_regex;
use serde_json::json;

/// Upper bound for one evaluation. The work budget is deterministic, so a rule
/// that stays within it always finishes far below this; the bound only catches
/// accidental non-termination.
const MAX_EVALUATION: Duration = Duration::from_secs(1);

/// Stack size `instant-server` gives its Tokio worker and blocking threads.
const PRODUCTION_STACK_SIZE: usize = 16 * 1024 * 1024;

fn string_literal() -> BoxedStrategy<String> {
    string_regex("[a-z0-9]{0,6}")
        .expect("valid regex")
        .prop_map(|value| format!("'{value}'"))
        .boxed()
}

fn string_expr(depth: u32) -> BoxedStrategy<String> {
    if depth == 0 {
        return string_literal();
    }
    proptest::prop_oneof![
        string_literal(),
        (string_expr(depth - 1), string_expr(depth - 1))
            .prop_map(|(left, right)| format!("({left} + {right})")),
        (string_expr(depth - 1), string_literal(), string_literal()).prop_map(
            |(target, search, replacement)| {
                format!("({target}.replace({search}, {replacement}))")
            }
        ),
        string_expr(depth - 1).prop_map(|target| format!("({target}.upperAscii())")),
        string_expr(depth - 1).prop_map(|target| format!("({target}.lowerAscii())")),
    ]
    .boxed()
}

fn int_expr(depth: u32) -> BoxedStrategy<String> {
    if depth == 0 {
        return (-100_i64..=100)
            .prop_map(|value| format!("({value})"))
            .boxed();
    }
    proptest::prop_oneof![
        (-100_i64..=100).prop_map(|value| format!("({value})")),
        (int_expr(depth - 1), int_expr(depth - 1))
            .prop_map(|(left, right)| format!("({left} + {right})")),
        (int_expr(depth - 1), int_expr(depth - 1))
            .prop_map(|(left, right)| format!("({left} - {right})")),
        (int_expr(depth - 1), -10_i64..=10)
            .prop_map(|(left, right)| format!("({left} * ({right}))")),
        string_expr(depth - 1).prop_map(|value| format!("size({value})")),
        proptest::collection::vec(int_expr(depth - 1), 1..4)
            .prop_map(|items| format!("size([{}])", items.join(", "))),
    ]
    .boxed()
}

fn bool_expr(depth: u32) -> BoxedStrategy<String> {
    let literal = || proptest::prop_oneof![Just("true".to_owned()), Just("false".to_owned())];
    if depth == 0 {
        return proptest::prop_oneof![
            literal(),
            (int_expr(1), int_expr(1)).prop_map(|(left, right)| format!("({left} < {right})")),
        ]
        .boxed();
    }
    proptest::prop_oneof![
        literal(),
        (int_expr(depth), int_expr(depth)).prop_map(|(left, right)| format!("({left} == {right})")),
        (int_expr(depth), int_expr(depth)).prop_map(|(left, right)| format!("({left} >= {right})")),
        bool_expr(depth - 1).prop_map(|value| format!("(!{value})")),
        (bool_expr(depth - 1), bool_expr(depth - 1))
            .prop_map(|(left, right)| format!("({left} && {right})")),
        (bool_expr(depth - 1), bool_expr(depth - 1))
            .prop_map(|(left, right)| format!("({left} || {right})")),
        (
            proptest::collection::vec(int_expr(depth), 1..4),
            int_expr(depth)
        )
            .prop_map(|(items, needle)| format!("[{}].exists(x, x > {needle})", items.join(", "))),
        (
            proptest::collection::vec(int_expr(depth), 1..4),
            int_expr(depth)
        )
            .prop_map(|(items, needle)| format!("[{}].all(x, x >= {needle})", items.join(", "))),
        string_expr(depth).prop_map(|value| format!("{value}.contains('a')")),
    ]
    .boxed()
}

/// Nested comprehensions bounded by `MAX_RULE_NESTING`. Each level adds two
/// bracket levels, so callers pass at most half the nesting limit. Budget
/// exhaustion itself is covered by `nested_comprehensions_exhaust_work_limit` in
/// the crate tests.
fn nested_exists(depth: usize) -> String {
    let mut body = "true".to_owned();
    for level in (0..depth).rev() {
        let variable = format!("x{level}");
        body = format!("[0, 1, 2, 3, 4, 5, 6, 7].exists({variable}, {variable} >= 0 && ({body}))");
    }
    body
}

fn on_production_stack<F>(work: F)
where
    F: FnOnce() + Send + 'static,
{
    let worker = std::thread::Builder::new()
        .name("cel-production-stack".to_owned())
        .stack_size(PRODUCTION_STACK_SIZE)
        .spawn(work)
        .expect("spawn production-stack thread");
    if let Err(payload) = worker.join() {
        std::panic::resume_unwind(payload);
    }
}

fn assert_bounded(source: &str) {
    let source = source.to_owned();
    on_production_stack(move || assert_bounded_on_production_stack(&source));
}

fn assert_bounded_on_production_stack(source: &str) {
    let Ok(program) = RuleProgram::compile(PermissionAction::View, source) else {
        // Generated expressions may fail static type checking; only the ones
        // that compile reach the evaluator this suite bounds.
        return;
    };
    let started = Instant::now();
    let result = program.evaluate(&EvaluationBindings::default());
    let elapsed = started.elapsed();
    assert!(
        elapsed < MAX_EVALUATION,
        "CEL evaluation of `{source}` took {elapsed:?}"
    );
    match result {
        Ok(_) => {}
        Err(PermissionError::Evaluation(message)) => assert!(
            message.contains("CEL work limit exceeded"),
            "unexpected CEL failure for `{source}`: {message}"
        ),
        Err(error) => panic!("unexpected CEL failure for `{source}`: {error}"),
    }
}

proptest! {
    #[test]
    fn generated_cel_expressions_evaluate_or_hit_the_work_budget(source in bool_expr(3)) {
        assert_bounded(&source);
    }

    #[test]
    fn deeply_nested_comprehensions_stay_within_the_work_budget(depth in 0_usize..=(MAX_RULE_NESTING / 2)) {
        assert_bounded(&nested_exists(depth));
    }
}

#[test]
fn over_deep_rules_are_rejected_before_recursion() {
    on_production_stack(over_deep_rules_are_rejected_on_production_stack);
}

fn over_deep_rules_are_rejected_on_production_stack() {
    let at_limit = format!(
        "{}true{}",
        "(".repeat(MAX_RULE_NESTING),
        ")".repeat(MAX_RULE_NESTING)
    );
    RuleProgram::compile(PermissionAction::View, at_limit)
        .expect("a rule at the nesting limit must compile");
    let source = format!(
        "{}true{}",
        "(".repeat(MAX_RULE_NESTING + 1),
        ")".repeat(MAX_RULE_NESTING + 1)
    );
    let error = RuleProgram::compile(PermissionAction::View, source.clone())
        .expect_err("over-deep rule must be rejected");
    assert!(
        matches!(error, PermissionError::Parse(_)),
        "unexpected error for an over-deep rule: {error}"
    );
    assert!(
        error.to_string().contains("nesting"),
        "over-deep rule error must name the nesting limit: {error}"
    );

    let chain = format!("1{}", " + 1".repeat(MAX_RULE_OPERATOR_CHAIN + 1));
    let error = RuleProgram::compile(PermissionAction::View, chain)
        .expect_err("an over-long operator chain must be rejected");
    assert!(
        matches!(error, PermissionError::Parse(_)),
        "unexpected error for an over-long operator chain: {error}"
    );
    assert!(
        error.to_string().contains("operator"),
        "operator-chain error must name the limit: {error}"
    );

    // The same pre-scan guards document validation, so an over-deep rule cannot
    // be saved through `POST /rules` either.
    let issues = RuleSet::try_new(json!({"todos": {"allow": {"view": source}}}))
        .expect_err("saving an over-deep rule must fail validation");
    assert!(
        issues.iter().any(|issue| issue.message.contains("nesting")),
        "saved over-deep rule must report the nesting limit: {issues:?}"
    );

    // This is the shape that overflowed the debug test stack before the
    // pre-scan existed; it must now fail validation instead of aborting.
    let crash_shape = nested_exists(MAX_RULE_NESTING);
    let error = RuleProgram::compile(PermissionAction::View, crash_shape)
        .expect_err("the stack-overflow shape must be rejected");
    assert!(
        matches!(error, PermissionError::Parse(_)),
        "unexpected error for the overflow shape: {error}"
    );
}
