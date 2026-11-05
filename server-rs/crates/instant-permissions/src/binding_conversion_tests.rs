use super::*;
use serde_json::json;

// Frozen pre-optimization JSON-copy path (source de99ff286cc3). Keep this
// independent of the direct CEL-map construction and null insertion helpers.
fn reference_binding(
    value: &JsonValue,
    paths: &[Vec<String>],
    etype: Option<&str>,
) -> Result<Value, PermissionError> {
    let mut value = normalize_json(value.clone());
    if let (Some(etype), Some(object)) = (etype, value.as_object_mut()) {
        object.insert("__instant_ref_etype".into(), json!(etype));
    }
    let mut value = normalize_json(value.clone());
    fn insert(value: &mut JsonValue, path: &[String]) {
        let Some((field, rest)) = path.split_first() else {
            return;
        };
        let Some(object) = value.as_object_mut() else {
            return;
        };
        if rest.is_empty() {
            object.entry(field.clone()).or_insert(JsonValue::Null);
        } else if let Some(child) = object.get_mut(field) {
            insert(child, rest);
        }
    }
    for path in paths {
        insert(&mut value, path);
    }
    json_to_cel(&value)
}

#[allow(clippy::too_many_lines)]
fn reference_advance(
    program: &RuleProgram,
    bindings: &EvaluationBindings,
    refs: &RefCache,
    remaining: &mut u64,
) -> Result<EvaluationOutcome, PermissionError> {
    let mut input_budget = 4 * 1024 * 1024;
    for value in [
        &bindings.data,
        &bindings.auth,
        &bindings.rule_params,
        &bindings.rate_limit,
    ]
    .into_iter()
    .chain(bindings.new_data.iter())
    .chain(bindings.linked_data.iter())
    .chain(bindings.actions.iter())
    .chain(refs.values().flatten())
    {
        charge_binding(value, &mut input_budget, 0)?;
    }
    let ref_values = refs
        .iter()
        .map(|(request, values)| {
            values
                .iter()
                .map(json_to_cel)
                .collect::<Result<Vec<_>, _>>()
                .map(|values| (request.clone(), values))
        })
        .collect::<Result<HashMap<_, _>, _>>()?;
    let missing = Arc::new(Mutex::new(HashSet::new()));
    let rate_requests = Arc::new(Mutex::new(Vec::new()));
    let mut context = PERMISSION_FUNCTIONS.new_function_scope();
    context.set_work_limit(*remaining);
    register_functions(
        &mut context,
        Arc::new(ref_values),
        Arc::clone(&missing),
        bindings.rate_limit_runtime.clone(),
        Arc::clone(&rate_requests),
    );
    for (name, value, etype) in [
        ("data", Some(&bindings.data), bindings.data_etype.as_deref()),
        ("auth", Some(&bindings.auth), Some("$users")),
        ("ruleParams", Some(&bindings.rule_params), None),
        ("rateLimit", Some(&bindings.rate_limit), None),
        ("newData", bindings.new_data.as_ref(), None),
        (
            "linkedData",
            bindings.linked_data.as_ref(),
            bindings.linked_data_etype.as_deref(),
        ),
        ("actions", bindings.actions.as_ref(), None),
    ] {
        if let Some(value) = value {
            context.add_variable_from_value(
                name,
                reference_binding(
                    value,
                    program.selected_paths.get(name).map_or(&[], Vec::as_slice),
                    etype,
                )?,
            );
        }
    }
    add_math_binding(&mut context)?;
    add_request_binding(&mut context, &bindings.request)?;
    for (name, bind) in program.bind_programs.iter() {
        match bind.execute(&context) {
            Ok(value) => context.add_variable_from_value(name, value),
            Err(error) => {
                *remaining = context.work_remaining();
                return evaluation_error(&error, &missing);
            }
        }
    }
    let value = program.program.execute(&context);
    *remaining = context.work_remaining();
    match value {
        Ok(value) => {
            consume_rate_limits(bindings, &rate_requests)?;
            value
                .json()
                .map(EvaluationOutcome::Value)
                .map_err(|error| PermissionError::Evaluation(error.to_string()))
        }
        Err(error) => evaluation_error(&error, &missing),
    }
}

fn compare(program: &RuleProgram, bindings: &EvaluationBindings, refs: &RefCache, limit: u64) {
    let (mut actual_budget, mut expected_budget) = (limit, limit);
    let expected = reference_advance(program, bindings, refs, &mut expected_budget);
    let actual = program.advance_with_budget(bindings, refs, &mut actual_budget);
    assert_eq!(actual, expected, "{}", program.source);
    assert_eq!(actual_budget, expected_budget, "fuel must be identical");
}

fn paths() -> Vec<Vec<String>> {
    [
        vec![],
        vec!["missing", "leaf"],
        vec!["missing"],
        vec!["nested", "missing"],
        vec!["nested", "inner", "leaf"],
        vec!["null", "missing"],
        vec!["array", "missing"],
        vec!["scalar", "missing"],
        vec!["__instant_ref_etype", "missing"],
        vec!["", "missing"],
        vec!["a.b"],
        vec!["missing"],
    ]
    .into_iter()
    .map(|path| path.into_iter().map(str::to_owned).collect())
    .collect()
}

#[test]
fn complete_binding_conversion_matches_json_copy_path() {
    let inputs = [
        json!(null),
        json!(false),
        json!(true),
        json!("text"),
        json!(0),
        json!(-0.0),
        json!(1.25),
        json!(i64::MIN),
        json!(i64::MAX),
        json!(u64::MAX),
        json!([null, {"nested": {}, "unselected": [1, 2]}]),
        json!({}),
        json!({"nested": {"inner": {}}, "null": null, "array": [{"x": 1}],
            "scalar": false, "": {}, "a.b": "literal", "unselected": [1, 2, 3]}),
        json!({"__instant_ref_etype": u64::MAX}),
        json!({"nested": {"__instant_ref_etype": u64::MAX}}),
    ];
    for input in inputs {
        let original = input.clone();
        for etype in [None, Some("posts"), Some("$users"), Some("")] {
            for selected in [vec![], paths(), paths().into_iter().rev().collect()] {
                assert_eq!(
                    json_binding_value(&input, &selected, etype),
                    reference_binding(&input, &selected, etype),
                    "{input:?} {selected:?} {etype:?}"
                );
            }
        }
        assert_eq!(input, original);
    }
}

#[test]
fn generated_nested_bindings_match_without_losing_unselected_values_or_errors() {
    fn generated(seed: u64, depth: u32) -> JsonValue {
        match (seed % 8, depth) {
            (0, _) => json!(null),
            (1, _) => json!((seed / 8) % 2 == 0),
            (2, _) => json!(seed),
            (3, _) => json!(format!("text-λ-{seed}")),
            (4, _) => json!(u64::MAX),
            (5, _) => json!({"__instant_ref_etype": u64::MAX}),
            (6, 1..) => json!([
                generated(seed.wrapping_mul(17).wrapping_add(1), depth - 1),
                generated(seed.wrapping_mul(19).wrapping_add(3), depth - 1)
            ]),
            (7, 1..) => json!({
                "nested": generated(seed.wrapping_mul(23).wrapping_add(7), depth - 1),
                "unselected": generated(seed.wrapping_mul(29).wrapping_add(1), depth - 1),
                "presentNull": null,
                "__instant_ref_etype": seed,
            }),
            _ => json!({}),
        }
    }
    for seed in 0..1024 {
        let input = generated(seed, 4);
        for etype in [None, Some("fresh-type")] {
            assert_eq!(
                json_binding_value(&input, &paths(), etype),
                reference_binding(&input, &paths(), etype)
            );
        }
    }
}

#[test]
fn cel_presence_dynamic_maps_binds_optional_inputs_and_fuel_match() {
    let bindings = EvaluationBindings {
        data: json!({"id": "post", "nullable": null, "profile": {}, "list": [1, 2],
            "unselected": "retained", "__instant_ref_etype": u64::MAX}),
        data_etype: Some("posts".into()),
        auth: json!({"id": "subject", "nullable": null}),
        rule_params: json!({"key": "unselected"}),
        new_data: Some(json!({"profile": {}, "nullable": null})),
        linked_data: Some(json!({"id": "comment", "nullable": null})),
        linked_data_etype: Some("comments".into()),
        actions: Some(json!(["link", "unlink"])),
        ..EvaluationBindings::default()
    };
    for source in [
        "has(data.nullable) && !has(data.missing)",
        "has(data.missing) ? data.missing == null : true",
        "data.profile.absent == null && !has(data.profile.absent)",
        "data[ruleParams.key] == 'retained' && size(data) >= 6",
        "data.list.all(x, x > 0) && data.list.map(x, x + 1) == [2, 3]",
        "data.__instant_ref_etype == 'posts' && auth.__instant_ref_etype == '$users'",
        "has(newData.nullable) && !has(newData.missing) && newData.profile.x == null",
        "linkedData.__instant_ref_etype == 'comments' && size(actions) == 2",
        "request.time.getTime() > 0 && math.isFinite(1.0)",
        "data.profile + 1",
        "newData == null",
        "data",
    ] {
        let program = RuleProgram::compile(PermissionAction::Link, source).unwrap();
        for limit in [0, 1, 20, 100_000] {
            compare(&program, &bindings, &RefCache::new(), limit);
        }
        let mut absent = bindings.clone();
        absent.new_data = None;
        absent.linked_data = None;
        absent.actions = None;
        compare(&program, &absent, &RefCache::new(), 100_000);
    }
    let program = RuleProgram::compile_with_binds(
        PermissionAction::View,
        "owner == auth.id && data.missing == null",
        vec![("owner".into(), "data.ownerId".into())],
    )
    .unwrap();
    for auth in [
        json!(null),
        json!({"id": "subject"}),
        json!({"id": "other"}),
    ] {
        let fresh = EvaluationBindings {
            data: json!({"ownerId": "subject"}),
            auth,
            ..EvaluationBindings::default()
        };
        compare(&program, &fresh, &RefCache::new(), 100_000);
    }
}

#[test]
fn reference_targets_requests_fresh_values_and_retry_fuel_match() {
    let program = RuleProgram::compile(
        PermissionAction::Link,
        "data.ref('owner.id') == ['allowed'] && auth.ref('$user.profile.id') == [] && linkedData.ref('post.id') == []",
    )
    .unwrap();
    let mut bindings = EvaluationBindings {
        data: json!({"id": "post", "__instant_ref_etype": "wrong"}),
        data_etype: Some("posts".into()),
        auth: json!({"id": "subject"}),
        linked_data: Some(json!({"id": 27})),
        linked_data_etype: Some("comments".into()),
        ..EvaluationBindings::default()
    };
    let mut refs = RefCache::new();
    let (mut expected_budget, mut actual_budget) = (100_000, 100_000);
    for _ in 0..4 {
        let expected = reference_advance(&program, &bindings, &refs, &mut expected_budget);
        let actual = program.advance_with_budget(&bindings, &refs, &mut actual_budget);
        assert_eq!(actual, expected);
        assert_eq!(actual_budget, expected_budget);
        match actual.unwrap() {
            EvaluationOutcome::NeedsRefs(requests) => {
                for request in requests {
                    let values = if request.etype == "posts" {
                        vec![json!("allowed")]
                    } else {
                        vec![]
                    };
                    refs.insert(request, values);
                }
            }
            EvaluationOutcome::Value(value) => {
                assert_eq!(value, json!(true));
                break;
            }
        }
    }
    assert_eq!(refs.len(), 3);
    for values in refs.values_mut() {
        *values = vec![json!("denied")];
    }
    compare(&program, &bindings, &refs, 100_000);
    assert_eq!(
        program.evaluate_bool(&bindings).unwrap_err(),
        PermissionError::MissingReferences(3)
    );
    for (request, values) in &mut refs {
        *values = if request.etype == "posts" {
            vec![json!("allowed")]
        } else {
            vec![]
        };
    }
    bindings.data_etype = Some("other-tenant-type".into());
    compare(&program, &bindings, &refs, 100_000);
    assert!(matches!(
        program.advance(&bindings, &refs).unwrap(),
        EvaluationOutcome::NeedsRefs(_)
    ));
}

#[test]
fn full_binding_budget_depth_and_shadowed_tag_inputs_remain_charged() {
    let program = RuleProgram::compile(PermissionAction::Link, "true").unwrap();
    for field in 0..8 {
        let large = json!("x".repeat(4 * 1024 * 1024));
        let mut bindings = EvaluationBindings::default();
        let mut refs = RefCache::new();
        match field {
            0 => bindings.data = json!({"unselected": large}),
            1 => bindings.auth = json!({"__instant_ref_etype": large}),
            2 => bindings.rule_params = large,
            3 => bindings.rate_limit = large,
            4 => bindings.new_data = Some(large),
            5 => bindings.linked_data = Some(large),
            6 => bindings.actions = Some(large),
            _ => {
                refs.insert(
                    RefRequest {
                        etype: "unused".into(),
                        entity_id: RefEntityId::Int(1),
                        path: "unused".into(),
                    },
                    vec![large],
                );
            }
        }
        compare(&program, &bindings, &refs, 100_000);
        assert_eq!(
            program.advance(&bindings, &refs),
            Err(PermissionError::Evaluation(
                "CEL binding budget exceeded".into()
            ))
        );
    }
    for depth in [63, 64, 65] {
        let mut value = json!(null);
        for _ in 0..depth {
            value = json!({"nested": value});
        }
        let bindings = EvaluationBindings {
            data: value,
            ..EvaluationBindings::default()
        };
        compare(&program, &bindings, &RefCache::new(), 100_000);
        assert_eq!(program.evaluate_bool(&bindings).is_ok(), depth <= 64);
    }
}

#[derive(Debug, Default)]
struct RecordingConsumer(Mutex<Vec<RateLimitRequest>>);

impl RateLimitConsumer for RecordingConsumer {
    fn consume(&self, request: &RateLimitRequest) -> Result<RateLimitDecision, String> {
        self.0.lock().unwrap().push(request.clone());
        Ok(RateLimitDecision::Allowed {
            remaining_tokens: 42,
        })
    }
}

#[test]
fn rate_limit_requests_are_fresh_and_identical_to_copy_path() {
    let rules = RuleSet::new(json!({
        "$rateLimits": {"reads": {"limits": [{"capacity": 10}]}},
        "posts": {"allow": {"view": "rateLimit.reads.limit(auth.id) && auth.id == data.ownerId"}}
    }));
    let program = rules
        .program("posts", PermissionAction::View)
        .unwrap()
        .unwrap();
    let expected = Arc::new(RecordingConsumer::default());
    let actual = Arc::new(RecordingConsumer::default());
    for subject in ["first", "second", "first"] {
        let mut bindings = EvaluationBindings {
            auth: json!({"id": subject}),
            data: json!({"ownerId": subject}),
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(Uuid::nil(), &mut bindings, expected.clone())
            .unwrap();
        let mut expected_budget = 100_000;
        let expected_value =
            reference_advance(&program, &bindings, &RefCache::new(), &mut expected_budget);
        rules
            .configure_rate_limits(Uuid::nil(), &mut bindings, actual.clone())
            .unwrap();
        let mut actual_budget = 100_000;
        assert_eq!(
            program.advance_with_budget(&bindings, &RefCache::new(), &mut actual_budget),
            expected_value
        );
        assert_eq!(actual_budget, expected_budget);
    }
    assert_eq!(*actual.0.lock().unwrap(), *expected.0.lock().unwrap());
    assert_eq!(actual.0.lock().unwrap().len(), 3);
}

#[test]
fn concurrent_conversion_keeps_inputs_references_and_optional_bindings_isolated() {
    let program = RuleProgram::compile(
        PermissionAction::Link,
        "auth.id == data.ownerId && data.ref('owner.id') == [auth.id] && newData.nonce == linkedData.nonce && actions == [ruleParams.nonce] && has(data.nullable) && !has(data.missing)",
    ).unwrap();
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let program = &program;
            scope.spawn(move || {
                for iteration in 0..64 {
                    let subject = format!("subject-{worker}-{iteration}");
                    let etype = format!("type-{worker}");
                    let mut bindings = EvaluationBindings {
                        data: json!({"id": iteration, "ownerId": subject, "nullable": null}),
                        data_etype: Some(etype.clone()),
                        auth: json!({"id": subject}),
                        rule_params: json!({"nonce": iteration}),
                        new_data: Some(json!({"nonce": iteration})),
                        linked_data: Some(json!({"nonce": iteration})),
                        actions: Some(json!([iteration])),
                        ..EvaluationBindings::default()
                    };
                    let request = RefRequest {
                        etype,
                        entity_id: RefEntityId::Int(iteration),
                        path: "owner.id".into(),
                    };
                    let mut refs = RefCache::from([(request.clone(), vec![json!(subject)])]);
                    compare(program, &bindings, &refs, 100_000);
                    assert_eq!(
                        program.advance(&bindings, &refs).unwrap(),
                        EvaluationOutcome::Value(json!(true))
                    );
                    bindings.auth = json!({"id": "other"});
                    compare(program, &bindings, &refs, 100_000);
                    assert_eq!(
                        program.advance(&bindings, &refs).unwrap(),
                        EvaluationOutcome::Value(json!(false))
                    );
                    bindings.auth = json!({"id": subject});
                    refs.insert(request, vec![json!("other")]);
                    compare(program, &bindings, &refs, 100_000);
                    assert_eq!(
                        program.advance(&bindings, &refs).unwrap(),
                        EvaluationOutcome::Value(json!(false))
                    );
                    assert!(!bindings.data.as_object().unwrap().contains_key("missing"));
                    assert!(
                        !bindings
                            .data
                            .as_object()
                            .unwrap()
                            .contains_key("__instant_ref_etype")
                    );
                }
            });
        }
    });
}
