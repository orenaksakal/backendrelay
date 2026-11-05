//! Conservative proof that a prepared permission pass cannot do database I/O.
use cel::common::ast::{EntryExpr, Expr, IdedExpr};

use crate::RuleProgram;

pub(super) fn program_has_database_callbacks(program: &RuleProgram) -> bool {
    expression_has_database_callbacks(program.program.expression())
        || program
            .bind_programs
            .iter()
            .any(|(_, bind)| expression_has_database_callbacks(bind.expression()))
}

fn expression_has_database_callbacks(expression: &IdedExpr) -> bool {
    match &expression.expr {
        Expr::Call(call) => {
            // These are the two stateful functions installed by
            // register_functions. Unlike reference_uses/rate_limit_uses, inspect
            // every call, not only literal paths/bucket names or taken branches.
            // Any future stateful native function must also be excluded here.
            matches!(call.func_name.as_str(), "ref" | "limit")
                || call
                    .target
                    .as_deref()
                    .is_some_and(expression_has_database_callbacks)
                || call.args.iter().any(expression_has_database_callbacks)
        }
        Expr::Comprehension(value) => [
            &value.iter_range,
            &value.accu_init,
            &value.loop_cond,
            &value.loop_step,
            &value.result,
        ]
        .into_iter()
        .any(|expression| expression_has_database_callbacks(expression)),
        Expr::List(list) => list.elements.iter().any(expression_has_database_callbacks),
        Expr::Map(map) => map
            .entries
            .iter()
            .any(|entry| entry_has_database_callbacks(&entry.expr)),
        Expr::Select(select) => expression_has_database_callbacks(&select.operand),
        Expr::Struct(value) => value
            .entries
            .iter()
            .any(|entry| entry_has_database_callbacks(&entry.expr)),
        Expr::Ident(_) | Expr::Literal(_) => false,
        Expr::Unspecified => true,
    }
}

fn entry_has_database_callbacks(entry: &EntryExpr) -> bool {
    match entry {
        EntryExpr::StructField(field) => expression_has_database_callbacks(&field.value),
        EntryExpr::MapEntry(entry) => {
            expression_has_database_callbacks(&entry.key)
                || expression_has_database_callbacks(&entry.value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_bookkeeping_tests::fixture;
    use crate::{
        EvaluationBindings, PermissionAction, PermissionError, QueryPermissionOptions, RefCache,
        RuleSet, filter_executed_query_owned, prepare_owned_query_permissions,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn pure_object_rules_presence_and_comprehensions_can_finish_without_callbacks() {
        for source in [
            "auth.id != null && auth.id == data.ownerId",
            "auth.id != null && auth.id in data.memberIds",
            "has(data.ownerId) && data.ownerId != null",
            "[data.ownerId].exists(owner, owner == auth.id)",
            "{'owner': data.ownerId}['owner'] == auth.id",
        ] {
            let program = crate::RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert!(!program_has_database_callbacks(&program), "{source}");
        }
    }

    #[test]
    fn literal_dynamic_and_dead_ref_or_limit_calls_are_all_excluded() {
        for source in [
            "data.ref('owner.id') == []",
            "data.ref(ruleParams.path) == []",
            "true || data.ref('owner.id') == []",
            "false && auth.ref('$user.profile.id') == []",
            "rateLimit.bucket.limit(auth.id)",
            "rateLimit[ruleParams.bucket].limit(auth.id)",
        ] {
            let program = crate::RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert!(program_has_database_callbacks(&program), "{source}");
        }
        let dynamic =
            crate::RuleProgram::compile(PermissionAction::View, "data.ref(ruleParams.path) == []")
                .unwrap();
        assert!(
            dynamic.reference_uses().is_empty(),
            "literal-only metadata is not an adequate proof"
        );
    }

    #[test]
    fn callback_walk_covers_binds_map_keys_values_lists_targets_and_comprehensions() {
        let program = crate::RuleProgram::compile_with_binds(
            PermissionAction::View,
            "selected",
            vec![("selected".into(), "data.ref('owner.id') == []".into())],
        )
        .unwrap();
        assert!(program_has_database_callbacks(&program));
        for source in [
            "{'value': data.ref('x')}",
            "{data.ref('x'): true}",
            "[data.ref('x')]",
            "[true].exists(value, data.ref('x') == [])",
            "data.ref('x').size() == 0",
            "(false ? data.ref('x') : []) == []",
            "{'nested': [rateLimit.bucket.limit(auth.id)]}.nested",
        ] {
            let expression = cel::Program::compile(source).unwrap();
            assert!(
                expression_has_database_callbacks(expression.expression()),
                "{source}"
            );
        }
        let mut unknown = cel::Program::compile("true").unwrap().expression().clone();
        unknown.expr = Expr::Unspecified;
        assert!(expression_has_database_callbacks(&unknown));
    }

    #[tokio::test]
    async fn prepared_pass_owns_fresh_records_not_the_loader_and_runs_the_complete_filter() {
        struct LoaderLifetime(Arc<AtomicBool>);
        impl Drop for LoaderLifetime {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let (catalog, query, input, data) = fixture();
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view": "auth.id == data.ownerId && data.unfetched == 'must remain complete'"},
                      "fields": {"secret": "auth.admin"}},
            "comments": {"allow": {"view": "true"}}
        }));
        let bindings = EvaluationBindings {
            auth: json!({"id":"me","admin":false}),
            ..Default::default()
        };
        let released = Arc::new(AtomicBool::new(false));
        let load_count = Arc::new(AtomicUsize::new(0));
        let prepared = {
            let loader = LoaderLifetime(Arc::clone(&released));
            let count = Arc::clone(&load_count);
            let fresh = data.clone();
            prepare_owned_query_permissions(
                &rules,
                &query,
                input.clone(),
                &catalog,
                &bindings,
                QueryPermissionOptions::default(),
                |loads| async move {
                    let _borrow = &loader;
                    assert_eq!(
                        loads.iter().map(|load| load.entities.len()).sum::<usize>(),
                        24
                    );
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(fresh)
                },
            )
            .await
            .unwrap()
        };
        assert!(released.load(Ordering::SeqCst));
        assert_eq!(load_count.load(Ordering::SeqCst), 1);
        assert!(prepared.can_release_snapshot());
        let actual = prepared
            .filter(&mut RefCache::new(), |_| async {
                panic!("a proven pure pass must never request database references")
            })
            .await
            .unwrap();
        let expected = filter_executed_query_owned(
            &rules,
            &query,
            input,
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            &mut RefCache::new(),
            |_| async { Ok(data) },
            |_| async { panic!("no refs") },
        )
        .await
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual[0].entities.len(), 6);
    }

    #[tokio::test]
    async fn any_object_or_field_callback_keeps_the_snapshot_capable_path() {
        let (catalog, query, input, data) = fixture();
        let bindings = EvaluationBindings {
            auth: json!({"id":"me"}),
            ..Default::default()
        };
        for code in [
            json!({"posts":{"allow":{"view":"true || data.ref('owner.id') == []"}}}),
            json!({"posts":{"allow":{"view":"true"},"fields":{"secret":"data.ref('owner.id') == []"}}}),
            // Even an unfetched field with a callback is classified conservatively.
            json!({"posts":{"allow":{"view":"true"},"fields":{"unfetched":"data.ref('owner.id') == []"}}}),
            json!({"posts":{"allow":{"view":"true"}},"comments":{"allow":{"view":"data.ref('post.id') == []"}}}),
        ] {
            let rules = RuleSet::new(code);
            let fresh = data.clone();
            let prepared = prepare_owned_query_permissions(
                &rules,
                &query,
                input.clone(),
                &catalog,
                &bindings,
                QueryPermissionOptions::default(),
                |_| async { Ok(fresh) },
            )
            .await
            .unwrap();
            assert!(!prepared.can_release_snapshot());
        }
    }

    #[tokio::test]
    async fn fresh_preparations_do_not_reuse_loaded_records_auth_or_denial_decisions() {
        let (catalog, query, input, data) = fixture();
        let rules = RuleSet::new(
            json!({"posts":{"allow":{"view":"auth.id == data.ownerId"}},"comments":{"allow":{"view":"true"}}}),
        );
        for (identity, owner) in [
            ("me", "me"),
            ("other", "me"),
            ("other", "other"),
            ("me", "other"),
        ] {
            let bindings = EvaluationBindings {
                auth: json!({"id":identity}),
                ..Default::default()
            };
            let mut fresh = data.clone();
            for (key, value) in &mut fresh {
                if key.etype == "posts" {
                    value["ownerId"] = json!(owner);
                }
            }
            let prepared = prepare_owned_query_permissions(
                &rules,
                &query,
                input.clone(),
                &catalog,
                &bindings,
                QueryPermissionOptions::default(),
                |_| async { Ok(fresh) },
            )
            .await
            .unwrap();
            assert!(prepared.can_release_snapshot());
            let filtered = prepared
                .filter(&mut RefCache::new(), |_| async { panic!("no refs") })
                .await
                .unwrap();
            assert_eq!(
                filtered[0].entities.len(),
                if identity == owner { 12 } else { 0 }
            );
        }
    }

    #[tokio::test]
    async fn preparation_preserves_loader_validation_and_callback_error_contracts() {
        let (catalog, query, input, data) = fixture();
        let bindings = EvaluationBindings::default();
        let pure = RuleSet::new(json!({"posts":{"allow":{"view":"true"}}}));
        let result = prepare_owned_query_permissions(
            &pure,
            &query,
            input.clone(),
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            |_| async { Err(PermissionError::Loader("sentinel-loader-error".into())) },
        )
        .await;
        assert!(
            matches!(result,Err(PermissionError::Loader(message)) if message=="sentinel-loader-error")
        );
        let result = prepare_owned_query_permissions(
            &pure,
            &query,
            input.clone(),
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            |_| async { Ok(std::collections::HashMap::new()) },
        )
        .await;
        assert!(matches!(result, Err(PermissionError::MissingEntityData(_))));
        let callbacks =
            RuleSet::new(json!({"posts":{"allow":{"view":"data.ref('owner.id') == []"}}}));
        let prepared = prepare_owned_query_permissions(
            &callbacks,
            &query,
            input,
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            |_| async { Ok(data) },
        )
        .await
        .unwrap();
        assert!(!prepared.can_release_snapshot());
        let result = prepared
            .filter(&mut RefCache::new(), |_| async {
                Err(PermissionError::Loader("sentinel-reference-error".into()))
            })
            .await;
        assert!(
            matches!(result,Err(PermissionError::Loader(message)) if message=="sentinel-reference-error")
        );
    }

    #[tokio::test]
    async fn pure_evaluation_still_charges_unreferenced_complete_bindings() {
        let (catalog, query, input, mut data) = fixture();
        let rules = RuleSet::new(json!({"posts":{"allow":{"view":"true"}}}));
        let bindings = EvaluationBindings::default();
        for (key, value) in &mut data {
            if key.etype == "posts" {
                // Match the existing 4 MiB input budget rather than assuming
                // that a permitted 2 MiB string should be rejected.
                value["unreferenced"] = json!("x".repeat(4 * 1024 * 1024));
            }
        }
        let prepared = prepare_owned_query_permissions(
            &rules,
            &query,
            input.clone(),
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            |_| async { Ok(data.clone()) },
        )
        .await
        .unwrap();
        assert!(prepared.can_release_snapshot());
        let actual = prepared
            .filter(&mut RefCache::new(), |_| async { panic!("no refs") })
            .await
            .unwrap_err();
        let expected = filter_executed_query_owned(
            &rules,
            &query,
            input,
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            &mut RefCache::new(),
            |_| async { Ok(data) },
            |_| async { panic!("no refs") },
        )
        .await
        .unwrap_err();
        assert_eq!(actual.to_string(), expected.to_string());
    }
}
