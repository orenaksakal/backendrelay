//! Bounded exact-input memoization of immutable CEL compilation artifacts.
//! No database rule document, evaluation context, identity or decision is cached.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use crate::{PermissionAction, RuleProgram};

const MAX_ENTRIES: usize = 256;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_KEY_BYTES: usize = 16 * 1024;

#[derive(Eq, Hash, PartialEq)]
pub(super) struct ProgramKey {
    action: PermissionAction,
    source: String,
    // Bind order and every declared body remain part of validation semantics.
    binds: Vec<(String, String)>,
}

impl ProgramKey {
    pub(super) fn new(
        action: PermissionAction,
        source: &str,
        binds: &[(String, String)],
    ) -> Option<Self> {
        let bytes = source.len()
            + binds
                .iter()
                .map(|(name, body)| name.len() + body.len())
                .sum::<usize>();
        (bytes <= MAX_KEY_BYTES).then(|| Self {
            action,
            source: source.into(),
            binds: binds.to_vec(),
        })
    }

    fn weight(&self) -> usize {
        // Source text also lives in the returned RuleProgram. ASTs/metadata are
        // bounded indirectly by the input and entry caps, not an allocator-size
        // promise. Oversized rules compile normally without entering this cache.
        self.source.len() * 2
            + self
                .binds
                .iter()
                .map(|(name, body)| name.len() + body.len())
                .sum::<usize>()
    }
}

#[derive(Default)]
struct ProgramCache {
    programs: HashMap<Arc<ProgramKey>, RuleProgram>,
    insertion_order: VecDeque<Arc<ProgramKey>>,
    input_bytes: usize,
}

impl ProgramCache {
    fn get(&self, key: &ProgramKey) -> Option<RuleProgram> {
        self.programs.get(key).cloned()
    }

    fn insert(&mut self, key: ProgramKey, program: RuleProgram) -> RuleProgram {
        if let Some(existing) = self.get(&key) {
            return existing;
        }
        let weight = key.weight();
        while self.programs.len() >= MAX_ENTRIES
            || self.input_bytes.saturating_add(weight) > MAX_INPUT_BYTES
        {
            let Some(old) = self.insertion_order.pop_front() else {
                return program;
            };
            if self.programs.remove(&old).is_some() {
                self.input_bytes = self.input_bytes.saturating_sub(old.weight());
            }
        }
        let key = Arc::new(key);
        self.input_bytes += weight;
        self.insertion_order.push_back(Arc::clone(&key));
        self.programs.insert(key, program.clone());
        program
    }
}

static COMPILED: LazyLock<Mutex<ProgramCache>> =
    LazyLock::new(|| Mutex::new(ProgramCache::default()));

pub(super) fn get(key: &ProgramKey) -> Option<RuleProgram> {
    COMPILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
}

pub(super) fn insert(key: ProgramKey, program: RuleProgram) -> RuleProgram {
    // Parsing/validation happens outside this mutex. Racing cold callers may
    // compile the same pure input, then converge on one immutable artifact.
    COMPILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EvaluationBindings, PermissionError};
    use serde_json::json;

    #[test]
    fn exact_program_hits_share_ast_but_never_authorization_bindings_or_results() {
        let source = "auth.id != null && auth.id == data.ownerId";
        let a = RuleProgram::compile(PermissionAction::View, source).unwrap();
        let b = RuleProgram::compile(PermissionAction::View, source).unwrap();
        assert!(Arc::ptr_eq(&a.program, &b.program));
        let mut bindings = EvaluationBindings {
            auth: json!({"id":"user-a"}),
            data: json!({"ownerId":"user-a"}),
            ..Default::default()
        };
        assert_eq!(a.evaluate(&bindings).unwrap(), json!(true));
        bindings.auth = json!({"id":"user-b"});
        assert_eq!(b.evaluate(&bindings).unwrap(), json!(false));
        bindings.data = json!({"ownerId":"user-b"});
        assert_eq!(a.evaluate(&bindings).unwrap(), json!(true));
    }

    #[test]
    fn changed_source_binds_and_action_do_not_reuse_a_different_policy() {
        let allow = RuleProgram::compile_with_binds(
            PermissionAction::View,
            "selected",
            vec![("selected".into(), "true".into())],
        )
        .unwrap();
        let deny = RuleProgram::compile_with_binds(
            PermissionAction::View,
            "selected",
            vec![("selected".into(), "false".into())],
        )
        .unwrap();
        assert!(!Arc::ptr_eq(&allow.program, &deny.program));
        assert_eq!(
            allow.evaluate(&EvaluationBindings::default()).unwrap(),
            json!(true)
        );
        assert_eq!(
            deny.evaluate(&EvaluationBindings::default()).unwrap(),
            json!(false)
        );
        RuleProgram::compile(PermissionAction::Update, "newData.ownerId == auth.id").unwrap();
        assert!(matches!(
            RuleProgram::compile(PermissionAction::View, "newData.ownerId == auth.id"),
            Err(PermissionError::UndeclaredVariable(_))
        ));
        assert!(RuleProgram::compile(PermissionAction::View, "auth.id ==").is_err());
    }

    #[test]
    fn compiled_artifacts_have_hard_count_and_input_bounds() {
        let mut cache = ProgramCache::default();
        for n in 0..MAX_ENTRIES + 20 {
            let source = format!("true // distinct {n}");
            let key = ProgramKey::new(PermissionAction::View, &source, &[]).unwrap();
            let program =
                RuleProgram::compile_uncached(PermissionAction::View, source, Vec::new()).unwrap();
            cache.insert(key, program);
            assert!(cache.programs.len() <= MAX_ENTRIES);
            assert!(cache.input_bytes <= MAX_INPUT_BYTES);
        }
        assert!(
            ProgramKey::new(PermissionAction::View, &"x".repeat(MAX_KEY_BYTES + 1), &[]).is_none()
        );
        let old = ProgramKey::new(PermissionAction::View, "true // distinct 0", &[]).unwrap();
        assert!(cache.get(&old).is_none());
    }
}
