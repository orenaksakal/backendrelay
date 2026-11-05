//! CEL permission-rule compatibility for `InstantDB`.

mod attribute_projection;
#[cfg(test)]
mod binding_conversion_tests;
mod compiled_programs;
mod postgres;
#[cfg(test)]
mod query_bookkeeping_tests;
#[cfg(test)]
mod query_relationship_tests;
mod query_snapshot;
mod rate_limit;
mod rule_where;
mod transaction;

pub use postgres::*;
pub use rate_limit::*;
pub use rule_where::*;
pub use transaction::*;

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex};

use cel::common::ast::{CallExpr, ComprehensionExpr, EntryExpr, Expr, IdedExpr, LiteralValue};
use cel::extractors::{Arguments, This};
use cel::objects::{KeyRef, Map as CelMap};
use cel::{Context, ExecutionError, Program, Value};
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, FixedOffset, SecondsFormat, Timelike, Utc,
};
use chrono_tz::Tz;
use instant_instaql::{AttributeCatalog, CompiledNode, CompiledQuery, ExecutedNode, SYSTEM_ETYPES};
use instant_transact::{EntityLookup, TransactionStep, TripleStep};
use serde_json::{Map, Value as JsonValue};
use thiserror::Error;
use uuid::Uuid;

const BASE_VARIABLES: [&str; 6] = ["data", "auth", "ruleParams", "request", "rateLimit", "math"];
const REQUEST_FIELDS: [&str; 4] = ["modifiedFields", "time", "ip", "origin"];
const CEL_TYPE_IDENTIFIERS: [&str; 13] = [
    "int",
    "uint",
    "bool",
    "double",
    "bytes",
    "string",
    "dyn",
    "type",
    "null_type",
    "list",
    "map",
    "timestamp",
    "duration",
];

/// Maximum bracket/brace/parenthesis depth accepted in rule source and binds.
///
/// Parsing, validating and dropping a CEL expression recurse once per nesting
/// level, so a hostile rule could overflow the request thread's stack before the
/// work limit stops evaluation. The limit is checked by a non-recursive pre-scan.
/// Production runs rules on 16 MiB Tokio threads: measured on that stack, the
/// first shape to run out (nested comprehensions) survives 87 levels in a debug
/// build, while release builds and the other shapes reach the CEL parser's own
/// 96-level recursion limit first. Sixteen is below a quarter of the release
/// bound and more than five times below the debug bound, while leaving room for
/// ordinary rules.
pub const MAX_RULE_NESTING: usize = 16;

/// Maximum same-level operator count accepted in one expression or bind.
///
/// Long chains become left-deep ASTs, which the recursive CEL validators and
/// destructors would otherwise walk on the stack. Left-deep `+` and field
/// selection chains survive 176 levels in a debug build on the production
/// 16 MiB stack, and release builds reach the source budget first; 88 is half
/// of the debug bound.
pub const MAX_RULE_OPERATOR_CHAIN: usize = 88;

// Only stateless native function definitions are shared. Auth, records, refs,
// rate-limit callbacks/requests, request metadata and fuel remain per evaluation.
static PERMISSION_FUNCTIONS: LazyLock<Context<'static>> = LazyLock::new(|| {
    let mut context = Context::empty();
    register_stateless_functions(&mut context);
    add_string_extension_bindings(&mut context);
    context
});

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PermissionAction {
    View,
    Create,
    Update,
    Delete,
    Restore,
    Link,
    Unlink,
    Write,
}

impl PermissionAction {
    fn allows(self, variable: &str) -> bool {
        if BASE_VARIABLES.contains(&variable) || CEL_TYPE_IDENTIFIERS.contains(&variable) {
            return true;
        }
        match self {
            Self::View | Self::Delete | Self::Restore => false,
            Self::Link => matches!(variable, "newData" | "linkedData" | "actions"),
            Self::Unlink => matches!(variable, "newData" | "linkedData"),
            Self::Create | Self::Update | Self::Write => variable == "newData",
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Restore => "restore",
            Self::Link => "link",
            Self::Unlink => "unlink",
            Self::Write => "write",
        }
    }
}

/// Persisted application rule JSON with JVM-compatible lookup precedence.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuleSet {
    code: JsonValue,
}

impl RuleSet {
    #[must_use]
    pub fn new(code: JsonValue) -> Self {
        Self { code }
    }

    #[must_use]
    pub fn code(&self) -> &JsonValue {
        &self.code
    }

    /// Resolves an entity action through entity, entity-default, global, and system
    /// fallback rules in the same order as `instant.model.rule/get-program!`.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected expression or bind configuration is invalid.
    pub fn program(
        &self,
        etype: &str,
        action: PermissionAction,
    ) -> Result<Option<RuleProgram>, PermissionError> {
        let action_name = action.as_str();
        let paths = [
            [etype, "allow", action_name],
            [etype, "allow", "$default"],
            ["$default", "allow", action_name],
            ["$default", "allow", "$default"],
        ];
        for path in paths {
            if let Some(expression) = json_path(&self.code, &path) {
                return self.compile_selected(etype, action, expression).map(Some);
            }
        }
        system_fallback(etype, action)
            .map(|source| RuleProgram::compile(action, source))
            .transpose()
    }

    /// Resolves link/unlink field rules without falling through to global object rules.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected expression or bind configuration is invalid.
    pub fn relationship_program(
        &self,
        etype: &str,
        action: PermissionAction,
        label: &str,
    ) -> Result<Option<RuleProgram>, PermissionError> {
        if !matches!(action, PermissionAction::Link | PermissionAction::Unlink) {
            return Err(PermissionError::InvalidRules(
                "relationship rules require link or unlink action".to_owned(),
            ));
        }
        for path in [
            [etype, "allow", action.as_str(), label],
            [etype, "allow", action.as_str(), "$default"],
        ] {
            if let Some(expression) = json_path(&self.code, &path) {
                return self.compile_selected(etype, action, expression).map(Some);
            }
        }
        Ok(None)
    }

    /// Resolves a field-level view rule.
    ///
    /// # Errors
    ///
    /// Rejects `id` field rules and invalid selected expressions/binds.
    pub fn field_program(
        &self,
        etype: &str,
        field: &str,
    ) -> Result<Option<RuleProgram>, PermissionError> {
        let path = [etype, "fields", field];
        let Some(expression) = json_path(&self.code, &path) else {
            return Ok(None);
        };
        if field == "id" {
            return Err(PermissionError::FieldRuleForId(etype.to_owned()));
        }
        self.compile_selected(etype, PermissionAction::View, expression)
            .map(Some)
    }

    /// Applies object `view` and field-level rules to shaped entity objects.
    /// Entity order is preserved, denied entities are removed, and denied fields
    /// are removed from otherwise permitted objects.
    ///
    /// A missing object or field rule permits access, matching the transaction
    /// and query fallbacks in the JVM service. System namespaces still use their
    /// built-in object fallbacks.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid rules, non-object entity values, reference
    /// loading failures, evaluation failures, or non-boolean decisions.
    pub fn filter_view_entities<F>(
        &self,
        etype: &str,
        entities: &[JsonValue],
        base_bindings: &EvaluationBindings,
        refs: &mut RefCache,
        mut load: F,
    ) -> Result<Vec<JsonValue>, PermissionError>
    where
        F: FnMut(&[RefRequest]) -> Result<RefCache, PermissionError>,
    {
        for entity in entities {
            if !entity.is_object() {
                return Err(PermissionError::InvalidEntityData);
            }
        }

        let object_program = self.program(etype, PermissionAction::View)?;
        let object_results = if let Some(program) = &object_program {
            let items = entities
                .iter()
                .map(|entity| EntityEvaluationItem {
                    program,
                    base: base_bindings,
                    data: entity,
                    etype,
                })
                .collect::<Vec<_>>();
            evaluate_batch_items(&items, refs, &mut load)?
        } else {
            vec![true; entities.len()]
        };

        let mut permitted = entities
            .iter()
            .zip(object_results)
            .filter(|(_, allowed)| *allowed)
            .map(|(entity, _)| entity.clone())
            .collect::<Vec<_>>();

        let Some(fields) = json_path(&self.code, &[etype, "fields"]).and_then(JsonValue::as_object)
        else {
            return Ok(permitted);
        };
        let mut field_names = fields
            .iter()
            .filter(|(_, expression)| !expression.is_null())
            .map(|(field, _)| field.clone())
            .collect::<Vec<_>>();
        field_names.sort();
        let field_programs = field_names
            .into_iter()
            .map(|field| {
                self.field_program(etype, &field)?
                    .map(|program| (field, program))
                    .ok_or_else(|| {
                        PermissionError::InvalidRules(
                            "field rule disappeared during compilation".to_owned(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut field_items = Vec::new();
        let mut field_evaluations = Vec::new();
        for (entity_index, entity) in permitted.iter().enumerate() {
            let object = entity
                .as_object()
                .ok_or(PermissionError::InvalidEntityData)?;
            for (program_index, (field, program)) in field_programs.iter().enumerate() {
                if object.contains_key(field) {
                    field_evaluations.push((entity_index, program_index));
                    field_items.push(EntityEvaluationItem {
                        program,
                        base: base_bindings,
                        data: entity,
                        etype,
                    });
                }
            }
        }
        let field_results = evaluate_batch_items(&field_items, refs, &mut load)?;
        for ((entity_index, program_index), allowed) in
            field_evaluations.into_iter().zip(field_results)
        {
            if !allowed {
                permitted[entity_index]
                    .as_object_mut()
                    .ok_or(PermissionError::InvalidEntityData)?
                    .remove(&field_programs[program_index].0);
            }
        }
        Ok(permitted)
    }

    /// Asynchronous counterpart to [`Self::filter_view_entities`] for database-backed
    /// reference loading. This is used by BYOP queries, whose already-shaped rows do
    /// not pass through the triple-store execution tree.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid rules/entities, reference loading failures,
    /// evaluation failures, or non-boolean decisions.
    pub async fn filter_view_entities_async<F, Fut>(
        &self,
        etype: &str,
        entities: &[JsonValue],
        base_bindings: &EvaluationBindings,
        refs: &mut RefCache,
        mut load: F,
    ) -> Result<Vec<JsonValue>, PermissionError>
    where
        F: FnMut(Vec<RefRequest>) -> Fut,
        Fut: Future<Output = Result<RefCache, PermissionError>>,
    {
        for entity in entities {
            if !entity.is_object() {
                return Err(PermissionError::InvalidEntityData);
            }
        }

        let object_program = self.program(etype, PermissionAction::View)?;
        let object_results = if let Some(program) = &object_program {
            let items = entities
                .iter()
                .map(|entity| EntityEvaluationItem {
                    program,
                    base: base_bindings,
                    data: entity,
                    etype,
                })
                .collect::<Vec<_>>();
            evaluate_batch_items_async(&items, refs, &mut load).await?
        } else {
            vec![true; entities.len()]
        };

        let mut permitted = entities
            .iter()
            .zip(object_results)
            .filter(|(_, allowed)| *allowed)
            .map(|(entity, _)| entity.clone())
            .collect::<Vec<_>>();

        let Some(fields) = json_path(&self.code, &[etype, "fields"]).and_then(JsonValue::as_object)
        else {
            return Ok(permitted);
        };
        let mut field_names = fields
            .iter()
            .filter(|(_, expression)| !expression.is_null())
            .map(|(field, _)| field.clone())
            .collect::<Vec<_>>();
        field_names.sort();
        let field_programs = field_names
            .into_iter()
            .map(|field| {
                self.field_program(etype, &field)?
                    .map(|program| (field, program))
                    .ok_or_else(|| {
                        PermissionError::InvalidRules(
                            "field rule disappeared during compilation".to_owned(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut field_items = Vec::new();
        let mut field_evaluations = Vec::new();
        for (entity_index, entity) in permitted.iter().enumerate() {
            let object = entity
                .as_object()
                .ok_or(PermissionError::InvalidEntityData)?;
            for (program_index, (field, program)) in field_programs.iter().enumerate() {
                if object.contains_key(field) {
                    field_evaluations.push((entity_index, program_index));
                    field_items.push(EntityEvaluationItem {
                        program,
                        base: base_bindings,
                        data: entity,
                        etype,
                    });
                }
            }
        }
        let field_results = evaluate_batch_items_async(&field_items, refs, load).await?;
        for ((entity_index, program_index), allowed) in
            field_evaluations.into_iter().zip(field_results)
        {
            if !allowed {
                permitted[entity_index]
                    .as_object_mut()
                    .ok_or(PermissionError::InvalidEntityData)?
                    .remove(&field_programs[program_index].0);
            }
        }
        Ok(permitted)
    }

    fn compile_selected(
        &self,
        etype: &str,
        action: PermissionAction,
        expression: &JsonValue,
    ) -> Result<RuleProgram, PermissionError> {
        let program =
            self.compile_selected_without_rate_limit_validation(etype, action, expression)?;
        let configured = self.code.get("$rateLimits").and_then(JsonValue::as_object);
        for name in program.rate_limit_uses() {
            if configured.is_none_or(|configured| !configured.contains_key(name)) {
                return Err(PermissionError::UnknownRateLimit(name.clone()));
            }
        }
        Ok(program)
    }

    fn compile_selected_without_rate_limit_validation(
        &self,
        etype: &str,
        action: PermissionAction,
        expression: &JsonValue,
    ) -> Result<RuleProgram, PermissionError> {
        let source = expression_source(expression)?;
        let mut binds = normalize_binds(json_path(&self.code, &["$default", "bind"]))?;
        binds.extend(normalize_binds(json_path(&self.code, &[etype, "bind"]))?);
        RuleProgram::compile_with_binds(action, source, binds)
    }
}

/// Stable identity used to preload complete entity maps for permission checks.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PermissionEntityKey {
    pub etype: String,
    pub entity_id: Uuid,
}

/// One query-permission entity load with its proven attribute projection.
///
/// `attributes: None` keeps the complete-entity contract. `Some(ids)` is only
/// used when the compiled view program was statically proven to read no other
/// attribute (and no field rule needs the entity).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionEntityLoad {
    pub entities: Vec<PermissionEntityKey>,
    pub attributes: Option<Vec<Uuid>>,
}

/// Query/refresh permission filtering behavior.
///
/// Projection is on by default. The server's `disable-permission-attribute-
/// projection` control toggle turns it off without changing rule, reference or
/// decision semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryPermissionOptions {
    pub attribute_projection: bool,
}

impl Default for QueryPermissionOptions {
    fn default() -> Self {
        Self {
            attribute_projection: true,
        }
    }
}

/// Stable, transport-neutral description of the selected permission rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionProgramInfo {
    pub etype: String,
    pub action: PermissionAction,
    pub code: String,
    pub display_code: String,
}

/// One object or field decision produced by a debug query permission pass.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryPermissionCheckResult {
    pub entity_id: Uuid,
    pub etype: String,
    pub label: Option<String>,
    pub record: JsonValue,
    pub program: Option<PermissionProgramInfo>,
    pub allowed: bool,
}

/// Filtered execution tree plus every permission decision used to produce it.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryPermissionAudit {
    pub results: Vec<ExecutedNode>,
    pub checks: Vec<QueryPermissionCheckResult>,
}

struct EntityPrograms {
    view: Option<RuleProgram>,
    fields: HashMap<String, RuleProgram>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DecisionKey {
    View(PermissionEntityKey),
    Field(PermissionEntityKey, String),
}

struct EvaluationSpec {
    decision: DecisionKey,
}

/// Applies permission decisions to a complete nested `InstaQL` execution tree
/// before public response shaping.
///
/// Complete entity maps are loaded once for every entity whose view or fetched
/// field has a rule. CEL reference paths are then loaded in coalesced async
/// rounds. Denied entities, denied triple rows, and descendants of denied
/// entities are removed while invalidation topics remain attached.
///
/// # Errors
///
/// Returns an error for malformed query/result trees, unknown attributes,
/// missing preloaded entities, invalid rules, CEL failures, or loader failures.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn filter_executed_query<EF, EFut, RF, RFut>(
    rules: &RuleSet,
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    base_bindings: &EvaluationBindings,
    options: QueryPermissionOptions,
    refs: &mut RefCache,
    load_entities: EF,
    load_refs: RF,
) -> Result<Vec<ExecutedNode>, PermissionError>
where
    EF: FnOnce(Vec<PermissionEntityLoad>) -> EFut,
    EFut: Future<Output = Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError>>,
    RF: FnMut(Vec<RefRequest>) -> RFut,
    RFut: Future<Output = Result<RefCache, PermissionError>>,
{
    evaluate_executed_query(
        rules,
        query,
        Cow::Borrowed(results),
        catalog,
        base_bindings,
        options,
        refs,
        load_entities,
        load_refs,
        false,
    )
    .await
    .map(|audit| audit.results)
}

/// Filters an owned execution tree without a redundant full-tree copy.
///
/// This runs the exact same validation, fresh entity/reference loads, CEL
/// decisions and descendant filtering as [`filter_executed_query`]. The caller
/// gives up the input tree; no permission decision or result is cached.
///
/// # Errors
///
/// Returns the same errors as [`filter_executed_query`].
#[allow(clippy::too_many_arguments)]
pub async fn filter_executed_query_owned<EF, EFut, RF, RFut>(
    rules: &RuleSet,
    query: &CompiledQuery,
    results: Vec<ExecutedNode>,
    catalog: &AttributeCatalog,
    base_bindings: &EvaluationBindings,
    options: QueryPermissionOptions,
    refs: &mut RefCache,
    load_entities: EF,
    load_refs: RF,
) -> Result<Vec<ExecutedNode>, PermissionError>
where
    EF: FnOnce(Vec<PermissionEntityLoad>) -> EFut,
    EFut: Future<Output = Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError>>,
    RF: FnMut(Vec<RefRequest>) -> RFut,
    RFut: Future<Output = Result<RefCache, PermissionError>>,
{
    evaluate_executed_query(
        rules,
        query,
        Cow::Owned(results),
        catalog,
        base_bindings,
        options,
        refs,
        load_entities,
        load_refs,
        false,
    )
    .await
    .map(|audit| audit.results)
}

/// Runs the post-query permission pass and retains the complete debug decision set.
///
/// Unlike the normal filter, this loads complete records for entities with no
/// selected rule too, matching the admin sandbox's inspectable record contract.
///
/// # Errors
///
/// Returns the same query/result, rule, CEL, and loader failures as
/// [`filter_executed_query`].
#[allow(clippy::too_many_arguments)]
pub async fn audit_executed_query<EF, EFut, RF, RFut>(
    rules: &RuleSet,
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    base_bindings: &EvaluationBindings,
    options: QueryPermissionOptions,
    refs: &mut RefCache,
    load_entities: EF,
    load_refs: RF,
) -> Result<QueryPermissionAudit, PermissionError>
where
    EF: FnOnce(Vec<PermissionEntityLoad>) -> EFut,
    EFut: Future<Output = Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError>>,
    RF: FnMut(Vec<RefRequest>) -> RFut,
    RFut: Future<Output = Result<RefCache, PermissionError>>,
{
    evaluate_executed_query(
        rules,
        query,
        Cow::Borrowed(results),
        catalog,
        base_bindings,
        options,
        refs,
        load_entities,
        load_refs,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn evaluate_executed_query<EF, EFut, RF, RFut>(
    rules: &RuleSet,
    query: &CompiledQuery,
    results: Cow<'_, [ExecutedNode]>,
    catalog: &AttributeCatalog,
    base_bindings: &EvaluationBindings,
    options: QueryPermissionOptions,
    refs: &mut RefCache,
    load_entities: EF,
    load_refs: RF,
    audit: bool,
) -> Result<QueryPermissionAudit, PermissionError>
where
    EF: FnOnce(Vec<PermissionEntityLoad>) -> EFut,
    EFut: Future<Output = Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError>>,
    RF: FnMut(Vec<RefRequest>) -> RFut,
    RFut: Future<Output = Result<RefCache, PermissionError>>,
{
    let prepared = prepare_query_permissions(
        rules,
        query,
        results,
        catalog,
        base_bindings,
        options,
        load_entities,
        audit,
    )
    .await?;
    finish_query_permissions(prepared, refs, load_refs).await
}

/// One request's permission inputs, loaded from its original read snapshot.
///
/// This value is consumed by evaluation. It is not a permission/entity cache:
/// every preparation performs the same fresh entity read (projected only when
/// the compiled view rule and catalog prove the exact attribute set) and
/// validates the same tree, rules and records as the ordinary permission filter.
pub struct PreparedQueryPermissions<'a> {
    query: &'a CompiledQuery,
    results: Cow<'a, [ExecutedNode]>,
    catalog: &'a AttributeCatalog,
    base_bindings: &'a EvaluationBindings,
    programs: HashMap<String, EntityPrograms>,
    entity_data: Cow<'a, HashMap<PermissionEntityKey, JsonValue>>,
    ordered_subjects: Vec<(PermissionEntityKey, HashSet<&'a str>)>,
    audit: bool,
}

impl PreparedQueryPermissions<'_> {
    /// Whether every compiled object/field rule and reachable bind is free of
    /// native reference and rate-limit callbacks, including dynamic/dead calls.
    ///
    /// Callers must additionally keep any separate query-level rate limits on
    /// their original path. Closing the snapshot never bypasses CEL evaluation.
    #[must_use]
    pub fn can_release_snapshot(&self) -> bool {
        self.programs.values().all(|programs| {
            programs
                .view
                .iter()
                .chain(programs.fields.values())
                .all(|program| !query_snapshot::program_has_database_callbacks(program))
        })
    }

    /// Runs the ordinary fresh CEL batch, filtering and error checks exactly once.
    ///
    /// # Errors
    ///
    /// Returns the same evaluation, budget, reference and result errors as the
    /// ordinary permission filter. Callback-capable programs still need their
    /// original snapshot-backed loader; no callback result is invented here.
    pub async fn filter<F, Fut>(
        self,
        refs: &mut RefCache,
        load_refs: F,
    ) -> Result<Vec<ExecutedNode>, PermissionError>
    where
        F: FnMut(Vec<RefRequest>) -> Fut,
        Fut: Future<Output = Result<RefCache, PermissionError>>,
    {
        finish_query_permissions(self, refs, load_refs)
            .await
            .map(|audit| audit.results)
    }
}

/// Prepares the owned query filter without evaluating or reusing a decision.
///
/// # Errors
///
/// Preserves the ordinary filter's tree/rule validation and complete fresh entity
/// load errors. The resulting value borrows only request inputs, not the loader
/// or its database transaction, so a proven CPU-only evaluation can outlive it.
pub async fn prepare_owned_query_permissions<'a, F, Fut>(
    rules: &RuleSet,
    query: &'a CompiledQuery,
    results: Vec<ExecutedNode>,
    catalog: &'a AttributeCatalog,
    base_bindings: &'a EvaluationBindings,
    options: QueryPermissionOptions,
    load_entities: F,
) -> Result<PreparedQueryPermissions<'a>, PermissionError>
where
    F: FnOnce(Vec<PermissionEntityLoad>) -> Fut,
    Fut: Future<Output = Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError>>,
{
    prepare_query_permissions(
        rules,
        query,
        Cow::Owned(results),
        catalog,
        base_bindings,
        options,
        load_entities,
        false,
    )
    .await
}

/// Prepares the query filter from entity data the caller already owns.
///
/// The shared root-frame path keeps every session's read-only entity map next
/// to its frame, so preparation borrows both the tree and the map instead of
/// deep-cloning them per session. Validation, programs, ordering and decisions
/// are identical to [`prepare_owned_query_permissions`]; only retained output
/// is cloned by [`PreparedQueryPermissions::filter`].
///
/// # Errors
///
/// Preserves the ordinary filter's tree/rule validation and complete entity
/// validation errors.
#[allow(clippy::too_many_arguments)]
pub fn prepare_borrowed_query_permissions<'a>(
    rules: &RuleSet,
    query: &'a CompiledQuery,
    results: &'a [ExecutedNode],
    catalog: &'a AttributeCatalog,
    base_bindings: &'a EvaluationBindings,
    entity_data: &'a HashMap<PermissionEntityKey, JsonValue>,
    options: QueryPermissionOptions,
) -> Result<PreparedQueryPermissions<'a>, PermissionError> {
    let (subjects, programs, entity_loads) = collect_query_permission_scope(
        rules,
        query,
        results,
        catalog,
        options.attribute_projection,
        false,
    )?;
    validate_entity_data(&entity_loads, entity_data)?;

    let mut ordered_subjects = subjects.into_iter().collect::<Vec<_>>();
    ordered_subjects.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(PreparedQueryPermissions {
        query,
        results: Cow::Borrowed(results),
        catalog,
        base_bindings,
        programs,
        entity_data: Cow::Borrowed(entity_data),
        ordered_subjects,
        audit: false,
    })
}

/// Computes the permission entity loads a query and its executed results need.
///
/// The result is independent of evaluation bindings: it is derived only from the
/// compiled query, its results, the catalog and the rule programs. Callers may
/// load the returned batch once and serve later per-session preparation from
/// that data through [`prepare_owned_query_permissions`].
///
/// # Errors
///
/// Returns the same tree/rule/catalog validation errors as the ordinary filter's
/// preparation, before any entity data is requested.
pub fn query_permission_entity_loads(
    rules: &RuleSet,
    query: &CompiledQuery,
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    project_attributes: bool,
) -> Result<Vec<PermissionEntityLoad>, PermissionError> {
    collect_query_permission_scope(rules, query, results, catalog, project_attributes, false)
        .map(|(_, _, entity_loads)| entity_loads)
}

/// Whether every view and field program reachable for the query's entity types
/// is free of native `ref`/`limit` callbacks.
///
/// This is a conservative, binding-independent superset of
/// [`PreparedQueryPermissions::can_release_snapshot`]: it inspects every declared
/// field rule for the queried entity types, whether or not the current results
/// fetch that field, and never loads entity data. A rule that does not compile is
/// reported as not provable so callers keep their existing path and error timing.
#[must_use]
pub fn query_programs_have_no_database_callbacks(rules: &RuleSet, query: &CompiledQuery) -> bool {
    let mut etypes = HashSet::new();
    collect_compiled_etypes(&query.roots, &mut etypes);
    let mut etypes = etypes.into_iter().collect::<Vec<_>>();
    etypes.sort();
    for etype in etypes {
        match rules.program(&etype, PermissionAction::View) {
            Ok(Some(program)) if query_snapshot::program_has_database_callbacks(&program) => {
                return false;
            }
            Ok(_) => {}
            // Keep the ordinary path responsible for reporting malformed rules.
            Err(_) => return false,
        }
        let Some(fields) =
            json_path(rules.code(), &[&etype, "fields"]).and_then(JsonValue::as_object)
        else {
            continue;
        };
        for field in fields
            .iter()
            .filter(|(_, expression)| !expression.is_null())
            .map(|(field, _)| field)
        {
            match rules.field_program(&etype, field) {
                Ok(Some(program)) if query_snapshot::program_has_database_callbacks(&program) => {
                    return false;
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
    }
    true
}

fn collect_compiled_etypes(nodes: &[CompiledNode], etypes: &mut HashSet<String>) {
    for node in nodes {
        etypes.insert(node.etype.clone());
        collect_compiled_etypes(&node.children, etypes);
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_query_permissions<'a, F, Fut>(
    rules: &RuleSet,
    query: &'a CompiledQuery,
    results: Cow<'a, [ExecutedNode]>,
    catalog: &'a AttributeCatalog,
    base_bindings: &'a EvaluationBindings,
    options: QueryPermissionOptions,
    load_entities: F,
    audit: bool,
) -> Result<PreparedQueryPermissions<'a>, PermissionError>
where
    F: FnOnce(Vec<PermissionEntityLoad>) -> Fut,
    Fut: Future<Output = Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError>>,
{
    let (subjects, programs, entity_loads) = collect_query_permission_scope(
        rules,
        query,
        &results,
        catalog,
        options.attribute_projection && !audit,
        audit,
    )?;
    let entity_data = load_entities(entity_loads.clone()).await?;
    validate_entity_data(&entity_loads, &entity_data)?;

    let mut ordered_subjects = subjects.into_iter().collect::<Vec<_>>();
    ordered_subjects.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(PreparedQueryPermissions {
        query,
        results,
        catalog,
        base_bindings,
        programs,
        entity_data: Cow::Owned(entity_data),
        ordered_subjects,
        audit,
    })
}

fn validate_entity_data(
    entity_loads: &[PermissionEntityLoad],
    entity_data: &HashMap<PermissionEntityKey, JsonValue>,
) -> Result<(), PermissionError> {
    for load in entity_loads {
        for key in &load.entities {
            let value = entity_data
                .get(key)
                .ok_or_else(|| PermissionError::MissingEntityData(key.clone()))?;
            if !value.is_object() {
                return Err(PermissionError::InvalidEntityData);
            }
        }
    }
    Ok(())
}

fn collect_query_permission_scope<'a>(
    rules: &RuleSet,
    query: &'a CompiledQuery,
    results: &[ExecutedNode],
    catalog: &'a AttributeCatalog,
    project_attributes: bool,
    audit: bool,
) -> Result<
    (
        HashMap<PermissionEntityKey, HashSet<&'a str>>,
        HashMap<String, EntityPrograms>,
        Vec<PermissionEntityLoad>,
    ),
    PermissionError,
> {
    // Labels live in this request's immutable catalog. Borrow them throughout
    // subject collection instead of allocating one name per fetched triple.
    let mut subjects = HashMap::<PermissionEntityKey, HashSet<&str>>::new();
    collect_permission_subjects(&query.roots, results, catalog, &mut subjects)?;

    let mut etypes = subjects
        .keys()
        .map(|key| key.etype.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    etypes.sort();
    let mut programs = HashMap::new();
    for etype in etypes {
        let view = rules.program(&etype, PermissionAction::View)?;
        let mut fields = HashMap::new();
        if let Some(field_rules) =
            json_path(rules.code(), &[&etype, "fields"]).and_then(JsonValue::as_object)
        {
            let mut names = field_rules
                .iter()
                .filter(|(_, expression)| !expression.is_null())
                .map(|(field, _)| field.clone())
                .collect::<Vec<_>>();
            names.sort();
            for field in names {
                if let Some(program) = rules.field_program(&etype, &field)? {
                    fields.insert(field, program);
                }
            }
        }
        programs.insert(etype, EntityPrograms { view, fields });
    }

    let mut required_entities = subjects
        .iter()
        .filter(|(key, labels)| {
            if audit {
                return true;
            }
            let entity_programs = &programs[&key.etype];
            entity_programs.view.is_some()
                || labels
                    .iter()
                    .any(|label| entity_programs.fields.contains_key(*label))
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    required_entities.sort();

    // Group entities that share an exact attribute set. Every entity whose
    // view program cannot be proven (or whose fetched label has a field rule)
    // stays in the complete group. The view proof depends only on the etype's
    // compiled program, so walk each etype's AST once per request.
    let mut view_projections = HashMap::<String, Option<Vec<Uuid>>>::new();
    let mut complete = Vec::new();
    let mut projected = std::collections::BTreeMap::<Vec<Uuid>, Vec<PermissionEntityKey>>::new();
    for key in required_entities {
        let entity_programs = &programs[&key.etype];
        let labels = &subjects[&key];
        let view_projection = view_projections
            .entry(key.etype.clone())
            .or_insert_with(|| {
                entity_programs.view.as_ref().and_then(|program| {
                    attribute_projection::projected_attribute_ids(program, &key.etype, catalog)
                })
            })
            .clone();
        let projection = if project_attributes
            && !labels
                .iter()
                .any(|label| entity_programs.fields.contains_key(*label))
        {
            view_projection
        } else {
            None
        };
        match projection {
            Some(attributes) => projected.entry(attributes).or_default().push(key),
            None => complete.push(key),
        }
    }
    let mut entity_loads = Vec::with_capacity(1 + projected.len());
    if !complete.is_empty() {
        entity_loads.push(PermissionEntityLoad {
            entities: complete,
            attributes: None,
        });
    }
    entity_loads.extend(
        projected
            .into_iter()
            .map(|(attributes, entities)| PermissionEntityLoad {
                entities,
                attributes: Some(attributes),
            }),
    );
    Ok((subjects, programs, entity_loads))
}

async fn finish_query_permissions<F, Fut>(
    prepared: PreparedQueryPermissions<'_>,
    refs: &mut RefCache,
    load_refs: F,
) -> Result<QueryPermissionAudit, PermissionError>
where
    F: FnMut(Vec<RefRequest>) -> Fut,
    Fut: Future<Output = Result<RefCache, PermissionError>>,
{
    let PreparedQueryPermissions {
        query,
        results,
        catalog,
        base_bindings,
        programs,
        entity_data,
        ordered_subjects,
        audit,
    } = prepared;
    let mut specs = Vec::new();
    let mut items = Vec::new();
    for (key, labels) in &ordered_subjects {
        let entity_programs = &programs[&key.etype];
        let Some(data) = entity_data.get(key) else {
            continue;
        };
        if let Some(program) = entity_programs.view.as_ref() {
            specs.push(EvaluationSpec {
                decision: DecisionKey::View(key.clone()),
            });
            items.push(EntityEvaluationItem {
                program,
                base: base_bindings,
                data,
                etype: &key.etype,
            });
        }
        let mut labels = labels.iter().copied().collect::<Vec<_>>();
        labels.sort();
        for field in labels {
            if let Some(program) = entity_programs.fields.get(field) {
                specs.push(EvaluationSpec {
                    decision: DecisionKey::Field(key.clone(), field.to_owned()),
                });
                items.push(EntityEvaluationItem {
                    program,
                    base: base_bindings,
                    data,
                    etype: &key.etype,
                });
            }
        }
    }
    let values = evaluate_batch_items_async(&items, refs, load_refs).await?;
    let decisions = specs
        .into_iter()
        .zip(values)
        .map(|(spec, value)| (spec.decision, value))
        .collect::<HashMap<_, _>>();

    let filtered = match results {
        Cow::Owned(mut results) => {
            filter_executed_level(&query.roots, &mut results, catalog, &decisions)?;
            results
        }
        Cow::Borrowed(results) => {
            filter_executed_level_borrowed(&query.roots, results, catalog, &decisions)?
        }
    };
    let mut checks = Vec::new();
    if audit {
        for (key, labels) in ordered_subjects {
            let record = entity_data
                .get(&key)
                .ok_or_else(|| PermissionError::MissingEntityData(key.clone()))?
                .clone();
            let entity_programs = &programs[&key.etype];
            checks.push(QueryPermissionCheckResult {
                entity_id: key.entity_id,
                etype: key.etype.clone(),
                label: None,
                record: record.clone(),
                program: entity_programs
                    .view
                    .as_ref()
                    .map(|program| permission_program_info(&key.etype, program)),
                allowed: decisions
                    .get(&DecisionKey::View(key.clone()))
                    .copied()
                    .unwrap_or(true),
            });
            let mut labels = labels.into_iter().collect::<Vec<_>>();
            labels.sort();
            for label in labels {
                let Some(program) = entity_programs.fields.get(label) else {
                    continue;
                };
                checks.push(QueryPermissionCheckResult {
                    entity_id: key.entity_id,
                    etype: key.etype.clone(),
                    label: Some(label.to_owned()),
                    record: record.clone(),
                    program: Some(permission_program_info(&key.etype, program)),
                    allowed: decisions
                        .get(&DecisionKey::Field(key.clone(), label.to_owned()))
                        .copied()
                        .unwrap_or(true),
                });
            }
        }
    }
    Ok(QueryPermissionAudit {
        results: filtered,
        checks,
    })
}

fn permission_program_info(etype: &str, program: &RuleProgram) -> PermissionProgramInfo {
    PermissionProgramInfo {
        etype: etype.to_owned(),
        action: program.action(),
        code: program.source().to_owned(),
        display_code: program.source().to_owned(),
    }
}

fn collect_permission_subjects<'catalog>(
    nodes: &[CompiledNode],
    results: &[ExecutedNode],
    catalog: &'catalog AttributeCatalog,
    subjects: &mut HashMap<PermissionEntityKey, HashSet<&'catalog str>>,
) -> Result<(), PermissionError> {
    if nodes.len() != results.len() {
        return Err(PermissionError::QueryResultCardinality {
            expected: nodes.len(),
            actual: results.len(),
        });
    }
    for (node, result) in nodes.iter().zip(results) {
        for entity in &result.entities {
            let entity_id = result_entity_id(&entity.fetch)?;
            let key = PermissionEntityKey {
                etype: node.etype.clone(),
                entity_id,
            };
            let labels = subjects.entry(key).or_default();
            for triple in entity.fetch.join_rows.iter().flatten() {
                let attribute = catalog
                    .by_id(triple.attr_id)
                    .ok_or(PermissionError::UnknownAttribute(triple.attr_id))?;
                labels.insert(attribute.label.as_str());
            }
            collect_permission_subjects(&node.children, &entity.children, catalog, subjects)?;
        }
    }
    Ok(())
}

fn result_entity_id(result: &instant_datalog::DatalogResult) -> Result<Uuid, PermissionError> {
    result
        .join_rows
        .iter()
        .flatten()
        .next()
        .map(|triple| triple.entity_id)
        .ok_or(PermissionError::EmptyEntityResult)
}

fn filter_executed_level(
    nodes: &[CompiledNode],
    results: &mut [ExecutedNode],
    catalog: &AttributeCatalog,
    decisions: &HashMap<DecisionKey, bool>,
) -> Result<(), PermissionError> {
    // This is derived only from the current evaluation's completed decisions,
    // not from a cached policy. With no field decisions, retain the same catalog
    // validation but avoid constructing two owned strings for every triple.
    let has_field_decisions = decisions
        .keys()
        .any(|key| matches!(key, DecisionKey::Field(..)));
    filter_executed_level_with_fields(nodes, results, catalog, decisions, has_field_decisions)
}

fn filter_executed_level_with_fields(
    nodes: &[CompiledNode],
    results: &mut [ExecutedNode],
    catalog: &AttributeCatalog,
    decisions: &HashMap<DecisionKey, bool>,
    has_field_decisions: bool,
) -> Result<(), PermissionError> {
    if nodes.len() != results.len() {
        return Err(PermissionError::QueryResultCardinality {
            expected: nodes.len(),
            actual: results.len(),
        });
    }
    for (node, result) in nodes.iter().zip(results) {
        let mut retained = Vec::new();
        let mut allowed_ids = HashSet::new();
        for mut entity in std::mem::take(&mut result.entities) {
            let entity_id = result_entity_id(&entity.fetch)?;
            let key = PermissionEntityKey {
                etype: node.etype.clone(),
                entity_id,
            };
            if !decisions
                .get(&DecisionKey::View(key.clone()))
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            entity.fetch.join_rows.retain(|row| {
                row.iter().all(|triple| {
                    catalog.by_id(triple.attr_id).is_some_and(|attribute| {
                        !has_field_decisions
                            || decisions
                                .get(&DecisionKey::Field(key.clone(), attribute.label.clone()))
                                .copied()
                                .unwrap_or(true)
                    })
                })
            });
            if entity.fetch.join_rows.is_empty() {
                continue;
            }
            filter_executed_level_with_fields(
                &node.children,
                &mut entity.children,
                catalog,
                decisions,
                has_field_decisions,
            )?;
            allowed_ids.insert(entity_id);
            retained.push(entity);
        }
        result.entities = retained;
        result.selector.join_rows.retain(|row| {
            selector_row_entity_id(node, row).is_some_and(|id| allowed_ids.contains(&id))
        });
        if let Some(page_info) = &mut result.selector.page_info {
            let cursor_attr_id = page_info
                .start_cursor
                .as_ref()
                .or(page_info.end_cursor.as_ref())
                .map(|cursor| cursor.attr_id);
            if let Some(attr_id) = cursor_attr_id {
                page_info.start_cursor = result
                    .selector
                    .join_rows
                    .first()
                    .and_then(|row| page_cursor_from_row(row, attr_id));
                page_info.end_cursor = result
                    .selector
                    .join_rows
                    .last()
                    .and_then(|row| page_cursor_from_row(row, attr_id));
            }
        }
    }
    Ok(())
}

/// Borrowed counterpart of [`filter_executed_level_with_fields`].
///
/// Shared frames are read by every session, so filtering must not clone a
/// complete executed tree just to discard most of it. Only retained entities,
/// retained selector rows and node-level metadata are cloned; denied entities
/// are skipped before any copy.
fn filter_executed_level_borrowed(
    nodes: &[CompiledNode],
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    decisions: &HashMap<DecisionKey, bool>,
) -> Result<Vec<ExecutedNode>, PermissionError> {
    let has_field_decisions = decisions
        .keys()
        .any(|key| matches!(key, DecisionKey::Field(..)));
    filter_executed_level_borrowed_with_fields(
        nodes,
        results,
        catalog,
        decisions,
        has_field_decisions,
    )
}

fn filter_executed_level_borrowed_with_fields(
    nodes: &[CompiledNode],
    results: &[ExecutedNode],
    catalog: &AttributeCatalog,
    decisions: &HashMap<DecisionKey, bool>,
    has_field_decisions: bool,
) -> Result<Vec<ExecutedNode>, PermissionError> {
    if nodes.len() != results.len() {
        return Err(PermissionError::QueryResultCardinality {
            expected: nodes.len(),
            actual: results.len(),
        });
    }
    let mut filtered = Vec::with_capacity(results.len());
    for (node, result) in nodes.iter().zip(results) {
        let mut retained = Vec::new();
        let mut allowed_ids = HashSet::new();
        for entity in &result.entities {
            let entity_id = result_entity_id(&entity.fetch)?;
            let key = PermissionEntityKey {
                etype: node.etype.clone(),
                entity_id,
            };
            if !decisions
                .get(&DecisionKey::View(key.clone()))
                .copied()
                .unwrap_or(true)
            {
                continue;
            }
            let mut entity = entity.clone();
            entity.fetch.join_rows.retain(|row| {
                row.iter().all(|triple| {
                    catalog.by_id(triple.attr_id).is_some_and(|attribute| {
                        !has_field_decisions
                            || decisions
                                .get(&DecisionKey::Field(key.clone(), attribute.label.clone()))
                                .copied()
                                .unwrap_or(true)
                    })
                })
            });
            if entity.fetch.join_rows.is_empty() {
                continue;
            }
            let children = filter_executed_level_borrowed_with_fields(
                &node.children,
                &entity.children,
                catalog,
                decisions,
                has_field_decisions,
            )?;
            entity.children = children;
            allowed_ids.insert(entity_id);
            retained.push(entity);
        }
        let mut selector = result.selector.clone();
        selector.join_rows.retain(|row| {
            selector_row_entity_id(node, row).is_some_and(|id| allowed_ids.contains(&id))
        });
        if let Some(page_info) = &mut selector.page_info {
            let cursor_attr_id = page_info
                .start_cursor
                .as_ref()
                .or(page_info.end_cursor.as_ref())
                .map(|cursor| cursor.attr_id);
            if let Some(attr_id) = cursor_attr_id {
                page_info.start_cursor = selector
                    .join_rows
                    .first()
                    .and_then(|row| page_cursor_from_row(row, attr_id));
                page_info.end_cursor = selector
                    .join_rows
                    .last()
                    .and_then(|row| page_cursor_from_row(row, attr_id));
            }
        }
        filtered.push(ExecutedNode {
            selector,
            aggregate: result.aggregate,
            entities: retained,
        });
    }
    Ok(filtered)
}

fn selector_row_entity_id(
    node: &CompiledNode,
    row: &[instant_datalog::DatalogTriple],
) -> Option<Uuid> {
    let Some(relationship) = node.relationship.as_ref().filter(|link| link.forward) else {
        return row.first().map(|triple| triple.entity_id);
    };
    // Forward child selectors start with [parent, link, child]. Permission
    // decisions are for the child, whose ID is in the value, not the entity
    // position. Removing that edge leaves SDK relationship filters empty even
    // though both entities were allowed. Reverse selectors already start with
    // the child's entity ID.
    let triple = row
        .iter()
        .find(|triple| triple.attr_id == relationship.attr_id)?;
    match &triple.value {
        instant_datalog::DatalogValue::Uuid(id) => Some(*id),
        instant_datalog::DatalogValue::Json(value) => {
            value.as_str().and_then(|value| Uuid::parse_str(value).ok())
        }
    }
}

fn page_cursor_from_row(
    row: &[instant_datalog::DatalogTriple],
    attr_id: Uuid,
) -> Option<instant_datalog::PageCursorResult> {
    let triple = row
        .iter()
        .find(|triple| triple.attr_id == attr_id)
        .or_else(|| row.first())?;
    Some(instant_datalog::PageCursorResult {
        entity_id: triple.entity_id,
        attr_id: triple.attr_id,
        value: Some(triple.value.clone()),
        created_at: triple.created_at,
    })
}

/// Original and locally projected entity maps used by transaction permission
/// planning. Create checks still run against read-your-writes database state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TransactionEntityMaps {
    pub original: HashMap<PermissionEntityKey, JsonValue>,
    pub updated: HashMap<PermissionEntityKey, JsonValue>,
    pub rule_params: HashMap<PermissionEntityKey, JsonValue>,
}

/// Projects resolved transaction steps into CEL `newData` maps and accumulates
/// per-entity `ruleParams`.
///
/// # Errors
///
/// Returns an error for unresolved lookups, unknown attributes, missing entity
/// types, or non-object entity values.
pub fn project_transaction_entities<I>(
    catalog: &AttributeCatalog,
    steps: &[TransactionStep],
    original: I,
) -> Result<TransactionEntityMaps, PermissionError>
where
    I: IntoIterator<Item = (PermissionEntityKey, JsonValue)>,
{
    let original = original.into_iter().collect::<HashMap<_, _>>();
    let mut updated = original.clone();
    let mut rule_params = HashMap::<PermissionEntityKey, Map<String, JsonValue>>::new();
    for step in steps {
        match step {
            TransactionStep::AddTriple(step) => {
                project_triple(catalog, &mut updated, step, false)?;
            }
            TransactionStep::DeepMergeTriple(step) => {
                project_triple(catalog, &mut updated, step, true)?;
            }
            TransactionStep::RetractTriple(step) => {
                let entity_id = resolved_entity_id(&step.entity)?;
                let attribute = catalog
                    .by_id(step.attr_id)
                    .ok_or(PermissionError::UnknownAttribute(step.attr_id))?;
                let key = PermissionEntityKey {
                    etype: step
                        .etype
                        .clone()
                        .unwrap_or_else(|| attribute.etype.clone()),
                    entity_id,
                };
                if let Some(entity) = updated.get_mut(&key) {
                    entity
                        .as_object_mut()
                        .ok_or(PermissionError::InvalidEntityData)?
                        .remove(&attribute.label);
                }
            }
            TransactionStep::RuleParams(step) => {
                let entity_id = resolved_entity_id(&step.entity)?;
                let etype = step
                    .etype
                    .clone()
                    .ok_or(PermissionError::MissingTransactionEntityType)?;
                let key = PermissionEntityKey { etype, entity_id };
                rule_params
                    .entry(key)
                    .or_default()
                    .extend(step.params.clone());
            }
            TransactionStep::DeleteEntity(_)
            | TransactionStep::AddAttr(_)
            | TransactionStep::UpdateAttr(_)
            | TransactionStep::DeleteAttr(_)
            | TransactionStep::RestoreAttr(_) => {}
        }
    }
    Ok(TransactionEntityMaps {
        original,
        updated,
        rule_params: rule_params
            .into_iter()
            .map(|(key, params)| (key, JsonValue::Object(params)))
            .collect(),
    })
}

fn project_triple(
    catalog: &AttributeCatalog,
    updated: &mut HashMap<PermissionEntityKey, JsonValue>,
    step: &TripleStep,
    deep_merge: bool,
) -> Result<(), PermissionError> {
    let entity_id = resolved_entity_id(&step.entity)?;
    let attribute = catalog
        .by_id(step.attr_id)
        .ok_or(PermissionError::UnknownAttribute(step.attr_id))?;
    let value = projected_value(attribute, &step.value)?;
    let key = PermissionEntityKey {
        etype: step
            .etype
            .clone()
            .unwrap_or_else(|| attribute.etype.clone()),
        entity_id,
    };
    let object = updated
        .entry(key)
        .or_insert_with(|| JsonValue::Object(Map::new()))
        .as_object_mut()
        .ok_or(PermissionError::InvalidEntityData)?;
    let value = if deep_merge {
        deep_merge_and_delete(
            object.remove(&attribute.label).unwrap_or(JsonValue::Null),
            value,
        )
    } else {
        value
    };
    object.insert(attribute.label.clone(), value);
    Ok(())
}

fn projected_value(
    attribute: &instant_instaql::Attribute,
    value: &JsonValue,
) -> Result<JsonValue, PermissionError> {
    let lookupable =
        attribute.value_type == instant_instaql::AttributeValueType::Ref || attribute.label == "id";
    if lookupable
        && value.as_array().is_some_and(|parts| {
            parts.len() == 2
                && parts[0]
                    .as_str()
                    .is_some_and(|value| Uuid::parse_str(value).is_ok())
        })
    {
        return Err(PermissionError::UnresolvedTransactionLookup);
    }
    Ok(value.clone())
}

fn resolved_entity_id(entity: &EntityLookup) -> Result<Uuid, PermissionError> {
    match entity {
        EntityLookup::Id(entity_id) => Ok(*entity_id),
        EntityLookup::Lookup(_) => Err(PermissionError::UnresolvedTransactionLookup),
    }
}

fn deep_merge_and_delete(original: JsonValue, updates: JsonValue) -> JsonValue {
    let JsonValue::Object(mut original) = original else {
        return updates;
    };
    let JsonValue::Object(updates) = updates else {
        return updates;
    };
    for (key, value) in updates {
        if value.is_null() {
            original.remove(&key);
        } else if value.is_object() && original.get(&key).is_some_and(JsonValue::is_object) {
            let current = original.remove(&key).unwrap_or(JsonValue::Null);
            original.insert(key, deep_merge_and_delete(current, value));
        } else {
            original.insert(key, value);
        }
    }
    JsonValue::Object(original)
}

/// Returns distinct modified field labels in first-step order, excluding `id`.
///
/// # Errors
///
/// Returns an error for unknown attributes or unresolved entity lookups.
pub fn transaction_modified_fields(
    entity_id: Uuid,
    steps: &[TransactionStep],
    catalog: &AttributeCatalog,
) -> Result<Vec<String>, PermissionError> {
    let mut fields = Vec::new();
    for step in steps {
        let (TransactionStep::AddTriple(step) | TransactionStep::DeepMergeTriple(step)) = step
        else {
            continue;
        };
        if resolved_entity_id(&step.entity)? != entity_id {
            continue;
        }
        let label = &catalog
            .by_id(step.attr_id)
            .ok_or(PermissionError::UnknownAttribute(step.attr_id))?
            .label;
        if label != "id" && !fields.contains(label) {
            fields.push(label.clone());
        }
    }
    Ok(fields)
}

fn json_path<'a>(value: &'a JsonValue, path: &[&str]) -> Option<&'a JsonValue> {
    path.iter()
        .try_fold(value, |current, key| current.get(key))
        .filter(|value| !value.is_null())
}

fn expression_source(value: &JsonValue) -> Result<String, PermissionError> {
    match value {
        JsonValue::Bool(value) => Ok(value.to_string()),
        JsonValue::String(value) => Ok(value.clone()),
        _ => Err(PermissionError::InvalidRules(
            "permission expression must be a string or boolean".to_owned(),
        )),
    }
}

fn normalize_binds(value: Option<&JsonValue>) -> Result<Vec<(String, String)>, PermissionError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        JsonValue::Object(values) => values
            .iter()
            .map(|(name, expression)| Ok((name.clone(), expression_source(expression)?)))
            .collect(),
        JsonValue::Array(values) => {
            let (pairs, remainder) = values.as_chunks::<2>();
            let binds = pairs
                .iter()
                .map(|pair| {
                    let name = pair[0].as_str().ok_or_else(|| {
                        PermissionError::InvalidRules("bind name must be a string".to_owned())
                    })?;
                    Ok((name.to_owned(), expression_source(&pair[1])?))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if remainder.is_empty() {
                Ok(binds)
            } else {
                Err(PermissionError::InvalidRules(
                    "bind should have an even number of elements".to_owned(),
                ))
            }
        }
        _ => Err(PermissionError::InvalidRules(
            "bind must be an object or array".to_owned(),
        )),
    }
}

fn system_fallback(etype: &str, action: PermissionAction) -> Option<&'static str> {
    if !SYSTEM_ETYPES.contains(&etype) {
        return None;
    }
    match (etype, action) {
        ("$users", PermissionAction::Create) => Some("true"),
        ("$users", PermissionAction::View | PermissionAction::Update) => Some(
            "auth.id == data.id || (data.linkedPrimaryUser != null && auth.id == data.linkedPrimaryUser)",
        ),
        _ => Some("false"),
    }
}

#[derive(Clone, Debug)]
pub struct RuleProgram {
    source: String,
    action: PermissionAction,
    bind_programs: Arc<Vec<(String, Program)>>,
    program: Arc<Program>,
    selected_paths: Arc<HashMap<String, Vec<Vec<String>>>>,
    reference_uses: Arc<Vec<ReferenceUse>>,
    rate_limit_uses: Arc<Vec<String>>,
}

impl RuleProgram {
    /// Parses a permission expression and validates Instant's action-specific bindings.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionError::Parse`] for invalid CEL or a validation error for
    /// undeclared roots and unsupported `request` fields.
    pub fn compile(
        action: PermissionAction,
        source: impl Into<String>,
    ) -> Result<Self, PermissionError> {
        Self::compile_with_binds(action, source, Vec::new())
    }

    /// Compiles a rule with entity/default bind definitions and topologically orders
    /// only the bindings reachable from the final expression.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate/cyclic binds or invalid CEL.
    pub fn compile_with_binds(
        action: PermissionAction,
        source: impl Into<String>,
        binds: Vec<(String, String)>,
    ) -> Result<Self, PermissionError> {
        let source = source.into();
        // Bound work before normalization, parsing, and recursive validation.
        if source.len() > 65_536
            || binds.len() > 128
            || binds
                .iter()
                .any(|(name, body)| name.len() > 256 || body.len() > 65_536)
            || binds
                .iter()
                .map(|(name, body)| name.len() + body.len())
                .sum::<usize>()
                .saturating_add(source.len())
                > 262_144
        {
            return Err(PermissionError::Parse(
                "Permission source budget exceeded".into(),
            ));
        }
        // This memoizes pure compilation only. The caller still resolves fresh
        // rule JSON and evaluates with fresh request/entity/ref bindings. Never
        // retain a permission result, auth value, reference value or CEL context.
        let key = compiled_programs::ProgramKey::new(action, &source, &binds);
        if let Some(key) = &key
            && let Some(program) = compiled_programs::get(key)
        {
            return Ok(program);
        }
        let program = Self::compile_uncached(action, source, binds)?;
        if let Some(key) = key {
            return Ok(compiled_programs::insert(key, program));
        }
        Ok(program)
    }

    fn compile_uncached(
        action: PermissionAction,
        source: String,
        binds: Vec<(String, String)>,
    ) -> Result<Self, PermissionError> {
        Self::compile_uncached_inner(action, source, binds, true)
    }

    /// Compiles without the pre-scan so the depth measurement can find where the
    /// CEL parser and recursive validators actually run out of stack.
    #[cfg(test)]
    fn compile_uncached_for_measurement(
        action: PermissionAction,
        source: String,
        binds: Vec<(String, String)>,
    ) -> Result<Self, PermissionError> {
        Self::compile_uncached_inner(action, source, binds, false)
    }

    fn compile_uncached_inner(
        action: PermissionAction,
        source: String,
        binds: Vec<(String, String)>,
        enforce_shape_limit: bool,
    ) -> Result<Self, PermissionError> {
        let normalized_source = normalize_adjacent_nots(&source);
        if enforce_shape_limit {
            validate_rule_shape(&normalized_source)?;
        }
        let validation_program = Program::compile(&normalized_source)
            .map_err(|error| PermissionError::Parse(error.to_string()))?;
        let bind_names = binds
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        if bind_names.len() != binds.len() {
            return Err(PermissionError::DuplicateBind);
        }
        validate_expression(action, validation_program.expression(), &bind_names)?;
        let mut parsed_binds = HashMap::new();
        let mut bind_runtime_sources = HashMap::new();
        for (name, bind_source) in binds {
            let normalized_bind_source = normalize_adjacent_nots(&bind_source);
            if enforce_shape_limit {
                validate_rule_shape(&normalized_bind_source)?;
            }
            let bind_program = Program::compile(&normalized_bind_source)
                .map_err(|error| PermissionError::Parse(error.to_string()))?;
            bind_runtime_sources.insert(name.clone(), normalized_bind_source);
            parsed_binds.insert(name, bind_program);
        }
        let bind_order = bind_order(validation_program.expression(), &parsed_binds)?;
        let mut static_bindings = bind_names
            .iter()
            .map(|name| (name.clone(), StaticType::Dynamic))
            .collect::<HashMap<_, _>>();
        for name in &bind_order {
            let bind_program = parsed_binds
                .get(name)
                .ok_or_else(|| PermissionError::UndeclaredVariable(name.clone()))?;
            validate_node(action, bind_program.expression(), &static_bindings)?;
            let bind_type = static_type(bind_program.expression(), &static_bindings);
            static_bindings.insert(name.clone(), bind_type);
        }
        validate_node(action, validation_program.expression(), &static_bindings)?;
        let mut selected_paths = HashMap::new();
        let mut presence_paths = HashSet::new();
        collect_selected_paths(validation_program.expression(), &mut selected_paths);
        collect_presence_paths(validation_program.expression(), &mut presence_paths);
        let mut reference_uses = HashSet::new();
        collect_reference_uses(validation_program.expression(), &mut reference_uses);
        let mut rate_limit_uses = HashSet::new();
        collect_rate_limit_uses(validation_program.expression(), &mut rate_limit_uses);
        let mut bind_programs = Vec::with_capacity(bind_order.len());
        for name in bind_order {
            let bind_program = parsed_binds
                .remove(&name)
                .ok_or_else(|| PermissionError::UndeclaredVariable(name.clone()))?;
            validate_expression(action, bind_program.expression(), &bind_names)?;
            collect_selected_paths(bind_program.expression(), &mut selected_paths);
            collect_presence_paths(bind_program.expression(), &mut presence_paths);
            collect_reference_uses(bind_program.expression(), &mut reference_uses);
            collect_rate_limit_uses(bind_program.expression(), &mut rate_limit_uses);
            let runtime_source = bind_runtime_sources
                .remove(&name)
                .ok_or_else(|| PermissionError::UndeclaredVariable(name.clone()))?;
            let runtime_program =
                Program::compile(&normalize_standard_runtime_calls(&runtime_source))
                    .map_err(|error| PermissionError::Parse(error.to_string()))?;
            bind_programs.push((name, runtime_program));
        }
        for (root, path) in presence_paths {
            if let Some(paths) = selected_paths.get_mut(&root) {
                paths.retain(|selected| selected != &path);
            }
        }
        let mut reference_uses = reference_uses.into_iter().collect::<Vec<_>>();
        reference_uses
            .sort_by(|left, right| (&left.target, &left.path).cmp(&(&right.target, &right.path)));
        let mut rate_limit_uses = rate_limit_uses.into_iter().collect::<Vec<_>>();
        rate_limit_uses.sort();
        let program = Program::compile(&normalize_standard_runtime_calls(&normalized_source))
            .map_err(|error| PermissionError::Parse(error.to_string()))?;
        Ok(Self {
            source,
            action,
            bind_programs: Arc::new(bind_programs),
            program: Arc::new(program),
            selected_paths: Arc::new(selected_paths),
            reference_uses: Arc::new(reference_uses),
            rate_limit_uses: Arc::new(rate_limit_uses),
        })
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub const fn action(&self) -> PermissionAction {
        self.action
    }

    /// Literal `ref` paths used by this rule and its reachable binds.
    #[must_use]
    pub fn reference_uses(&self) -> &[ReferenceUse] {
        &self.reference_uses
    }

    /// Named `$rateLimits` buckets referenced by reachable rule expressions.
    #[must_use]
    pub fn rate_limit_uses(&self) -> &[String] {
        &self.rate_limit_uses
    }

    /// Evaluates a rule with request and entity bindings.
    ///
    /// # Errors
    ///
    /// Returns an error when binding serialization or CEL evaluation fails.
    pub fn evaluate(&self, bindings: &EvaluationBindings) -> Result<JsonValue, PermissionError> {
        match self.advance(bindings, &RefCache::new())? {
            EvaluationOutcome::Value(value) => Ok(value),
            EvaluationOutcome::NeedsRefs(requests) => {
                Err(PermissionError::MissingReferences(requests.len()))
            }
        }
    }

    /// Advances evaluation using already-loaded reference values.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed bindings or CEL failures other than a missing
    /// `data.ref`/`auth.ref` value.
    pub fn advance(
        &self,
        bindings: &EvaluationBindings,
        refs: &RefCache,
    ) -> Result<EvaluationOutcome, PermissionError> {
        self.advance_with_budget(bindings, refs, &mut 100_000)
    }

    fn advance_with_budget(
        &self,
        bindings: &EvaluationBindings,
        refs: &RefCache,
        remaining: &mut u64,
    ) -> Result<EvaluationOutcome, PermissionError> {
        self.advance_view(&BindingView::from_bindings(bindings), refs, remaining)
    }

    /// Evaluates against a shared base binding set with only `data` replaced.
    ///
    /// This is the batched entity path's alternative to cloning the complete
    /// `EvaluationBindings` (including auth and rule parameters) per entity.
    fn advance_entity_with_budget(
        &self,
        base: &EvaluationBindings,
        data: &JsonValue,
        etype: &str,
        refs: &RefCache,
        remaining: &mut u64,
    ) -> Result<EvaluationOutcome, PermissionError> {
        self.advance_view(&BindingView::for_entity(base, data, etype), refs, remaining)
    }

    fn advance_view(
        &self,
        bindings: &BindingView<'_>,
        refs: &RefCache,
        remaining: &mut u64,
    ) -> Result<EvaluationOutcome, PermissionError> {
        let mut input_budget = 4 * 1024 * 1024;
        for value in [
            bindings.data,
            bindings.auth,
            bindings.rule_params,
            bindings.rate_limit,
        ]
        .into_iter()
        .chain(bindings.new_data)
        .chain(bindings.linked_data)
        .chain(bindings.actions)
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
        let ref_values = Arc::new(ref_values);
        let missing_refs = Arc::new(Mutex::new(HashSet::new()));
        let rate_limit_requests = Arc::new(Mutex::new(Vec::new()));
        let mut context = PERMISSION_FUNCTIONS.new_function_scope();
        // One budget for binds and the final rule, inherited by every CEL scope.
        // Charges expression visits and sized values; shared across reference retries.
        // This is a deterministic work budget, not a wall-clock or I/O deadline.
        context.set_work_limit(*remaining);
        register_functions(
            &mut context,
            Arc::clone(&ref_values),
            Arc::clone(&missing_refs),
            bindings.rate_limit_runtime.cloned(),
            Arc::clone(&rate_limit_requests),
        );
        add_json_binding(
            &mut context,
            "data",
            bindings.data,
            &self.selected_paths,
            bindings.data_etype,
        )?;
        add_json_binding(
            &mut context,
            "auth",
            bindings.auth,
            &self.selected_paths,
            Some("$users"),
        )?;
        add_json_binding(
            &mut context,
            "ruleParams",
            bindings.rule_params,
            &self.selected_paths,
            None,
        )?;
        add_json_binding(
            &mut context,
            "rateLimit",
            bindings.rate_limit,
            &self.selected_paths,
            None,
        )?;
        add_math_binding(&mut context)?;
        add_request_binding(&mut context, bindings.request)?;
        if let Some(value) = bindings.new_data {
            add_json_binding(&mut context, "newData", value, &self.selected_paths, None)?;
        }
        if let Some(value) = bindings.linked_data {
            add_json_binding(
                &mut context,
                "linkedData",
                value,
                &self.selected_paths,
                bindings.linked_data_etype,
            )?;
        }
        if let Some(value) = bindings.actions {
            add_json_binding(&mut context, "actions", value, &self.selected_paths, None)?;
        }

        for (name, program) in self.bind_programs.iter() {
            match program.execute(&context) {
                Ok(value) => context.add_variable_from_value(name, value),
                Err(error) => {
                    *remaining = context.work_remaining();
                    return evaluation_error(&error, &missing_refs);
                }
            }
        }
        let outcome = self.program.execute(&context);
        *remaining = context.work_remaining();
        match outcome {
            Ok(value) => {
                consume_rate_limits_view(bindings, &rate_limit_requests)?;
                value
                    .json()
                    .map(EvaluationOutcome::Value)
                    .map_err(|error| PermissionError::Evaluation(error.to_string()))
            }
            Err(error) => evaluation_error(&error, &missing_refs),
        }
    }

    /// Evaluates a rule and enforces the permission decision's boolean return type.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionError::InvalidReturnType`] for non-boolean rules.
    pub fn evaluate_bool(&self, bindings: &EvaluationBindings) -> Result<bool, PermissionError> {
        self.evaluate(bindings)?
            .as_bool()
            .ok_or(PermissionError::InvalidReturnType)
    }
}

fn validate_rule_shape(source: &str) -> Result<(), PermissionError> {
    // A bounded, non-recursive scan runs before the CEL parser: brackets bound
    // the parser/AST recursion depth and operators bound left-deep AST walks.
    let characters = source.chars().collect::<Vec<_>>();
    let mut index = 0;
    let mut brackets = Vec::new();
    let mut operators = vec![0_usize];
    let mut in_comment = false;
    while index < characters.len() {
        let character = characters[index];
        if in_comment {
            if character == '\n' {
                in_comment = false;
            }
            index += 1;
            continue;
        }
        if character == '/' && characters.get(index + 1) == Some(&'/') {
            in_comment = true;
            index += 2;
            continue;
        }
        if matches!(character, '\'' | '"') {
            let quote = character;
            let triple = characters.get(index + 1) == Some(&quote)
                && characters.get(index + 2) == Some(&quote);
            index += if triple { 3 } else { 1 };
            let mut escaped = false;
            while index < characters.len() {
                let current = characters[index];
                if escaped {
                    escaped = false;
                } else if current == '\\' {
                    escaped = true;
                } else if current == quote
                    && (!triple
                        || (characters.get(index + 1) == Some(&quote)
                            && characters.get(index + 2) == Some(&quote)))
                {
                    index += if triple { 3 } else { 1 };
                    break;
                }
                index += 1;
            }
            continue;
        }
        match character {
            '(' | '[' | '{' => {
                brackets.push(character);
                if brackets.len() > MAX_RULE_NESTING {
                    return Err(PermissionError::Parse(format!(
                        "permission rule nesting exceeds {MAX_RULE_NESTING} levels"
                    )));
                }
                operators.push(0);
            }
            ')' | ']' | '}' => {
                let matches_open = brackets.last().is_some_and(|open| {
                    matches!((open, character), ('(', ')') | ('[', ']') | ('{', '}'))
                });
                if matches_open {
                    brackets.pop();
                    operators.pop();
                }
            }
            '+' | '-' | '*' | '/' | '%' | '!' | '<' | '>' | '=' | '&' | '|' | '?' | ':' | '.' => {
                let count = operators
                    .last_mut()
                    .expect("expression shape tracking starts with one operator scope");
                *count += 1;
                if *count > MAX_RULE_OPERATOR_CHAIN {
                    return Err(PermissionError::Parse(format!(
                        "permission rule operator chain exceeds {MAX_RULE_OPERATOR_CHAIN} operators"
                    )));
                }
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

// cel-rust tokenizes adjacent `!!` as one unary not. CEL/JVM treats every `!`
// independently, so parenthesize repeated unary-not operands before parsing.
fn normalize_adjacent_nots(source: &str) -> String {
    let characters = source.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(source.len());
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < characters.len() {
        let character = characters[index];
        if let Some(active_quote) = quote {
            normalized.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            normalized.push(character);
            index += 1;
            continue;
        }
        if character == '!' && characters.get(index + 1) == Some(&'!') {
            let mut not_count = 0;
            while characters.get(index + not_count) == Some(&'!') {
                not_count += 1;
            }
            let mut operand_start = index + not_count;
            while characters
                .get(operand_start)
                .is_some_and(|character| character.is_whitespace())
            {
                operand_start += 1;
            }
            let operand_end = unary_operand_end(&characters, operand_start);
            for _ in 0..not_count {
                normalized.push_str("!(");
            }
            normalized.extend(characters[operand_start..operand_end].iter());
            for _ in 0..not_count {
                normalized.push(')');
            }
            index = operand_end;
            continue;
        }
        normalized.push(character);
        index += 1;
    }
    normalized
}

fn standard_runtime_alias(name: &str) -> Option<&'static str> {
    match name {
        "bool" => Some("_instant_bool"),
        "int" => Some("_instant_int"),
        "uint" => Some("_instant_uint"),
        "string" => Some("_instant_string"),
        "duration" => Some("_instant_duration"),
        "timestamp" => Some("_instant_timestamp"),
        "matches" => Some("_instant_matches"),
        "getFullYear" => Some("_instant_getFullYear"),
        "getMonth" => Some("_instant_getMonth"),
        "getDayOfYear" => Some("_instant_getDayOfYear"),
        "getDayOfMonth" => Some("_instant_getDayOfMonth"),
        "getDate" => Some("_instant_getDate"),
        "getDayOfWeek" => Some("_instant_getDayOfWeek"),
        "getHours" => Some("_instant_getHours"),
        "getMinutes" => Some("_instant_getMinutes"),
        "getSeconds" => Some("_instant_getSeconds"),
        "getMilliseconds" => Some("_instant_getMilliseconds"),
        _ => None,
    }
}

fn normalize_standard_runtime_calls(source: &str) -> String {
    let characters = source.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(source.len());
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    while index < characters.len() {
        let character = characters[index];
        if let Some(active_quote) = quote {
            normalized.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            normalized.push(character);
            index += 1;
            continue;
        }
        if character.is_ascii_alphabetic() || character == '_' {
            let start = index;
            index += 1;
            while characters
                .get(index)
                .is_some_and(|character| character.is_ascii_alphanumeric() || *character == '_')
            {
                index += 1;
            }
            let name = characters[start..index].iter().collect::<String>();
            let mut next = index;
            while characters
                .get(next)
                .is_some_and(|character| character.is_whitespace())
            {
                next += 1;
            }
            if characters.get(next) == Some(&'(')
                && let Some(alias) = standard_runtime_alias(&name)
            {
                normalized.push_str(alias);
            } else {
                normalized.push_str(&name);
            }
            continue;
        }
        normalized.push(character);
        index += 1;
    }
    normalized
}

fn unary_operand_end(characters: &[char], start: usize) -> usize {
    let mut index = start;
    let mut depths = [0_u32; 3];
    let mut quote = None;
    let mut escaped = false;
    while index < characters.len() {
        let character = characters[index];
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => depths[0] += 1,
            '[' => depths[1] += 1,
            '{' => depths[2] += 1,
            ')' if depths[0] > 0 => depths[0] -= 1,
            ']' if depths[1] > 0 => depths[1] -= 1,
            '}' if depths[2] > 0 => depths[2] -= 1,
            _ if depths == [0, 0, 0]
                && (character.is_whitespace()
                    || matches!(
                        character,
                        '&' | '|' | '=' | '<' | '>' | '+' | '-' | '*' | '/'
                    )) =>
            {
                break;
            }
            _ => {}
        }
        index += 1;
    }
    index
}

fn evaluation_error(
    error: &ExecutionError,
    missing_refs: &Mutex<HashSet<RefRequest>>,
) -> Result<EvaluationOutcome, PermissionError> {
    if let ExecutionError::FunctionError { function, message } = error
        && is_string_extension_name(function)
    {
        return Err(PermissionError::Evaluation(message.clone()));
    }
    if !matches!(
        &error,
        ExecutionError::FunctionError { function, .. } if function == "ref"
    ) {
        return Err(PermissionError::Evaluation(error.to_string()));
    }
    let mut requests = missing_refs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    requests.sort_by(|left, right| {
        (&left.etype, &left.entity_id, &left.path).cmp(&(
            &right.etype,
            &right.entity_id,
            &right.path,
        ))
    });
    if requests.is_empty() {
        Err(PermissionError::Evaluation(
            "ref function failed without a missing reference".to_owned(),
        ))
    } else {
        Ok(EvaluationOutcome::NeedsRefs(requests))
    }
}

fn is_string_extension_name(name: &str) -> bool {
    matches!(
        name,
        "charAt"
            | "indexOf"
            | "join"
            | "lastIndexOf"
            | "lowerAscii"
            | "replace"
            | "split"
            | "substring"
            | "trim"
            | "upperAscii"
    )
}

#[derive(Clone, Debug, Default)]
pub struct EvaluationBindings {
    pub data: JsonValue,
    pub data_etype: Option<String>,
    pub auth: JsonValue,
    pub rule_params: JsonValue,
    pub request: RequestBinding,
    pub rate_limit: JsonValue,
    #[doc(hidden)]
    pub rate_limit_runtime: Option<RateLimitRuntime>,
    #[doc(hidden)]
    pub deferred_rate_limit_etypes: HashSet<String>,
    pub new_data: Option<JsonValue>,
    pub linked_data: Option<JsonValue>,
    pub linked_data_etype: Option<String>,
    pub actions: Option<JsonValue>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RefEntityId {
    Uuid(Uuid),
    String(String),
    Int(i64),
    UInt(u64),
    Float(u64),
    Bool(bool),
}

impl RefEntityId {
    #[must_use]
    pub const fn as_uuid(&self) -> Option<Uuid> {
        match self {
            Self::Uuid(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_json(&self) -> JsonValue {
        match self {
            Self::Uuid(value) => JsonValue::String(value.to_string()),
            Self::String(value) => JsonValue::String(value.clone()),
            Self::Int(value) => JsonValue::Number((*value).into()),
            Self::UInt(value) => JsonValue::Number((*value).into()),
            Self::Float(bits) => serde_json::Number::from_f64(f64::from_bits(*bits))
                .map_or(JsonValue::Null, JsonValue::Number),
            Self::Bool(value) => JsonValue::Bool(*value),
        }
    }
}

impl From<Uuid> for RefEntityId {
    fn from(value: Uuid) -> Self {
        Self::Uuid(value)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RefRequest {
    pub entity_id: RefEntityId,
    pub etype: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ReferenceUse {
    pub target: String,
    pub path: String,
}

pub type RefCache = HashMap<RefRequest, Vec<JsonValue>>;

#[derive(Clone, Debug, PartialEq)]
pub enum EvaluationOutcome {
    Value(JsonValue),
    NeedsRefs(Vec<RefRequest>),
}

/// One permission evaluation in a shared reference-loading batch.
#[derive(Clone, Copy, Debug)]
pub struct EvaluationItem<'a> {
    pub program: &'a RuleProgram,
    pub bindings: &'a EvaluationBindings,
}

/// One entity evaluation that borrows the request's base bindings and the
/// entity record instead of cloning both per entity.
#[derive(Clone, Copy, Debug)]
struct EntityEvaluationItem<'a> {
    program: &'a RuleProgram,
    base: &'a EvaluationBindings,
    data: &'a JsonValue,
    etype: &'a str,
}

trait EvaluationBatchItem {
    fn advance_with_budget(
        &self,
        refs: &RefCache,
        remaining: &mut u64,
    ) -> Result<EvaluationOutcome, PermissionError>;
}

impl EvaluationBatchItem for EvaluationItem<'_> {
    fn advance_with_budget(
        &self,
        refs: &RefCache,
        remaining: &mut u64,
    ) -> Result<EvaluationOutcome, PermissionError> {
        self.program
            .advance_with_budget(self.bindings, refs, remaining)
    }
}

impl EvaluationBatchItem for EntityEvaluationItem<'_> {
    fn advance_with_budget(
        &self,
        refs: &RefCache,
        remaining: &mut u64,
    ) -> Result<EvaluationOutcome, PermissionError> {
        self.program
            .advance_entity_with_budget(self.base, self.data, self.etype, refs, remaining)
    }
}

fn evaluate_batch_items<I, F>(
    items: &[I],
    refs: &mut RefCache,
    mut load: F,
) -> Result<Vec<bool>, PermissionError>
where
    I: EvaluationBatchItem,
    F: FnMut(&[RefRequest]) -> Result<RefCache, PermissionError>,
{
    let mut results = vec![None; items.len()];
    let mut budgets = vec![100_000; items.len()];
    let mut pending = (0..items.len()).collect::<Vec<_>>();

    while !pending.is_empty() {
        let mut rerun = Vec::new();
        let mut missing = HashSet::new();
        for index in pending {
            let item = &items[index];
            match item.advance_with_budget(refs, &mut budgets[index])? {
                EvaluationOutcome::Value(value) => {
                    results[index] =
                        Some(value.as_bool().ok_or(PermissionError::InvalidReturnType)?);
                }
                EvaluationOutcome::NeedsRefs(requests) => {
                    missing.extend(requests);
                    rerun.push(index);
                }
            }
        }

        if missing.is_empty() {
            break;
        }
        let mut missing = missing.into_iter().collect::<Vec<_>>();
        sort_ref_requests(&mut missing);
        refs.extend(load(&missing)?);
        let unresolved = missing
            .iter()
            .filter(|request| !refs.contains_key(*request))
            .count();
        if unresolved != 0 {
            return Err(PermissionError::UnresolvedReferences(unresolved));
        }
        pending = rerun;
    }

    collect_batch_results(results)
}

async fn evaluate_batch_items_async<I, F, Fut>(
    items: &[I],
    refs: &mut RefCache,
    mut load: F,
) -> Result<Vec<bool>, PermissionError>
where
    I: EvaluationBatchItem,
    F: FnMut(Vec<RefRequest>) -> Fut,
    Fut: Future<Output = Result<RefCache, PermissionError>>,
{
    let mut results = vec![None; items.len()];
    let mut budgets = vec![100_000; items.len()];
    let mut pending = (0..items.len()).collect::<Vec<_>>();

    while !pending.is_empty() {
        let mut rerun = Vec::new();
        let mut missing = HashSet::new();
        for index in pending {
            let item = &items[index];
            match item.advance_with_budget(refs, &mut budgets[index])? {
                EvaluationOutcome::Value(value) => {
                    results[index] =
                        Some(value.as_bool().ok_or(PermissionError::InvalidReturnType)?);
                }
                EvaluationOutcome::NeedsRefs(requests) => {
                    missing.extend(requests);
                    rerun.push(index);
                }
            }
        }

        if missing.is_empty() {
            break;
        }
        let mut missing = missing.into_iter().collect::<Vec<_>>();
        sort_ref_requests(&mut missing);
        refs.extend(load(missing.clone()).await?);
        let unresolved = missing
            .iter()
            .filter(|request| !refs.contains_key(*request))
            .count();
        if unresolved != 0 {
            return Err(PermissionError::UnresolvedReferences(unresolved));
        }
        pending = rerun;
    }

    collect_batch_results(results)
}

/// Evaluates permission decisions while coalescing unresolved `ref` calls into
/// shared loader rounds. Loaded values are retained in `refs` for later batches.
///
/// The loader must include every requested key in its returned cache. An empty
/// reference result is represented by an entry whose value is an empty vector.
///
/// # Errors
///
/// Returns an evaluation/type error, propagates a loader error, or reports a
/// loader response that omitted one of the requested references.
pub fn evaluate_batch<F>(
    items: &[EvaluationItem<'_>],
    refs: &mut RefCache,
    load: F,
) -> Result<Vec<bool>, PermissionError>
where
    F: FnMut(&[RefRequest]) -> Result<RefCache, PermissionError>,
{
    evaluate_batch_items(items, refs, load)
}

/// Async counterpart to [`evaluate_batch`] for PostgreSQL-backed reference loads.
/// Requests are passed by value so the loader future can own them across await
/// points without boxing or borrowing the evaluator stack.
///
/// # Errors
///
/// Returns the same errors as [`evaluate_batch`] and propagates async loader
/// failures.
pub async fn evaluate_batch_async<F, Fut>(
    items: &[EvaluationItem<'_>],
    refs: &mut RefCache,
    load: F,
) -> Result<Vec<bool>, PermissionError>
where
    F: FnMut(Vec<RefRequest>) -> Fut,
    Fut: Future<Output = Result<RefCache, PermissionError>>,
{
    evaluate_batch_items_async(items, refs, load).await
}

fn sort_ref_requests(requests: &mut [RefRequest]) {
    requests.sort_by(|left, right| {
        (&left.etype, &left.entity_id, &left.path).cmp(&(
            &right.etype,
            &right.entity_id,
            &right.path,
        ))
    });
}

fn collect_batch_results(results: Vec<Option<bool>>) -> Result<Vec<bool>, PermissionError> {
    results
        .into_iter()
        .map(|result| {
            result.ok_or_else(|| {
                PermissionError::Evaluation("permission evaluation produced no result".to_owned())
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct RequestBinding {
    pub modified_fields: Vec<String>,
    pub time: DateTime<Utc>,
    pub ip: Option<String>,
    pub origin: Option<String>,
}

impl Default for RequestBinding {
    fn default() -> Self {
        Self {
            modified_fields: Vec::new(),
            time: Utc::now(),
            ip: None,
            origin: None,
        }
    }
}

fn validate_expression(
    action: PermissionAction,
    expression: &IdedExpr,
    declared_bindings: &HashSet<String>,
) -> Result<(), PermissionError> {
    let locals = declared_bindings
        .iter()
        .map(|name| (name.clone(), StaticType::Dynamic))
        .collect();
    validate_node(action, expression, &locals)
}

fn bind_order(
    expression: &IdedExpr,
    binds: &HashMap<String, Program>,
) -> Result<Vec<String>, PermissionError> {
    let mut states = HashMap::new();
    let mut stack = Vec::new();
    let mut order = Vec::new();
    let mut roots = external_variables(expression)
        .into_iter()
        .filter(|name| binds.contains_key(name))
        .collect::<Vec<_>>();
    roots.sort();
    for name in roots {
        visit_bind(&name, binds, &mut states, &mut stack, &mut order)?;
    }
    Ok(order)
}

fn visit_bind(
    name: &str,
    binds: &HashMap<String, Program>,
    states: &mut HashMap<String, u8>,
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
) -> Result<(), PermissionError> {
    match states.get(name).copied() {
        Some(2) => return Ok(()),
        Some(1) => {
            let start = stack.iter().position(|item| item == name).unwrap_or(0);
            let mut cycle = stack[start..].to_vec();
            cycle.push(name.to_owned());
            return Err(PermissionError::CyclicBind(cycle));
        }
        _ => {}
    }
    states.insert(name.to_owned(), 1);
    stack.push(name.to_owned());
    let program = binds
        .get(name)
        .ok_or_else(|| PermissionError::UndeclaredVariable(name.to_owned()))?;
    let mut dependencies = external_variables(program.expression())
        .into_iter()
        .filter(|dependency| binds.contains_key(dependency))
        .collect::<Vec<_>>();
    dependencies.sort();
    for dependency in dependencies {
        visit_bind(&dependency, binds, states, stack, order)?;
    }
    stack.pop();
    states.insert(name.to_owned(), 2);
    order.push(name.to_owned());
    Ok(())
}

fn external_variables(expression: &IdedExpr) -> HashSet<String> {
    let mut variables = HashSet::new();
    collect_external_variables(expression, &HashSet::new(), &mut variables);
    variables
}

fn collect_external_variables(
    expression: &IdedExpr,
    locals: &HashSet<String>,
    variables: &mut HashSet<String>,
) {
    match &expression.expr {
        Expr::Ident(name) if !name.starts_with('@') && !locals.contains(name) => {
            variables.insert(name.clone());
        }
        Expr::Call(call) => {
            if let Some(target) = &call.target {
                collect_external_variables(target, locals, variables);
            }
            for argument in &call.args {
                collect_external_variables(argument, locals, variables);
            }
        }
        Expr::Comprehension(comprehension) => {
            collect_external_variables(&comprehension.iter_range, locals, variables);
            collect_external_variables(&comprehension.accu_init, locals, variables);
            let mut inner = locals.clone();
            inner.insert(comprehension.iter_var.clone());
            inner.insert(comprehension.accu_var.clone());
            inner.extend(comprehension.iter_var2.iter().cloned());
            collect_external_variables(&comprehension.loop_cond, &inner, variables);
            collect_external_variables(&comprehension.loop_step, &inner, variables);
            collect_external_variables(&comprehension.result, &inner, variables);
        }
        Expr::List(list) => {
            for element in &list.elements {
                collect_external_variables(element, locals, variables);
            }
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                collect_entry_variables(&entry.expr, locals, variables);
            }
        }
        Expr::Select(select) => collect_external_variables(&select.operand, locals, variables),
        Expr::Struct(value) => {
            for entry in &value.entries {
                collect_entry_variables(&entry.expr, locals, variables);
            }
        }
        Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => {}
    }
}

fn collect_entry_variables(
    entry: &EntryExpr,
    locals: &HashSet<String>,
    variables: &mut HashSet<String>,
) {
    match entry {
        EntryExpr::StructField(field) => {
            collect_external_variables(&field.value, locals, variables);
        }
        EntryExpr::MapEntry(entry) => {
            collect_external_variables(&entry.key, locals, variables);
            collect_external_variables(&entry.value, locals, variables);
        }
    }
}

fn validate_node(
    action: PermissionAction,
    expression: &IdedExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    match &expression.expr {
        Expr::Ident(name) if !name.starts_with('@') && !locals.contains_key(name) => {
            if !action.allows(name) {
                return Err(PermissionError::UndeclaredVariable(name.clone()));
            }
        }
        Expr::Call(call) => {
            validate_static_call(call, locals)?;
            if call.func_name == "ref"
                && call.target.as_deref().and_then(root_ident) == Some("auth")
                && call
                    .args
                    .first()
                    .and_then(literal_string)
                    .is_none_or(|path| !path.starts_with("$user."))
            {
                return Err(PermissionError::InvalidAuthRef);
            }
            if let Some(target) = &call.target {
                validate_node(action, target, locals)?;
            }
            for argument in &call.args {
                validate_node(action, argument, locals)?;
            }
        }
        Expr::Comprehension(comprehension) => {
            validate_node(action, &comprehension.iter_range, locals)?;
            validate_node(action, &comprehension.accu_init, locals)?;
            if !matches!(
                static_type(&comprehension.iter_range, locals),
                StaticType::List(_) | StaticType::Map(_, _) | StaticType::Dynamic
            ) {
                return Err(static_overload_error("comprehension"));
            }
            let inner = comprehension_locals(comprehension, locals);
            validate_node(action, &comprehension.loop_cond, &inner)?;
            validate_node(action, &comprehension.loop_step, &inner)?;
            validate_node(action, &comprehension.result, &inner)?;
            if !static_type_matches(
                &static_type(&comprehension.loop_cond, &inner),
                &[StaticType::Bool],
            ) {
                return Err(static_overload_error("comprehension"));
            }
            let accumulator = static_type(&comprehension.accu_init, locals);
            let step = static_type(&comprehension.loop_step, &inner);
            if !collection_types_compatible(accumulator, step) {
                return Err(static_overload_error("comprehension"));
            }
        }
        Expr::List(list) => {
            for element in &list.elements {
                validate_node(action, element, locals)?;
            }
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                validate_entry(action, &entry.expr, locals)?;
            }
        }
        Expr::Select(select) => {
            if root_ident(&select.operand) == Some("request")
                && !REQUEST_FIELDS.contains(&select.field.as_str())
            {
                return Err(PermissionError::InvalidRequestField(select.field.clone()));
            }
            validate_node(action, &select.operand, locals)?;
        }
        Expr::Struct(value) => {
            for entry in &value.entries {
                validate_entry(action, &entry.expr, locals)?;
            }
        }
        Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => {}
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum StaticType {
    Bool,
    Bytes,
    Double,
    Duration,
    Int,
    String,
    List(Box<Self>),
    Map(Box<Self>, Box<Self>),
    Timestamp,
    Type,
    UInt,
    Null,
    Dynamic,
    Invalid,
}

#[allow(clippy::too_many_lines)] // This is the central cel-java declaration/result table.
fn static_type(expression: &IdedExpr, locals: &HashMap<String, StaticType>) -> StaticType {
    match &expression.expr {
        Expr::Literal(value) => literal_static_type(value),
        Expr::Map(map) => StaticType::Map(
            Box::new(homogeneous_type(
                map.entries.iter().filter_map(|entry| match &entry.expr {
                    EntryExpr::MapEntry(entry) => Some(&entry.key),
                    EntryExpr::StructField(_) => None,
                }),
                locals,
            )),
            Box::new(homogeneous_type(
                map.entries.iter().filter_map(|entry| match &entry.expr {
                    EntryExpr::MapEntry(entry) => Some(&entry.value),
                    EntryExpr::StructField(_) => None,
                }),
                locals,
            )),
        ),
        Expr::List(list) => {
            StaticType::List(Box::new(homogeneous_type(list.elements.iter(), locals)))
        }
        Expr::Select(select) if select.test => StaticType::Bool,
        Expr::Select(select)
            if select.field == "modifiedFields"
                && root_ident(&select.operand) == Some("request") =>
        {
            StaticType::List(Box::new(StaticType::String))
        }
        Expr::Select(select)
            if select.field == "time" && root_ident(&select.operand) == Some("request") =>
        {
            StaticType::Timestamp
        }
        Expr::Select(select)
            if matches!(select.field.as_str(), "ip" | "origin")
                && root_ident(&select.operand) == Some("request") =>
        {
            StaticType::String
        }
        Expr::Select(select) => selected_static_type(&select.operand, &select.field, locals),
        Expr::Call(call) if is_math_call(call) => {
            let result = math_static_result_type(call, locals);
            if result == StaticType::Invalid {
                StaticType::Dynamic
            } else {
                result
            }
        }
        Expr::Call(call) if call.func_name == "timestamp" => StaticType::Timestamp,
        Expr::Call(call) if call.func_name == "duration" => StaticType::Duration,
        Expr::Call(call) if call.func_name == "bool" => StaticType::Bool,
        Expr::Call(call) if call.func_name == "bytes" => StaticType::Bytes,
        Expr::Call(call) if call.func_name == "double" => StaticType::Double,
        Expr::Call(call) if call.func_name == "int" => StaticType::Int,
        Expr::Call(call) if call.func_name == "string" => StaticType::String,
        Expr::Call(call) if call.func_name == "type" => StaticType::Type,
        Expr::Call(call) if call.func_name == "uint" => StaticType::UInt,
        Expr::Call(call)
            if matches!(
                call.func_name.as_str(),
                "charAt" | "join" | "lowerAscii" | "replace" | "substring" | "trim" | "upperAscii"
            ) =>
        {
            StaticType::String
        }
        Expr::Call(call) if matches!(call.func_name.as_str(), "indexOf" | "lastIndexOf") => {
            StaticType::Int
        }
        Expr::Call(call) if call.func_name == "split" => {
            StaticType::List(Box::new(StaticType::String))
        }
        Expr::Call(call) if call.func_name == "size" => StaticType::Int,
        Expr::Call(call) if call.func_name == "-_" && call.args.len() == 1 => {
            match static_type(&call.args[0], locals) {
                StaticType::Int => StaticType::Int,
                StaticType::Double => StaticType::Double,
                StaticType::Dynamic => StaticType::Dynamic,
                _ => StaticType::Invalid,
            }
        }
        Expr::Call(call)
            if matches!(
                call.func_name.as_str(),
                "getFullYear"
                    | "getMonth"
                    | "getDayOfYear"
                    | "getDayOfMonth"
                    | "getDate"
                    | "getDayOfWeek"
                    | "getHours"
                    | "getMinutes"
                    | "getSeconds"
                    | "getMilliseconds"
                    | "getTime"
            ) =>
        {
            StaticType::Int
        }
        Expr::Call(call) if call.func_name == "_[_]" && call.args.len() == 2 => {
            indexed_static_type(&call.args[0], &call.args[1], locals)
        }
        Expr::Call(call)
            if matches!(
                call.func_name.as_str(),
                "!_" | "@not_strictly_false"
                    | "_&&_"
                    | "_||_"
                    | "_==_"
                    | "_!=_"
                    | "_>_"
                    | "_>=_"
                    | "_<_"
                    | "_<=_"
                    | "@in"
                    | "matches"
                    | "contains"
                    | "endsWith"
                    | "startsWith"
            ) =>
        {
            StaticType::Bool
        }
        Expr::Call(call) if call.func_name == "_?_:_" && call.args.len() == 3 => {
            let when_true = static_type(&call.args[1], locals);
            let when_false = static_type(&call.args[2], locals);
            common_static_type(when_true, when_false)
        }
        Expr::Call(call)
            if call.args.len() == 2
                && matches!(
                    call.func_name.as_str(),
                    "_+_" | "_-_" | "_*_" | "_/_" | "_%_"
                ) =>
        {
            let result = arithmetic_result_type(
                &call.func_name,
                static_type(&call.args[0], locals),
                static_type(&call.args[1], locals),
            );
            if result == StaticType::Invalid {
                StaticType::Dynamic
            } else {
                result
            }
        }
        Expr::Comprehension(comprehension) => comprehension_static_type(comprehension, locals),
        Expr::Ident(name) if CEL_TYPE_IDENTIFIERS.contains(&name.as_str()) => StaticType::Type,
        Expr::Ident(name) => locals.get(name).cloned().unwrap_or(StaticType::Dynamic),
        _ => StaticType::Dynamic,
    }
}

fn literal_static_type(value: &LiteralValue) -> StaticType {
    match value {
        LiteralValue::Boolean(_) => StaticType::Bool,
        LiteralValue::Bytes(_) => StaticType::Bytes,
        LiteralValue::Double(_) => StaticType::Double,
        LiteralValue::Int(_) => StaticType::Int,
        LiteralValue::UInt(_) => StaticType::UInt,
        LiteralValue::String(_) => StaticType::String,
        LiteralValue::Null => StaticType::Null,
    }
}

fn iterable_element_type(
    expression: &IdedExpr,
    locals: &HashMap<String, StaticType>,
) -> StaticType {
    match static_type(expression, locals) {
        StaticType::List(element) | StaticType::Map(element, _) => *element,
        _ => StaticType::Dynamic,
    }
}

fn iterable_second_type(expression: &IdedExpr, locals: &HashMap<String, StaticType>) -> StaticType {
    match static_type(expression, locals) {
        StaticType::Map(_, value) => *value,
        _ => StaticType::Dynamic,
    }
}

fn selected_static_type(
    operand: &IdedExpr,
    field: &str,
    locals: &HashMap<String, StaticType>,
) -> StaticType {
    if let Expr::Map(map) = &operand.expr
        && let Some(value) = map.entries.iter().find_map(|entry| match &entry.expr {
            EntryExpr::MapEntry(entry) if literal_string(&entry.key) == Some(field) => {
                Some(&entry.value)
            }
            EntryExpr::StructField(_) | EntryExpr::MapEntry(_) => None,
        })
    {
        return static_type(value, locals);
    }

    match static_type(operand, locals) {
        StaticType::Map(key, value)
            if collection_types_compatible((*key).clone(), StaticType::String) =>
        {
            *value
        }
        StaticType::Dynamic => StaticType::Dynamic,
        _ => StaticType::Invalid,
    }
}

fn comprehension_locals(
    comprehension: &ComprehensionExpr,
    locals: &HashMap<String, StaticType>,
) -> HashMap<String, StaticType> {
    let mut inner = locals.clone();
    inner.insert(
        comprehension.iter_var.clone(),
        iterable_element_type(&comprehension.iter_range, locals),
    );
    if let Some(iter_var2) = &comprehension.iter_var2 {
        inner.insert(
            iter_var2.clone(),
            iterable_second_type(&comprehension.iter_range, locals),
        );
    }
    let accumulator = static_type(&comprehension.accu_init, locals);
    inner.insert(comprehension.accu_var.clone(), accumulator.clone());
    let stepped = static_type(&comprehension.loop_step, &inner);
    inner.insert(
        comprehension.accu_var.clone(),
        common_static_type(accumulator, stepped),
    );
    inner
}

fn comprehension_static_type(
    comprehension: &ComprehensionExpr,
    locals: &HashMap<String, StaticType>,
) -> StaticType {
    let inner = comprehension_locals(comprehension, locals);
    static_type(&comprehension.result, &inner)
}

fn homogeneous_type<'a>(
    mut expressions: impl Iterator<Item = &'a IdedExpr>,
    locals: &HashMap<String, StaticType>,
) -> StaticType {
    let Some(first) = expressions.next() else {
        return StaticType::Dynamic;
    };
    let first_type = static_type(first, locals);
    if expressions.all(|expression| static_type(expression, locals) == first_type) {
        first_type
    } else {
        StaticType::Dynamic
    }
}

fn common_static_type(left: StaticType, right: StaticType) -> StaticType {
    match (left, right) {
        (StaticType::Dynamic, other) | (other, StaticType::Dynamic) => other,
        (StaticType::List(left), StaticType::List(right)) => {
            StaticType::List(Box::new(common_static_type(*left, *right)))
        }
        (StaticType::Map(left_key, left_value), StaticType::Map(right_key, right_value)) => {
            StaticType::Map(
                Box::new(common_static_type(*left_key, *right_key)),
                Box::new(common_static_type(*left_value, *right_value)),
            )
        }
        (left, right) if left == right => left,
        _ => StaticType::Dynamic,
    }
}

fn collection_value_type(
    expression: &IdedExpr,
    locals: &HashMap<String, StaticType>,
) -> StaticType {
    match static_type(expression, locals) {
        StaticType::List(value) | StaticType::Map(_, value) => *value,
        _ => StaticType::Dynamic,
    }
}

fn indexed_static_type(
    collection: &IdedExpr,
    index: &IdedExpr,
    locals: &HashMap<String, StaticType>,
) -> StaticType {
    match (&collection.expr, &index.expr) {
        (Expr::List(list), Expr::Literal(LiteralValue::Int(index))) => usize::try_from(**index)
            .ok()
            .and_then(|index| list.elements.get(index))
            .map_or_else(
                || collection_value_type(collection, locals),
                |element| static_type(element, locals),
            ),
        (Expr::Map(map), _) => map
            .entries
            .iter()
            .find_map(|entry| match &entry.expr {
                EntryExpr::MapEntry(entry)
                    if literal_string(&entry.key) == literal_string(index)
                        && literal_string(index).is_some() =>
                {
                    Some(static_type(&entry.value, locals))
                }
                EntryExpr::StructField(_) | EntryExpr::MapEntry(_) => None,
            })
            .unwrap_or_else(|| collection_value_type(collection, locals)),
        _ => collection_value_type(collection, locals),
    }
}

fn collection_key_type(expression: &IdedExpr, locals: &HashMap<String, StaticType>) -> StaticType {
    match static_type(expression, locals) {
        StaticType::Map(key, _) => *key,
        _ => StaticType::Dynamic,
    }
}

fn known_list(expression: &IdedExpr, locals: &HashMap<String, StaticType>) -> bool {
    matches!(static_type(expression, locals), StaticType::List(_))
}

fn collection_types_compatible(expected: StaticType, actual: StaticType) -> bool {
    match (expected, actual) {
        (StaticType::Dynamic, _) | (_, StaticType::Dynamic) => true,
        (StaticType::List(expected), StaticType::List(actual)) => {
            collection_types_compatible(*expected, *actual)
        }
        (
            StaticType::Map(expected_key, expected_value),
            StaticType::Map(actual_key, actual_value),
        ) => {
            collection_types_compatible(*expected_key, *actual_key)
                && collection_types_compatible(*expected_value, *actual_value)
        }
        (expected, actual) => expected == actual,
    }
}

fn validate_static_collection_call(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    match (
        call.func_name.as_str(),
        call.target.as_deref(),
        call.args.as_slice(),
    ) {
        ("size", Some(target), [])
            if !matches!(
                static_type(target, locals),
                StaticType::String
                    | StaticType::Bytes
                    | StaticType::List(_)
                    | StaticType::Map(_, _)
                    | StaticType::Dynamic
            ) =>
        {
            Err(PermissionError::StaticType(
                "found no matching overload for 'size'".to_owned(),
            ))
        }
        ("size", None, [target])
            if !matches!(
                static_type(target, locals),
                StaticType::String
                    | StaticType::Bytes
                    | StaticType::List(_)
                    | StaticType::Map(_, _)
                    | StaticType::Dynamic
            ) =>
        {
            Err(PermissionError::StaticType(
                "found no matching overload for 'size'".to_owned(),
            ))
        }
        ("size", Some(_), []) | ("size", None, [_]) => Ok(()),
        ("size", _, _) => Err(PermissionError::StaticType(
            "found no matching overload for 'size'".to_owned(),
        )),
        ("_[_]", None, [collection, index]) => {
            let collection_type = static_type(collection, locals);
            let valid = if known_list(collection, locals) {
                matches!(
                    static_type(index, locals),
                    StaticType::Int | StaticType::Dynamic
                )
            } else {
                match collection_type {
                    StaticType::Map(_, _) => collection_types_compatible(
                        collection_key_type(collection, locals),
                        static_type(index, locals),
                    ),
                    StaticType::Dynamic => true,
                    _ => false,
                }
            };
            if valid {
                Ok(())
            } else {
                Err(PermissionError::StaticType(
                    "found no matching overload for '_[_]'".to_owned(),
                ))
            }
        }
        ("@in", None, [item, collection]) => {
            let collection_type = static_type(collection, locals);
            let valid = if known_list(collection, locals) {
                collection_types_compatible(
                    collection_value_type(collection, locals),
                    static_type(item, locals),
                )
            } else {
                match collection_type {
                    StaticType::Map(_, _) => collection_types_compatible(
                        collection_key_type(collection, locals),
                        static_type(item, locals),
                    ),
                    StaticType::Dynamic => true,
                    _ => false,
                }
            };
            if valid {
                Ok(())
            } else {
                Err(PermissionError::StaticType(
                    "found no matching overload for '@in'".to_owned(),
                ))
            }
        }
        _ => Ok(()),
    }
}

fn static_overload_error(name: &str) -> PermissionError {
    PermissionError::StaticType(format!("found no matching overload for '{name}'"))
}

fn static_type_matches(actual: &StaticType, expected: &[StaticType]) -> bool {
    *actual == StaticType::Dynamic || expected.contains(actual)
}

fn is_math_call(call: &CallExpr) -> bool {
    call.target
        .as_deref()
        .is_some_and(|target| root_ident(target) == Some("math"))
}

fn is_math_numeric(value: &StaticType) -> bool {
    matches!(
        value,
        StaticType::Int | StaticType::UInt | StaticType::Double | StaticType::Dynamic
    )
}

fn math_static_result_type(call: &CallExpr, locals: &HashMap<String, StaticType>) -> StaticType {
    let arguments = &call.args;
    let one_numeric = || {
        arguments
            .first()
            .filter(|_| arguments.len() == 1)
            .map(|argument| static_type(argument, locals))
            .filter(is_math_numeric)
    };
    let one_integral = || {
        arguments
            .first()
            .filter(|_| arguments.len() == 1)
            .map(|argument| static_type(argument, locals))
            .filter(|value| {
                matches!(
                    value,
                    StaticType::Int | StaticType::UInt | StaticType::Dynamic
                )
            })
    };
    let one_float = || {
        arguments
            .first()
            .filter(|_| arguments.len() == 1)
            .map(|argument| static_type(argument, locals))
            .filter(|value| matches!(value, StaticType::Double | StaticType::Dynamic))
    };
    let two_matching_integrals = || match arguments.as_slice() {
        [left, right] => match (static_type(left, locals), static_type(right, locals)) {
            (StaticType::Int, StaticType::Int) => Some(StaticType::Int),
            (StaticType::UInt, StaticType::UInt) => Some(StaticType::UInt),
            (StaticType::Dynamic, _) | (_, StaticType::Dynamic) => Some(StaticType::Dynamic),
            _ => None,
        },
        _ => None,
    };
    let shift_result_type = || match arguments.as_slice() {
        [value, shift]
            if matches!(
                static_type(value, locals),
                StaticType::Int | StaticType::UInt | StaticType::Dynamic
            ) && matches!(
                static_type(shift, locals),
                StaticType::Int | StaticType::Dynamic
            ) =>
        {
            Some(static_type(value, locals))
        }
        _ => None,
    };
    match call.func_name.as_str() {
        "abs" | "sign" => one_numeric().unwrap_or(StaticType::Invalid),
        "ceil" | "floor" | "round" | "trunc" => one_float().map_or(StaticType::Invalid, |value| {
            if value == StaticType::Dynamic {
                StaticType::Dynamic
            } else {
                StaticType::Double
            }
        }),
        "sqrt" => one_numeric().map_or(StaticType::Invalid, |value| {
            if value == StaticType::Dynamic {
                StaticType::Dynamic
            } else {
                StaticType::Double
            }
        }),
        "isInf" | "isNaN" | "isFinite" => {
            one_float().map_or(StaticType::Invalid, |_| StaticType::Bool)
        }
        "bitNot" => one_integral().unwrap_or(StaticType::Invalid),
        "bitAnd" | "bitOr" | "bitXor" => two_matching_integrals().unwrap_or(StaticType::Invalid),
        "bitShiftLeft" | "bitShiftRight" => shift_result_type().unwrap_or(StaticType::Invalid),
        "greatest" | "least" => math_extreme_static_result_type(arguments, locals),
        _ => StaticType::Invalid,
    }
}

fn math_extreme_static_result_type(
    arguments: &[IdedExpr],
    locals: &HashMap<String, StaticType>,
) -> StaticType {
    let values = match arguments {
        [argument] => match static_type(argument, locals) {
            StaticType::List(element) => vec![*element],
            StaticType::Dynamic => vec![StaticType::Dynamic],
            value if is_math_numeric(&value) => vec![value],
            _ => return StaticType::Invalid,
        },
        [] => return StaticType::Invalid,
        _ => arguments
            .iter()
            .map(|argument| static_type(argument, locals))
            .collect(),
    };
    if values.iter().any(|value| !is_math_numeric(value)) {
        return StaticType::Invalid;
    }
    values
        .into_iter()
        .reduce(common_static_type)
        .unwrap_or(StaticType::Invalid)
}

fn optional_static_string_argument(
    arguments: &[IdedExpr],
    locals: &HashMap<String, StaticType>,
) -> bool {
    arguments.is_empty()
        || matches!(arguments, [argument] if static_type_matches(
            &static_type(argument, locals),
            &[StaticType::String],
        ))
}

fn global_static_signature(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
    expected: &[StaticType],
) -> bool {
    call.target.is_none()
        && matches!(call.args.as_slice(), [argument] if static_type_matches(
            &static_type(argument, locals), expected,
        ))
}

fn static_conversion_call_allowed(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Option<bool> {
    use StaticType::{Bool, Bytes, Double, Duration, Int, String, Timestamp, UInt};
    let expected: &[StaticType] = match call.func_name.as_str() {
        "bool" => &[Bool, String],
        "bytes" => &[Bytes, String],
        "double" => &[Double, Int, UInt, String],
        "int" => &[Int, UInt, Double, String, Timestamp],
        "uint" => &[UInt, Int, Double, String],
        "string" => &[String, Int, UInt, Double, Bool, Bytes, Timestamp, Duration],
        "duration" => &[Duration, String],
        "timestamp" => &[Timestamp, String, Int],
        "dyn" | "type" => &[],
        _ => return None,
    };
    Some(if expected.is_empty() {
        call.target.is_none() && call.args.len() == 1
    } else {
        global_static_signature(call, locals, expected)
    })
}

fn static_string_matcher_call_allowed(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Option<bool> {
    let is_string =
        |expression| static_type_matches(&static_type(expression, locals), &[StaticType::String]);
    match (
        call.func_name.as_str(),
        call.target.as_deref(),
        call.args.as_slice(),
    ) {
        ("contains" | "endsWith" | "startsWith", Some(target), [argument]) => {
            Some(is_string(target) && is_string(argument))
        }
        ("matches", Some(target), [pattern]) | ("matches", None, [target, pattern]) => {
            Some(is_string(target) && is_string(pattern))
        }
        ("contains" | "endsWith" | "startsWith" | "matches", _, _) => Some(false),
        _ => None,
    }
}

fn static_date_time_call_allowed(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Option<bool> {
    let target_type = call
        .target
        .as_deref()
        .map(|target| static_type(target, locals));
    let arguments = call.args.as_slice();
    match call.func_name.as_str() {
        "getFullYear" | "getMonth" | "getDayOfYear" | "getDayOfMonth" | "getDate"
        | "getDayOfWeek" => Some(
            target_type
                .as_ref()
                .is_some_and(|actual| static_type_matches(actual, &[StaticType::Timestamp]))
                && optional_static_string_argument(arguments, locals),
        ),
        "getHours" | "getMinutes" | "getSeconds" | "getMilliseconds" => {
            Some(target_type.as_ref().is_some_and(|actual| {
                static_type_matches(actual, &[StaticType::Timestamp])
                    && optional_static_string_argument(arguments, locals)
                    || static_type_matches(actual, &[StaticType::Duration]) && arguments.is_empty()
            }))
        }
        "getTime" => Some(
            target_type
                .as_ref()
                .is_some_and(|actual| static_type_matches(actual, &[StaticType::Timestamp]))
                && arguments.is_empty(),
        ),
        _ => None,
    }
}

fn validate_static_standard_call(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    let allowed = static_conversion_call_allowed(call, locals)
        .or_else(|| static_string_matcher_call_allowed(call, locals))
        .or_else(|| static_date_time_call_allowed(call, locals));
    if allowed == Some(false) {
        Err(static_overload_error(&call.func_name))
    } else {
        Ok(())
    }
}

fn validate_static_call(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    if matches!(
        call.func_name.as_str(),
        "has" | "all" | "exists" | "exists_one" | "existsOne" | "map" | "filter"
    ) {
        return Err(static_overload_error(&call.func_name));
    }
    validate_static_math_call(call, locals)?;
    validate_static_arithmetic_call(call, locals)?;
    validate_static_collection_call(call, locals)?;
    validate_static_standard_call(call, locals)?;
    validate_static_string_extension_call(call, locals)?;
    match (call.func_name.as_str(), call.args.as_slice()) {
        ("!_", [argument])
            if !matches!(
                static_type(argument, locals),
                StaticType::Bool | StaticType::Dynamic
            ) =>
        {
            Err(static_overload_error("!_"))
        }
        (name @ ("_&&_" | "_||_"), arguments)
            if arguments.iter().any(|argument| {
                !matches!(
                    static_type(argument, locals),
                    StaticType::Bool | StaticType::Dynamic
                )
            }) =>
        {
            Err(static_overload_error(name))
        }
        (name @ ("_==_" | "_!=_"), [left, right])
            if !equality_types_compatible(
                static_type(left, locals),
                static_type(right, locals),
            ) =>
        {
            Err(static_overload_error(name))
        }
        (name @ ("_>_" | "_>=_" | "_<_" | "_<=_"), [left, right])
            if !ordering_types_compatible(
                static_type(left, locals),
                static_type(right, locals),
            ) =>
        {
            Err(static_overload_error(name))
        }
        ("_?_:_", [condition, _, _])
            if !matches!(
                static_type(condition, locals),
                StaticType::Bool | StaticType::Dynamic
            ) =>
        {
            Err(static_overload_error("_?_:_"))
        }
        ("_?_:_", [_, when_true, when_false])
            if !equality_types_compatible(
                static_type(when_true, locals),
                static_type(when_false, locals),
            ) =>
        {
            Err(static_overload_error("_?_:_"))
        }
        _ => Ok(()),
    }
}

fn validate_static_math_call(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    if is_math_call(call) && math_static_result_type(call, locals) == StaticType::Invalid {
        return Err(static_overload_error(&call.func_name));
    }
    Ok(())
}

fn validate_static_string_extension_call(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    let name = call.func_name.as_str();
    if !is_string_extension_name(name) {
        return Ok(());
    }
    let Some(target) = call.target.as_deref() else {
        return Err(PermissionError::StaticType(format!(
            "found no matching member overload for `{name}`"
        )));
    };
    let target_type = static_type(target, locals);
    let target_matches = if name == "join" {
        match &target_type {
            StaticType::List(element) => {
                matches!(element.as_ref(), StaticType::String | StaticType::Dynamic)
            }
            StaticType::Dynamic => true,
            _ => false,
        }
    } else {
        matches!(target_type, StaticType::String | StaticType::Dynamic)
    };
    if !target_matches {
        return Err(PermissionError::StaticType(format!(
            "found no matching overload for `{name}`"
        )));
    }

    let signatures: &[&[StaticType]] = match name {
        "charAt" => &[&[StaticType::Int]],
        "indexOf" | "lastIndexOf" | "split" => &[
            &[StaticType::String],
            &[StaticType::String, StaticType::Int],
        ],
        "join" => &[&[], &[StaticType::String]],
        "lowerAscii" | "trim" | "upperAscii" => &[&[]],
        "replace" => &[
            &[StaticType::String, StaticType::String],
            &[StaticType::String, StaticType::String, StaticType::Int],
        ],
        "substring" => &[&[StaticType::Int], &[StaticType::Int, StaticType::Int]],
        _ => unreachable!(),
    };
    if signatures.iter().any(|signature| {
        signature.len() == call.args.len()
            && signature
                .iter()
                .zip(&call.args)
                .all(|(expected, argument)| {
                    static_string_argument_matches(expected, argument, locals)
                })
    }) {
        Ok(())
    } else {
        Err(PermissionError::StaticType(format!(
            "found no matching overload for `{name}`"
        )))
    }
}

fn static_string_argument_matches(
    expected: &StaticType,
    argument: &IdedExpr,
    locals: &HashMap<String, StaticType>,
) -> bool {
    let actual = static_type(argument, locals);
    match expected {
        StaticType::String => matches!(actual, StaticType::String | StaticType::Dynamic),
        StaticType::Int => matches!(actual, StaticType::Int | StaticType::Dynamic),
        _ => &actual == expected || actual == StaticType::Dynamic,
    }
}

fn validate_static_arithmetic_call(
    call: &CallExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    match (call.func_name.as_str(), call.args.as_slice()) {
        ("-_", [argument])
            if !matches!(
                static_type(argument, locals),
                StaticType::Int | StaticType::Double | StaticType::Dynamic
            ) =>
        {
            Err(static_overload_error("-_"))
        }
        ("_+_" | "_-_" | "_*_" | "_/_" | "_%_", [left, right])
            if arithmetic_result_type(
                &call.func_name,
                static_type(left, locals),
                static_type(right, locals),
            ) == StaticType::Invalid =>
        {
            Err(static_overload_error(&call.func_name))
        }
        _ => Ok(()),
    }
}

fn equality_types_compatible(left: StaticType, right: StaticType) -> bool {
    use StaticType::{Double, Dynamic, Int, UInt};

    match (left, right) {
        (Dynamic, _) | (_, Dynamic) | (Int | UInt | Double, Int | UInt | Double) => true,
        (StaticType::List(left), StaticType::List(right)) => {
            equality_types_compatible(*left, *right)
        }
        (StaticType::Map(left_key, left_value), StaticType::Map(right_key, right_value)) => {
            equality_types_compatible(*left_key, *right_key)
                && equality_types_compatible(*left_value, *right_value)
        }
        (left, right) => left == right,
    }
}

fn arithmetic_result_type(operator: &str, left: StaticType, right: StaticType) -> StaticType {
    use StaticType::{Bytes, Double, Duration, Dynamic, Int, Invalid, String, Timestamp, UInt};

    if left == Dynamic || right == Dynamic {
        return Dynamic;
    }

    match (operator, left, right) {
        ("_+_" | "_-_" | "_*_" | "_/_" | "_%_", Int, Int) => Int,
        ("_+_" | "_-_" | "_*_" | "_/_" | "_%_", UInt, UInt) => UInt,
        ("_+_" | "_-_" | "_*_" | "_/_", Double, Double) => Double,
        ("_+_", String, String) => String,
        ("_+_", Bytes, Bytes) => Bytes,
        ("_+_", StaticType::List(left), StaticType::List(right))
            if collection_types_compatible((*left).clone(), (*right).clone()) =>
        {
            StaticType::List(Box::new(common_static_type(*left, *right)))
        }
        ("_+_" | "_-_", Timestamp, Duration) | ("_+_", Duration, Timestamp) => Timestamp,
        ("_+_" | "_-_", Duration, Duration) | ("_-_", Timestamp, Timestamp) => Duration,
        _ => Invalid,
    }
}

fn ordering_types_compatible(left: StaticType, right: StaticType) -> bool {
    use StaticType::{Bool, Bytes, Double, Duration, Dynamic, Int, String, Timestamp, UInt};

    matches!(
        (left, right),
        (Dynamic, _)
            | (_, Dynamic)
            | (Int | UInt | Double, Int | UInt | Double)
            | (Bool, Bool)
            | (Bytes, Bytes)
            | (String, String)
            | (Timestamp, Timestamp)
            | (Duration, Duration)
    )
}

fn validate_entry(
    action: PermissionAction,
    entry: &EntryExpr,
    locals: &HashMap<String, StaticType>,
) -> Result<(), PermissionError> {
    match entry {
        EntryExpr::StructField(field) => validate_node(action, &field.value, locals),
        EntryExpr::MapEntry(entry) => {
            validate_node(action, &entry.key, locals)?;
            validate_node(action, &entry.value, locals)
        }
    }
}

fn root_ident(expression: &IdedExpr) -> Option<&str> {
    match &expression.expr {
        Expr::Ident(name) => Some(name),
        Expr::Select(select) => root_ident(&select.operand),
        _ => None,
    }
}

fn literal_string(expression: &IdedExpr) -> Option<&str> {
    match &expression.expr {
        Expr::Literal(LiteralValue::String(value)) => Some(value.inner()),
        _ => None,
    }
}

fn select_path(expression: &IdedExpr) -> Option<(String, Vec<String>)> {
    match &expression.expr {
        Expr::Ident(name) => Some((name.clone(), Vec::new())),
        Expr::Select(select) => {
            let (root, mut path) = select_path(&select.operand)?;
            path.push(select.field.clone());
            Some((root, path))
        }
        _ => None,
    }
}

fn collect_selected_paths(expression: &IdedExpr, paths: &mut HashMap<String, Vec<Vec<String>>>) {
    if let Some((root, path)) = select_path(expression)
        && !path.is_empty()
        && !matches!(&expression.expr, Expr::Select(select) if select.test)
    {
        let root_paths = paths.entry(root).or_default();
        if !root_paths.contains(&path) {
            root_paths.push(path);
        }
    }
    match &expression.expr {
        Expr::Call(call) => {
            if let Some(target) = &call.target {
                collect_selected_paths(target, paths);
            }
            for argument in &call.args {
                collect_selected_paths(argument, paths);
            }
        }
        Expr::Comprehension(comprehension) => {
            for child in [
                &comprehension.iter_range,
                &comprehension.accu_init,
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ] {
                collect_selected_paths(child, paths);
            }
        }
        Expr::List(list) => {
            for element in &list.elements {
                collect_selected_paths(element, paths);
            }
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                collect_entry_paths(&entry.expr, paths);
            }
        }
        Expr::Select(select) if !select.test => collect_selected_paths(&select.operand, paths),
        Expr::Struct(value) => {
            for entry in &value.entries {
                collect_entry_paths(&entry.expr, paths);
            }
        }
        Expr::Select(_) | Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => {}
    }
}

fn collect_presence_paths(expression: &IdedExpr, paths: &mut HashSet<(String, Vec<String>)>) {
    if matches!(&expression.expr, Expr::Select(select) if select.test)
        && let Some((root, path)) = select_path(expression)
    {
        paths.insert((root, path));
    }
    match &expression.expr {
        Expr::Call(call) => {
            if let Some(target) = &call.target {
                collect_presence_paths(target, paths);
            }
            for argument in &call.args {
                collect_presence_paths(argument, paths);
            }
        }
        Expr::Comprehension(comprehension) => {
            for child in [
                &comprehension.iter_range,
                &comprehension.accu_init,
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ] {
                collect_presence_paths(child, paths);
            }
        }
        Expr::List(list) => {
            for element in &list.elements {
                collect_presence_paths(element, paths);
            }
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                collect_entry_presence_paths(&entry.expr, paths);
            }
        }
        Expr::Select(select) => collect_presence_paths(&select.operand, paths),
        Expr::Struct(value) => {
            for entry in &value.entries {
                collect_entry_presence_paths(&entry.expr, paths);
            }
        }
        Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => {}
    }
}

fn collect_entry_presence_paths(entry: &EntryExpr, paths: &mut HashSet<(String, Vec<String>)>) {
    match entry {
        EntryExpr::StructField(field) => collect_presence_paths(&field.value, paths),
        EntryExpr::MapEntry(entry) => {
            collect_presence_paths(&entry.key, paths);
            collect_presence_paths(&entry.value, paths);
        }
    }
}

fn collect_entry_paths(entry: &EntryExpr, paths: &mut HashMap<String, Vec<Vec<String>>>) {
    match entry {
        EntryExpr::StructField(field) => collect_selected_paths(&field.value, paths),
        EntryExpr::MapEntry(entry) => {
            collect_selected_paths(&entry.key, paths);
            collect_selected_paths(&entry.value, paths);
        }
    }
}

fn collect_reference_uses(expression: &IdedExpr, uses: &mut HashSet<ReferenceUse>) {
    match &expression.expr {
        Expr::Call(call) => {
            if call.func_name == "ref"
                && let (Some(target), Some(path)) = (
                    call.target.as_deref().and_then(root_ident),
                    call.args.first().and_then(literal_string),
                )
                && matches!(target, "data" | "auth" | "linkedData")
            {
                uses.insert(ReferenceUse {
                    target: target.to_owned(),
                    path: if target == "auth" {
                        path.strip_prefix("$user.").unwrap_or(path).to_owned()
                    } else {
                        path.to_owned()
                    },
                });
            }
            if let Some(target) = &call.target {
                collect_reference_uses(target, uses);
            }
            for argument in &call.args {
                collect_reference_uses(argument, uses);
            }
        }
        Expr::Comprehension(comprehension) => {
            for child in [
                &comprehension.iter_range,
                &comprehension.accu_init,
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ] {
                collect_reference_uses(child, uses);
            }
        }
        Expr::List(list) => {
            for element in &list.elements {
                collect_reference_uses(element, uses);
            }
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                collect_entry_reference_uses(&entry.expr, uses);
            }
        }
        Expr::Select(select) => collect_reference_uses(&select.operand, uses),
        Expr::Struct(value) => {
            for entry in &value.entries {
                collect_entry_reference_uses(&entry.expr, uses);
            }
        }
        Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => {}
    }
}

fn collect_rate_limit_uses(expression: &IdedExpr, uses: &mut HashSet<String>) {
    match &expression.expr {
        Expr::Call(call) => {
            if call.func_name == "limit"
                && let Some(name) = call.target.as_deref().and_then(rate_limit_bucket_name)
            {
                uses.insert(name.to_owned());
            }
            if let Some(target) = &call.target {
                collect_rate_limit_uses(target, uses);
            }
            for argument in &call.args {
                collect_rate_limit_uses(argument, uses);
            }
        }
        Expr::Comprehension(comprehension) => {
            for child in [
                &comprehension.iter_range,
                &comprehension.accu_init,
                &comprehension.loop_cond,
                &comprehension.loop_step,
                &comprehension.result,
            ] {
                collect_rate_limit_uses(child, uses);
            }
        }
        Expr::List(list) => {
            for element in &list.elements {
                collect_rate_limit_uses(element, uses);
            }
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                collect_entry_rate_limit_uses(&entry.expr, uses);
            }
        }
        Expr::Select(select) => collect_rate_limit_uses(&select.operand, uses),
        Expr::Struct(value) => {
            for entry in &value.entries {
                collect_entry_rate_limit_uses(&entry.expr, uses);
            }
        }
        Expr::Ident(_) | Expr::Literal(_) | Expr::Unspecified => {}
    }
}

fn collect_entry_rate_limit_uses(entry: &EntryExpr, uses: &mut HashSet<String>) {
    match entry {
        EntryExpr::StructField(field) => collect_rate_limit_uses(&field.value, uses),
        EntryExpr::MapEntry(entry) => {
            collect_rate_limit_uses(&entry.key, uses);
            collect_rate_limit_uses(&entry.value, uses);
        }
    }
}

fn rate_limit_bucket_name(expression: &IdedExpr) -> Option<&str> {
    match &expression.expr {
        Expr::Select(select) if root_ident(&select.operand) == Some("rateLimit") => {
            Some(select.field.as_str())
        }
        Expr::Call(call) if call.func_name == "_[_]" && call.args.len() == 2 => {
            (root_ident(&call.args[0]) == Some("rateLimit"))
                .then(|| literal_string(&call.args[1]))
                .flatten()
        }
        _ => None,
    }
}

fn collect_entry_reference_uses(entry: &EntryExpr, uses: &mut HashSet<ReferenceUse>) {
    match entry {
        EntryExpr::StructField(field) => collect_reference_uses(&field.value, uses),
        EntryExpr::MapEntry(entry) => {
            collect_reference_uses(&entry.key, uses);
            collect_reference_uses(&entry.value, uses);
        }
    }
}

fn add_json_binding(
    context: &mut Context<'_>,
    name: &str,
    value: &JsonValue,
    paths: &HashMap<String, Vec<Vec<String>>>,
    ref_etype: Option<&str>,
) -> Result<(), PermissionError> {
    context
        .add_variable(
            name,
            json_binding_value(value, paths.get(name).map_or(&[], Vec::as_slice), ref_etype)?,
        )
        .map_err(|error| PermissionError::Binding(error.to_string()))
}

// Convert the complete input directly into fresh CEL values, rather than cloning
// JSON once for reference tagging and again for missing-field normalization.
// Only the root null becomes a map; nested nulls/scalars and every unselected
// field are preserved. Nothing here is shared with another evaluation.
fn json_binding_value(
    value: &JsonValue,
    paths: &[Vec<String>],
    ref_etype: Option<&str>,
) -> Result<Value, PermissionError> {
    let mut value = if value.is_null() || value.is_object() {
        let mut values = HashMap::with_capacity(
            value.as_object().map_or(0, Map::len) + usize::from(ref_etype.is_some()),
        );
        if let Some(object) = value.as_object() {
            for (key, value) in object {
                // Tagging previously overwrote this JSON field before conversion.
                // In particular, do not reject an otherwise invalid shadowed value.
                if ref_etype.is_some() && key == "__instant_ref_etype" {
                    continue;
                }
                values.insert(key.clone().into(), json_to_cel(value)?);
            }
        }
        if let Some(etype) = ref_etype {
            values.insert(
                "__instant_ref_etype".into(),
                Value::String(etype.to_owned().into()),
            );
        }
        Value::Map(CelMap {
            map: Arc::new(values),
        })
    } else {
        json_to_cel(value)?
    };
    for path in paths {
        insert_missing_cel_null(&mut value, path);
    }
    Ok(value)
}

fn insert_missing_cel_null(value: &mut Value, path: &[String]) {
    let Some((field, rest)) = path.split_first() else {
        return;
    };
    let Value::Map(object) = value else {
        return;
    };
    // Conversion has just created these maps, before publishing the binding to
    // a context. The Arcs are unique; make_mut also keeps this helper copy-safe.
    let object = Arc::make_mut(&mut object.map);
    let key = &KeyRef::String(field) as &dyn cel::objects::AsKeyRef;
    if rest.is_empty() {
        if !object.contains_key(key) {
            object.insert(field.clone().into(), Value::Null);
        }
    } else if let Some(child) = object.get_mut(key) {
        insert_missing_cel_null(child, rest);
    }
}

fn charge_binding(
    value: &JsonValue,
    remaining: &mut usize,
    depth: usize,
) -> Result<(), PermissionError> {
    let exhausted = || PermissionError::Evaluation("CEL binding budget exceeded".into());
    if depth > 64 {
        return Err(exhausted());
    }
    let bytes = match value {
        JsonValue::String(text) => text.len().saturating_add(64),
        _ => 64,
    };
    *remaining = remaining.checked_sub(bytes).ok_or_else(exhausted)?;
    match value {
        JsonValue::Array(values) => {
            for value in values {
                charge_binding(value, remaining, depth + 1)?;
            }
        }
        JsonValue::Object(values) => {
            for (key, value) in values {
                *remaining = remaining.checked_sub(key.len()).ok_or_else(exhausted)?;
                charge_binding(value, remaining, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn json_to_cel(value: &JsonValue) -> Result<Value, PermissionError> {
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Bool(*value)),
        JsonValue::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(Value::Int(value))
            } else if let Some(value) = value.as_u64() {
                i64::try_from(value).map(Value::Int).map_err(|_| {
                    PermissionError::Binding("integer exceeds CEL int range".to_owned())
                })
            } else {
                value
                    .as_f64()
                    .map(Value::Float)
                    .ok_or_else(|| PermissionError::Binding("invalid JSON number".to_owned()))
            }
        }
        JsonValue::String(value) => Ok(Value::String(value.clone().into())),
        JsonValue::Array(values) => values
            .iter()
            .map(json_to_cel)
            .collect::<Result<Vec<_>, _>>()
            .map(|values| Value::List(values.into())),
        JsonValue::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), json_to_cel(value)?)))
            .collect::<Result<HashMap<_, _>, PermissionError>>()
            .map(CelMap::from)
            .map(Value::Map),
    }
}

fn normalize_json(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Null => JsonValue::Object(Map::new()),
        value => value,
    }
}

fn add_request_binding(
    context: &mut Context<'_>,
    request: &RequestBinding,
) -> Result<(), PermissionError> {
    let mut values = HashMap::from([
        (
            "modifiedFields".to_owned(),
            Value::List(
                request
                    .modified_fields
                    .iter()
                    .cloned()
                    .map(|value| Value::String(value.into()))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        ),
        (
            "time".to_owned(),
            Value::Timestamp(request.time.fixed_offset()),
        ),
    ]);
    if let Some(ip) = &request.ip {
        values.insert("ip".to_owned(), Value::String(ip.clone().into()));
    }
    if let Some(origin) = &request.origin {
        values.insert("origin".to_owned(), Value::String(origin.clone().into()));
    }
    context
        .add_variable("request", Value::Map(CelMap::from(values)))
        .map_err(|error| PermissionError::Binding(error.to_string()))
}

fn add_math_binding(context: &mut Context<'_>) -> Result<(), PermissionError> {
    context
        .add_variable(
            "math",
            Value::Map(CelMap::from(HashMap::from([(
                "__instant_math".to_owned(),
                Value::Bool(true),
            )]))),
        )
        .map_err(|error| PermissionError::Binding(error.to_string()))
}

fn register_functions(
    context: &mut Context<'_>,
    ref_values: Arc<HashMap<RefRequest, Vec<Value>>>,
    missing_refs: Arc<Mutex<HashSet<RefRequest>>>,
    rate_limit_runtime: Option<RateLimitRuntime>,
    rate_limit_requests: Arc<Mutex<Vec<RateLimitRequest>>>,
) {
    context.add_function("ref", move |this: This<Value>, path: Arc<String>| {
        instant_ref(
            this,
            path.as_str(),
            ref_values.as_ref(),
            missing_refs.as_ref(),
        )
    });
    context.add_function("limit", move |this: This<Value>, arguments: Arguments| {
        plan_rate_limit(
            this,
            arguments,
            rate_limit_runtime.as_ref(),
            rate_limit_requests.as_ref(),
        )
    });
}

fn register_stateless_functions(context: &mut Context<'_>) {
    context.add_function("_instant_bool", instant_convert_bool);
    context.add_function("_instant_int", instant_convert_int);
    context.add_function("_instant_uint", instant_convert_uint);
    context.add_function("_instant_string", instant_convert_string);
    context.add_function("_instant_duration", instant_duration);
    context.add_function("_instant_timestamp", instant_timestamp);
    context.add_function("getTime", instant_get_time);
    context.add_function("_instant_matches", instant_matches);
    context.add_function("_instant_getFullYear", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::FullYear)
    });
    context.add_function("_instant_getMonth", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::Month)
    });
    context.add_function("_instant_getDayOfYear", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::DayOfYear)
    });
    context.add_function("_instant_getDayOfMonth", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::DayOfMonth)
    });
    context.add_function("_instant_getDate", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::Date)
    });
    context.add_function("_instant_getDayOfWeek", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::DayOfWeek)
    });
    context.add_function("_instant_getHours", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::Hours)
    });
    context.add_function("_instant_getMinutes", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::Minutes)
    });
    context.add_function("_instant_getSeconds", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::Seconds)
    });
    context.add_function("_instant_getMilliseconds", |this, arguments| {
        instant_date_time(this, arguments, DateTimePart::Milliseconds)
    });
    context.add_function("abs", instant_math_abs);
    context.add_function("greatest", instant_math_greatest);
    context.add_function("least", instant_math_least);
    context.add_function("ceil", instant_math_ceil);
    context.add_function("floor", instant_math_floor);
    context.add_function("round", instant_math_round);
    context.add_function("trunc", instant_math_trunc);
    context.add_function("sign", instant_math_sign);
    context.add_function("sqrt", instant_math_sqrt);
    context.add_function("isInf", instant_math_is_inf);
    context.add_function("isNaN", instant_math_is_nan);
    context.add_function("isFinite", instant_math_is_finite);
    context.add_function("bitAnd", instant_math_bit_and);
    context.add_function("bitOr", instant_math_bit_or);
    context.add_function("bitXor", instant_math_bit_xor);
    context.add_function("bitNot", instant_math_bit_not);
    context.add_function("bitShiftLeft", instant_math_bit_shift_left);
    context.add_function("bitShiftRight", instant_math_bit_shift_right);
}

fn plan_rate_limit(
    This(target): This<Value>,
    Arguments(arguments): Arguments,
    runtime: Option<&RateLimitRuntime>,
    requests: &Mutex<Vec<RateLimitRequest>>,
) -> Result<bool, ExecutionError> {
    let runtime = runtime.ok_or_else(|| {
        ExecutionError::function_error("limit", "no rate limit consumer is configured")
    })?;
    let Value::Map(target) = target else {
        return Err(ExecutionError::function_error(
            "limit",
            "target is not a rate limit bucket",
        ));
    };
    let bucket_name = match target.get(&KeyRef::String("__instant_rate_limit_bucket")) {
        Some(Value::String(name)) => name.to_string(),
        _ => {
            return Err(ExecutionError::function_error(
                "limit",
                "target is not a rate limit bucket",
            ));
        }
    };
    let Some(bucket_key) = arguments.first() else {
        return Err(ExecutionError::function_error(
            "limit",
            "bucket key is required",
        ));
    };
    let tokens = match arguments.get(1) {
        None => 1,
        Some(Value::Int(tokens)) if *tokens > 0 => u64::try_from(*tokens).map_err(|_| {
            ExecutionError::function_error("limit", "tokens must be a positive integer")
        })?,
        Some(_) => {
            return Err(ExecutionError::function_error(
                "limit",
                "tokens must be a positive integer",
            ));
        }
    };
    if arguments.len() > 2 {
        return Err(ExecutionError::function_error(
            "limit",
            "limit accepts a bucket key and optional token count",
        ));
    }
    let config = runtime
        .configs
        .get(&bucket_name)
        .cloned()
        .ok_or_else(|| ExecutionError::function_error("limit", "unknown rate limit bucket"))?;
    let bucket_key = bucket_key
        .json()
        .map_err(|error| ExecutionError::function_error("limit", error.to_string()))?;
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(RateLimitRequest {
            app_id: runtime.app_id,
            bucket_name,
            config,
            bucket_key,
            tokens,
        });
    Ok(true)
}

/// Borrowed view of one evaluation's binding inputs.
///
/// Entity evaluations share every base field and replace only `data` and its
/// entity type, so batched query filtering never clones auth, rule parameters
/// or the request binding once per entity.
struct BindingView<'a> {
    data: &'a JsonValue,
    data_etype: Option<&'a str>,
    auth: &'a JsonValue,
    rule_params: &'a JsonValue,
    request: &'a RequestBinding,
    rate_limit: &'a JsonValue,
    rate_limit_runtime: Option<&'a RateLimitRuntime>,
    deferred_rate_limit_etypes: &'a HashSet<String>,
    new_data: Option<&'a JsonValue>,
    linked_data: Option<&'a JsonValue>,
    linked_data_etype: Option<&'a str>,
    actions: Option<&'a JsonValue>,
}

impl<'a> BindingView<'a> {
    fn from_bindings(bindings: &'a EvaluationBindings) -> Self {
        // Exhaustive on purpose: a new binding field must be added to the view,
        // or evaluation through the view would silently drop it.
        let EvaluationBindings {
            data,
            data_etype,
            auth,
            rule_params,
            request,
            rate_limit,
            rate_limit_runtime,
            deferred_rate_limit_etypes,
            new_data,
            linked_data,
            linked_data_etype,
            actions,
        } = bindings;
        Self {
            data,
            data_etype: data_etype.as_deref(),
            auth,
            rule_params,
            request,
            rate_limit,
            rate_limit_runtime: rate_limit_runtime.as_ref(),
            deferred_rate_limit_etypes,
            new_data: new_data.as_ref(),
            linked_data: linked_data.as_ref(),
            linked_data_etype: linked_data_etype.as_deref(),
            actions: actions.as_ref(),
        }
    }

    fn for_entity(base: &'a EvaluationBindings, data: &'a JsonValue, etype: &'a str) -> Self {
        let mut bindings = Self::from_bindings(base);
        bindings.data = data;
        bindings.data_etype = Some(etype);
        bindings
    }
}

#[cfg(test)]
fn consume_rate_limits(
    bindings: &EvaluationBindings,
    requests: &Mutex<Vec<RateLimitRequest>>,
) -> Result<(), PermissionError> {
    consume_rate_limits_view(&BindingView::from_bindings(bindings), requests)
}

fn consume_rate_limits_view(
    bindings: &BindingView<'_>,
    requests: &Mutex<Vec<RateLimitRequest>>,
) -> Result<(), PermissionError> {
    let requests = requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if requests.is_empty() {
        return Ok(());
    }
    if bindings
        .data_etype
        .is_some_and(|etype| bindings.deferred_rate_limit_etypes.contains(etype))
    {
        return Ok(());
    }
    let runtime = bindings
        .rate_limit_runtime
        .ok_or(PermissionError::RateLimitUnavailable)?;
    for request in requests.iter() {
        match runtime
            .consumer
            .consume(request)
            .map_err(PermissionError::RateLimitStore)?
        {
            RateLimitDecision::Allowed { .. } => {}
            RateLimitDecision::Denied {
                retry_after,
                remaining_tokens,
            } => {
                return Err(PermissionError::rate_limited(retry_after, remaining_tokens));
            }
        }
    }
    Ok(())
}

fn add_string_extension_bindings(context: &mut Context<'_>) {
    context.add_function("charAt", instant_string_char_at);
    context.add_function("indexOf", instant_string_index_of);
    context.add_function("join", instant_string_join);
    context.add_function("lastIndexOf", instant_string_last_index_of);
    context.add_function("lowerAscii", instant_string_lower_ascii);
    context.add_function("replace", instant_string_replace);
    context.add_function("split", instant_string_split);
    context.add_function("substring", instant_string_substring);
    context.add_function("trim", instant_string_trim);
    context.add_function("upperAscii", instant_string_upper_ascii);
}

fn string_extension_error(function: &str, message: impl Into<String>) -> ExecutionError {
    ExecutionError::function_error(function, message.into())
}

fn string_extension_failure(function: &str, detail: impl std::fmt::Display) -> ExecutionError {
    string_extension_error(function, format!("{function} failure: {detail}"))
}

fn string_extension_arity(function: &str, actual: usize, expected: &str) -> ExecutionError {
    string_extension_failure(
        function,
        format!("expected {expected} argument(s), received {actual}"),
    )
}

fn string_value<'a>(value: &'a Value, function: &str) -> Result<&'a str, ExecutionError> {
    match value {
        Value::String(value) => Ok(value),
        value => Err(string_extension_failure(
            function,
            format!("argument must be a string, got {value:?}"),
        )),
    }
}

fn int_value(value: &Value, function: &str) -> Result<i64, ExecutionError> {
    match value {
        Value::Int(value) => Ok(*value),
        value => Err(string_extension_failure(
            function,
            format!("argument must be an int, got {value:?}"),
        )),
    }
}

fn int32_index(value: i64, function: &str, label: &str) -> Result<i32, ExecutionError> {
    i32::try_from(value).map_err(|_| {
        string_extension_failure(
            function,
            format!("{label} must not exceed the int32 range: {value}"),
        )
    })
}

fn codepoint_index_of(haystack: &[char], needle: &[char], offset: usize) -> Option<usize> {
    if needle.is_empty() {
        return (offset <= haystack.len()).then_some(offset);
    }
    if offset > haystack.len().saturating_sub(needle.len()) || needle.len() > haystack.len() {
        return None;
    }
    (offset..=haystack.len() - needle.len())
        .find(|index| haystack[*index..*index + needle.len()] == *needle)
}

fn instant_string_char_at(
    This(target): This<Arc<String>>,
    index: i64,
) -> Result<Value, ExecutionError> {
    let index = int32_index(index, "charAt", "Index")?;
    let codepoints = target.chars().collect::<Vec<_>>();
    let index = usize::try_from(index)
        .map_err(|_| string_extension_failure("charAt", format!("Index out of range: {index}")))?;
    if index > codepoints.len() {
        return Err(string_extension_failure(
            "charAt",
            format!("Index out of range: {index}"),
        ));
    }
    Ok(Value::String(
        codepoints
            .get(index)
            .map_or_else(String::new, char::to_string)
            .into(),
    ))
}

fn instant_string_index_of(
    This(target): This<Arc<String>>,
    Arguments(arguments): Arguments,
) -> Result<i64, ExecutionError> {
    let (needle, offset) = match arguments.as_slice() {
        [needle] => (string_value(needle, "indexOf")?, 0),
        [needle, offset] => (
            string_value(needle, "indexOf")?,
            int_value(offset, "indexOf")?,
        ),
        _ => {
            return Err(string_extension_arity(
                "indexOf",
                arguments.len(),
                "one or two",
            ));
        }
    };
    let offset = int32_index(offset, "indexOf", "Offset")?;
    if needle.is_empty() {
        return Ok(i64::from(offset));
    }
    let haystack = target.chars().collect::<Vec<_>>();
    let needle = needle.chars().collect::<Vec<_>>();
    let offset = usize::try_from(offset).map_err(|_| {
        string_extension_failure("indexOf", format!("Offset out of range: {offset}"))
    })?;
    if offset >= haystack.len() {
        return Err(string_extension_failure(
            "indexOf",
            format!("Offset out of range: {offset}"),
        ));
    }
    Ok(codepoint_index_of(&haystack, &needle, offset)
        .map_or(-1, |index| i64::try_from(index).unwrap_or(i64::MAX)))
}

fn instant_string_join(
    This(target): This<Value>,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    let separator = match arguments.as_slice() {
        [] => "",
        [separator] => string_value(separator, "join")?,
        _ => {
            return Err(string_extension_arity(
                "join",
                arguments.len(),
                "zero or one",
            ));
        }
    };
    let Value::List(values) = target else {
        return Err(string_extension_error("join", "target must be a list"));
    };
    let mut output = String::new();
    for (index, value) in values.iter().enumerate() {
        let value = java_join_value(value)?;
        let separator = if index == 0 { "" } else { separator };
        if output
            .len()
            .saturating_add(separator.len())
            .saturating_add(value.len())
            > 1_048_576
        {
            return Err(ExecutionError::WorkLimitExceeded);
        }
        output.push_str(separator);
        output.push_str(&value);
    }
    Ok(Value::String(output.into()))
}

fn java_join_value(value: &Value) -> Result<String, ExecutionError> {
    if matches!(value, Value::Null) {
        return Err(string_extension_failure("join", "null list element"));
    }
    Ok(java_object_string(value))
}

fn java_object_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.to_string(),
        Value::Int(value) => value.to_string(),
        Value::UInt(value) => value.to_string(),
        Value::Float(value) => java_double_string(*value),
        Value::Bool(value) => value.to_string(),
        Value::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(java_object_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Map(value) => {
            let mut entries = value
                .map
                .iter()
                .map(|(key, value)| (key.to_string(), java_object_string(value)))
                .collect::<Vec<_>>();
            entries.sort();
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::Bytes(value) => format!("{:?}", value.as_slice()),
        Value::Duration(value) => value.to_string(),
        Value::Timestamp(value) => value.to_string(),
        Value::Null => "null".to_owned(),
        Value::Function(name, _) => name.to_string(),
        Value::Opaque(value) => format!("{:?}", value.as_debug()),
    }
}

fn java_double_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value == f64::INFINITY {
        return "Infinity".to_owned();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0".to_owned()
        } else {
            "0.0".to_owned()
        };
    }

    let negative = value.is_sign_negative();
    let raw = value.abs().to_string();
    let (coefficient, explicit_exponent) = raw
        .split_once(['e', 'E'])
        .map_or((raw.as_str(), 0), |(coefficient, exponent)| {
            (coefficient, exponent.parse::<i32>().unwrap_or(0))
        });
    let integer_digits = coefficient.find('.').unwrap_or(coefficient.len());
    let mut digits = coefficient
        .bytes()
        .filter(u8::is_ascii_digit)
        .map(char::from)
        .collect::<String>();
    let leading_zeroes = digits.bytes().take_while(|digit| *digit == b'0').count();
    digits.drain(..leading_zeroes);
    let exponent = i32::try_from(integer_digits).unwrap_or(i32::MAX) - 1 + explicit_exponent
        - i32::try_from(leading_zeroes).unwrap_or(i32::MAX);
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }

    let sign = if negative { "-" } else { "" };
    if !(-3..7).contains(&exponent) {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() { "0" } else { rest };
        return format!("{sign}{first}.{fraction}E{exponent}");
    }
    if exponent < 0 {
        return format!(
            "{sign}0.{}{digits}",
            "0".repeat(usize::try_from(-exponent - 1).unwrap_or(0))
        );
    }
    let integer_length = usize::try_from(exponent + 1).unwrap_or(usize::MAX);
    if integer_length >= digits.len() {
        return format!(
            "{sign}{digits}{}.0",
            "0".repeat(integer_length.saturating_sub(digits.len()))
        );
    }
    let (integer, fraction) = digits.split_at(integer_length);
    format!("{sign}{integer}.{fraction}")
}

fn instant_string_last_index_of(
    This(target): This<Arc<String>>,
    Arguments(arguments): Arguments,
) -> Result<i64, ExecutionError> {
    let haystack = target.chars().collect::<Vec<_>>();
    let (needle, offset) = match arguments.as_slice() {
        [needle] => (string_value(needle, "lastIndexOf")?, None),
        [needle, offset] => (
            string_value(needle, "lastIndexOf")?,
            Some(int_value(offset, "lastIndexOf")?),
        ),
        _ => {
            return Err(string_extension_arity(
                "lastIndexOf",
                arguments.len(),
                "one or two",
            ));
        }
    };
    let needle = needle.chars().collect::<Vec<_>>();
    if needle.is_empty() {
        return Ok(offset.unwrap_or_else(|| i64::try_from(haystack.len()).unwrap_or(i64::MAX)));
    }
    if offset.is_none() && haystack.len() < needle.len() {
        return Ok(-1);
    }
    let offset = offset
        .unwrap_or_else(|| i64::try_from(haystack.len().saturating_sub(1)).unwrap_or(i64::MAX));
    let offset = int32_index(offset, "lastIndexOf", "Offset")?;
    let offset = usize::try_from(offset).map_err(|_| {
        string_extension_failure("lastIndexOf", format!("Offset out of range: {offset}"))
    })?;
    if offset >= haystack.len() {
        return Err(string_extension_failure(
            "lastIndexOf",
            format!("Offset out of range: {offset}"),
        ));
    }
    if needle.len() > haystack.len() {
        return Ok(-1);
    }
    let start = offset.min(haystack.len() - needle.len());
    Ok((0..=start)
        .rev()
        .find(|index| haystack[*index..*index + needle.len()] == *needle)
        .map_or(-1, |index| i64::try_from(index).unwrap_or(i64::MAX)))
}

fn instant_string_lower_ascii(This(target): This<Arc<String>>) -> String {
    target.to_ascii_lowercase()
}

fn instant_string_upper_ascii(This(target): This<Arc<String>>) -> String {
    target.to_ascii_uppercase()
}

fn replace_codepoints(
    text: &str,
    search: &str,
    replacement: &str,
    limit: i32,
) -> Result<String, ExecutionError> {
    if search == replacement || limit == 0 {
        return Ok(text.to_owned());
    }
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    // Rust's literal matching observes UTF-8 boundaries, including empty-pattern
    // insertion between codepoints. Count the exact output before allocating it.
    let matches = text.match_indices(search).take(limit).count();
    let output_bytes = text
        .len()
        .saturating_sub(matches.saturating_mul(search.len()))
        .saturating_add(matches.saturating_mul(replacement.len()));
    if output_bytes > 1_048_576 {
        return Err(ExecutionError::WorkLimitExceeded);
    }
    Ok(text.replacen(search, replacement, limit))
}

fn instant_string_replace(
    This(target): This<Arc<String>>,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    let (search, replacement, limit) = match arguments.as_slice() {
        [search, replacement] => (
            string_value(search, "replace")?,
            string_value(replacement, "replace")?,
            -1,
        ),
        [search, replacement, limit] => (
            string_value(search, "replace")?,
            string_value(replacement, "replace")?,
            int32_index(int_value(limit, "replace")?, "replace", "Index")?,
        ),
        _ => {
            return Err(string_extension_arity(
                "replace",
                arguments.len(),
                "two or three",
            ));
        }
    };
    Ok(Value::String(
        replace_codepoints(&target, search, replacement, limit)?.into(),
    ))
}

fn explode_codepoints(value: &str, mut limit: i32) -> Vec<String> {
    let codepoints = value.chars().collect::<Vec<_>>();
    if limit > 0 {
        limit -= 1;
    }
    let individual = codepoints.len().min(usize::try_from(limit).unwrap_or(0));
    let mut result = codepoints[..individual]
        .iter()
        .map(char::to_string)
        .collect::<Vec<_>>();
    if codepoints.len() > usize::try_from(limit).unwrap_or(0) {
        result.push(codepoints[individual..].iter().collect());
    }
    result
}

fn instant_string_split(
    This(target): This<Arc<String>>,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    let (separator, mut limit) = match arguments.as_slice() {
        [separator] => (string_value(separator, "split")?, i32::MAX),
        [separator, limit] => (
            string_value(separator, "split")?,
            int32_index(int_value(limit, "split")?, "split", "Limit")?,
        ),
        _ => {
            return Err(string_extension_arity(
                "split",
                arguments.len(),
                "one or two",
            ));
        }
    };
    if limit == 0 {
        return Ok(Value::List(Vec::new().into()));
    }
    if limit == 1 {
        return Ok(Value::List(vec![Value::String(target)].into()));
    }
    if limit < 0 {
        limit = i32::try_from(target.encode_utf16().count()).unwrap_or(i32::MAX);
        if limit == 0 && !separator.is_empty() {
            return Err(string_extension_failure(
                "split",
                "must be greater than zero: 0",
            ));
        }
    }
    let parts = if separator.is_empty() {
        explode_codepoints(&target, limit)
    } else {
        target
            .splitn(usize::try_from(limit).unwrap_or(usize::MAX), separator)
            .map(str::to_owned)
            .collect()
    };
    Ok(Value::List(
        parts
            .into_iter()
            .map(|part| Value::String(part.into()))
            .collect::<Vec<_>>()
            .into(),
    ))
}

fn instant_string_substring(
    This(target): This<Arc<String>>,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    let (begin, end) = match arguments.as_slice() {
        [begin] => (int_value(begin, "substring")?, None),
        [begin, end] => (
            int_value(begin, "substring")?,
            Some(int_value(end, "substring")?),
        ),
        _ => {
            return Err(string_extension_arity(
                "substring",
                arguments.len(),
                "one or two",
            ));
        }
    };
    let codepoints = target.chars().collect::<Vec<_>>();
    let (begin, end) = match end {
        Some(end) => {
            let Ok(begin_index) = i32::try_from(begin) else {
                return Err(string_extension_failure(
                    "substring",
                    format!("Indices must not exceed the int32 range: [{begin}, {end})"),
                ));
            };
            let Ok(end_index) = i32::try_from(end) else {
                return Err(string_extension_failure(
                    "substring",
                    format!("Indices must not exceed the int32 range: [{begin}, {end})"),
                ));
            };
            (begin_index, end_index)
        }
        None => (
            i32::try_from(begin).map_err(|_| {
                string_extension_failure(
                    "substring",
                    format!("Index must not exceed the int32 range: {begin}"),
                )
            })?,
            i32::try_from(codepoints.len()).unwrap_or(i32::MAX),
        ),
    };
    if begin < 0
        || begin > end
        || usize::try_from(begin).unwrap_or(usize::MAX) > codepoints.len()
        || usize::try_from(end).unwrap_or(usize::MAX) > codepoints.len()
    {
        return Err(string_extension_failure(
            "substring",
            format!("Range [{begin}, {end}) out of bounds"),
        ));
    }
    let begin = usize::try_from(begin).expect("non-negative begin index");
    let end = usize::try_from(end).expect("non-negative bounded end index");
    Ok(Value::String(
        codepoints[begin..end].iter().collect::<String>().into(),
    ))
}

const fn cel_string_whitespace(character: char) -> bool {
    matches!(
        character as u32,
        0x0009..=0x000D
            | 0x0020
            | 0x0085
            | 0x00A0
            | 0x1680
            | 0x2000..=0x200A
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
    )
}

fn instant_string_trim(This(target): This<Arc<String>>) -> String {
    target.trim_matches(cel_string_whitespace).to_owned()
}

fn instant_ref(
    This(target): This<Value>,
    path: &str,
    ref_values: &HashMap<RefRequest, Vec<Value>>,
    missing_refs: &Mutex<HashSet<RefRequest>>,
) -> Result<Value, ExecutionError> {
    let Value::Map(target) = target else {
        return Err(ExecutionError::function_error(
            "ref",
            "target is not an object",
        ));
    };
    let entity_id = match target.get(&KeyRef::String("id")) {
        None | Some(Value::Null) => return Ok(Value::List(Vec::new().into())),
        Some(Value::String(value)) => value.parse().map_or_else(
            |_| RefEntityId::String(value.to_string()),
            RefEntityId::Uuid,
        ),
        Some(Value::Int(value)) => RefEntityId::Int(*value),
        Some(Value::UInt(value)) => RefEntityId::UInt(*value),
        Some(Value::Float(value)) if value.is_finite() => RefEntityId::Float(value.to_bits()),
        Some(Value::Bool(value)) => RefEntityId::Bool(*value),
        _ => {
            return Err(ExecutionError::function_error(
                "ref",
                "target id is not a scalar",
            ));
        }
    };
    let etype = match target.get(&KeyRef::String("__instant_ref_etype")) {
        Some(Value::String(value)) => value.to_string(),
        _ => {
            return Err(ExecutionError::function_error(
                "ref",
                "target has no entity type",
            ));
        }
    };
    let path = if etype == "$users" {
        path.strip_prefix("$user.").unwrap_or(path).to_owned()
    } else {
        path.to_owned()
    };
    let request = RefRequest {
        entity_id,
        etype,
        path,
    };
    if let Some(values) = ref_values.get(&request) {
        return Ok(Value::List(values.clone().into()));
    }
    missing_refs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(request);
    Err(ExecutionError::function_error("ref", "missing reference"))
}

fn instant_convert_bool(value: Value) -> Result<Value, ExecutionError> {
    match value {
        Value::Bool(value) => Ok(Value::Bool(value)),
        Value::String(value) => match value.as_str() {
            "true" | "TRUE" | "True" | "t" | "1" => Ok(Value::Bool(true)),
            "false" | "FALSE" | "False" | "f" | "0" => Ok(Value::Bool(false)),
            value => Err(ExecutionError::function_error(
                "bool",
                format!("Type conversion error from 'string' to 'bool': [{value}]"),
            )),
        },
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn instant_convert_int(value: Value) -> Result<Value, ExecutionError> {
    match value {
        Value::Int(value) => Ok(Value::Int(value)),
        Value::UInt(value) => i64::try_from(value)
            .map(Value::Int)
            .map_err(|_| ExecutionError::function_error("int", "unsigned out of int range")),
        Value::Float(value)
            if value.is_finite()
                && value >= i64::MIN as f64
                && value < 9_223_372_036_854_775_808.0 =>
        {
            Ok(Value::Int(value as i64))
        }
        Value::Float(_) => Err(ExecutionError::function_error(
            "int",
            "double is out of range for int",
        )),
        Value::String(value) => value.parse::<i64>().map(Value::Int).map_err(|error| {
            ExecutionError::function_error("int", format!("string parse error: {error}"))
        }),
        Value::Timestamp(value) => Ok(Value::Int(value.timestamp())),
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn instant_convert_uint(value: Value) -> Result<Value, ExecutionError> {
    match value {
        Value::UInt(value) => Ok(Value::UInt(value)),
        Value::Int(value) => u64::try_from(value)
            .map(Value::UInt)
            .map_err(|_| ExecutionError::function_error("uint", "integer out of uint range")),
        Value::Float(value)
            if value.is_finite() && (0.0..18_446_744_073_709_551_616.0).contains(&value) =>
        {
            Ok(Value::UInt(value as u64))
        }
        Value::Float(_) => Err(ExecutionError::function_error(
            "uint",
            "double is out of range for uint",
        )),
        Value::String(value) => value.parse::<u64>().map(Value::UInt).map_err(|error| {
            ExecutionError::function_error("uint", format!("string parse error: {error}"))
        }),
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

fn instant_convert_string(value: Value) -> Result<Value, ExecutionError> {
    let value = match value {
        Value::String(value) => return Ok(Value::String(value)),
        Value::Int(value) => value.to_string(),
        Value::UInt(value) => value.to_string(),
        Value::Float(value) => java_double_string(value),
        Value::Bool(value) => value.to_string(),
        Value::Bytes(value) => std::str::from_utf8(&value)
            .map(str::to_owned)
            .map_err(|_| {
                ExecutionError::function_error(
                    "string",
                    "invalid UTF-8 in bytes, cannot convert to string",
                )
            })?,
        Value::Timestamp(value) => value
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true),
        Value::Duration(value) => cel_java_duration_string(value),
        value => return Err(ExecutionError::unsupported_target_type(value)),
    };
    Ok(Value::String(value.into()))
}

fn cel_java_duration_string(value: ChronoDuration) -> String {
    if value == ChronoDuration::zero() {
        return "0s".to_owned();
    }
    let seconds = value.num_seconds();
    let remainder = value - ChronoDuration::seconds(seconds);
    let nanos = remainder.num_nanoseconds().unwrap_or_default();
    let negative = seconds < 0 || nanos < 0;
    let seconds = seconds.unsigned_abs();
    let nanos = nanos.unsigned_abs();
    let fraction = if nanos == 0 {
        String::new()
    } else if nanos.is_multiple_of(1_000_000) {
        format!(".{:03}", nanos / 1_000_000)
    } else if nanos.is_multiple_of(1_000) {
        format!(".{:06}", nanos / 1_000)
    } else {
        format!(".{nanos:09}")
    };
    format!("{}{seconds}{fraction}s", if negative { "-" } else { "" })
}

fn instant_duration(value: Value) -> Result<Value, ExecutionError> {
    match value {
        Value::Duration(value) => Ok(Value::Duration(value)),
        Value::String(value) => parse_cel_duration(&value).map(Value::Duration),
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

fn parse_cel_duration(value: &str) -> Result<ChronoDuration, ExecutionError> {
    let numeric = value
        .strip_suffix('s')
        .ok_or_else(|| ExecutionError::function_error("duration", "invalid duration format"))?;
    let negative = numeric.starts_with('-');
    let numeric = numeric
        .strip_prefix('-')
        .or_else(|| numeric.strip_prefix('+'))
        .unwrap_or(numeric);
    let (seconds, fraction) = numeric.split_once('.').unwrap_or((numeric, ""));
    if seconds.is_empty()
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ExecutionError::function_error(
            "duration",
            "invalid duration format",
        ));
    }
    let seconds = seconds
        .parse::<i64>()
        .map_err(|_| ExecutionError::function_error("duration", "invalid duration range"))?;
    if seconds > 315_576_000_000 {
        return Err(ExecutionError::function_error(
            "duration",
            "invalid duration range",
        ));
    }
    let nanos = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<9}")
            .parse::<i64>()
            .map_err(|_| ExecutionError::function_error("duration", "invalid duration format"))?
    };
    let value = ChronoDuration::seconds(seconds) + ChronoDuration::nanoseconds(nanos);
    Ok(if negative { -value } else { value })
}

#[allow(clippy::needless_pass_by_value)] // Required by cel-rust's extractor ABI.
fn instant_matches(This(value): This<Value>, pattern: Arc<String>) -> Result<bool, ExecutionError> {
    let Value::String(value) = value else {
        return Err(ExecutionError::function_error(
            "matches",
            "string target required",
        ));
    };
    regex::Regex::new(&pattern)
        .map(|pattern| pattern.is_match(&value))
        .map_err(|error| ExecutionError::function_error("matches", error.to_string()))
}

fn instant_timestamp(value: Value) -> Result<Value, ExecutionError> {
    let timestamp = match value {
        Value::Int(epoch_millis) => DateTime::from_timestamp_millis(epoch_millis)
            .map(|timestamp| timestamp.fixed_offset())
            .ok_or_else(|| ExecutionError::function_error("timestamp", "invalid epoch millis"))?,
        Value::String(value) => parse_timestamp(&value)
            .ok_or_else(|| ExecutionError::function_error("timestamp", "invalid date string"))?,
        Value::Timestamp(value) => value,
        value => return Err(ExecutionError::unsupported_target_type(value)),
    };
    Ok(Value::Timestamp(timestamp))
}

fn instant_get_time(This(timestamp): This<DateTime<FixedOffset>>) -> i64 {
    timestamp.timestamp()
}

#[derive(Clone, Copy)]
enum DateTimePart {
    FullYear,
    Month,
    DayOfYear,
    DayOfMonth,
    Date,
    DayOfWeek,
    Hours,
    Minutes,
    Seconds,
    Milliseconds,
}

fn instant_date_time(
    This(value): This<Value>,
    Arguments(arguments): Arguments,
    part: DateTimePart,
) -> Result<Value, ExecutionError> {
    match value {
        Value::Timestamp(timestamp) => {
            let timezone = match arguments.as_slice() {
                [] => None,
                [Value::String(timezone)] => Some(timezone.as_str()),
                _ => return Err(static_runtime_overload(part)),
            };
            timestamp_part(&timestamp, timezone, part).map(Value::Int)
        }
        Value::Duration(duration)
            if arguments.is_empty()
                && matches!(
                    part,
                    DateTimePart::Hours
                        | DateTimePart::Minutes
                        | DateTimePart::Seconds
                        | DateTimePart::Milliseconds
                ) =>
        {
            Ok(Value::Int(match part {
                DateTimePart::Hours => duration.num_hours(),
                DateTimePart::Minutes => duration.num_minutes(),
                DateTimePart::Seconds => duration.num_seconds(),
                DateTimePart::Milliseconds => duration.num_milliseconds() % 1_000,
                _ => unreachable!(),
            }))
        }
        _ => Err(static_runtime_overload(part)),
    }
}

fn static_runtime_overload(part: DateTimePart) -> ExecutionError {
    ExecutionError::function_error(date_time_name(part), "no matching overload")
}

const fn date_time_name(part: DateTimePart) -> &'static str {
    match part {
        DateTimePart::FullYear => "getFullYear",
        DateTimePart::Month => "getMonth",
        DateTimePart::DayOfYear => "getDayOfYear",
        DateTimePart::DayOfMonth => "getDayOfMonth",
        DateTimePart::Date => "getDate",
        DateTimePart::DayOfWeek => "getDayOfWeek",
        DateTimePart::Hours => "getHours",
        DateTimePart::Minutes => "getMinutes",
        DateTimePart::Seconds => "getSeconds",
        DateTimePart::Milliseconds => "getMilliseconds",
    }
}

fn timestamp_part(
    timestamp: &DateTime<FixedOffset>,
    timezone: Option<&str>,
    part: DateTimePart,
) -> Result<i64, ExecutionError> {
    let Some(timezone) = timezone else {
        return Ok(date_time_part(&timestamp.with_timezone(&Utc), part));
    };
    if let Ok(timezone) = timezone.parse::<Tz>() {
        return Ok(date_time_part(&timestamp.with_timezone(&timezone), part));
    }
    let offset = parse_cel_fixed_offset(timezone)
        .ok_or_else(|| ExecutionError::function_error(date_time_name(part), "invalid timezone"))?;
    Ok(date_time_part(&timestamp.with_timezone(&offset), part))
}

fn parse_cel_fixed_offset(value: &str) -> Option<FixedOffset> {
    let (hours, minutes) = value.split_once(':')?;
    let negative = hours.starts_with('-');
    let hours = hours
        .strip_prefix('-')
        .or_else(|| hours.strip_prefix('+'))
        .unwrap_or(hours);
    let hours = hours.parse::<i32>().ok()?;
    let minutes = minutes.parse::<i32>().ok()?;
    if hours > 18 || minutes > 59 {
        return None;
    }
    let seconds = hours
        .checked_mul(3_600)?
        .checked_add(minutes.checked_mul(60)?)?;
    FixedOffset::east_opt(if negative { -seconds } else { seconds })
}

fn date_time_part<T: chrono::TimeZone>(timestamp: &DateTime<T>, part: DateTimePart) -> i64 {
    match part {
        DateTimePart::FullYear => i64::from(timestamp.year()),
        DateTimePart::Month => i64::from(timestamp.month0()),
        DateTimePart::DayOfYear => i64::from(timestamp.ordinal0()),
        DateTimePart::DayOfMonth => i64::from(timestamp.day0()),
        DateTimePart::Date => i64::from(timestamp.day()),
        DateTimePart::DayOfWeek => i64::from(timestamp.weekday().num_days_from_sunday()),
        DateTimePart::Hours => i64::from(timestamp.hour()),
        DateTimePart::Minutes => i64::from(timestamp.minute()),
        DateTimePart::Seconds => i64::from(timestamp.second()),
        DateTimePart::Milliseconds => i64::from(timestamp.timestamp_subsec_millis()),
    }
}

fn instant_math_abs(This(target): This<Value>, value: Value) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "abs")?;
    match value {
        Value::Int(value) => value
            .checked_abs()
            .map(Value::Int)
            .ok_or_else(|| ExecutionError::function_error("abs", "integer result is out of range")),
        Value::UInt(value) => Ok(Value::UInt(value)),
        Value::Float(value) => Ok(Value::Float(value.abs())),
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

fn require_math_namespace(target: Value, function: &str) -> Result<(), ExecutionError> {
    let Value::Map(target) = target else {
        return Err(ExecutionError::function_error(
            function,
            "target is not the math namespace",
        ));
    };
    if matches!(
        target.get(&KeyRef::String("__instant_math")),
        Some(Value::Bool(true))
    ) {
        Ok(())
    } else {
        Err(ExecutionError::function_error(
            function,
            "target is not the math namespace",
        ))
    }
}

fn math_arguments(arguments: &[Value], function: &str) -> Result<Vec<Value>, ExecutionError> {
    let values = match arguments {
        [Value::List(values)] => values.as_ref().clone(),
        values => values.to_vec(),
    };
    if values.is_empty() {
        return Err(ExecutionError::function_error(
            function,
            "at least one numeric argument is required",
        ));
    }
    if values.iter().all(math_numeric) {
        Ok(values)
    } else {
        Err(ExecutionError::function_error(
            function,
            "arguments must be numeric",
        ))
    }
}

const fn math_numeric(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::UInt(_) | Value::Float(_))
}

fn instant_math_greatest(
    This(target): This<Value>,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "greatest")?;
    math_extreme(arguments.as_ref(), "greatest", Ordering::Greater)
}

fn instant_math_least(
    This(target): This<Value>,
    Arguments(arguments): Arguments,
) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "least")?;
    math_extreme(arguments.as_ref(), "least", Ordering::Less)
}

fn math_extreme(
    arguments: &[Value],
    function: &str,
    replace_on: Ordering,
) -> Result<Value, ExecutionError> {
    let values = math_arguments(arguments, function)?;
    let mut result = values[0].clone();
    for value in values.into_iter().skip(1) {
        if math_numeric_compare(&value, &result) == replace_on {
            result = value;
        }
    }
    Ok(result)
}

fn math_numeric_compare(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Int(left), Value::Int(right)) => left.cmp(right),
        (Value::UInt(left), Value::UInt(right)) => left.cmp(right),
        (Value::Float(left), Value::Float(right)) => java_double_compare(*left, *right),
        (Value::Int(left), Value::UInt(right)) => compare_int_uint(*left, *right),
        (Value::UInt(left), Value::Int(right)) => compare_int_uint(*right, *left).reverse(),
        (Value::Int(left), Value::Float(right)) => compare_double_int(*right, *left).reverse(),
        (Value::Float(left), Value::Int(right)) => compare_double_int(*left, *right),
        (Value::UInt(left), Value::Float(right)) => compare_double_uint(*right, *left).reverse(),
        (Value::Float(left), Value::UInt(right)) => compare_double_uint(*left, *right),
        _ => unreachable!("math arguments are validated before comparison"),
    }
}

fn java_double_compare(left: f64, right: f64) -> Ordering {
    if left.is_nan() {
        return if right.is_nan() {
            Ordering::Equal
        } else {
            Ordering::Greater
        };
    }
    if right.is_nan() {
        return Ordering::Less;
    }
    left.total_cmp(&right)
}

fn compare_double_int(double: f64, integer: i64) -> Ordering {
    const INT_MIN_AS_DOUBLE: f64 = -9_223_372_036_854_775_808.0;
    const INT_MAX_AS_DOUBLE: f64 = 9_223_372_036_854_775_808.0;
    if double < INT_MIN_AS_DOUBLE {
        Ordering::Less
    } else if double > INT_MAX_AS_DOUBLE {
        Ordering::Greater
    } else {
        java_double_compare(double, i64_as_f64(integer))
    }
}

fn compare_double_uint(double: f64, integer: u64) -> Ordering {
    const UINT_MAX_AS_DOUBLE: f64 = 18_446_744_073_709_551_616.0;
    if double < 0.0 {
        Ordering::Less
    } else if double > UINT_MAX_AS_DOUBLE {
        Ordering::Greater
    } else {
        java_double_compare(double, u64_as_f64(integer))
    }
}

fn compare_int_uint(integer: i64, unsigned: u64) -> Ordering {
    if integer < 0 || unsigned >= i64::MAX as u64 {
        Ordering::Less
    } else {
        integer.cast_unsigned().cmp(&unsigned)
    }
}

#[allow(clippy::cast_precision_loss)] // Matches cel-java Number.doubleValue conversion exactly.
fn i64_as_f64(value: i64) -> f64 {
    value as f64
}

#[allow(clippy::cast_precision_loss)] // Matches cel-java Number.doubleValue conversion exactly.
fn u64_as_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn java_double_to_long_as_double(value: f64) -> f64 {
    // Java's Double.longValue saturates finite out-of-range values.
    (value as i64) as f64
}

macro_rules! unary_float_math {
    ($name:ident, $function:literal, $operation:expr) => {
        fn $name(This(target): This<Value>, value: Value) -> Result<Value, ExecutionError> {
            require_math_namespace(target, $function)?;
            let Value::Float(value) = value else {
                return Err(ExecutionError::unsupported_target_type(value));
            };
            Ok(Value::Float($operation(value)))
        }
    };
}

unary_float_math!(instant_math_ceil, "ceil", f64::ceil);
unary_float_math!(instant_math_floor, "floor", f64::floor);

fn instant_math_round(This(target): This<Value>, value: Value) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "round")?;
    let Value::Float(value) = value else {
        return Err(ExecutionError::unsupported_target_type(value));
    };
    if !value.is_finite() {
        return Ok(Value::Float(value));
    }
    let rounded = value.round_ties_even();
    if !((-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&rounded)) {
        return Err(ExecutionError::function_error(
            "round",
            "rounded value is out of int64 range",
        ));
    }
    Ok(Value::Float(java_double_to_long_as_double(rounded)))
}

fn instant_math_trunc(This(target): This<Value>, value: Value) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "trunc")?;
    let Value::Float(value) = value else {
        return Err(ExecutionError::unsupported_target_type(value));
    };
    if !value.is_finite() {
        return Ok(Value::Float(value));
    }
    Ok(Value::Float(java_double_to_long_as_double(value)))
}

fn instant_math_sign(This(target): This<Value>, value: Value) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "sign")?;
    match value {
        Value::Int(value) => Ok(Value::Int(value.signum())),
        Value::UInt(value) => Ok(Value::UInt(u64::from(value != 0))),
        Value::Float(value) if value.is_nan() => Ok(Value::Float(value)),
        Value::Float(value) => Ok(Value::Float(if value == 0.0 {
            0.0
        } else if value > 0.0 {
            1.0
        } else {
            -1.0
        })),
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

fn instant_math_sqrt(This(target): This<Value>, value: Value) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "sqrt")?;
    let value = match value {
        Value::Int(value) => i64_as_f64(value),
        Value::UInt(value) => u64_as_f64(value),
        Value::Float(value) => value,
        value => return Err(ExecutionError::unsupported_target_type(value)),
    };
    Ok(Value::Float(value.sqrt()))
}

fn instant_math_is_inf(This(target): This<Value>, value: Value) -> Result<bool, ExecutionError> {
    require_math_namespace(target, "isInf")?;
    let Value::Float(value) = value else {
        return Err(ExecutionError::unsupported_target_type(value));
    };
    Ok(value.is_infinite())
}

fn instant_math_is_nan(This(target): This<Value>, value: Value) -> Result<bool, ExecutionError> {
    require_math_namespace(target, "isNaN")?;
    let Value::Float(value) = value else {
        return Err(ExecutionError::unsupported_target_type(value));
    };
    Ok(value.is_nan())
}

fn instant_math_is_finite(This(target): This<Value>, value: Value) -> Result<bool, ExecutionError> {
    require_math_namespace(target, "isFinite")?;
    let Value::Float(value) = value else {
        return Err(ExecutionError::unsupported_target_type(value));
    };
    Ok(value.is_finite())
}

#[derive(Clone, Copy)]
enum MathBitOperation {
    And,
    Or,
    Xor,
}

fn instant_math_bit_and(
    This(target): This<Value>,
    left: Value,
    right: Value,
) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "bitAnd")?;
    math_bit_binary(left, right, "bitAnd", MathBitOperation::And)
}

fn instant_math_bit_or(
    This(target): This<Value>,
    left: Value,
    right: Value,
) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "bitOr")?;
    math_bit_binary(left, right, "bitOr", MathBitOperation::Or)
}

fn instant_math_bit_xor(
    This(target): This<Value>,
    left: Value,
    right: Value,
) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "bitXor")?;
    math_bit_binary(left, right, "bitXor", MathBitOperation::Xor)
}

fn math_bit_binary(
    left: Value,
    right: Value,
    function: &str,
    operation: MathBitOperation,
) -> Result<Value, ExecutionError> {
    match (left, right) {
        (Value::Int(left), Value::Int(right)) => Ok(Value::Int(match operation {
            MathBitOperation::And => left & right,
            MathBitOperation::Or => left | right,
            MathBitOperation::Xor => left ^ right,
        })),
        (Value::UInt(left), Value::UInt(right)) => Ok(Value::UInt(match operation {
            MathBitOperation::And => left & right,
            MathBitOperation::Or => left | right,
            MathBitOperation::Xor => left ^ right,
        })),
        (left, right) => Err(ExecutionError::function_error(
            function,
            format!("operands must both be int or both be uint, got {left:?} and {right:?}"),
        )),
    }
}

fn instant_math_bit_not(This(target): This<Value>, value: Value) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "bitNot")?;
    match value {
        Value::Int(value) => Ok(Value::Int(!value)),
        Value::UInt(value) => Ok(Value::UInt(!value)),
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

fn instant_math_bit_shift_left(
    This(target): This<Value>,
    value: Value,
    shift: Value,
) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "bitShiftLeft")?;
    let offset = math_shift_offset(&shift, "bitShiftLeft")?;
    drop(shift);
    math_bit_shift(value, offset, "bitShiftLeft", false)
}

fn instant_math_bit_shift_right(
    This(target): This<Value>,
    value: Value,
    shift: Value,
) -> Result<Value, ExecutionError> {
    require_math_namespace(target, "bitShiftRight")?;
    let offset = math_shift_offset(&shift, "bitShiftRight")?;
    drop(shift);
    math_bit_shift(value, offset, "bitShiftRight", true)
}

fn math_shift_offset(shift: &Value, function: &str) -> Result<i64, ExecutionError> {
    match shift {
        Value::Int(shift) => Ok(*shift),
        _ => Err(ExecutionError::function_error(
            function,
            "shift offset must be an int",
        )),
    }
}

fn math_bit_shift(
    value: Value,
    shift: i64,
    function: &str,
    right: bool,
) -> Result<Value, ExecutionError> {
    if shift < 0 {
        return Err(ExecutionError::function_error(
            function,
            format!("negative offset: {shift}"),
        ));
    }
    if shift > 63 {
        return match value {
            Value::Int(_) => Ok(Value::Int(0)),
            Value::UInt(_) => Ok(Value::UInt(0)),
            value => Err(ExecutionError::unsupported_target_type(value)),
        };
    }
    let shift = u32::try_from(shift).expect("0..=63 shift fits u32");
    match value {
        Value::Int(value) if right => Ok(Value::Int(
            value.cast_unsigned().wrapping_shr(shift).cast_signed(),
        )),
        Value::Int(value) => Ok(Value::Int(value.wrapping_shl(shift))),
        Value::UInt(value) if right => Ok(Value::UInt(value.wrapping_shr(shift))),
        Value::UInt(value) => Ok(Value::UInt(value.wrapping_shl(shift))),
        value => Err(ExecutionError::unsupported_target_type(value)),
    }
}

fn parse_timestamp(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).ok().or_else(|| {
        value
            .parse::<i64>()
            .ok()
            .and_then(DateTime::from_timestamp_millis)
            .map(|timestamp| timestamp.fixed_offset())
    })
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PermissionError {
    #[error("invalid CEL expression: {0}")]
    Parse(String),
    #[error("undeclared reference to '{0}'")]
    UndeclaredVariable(String),
    #[error("undefined field '{0}' on request")]
    InvalidRequestField(String),
    #[error("auth.ref arg must start with `$user.`")]
    InvalidAuthRef,
    #[error("invalid CEL types: {0}")]
    StaticType(String),
    #[error("could not serialize CEL binding: {0}")]
    Binding(String),
    #[error("could not evaluate permission rule: {0}")]
    Evaluation(String),
    #[error("permission rule cannot be converted to an InstaQL constraint: {0}")]
    RuleWhereUnsupported(String),
    #[error("invalid return type from the CEL rule; expected boolean")]
    InvalidReturnType,
    #[error("permission rule requires {0} unloaded references")]
    MissingReferences(usize),
    #[error("reference loader omitted {0} requested references")]
    UnresolvedReferences(usize),
    #[error("bind should only contain a given variable name once")]
    DuplicateBind,
    #[error("the binds have a cyclic dependency: {0:?}")]
    CyclicBind(Vec<String>),
    #[error("invalid permission rules: {0}")]
    InvalidRules(String),
    #[error("`{0}` is not a valid rate limit config; define it in `$rateLimits`")]
    UnknownRateLimit(String),
    #[error("permission rule uses rate limits but no rate limit consumer is configured")]
    RateLimitUnavailable,
    #[error("rate limit store failed: {0}")]
    RateLimitStore(String),
    #[error(
        "permission rate limit exceeded until {retry_at} with {remaining_tokens} tokens remaining"
    )]
    RateLimited {
        retry_at: DateTime<Utc>,
        remaining_tokens: u64,
    },
    #[error("permission filtering expected entity objects")]
    InvalidEntityData,
    #[error("permission query expected {expected} result nodes, got {actual}")]
    QueryResultCardinality { expected: usize, actual: usize },
    #[error("permission query encountered an entity result with no triples")]
    EmptyEntityResult,
    #[error("permission query references unknown attribute {0}")]
    UnknownAttribute(Uuid),
    #[error("permission query did not preload entity {0:?}")]
    MissingEntityData(PermissionEntityKey),
    #[error("permission transaction contains an unresolved lookup")]
    UnresolvedTransactionLookup,
    #[error("permission transaction step is missing its entity type")]
    MissingTransactionEntityType,
    #[error("permission transaction reference attribute has incomplete reverse metadata")]
    IncompleteRelationship,
    #[error("permission transaction expected a UUID reference, got {0}")]
    InvalidTransactionReference(JsonValue),
    #[error("expected permissioned link value to be a UUID, got {value}")]
    InvalidTransactionLinkValue {
        value: JsonValue,
        tx_step: JsonValue,
    },
    #[error("invalid permissioned transaction lookup: {message}")]
    InvalidTransactionLookupType {
        data_type: String,
        input: JsonValue,
        message: String,
        tx_step: JsonValue,
        include_tx_step_in_error: bool,
    },
    #[error("$users is a system entity and cannot be created directly")]
    DirectUserCreate,
    #[error("permission denied for {etype} {scope} {action} on {entity_id:?}")]
    PermissionDenied {
        etype: String,
        scope: &'static str,
        action: &'static str,
        entity_id: Option<Uuid>,
    },
    #[error("permission data loader failed: {0}")]
    Loader(String),
    #[error("you cannot set field rules for id; use {0} -> allow -> view instead")]
    FieldRuleForId(String),
}

impl PermissionError {
    #[must_use]
    pub fn rate_limited(retry_after: std::time::Duration, remaining_tokens: u64) -> Self {
        let retry_at = Utc::now()
            .checked_add_signed(
                ChronoDuration::from_std(retry_after).unwrap_or(ChronoDuration::MAX),
            )
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        Self::RateLimited {
            retry_at,
            remaining_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use instant_datalog::{
        DatalogResult, DatalogTriple, DatalogValue, PageCursorResult, PageResultInfo,
    };
    use instant_instaql::{
        Attribute, AttributeValueType, Cardinality, CompiledNode, CompiledQuery, ExecutedEntity,
        ExecutedNode, FormOptions,
    };
    use instant_transact::{RuleParamsStep, TripleOptions};
    use serde_json::json;

    use super::*;

    fn permission_attribute(id: u128, etype: &str, label: &str) -> Attribute {
        Attribute {
            id: Uuid::from_u128(id),
            wire: None,
            etype: etype.to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: label == "id",
            setting_unique: false,
            indexed: label == "id",
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    fn permission_triple(
        entity_id: Uuid,
        attribute: &Attribute,
        value: JsonValue,
    ) -> DatalogTriple {
        DatalogTriple {
            entity_id,
            attr_id: attribute.id,
            value: DatalogValue::Json(value),
            created_at: i64::try_from(entity_id.as_u128()).unwrap_or_default(),
        }
    }

    fn permission_node(key: &str, etype: &str, children: Vec<CompiledNode>) -> CompiledNode {
        CompiledNode {
            key: key.to_owned(),
            etype: etype.to_owned(),
            level: 0,
            options: FormOptions::default(),
            selector_clauses: Vec::new(),
            missing: false,
            page: None,
            aggregate: None,
            fetch_attr_ids: Vec::new(),
            relationship: None,
            children,
        }
    }

    #[test]
    fn standard_macros_match_authoritative_cel_cases() {
        let cases = [
            ("has({'name': 'Alice'}.name)", json!(true)),
            ("[1, 2, 3].all(x, x > 0)", json!(true)),
            ("[1, 2, 3].exists(x, x > 2)", json!(true)),
            ("[1, 2, 3].exists_one(x, x > 2)", json!(true)),
            ("[1, 2, 3].map(x, x * 2)", json!([2, 4, 6])),
            ("[1, 2, 3, 4].filter(x, x % 2 == 0)", json!([2, 4])),
        ];
        for (source, expected) in cases {
            let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert_eq!(
                program.evaluate(&EvaluationBindings::default()).unwrap(),
                expected
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keeps the source-pinned extension corpus together.
    fn cel_java_string_extensions_match_the_pinned_runtime() {
        const ORACLE: &str =
            "cel-java@v0.11.1 extensions/src/main/java/dev/cel/extensions/CelStringExtensions.java";
        assert!(ORACLE.contains("v0.11.1"));
        let cases = [
            ("'a😁b'.charAt(1)", json!("😁")),
            ("'hello'.charAt(5)", json!("")),
            ("'hello mellow'.indexOf('ello')", json!(1)),
            ("'hello mellow'.indexOf('ello', 2)", json!(7)),
            ("''.indexOf('', -5)", json!(-5)),
            ("'hello'.indexOf('', 99)", json!(99)),
            ("['hello', 'mellow'].join()", json!("hellomellow")),
            ("['hello', 'mellow'].join(' ')", json!("hello mellow")),
            (
                "[dyn(1), dyn(true), dyn(1.0)].join('-')",
                json!("1-true-1.0"),
            ),
            (
                "[dyn([1, 2]), dyn({'b': 2, 'a': 1})].join('|')",
                json!("[1, 2]|{a=1, b=2}"),
            ),
            (
                "[dyn(1e20), dyn(1e-4), dyn(-0.0)].join('|')",
                json!("1.0E20|1.0E-4|-0.0"),
            ),
            (
                "[dyn(1e7), dyn(9999999.0), dyn(0.001)].join('|')",
                json!("1.0E7|9999999.0|0.001"),
            ),
            ("'hello mellow'.lastIndexOf('ello')", json!(7)),
            ("'hello mellow'.lastIndexOf('ello', 6)", json!(1)),
            ("''.lastIndexOf('', -5)", json!(-5)),
            (
                "'hello'.lastIndexOf('', 2147483648)",
                json!(2_147_483_648_i64),
            ),
            ("'TacoCÆt Xii'.lowerAscii()", json!("tacocÆt xii")),
            ("'hello hello'.replace('he', 'we')", json!("wello wello")),
            ("'hello hello'.replace('he', 'we', 1)", json!("wello hello")),
            ("'abc'.replace('', '-', -1)", json!("-a-b-c-")),
            ("''.replace('', 'x')", json!("x")),
            ("'abc'.replace('a', 'x', 0)", json!("abc")),
            ("'a😁b'.split('')", json!(["a", "😁", "b"])),
            ("'a😁b'.split('', 2)", json!(["a", "😁b"])),
            ("'hello hello'.split(' ', 0)", json!([])),
            ("'hello hello'.split(' ', 1)", json!(["hello hello"])),
            ("'hello hello'.split(' ', -1)", json!(["hello", "hello"])),
            ("' a  '.split(' ')", json!(["", "a", "", ""])),
            (
                "'hello hello hello'.split(' ', 2)",
                json!(["hello", "hello hello"]),
            ),
            ("'ta😁cat'.substring(2)", json!("😁cat")),
            ("'ta😁cat'.substring(2, 4)", json!("😁c")),
            ("'😁'.substring(1)", json!("")),
            ("'\u{0085}\u{00a0}trim\u{3000}'.trim()", json!("trim")),
            (
                "'\u{200b}trim\u{200b}'.trim()",
                json!("\u{200b}trim\u{200b}"),
            ),
            ("'TacoCÆt Xii'.upperAscii()", json!("TACOCÆT XII")),
        ];
        for (source, expected) in cases {
            let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert_eq!(
                program.evaluate(&EvaluationBindings::default()).unwrap(),
                expected,
                "{source}"
            );
        }

        for source in [
            "'hello'.charAt(-1)",
            "'hello'.indexOf('e', 5)",
            "'hello'.lastIndexOf('e', -1)",
            "'hello'.substring(3, 2)",
            "''.split(' ', -1)",
        ] {
            let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert!(
                matches!(
                    program.evaluate(&EvaluationBindings::default()),
                    Err(PermissionError::Evaluation(_))
                ),
                "{source}"
            );
        }

        for source in [
            "charAt('hello', 1)",
            "'hello'.charAt()",
            "'hello'.charAt(1u)",
            "'hello'.charAt(1.0 + 1.0)",
            "'hello'.substring(0, 1u + 1u)",
            "'hello'.split(2)",
            "['x', 'y'].join(2)",
            "[1, 2].join()",
        ] {
            assert!(
                matches!(
                    RuleProgram::compile(PermissionAction::View, source),
                    Err(PermissionError::StaticType(_))
                ),
                "{source}"
            );
        }

        for (source, expected) in [
            (
                "'hello'.charAt(-1)",
                "charAt failure: Index out of range: -1",
            ),
            (
                "'hello'.indexOf('e', 5)",
                "indexOf failure: Offset out of range: 5",
            ),
            (
                "'hello'.substring(3, 2)",
                "substring failure: Range [3, 2) out of bounds",
            ),
            (
                "'hello'.substring(2147483648)",
                "substring failure: Index must not exceed the int32 range: 2147483648",
            ),
            (
                "'hello'.substring(0, 2147483648)",
                "substring failure: Indices must not exceed the int32 range: [0, 2147483648)",
            ),
            (
                "'hello'.charAt(2147483648)",
                "charAt failure: Index must not exceed the int32 range: 2147483648",
            ),
            (
                "'hello'.indexOf('', 2147483648)",
                "indexOf failure: Offset must not exceed the int32 range: 2147483648",
            ),
            (
                "'hello'.lastIndexOf('e', 2147483648)",
                "lastIndexOf failure: Offset must not exceed the int32 range: 2147483648",
            ),
            (
                "'hello'.replace('l', 'x', 2147483648)",
                "replace failure: Index must not exceed the int32 range: 2147483648",
            ),
            (
                "'hello'.split('l', 2147483648)",
                "split failure: Limit must not exceed the int32 range: 2147483648",
            ),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap()
                    .evaluate(&EvaluationBindings::default()),
                Err(PermissionError::Evaluation(expected.to_owned())),
                "{source}"
            );
        }
    }

    #[test]
    fn preserves_false_and_missing_map_fields() {
        let bindings = EvaluationBindings {
            data: json!({"isFavorite": false}),
            ..EvaluationBindings::default()
        };
        assert!(
            !RuleProgram::compile(PermissionAction::View, "data.isFavorite")
                .unwrap()
                .evaluate_bool(&bindings)
                .unwrap()
        );
        assert!(
            RuleProgram::compile(PermissionAction::View, "data.missing == null")
                .unwrap()
                .evaluate_bool(&bindings)
                .unwrap()
        );
    }

    #[test]
    fn request_ip_matches_the_resolved_client_address() {
        let program =
            RuleProgram::compile(PermissionAction::View, "request.ip == '203.0.113.9'").unwrap();
        assert!(
            program
                .evaluate_bool(&EvaluationBindings {
                    request: RequestBinding {
                        ip: Some("203.0.113.9".to_owned()),
                        ..RequestBinding::default()
                    },
                    ..EvaluationBindings::default()
                })
                .unwrap()
        );
        assert!(
            !program
                .evaluate_bool(&EvaluationBindings {
                    request: RequestBinding {
                        ip: Some("203.0.113.10".to_owned()),
                        ..RequestBinding::default()
                    },
                    ..EvaluationBindings::default()
                })
                .unwrap()
        );
    }

    #[test]
    fn presence_preserves_missing_null_nested_and_selected_path_semantics() {
        let absent = EvaluationBindings::default();
        let request_presence = RuleProgram::compile(
            PermissionAction::View,
            "has(request.ip) || has(request.origin)",
        )
        .unwrap();
        assert!(!request_presence.evaluate_bool(&absent).unwrap());
        assert!(
            request_presence
                .evaluate_bool(&EvaluationBindings {
                    request: RequestBinding {
                        ip: Some("127.0.0.1".to_owned()),
                        ..RequestBinding::default()
                    },
                    ..EvaluationBindings::default()
                })
                .unwrap()
        );

        let auth_presence = RuleProgram::compile(
            PermissionAction::View,
            "has(auth.nullable) && !has(auth.missing)",
        )
        .unwrap();
        assert!(
            auth_presence
                .evaluate_bool(&EvaluationBindings {
                    auth: json!({"nullable": null}),
                    ..EvaluationBindings::default()
                })
                .unwrap()
        );

        let selected_same_path = RuleProgram::compile(
            PermissionAction::View,
            "has(auth.email) ? auth.email == 'x@example.com' : true",
        )
        .unwrap();
        assert!(selected_same_path.evaluate_bool(&absent).unwrap());
        let selected_in_bind = RuleProgram::compile_with_binds(
            PermissionAction::View,
            "emailMatches",
            vec![(
                "emailMatches".to_owned(),
                "has(auth.email) ? auth.email == 'x@example.com' : true".to_owned(),
            )],
        )
        .unwrap();
        assert!(selected_in_bind.evaluate_bool(&absent).unwrap());

        let nested =
            RuleProgram::compile(PermissionAction::View, "has(auth.profile.email)").unwrap();
        for auth in [json!({}), json!({"profile": null})] {
            assert!(matches!(
                nested.evaluate_bool(&EvaluationBindings {
                    auth,
                    ..EvaluationBindings::default()
                }),
                Err(PermissionError::Evaluation(_))
            ));
        }
        assert!(
            !nested
                .evaluate_bool(&EvaluationBindings {
                    auth: json!({"profile": {}}),
                    ..EvaluationBindings::default()
                })
                .unwrap()
        );
        assert!(
            nested
                .evaluate_bool(&EvaluationBindings {
                    auth: json!({"profile": {"email": null}}),
                    ..EvaluationBindings::default()
                })
                .unwrap()
        );

        let data_presence = RuleProgram::compile(
            PermissionAction::View,
            "has(data.flag) && (data.flag == false || data.flag == null)",
        )
        .unwrap();
        for data in [json!({"flag": false}), json!({"flag": null})] {
            assert!(
                data_presence
                    .evaluate_bool(&EvaluationBindings {
                        data,
                        ..EvaluationBindings::default()
                    })
                    .unwrap()
            );
        }
        assert!(
            !data_presence
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );
    }

    #[test]
    fn enforces_action_and_request_binding_contracts_at_compile_time() {
        assert_eq!(
            RuleProgram::compile(PermissionAction::View, "newData.isFavorite").unwrap_err(),
            PermissionError::UndeclaredVariable("newData".to_owned())
        );
        assert_eq!(
            RuleProgram::compile(PermissionAction::Delete, "newData.isFavorite").unwrap_err(),
            PermissionError::UndeclaredVariable("newData".to_owned())
        );
        assert_eq!(
            RuleProgram::compile(PermissionAction::Update, "request.hello").unwrap_err(),
            PermissionError::InvalidRequestField("hello".to_owned())
        );
        RuleProgram::compile(
            PermissionAction::Update,
            "request.modifiedFields.all(f, f in ['likes'])",
        )
        .unwrap();
        assert_eq!(
            RuleProgram::compile(PermissionAction::View, "auth.ref('$users.id') == [auth.id]")
                .unwrap_err(),
            PermissionError::InvalidAuthRef
        );
        assert_eq!(
            RuleProgram::compile(PermissionAction::View, "auth.ref(data.path) == []").unwrap_err(),
            PermissionError::InvalidAuthRef
        );
        assert!(matches!(
            RuleProgram::compile(PermissionAction::View, "!10"),
            Err(PermissionError::StaticType(_))
        ));
        assert!(matches!(
            RuleProgram::compile(PermissionAction::Update, "request.modifiedFields == 1"),
            Err(PermissionError::StaticType(_))
        ));
        assert!(matches!(
            RuleProgram::compile(PermissionAction::Update, "1 in request.modifiedFields"),
            Err(PermissionError::StaticType(_))
        ));
        for source in ["1 < 'one'", "timestamp('2026-01-01T00:00:00Z') < 1"] {
            assert!(
                matches!(
                    RuleProgram::compile(PermissionAction::View, source),
                    Err(PermissionError::StaticType(_))
                ),
                "expected incompatible ordering types to fail: {source}"
            );
        }
        for source in [
            "1 < 2",
            "'one' < 'two'",
            "false < true",
            "b'a' < b'b'",
            "timestamp('2026-01-01T00:00:00Z') < timestamp('2026-01-02T00:00:00Z')",
        ] {
            RuleProgram::compile(PermissionAction::View, source).unwrap();
        }
        RuleProgram::compile(
            PermissionAction::Update,
            "request.modifiedFields == ['likes'] && 'likes' in request.modifiedFields",
        )
        .unwrap();
    }

    #[test]
    fn equality_operators_reject_incompatible_static_types() {
        for source in [
            "request.time == 0",
            "request.time != 0",
            "request.ip == true",
            "'one' != false",
            "true == 1",
            "['one'] != [1]",
        ] {
            assert!(
                matches!(
                    RuleProgram::compile(PermissionAction::View, source),
                    Err(PermissionError::StaticType(_))
                ),
                "expected incompatible equality types to fail: {source}",
            );
        }

        for source in [
            "request.time == timestamp(0)",
            "request.ip != '127.0.0.1'",
            "true == false",
            "1 == 1u",
            "1.0 != 2",
            "['one'] == ['two']",
            "[1] != [2]",
            "null == null",
            "data.value != 0",
            "auth.id == request.ip",
        ] {
            RuleProgram::compile(PermissionAction::View, source).unwrap();
        }
    }

    #[test]
    fn conditionals_and_comprehension_predicates_require_reference_types() {
        for source in [
            "1 ? true : false",
            "true ? true : 1",
            "request.modifiedFields.all(field, field)",
            "[1, 2].exists(value, value)",
            "[1, 2].exists_one(value, value)",
            "[1, 2].filter(value, value).size() > 0",
            "[1, 2].map(value, value, value).size() > 0",
        ] {
            assert!(
                matches!(
                    RuleProgram::compile(PermissionAction::Update, source),
                    Err(PermissionError::StaticType(_))
                ),
                "expected invalid conditional or comprehension types to fail: {source}",
            );
        }

        for source in [
            "true ? true : false",
            "request.modifiedFields.all(field, field in ['title'])",
            "[1, 2].exists(value, value > 1)",
            "[1, 2].exists_one(value, value == 1)",
            "[1, 2].filter(value, value > 1).size() == 1",
            "[1, 2].map(value, value > 1, value).size() == 1",
        ] {
            RuleProgram::compile(PermissionAction::Update, source).unwrap();
        }

        for (source, expected) in [
            ("[].all(value, value)", true),
            ("[].exists(value, value)", false),
            ("[].exists_one(value, value)", false),
            ("[].filter(value, value).size() == 0", true),
            ("[].map(value, value, value).size() == 0", true),
        ] {
            let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert_eq!(
                program
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap(),
                expected,
                "unexpected empty list(dyn) comprehension result: {source}",
            );
        }
    }

    #[test]
    fn structured_collection_and_macro_types_match_cel_java_overloads() {
        // Pinned oracle: cel-java 0.11.1 checker declarations for list/map
        // indexing, homogeneous list concatenation, comprehensions, logical
        // operators, arithmetic, comparisons, and conditionals.
        for (source, overload) in [
            ("{'count': 1}.count + 'x' == 2", "_+_"),
            ("{'count': 1}['count'] + 'x' == 2", "_+_"),
            ("[{'count': 1}][0].count + 'x' == 2", "_+_"),
            ("[1, 2].map(value, value + 1)[0] + 'x' == 2", "_+_"),
            ("[1, 2].filter(value, value > 1)[0] + 'x' == 2", "_+_"),
            ("[1] + ['one'] == []", "_+_"),
            ("[1] == ['one']", "_==_"),
            ("{'one': 1} == {'one': 'one'}", "_==_"),
            ("[1].all(value, value + 1)", "_&&_"),
            ("[1].map(value, value, value).size() == 1", "_?_:_"),
            ("true && 1", "_&&_"),
            ("1 < 'one'", "_<_"),
            ("true ? 1 : 'one'", "_?_:_"),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source).unwrap_err(),
                PermissionError::StaticType(format!("found no matching overload for '{overload}'")),
                "unexpected cel-java checker result for {source}",
            );
        }

        for source in [
            "{'count': 1}.count + 1 == 2",
            "{'count': 1}['count'] + 1 == 2",
            "[{'count': 1}][0].count == 1",
            "[1, 2].map(value, value + 1)[0] == 2",
            "[1, 2].filter(value, value > 1)[0] == 2",
            "{'one': 1}.all(key, key == 'one')",
            "[1] + [2] == [1, 2]",
            "[1].map(value, value > 0, {'value': value})[0].value == 1",
        ] {
            let program = RuleProgram::compile(PermissionAction::View, source)
                .unwrap_or_else(|error| panic!("expected {source} to compile: {error}"));
            assert!(
                program
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap_or_else(|error| panic!("expected {source} to evaluate: {error}")),
                "unexpected false result for {source}",
            );
        }
    }

    #[test]
    fn ruleset_production_path_preserves_structured_macro_types() {
        assert_eq!(
            RuleProgram::compile_with_binds(
                PermissionAction::View,
                "numbers[0] + 'invalid' == 2",
                vec![(
                    "numbers".to_owned(),
                    "[1, 2].map(value, value + 1)".to_owned(),
                )],
            )
            .unwrap_err(),
            PermissionError::StaticType("found no matching overload for '_+_'".to_owned()),
        );

        let bound = RuleProgram::compile_with_binds(
            PermissionAction::View,
            "numbers[0] == 2",
            vec![(
                "numbers".to_owned(),
                "[1, 2].map(value, value + 1)".to_owned(),
            )],
        )
        .unwrap();
        assert!(bound.evaluate_bool(&EvaluationBindings::default()).unwrap());

        let invalid = RuleSet::new(json!({
            "posts": {"allow": {"view":
                "[1, 2].map(value, value + 1)[0] + 'x' == 2"
            }}
        }));
        assert_eq!(
            invalid
                .program("posts", PermissionAction::View)
                .unwrap_err(),
            PermissionError::StaticType("found no matching overload for '_+_'".to_owned()),
        );

        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view":
                "[1, 2].map(value, value + 1)[0] == 2 && {'role': 'admin'}.role == data.role"
            }}
        }));
        let mut refs = RefCache::new();
        assert_eq!(
            rules
                .filter_view_entities(
                    "posts",
                    &[
                        json!({"id": "one", "role": "admin"}),
                        json!({"id": "two", "role": "member"})
                    ],
                    &EvaluationBindings::default(),
                    &mut refs,
                    |_| Ok(RefCache::new()),
                )
                .unwrap(),
            vec![json!({"id": "one", "role": "admin"})],
        );
    }

    #[test]
    fn arithmetic_operators_reject_missing_static_overloads() {
        assert_eq!(
            RuleProgram::compile(PermissionAction::View, "5.5 % 2.0 == 1.5").unwrap_err(),
            PermissionError::StaticType("found no matching overload for '_%_'".to_owned()),
        );
        for source in [
            "true ? true : 1 + false",
            "1 - 'one' == 0",
            "true * 1 == 1",
            "'one' / 'two' == 1",
            "null % 2 == 0",
            "request.time + request.time == request.time",
            "-true == false",
        ] {
            assert!(
                matches!(
                    RuleProgram::compile(PermissionAction::View, source),
                    Err(PermissionError::StaticType(_))
                ),
                "expected invalid arithmetic overload to fail: {source}",
            );
        }

        for source in [
            "1 + 2 == 3",
            "'one' + 'two' == 'onetwo'",
            "b'one' + b'two' == b'onetwo'",
            "[1] + [2] == [1, 2]",
            "[] + [1] == [1]",
            "request.time - timestamp(0) >= duration('0s')",
            "timestamp(0) + duration('1s') > timestamp(0)",
            "-1 < 0",
            "data.value + 1 == 2",
        ] {
            RuleProgram::compile(PermissionAction::View, source).unwrap_or_else(|error| {
                panic!("expected valid arithmetic overload {source}: {error}")
            });
        }

        let invalid = RuleSet::new(json!({
            "posts": {"allow": {"view": "5.5 % 2.0 == 1.5"}}
        }));
        assert_eq!(
            invalid
                .program("posts", PermissionAction::View)
                .unwrap_err(),
            PermissionError::StaticType("found no matching overload for '_%_'".to_owned()),
        );
    }

    #[test]
    fn boolean_and_bytes_ordering_match_standard_cel_comparers() {
        for source in [
            "false < true",
            "false <= false",
            "true > false",
            "true >= true",
            "b'a' < b'b'",
            "b'a' <= b'a'",
            "b'b' > b'a'",
            "b'b' >= b'b'",
        ] {
            assert!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap_or_else(|error| panic!("expected {source} to compile: {error}"))
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap_or_else(|error| panic!("expected {source} to evaluate: {error}")),
                "unexpected false result for {source}",
            );
        }

        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view": "false < true && b'a' < b'b' && data.visible"}}
        }));
        let mut refs = RefCache::new();
        assert_eq!(
            rules
                .filter_view_entities(
                    "posts",
                    &[
                        json!({"id": "one", "visible": true}),
                        json!({"id": "two", "visible": false}),
                    ],
                    &EvaluationBindings::default(),
                    &mut refs,
                    |_| Ok(RefCache::new()),
                )
                .unwrap(),
            vec![json!({"id": "one", "visible": true})],
        );
    }

    #[test]
    fn collection_operators_match_cel_java_static_overloads() {
        // Pinned oracle: cel-java 0.11.1 StandardDeclarations collection overloads.
        // Its checker reports `found no matching overload for ...` for these
        // statically known list/map key, index, membership, and size mismatches.
        for (source, function) in [
            ("1 in ['one']", "@in"),
            ("'one' in [1]", "@in"),
            ("1 in [1u]", "@in"),
            ("'one' in 'not-a-list'", "@in"),
            ("1 in {'one': true}", "@in"),
            ("['one'][true] == 'one'", "_[_]"),
            ("{'one': 1}[1] == 1", "_[_]"),
            ("1[0] == 1", "_[_]"),
            ("1.size() == 1", "size"),
            ("size(1) == 1", "size"),
            ("size([1], [2]) == 1", "size"),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source).unwrap_err(),
                PermissionError::StaticType(format!("found no matching overload for '{function}'")),
                "unexpected cel-java overload result for {source}",
            );
        }

        // Index result propagation is part of the same checker contract: the
        // selected element/value retains its statically known CEL type.
        for source in ["['one'][0] + 1 == 1", "{'one': 1}['one'] + 'x' == 1"] {
            assert!(
                matches!(
                    RuleProgram::compile(PermissionAction::View, source),
                    Err(PermissionError::StaticType(_))
                ),
                "expected an invalid nested overload for {source}",
            );
        }
    }

    #[test]
    fn collection_operators_evaluate_with_cel_java_semantics() {
        for source in [
            "'one' in ['one']",
            "1 in [1]",
            "'one' in {'one': true}",
            "['one'][0] == 'one'",
            "{'one': 1}['one'] == 1",
            "size('hello') == 5",
            "b'hello'.size() == 5",
            "[1, 2].size() == 2",
            "size({'one': 1}) == 1",
            "data.value in [data.value]",
            "[][0] == null ? true : false",
        ] {
            let result = RuleProgram::compile(PermissionAction::View, source)
                .unwrap_or_else(|error| panic!("expected {source} to compile: {error}"))
                .evaluate_bool(&EvaluationBindings {
                    data: json!({"value": "dynamic"}),
                    ..EvaluationBindings::default()
                });
            if source == "[][0] == null ? true : false" {
                assert!(matches!(result, Err(PermissionError::Evaluation(_))));
            } else {
                assert_eq!(result, Ok(true), "unexpected result for {source}");
            }
        }
    }

    #[test]
    fn numeric_map_keys_match_cel_java_lossless_lookup_semantics() {
        // Pinned oracle: cel-java v0.11.1 RuntimeEquality.java:62-105,193-221,
        // 226-242. Map indexing, membership, and aggregate equality first try
        // the native key and then retry only lossless numeric representations.
        for (source, expected) in [
            ("{1: 'v'} == {1u: 'v'}", true),
            ("{1u: 'v'} == {1: 'v'}", true),
            ("dyn(1) in {1u: true}", true),
            ("dyn(1u) in {1: true}", true),
            ("{1u: true}[dyn(1)]", true),
            ("{1: true}[dyn(1u)]", true),
            ("dyn(1.0) in {1: true}", true),
            ("dyn(1.0) in {1u: true}", true),
            ("{1: true}[dyn(1.0)]", true),
            ("{1u: true}[dyn(1.0)]", true),
            (
                "{9223372036854775807: 'v'} == {9223372036854775807u: 'v'}",
                true,
            ),
            (
                "dyn(9223372036854775808.0) in {9223372036854775808u: true}",
                false,
            ),
            (
                "dyn(9223372036854774784.0) in {9223372036854774784u: true}",
                true,
            ),
            ("dyn(-1) in {1u: true}", false),
            ("dyn(-1.0) in {1u: true}", false),
            ("dyn(1.5) in {1: true}", false),
            ("dyn(1.5) in {1u: true}", false),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap_or_else(|error| panic!("expected {source} to compile: {error}"))
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap_or_else(|error| panic!("expected {source} to evaluate: {error}")),
                expected,
                "unexpected lossless map-key result for {source}",
            );
        }
    }

    #[test]
    fn ruleset_post_filter_uses_lossless_numeric_map_keys() {
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view":
                "dyn(data.lookup) in {1u: true} && {1: 'value'} == {1u: 'value'}"
            }}
        }));
        let mut refs = RefCache::new();
        assert_eq!(
            rules
                .filter_view_entities(
                    "posts",
                    &[
                        json!({"id": "one", "lookup": 1}),
                        json!({"id": "two", "lookup": 2}),
                    ],
                    &EvaluationBindings::default(),
                    &mut refs,
                    |_| Ok(RefCache::new()),
                )
                .unwrap(),
            vec![json!({"id": "one", "lookup": 1})],
        );
    }

    #[test]
    fn ruleset_production_path_enforces_collection_overloads() {
        let invalid = RuleSet::new(json!({
            "posts": {"allow": {"view": "{'owner': true}[1]"}}
        }));
        assert_eq!(
            invalid
                .program("posts", PermissionAction::View)
                .unwrap_err(),
            PermissionError::StaticType("found no matching overload for '_[_]'".to_owned()),
        );

        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view":
                "['admin', 'member'][0] == data.role && 'role' in {'role': true} && size({'role': true}) == 1"
            }}
        }));
        let mut refs = RefCache::new();
        assert_eq!(
            rules
                .filter_view_entities(
                    "posts",
                    &[
                        json!({"id": "one", "role": "admin"}),
                        json!({"id": "two", "role": "member"})
                    ],
                    &EvaluationBindings::default(),
                    &mut refs,
                    |_| Ok(RefCache::new()),
                )
                .unwrap(),
            vec![json!({"id": "one", "role": "admin"})],
        );
    }

    #[test]
    fn standard_conversion_string_and_time_overloads_match_cel_java() {
        // Pinned oracle: cel-java 0.11.1 CelStandardDeclarations Conversions,
        // StringMatchers, and DateTime overload families.
        for (source, function) in [
            ("bool(1)", "bool"),
            ("bytes(true)", "bytes"),
            ("double(false)", "double"),
            ("int([])", "int"),
            ("uint({})", "uint"),
            ("string([])", "string"),
            ("duration(1)", "duration"),
            ("timestamp(1u)", "timestamp"),
            ("int(1, 2)", "int"),
            ("1.contains('1')", "contains"),
            ("'one'.startsWith(1)", "startsWith"),
            ("contains('one', 'n')", "contains"),
            ("1.matches('.*')", "matches"),
            ("matches('one', 1)", "matches"),
            ("1.getFullYear()", "getFullYear"),
            ("duration('1s').getFullYear()", "getFullYear"),
            ("timestamp(0).getMonth(1)", "getMonth"),
            ("duration('1s').getSeconds('UTC')", "getSeconds"),
            ("timestamp(0).getTime('UTC')", "getTime"),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source).unwrap_err(),
                PermissionError::StaticType(format!("found no matching overload for '{function}'")),
                "unexpected cel-java overload result for {source}",
            );
        }

        for source in [
            "int('1') + 'x' == '1x'",
            "timestamp(0).getFullYear() + 'x' == '1970x'",
            "'one'.contains('o') + 1 == 2",
        ] {
            assert!(matches!(
                RuleProgram::compile(PermissionAction::View, source),
                Err(PermissionError::StaticType(_))
            ));
        }
    }

    #[test]
    fn type_operator_and_standard_identifiers_match_cel_java() {
        const ORACLE: &str = "cel-java@v0.11.1 checker/src/main/java/dev/cel/checker/CelStandardDeclarations.java:81,329-331,1523-1533";
        assert!(ORACLE.contains("v0.11.1"));
        // The pinned declarations define type(A) -> type(A) and the standard
        // type-denotation identifiers.
        for source in [
            "type(1) == int",
            "type(1u) == uint",
            "type(true) == bool",
            "type(1.0) == double",
            "type(b'x') == bytes",
            "type('x') == string",
            "type(null) == null_type",
            "type([]) == list",
            "type({}) == map",
            "type(timestamp(0)) == timestamp",
            "type(duration('1s')) == duration",
            "type(type(1)) == type",
            "type(1) != uint",
        ] {
            assert!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap_or_else(|error| panic!("expected {source} to compile: {error}"))
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap_or_else(|error| panic!("expected {source} to evaluate: {error}")),
                "unexpected type result for {source}",
            );
        }

        for source in ["type() == type", "type(1, 2) == type"] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source).unwrap_err(),
                PermissionError::StaticType("found no matching overload for 'type'".to_owned()),
            );
        }

        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view": "type(data.rank) == int"}}
        }));
        let mut refs = RefCache::new();
        assert_eq!(
            rules
                .filter_view_entities(
                    "posts",
                    &[
                        json!({"id": "one", "rank": 1}),
                        json!({"id": "two", "rank": "1"})
                    ],
                    &EvaluationBindings::default(),
                    &mut refs,
                    |_| Ok(RefCache::new()),
                )
                .unwrap(),
            vec![json!({"id": "one", "rank": 1})],
        );
    }

    #[test]
    fn standard_conversions_and_time_helpers_use_cel_java_runtime_semantics() {
        for source in [
            "bool('true') && bool('TRUE') && bool('t') && bool('1')",
            "!bool('false') && !bool('FALSE') && !bool('f') && !bool('0')",
            "string(true) == 'true'",
            "string(1.5) == '1.5'",
            "int(timestamp('1970-01-01T00:16:40.999Z')) == 1000",
            "bytes('hello') == b'hello' && string(b'hello') == 'hello'",
            "double('1.5') == 1.5 && uint('2') == 2u && int(2u) == 2",
            "duration(duration('1.234s')).getMilliseconds() == 234",
            "duration('-1.234s').getMilliseconds() == -234",
            "string(duration('1.234s')) == '1.234s'",
            "timestamp(0).getFullYear() == 1970 && timestamp(0).getMonth() == 0",
            "timestamp('2024-02-29T23:30:45.678Z').getDayOfYear() == 59",
            "timestamp('2024-02-29T23:30:45.678Z').getDayOfMonth() == 28",
            "timestamp('2024-02-29T23:30:45.678Z').getDate() == 29",
            "timestamp('2024-02-29T23:30:45.678Z').getDayOfWeek() == 4",
            "timestamp('2024-02-29T23:30:45.678Z').getMilliseconds() == 678",
            "timestamp('2024-07-01T12:00:00Z').getHours('America/Los_Angeles') == 5",
            "timestamp('2024-07-01T12:00:00Z').getHours('-7:00') == 5",
            "'instantdb'.contains('stant') && 'instantdb'.startsWith('inst') && 'instantdb'.endsWith('db')",
            "'instantdb'.matches('^instant.*$') && matches('instantdb', '^instant.*$')",
        ] {
            assert!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap_or_else(|error| panic!("expected {source} to compile: {error}"))
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap_or_else(|error| panic!("expected {source} to evaluate: {error}")),
                "unexpected false result for {source}",
            );
        }

        for source in ["bool('yes')", "duration('1h')", "string(b'\\xff')"] {
            assert!(matches!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap()
                    .evaluate(&EvaluationBindings::default()),
                Err(PermissionError::Evaluation(_))
            ));
        }
    }

    #[test]
    fn ruleset_production_path_enforces_and_evaluates_standard_overloads() {
        let invalid = RuleSet::new(json!({
            "posts": {"allow": {"view": "timestamp(0).getFullYear(true) == 1970"}}
        }));
        assert_eq!(
            invalid
                .program("posts", PermissionAction::View)
                .unwrap_err(),
            PermissionError::StaticType("found no matching overload for 'getFullYear'".to_owned()),
        );

        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view":
                "bool(data.visible) && int(timestamp(data.created)) == 0 && data.slug.startsWith('public-')"
            }}
        }));
        let mut refs = RefCache::new();
        assert_eq!(
            rules
                .filter_view_entities(
                    "posts",
                    &[
                        json!({"id": "one", "visible": "true", "created": 0, "slug": "public-one"}),
                        json!({"id": "two", "visible": "false", "created": 0, "slug": "public-two"})
                    ],
                    &EvaluationBindings::default(),
                    &mut refs,
                    |_| Ok(RefCache::new()),
                )
                .unwrap(),
            vec![json!({"id": "one", "visible": "true", "created": 0, "slug": "public-one"})],
        );
    }

    #[test]
    fn request_time_and_modified_fields_have_jvm_types() {
        let bindings = EvaluationBindings {
            data: json!({"created": 0}),
            request: RequestBinding {
                modified_fields: vec!["likes".to_owned()],
                time: Utc.timestamp_opt(1_000, 0).unwrap(),
                ip: None,
                origin: None,
            },
            ..EvaluationBindings::default()
        };
        assert!(
            RuleProgram::compile(
                PermissionAction::Update,
                "request.time > timestamp(data.created) && 'likes' in request.modifiedFields",
            )
            .unwrap()
            .evaluate_bool(&bindings)
            .unwrap()
        );
        assert!(
            RuleProgram::compile(
                PermissionAction::View,
                "timestamp('2026-02-06T22:36:52.157181Z').getTime() > 0",
            )
            .unwrap()
            .evaluate_bool(&EvaluationBindings::default())
            .unwrap()
        );
    }

    #[test]
    fn get_time_returns_jvm_epoch_seconds() {
        let bindings = EvaluationBindings {
            request: RequestBinding {
                time: Utc.timestamp_opt(1_000, 999_000_000).unwrap(),
                ..RequestBinding::default()
            },
            ..EvaluationBindings::default()
        };
        for source in [
            "request.time.getTime() == 1000",
            "timestamp('1970-01-01T00:16:40.999Z').getTime() == 1000",
            "timestamp(1000999).getTime() == 1000",
        ] {
            assert!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap()
                    .evaluate_bool(&bindings)
                    .unwrap(),
                "expected JVM epoch-second semantics for {source}",
            );
        }
    }

    #[test]
    fn documented_recent_update_rule_uses_duration_minutes_and_math_abs() {
        let program = RuleProgram::compile(
            PermissionAction::Update,
            "math.abs((request.time - timestamp(data.updatedAt)).getMinutes()) <= 1",
        )
        .unwrap();
        let request_time = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let request = RequestBinding {
            time: request_time,
            ..RequestBinding::default()
        };

        for (offset_millis, expected) in [
            (-119_999_i64, true),
            (119_999, true),
            (-120_000, false),
            (120_000, false),
        ] {
            let bindings = EvaluationBindings {
                data: json!({
                    "updatedAt": request_time.timestamp_millis() + offset_millis,
                }),
                request: request.clone(),
                ..EvaluationBindings::default()
            };
            assert_eq!(
                program.evaluate_bool(&bindings).unwrap(),
                expected,
                "unexpected decision for updatedAt offset {offset_millis}ms",
            );
        }
    }

    #[test]
    fn math_abs_preserves_supported_numeric_types() {
        for source in [
            "math.abs(-7) == 7",
            "math.abs(7u) == 7u",
            "math.abs(-1.5) == 1.5",
        ] {
            assert!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap()
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap(),
                "expected CEL math.abs compatibility for {source}",
            );
        }
    }

    #[test]
    fn math_extensions_preserve_cel_java_static_overloads_and_result_types() {
        // Pinned oracle: cel-java@v0.11.1
        // extensions/src/main/java/dev/cel/extensions/CelMathExtensions.java.
        for (source, overload) in [
            ("math.abs('nope') == 0", "abs"),
            ("math.ceil() == 0.0", "ceil"),
            ("math.ceil(1) == 1.0", "ceil"),
            ("math.isFinite('nope')", "isFinite"),
            ("math.isFinite(1)", "isFinite"),
            ("math.bitAnd(1, 1u) == 0", "bitAnd"),
            ("math.bitNot(1.0) == 0", "bitNot"),
            ("math.bitShiftRight(1u, 1u) == 0u", "bitShiftRight"),
            ("math.greatest() == 0", "greatest"),
            ("math.least(['nope']) == 'nope'", "least"),
            ("math.unknown(1) == 1", "unknown"),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source).unwrap_err(),
                PermissionError::StaticType(format!("found no matching overload for '{overload}'")),
                "unexpected CEL math checker result for {source}",
            );
        }

        for (source, overload) in [
            ("math.abs(-1) + 'x' == '1x'", "_+_"),
            ("math.sqrt(4) + 'x' == '2x'", "_+_"),
            ("math.isFinite(1.0) + 1 == 2", "_+_"),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source).unwrap_err(),
                PermissionError::StaticType(format!("found no matching overload for '{overload}'")),
                "math result type did not propagate for {source}",
            );
        }

        RuleProgram::compile(
            PermissionAction::View,
            "math.abs(data.value) == data.expected",
        )
        .unwrap();
    }

    #[test]
    fn cel_java_math_extensions_evaluate_with_reference_semantics() {
        for source in [
            "math.greatest(1, 3u, 2.5) == 3u",
            "math.least([3, -1, 2u]) == -1",
            "math.greatest(7) == 7",
            "math.ceil(1.2) == 2.0",
            "math.floor(-1.2) == -2.0",
            // Instant pins cel-java 0.11.1, whose round implementation uses HALF_EVEN.
            "math.round(2.5) == 2.0 && math.round(3.5) == 4.0 && math.round(-2.5) == -2.0",
            "math.trunc(-1.9) == -1.0",
            "math.sign(-8) == -1 && math.sign(0u) == 0u && math.sign(-0.0) == 0.0",
            "math.sqrt(81) == 9.0 && math.sqrt(81u) == 9.0 && math.sqrt(2.25) == 1.5",
            "math.isNaN(math.sqrt(-1))",
            "math.isFinite(math.sqrt(4))",
        ] {
            assert!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap()
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap(),
                "expected cel-java math compatibility for {source}",
            );
        }
    }

    #[test]
    fn math_extrema_preserve_first_equal_type_and_exact_integer_ordering() {
        assert_eq!(
            math_extreme(
                &[Value::UInt(1), Value::Float(1.0)],
                "greatest",
                Ordering::Greater,
            )
            .unwrap(),
            Value::UInt(1),
        );
        assert_eq!(
            math_extreme(
                &[Value::Int(i64::MAX), Value::UInt(i64::MAX as u64)],
                "greatest",
                Ordering::Greater,
            )
            .unwrap(),
            Value::UInt(i64::MAX as u64),
        );
        assert_eq!(
            math_extreme(
                &[
                    Value::UInt(u64::MAX),
                    Value::Float(18_446_744_073_709_551_616.0),
                ],
                "least",
                Ordering::Less,
            )
            .unwrap(),
            Value::UInt(u64::MAX),
        );
    }

    #[test]
    fn floating_math_preserves_non_finite_and_java_conversion_edges() {
        let target = || {
            Value::Map(CelMap::from(HashMap::from([(
                "__instant_math".to_owned(),
                Value::Bool(true),
            )])))
        };
        assert!(instant_math_is_inf(This(target()), Value::Float(f64::INFINITY)).unwrap());
        assert!(matches!(
            instant_math_sign(This(target()), Value::Float(f64::NAN)).unwrap(),
            Value::Float(value) if value.is_nan()
        ));
        assert_eq!(
            instant_math_trunc(This(target()), Value::Float(f64::MAX)).unwrap(),
            Value::Float(9_223_372_036_854_775_808.0),
        );
        assert!(instant_math_round(This(target()), Value::Float(f64::MAX)).is_err());
        assert!(instant_math_is_finite(This(target()), Value::Int(1)).is_err());
    }

    #[test]
    fn math_extensions_reject_empty_or_non_numeric_extrema() {
        assert!(matches!(
            RuleProgram::compile(PermissionAction::View, "math.greatest([]) == 0")
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default()),
            Err(PermissionError::Evaluation(_))
        ));
        assert_eq!(
            RuleProgram::compile(PermissionAction::View, "math.least('nope') == 0").unwrap_err(),
            PermissionError::StaticType("found no matching overload for 'least'".to_owned()),
        );
    }

    #[test]
    fn cel_java_bitwise_math_extensions_preserve_int_and_uint_semantics() {
        for source in [
            "math.bitAnd(6, 3) == 2 && math.bitAnd(6u, 3u) == 2u",
            "math.bitOr(4, 3) == 7 && math.bitOr(4u, 3u) == 7u",
            "math.bitXor(7, 3) == 4 && math.bitXor(7u, 3u) == 4u",
            "math.bitNot(0) == -1",
            "math.bitNot(0u) == 18446744073709551615u",
            "math.bitShiftLeft(3, 2) == 12 && math.bitShiftLeft(3u, 2) == 12u",
            // cel-java applies Java's unsigned >>> operator even to signed CEL ints.
            "math.bitShiftRight(-1, 1) == 9223372036854775807",
            "math.bitShiftRight(12u, 2) == 3u",
            "math.bitShiftLeft(1, 64) == 0 && math.bitShiftRight(1u, 100) == 0u",
        ] {
            assert!(
                RuleProgram::compile(PermissionAction::View, source)
                    .unwrap()
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap(),
                "expected cel-java bitwise math compatibility for {source}",
            );
        }
    }

    #[test]
    fn bitwise_math_rejects_negative_mixed_and_non_int_shifts() {
        let source = "math.bitShiftLeft(1, -1) == 0";
        assert!(matches!(
            RuleProgram::compile(PermissionAction::View, source)
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default()),
            Err(PermissionError::Evaluation(_))
        ));
        for (source, function) in [
            ("math.bitShiftRight(1u, 1u) == 0u", "bitShiftRight"),
            ("math.bitAnd(1, 1u) == 1", "bitAnd"),
        ] {
            assert_eq!(
                RuleProgram::compile(PermissionAction::View, source).unwrap_err(),
                PermissionError::StaticType(format!("found no matching overload for '{function}'")),
            );
        }
    }

    #[test]
    fn native_string_expansion_is_bounded_and_cannot_be_suppressed() {
        let bindings = EvaluationBindings {
            data: json!({"text": "x".repeat(10_000)}),
            ..EvaluationBindings::default()
        };
        let program = RuleProgram::compile(
            PermissionAction::View,
            "data.text.replace('x', data.text).size() > 0 || true",
        )
        .unwrap();
        assert_eq!(
            program.evaluate_bool(&bindings),
            Err(PermissionError::Evaluation(
                "CEL work limit exceeded".into()
            ))
        );
    }

    #[test]
    fn oversized_bindings_fail_before_cloning() {
        let bindings = EvaluationBindings {
            data: json!({"text": "x".repeat(4 * 1024 * 1024)}),
            ..EvaluationBindings::default()
        };
        let program = RuleProgram::compile(PermissionAction::View, "true").unwrap();
        assert_eq!(
            program.evaluate_bool(&bindings),
            Err(PermissionError::Evaluation(
                "CEL binding budget exceeded".into()
            ))
        );
    }

    #[test]
    fn oversized_permission_sources_fail_before_parsing() {
        let source = format!("'{}' == ''", "x".repeat(65_536));
        assert!(matches!(
            RuleProgram::compile(PermissionAction::View, source),
            Err(PermissionError::Parse(_))
        ));
    }

    #[test]
    fn binding_budget_is_shared_across_auth_and_data_and_counts_map_keys() {
        let program = RuleProgram::compile(PermissionAction::View, "true").unwrap();
        for bindings in [
            EvaluationBindings {
                auth: json!({"profile": "x".repeat(2 * 1024 * 1024)}),
                data: json!({"body": "x".repeat(2 * 1024 * 1024)}),
                ..EvaluationBindings::default()
            },
            EvaluationBindings {
                data: json!({"x".repeat(4 * 1024 * 1024): null}),
                ..EvaluationBindings::default()
            },
        ] {
            assert_eq!(
                program.evaluate_bool(&bindings),
                Err(PermissionError::Evaluation(
                    "CEL binding budget exceeded".into()
                ))
            );
        }
    }

    #[test]
    fn deeply_nested_bindings_fail_before_recursive_normalization() {
        let mut data = json!(null);
        for _ in 0..66 {
            data = json!([data]);
        }
        let bindings = EvaluationBindings {
            data,
            ..EvaluationBindings::default()
        };
        let program = RuleProgram::compile(PermissionAction::View, "true").unwrap();
        assert_eq!(
            program.evaluate_bool(&bindings),
            Err(PermissionError::Evaluation(
                "CEL binding budget exceeded".into()
            ))
        );
    }

    #[test]
    fn reference_retries_share_one_work_budget() {
        let program = RuleProgram::compile(
            PermissionAction::View,
            "data.items.all(x, data.items.all(y, x + y >= 0)) && auth.ref('$user.id') != []",
        )
        .unwrap();
        let bindings = EvaluationBindings {
            data: json!({"items": (0..90).collect::<Vec<_>>()}),
            auth: json!({"id": Uuid::nil()}),
            ..EvaluationBindings::default()
        };
        let items = [EvaluationItem {
            program: &program,
            bindings: &bindings,
        }];
        let result = evaluate_batch(&items, &mut RefCache::new(), |requests| {
            Ok(requests
                .iter()
                .map(|r| (r.clone(), vec![json!(Uuid::nil())]))
                .collect())
        });
        assert_eq!(
            result,
            Err(PermissionError::Evaluation(
                "CEL work limit exceeded".into()
            ))
        );
    }

    #[test]
    fn nested_comprehensions_exhaust_work_limit() {
        let bindings = EvaluationBindings {
            data: json!({"id": Uuid::nil(), "items": (0..200).collect::<Vec<_>>() }),
            data_etype: Some("posts".to_owned()),
            ..EvaluationBindings::default()
        };
        for source in [
            "data.items.all(x, data.items.all(y, x + y >= 0))",
            "data.items.all(x, data.items.all(y, x + y >= 0)) || true",
            "!(data.items.all(x, data.items.all(y, x + y >= 0)) && false)",
            "data.ref('owners.id') == [] || data.items.all(x, data.items.all(y, x + y >= 0))",
            "data.items.map(x, data.items.filter(y, x + y >= 0)).size() > 0",
        ] {
            let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert_eq!(
                program.evaluate_bool(&bindings),
                Err(PermissionError::Evaluation(
                    "CEL work limit exceeded".to_owned()
                )),
                "{source}",
            );
        }
    }

    #[test]
    fn work_limit_is_shared_across_permission_binds() {
        let bindings = EvaluationBindings {
            data: json!({"items": (0..70).collect::<Vec<_>>() }),
            ..EvaluationBindings::default()
        };
        let source = "data.items.all(x, data.items.all(y, x + y >= 0))";
        assert!(
            RuleProgram::compile(PermissionAction::View, source)
                .unwrap()
                .evaluate_bool(&bindings)
                .unwrap()
        );
        let program = RuleProgram::compile_with_binds(
            PermissionAction::View,
            format!("a && b && ({source})"),
            vec![
                ("a".to_owned(), source.to_owned()),
                ("b".to_owned(), source.to_owned()),
            ],
        )
        .unwrap();
        assert_eq!(
            program.evaluate_bool(&bindings),
            Err(PermissionError::Evaluation(
                "CEL work limit exceeded".to_owned()
            ))
        );
    }

    #[test]
    fn work_limit_preserves_normal_rules_and_fresh_evaluations() {
        for source in [
            "[1, 2, 3].all(x, [1, 2, 3].exists(y, x == y))",
            "[1, 2, 3].map(x, x + 1).filter(x, x > 2) == [3, 4]",
            "[1, 2, 3].exists_one(x, x == 2)",
            "'HELLO'.lowerAscii() == 'hello' && math.abs(-2) == 2",
            "data.missing == 1 || true",
            "!(data.missing == 1 && false)",
            "true || data.items.all(x, data.items.all(y, x + y >= 0))",
        ] {
            let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
            let bindings = EvaluationBindings {
                data: json!({"items": (0..200).collect::<Vec<_>>() }),
                ..EvaluationBindings::default()
            };
            for _ in 0..2 {
                assert_eq!(program.evaluate_bool(&bindings), Ok(true), "{source}");
            }
        }
    }

    #[test]
    fn permission_decisions_must_be_boolean() {
        let program = RuleProgram::compile(PermissionAction::View, "[1, 2, 3]").unwrap();
        assert_eq!(
            program.evaluate_bool(&EvaluationBindings::default()),
            Err(PermissionError::InvalidReturnType)
        );
    }

    #[test]
    fn reference_evaluation_reports_batches_and_resumes_from_cache() {
        let user_id = Uuid::new_v4();
        let program = RuleProgram::compile(
            PermissionAction::View,
            format!("auth.ref('$user.id') == ['{user_id}']"),
        )
        .unwrap();
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        let request = RefRequest {
            entity_id: user_id.into(),
            etype: "$users".to_owned(),
            path: "id".to_owned(),
        };
        assert_eq!(
            program.advance(&bindings, &RefCache::new()).unwrap(),
            EvaluationOutcome::NeedsRefs(vec![request.clone()])
        );

        let refs = RefCache::from([(request, vec![json!(user_id)])]);
        assert_eq!(
            program.advance(&bindings, &refs).unwrap(),
            EvaluationOutcome::Value(json!(true))
        );
    }

    #[test]
    fn data_refs_preserve_non_uuid_string_and_numeric_entity_ids() {
        let program =
            RuleProgram::compile(PermissionAction::View, "data.ref('owner.id') == []").unwrap();
        for (id, expected) in [
            (
                json!("post-slug"),
                RefEntityId::String("post-slug".to_owned()),
            ),
            (json!(42), RefEntityId::Int(42)),
        ] {
            let bindings = EvaluationBindings {
                data: json!({"id": id}),
                data_etype: Some("posts".to_owned()),
                ..EvaluationBindings::default()
            };
            let request = RefRequest {
                entity_id: expected,
                etype: "posts".to_owned(),
                path: "owner.id".to_owned(),
            };
            assert_eq!(
                program.advance(&bindings, &RefCache::new()).unwrap(),
                EvaluationOutcome::NeedsRefs(vec![request.clone()])
            );
            assert_eq!(
                program
                    .advance(&bindings, &RefCache::from([(request, Vec::new())]))
                    .unwrap(),
                EvaluationOutcome::Value(json!(true))
            );
        }
    }

    #[test]
    fn reference_calls_obey_boolean_short_circuiting() {
        let user_id = Uuid::new_v4();
        let program =
            RuleProgram::compile(PermissionAction::View, "true || auth.ref('$user.id') == []")
                .unwrap();
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        assert_eq!(
            program.advance(&bindings, &RefCache::new()).unwrap(),
            EvaluationOutcome::Value(json!(true))
        );
    }

    #[test]
    fn linked_data_refs_carry_the_reverse_entity_type_and_ref_metadata_is_static() {
        let linked_id = Uuid::new_v4();
        let auth_id = Uuid::new_v4();
        let program = RuleProgram::compile(
            PermissionAction::Link,
            "linkedData.ref('owner.id') == [] && auth.ref('$user.profile.id') == []",
        )
        .unwrap();
        assert_eq!(
            program.reference_uses(),
            [
                ReferenceUse {
                    target: "auth".to_owned(),
                    path: "profile.id".to_owned(),
                },
                ReferenceUse {
                    target: "linkedData".to_owned(),
                    path: "owner.id".to_owned(),
                },
            ]
        );
        let bindings = EvaluationBindings {
            auth: json!({"id": auth_id}),
            linked_data: Some(json!({"id": linked_id})),
            linked_data_etype: Some("comments".to_owned()),
            ..EvaluationBindings::default()
        };
        assert_eq!(
            program.advance(&bindings, &RefCache::new()).unwrap(),
            EvaluationOutcome::NeedsRefs(vec![
                RefRequest {
                    entity_id: auth_id.into(),
                    etype: "$users".to_owned(),
                    path: "profile.id".to_owned(),
                },
                RefRequest {
                    entity_id: linked_id.into(),
                    etype: "comments".to_owned(),
                    path: "owner.id".to_owned(),
                },
            ])
        );
    }

    #[test]
    fn batch_evaluation_coalesces_reference_loading_and_reuses_the_cache() {
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let programs = [
            RuleProgram::compile(
                PermissionAction::View,
                format!("auth.ref('$user.id') == ['{first_id}']"),
            )
            .unwrap(),
            RuleProgram::compile(
                PermissionAction::View,
                format!("auth.ref('$user.id') == ['{second_id}']"),
            )
            .unwrap(),
        ];
        let bindings = [
            EvaluationBindings {
                auth: json!({"id": first_id}),
                ..EvaluationBindings::default()
            },
            EvaluationBindings {
                auth: json!({"id": second_id}),
                ..EvaluationBindings::default()
            },
        ];
        let items = [
            EvaluationItem {
                program: &programs[0],
                bindings: &bindings[0],
            },
            EvaluationItem {
                program: &programs[1],
                bindings: &bindings[1],
            },
        ];
        let mut refs = RefCache::new();
        let mut loader_calls = 0;
        let results = evaluate_batch(&items, &mut refs, |requests| {
            loader_calls += 1;
            assert_eq!(requests.len(), 2);
            Ok(requests
                .iter()
                .map(|request| (request.clone(), vec![request.entity_id.as_json()]))
                .collect())
        })
        .unwrap();

        assert_eq!(results, vec![true, true]);
        assert_eq!(loader_calls, 1);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            evaluate_batch(&items, &mut refs, |_| panic!("cache should satisfy refs")).unwrap(),
            vec![true, true]
        );
    }

    #[test]
    fn batch_loader_must_distinguish_empty_results_from_omissions() {
        let user_id = Uuid::new_v4();
        let program =
            RuleProgram::compile(PermissionAction::View, "auth.ref('$user.missing') == []")
                .unwrap();
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        let items = [EvaluationItem {
            program: &program,
            bindings: &bindings,
        }];

        assert_eq!(
            evaluate_batch(&items, &mut RefCache::new(), |_| Ok(RefCache::new())),
            Err(PermissionError::UnresolvedReferences(1))
        );
        assert_eq!(
            evaluate_batch(&items, &mut RefCache::new(), |requests| {
                Ok(RefCache::from([(requests[0].clone(), Vec::new())]))
            })
            .unwrap(),
            vec![true]
        );
    }

    #[tokio::test]
    async fn async_batch_loader_supports_owned_database_requests() {
        let user_id = Uuid::new_v4();
        let program = RuleProgram::compile(
            PermissionAction::View,
            "auth.ref('$user.teams.id') == ['allowed']",
        )
        .unwrap();
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        let items = [EvaluationItem {
            program: &program,
            bindings: &bindings,
        }];
        let mut refs = RefCache::new();
        let results = evaluate_batch_async(&items, &mut refs, |requests| async move {
            assert_eq!(requests.len(), 1);
            Ok(RefCache::from([(
                requests[0].clone(),
                vec![json!("allowed")],
            )]))
        })
        .await
        .unwrap();

        assert_eq!(results, vec![true]);
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn binds_are_dependency_sorted_and_unused_binds_are_ignored() {
        let program = RuleProgram::compile_with_binds(
            PermissionAction::View,
            "child",
            vec![
                ("unused".to_owned(), "missing.variable".to_owned()),
                ("child".to_owned(), "parent || false".to_owned()),
                ("parent".to_owned(), "true".to_owned()),
            ],
        )
        .unwrap();
        assert!(
            program
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );

        assert!(matches!(
            RuleProgram::compile_with_binds(
                PermissionAction::View,
                "a",
                vec![
                    ("a".to_owned(), "b".to_owned()),
                    ("b".to_owned(), "a".to_owned()),
                ],
            ),
            Err(PermissionError::CyclicBind(_))
        ));
    }

    #[test]
    fn rule_lookup_precedence_booleans_binds_and_fallbacks_match_jvm() {
        let rules = RuleSet::new(json!({
            "$default": {
                "bind": ["global", "true"],
                "allow": {"view": false, "$default": false}
            },
            "posts": {
                "bind": {"local": "global && true"},
                "allow": {"view": "local", "$default": false}
            }
        }));
        assert!(
            rules
                .program("posts", PermissionAction::View)
                .unwrap()
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );
        assert!(
            !rules
                .program("comments", PermissionAction::View)
                .unwrap()
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );

        let empty = RuleSet::default();
        assert!(
            empty
                .program("posts", PermissionAction::View)
                .unwrap()
                .is_none()
        );
        assert!(
            empty
                .program("$users", PermissionAction::Create)
                .unwrap()
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );
        assert!(
            !empty
                .program("$files", PermissionAction::View)
                .unwrap()
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );
    }

    #[test]
    fn system_namespace_fallback_permissions_match_jvm() {
        let empty = RuleSet::default();
        assert!(
            empty
                .program("$users", PermissionAction::Create)
                .unwrap()
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );
        let user_id = Uuid::new_v4().to_string();
        let other_id = Uuid::new_v4().to_string();
        let own_user = EvaluationBindings {
            auth: json!({"id": user_id}),
            data: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        for action in [PermissionAction::View, PermissionAction::Update] {
            assert!(
                empty
                    .program("$users", action)
                    .unwrap()
                    .unwrap()
                    .evaluate_bool(&own_user)
                    .unwrap()
            );
        }

        let linked_guest = EvaluationBindings {
            auth: json!({"id": user_id}),
            data: json!({"id": other_id, "linkedPrimaryUser": user_id}),
            ..EvaluationBindings::default()
        };
        for action in [PermissionAction::View, PermissionAction::Update] {
            assert!(
                empty
                    .program("$users", action)
                    .unwrap()
                    .unwrap()
                    .evaluate_bool(&linked_guest)
                    .unwrap()
            );
        }

        let denied_actions = [
            PermissionAction::Delete,
            PermissionAction::Restore,
            PermissionAction::Link,
            PermissionAction::Unlink,
            PermissionAction::Write,
        ];
        for action in denied_actions {
            assert!(
                !empty
                    .program("$users", action)
                    .unwrap()
                    .unwrap()
                    .evaluate_bool(&EvaluationBindings::default())
                    .unwrap(),
                "$users.{} must default-deny",
                action.as_str()
            );
        }

        for etype in instant_instaql::SYSTEM_ETYPES {
            if etype != "$users" {
                for action in [
                    PermissionAction::View,
                    PermissionAction::Create,
                    PermissionAction::Update,
                    PermissionAction::Delete,
                    PermissionAction::Restore,
                    PermissionAction::Link,
                    PermissionAction::Unlink,
                    PermissionAction::Write,
                ] {
                    assert!(
                        !empty
                            .program(etype, action)
                            .unwrap()
                            .unwrap()
                            .evaluate_bool(&EvaluationBindings::default())
                            .unwrap(),
                        "{etype}.{} must default-deny",
                        action.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn relationship_and_field_rules_use_specialized_paths() {
        let rules = RuleSet::new(json!({
            "$default": {"allow": {"$default": true}},
            "posts": {
                "allow": {"link": {"author": false, "$default": true}},
                "fields": {"id": true, "title": "data.published"}
            }
        }));
        assert!(
            !rules
                .relationship_program("posts", PermissionAction::Link, "author")
                .unwrap()
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );
        assert!(
            rules
                .relationship_program("posts", PermissionAction::Link, "comments")
                .unwrap()
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap()
        );
        assert!(
            rules
                .relationship_program("comments", PermissionAction::Link, "post")
                .unwrap()
                .is_none()
        );

        let bindings = EvaluationBindings {
            data: json!({"published": true}),
            ..EvaluationBindings::default()
        };
        assert!(
            rules
                .field_program("posts", "title")
                .unwrap()
                .unwrap()
                .evaluate_bool(&bindings)
                .unwrap()
        );
        assert_eq!(
            rules.field_program("posts", "id").unwrap_err(),
            PermissionError::FieldRuleForId("posts".to_owned())
        );
    }

    #[test]
    fn view_filter_applies_object_and_field_rules_in_stable_order() {
        let user_id = Uuid::new_v4();
        let rules = RuleSet::new(json!({
            "posts": {
                "allow": {"view": "data.published"},
                "fields": {"secret": "auth.id == data.ownerId"}
            }
        }));
        let entities = vec![
            json!({"id": Uuid::from_u128(1), "published": true, "ownerId": user_id, "secret": "a"}),
            json!({"id": Uuid::from_u128(2), "published": false, "ownerId": user_id, "secret": "b"}),
            json!({"id": Uuid::from_u128(3), "published": true, "ownerId": Uuid::new_v4(), "secret": "c"}),
        ];
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };

        assert_eq!(
            rules
                .filter_view_entities(
                    "posts",
                    &entities,
                    &bindings,
                    &mut RefCache::new(),
                    |_| panic!("rules contain no ref calls"),
                )
                .unwrap(),
            vec![
                entities[0].clone(),
                json!({"id": Uuid::from_u128(3), "published": true, "ownerId": entities[2]["ownerId"]}),
            ]
        );
    }

    #[test]
    fn view_filter_batches_data_refs_across_entities() {
        let user_id = Uuid::new_v4();
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view": "auth.id in data.ref('owner.id')"}}
        }));
        let entities = vec![json!({"id": first_id}), json!({"id": second_id})];
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        let mut calls = 0;
        let filtered = rules
            .filter_view_entities(
                "posts",
                &entities,
                &bindings,
                &mut RefCache::new(),
                |requests| {
                    calls += 1;
                    assert_eq!(requests.len(), 2);
                    Ok(requests
                        .iter()
                        .map(|request| {
                            (
                                request.clone(),
                                if request.entity_id == first_id.into() {
                                    vec![json!(user_id)]
                                } else {
                                    Vec::new()
                                },
                            )
                        })
                        .collect())
                },
            )
            .unwrap();

        assert_eq!(filtered, vec![entities[0].clone()]);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn async_view_filter_batches_data_refs_and_field_rules() {
        let user_id = Uuid::new_v4();
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let rules = RuleSet::new(json!({
            "posts": {
                "allow": {"view": "auth.id in data.ref('owner.id')"},
                "fields": {"secret": "data.published"}
            }
        }));
        let entities = vec![
            json!({"id": first_id, "published": false, "secret": "hidden"}),
            json!({"id": second_id, "published": true, "secret": "visible"}),
        ];
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        let mut calls = 0;
        let filtered = rules
            .filter_view_entities_async(
                "posts",
                &entities,
                &bindings,
                &mut RefCache::new(),
                |requests| {
                    calls += 1;
                    async move {
                        Ok(requests
                            .into_iter()
                            .map(|request| (request, vec![json!(user_id)]))
                            .collect())
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(
            filtered,
            vec![
                json!({"id": first_id, "published": false}),
                entities[1].clone(),
            ]
        );
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn nested_query_filter_uses_full_entities_and_prunes_denied_descendants() {
        let post_id_attr = permission_attribute(101, "posts", "id");
        let post_title_attr = permission_attribute(102, "posts", "title");
        let post_secret_attr = permission_attribute(103, "posts", "secret");
        let comment_id_attr = permission_attribute(201, "comments", "id");
        let comment_body_attr = permission_attribute(202, "comments", "body");
        let catalog = AttributeCatalog {
            attributes: vec![
                post_id_attr.clone(),
                post_title_attr.clone(),
                post_secret_attr.clone(),
                comment_id_attr.clone(),
                comment_body_attr.clone(),
            ],
        };
        let query = CompiledQuery {
            roots: vec![permission_node(
                "posts",
                "posts",
                vec![permission_node("comments", "comments", Vec::new())],
            )],
        };
        let first_post = Uuid::from_u128(1);
        let second_post = Uuid::from_u128(2);
        let comment = Uuid::from_u128(3);
        let first_selector = permission_triple(first_post, &post_id_attr, json!(first_post));
        let second_selector = permission_triple(second_post, &post_id_attr, json!(second_post));
        let comment_selector = permission_triple(comment, &comment_id_attr, json!(comment));
        let empty_comments = ExecutedNode::default();
        let results = vec![ExecutedNode {
            selector: DatalogResult {
                join_rows: vec![vec![first_selector.clone()], vec![second_selector.clone()]],
                page_info: Some(PageResultInfo {
                    start_cursor: Some(PageCursorResult {
                        entity_id: first_post,
                        attr_id: post_id_attr.id,
                        value: Some(first_selector.value.clone()),
                        created_at: first_selector.created_at,
                    }),
                    end_cursor: Some(PageCursorResult {
                        entity_id: second_post,
                        attr_id: post_id_attr.id,
                        value: Some(second_selector.value.clone()),
                        created_at: second_selector.created_at,
                    }),
                    has_next_page: true,
                    has_previous_page: false,
                }),
                ..DatalogResult::default()
            },
            aggregate: None,
            entities: vec![
                ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![
                            vec![permission_triple(
                                first_post,
                                &post_id_attr,
                                json!(first_post),
                            )],
                            vec![permission_triple(
                                first_post,
                                &post_title_attr,
                                json!("first"),
                            )],
                            vec![permission_triple(
                                first_post,
                                &post_secret_attr,
                                json!("hidden"),
                            )],
                        ],
                        ..DatalogResult::default()
                    },
                    children: vec![ExecutedNode {
                        selector: DatalogResult {
                            join_rows: vec![vec![comment_selector]],
                            ..DatalogResult::default()
                        },
                        aggregate: None,
                        entities: vec![ExecutedEntity {
                            fetch: DatalogResult {
                                join_rows: vec![
                                    vec![permission_triple(
                                        comment,
                                        &comment_id_attr,
                                        json!(comment),
                                    )],
                                    vec![permission_triple(
                                        comment,
                                        &comment_body_attr,
                                        json!("denied"),
                                    )],
                                ],
                                ..DatalogResult::default()
                            },
                            children: Vec::new(),
                        }],
                    }],
                },
                ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![
                            vec![permission_triple(
                                second_post,
                                &post_id_attr,
                                json!(second_post),
                            )],
                            vec![permission_triple(
                                second_post,
                                &post_title_attr,
                                json!("second"),
                            )],
                        ],
                        ..DatalogResult::default()
                    },
                    children: vec![empty_comments],
                },
            ],
        }];
        let rules = RuleSet::new(json!({
            "posts": {
                "allow": {"view": "data.visible"},
                "fields": {"secret": "data.showSecret"}
            },
            "comments": {"allow": {"view": "data.visible"}}
        }));
        let entity_data = HashMap::from([
            (
                PermissionEntityKey {
                    etype: "posts".to_owned(),
                    entity_id: first_post,
                },
                json!({"id": first_post, "visible": true, "showSecret": false}),
            ),
            (
                PermissionEntityKey {
                    etype: "posts".to_owned(),
                    entity_id: second_post,
                },
                json!({"id": second_post, "visible": false, "showSecret": true}),
            ),
            (
                PermissionEntityKey {
                    etype: "comments".to_owned(),
                    entity_id: comment,
                },
                json!({"id": comment, "visible": false}),
            ),
        ]);
        let expected_keys = {
            let mut keys = entity_data.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            keys
        };
        let audit = audit_executed_query(
            &rules,
            &query,
            &results,
            &catalog,
            &EvaluationBindings::default(),
            QueryPermissionOptions::default(),
            &mut RefCache::new(),
            |loads| async move {
                let mut keys = loads
                    .iter()
                    .flat_map(|load| load.entities.iter())
                    .cloned()
                    .collect::<Vec<_>>();
                keys.sort();
                assert_eq!(keys, expected_keys);
                Ok(entity_data)
            },
            |_| async { panic!("rules contain no ref calls") },
        )
        .await
        .unwrap();
        assert_eq!(audit.checks.len(), 4);
        assert!(audit.checks.iter().any(|check| {
            check.entity_id == first_post
                && check.label.as_deref() == Some("secret")
                && !check.allowed
                && check
                    .program
                    .as_ref()
                    .is_some_and(|program| program.code == "data.showSecret")
        }));
        assert!(audit.checks.iter().any(|check| {
            check.entity_id == second_post && check.label.is_none() && !check.allowed
        }));
        let filtered = audit.results;

        let root = &filtered[0];
        assert_eq!(root.entities.len(), 1);
        assert_eq!(root.selector.join_rows.len(), 1);
        assert_eq!(
            root.selector
                .page_info
                .as_ref()
                .unwrap()
                .end_cursor
                .as_ref()
                .unwrap()
                .entity_id,
            first_post
        );
        let retained = &root.entities[0];
        assert!(retained.fetch.join_rows.iter().flatten().all(|triple| {
            triple.attr_id != post_secret_attr.id && triple.entity_id == first_post
        }));
        assert!(retained.children[0].entities.is_empty());
        assert!(retained.children[0].selector.join_rows.is_empty());
    }

    #[tokio::test]
    async fn borrowed_query_preparation_matches_owned_without_mutating_shared_frames() {
        let id_attr = permission_attribute(401, "posts", "id");
        let title_attr = permission_attribute(402, "posts", "title");
        let secret_attr = permission_attribute(403, "posts", "secret");
        let catalog = AttributeCatalog {
            attributes: vec![id_attr.clone(), title_attr.clone(), secret_attr.clone()],
        };
        let query = CompiledQuery {
            roots: vec![permission_node("posts", "posts", Vec::new())],
        };
        let first_post = Uuid::from_u128(11);
        let second_post = Uuid::from_u128(12);
        let results = vec![ExecutedNode {
            selector: DatalogResult {
                join_rows: vec![
                    vec![permission_triple(first_post, &id_attr, json!(first_post))],
                    vec![permission_triple(second_post, &id_attr, json!(second_post))],
                ],
                ..DatalogResult::default()
            },
            aggregate: None,
            entities: vec![
                ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![
                            vec![permission_triple(first_post, &id_attr, json!(first_post))],
                            vec![permission_triple(first_post, &title_attr, json!("first"))],
                            vec![permission_triple(first_post, &secret_attr, json!("hidden"))],
                        ],
                        ..DatalogResult::default()
                    },
                    children: Vec::new(),
                },
                ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![
                            vec![permission_triple(second_post, &id_attr, json!(second_post))],
                            vec![permission_triple(second_post, &title_attr, json!("second"))],
                            vec![permission_triple(
                                second_post,
                                &secret_attr,
                                json!("visible-secret"),
                            )],
                        ],
                        ..DatalogResult::default()
                    },
                    children: Vec::new(),
                },
            ],
        }];
        let original_results = results.clone();
        let rules = RuleSet::new(json!({
            "posts": {
                "allow": {"view": "data.visible"},
                "fields": {"secret": "data.showSecret"}
            }
        }));
        let bindings = EvaluationBindings::default();
        let entity_data = HashMap::from([
            (
                PermissionEntityKey {
                    etype: "posts".to_owned(),
                    entity_id: first_post,
                },
                json!({"id": first_post, "visible": true, "showSecret": false}),
            ),
            (
                PermissionEntityKey {
                    etype: "posts".to_owned(),
                    entity_id: second_post,
                },
                json!({"id": second_post, "visible": false, "showSecret": true}),
            ),
        ]);
        let loader_data = entity_data.clone();
        let owned = prepare_owned_query_permissions(
            &rules,
            &query,
            results.clone(),
            &catalog,
            &bindings,
            QueryPermissionOptions::default(),
            move |_| async move { Ok(loader_data) },
        )
        .await
        .unwrap();
        let owned_filtered = owned
            .filter(&mut RefCache::new(), |_| async { Ok(RefCache::new()) })
            .await
            .unwrap();
        let borrowed = prepare_borrowed_query_permissions(
            &rules,
            &query,
            &results,
            &catalog,
            &bindings,
            &entity_data,
            QueryPermissionOptions::default(),
        )
        .unwrap();
        let borrowed_filtered = borrowed
            .filter(&mut RefCache::new(), |_| async { Ok(RefCache::new()) })
            .await
            .unwrap();

        assert_eq!(borrowed_filtered, owned_filtered);
        assert_eq!(results, original_results, "borrowed frames stay unchanged");
        assert_eq!(borrowed_filtered.len(), 1);
        assert_eq!(borrowed_filtered[0].entities.len(), 1);
        assert_eq!(borrowed_filtered[0].selector.join_rows.len(), 1);
        assert!(
            borrowed_filtered[0].entities[0]
                .fetch
                .join_rows
                .iter()
                .flatten()
                .all(|triple| triple.attr_id != secret_attr.id)
        );
    }

    #[tokio::test]
    async fn entity_items_match_owned_bindings_for_every_entity() {
        let owner = Uuid::from_u128(21);
        let other = Uuid::from_u128(22);
        let program = RuleProgram::compile(
            PermissionAction::View,
            "data.ownerId == auth.id && data.visible",
        )
        .unwrap();
        let base = EvaluationBindings {
            auth: json!({"id": owner}),
            ..EvaluationBindings::default()
        };
        let entities = vec![
            json!({"ownerId": owner, "visible": true}),
            json!({"ownerId": other, "visible": true}),
            json!({"ownerId": owner, "visible": false}),
        ];
        let owned_bindings = entities
            .iter()
            .map(|entity| {
                let mut bindings = base.clone();
                bindings.data = entity.clone();
                bindings.data_etype = Some("posts".to_owned());
                bindings
            })
            .collect::<Vec<_>>();
        let items = owned_bindings
            .iter()
            .map(|bindings| EvaluationItem {
                program: &program,
                bindings,
            })
            .collect::<Vec<_>>();
        let owned_results = evaluate_batch_async(&items, &mut RefCache::new(), |_| async {
            Ok(RefCache::new())
        })
        .await
        .unwrap();

        let entity_items = entities
            .iter()
            .map(|data| EntityEvaluationItem {
                program: &program,
                base: &base,
                data,
                etype: "posts",
            })
            .collect::<Vec<_>>();
        let entity_results =
            evaluate_batch_items_async(&entity_items, &mut RefCache::new(), |_| async {
                Ok(RefCache::new())
            })
            .await
            .unwrap();

        assert_eq!(entity_results, owned_results);
        assert_eq!(entity_results, vec![true, false, false]);
    }

    #[test]
    fn transaction_projection_matches_jvm_new_data_and_modified_fields() {
        let id_attr = permission_attribute(301, "posts", "id");
        let title_attr = permission_attribute(302, "posts", "title");
        let profile_attr = permission_attribute(303, "posts", "profile");
        let catalog = AttributeCatalog {
            attributes: vec![id_attr.clone(), title_attr.clone(), profile_attr.clone()],
        };
        let entity_id = Uuid::new_v4();
        let entity = EntityLookup::Id(entity_id);
        let steps = vec![
            TransactionStep::AddTriple(TripleStep {
                entity: entity.clone(),
                etype: Some("posts".to_owned()),
                attr_id: id_attr.id,
                reverse_etype: None,
                value: json!(entity_id),
                options: TripleOptions::default(),
            }),
            TransactionStep::AddTriple(TripleStep {
                entity: entity.clone(),
                etype: Some("posts".to_owned()),
                attr_id: title_attr.id,
                reverse_etype: None,
                value: json!("new"),
                options: TripleOptions::default(),
            }),
            TransactionStep::DeepMergeTriple(TripleStep {
                entity: entity.clone(),
                etype: Some("posts".to_owned()),
                attr_id: profile_attr.id,
                reverse_etype: None,
                value: json!({"nested": {"remove": null, "keep": 2}, "added": true}),
                options: TripleOptions::default(),
            }),
            TransactionStep::RuleParams(RuleParamsStep {
                entity: entity.clone(),
                etype: Some("posts".to_owned()),
                params: serde_json::Map::from_iter([("first".to_owned(), json!(1))]),
            }),
            TransactionStep::RuleParams(RuleParamsStep {
                entity,
                etype: Some("posts".to_owned()),
                params: serde_json::Map::from_iter([("second".to_owned(), json!(2))]),
            }),
        ];
        let key = PermissionEntityKey {
            etype: "posts".to_owned(),
            entity_id,
        };
        let original = HashMap::from([(
            key.clone(),
            json!({
                "id": entity_id,
                "title": "old",
                "profile": {"nested": {"remove": 1, "untouched": 3}}
            }),
        )]);
        let projected = project_transaction_entities(&catalog, &steps, original.clone()).unwrap();

        assert_eq!(projected.original, original);
        assert_eq!(
            projected.updated[&key],
            json!({
                "id": entity_id,
                "title": "new",
                "profile": {
                    "nested": {"untouched": 3, "keep": 2},
                    "added": true
                }
            })
        );
        assert_eq!(
            projected.rule_params[&key],
            json!({"first": 1, "second": 2})
        );
        assert_eq!(
            transaction_modified_fields(entity_id, &steps, &catalog).unwrap(),
            vec!["title".to_owned(), "profile".to_owned()]
        );
    }
}

#[cfg(test)]
mod rule_depth_tests {
    //! Manual measurement of the rule recursion depth that fits a production
    //! thread.
    //!
    //! Production parses, validates and evaluates rules on Tokio threads
    //! configured with [`PRODUCTION_STACK_SIZE`]. Each probe runs in a child
    //! process because an exhausted stack aborts the process instead of
    //! unwinding. Run both profiles:
    //!
    //! ```sh
    //! cargo test --locked -p instant-permissions --lib -- --ignored --nocapture \
    //!     measure_rule_recursion_depths
    //! cargo test --release --locked -p instant-permissions --lib -- --ignored \
    //!     --nocapture measure_rule_recursion_depths
    //! ```

    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::{EvaluationBindings, PermissionAction, RuleProgram};

    /// Stack size `instant-server` gives Tokio worker and blocking threads.
    const PRODUCTION_STACK_SIZE: usize = 16 * 1024 * 1024;

    /// Mirrors the per-expression source budget in `compile_with_binds`.
    const MAX_SOURCE_BYTES: usize = 65_536;

    const CHILD_SHAPE: &str = "RULE_DEPTH_SHAPE";
    const CHILD_DEPTH: &str = "RULE_DEPTH_VALUE";
    const CHILD_TEST: &str = "rule_depth_tests::probe_rule_recursion_depth";

    const SHAPES: [&str; 9] = [
        "parens",
        "lists",
        "maps",
        "calls",
        "comprehensions",
        "unary",
        "operators",
        "and_chain",
        "select_chain",
    ];

    fn probe_source(shape: &str, depth: usize) -> String {
        match shape {
            "parens" => format!("{}true{}", "(".repeat(depth), ")".repeat(depth)),
            "lists" => format!("{}{}", "[".repeat(depth), "]".repeat(depth)),
            "maps" => format!("{}{}{}", "{\"a\":".repeat(depth), "null", "}".repeat(depth)),
            "calls" => {
                let mut source = "'x'".to_owned();
                for level in 0..depth {
                    source = if level % 2 == 0 {
                        format!("size({source})")
                    } else {
                        format!("string({source})")
                    };
                }
                source
            }
            "comprehensions" => {
                let mut body = "true".to_owned();
                for level in (0..depth).rev() {
                    body = format!("[0].exists(x{level}, {body})");
                }
                body
            }
            "unary" => format!("{}1", "-".repeat(depth)),
            "operators" => format!("1{}", " + 1".repeat(depth)),
            "and_chain" => format!("true{}", " && true".repeat(depth)),
            "select_chain" => format!("data{}", ".field".repeat(depth)),
            _ => panic!("unknown probe shape {shape}"),
        }
    }

    fn probe_survives(executable: &Path, shape: &str, depth: usize) -> bool {
        let status = Command::new(executable)
            .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
            .env(CHILD_SHAPE, shape)
            .env(CHILD_DEPTH, depth.to_string())
            .stdout(Stdio::null())
            .status()
            .expect("run depth probe");
        match status.code() {
            Some(0) => true,
            Some(101) => panic!("probe panicked for {shape} at depth {depth}"),
            Some(_) | None => false,
        }
    }

    /// Largest depth whose source still fits the production source budget.
    fn source_depth_cap(shape: &str) -> usize {
        let mut low = 0;
        let mut high = MAX_SOURCE_BYTES;
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if probe_source(shape, middle).len() <= MAX_SOURCE_BYTES {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        low
    }

    fn largest_surviving_depth(executable: &Path, shape: &str) -> usize {
        let cap = source_depth_cap(shape);
        let mut survivor = 0;
        let mut step = 1;
        let mut failed = None;
        while survivor < cap {
            let candidate = cap.min(survivor + step);
            if probe_survives(executable, shape, candidate) {
                survivor = candidate;
                step = step.saturating_mul(2);
            } else {
                failed = Some(candidate);
                break;
            }
        }
        let Some(mut failed) = failed else {
            return cap;
        };
        while failed > survivor + 1 {
            let middle = survivor + (failed - survivor) / 2;
            if probe_survives(executable, shape, middle) {
                survivor = middle;
            } else {
                failed = middle;
            }
        }
        survivor
    }

    #[test]
    #[ignore = "manual stack measurement; run with --ignored --nocapture in debug and release"]
    fn probe_rule_recursion_depth() {
        let Ok(shape) = std::env::var(CHILD_SHAPE) else {
            return;
        };
        let depth = std::env::var(CHILD_DEPTH)
            .expect("depth environment variable")
            .parse::<usize>()
            .expect("numeric depth");
        let source = probe_source(&shape, depth);
        let worker = std::thread::Builder::new()
            .name("rule-depth-probe".to_owned())
            .stack_size(PRODUCTION_STACK_SIZE)
            .spawn(move || {
                match RuleProgram::compile_uncached_for_measurement(
                    PermissionAction::View,
                    source,
                    Vec::new(),
                ) {
                    Ok(program) => {
                        let _ = program.evaluate(&EvaluationBindings::default());
                    }
                    Err(_) => {
                        // A graceful reject (for example the CEL parser's own
                        // recursion limit) is not a stack limit, so tell the
                        // parent apart from a successful parse, compile and drop.
                        std::process::exit(2);
                    }
                }
            })
            .expect("spawn probe thread");
        if let Err(payload) = worker.join() {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    #[ignore = "manual stack measurement; run with --ignored --nocapture in debug and release"]
    fn measure_rule_recursion_depths() {
        let executable = std::env::current_exe().expect("current test executable");
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        println!("profile {profile}, stack {PRODUCTION_STACK_SIZE} bytes");
        for shape in SHAPES {
            let depth = largest_surviving_depth(&executable, shape);
            println!("{shape}: {depth}");
        }
    }
}
