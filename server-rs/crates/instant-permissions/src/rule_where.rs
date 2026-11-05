//! Symbolic CEL evaluation for translating view rules into `InstaQL` constraints.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::future::Future;

use cel::common::ast::{CallExpr, Expr, IdedExpr, LiteralValue};
use instant_datalog::DataType;
use instant_instaql::{
    AttributeCatalog, AttributeValueType, CompiledNode, CompiledQuery, ExecutedNode, Form,
    InstaqlError, QueryConstraint, WhereComparator, WhereCondition, WhereValue, coerce_forms,
    compile_query_with_constraints,
};
use serde_json::{Number, Value as JsonValue};
use uuid::Uuid;

use crate::{
    EvaluationBindings, PermissionAction, PermissionError, RateLimitConsumer, RateLimitDecision,
    RateLimitRequest, RefCache, RefRequest, RuleProgram, RuleSet, parse_timestamp,
    rate_limit_bucket_name,
};

/// A deferred query rate-limit request associated with its entity type.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleQueryRateLimit {
    pub etype: String,
    pub request: RateLimitRequest,
}

/// Permission planning output used to compile and later account for an `InstaQL` query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PermissionQueryPlan {
    pub constraints: HashMap<String, QueryConstraint>,
    /// Entity types whose rules are valid but cannot be represented by `InstaQL`.
    /// They remain protected by the normal post-query permission filter.
    pub fallback_etypes: Vec<String>,
    pub rate_limits: Vec<RuleQueryRateLimit>,
}

impl PermissionQueryPlan {
    /// Compiles the raw query with all safely extracted object-view constraints.
    ///
    /// # Errors
    ///
    /// Returns the normal `InstaQL` validation or Datalog compilation errors.
    pub fn compile(
        &self,
        query: &JsonValue,
        catalog: &AttributeCatalog,
        admin: bool,
    ) -> Result<CompiledQuery, InstaqlError> {
        compile_query_with_constraints(query, catalog, admin, &self.constraints)
    }

    /// Returns bindings for the defensive post-query permission pass without
    /// consuming rule-where rate limits a second time.
    #[must_use]
    pub fn post_filter_bindings(&self, bindings: &EvaluationBindings) -> EvaluationBindings {
        let mut bindings = bindings.clone();
        bindings
            .deferred_rate_limit_etypes
            .extend(self.rate_limits.iter().map(|planned| planned.etype.clone()));
        bindings
    }

    /// Consumes deferred query limits after the permission-checked entity count
    /// is known. Each CEL token value is multiplied by that entity type's count.
    ///
    /// # Errors
    ///
    /// Returns overflow, backend, or exhausted-bucket errors.
    pub fn consume_rate_limits(
        &self,
        entity_counts: &HashMap<String, u64>,
        consumer: &dyn RateLimitConsumer,
    ) -> Result<(), PermissionError> {
        for planned in &self.rate_limits {
            let count = entity_counts.get(&planned.etype).copied().unwrap_or(0);
            if count == 0 {
                continue;
            }
            let mut request = planned.request.clone();
            request.tokens = request.tokens.checked_mul(count).ok_or_else(|| {
                PermissionError::RateLimitStore("query rate limit token count overflow".to_owned())
            })?;
            match consumer
                .consume(&request)
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

    /// Counts unique permission-checked entity IDs by type in a filtered query
    /// result, matching the JVM `checked-eids` rate-limit accounting contract.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched query/result trees or entity nodes with no
    /// fetched triples.
    pub fn checked_entity_counts(
        &self,
        query: &CompiledQuery,
        results: &[ExecutedNode],
    ) -> Result<HashMap<String, u64>, PermissionError> {
        let mut entities = HashMap::<String, HashSet<Uuid>>::new();
        collect_checked_entities(&query.roots, results, &mut entities)?;
        entities
            .into_iter()
            .map(|(etype, ids)| {
                u64::try_from(ids.len())
                    .map(|count| (etype, count))
                    .map_err(|_| {
                        PermissionError::RateLimitStore(
                            "permission-checked entity count overflow".to_owned(),
                        )
                    })
            })
            .collect()
    }
}

fn collect_checked_entities(
    nodes: &[CompiledNode],
    results: &[ExecutedNode],
    entities: &mut HashMap<String, HashSet<Uuid>>,
) -> Result<(), PermissionError> {
    if nodes.len() != results.len() {
        return Err(PermissionError::QueryResultCardinality {
            expected: nodes.len(),
            actual: results.len(),
        });
    }
    for (node, result) in nodes.iter().zip(results) {
        for entity in &result.entities {
            let entity_id = entity
                .fetch
                .join_rows
                .iter()
                .flatten()
                .next()
                .map(|triple| triple.entity_id)
                .ok_or(PermissionError::EmptyEntityResult)?;
            entities
                .entry(node.etype.clone())
                .or_default()
                .insert(entity_id);
            collect_checked_entities(&node.children, &entity.children, entities)?;
        }
    }
    Ok(())
}

/// Permission-derived query restriction and deferred per-entity rate-limit work.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleWherePlan {
    /// `None` means the rule is concretely true and does not restrict the query.
    pub constraint: Option<QueryConstraint>,
    /// Requests are recorded with the token count for one returned entity.
    pub rate_limits: Vec<RateLimitRequest>,
}

/// One round of symbolic rule evaluation.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleWhereOutcome {
    Plan(RuleWherePlan),
    NeedsRefs(Vec<RefRequest>),
}

#[derive(Clone, Debug, PartialEq)]
enum SymbolicValue {
    Concrete(JsonValue),
    UIntLiteral,
    Timestamp(chrono::DateTime<chrono::FixedOffset>),
    DataRoot,
    DataKey(String),
    RefPath(String),
    RefPathSize(String),
    Clause(WhereCondition),
    RateLimitRoot,
    RateLimitBucket(String),
}

struct SymbolicEvaluator<'a> {
    catalog: &'a AttributeCatalog,
    etype: &'a str,
    bindings: &'a EvaluationBindings,
    refs: &'a RefCache,
    missing_refs: HashSet<RefRequest>,
    rate_limits: Vec<RateLimitRequest>,
}

impl RuleProgram {
    /// Symbolically evaluates a view rule and extracts the equivalent `InstaQL`
    /// constraint. Unsupported expressions return an explicit error so callers
    /// can safely fall back to post-query entity filtering.
    ///
    /// # Errors
    ///
    /// Returns an unsupported/type error for CEL that cannot safely be pushed
    /// down, or a rate-limit configuration error.
    pub fn extract_query_constraint(
        &self,
        etype: &str,
        catalog: &AttributeCatalog,
        bindings: &EvaluationBindings,
        refs: &RefCache,
    ) -> Result<RuleWhereOutcome, PermissionError> {
        let mut evaluator = SymbolicEvaluator {
            catalog,
            etype,
            bindings,
            refs,
            missing_refs: HashSet::new(),
            rate_limits: Vec::new(),
        };
        let mut locals = HashMap::new();
        for (name, program) in self.bind_programs.iter() {
            let value = evaluator.eval(program.expression(), &locals)?;
            locals.insert(name.clone(), value);
        }
        let value = boolean_symbol(evaluator.eval(self.program.expression(), &locals)?)?;
        if !evaluator.missing_refs.is_empty() {
            let mut requests = evaluator.missing_refs.into_iter().collect::<Vec<_>>();
            requests.sort_by(|left, right| {
                (&left.etype, &left.entity_id, &left.path).cmp(&(
                    &right.etype,
                    &right.entity_id,
                    &right.path,
                ))
            });
            return Ok(RuleWhereOutcome::NeedsRefs(requests));
        }
        let constraint = match value {
            SymbolicValue::Concrete(JsonValue::Bool(true)) => None,
            SymbolicValue::Concrete(JsonValue::Bool(false) | JsonValue::Null) => {
                Some(QueryConstraint::DenyAll)
            }
            SymbolicValue::Clause(condition) => Some(QueryConstraint::Where(vec![condition])),
            _ => {
                return Err(unsupported(
                    "view rule did not reduce to a boolean constraint",
                ));
            }
        };
        Ok(RuleWhereOutcome::Plan(RuleWherePlan {
            constraint,
            rate_limits: evaluator.rate_limits,
        }))
    }
}

impl RuleSet {
    /// Plans object-view permission pushdown for every entity type present in a
    /// raw `InstaQL` query. Reference loads are coalesced across entity types.
    /// Expressions that cannot be represented safely are listed as fallbacks;
    /// callers must retain [`crate::filter_executed_query`] after execution.
    ///
    /// # Errors
    ///
    /// Returns query coercion, rule compilation, reference-loader, and rate-limit
    /// configuration errors. Unsupported pushdown expressions are not errors.
    pub fn plan_query_permissions<F>(
        &self,
        query: &JsonValue,
        catalog: &AttributeCatalog,
        bindings: &EvaluationBindings,
        refs: &mut RefCache,
        mut load_refs: F,
    ) -> Result<PermissionQueryPlan, PermissionQueryPlanningError>
    where
        F: FnMut(&[RefRequest]) -> Result<RefCache, PermissionError>,
    {
        let programs = query_view_programs(self, query, catalog)?;
        plan_programs(programs, catalog, bindings, refs, |requests| {
            load_refs(&requests)
        })
        .map_err(PermissionQueryPlanningError::Permission)
    }

    /// Async counterpart to [`Self::plan_query_permissions`].
    ///
    /// # Errors
    ///
    /// Returns the same errors as the synchronous planner.
    pub async fn plan_query_permissions_async<F, Fut>(
        &self,
        query: &JsonValue,
        catalog: &AttributeCatalog,
        bindings: &EvaluationBindings,
        refs: &mut RefCache,
        load_refs: F,
    ) -> Result<PermissionQueryPlan, PermissionQueryPlanningError>
    where
        F: FnMut(Vec<RefRequest>) -> Fut,
        Fut: Future<Output = Result<RefCache, PermissionError>>,
    {
        let programs = query_view_programs(self, query, catalog)?;
        plan_programs_async(programs, catalog, bindings, refs, load_refs)
            .await
            .map_err(PermissionQueryPlanningError::Permission)
    }
}

/// Error from query-shape coercion or permission pushdown planning.
#[derive(Debug, thiserror::Error)]
pub enum PermissionQueryPlanningError {
    #[error(transparent)]
    Instaql(#[from] InstaqlError),
    #[error(transparent)]
    Permission(#[from] PermissionError),
}

fn query_view_programs(
    rules: &RuleSet,
    query: &JsonValue,
    catalog: &AttributeCatalog,
) -> Result<Vec<(String, Option<RuleProgram>)>, PermissionQueryPlanningError> {
    let forms = coerce_forms(query, catalog)?;
    let mut etypes = HashSet::new();
    collect_form_etypes(&forms, &mut etypes);
    let mut etypes = etypes.into_iter().collect::<Vec<_>>();
    etypes.sort();
    etypes
        .into_iter()
        .map(|etype| {
            let program = rules.program(&etype, PermissionAction::View)?;
            Ok((etype, program))
        })
        .collect()
}

fn collect_form_etypes(forms: &[Form], etypes: &mut HashSet<String>) {
    for form in forms {
        etypes.insert(form.etype.clone());
        collect_form_etypes(&form.children, etypes);
    }
}

fn plan_programs<F>(
    programs: Vec<(String, Option<RuleProgram>)>,
    catalog: &AttributeCatalog,
    bindings: &EvaluationBindings,
    refs: &mut RefCache,
    mut load_refs: F,
) -> Result<PermissionQueryPlan, PermissionError>
where
    F: FnMut(Vec<RefRequest>) -> Result<RefCache, PermissionError>,
{
    let mut pending = programs
        .into_iter()
        .filter_map(|(etype, program)| program.map(|program| (etype, program)))
        .collect::<Vec<_>>();
    let mut plan = PermissionQueryPlan::default();
    while !pending.is_empty() {
        let mut rerun = Vec::new();
        let mut missing = HashSet::new();
        for (etype, program) in pending {
            match program.extract_query_constraint(&etype, catalog, bindings, refs) {
                Ok(RuleWhereOutcome::Plan(result)) => {
                    if let Some(constraint) = result.constraint {
                        plan.constraints.insert(etype.clone(), constraint);
                    }
                    plan.rate_limits
                        .extend(
                            result
                                .rate_limits
                                .into_iter()
                                .map(|request| RuleQueryRateLimit {
                                    etype: etype.clone(),
                                    request,
                                }),
                        );
                }
                Ok(RuleWhereOutcome::NeedsRefs(requests)) => {
                    missing.extend(requests);
                    rerun.push((etype, program));
                }
                Err(PermissionError::RuleWhereUnsupported(_)) => {
                    plan.fallback_etypes.push(etype);
                }
                Err(error) => return Err(error),
            }
        }
        if rerun.is_empty() {
            break;
        }
        let mut requests = missing.into_iter().collect::<Vec<_>>();
        sort_requests(&mut requests);
        refs.extend(load_refs(requests.clone())?);
        let unresolved = requests
            .iter()
            .filter(|request| !refs.contains_key(*request))
            .count();
        if unresolved != 0 {
            return Err(PermissionError::UnresolvedReferences(unresolved));
        }
        pending = rerun;
    }
    plan.fallback_etypes.sort();
    Ok(plan)
}

async fn plan_programs_async<F, Fut>(
    programs: Vec<(String, Option<RuleProgram>)>,
    catalog: &AttributeCatalog,
    bindings: &EvaluationBindings,
    refs: &mut RefCache,
    mut load_refs: F,
) -> Result<PermissionQueryPlan, PermissionError>
where
    F: FnMut(Vec<RefRequest>) -> Fut,
    Fut: Future<Output = Result<RefCache, PermissionError>>,
{
    let mut pending = programs
        .into_iter()
        .filter_map(|(etype, program)| program.map(|program| (etype, program)))
        .collect::<Vec<_>>();
    let mut plan = PermissionQueryPlan::default();
    while !pending.is_empty() {
        let mut rerun = Vec::new();
        let mut missing = HashSet::new();
        for (etype, program) in pending {
            match program.extract_query_constraint(&etype, catalog, bindings, refs) {
                Ok(RuleWhereOutcome::Plan(result)) => {
                    if let Some(constraint) = result.constraint {
                        plan.constraints.insert(etype.clone(), constraint);
                    }
                    plan.rate_limits
                        .extend(
                            result
                                .rate_limits
                                .into_iter()
                                .map(|request| RuleQueryRateLimit {
                                    etype: etype.clone(),
                                    request,
                                }),
                        );
                }
                Ok(RuleWhereOutcome::NeedsRefs(requests)) => {
                    missing.extend(requests);
                    rerun.push((etype, program));
                }
                Err(PermissionError::RuleWhereUnsupported(_)) => {
                    plan.fallback_etypes.push(etype);
                }
                Err(error) => return Err(error),
            }
        }
        if rerun.is_empty() {
            break;
        }
        let mut requests = missing.into_iter().collect::<Vec<_>>();
        sort_requests(&mut requests);
        refs.extend(load_refs(requests.clone()).await?);
        let unresolved = requests
            .iter()
            .filter(|request| !refs.contains_key(*request))
            .count();
        if unresolved != 0 {
            return Err(PermissionError::UnresolvedReferences(unresolved));
        }
        pending = rerun;
    }
    plan.fallback_etypes.sort();
    Ok(plan)
}

fn sort_requests(requests: &mut [RefRequest]) {
    requests.sort_by(|left, right| {
        (&left.etype, &left.entity_id, &left.path).cmp(&(
            &right.etype,
            &right.entity_id,
            &right.path,
        ))
    });
}

impl SymbolicEvaluator<'_> {
    fn eval(
        &mut self,
        expression: &IdedExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        match &expression.expr {
            Expr::Ident(name) => self.ident(name, locals),
            Expr::Literal(value) => literal(value),
            Expr::List(list) => list
                .elements
                .iter()
                .map(|value| self.eval_concrete(value, locals))
                .collect::<Result<Vec<_>, _>>()
                .map(JsonValue::Array)
                .map(SymbolicValue::Concrete),
            Expr::Select(select) if select.test => {
                let target = self.eval(&select.operand, locals)?;
                Self::presence(target, &select.field)
            }
            Expr::Select(select) => {
                let target = self.eval(&select.operand, locals)?;
                self.select(target, &select.field)
            }
            Expr::Call(call) => self.call(call, locals),
            Expr::Map(_) | Expr::Struct(_) | Expr::Comprehension(_) | Expr::Unspecified => {
                Err(unsupported("unsupported CEL expression in query rule"))
            }
        }
    }

    fn ident(
        &self,
        name: &str,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        if let Some(value) = locals.get(name) {
            return Ok(value.clone());
        }
        match name {
            "data" => Ok(SymbolicValue::DataRoot),
            "auth" => Ok(SymbolicValue::Concrete(crate::normalize_json(
                self.bindings.auth.clone(),
            ))),
            "ruleParams" => Ok(SymbolicValue::Concrete(crate::normalize_json(
                self.bindings.rule_params.clone(),
            ))),
            "rateLimit" => Ok(SymbolicValue::RateLimitRoot),
            "request" => Ok(SymbolicValue::Concrete(request_json(
                &self.bindings.request,
            ))),
            _ => Err(unsupported(format!("unsupported CEL identifier `{name}`"))),
        }
    }

    fn select(&self, target: SymbolicValue, field: &str) -> Result<SymbolicValue, PermissionError> {
        match target {
            SymbolicValue::DataRoot => {
                self.catalog.forward(self.etype, field).ok_or_else(|| {
                    unsupported(format!("unknown field `{}.{field}`", self.etype))
                })?;
                Ok(SymbolicValue::DataKey(field.to_owned()))
            }
            SymbolicValue::Concrete(JsonValue::Object(object)) => Ok(SymbolicValue::Concrete(
                object.get(field).cloned().unwrap_or(JsonValue::Null),
            )),
            SymbolicValue::Concrete(JsonValue::Null) => {
                Ok(SymbolicValue::Concrete(JsonValue::Null))
            }
            SymbolicValue::RateLimitRoot => Ok(SymbolicValue::RateLimitBucket(field.to_owned())),
            _ => Err(unsupported(format!(
                "cannot select field `{field}` from symbolic value"
            ))),
        }
    }

    fn presence(target: SymbolicValue, field: &str) -> Result<SymbolicValue, PermissionError> {
        match target {
            SymbolicValue::Concrete(JsonValue::Object(object)) => {
                Ok(concrete_bool(object.contains_key(field)))
            }
            SymbolicValue::DataRoot => Err(unsupported(
                "data-field presence requires full CEL evaluation",
            )),
            SymbolicValue::Concrete(JsonValue::Null) => Err(unsupported(
                "presence on a missing or null parent requires full CEL evaluation",
            )),
            _ => Err(unsupported(format!(
                "cannot test field `{field}` presence on symbolic value"
            ))),
        }
    }

    fn call(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        match call.func_name.as_str() {
            "_&&_" | "_||_" => self.logical(call, locals),
            "!_" => negate(self.one_arg(call, locals)?),
            "_?_:_" => self.conditional(call, locals),
            "_==_" | "_!=_" => self.equality(call, locals),
            "_>_" | "_>=_" | "_<_" | "_<=_" => self.comparison(call, locals),
            "@in" => self.membership(call, locals),
            "ref" => self.reference(call, locals),
            "size" => self.size(call, locals),
            "timestamp" | "_instant_timestamp" => self.timestamp(call, locals),
            "startsWith" | "endsWith" | "contains" => self.string_match(call, locals),
            "_[_]" => self.index(call, locals),
            "limit" => self.rate_limit(call, locals),
            name => Err(unsupported(format!(
                "CEL function `{name}` cannot be converted to an InstaQL where clause"
            ))),
        }
    }

    fn one_arg(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let [argument] = call.args.as_slice() else {
            return Err(unsupported("expected one CEL argument"));
        };
        self.eval(argument, locals)
    }

    fn two_args(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<(SymbolicValue, SymbolicValue), PermissionError> {
        let [left, right] = call.args.as_slice() else {
            return Err(unsupported("expected two CEL arguments"));
        };
        Ok((self.eval(left, locals)?, self.eval(right, locals)?))
    }

    fn logical(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let [left, right] = call.args.as_slice() else {
            return Err(unsupported("expected two CEL arguments"));
        };
        let is_and = call.func_name == "_&&_";
        let left = boolean_symbol(self.eval(left, locals)?)?;
        if matches!(
            (&left, is_and),
            (SymbolicValue::Concrete(JsonValue::Bool(false)), true)
                | (SymbolicValue::Concrete(JsonValue::Bool(true)), false)
        ) {
            return Ok(left);
        }
        let right = self.eval(right, locals)?;
        combine_logical(left, right, is_and)
    }

    fn conditional(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let [condition, when_true, when_false] = call.args.as_slice() else {
            return Err(unsupported("conditional requires three arguments"));
        };
        match boolean_symbol(self.eval(condition, locals)?)? {
            SymbolicValue::Concrete(JsonValue::Bool(true)) => self.eval(when_true, locals),
            SymbolicValue::Concrete(JsonValue::Bool(false)) => self.eval(when_false, locals),
            _ => Err(unsupported(
                "conditional contains a symbolic condition that cannot be pushed down safely",
            )),
        }
    }

    fn equality(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let (left, right) = self.two_args(call, locals)?;
        equality(left, right, call.func_name == "_==_")
    }

    fn comparison(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let (left, right) = self.two_args(call, locals)?;
        let operation = match call.func_name.as_str() {
            "_>_" => WhereComparator::GreaterThan,
            "_>=_" => WhereComparator::GreaterThanOrEqual,
            "_<_" => WhereComparator::LessThan,
            "_<=_" => WhereComparator::LessThanOrEqual,
            _ => unreachable!(),
        };
        match (left, right) {
            (SymbolicValue::Timestamp(left), SymbolicValue::Timestamp(right)) => {
                compare_timestamps(&left, &right, operation).map(concrete_bool)
            }
            (SymbolicValue::DataKey(field), SymbolicValue::Timestamp(value)) => {
                self.comparator_clause(&field, operation, timestamp_json(value))
            }
            (SymbolicValue::Timestamp(value), SymbolicValue::DataKey(field)) => {
                self.comparator_clause(&field, reverse_comparator(operation), timestamp_json(value))
            }
            (SymbolicValue::DataKey(field), SymbolicValue::Concrete(value)) => {
                self.comparator_clause(&field, operation, value)
            }
            (SymbolicValue::Concrete(value), SymbolicValue::DataKey(field)) => {
                self.comparator_clause(&field, reverse_comparator(operation), value)
            }
            (SymbolicValue::Concrete(left), SymbolicValue::Concrete(right)) => {
                compare_concrete(&left, &right, operation).map(concrete_bool)
            }
            _ => Err(unsupported(
                "comparison contains unsupported symbolic values",
            )),
        }
    }

    fn comparator_clause(
        &self,
        field: &str,
        operation: WhereComparator,
        value: JsonValue,
    ) -> Result<SymbolicValue, PermissionError> {
        let attribute = self
            .catalog
            .forward(self.etype, field)
            .ok_or_else(|| unsupported(format!("unknown field `{}.{field}`", self.etype)))?;
        if !attribute.indexed || attribute.indexing || attribute.checking_data_type {
            return Err(unsupported(format!(
                "comparison field `{}.{field}` is not indexed and ready",
                self.etype
            )));
        }
        let Some(data_type) = attribute.checked_data_type else {
            return Err(unsupported(format!(
                "comparison field `{}.{field}` has no checked data type",
                self.etype
            )));
        };
        if !matches_checked_type(&value, data_type) {
            return Err(unsupported(format!(
                "comparison value does not match the type of `{}.{field}`",
                self.etype
            )));
        }
        Ok(condition(
            field,
            WhereValue::Comparator { operation, value },
        ))
    }

    fn membership(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let (item, collection) = self.two_args(call, locals)?;
        match (item, collection) {
            (SymbolicValue::DataKey(field), SymbolicValue::Concrete(JsonValue::Array(values))) => {
                if values.iter().all(queryable_where_scalar) {
                    Ok(condition(&field, WhereValue::In(values)))
                } else {
                    Err(unsupported(
                        "membership values must be non-null CEL scalars",
                    ))
                }
            }
            (SymbolicValue::Concrete(JsonValue::Null), SymbolicValue::RefPath(_)) => {
                Ok(concrete_bool(false))
            }
            (SymbolicValue::Concrete(value), SymbolicValue::RefPath(path))
                if !value.is_array() && !value.is_object() =>
            {
                Ok(condition(&path, WhereValue::Scalar(value)))
            }
            (SymbolicValue::Concrete(value), SymbolicValue::Concrete(JsonValue::Array(values))) => {
                Ok(concrete_bool(
                    values
                        .iter()
                        .any(|candidate| java_json_equal(&value, candidate))
                        || (value.is_number()
                            && values
                                .iter()
                                .any(|candidate| cel_concrete_equal(&value, candidate))),
                ))
            }
            _ => Err(unsupported(
                "membership contains unsupported symbolic values",
            )),
        }
    }

    fn reference(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let Some(target) = &call.target else {
            return Err(unsupported("ref requires a target"));
        };
        let [path] = call.args.as_slice() else {
            return Err(unsupported("ref requires one path"));
        };
        let path = self.eval_concrete(path, locals)?;
        let path = path
            .as_str()
            .ok_or_else(|| unsupported("ref path must be a string"))?;
        match self.eval(target, locals)? {
            SymbolicValue::DataRoot => {
                self.validate_ref_path(path)?;
                Ok(SymbolicValue::RefPath(path.to_owned()))
            }
            SymbolicValue::Concrete(JsonValue::Object(object)) => {
                let Some(entity_id) = object
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .and_then(|value| value.parse::<Uuid>().ok())
                else {
                    return Ok(SymbolicValue::Concrete(JsonValue::Array(Vec::new())));
                };
                let path = path.strip_prefix("$user.").unwrap_or(path).to_owned();
                let request = RefRequest {
                    entity_id: entity_id.into(),
                    etype: "$users".to_owned(),
                    path,
                };
                if let Some(values) = self.refs.get(&request) {
                    Ok(SymbolicValue::Concrete(JsonValue::Array(values.clone())))
                } else {
                    self.missing_refs.insert(request);
                    Ok(SymbolicValue::Concrete(JsonValue::Array(Vec::new())))
                }
            }
            _ => Err(unsupported("ref target must be data or auth")),
        }
    }

    fn validate_ref_path(&self, path: &str) -> Result<(), PermissionError> {
        let segments = path.split('.').collect::<Vec<_>>();
        if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
            return Err(unsupported("ref path is empty"));
        }
        let mut etype = self.etype;
        for (index, segment) in segments.iter().enumerate() {
            let attribute = self
                .catalog
                .forward(etype, segment)
                .or_else(|| self.catalog.reverse(etype, segment))
                .ok_or_else(|| unsupported(format!("unknown ref path `{path}`")))?;
            if index + 1 < segments.len() {
                if attribute.value_type != AttributeValueType::Ref {
                    return Err(unsupported(format!("non-reference segment in `{path}`")));
                }
                etype = self
                    .catalog
                    .link_etype(etype, segment)
                    .ok_or_else(|| unsupported(format!("incomplete ref path `{path}`")))?;
            }
        }
        Ok(())
    }

    fn size(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        match self.one_arg(call, locals)? {
            SymbolicValue::RefPath(path) => {
                let segments = path.split('.').collect::<Vec<_>>();
                if segments.len() == 2 && segments[1] == "id" {
                    Ok(SymbolicValue::RefPathSize(segments[0].to_owned()))
                } else {
                    Err(unsupported("size only supports data.ref('relation.id')"))
                }
            }
            SymbolicValue::Concrete(JsonValue::Array(values)) => Ok(SymbolicValue::Concrete(
                JsonValue::Number(Number::from(values.len())),
            )),
            _ => Err(unsupported("size target is unsupported")),
        }
    }

    fn timestamp(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        match self.one_arg(call, locals)? {
            value @ SymbolicValue::DataKey(_) => Ok(value),
            SymbolicValue::Concrete(JsonValue::String(value)) => parse_timestamp(&value)
                .map(SymbolicValue::Timestamp)
                .ok_or_else(|| unsupported("invalid timestamp string")),
            SymbolicValue::Concrete(JsonValue::Number(value)) => value
                .as_i64()
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|timestamp| SymbolicValue::Timestamp(timestamp.fixed_offset()))
                .ok_or_else(|| unsupported("invalid timestamp milliseconds")),
            _ => Err(unsupported("timestamp target is unsupported")),
        }
    }

    fn string_match(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let Some(target) = &call.target else {
            return Err(unsupported("string predicate requires a target"));
        };
        let [needle] = call.args.as_slice() else {
            return Err(unsupported("string predicate requires one argument"));
        };
        let target = self.eval(target, locals)?;
        let needle = self.eval_concrete(needle, locals)?;
        let needle = needle
            .as_str()
            .ok_or_else(|| unsupported("string predicate argument must be a string"))?;
        match target {
            SymbolicValue::DataKey(field) => {
                let attribute = self.catalog.forward(self.etype, &field).ok_or_else(|| {
                    unsupported(format!("unknown field `{}.{field}`", self.etype))
                })?;
                if !attribute.indexed || attribute.checked_data_type != Some(DataType::String) {
                    return Err(unsupported(format!(
                        "string predicate field `{}.{field}` is not an indexed checked string",
                        self.etype
                    )));
                }
                let pattern = match call.func_name.as_str() {
                    "startsWith" => format!("{needle}%"),
                    "endsWith" => format!("%{needle}"),
                    // Preserve the current JVM implementation's `%s` suffix.
                    "contains" => format!("%{needle}%s"),
                    _ => unreachable!(),
                };
                Ok(condition(
                    &field,
                    WhereValue::Comparator {
                        operation: WhereComparator::Like,
                        value: JsonValue::String(pattern),
                    },
                ))
            }
            SymbolicValue::Concrete(JsonValue::String(value)) => {
                Ok(concrete_bool(match call.func_name.as_str() {
                    "startsWith" => value.starts_with(needle),
                    "endsWith" => value.ends_with(needle),
                    "contains" => value.contains(needle),
                    _ => unreachable!(),
                }))
            }
            _ => Err(unsupported("string predicate target is unsupported")),
        }
    }

    fn index(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let (target, index) = self.two_args(call, locals)?;
        match (target, index) {
            (SymbolicValue::RateLimitRoot, SymbolicValue::Concrete(JsonValue::String(name))) => {
                Ok(SymbolicValue::RateLimitBucket(name))
            }
            (SymbolicValue::Concrete(JsonValue::Array(values)), SymbolicValue::Concrete(index)) => {
                let index = index
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| unsupported("list index must be a non-negative integer"))?;
                Ok(SymbolicValue::Concrete(
                    values.get(index).cloned().unwrap_or(JsonValue::Null),
                ))
            }
            (SymbolicValue::Concrete(JsonValue::Object(values)), SymbolicValue::Concrete(key)) => {
                let key = key
                    .as_str()
                    .ok_or_else(|| unsupported("map key must be a string"))?;
                Ok(SymbolicValue::Concrete(
                    values.get(key).cloned().unwrap_or(JsonValue::Null),
                ))
            }
            _ => Err(unsupported("index target is unsupported")),
        }
    }

    fn rate_limit(
        &mut self,
        call: &CallExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<SymbolicValue, PermissionError> {
        let bucket_name = call
            .target
            .as_deref()
            .and_then(rate_limit_bucket_name)
            .map(str::to_owned)
            .or_else(|| {
                call.target
                    .as_deref()
                    .and_then(|target| self.eval(target, locals).ok())
                    .and_then(|value| match value {
                        SymbolicValue::RateLimitBucket(name) => Some(name),
                        _ => None,
                    })
            })
            .ok_or_else(|| unsupported("rate limit call has no named bucket"))?;
        let runtime = self
            .bindings
            .rate_limit_runtime
            .as_ref()
            .ok_or(PermissionError::RateLimitUnavailable)?;
        let config = runtime
            .configs
            .get(&bucket_name)
            .cloned()
            .ok_or_else(|| PermissionError::UnknownRateLimit(bucket_name.clone()))?;
        if call.args.is_empty() || call.args.len() > 2 {
            return Err(unsupported("limit accepts a key and optional token count"));
        }
        let bucket_key = self.eval_concrete(&call.args[0], locals)?;
        let tokens = match call.args.get(1) {
            None => 1,
            Some(expression) => self
                .eval_concrete(expression, locals)?
                .as_u64()
                .filter(|tokens| *tokens > 0)
                .ok_or_else(|| unsupported("rate limit tokens must be a positive integer"))?,
        };
        self.rate_limits.push(RateLimitRequest {
            app_id: runtime.app_id,
            bucket_name,
            config,
            bucket_key,
            tokens,
        });
        Ok(concrete_bool(true))
    }

    fn eval_concrete(
        &mut self,
        expression: &IdedExpr,
        locals: &HashMap<String, SymbolicValue>,
    ) -> Result<JsonValue, PermissionError> {
        match self.eval(expression, locals)? {
            SymbolicValue::Concrete(value) => Ok(value),
            SymbolicValue::Timestamp(value) => Ok(JsonValue::String(value.to_rfc3339())),
            SymbolicValue::UIntLiteral => Err(unsupported(
                "uint literals require type-preserving post-query evaluation",
            )),
            _ => Err(unsupported("expected a concrete CEL value")),
        }
    }
}

fn literal(value: &LiteralValue) -> Result<SymbolicValue, PermissionError> {
    Ok(match value {
        LiteralValue::Boolean(value) => SymbolicValue::Concrete(JsonValue::Bool(**value)),
        LiteralValue::Double(value) => Number::from_f64(**value)
            .map(JsonValue::Number)
            .map(SymbolicValue::Concrete)
            .ok_or_else(|| unsupported("non-finite number literal"))?,
        LiteralValue::Int(value) => {
            SymbolicValue::Concrete(JsonValue::Number(Number::from(**value)))
        }
        LiteralValue::Null => SymbolicValue::Concrete(JsonValue::Null),
        LiteralValue::String(value) => {
            SymbolicValue::Concrete(JsonValue::String(value.inner().to_owned()))
        }
        LiteralValue::UInt(_) => SymbolicValue::UIntLiteral,
        LiteralValue::Bytes(_) => return Err(unsupported("bytes literals are not queryable")),
    })
}

fn request_json(request: &crate::RequestBinding) -> JsonValue {
    let mut object = serde_json::Map::from_iter([
        (
            "modifiedFields".to_owned(),
            serde_json::json!(request.modified_fields),
        ),
        (
            "time".to_owned(),
            JsonValue::String(request.time.to_rfc3339()),
        ),
    ]);
    if let Some(ip) = &request.ip {
        object.insert("ip".to_owned(), JsonValue::String(ip.clone()));
    }
    if let Some(origin) = &request.origin {
        object.insert("origin".to_owned(), JsonValue::String(origin.clone()));
    }
    JsonValue::Object(object)
}

fn equality(
    left: SymbolicValue,
    right: SymbolicValue,
    equal: bool,
) -> Result<SymbolicValue, PermissionError> {
    match (left, right) {
        (SymbolicValue::Timestamp(left), SymbolicValue::Timestamp(right)) => {
            Ok(concrete_bool((left == right) == equal))
        }
        (SymbolicValue::DataKey(field), SymbolicValue::Timestamp(value))
        | (SymbolicValue::Timestamp(value), SymbolicValue::DataKey(field)) => equality(
            SymbolicValue::DataKey(field),
            SymbolicValue::Concrete(timestamp_json(value)),
            equal,
        ),
        (SymbolicValue::DataKey(field), SymbolicValue::Concrete(value))
        | (SymbolicValue::Concrete(value), SymbolicValue::DataKey(field)) => {
            if value.is_array() || value.is_object() {
                return Err(unsupported(
                    "data fields can only be compared with scalar or null values",
                ));
            }
            let where_value = if equal {
                if value.is_null() {
                    WhereValue::IsNull(true)
                } else {
                    WhereValue::Scalar(value)
                }
            } else if value.is_null() {
                WhereValue::IsNull(false)
            } else {
                WhereValue::Not(value)
            };
            Ok(condition(&field, where_value))
        }
        (SymbolicValue::RefPath(path), SymbolicValue::Concrete(JsonValue::Array(values)))
        | (SymbolicValue::Concrete(JsonValue::Array(values)), SymbolicValue::RefPath(path))
            if values.is_empty() =>
        {
            let relation = relation_from_id_path(&path)?;
            Ok(condition(&relation, WhereValue::IsNull(equal)))
        }
        (SymbolicValue::RefPathSize(relation), SymbolicValue::Concrete(value))
        | (SymbolicValue::Concrete(value), SymbolicValue::RefPathSize(relation))
            if value.as_u64() == Some(0) =>
        {
            Ok(condition(&relation, WhereValue::IsNull(equal)))
        }
        (SymbolicValue::Concrete(left), SymbolicValue::Concrete(right)) => {
            Ok(concrete_bool(cel_concrete_equal(&left, &right) == equal))
        }
        _ => Err(unsupported("equality contains unsupported symbolic values")),
    }
}

fn combine_logical(
    left: SymbolicValue,
    right: SymbolicValue,
    is_and: bool,
) -> Result<SymbolicValue, PermissionError> {
    let left = boolean_symbol(left)?;
    let right = boolean_symbol(right)?;
    match (&left, &right, is_and) {
        (SymbolicValue::Concrete(JsonValue::Bool(false)), _, true)
        | (_, SymbolicValue::Concrete(JsonValue::Bool(false)), true) => Ok(concrete_bool(false)),
        (SymbolicValue::Concrete(JsonValue::Bool(true)), _, false)
        | (_, SymbolicValue::Concrete(JsonValue::Bool(true)), false) => Ok(concrete_bool(true)),
        (SymbolicValue::Concrete(JsonValue::Bool(true)), _, true)
        | (SymbolicValue::Concrete(JsonValue::Bool(false)), _, false) => Ok(right),
        (_, SymbolicValue::Concrete(JsonValue::Bool(true)), true)
        | (_, SymbolicValue::Concrete(JsonValue::Bool(false)), false) => Ok(left),
        (SymbolicValue::Clause(left), SymbolicValue::Clause(right), true) => Ok(
            SymbolicValue::Clause(and_conditions(left.clone(), right.clone())),
        ),
        (SymbolicValue::Clause(left), SymbolicValue::Clause(right), false) => Ok(
            SymbolicValue::Clause(or_conditions(left.clone(), right.clone())),
        ),
        _ => Err(unsupported("logical operator requires boolean values")),
    }
}

fn boolean_symbol(value: SymbolicValue) -> Result<SymbolicValue, PermissionError> {
    match value {
        SymbolicValue::DataKey(field) => {
            Ok(condition(&field, WhereValue::Scalar(JsonValue::Bool(true))))
        }
        SymbolicValue::Concrete(JsonValue::Bool(_)) | SymbolicValue::Clause(_) => Ok(value),
        _ => Err(unsupported("logical operator requires boolean values")),
    }
}

fn negate(value: SymbolicValue) -> Result<SymbolicValue, PermissionError> {
    match value {
        SymbolicValue::Concrete(JsonValue::Bool(value)) => Ok(concrete_bool(!value)),
        SymbolicValue::DataKey(field) => Ok(condition(
            &field,
            WhereValue::Scalar(JsonValue::Bool(false)),
        )),
        SymbolicValue::Clause(condition) => negate_condition(condition).map(SymbolicValue::Clause),
        _ => Err(unsupported("logical not requires a boolean value")),
    }
}

fn negate_condition(condition: WhereCondition) -> Result<WhereCondition, PermissionError> {
    match condition {
        WhereCondition::Condition { path, value } => {
            let value = match value {
                WhereValue::IsNull(value) => WhereValue::IsNull(!value),
                WhereValue::Not(value) => WhereValue::Scalar(value),
                WhereValue::Scalar(value) => WhereValue::Not(value),
                _ => return Err(unsupported("this where operator cannot be negated")),
            };
            Ok(WhereCondition::Condition { path, value })
        }
        WhereCondition::And { conditions } => Ok(WhereCondition::Or {
            conditions: conditions
                .into_iter()
                .map(negate_condition)
                .collect::<Result<_, _>>()?,
        }),
        WhereCondition::Or { conditions } => Ok(WhereCondition::And {
            conditions: conditions
                .into_iter()
                .map(negate_condition)
                .collect::<Result<_, _>>()?,
        }),
    }
}

fn and_conditions(left: WhereCondition, right: WhereCondition) -> WhereCondition {
    let mut conditions = match left {
        WhereCondition::And { conditions } => conditions,
        condition => vec![condition],
    };
    match right {
        WhereCondition::And { conditions: right } => conditions.extend(right),
        condition => conditions.push(condition),
    }
    WhereCondition::And { conditions }
}

fn or_conditions(left: WhereCondition, right: WhereCondition) -> WhereCondition {
    let mut conditions = match left {
        WhereCondition::Or { conditions } => conditions,
        condition => vec![condition],
    };
    match right {
        WhereCondition::Or { conditions: right } => conditions.extend(right),
        condition => conditions.push(condition),
    }
    WhereCondition::Or { conditions }
}

fn condition(path: &str, value: WhereValue) -> SymbolicValue {
    SymbolicValue::Clause(WhereCondition::Condition {
        path: path.split('.').map(str::to_owned).collect(),
        value,
    })
}

fn relation_from_id_path(path: &str) -> Result<String, PermissionError> {
    let segments = path.split('.').collect::<Vec<_>>();
    if segments.len() == 2 && segments[1] == "id" {
        Ok(segments[0].to_owned())
    } else {
        Err(unsupported("empty ref equality only supports relation.id"))
    }
}

fn reverse_comparator(operation: WhereComparator) -> WhereComparator {
    match operation {
        WhereComparator::GreaterThan => WhereComparator::LessThan,
        WhereComparator::GreaterThanOrEqual => WhereComparator::LessThanOrEqual,
        WhereComparator::LessThan => WhereComparator::GreaterThan,
        WhereComparator::LessThanOrEqual => WhereComparator::GreaterThanOrEqual,
        value @ (WhereComparator::Like | WhereComparator::CaseInsensitiveLike) => value,
    }
}

fn compare_concrete(
    left: &JsonValue,
    right: &JsonValue,
    operation: WhereComparator,
) -> Result<bool, PermissionError> {
    let ordering = match (left, right) {
        (JsonValue::Number(left), JsonValue::Number(right)) => compare_json_numbers(left, right)?,
        (JsonValue::String(left), JsonValue::String(right)) => left.cmp(right),
        (JsonValue::Bool(left), JsonValue::Bool(right)) => left.cmp(right),
        _ => return Err(unsupported("concrete values are not comparable")),
    };
    compare_ordering(ordering, operation)
}

fn cel_concrete_equal(left: &JsonValue, right: &JsonValue) -> bool {
    match (left, right) {
        (JsonValue::Number(left), JsonValue::Number(right)) => cel_json_numbers_equal(left, right),
        (JsonValue::Array(left), JsonValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| cel_concrete_equal(left, right))
        }
        (JsonValue::Object(left), JsonValue::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| cel_concrete_equal(left, right))
                })
        }
        _ => left == right,
    }
}

fn java_json_equal(left: &JsonValue, right: &JsonValue) -> bool {
    match (left, right) {
        (JsonValue::Number(left), JsonValue::Number(right)) => {
            match (json_float(left), json_float(right)) {
                (Some(left), Some(right)) => {
                    (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
                }
                (None, None) if left.as_i64().is_some() && right.as_i64().is_some() => {
                    left.as_i64() == right.as_i64()
                }
                (None, None) if left.as_i64().is_none() && right.as_i64().is_none() => {
                    left.as_u64() == right.as_u64()
                }
                _ => false,
            }
        }
        (JsonValue::Array(left), JsonValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| java_json_equal(left, right))
        }
        (JsonValue::Object(left), JsonValue::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| java_json_equal(left, right))
                })
        }
        _ => left == right,
    }
}

fn compare_json_numbers(left: &Number, right: &Number) -> Result<Ordering, PermissionError> {
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        return Ok(left.cmp(&right));
    }
    if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
        return Ok(left.cmp(&right));
    }
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_u64()) {
        return Ok(u64::try_from(left).map_or(Ordering::Less, |left| left.cmp(&right)));
    }
    if let (Some(left), Some(right)) = (left.as_u64(), right.as_i64()) {
        return Ok(u64::try_from(right).map_or(Ordering::Greater, |right| left.cmp(&right)));
    }
    if let (Some(left), Some(right)) = (left.as_i64(), json_float(right)) {
        return Ok(crate::compare_double_int(right, left).reverse());
    }
    if let (Some(left), Some(right)) = (json_float(left), right.as_i64()) {
        return Ok(crate::compare_double_int(left, right));
    }
    if let (Some(left), Some(right)) = (left.as_u64(), json_float(right)) {
        return Ok(crate::compare_double_uint(right, left).reverse());
    }
    if let (Some(left), Some(right)) = (json_float(left), right.as_u64()) {
        return Ok(crate::compare_double_uint(left, right));
    }
    json_float(left)
        .zip(json_float(right))
        .map(|(left, right)| crate::java_double_compare(left, right))
        .ok_or_else(|| unsupported("numbers are not comparable"))
}

#[allow(clippy::float_cmp)]
fn cel_json_numbers_equal(left: &Number, right: &Number) -> bool {
    match (json_float(left), json_float(right)) {
        // cel-java numericEquals deliberately uses primitive double equality
        // for double/double, unlike Java Double.compare ordering.
        (Some(left), Some(right)) => !left.is_nan() && !right.is_nan() && left == right,
        _ => compare_json_numbers(left, right) == Ok(Ordering::Equal),
    }
}

fn compare_ordering(
    ordering: Ordering,
    operation: WhereComparator,
) -> Result<bool, PermissionError> {
    Ok(match operation {
        WhereComparator::GreaterThan => ordering == Ordering::Greater,
        WhereComparator::GreaterThanOrEqual => ordering != Ordering::Less,
        WhereComparator::LessThan => ordering == Ordering::Less,
        WhereComparator::LessThanOrEqual => ordering != Ordering::Greater,
        WhereComparator::Like | WhereComparator::CaseInsensitiveLike => {
            return Err(unsupported("LIKE is not a concrete comparison"));
        }
    })
}

fn json_float(value: &Number) -> Option<f64> {
    (value.as_i64().is_none() && value.as_u64().is_none())
        .then(|| value.as_f64())
        .flatten()
}

fn compare_timestamps(
    left: &chrono::DateTime<chrono::FixedOffset>,
    right: &chrono::DateTime<chrono::FixedOffset>,
    operation: WhereComparator,
) -> Result<bool, PermissionError> {
    let ordering = left.cmp(right);
    Ok(match operation {
        WhereComparator::GreaterThan => ordering == Ordering::Greater,
        WhereComparator::GreaterThanOrEqual => ordering != Ordering::Less,
        WhereComparator::LessThan => ordering == Ordering::Less,
        WhereComparator::LessThanOrEqual => ordering != Ordering::Greater,
        WhereComparator::Like | WhereComparator::CaseInsensitiveLike => {
            return Err(unsupported("LIKE is not a timestamp comparison"));
        }
    })
}

fn timestamp_json(value: chrono::DateTime<chrono::FixedOffset>) -> JsonValue {
    JsonValue::String(value.to_rfc3339())
}

fn queryable_where_scalar(value: &JsonValue) -> bool {
    matches!(
        value,
        JsonValue::Bool(_) | JsonValue::Number(_) | JsonValue::String(_)
    )
}

fn matches_checked_type(value: &JsonValue, data_type: DataType) -> bool {
    match data_type {
        DataType::String => value.is_string(),
        DataType::Number => value.is_number(),
        DataType::Boolean => value.is_boolean(),
        DataType::Date => {
            value.as_str().and_then(parse_timestamp).is_some()
                || value
                    .as_i64()
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .is_some()
        }
    }
}

fn concrete_bool(value: bool) -> SymbolicValue {
    SymbolicValue::Concrete(JsonValue::Bool(value))
}

fn unsupported(message: impl Into<String>) -> PermissionError {
    PermissionError::RuleWhereUnsupported(message.into())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use instant_datalog::{DatalogResult, DatalogTriple, DatalogValue};
    use instant_instaql::{Attribute, Cardinality, ExecutedEntity};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{InMemoryRateLimiter, PermissionAction, RuleSet};

    fn blob(label: &str) -> Attribute {
        Attribute {
            id: Uuid::new_v4(),
            wire: None,
            etype: "posts".to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: label == "id",
            setting_unique: false,
            indexed: false,
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

    fn checked(label: &str, data_type: DataType) -> Attribute {
        Attribute {
            indexed: true,
            checked_data_type: Some(data_type),
            ..blob(label)
        }
    }

    fn owner_link() -> Attribute {
        Attribute {
            id: Uuid::new_v4(),
            wire: None,
            etype: "posts".to_owned(),
            label: "owner".to_owned(),
            value_type: AttributeValueType::Ref,
            cardinality: Cardinality::One,
            unique: false,
            setting_unique: false,
            indexed: false,
            required: false,
            checked_data_type: None,
            indexing: false,
            checking_data_type: false,
            reverse_etype: Some("users".to_owned()),
            reverse_label: Some("posts".to_owned()),
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    fn catalog() -> AttributeCatalog {
        AttributeCatalog {
            attributes: vec![
                blob("id"),
                blob("owner_id"),
                blob("conversation_id"),
                blob("is_public"),
                blob("flag"),
                checked("score", DataType::Number),
                checked("path", DataType::String),
                checked("created_at", DataType::Date),
                owner_link(),
                Attribute {
                    etype: "users".to_owned(),
                    label: "id".to_owned(),
                    unique: true,
                    ..blob("id")
                },
            ],
        }
    }

    fn schema_attribute(
        etype: &str,
        label: &str,
        indexed: bool,
        checked_data_type: Option<DataType>,
    ) -> Attribute {
        Attribute {
            id: Uuid::new_v4(),
            wire: None,
            etype: etype.to_owned(),
            label: label.to_owned(),
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            unique: label == "id",
            setting_unique: false,
            indexed,
            required: false,
            checked_data_type,
            indexing: false,
            checking_data_type: false,
            reverse_etype: None,
            reverse_label: None,
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    fn schema_differential_catalog() -> AttributeCatalog {
        AttributeCatalog {
            attributes: vec![
                schema_attribute("posts", "id", true, None),
                schema_attribute("posts", "owner", false, None),
                schema_attribute("comments", "id", true, None),
                schema_attribute("comments", "score", true, Some(DataType::Number)),
                schema_attribute("files", "id", true, None),
                schema_attribute("files", "path", true, Some(DataType::String)),
                schema_attribute("drafts", "id", true, None),
                schema_attribute("drafts", "title", true, Some(DataType::String)),
                schema_attribute("tasks", "id", true, None),
                schema_attribute("tasks", "visible", false, None),
                schema_attribute("tasks", "published", false, None),
                schema_attribute("labels", "id", true, None),
                schema_attribute("labels", "tag", false, None),
                schema_attribute("votes", "id", true, None),
                schema_attribute("reports", "id", true, None),
                schema_attribute("users", "id", true, None),
                schema_attribute("users", "name", false, None),
                schema_attribute("users", "role", false, None),
                schema_attribute("profiles", "id", true, None),
                schema_attribute("profiles", "nickname", false, None),
                schema_attribute("profiles", "active", false, None),
            ],
        }
    }

    fn extract(source: &str) -> RuleWherePlan {
        let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
        let bindings = EvaluationBindings {
            auth: json!({"id": "user-1", "email": "a@example.com"}),
            ..EvaluationBindings::default()
        };
        match program
            .extract_query_constraint("posts", &catalog(), &bindings, &RefCache::new())
            .unwrap_or_else(|error| panic!("failed to extract `{source}`: {error}"))
        {
            RuleWhereOutcome::Plan(plan) => plan,
            RuleWhereOutcome::NeedsRefs(_) => panic!("unexpected refs"),
        }
    }

    fn extract_with(
        program: &RuleProgram,
        bindings: &EvaluationBindings,
    ) -> Result<RuleWherePlan, PermissionError> {
        match program.extract_query_constraint("posts", &catalog(), bindings, &RefCache::new())? {
            RuleWhereOutcome::Plan(plan) => Ok(plan),
            RuleWhereOutcome::NeedsRefs(_) => panic!("unexpected refs"),
        }
    }

    fn one_condition(path: &str, value: WhereValue) -> QueryConstraint {
        QueryConstraint::Where(vec![WhereCondition::Condition {
            path: path.split('.').map(str::to_owned).collect(),
            value,
        }])
    }

    #[test]
    fn extracts_boolean_equality_null_not_and_membership() {
        assert_eq!(
            extract("data.owner_id == auth.id").constraint,
            Some(one_condition(
                "owner_id",
                WhereValue::Scalar(json!("user-1"))
            ))
        );
        assert_eq!(
            extract("data.conversation_id != null").constraint,
            Some(one_condition("conversation_id", WhereValue::IsNull(false)))
        );
        assert_eq!(
            extract("data.owner_id != 'other'").constraint,
            Some(one_condition("owner_id", WhereValue::Not(json!("other"))))
        );
        assert_eq!(
            extract("data.owner_id in ['one', 'two']").constraint,
            Some(one_condition(
                "owner_id",
                WhereValue::In(vec![json!("one"), json!("two")])
            ))
        );
        assert_eq!(
            extract("!!data.flag").constraint,
            Some(one_condition("flag", WhereValue::Not(json!(false))))
        );
    }

    #[test]
    fn extracts_flattened_boolean_trees_and_concrete_short_circuits() {
        let plan = extract("data.is_public || data.owner_id == auth.id");
        assert_eq!(
            plan.constraint,
            Some(QueryConstraint::Where(vec![WhereCondition::Or {
                conditions: vec![
                    WhereCondition::Condition {
                        path: vec!["is_public".to_owned()],
                        value: WhereValue::Scalar(json!(true)),
                    },
                    WhereCondition::Condition {
                        path: vec!["owner_id".to_owned()],
                        value: WhereValue::Scalar(json!("user-1")),
                    },
                ],
            }]))
        );
        assert_eq!(
            extract("auth.email == 'a@example.com' || data.owner_id == 'x'").constraint,
            None
        );
        assert_eq!(
            extract("auth.email == 'wrong' && data.owner_id == 'x'").constraint,
            Some(QueryConstraint::DenyAll)
        );
    }

    #[test]
    fn extracts_concrete_conditionals_without_evaluating_the_unselected_branch() {
        assert_eq!(
            extract("auth.email == 'a@example.com' ? data.owner_id == auth.id : data.is_public")
                .constraint,
            Some(one_condition(
                "owner_id",
                WhereValue::Scalar(json!("user-1"))
            ))
        );
        assert_eq!(
            extract("auth.email == 'wrong' ? data.owner_id == auth.id : data.is_public").constraint,
            Some(one_condition("is_public", WhereValue::Scalar(json!(true))))
        );
        assert_eq!(
            extract("true ? data.is_public : data.owner_id + '-suffix' == auth.id").constraint,
            Some(one_condition("is_public", WhereValue::Scalar(json!(true))))
        );
    }

    #[test]
    fn presence_pushdown_matches_runtime_or_falls_back_safely() {
        let request = RuleProgram::compile(
            PermissionAction::View,
            "has(request.ip) || has(request.origin) ? data.flag : data.is_public",
        )
        .unwrap();
        assert_eq!(
            extract_with(&request, &EvaluationBindings::default())
                .unwrap()
                .constraint,
            Some(one_condition("is_public", WhereValue::Scalar(json!(true))))
        );
        assert_eq!(
            extract_with(
                &request,
                &EvaluationBindings {
                    request: crate::RequestBinding {
                        origin: Some("https://example.com".to_owned()),
                        ..crate::RequestBinding::default()
                    },
                    ..EvaluationBindings::default()
                },
            )
            .unwrap()
            .constraint,
            Some(one_condition("flag", WhereValue::Scalar(json!(true))))
        );

        let auth = RuleProgram::compile(
            PermissionAction::View,
            "has(auth.email) ? (auth.email == null ? data.flag : data.owner_id == auth.id) : data.is_public",
        )
        .unwrap();
        assert_eq!(
            extract_with(
                &auth,
                &EvaluationBindings {
                    auth: json!({"id": "user-1"}),
                    ..EvaluationBindings::default()
                },
            )
            .unwrap()
            .constraint,
            Some(one_condition("is_public", WhereValue::Scalar(json!(true))))
        );
        assert_eq!(
            extract_with(
                &auth,
                &EvaluationBindings {
                    auth: json!({"id": "user-1", "email": null}),
                    ..EvaluationBindings::default()
                },
            )
            .unwrap()
            .constraint,
            Some(one_condition("flag", WhereValue::Scalar(json!(true))))
        );

        let bind = RuleProgram::compile_with_binds(
            PermissionAction::View,
            "emailMatches ? data.flag : data.is_public",
            vec![(
                "emailMatches".to_owned(),
                "has(auth.email) ? auth.email == 'x@example.com' : false".to_owned(),
            )],
        )
        .unwrap();
        assert_eq!(
            extract_with(&bind, &EvaluationBindings::default())
                .unwrap()
                .constraint,
            Some(one_condition("is_public", WhereValue::Scalar(json!(true))))
        );
    }

    #[test]
    fn nested_and_data_presence_fall_back_without_semantic_narrowing() {
        let nested = RuleProgram::compile(
            PermissionAction::View,
            "has(auth.profile.email) ? data.flag : data.is_public",
        )
        .unwrap();
        for auth in [json!({}), json!({"profile": null})] {
            assert!(matches!(
                extract_with(
                    &nested,
                    &EvaluationBindings {
                        auth,
                        ..EvaluationBindings::default()
                    },
                ),
                Err(PermissionError::RuleWhereUnsupported(_))
            ));
        }
        assert_eq!(
            extract_with(
                &nested,
                &EvaluationBindings {
                    auth: json!({"profile": {}}),
                    ..EvaluationBindings::default()
                },
            )
            .unwrap()
            .constraint,
            Some(one_condition("is_public", WhereValue::Scalar(json!(true))))
        );
        assert_eq!(
            extract_with(
                &nested,
                &EvaluationBindings {
                    auth: json!({"profile": {"email": null}}),
                    ..EvaluationBindings::default()
                },
            )
            .unwrap()
            .constraint,
            Some(one_condition("flag", WhereValue::Scalar(json!(true))))
        );

        let data = RuleProgram::compile(
            PermissionAction::View,
            "has(data.flag) ? data.flag == null : data.is_public",
        )
        .unwrap();
        assert!(matches!(
            extract_with(&data, &EvaluationBindings::default()),
            Err(PermissionError::RuleWhereUnsupported(_))
        ));
    }

    #[test]
    fn extracts_comparators_string_predicates_and_empty_refs() {
        assert_eq!(
            extract("data.score >= 10").constraint,
            Some(one_condition(
                "score",
                WhereValue::Comparator {
                    operation: WhereComparator::GreaterThanOrEqual,
                    value: json!(10),
                }
            ))
        );
        assert_eq!(
            extract("data.path.endsWith('.jpg')").constraint,
            Some(one_condition(
                "path",
                WhereValue::Comparator {
                    operation: WhereComparator::Like,
                    value: json!("%.jpg"),
                }
            ))
        );
        assert_eq!(
            extract("data.ref('owner.id') == []").constraint,
            Some(one_condition("owner", WhereValue::IsNull(true)))
        );
        assert_eq!(
            extract("size(data.ref('owner.id')) != 0").constraint,
            Some(one_condition("owner", WhereValue::IsNull(false)))
        );
        assert_eq!(
            extract("auth.id in data.ref('owner.id')").constraint,
            Some(one_condition(
                "owner.id",
                WhereValue::Scalar(json!("user-1"))
            ))
        );
    }

    #[test]
    fn concrete_timestamp_ordering_uses_instants_instead_of_rfc3339_text() {
        assert_eq!(
            extract("timestamp('2026-01-01T00:00:00-07:00') < timestamp('2026-01-01T06:30:00Z')")
                .constraint,
            Some(QueryConstraint::DenyAll),
        );
        assert_eq!(
            extract("timestamp('2026-01-01T00:00:00-07:00') > timestamp('2026-01-01T06:30:00Z')")
                .constraint,
            None,
        );
        assert_eq!(
            extract("timestamp('2026-01-01T00:00:00-07:00') == timestamp('2026-01-01T07:00:00Z')")
                .constraint,
            None,
        );
        assert_eq!(
            extract("timestamp(data.created_at) < timestamp('2026-01-02T00:00:00Z')").constraint,
            Some(one_condition(
                "created_at",
                WhereValue::Comparator {
                    operation: WhereComparator::LessThan,
                    value: json!("2026-01-02T00:00:00+00:00"),
                },
            )),
        );
    }

    fn assert_runtime_and_planner_numeric_case(predicate: &str, auth: &JsonValue, expected: bool) {
        let bindings = EvaluationBindings {
            auth: auth.clone(),
            ..EvaluationBindings::default()
        };
        assert_eq!(
            RuleProgram::compile(PermissionAction::View, predicate)
                .unwrap()
                .evaluate_bool(&bindings)
                .unwrap(),
            expected,
            "runtime diverged for {predicate}"
        );

        let source = format!("{predicate} ? data.flag : data.is_public");
        let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
        assert_eq!(
            extract_with(&program, &bindings)
                .unwrap_or_else(|error| panic!("planner failed for {predicate}: {error}"))
                .constraint,
            Some(one_condition(
                if expected { "flag" } else { "is_public" },
                WhereValue::Scalar(json!(true)),
            )),
            "planner diverged for {predicate}"
        );
    }

    fn assert_uint_runtime_and_planner_fallback(predicate: &str, expected: bool) {
        assert_eq!(
            RuleProgram::compile(PermissionAction::View, predicate)
                .unwrap()
                .evaluate_bool(&EvaluationBindings::default())
                .unwrap(),
            expected,
            "runtime diverged for {predicate}"
        );

        let source = format!("{predicate} ? data.flag : data.is_public");
        let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
        assert!(
            matches!(
                extract_with(&program, &EvaluationBindings::default()),
                Err(PermissionError::RuleWhereUnsupported(_)),
            ),
            "uint literal did not fall back for {predicate}"
        );
    }

    #[test]
    fn concrete_numeric_decisions_match_cel_runtime_before_query_pushdown() {
        const ORACLE: &str = "cel-java@v0.11.1 common/src/main/java/dev/cel/common/internal/ComparisonFunctions.java:37-157; runtime/src/main/java/dev/cel/runtime/RuntimeEquality.java:48-61,131-220";
        assert!(ORACLE.contains("cel-java@v0.11.1"));

        let cases = [
            ("auth.value == 1.0", json!({"value": 1}), true),
            ("auth.value in [1.0, 2.0]", json!({"value": 1}), true),
            (
                "auth.value > 9007199254740992",
                json!({"value": 9_007_199_254_740_993_i64}),
                true,
            ),
            (
                "auth.value == auth.expected",
                json!({
                    "value": [1, {"nested": 2}],
                    "expected": [1.0, {"nested": 2.0}],
                }),
                true,
            ),
            (
                "auth.value == 9007199254740992",
                json!({"value": 9_007_199_254_740_993_i64}),
                false,
            ),
            (
                "auth.value == 9007199254740992.0",
                json!({"value": 9_007_199_254_740_993_i64}),
                true,
            ),
            (
                "auth.value > 9007199254740992.0",
                json!({"value": 9_007_199_254_740_993_i64}),
                false,
            ),
            (
                "9007199254740992.0 == auth.value",
                json!({"value": 9_007_199_254_740_993_i64}),
                true,
            ),
            (
                "9007199254740992.0 < auth.value",
                json!({"value": 9_007_199_254_740_993_i64}),
                false,
            ),
            ("auth.value < 1.5", json!({"value": 1}), true),
            ("1.5 > auth.value", json!({"value": 1}), true),
            ("auth.value > -1.5", json!({"value": -1}), true),
            (
                "auth.value == 9223372036854775808.0",
                json!({"value": i64::MAX}),
                true,
            ),
            (
                "auth.value == -9223372036854775808.0",
                json!({"value": i64::MIN}),
                true,
            ),
            ("auth.value == -0.0", json!({"value": 0}), false),
            ("auth.value > -0.0", json!({"value": 0}), true),
            ("-0.0 < auth.value", json!({"value": 0}), true),
            ("auth.value == 0.0", json!({"value": -0.0}), true),
            ("auth.value < 0.0", json!({"value": -0.0}), true),
            ("auth.value in [[1.0]]", json!({"value": [1]}), false),
        ];
        for (predicate, auth, expected) in cases {
            assert_runtime_and_planner_numeric_case(predicate, &auth, expected);
        }

        for (predicate, expected) in [
            ("9223372036854775807 == 9223372036854775807u", false),
            ("9223372036854775807 < 9223372036854775807u", true),
            ("9223372036854775807u > 9223372036854775807", true),
        ] {
            assert_uint_runtime_and_planner_fallback(predicate, expected);
        }
    }

    #[test]
    fn complex_or_null_membership_values_fall_back_instead_of_reaching_instaql() {
        for source in [
            "data.owner_id in [null]",
            "data.owner_id in [['nested']]",
            "data.owner_id in [{'nested': true}]",
        ] {
            let program = RuleProgram::compile(PermissionAction::View, source).unwrap();
            assert!(matches!(
                program.extract_query_constraint(
                    "posts",
                    &catalog(),
                    &EvaluationBindings::default(),
                    &RefCache::new(),
                ),
                Err(PermissionError::RuleWhereUnsupported(_)),
            ));
        }

        let mut refs = RefCache::new();
        let plan = RuleSet::new(json!({
            "posts": {"allow": {"view": "data.owner_id in [['nested']]"}}
        }))
        .plan_query_permissions(
            &json!({"posts": {}}),
            &catalog(),
            &EvaluationBindings::default(),
            &mut refs,
            |_| panic!("no refs expected"),
        )
        .unwrap();
        assert_eq!(plan.fallback_etypes, vec!["posts"]);
        assert!(plan.constraints.is_empty());
    }

    #[test]
    fn numeric_map_membership_retains_runtime_post_filter_fallback() {
        let program =
            RuleProgram::compile(PermissionAction::View, "dyn(data.lookup) in {1u: true}").unwrap();
        assert!(matches!(
            program.extract_query_constraint(
                "posts",
                &catalog(),
                &EvaluationBindings::default(),
                &RefCache::new(),
            ),
            Err(PermissionError::RuleWhereUnsupported(_)),
        ));

        let mut refs = RefCache::new();
        let plan = RuleSet::new(json!({
            "posts": {"allow": {"view": "dyn(data.lookup) in {1u: true}"}}
        }))
        .plan_query_permissions(
            &json!({"posts": {}}),
            &catalog(),
            &EvaluationBindings::default(),
            &mut refs,
            |_| panic!("no refs expected"),
        )
        .unwrap();
        assert_eq!(plan.fallback_etypes, vec!["posts"]);
        assert!(plan.constraints.is_empty());
    }

    #[test]
    fn resolves_reachable_binds_and_auth_refs() {
        let user_id = Uuid::new_v4();
        let request = RefRequest {
            entity_id: user_id.into(),
            etype: "$users".to_owned(),
            path: "profile.id".to_owned(),
        };
        let program = RuleProgram::compile_with_binds(
            PermissionAction::View,
            "data.owner_id == owner",
            vec![(
                "owner".to_owned(),
                "auth.ref('$user.profile.id')[0]".to_owned(),
            )],
        )
        .unwrap();
        let bindings = EvaluationBindings {
            auth: json!({"id": user_id}),
            ..EvaluationBindings::default()
        };
        assert_eq!(
            program
                .extract_query_constraint("posts", &catalog(), &bindings, &RefCache::new())
                .unwrap(),
            RuleWhereOutcome::NeedsRefs(vec![request.clone()])
        );
        let refs = HashMap::from([(request, vec![json!("profile-1")])]);
        let RuleWhereOutcome::Plan(plan) = program
            .extract_query_constraint("posts", &catalog(), &bindings, &refs)
            .unwrap()
        else {
            panic!("expected plan")
        };
        assert_eq!(
            plan.constraint,
            Some(one_condition(
                "owner_id",
                WhereValue::Scalar(json!("profile-1"))
            ))
        );
    }

    #[test]
    fn records_rate_limits_without_consuming_them() {
        let rules = RuleSet::new(json!({
            "$rateLimits": {
                "read": {"limits": [{"capacity": 10, "refillRate": 1, "refillPeriod": "1s"}]}
            }
        }));
        let mut bindings = EvaluationBindings {
            auth: json!({"id": "user-1"}),
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(
                Uuid::new_v4(),
                &mut bindings,
                Arc::new(InMemoryRateLimiter::default()),
            )
            .unwrap();
        let program = RuleProgram::compile(
            PermissionAction::View,
            "rateLimit.read.limit(auth.id, 2) && data.is_public",
        )
        .unwrap();
        let RuleWhereOutcome::Plan(plan) = program
            .extract_query_constraint("posts", &catalog(), &bindings, &RefCache::new())
            .unwrap()
        else {
            panic!("expected plan")
        };
        assert_eq!(
            plan.constraint,
            Some(one_condition("is_public", WhereValue::Scalar(json!(true))))
        );
        assert_eq!(plan.rate_limits.len(), 1);
        assert_eq!(plan.rate_limits[0].tokens, 2);
        assert_eq!(plan.rate_limits[0].bucket_key, json!("user-1"));
    }

    #[test]
    fn plans_and_compiles_query_permissions_with_safe_fallbacks() {
        let rules = RuleSet::new(json!({
            "posts": {"allow": {"view": "data.owner_id == auth.id"}}
        }));
        let query = json!({"posts": {}});
        let bindings = EvaluationBindings {
            auth: json!({"id": "user-1"}),
            ..EvaluationBindings::default()
        };
        let mut refs = RefCache::new();
        let plan = rules
            .plan_query_permissions(&query, &catalog(), &bindings, &mut refs, |_| {
                panic!("no refs expected")
            })
            .unwrap();
        assert!(plan.fallback_etypes.is_empty());
        assert_eq!(
            plan.constraints.get("posts"),
            Some(&one_condition(
                "owner_id",
                WhereValue::Scalar(json!("user-1"))
            ))
        );
        let compiled = plan.compile(&query, &catalog(), false).unwrap();
        assert_eq!(compiled.roots[0].options.where_clause.len(), 1);

        let fallback = RuleSet::new(json!({
            "posts": {"allow": {"view": "data.owner_id + '-suffix' == auth.id"}}
        }))
        .plan_query_permissions(&query, &catalog(), &bindings, &mut refs, |_| {
            panic!("no refs expected")
        })
        .unwrap();
        assert_eq!(fallback.fallback_etypes, vec!["posts"]);
        assert!(fallback.constraints.is_empty());
    }

    #[test]
    fn unsupported_conjunctions_fall_back_without_retaining_partial_pushdown() {
        let catalog = AttributeCatalog {
            attributes: ["id", "owner", "visible", "published"]
                .into_iter()
                .map(|label| schema_attribute("posts", label, label == "id", None))
                .collect(),
        };
        let rules = RuleSet::new(json!({
            "posts": {
                "allow": {
                    "view": "data.owner == auth.id && [data.visible, data.published].all(value, value)"
                }
            }
        }));
        let bindings = EvaluationBindings {
            auth: json!({"id": "owner-1"}),
            ..EvaluationBindings::default()
        };
        let query = json!({"posts": {}});
        let plan = rules
            .plan_query_permissions(&query, &catalog, &bindings, &mut RefCache::new(), |_| {
                panic!("this rule has no refs")
            })
            .unwrap();
        assert_eq!(plan.fallback_etypes, vec!["posts"]);
        assert!(
            plan.constraints.is_empty(),
            "a partial symbolic conjunction must not narrow rows before CEL post-filtering"
        );

        let entities = vec![
            json!({"id": "post-1", "owner": "owner-1", "visible": true, "published": true}),
            json!({"id": "post-2", "owner": "owner-1", "visible": true, "published": false}),
            json!({"id": "post-3", "owner": "owner-2", "visible": true, "published": true}),
        ];
        let filtered = rules
            .filter_view_entities("posts", &entities, &bindings, &mut RefCache::new(), |_| {
                panic!("this rule has no refs")
            })
            .unwrap();
        assert_eq!(filtered, vec![entities[0].clone()]);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // Keeps the source-pinned schema corpus in one table.
    fn full_schema_permission_differential_preserves_pushdown_and_post_filter_semantics() {
        // Pinned oracle: instantdb/instant@45d3ffbb server/src/instant/db/cel.clj:729-1617.
        // The JVM planner only emits native where clauses for the supported
        // symbolic overloads; every other valid CEL expression is evaluated
        // against returned entities rather than weakening the query.
        let catalog = schema_differential_catalog();
        let bindings = EvaluationBindings {
            auth: json!({"id": "owner-1"}),
            ..EvaluationBindings::default()
        };
        let cases = [
            (
                "posts",
                "data.owner == auth.id",
                vec![
                    json!({"id": "post-1", "owner": "owner-1"}),
                    json!({"id": "post-2", "owner": "owner-2"}),
                ],
                vec!["post-1"],
                true,
            ),
            (
                "comments",
                "data.score >= 2",
                vec![
                    json!({"id": "comment-1", "score": 2}),
                    json!({"id": "comment-2", "score": 1}),
                ],
                vec!["comment-1"],
                true,
            ),
            (
                "files",
                "data.path.startsWith('public-')",
                vec![
                    json!({"id": "file-1", "path": "public-readme"}),
                    json!({"id": "file-2", "path": "private-note"}),
                ],
                vec!["file-1"],
                true,
            ),
            (
                "drafts",
                "['public-', 'shared-'].exists(prefix, data.title.startsWith(prefix))",
                vec![
                    json!({"id": "draft-1", "title": "public-plan"}),
                    json!({"id": "draft-2", "title": "private-plan"}),
                ],
                vec!["draft-1"],
                false,
            ),
            (
                "tasks",
                "[data.visible, data.published].all(value, value)",
                vec![
                    json!({"id": "task-1", "visible": true, "published": true}),
                    json!({"id": "task-2", "visible": true, "published": false}),
                ],
                vec!["task-1"],
                false,
            ),
            (
                "labels",
                "[data.tag, 'public'].filter(value, value == 'public').size() == 1",
                vec![
                    json!({"id": "label-1", "tag": "private"}),
                    json!({"id": "label-2", "tag": "public"}),
                ],
                vec!["label-1"],
                false,
            ),
            (
                "votes",
                "[true, false].exists_one(value, value)",
                vec![json!({"id": "vote-1"})],
                vec!["vote-1"],
                false,
            ),
            (
                "reports",
                "[1, 2].map(value, value + 1)[0] == 2",
                vec![json!({"id": "report-1"})],
                vec!["report-1"],
                false,
            ),
            (
                "users",
                "data.name == data.role",
                vec![
                    json!({"id": "user-1", "name": "admin", "role": "admin"}),
                    json!({"id": "user-2", "name": "member", "role": "admin"}),
                ],
                vec!["user-1"],
                false,
            ),
            (
                "profiles",
                "has(data.nickname) && data.active",
                vec![
                    json!({"id": "profile-1", "nickname": "a", "active": true}),
                    json!({"id": "profile-2", "active": true}),
                    json!({"id": "profile-3", "nickname": "c", "active": false}),
                ],
                vec!["profile-1"],
                false,
            ),
        ];

        for (etype, source, entities, expected_ids, pushable) in cases {
            let rules = RuleSet::new(json!({etype: {"allow": {"view": source}}}));
            let query =
                JsonValue::Object(serde_json::Map::from_iter([(etype.to_owned(), json!({}))]));
            let plan = rules
                .plan_query_permissions(&query, &catalog, &bindings, &mut RefCache::new(), |_| {
                    panic!("{source} does not use refs")
                })
                .unwrap_or_else(|error| panic!("failed to plan {etype}: {error}"));
            assert_eq!(
                plan.constraints.contains_key(etype),
                pushable,
                "unexpected pushdown for {source}",
            );
            assert_eq!(
                plan.fallback_etypes.contains(&etype.to_owned()),
                !pushable,
                "unexpected fallback for {source}",
            );

            let mut refs = RefCache::new();
            let filtered = rules
                .filter_view_entities(etype, &entities, &bindings, &mut refs, |_| {
                    panic!("{source} does not use refs")
                })
                .unwrap_or_else(|error| panic!("failed to post-filter {etype}: {error}"));
            assert_eq!(
                filtered
                    .iter()
                    .map(|entity| entity["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                expected_ids,
                "post-filter diverged for {source}",
            );
        }
    }

    #[test]
    fn query_rate_limits_scale_by_checked_entity_count() {
        let rules = RuleSet::new(json!({
            "$rateLimits": {
                "read": {"limits": [{"capacity": 5, "refillRate": 1, "refillPeriod": "1h"}]}
            },
            "posts": {"allow": {"view": "rateLimit.read.limit(auth.id, 2) && true"}}
        }));
        let limiter = Arc::new(InMemoryRateLimiter::default());
        let mut bindings = EvaluationBindings {
            auth: json!({"id": "user-1"}),
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(Uuid::new_v4(), &mut bindings, limiter.clone())
            .unwrap();
        let plan = rules
            .plan_query_permissions(
                &json!({"posts": {}}),
                &catalog(),
                &bindings,
                &mut RefCache::new(),
                |_| panic!("no refs expected"),
            )
            .unwrap();
        assert_eq!(plan.rate_limits.len(), 1);
        assert!(matches!(
            plan.consume_rate_limits(&HashMap::from([("posts".to_owned(), 3)]), limiter.as_ref()),
            Err(PermissionError::RateLimited { .. })
        ));
        plan.consume_rate_limits(&HashMap::from([("posts".to_owned(), 2)]), limiter.as_ref())
            .unwrap();
    }

    #[test]
    fn post_filter_does_not_double_consume_deferred_query_limits() {
        let rules = RuleSet::new(json!({
            "$rateLimits": {
                "read": {"limits": [{"capacity": 2, "refillRate": 1, "refillPeriod": "1h"}]}
            },
            "posts": {"allow": {"view": "rateLimit.read.limit(auth.id) && data.is_public"}}
        }));
        let limiter = Arc::new(InMemoryRateLimiter::default());
        let mut bindings = EvaluationBindings {
            auth: json!({"id": "user-1"}),
            ..EvaluationBindings::default()
        };
        rules
            .configure_rate_limits(Uuid::new_v4(), &mut bindings, limiter.clone())
            .unwrap();
        let query_json = json!({"posts": {}});
        let plan = rules
            .plan_query_permissions(
                &query_json,
                &catalog(),
                &bindings,
                &mut RefCache::new(),
                |_| panic!("no refs expected"),
            )
            .unwrap();
        let mut post_bindings = plan.post_filter_bindings(&bindings);
        post_bindings.data = json!({"id": Uuid::new_v4(), "is_public": true});
        post_bindings.data_etype = Some("posts".to_owned());
        assert!(
            rules
                .program("posts", PermissionAction::View)
                .unwrap()
                .unwrap()
                .evaluate_bool(&post_bindings)
                .unwrap()
        );
        plan.consume_rate_limits(&HashMap::from([("posts".to_owned(), 2)]), limiter.as_ref())
            .unwrap();

        let compiled = plan.compile(&query_json, &catalog(), false).unwrap();
        let entity_id = Uuid::new_v4();
        let triple = DatalogTriple {
            entity_id,
            attr_id: catalog().forward("posts", "id").unwrap().id,
            value: DatalogValue::Json(json!(entity_id)),
            created_at: 1,
        };
        let results = vec![ExecutedNode {
            selector: DatalogResult::default(),
            aggregate: None,
            entities: vec![
                ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![vec![triple.clone()]],
                        ..DatalogResult::default()
                    },
                    children: Vec::new(),
                },
                ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: vec![vec![triple]],
                        ..DatalogResult::default()
                    },
                    children: Vec::new(),
                },
            ],
        }];
        assert_eq!(
            plan.checked_entity_counts(&compiled, &results).unwrap(),
            HashMap::from([("posts".to_owned(), 1)])
        );
    }
}
