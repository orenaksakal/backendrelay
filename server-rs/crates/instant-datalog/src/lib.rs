//! Compatibility types and transformations for `InstantDB`'s Datalog-ish query engine.
//!
//! This crate deliberately starts with the pure transformations from
//! `instant.db.datalog`. Keeping these functions independent of Postgres lets us run
//! the same fixtures through Clojure and Rust before porting SQL generation.

use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasher;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

mod cte;
mod keyed_scan;
mod pagination;
mod predicate;
mod result;
mod sql;

pub use cte::*;
pub use keyed_scan::*;
pub use pagination::*;
pub use predicate::*;
pub use result::*;
pub use sql::*;

const COMPONENT_COUNT: usize = 4;

/// One of the physical triple indexes supported by the existing Postgres schema.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexKey {
    Ea,
    Eav,
    Av,
    Ave,
    Vae,
}

impl fmt::Display for IndexKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ea => "ea",
            Self::Eav => "eav",
            Self::Av => "av",
            Self::Ave => "ave",
            Self::Vae => "vae",
        })
    }
}

impl IndexKey {
    /// Whether the physical index interprets its value component as a UUID
    /// reference rather than an arbitrary JSON value.
    #[must_use]
    pub const fn value_is_reference(self) -> bool {
        matches!(self, Self::Eav | Self::Vae)
    }
}

impl FromStr for IndexKey {
    type Err = PatternError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim_start_matches(':') {
            "ea" => Ok(Self::Ea),
            "eav" => Ok(Self::Eav),
            "av" => Ok(Self::Av),
            "ave" => Ok(Self::Ave),
            "vae" => Ok(Self::Vae),
            other => Err(PatternError::UnknownIndex(other.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataType {
    String,
    Number,
    Boolean,
    Date,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ComparatorOperation {
    #[serde(rename = "$gt")]
    GreaterThan,
    #[serde(rename = "$gte")]
    GreaterThanOrEqual,
    #[serde(rename = "$lt")]
    LessThan,
    #[serde(rename = "$lte")]
    LessThanOrEqual,
    #[serde(rename = "$like")]
    Like,
    #[serde(rename = "$ilike")]
    CaseInsensitiveLike,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ComparatorClause {
    pub operation: ComparatorOperation,
    pub data_type: DataType,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Mirrors the public Clojure `$isNull` contract.
pub struct IsNullClause {
    pub attr_id: Uuid,
    pub nil: bool,
    pub reference: bool,
    pub reverse: bool,
    pub indexed: bool,
    pub indexed_checked_type: Option<DataType>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "function", content = "argument", rename_all = "kebab-case")]
pub enum FunctionClause {
    Not(Value),
    IsNull(IsNullClause),
    Comparator(ComparatorClause),
    EntityIdStartsWith(String),
}

/// An index may carry the checked data type used by comparator queries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Index {
    pub key: IndexKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<DataType>,
}

impl Index {
    #[must_use]
    pub const fn new(key: IndexKey) -> Self {
        Self {
            key,
            data_type: None,
        }
    }

    #[must_use]
    pub const fn checked(key: IndexKey, data_type: DataType) -> Self {
        Self {
            key,
            data_type: Some(data_type),
        }
    }
}

/// A query symbol. The wildcard `_` is represented by [`Term::Any`], never a symbol.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Symbol(String);

impl Symbol {
    /// Creates a named query variable.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError::InvalidSymbol`] for an empty value or `_`, which is
    /// reserved for [`Term::Any`].
    pub fn new(value: impl Into<String>) -> Result<Self, PatternError> {
        let value = value.into();
        if value.is_empty() || value == "_" {
            return Err(PatternError::InvalidSymbol(value));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A raw component before Clojure's `coerce-pattern` behavior is applied.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum RawTerm {
    Any,
    Variable(Symbol),
    Constant(Value),
    Constants(Vec<Value>),
    Function(FunctionClause),
}

/// A normalized component. Constants always have set semantics, including singletons.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum Term {
    Any,
    Variable(Symbol),
    Constants(Vec<Value>),
    Function(FunctionClause),
}

impl Term {
    #[must_use]
    pub fn constants(values: impl IntoIterator<Item = Value>) -> Self {
        let mut unique = Vec::new();
        for value in values {
            if !unique.contains(&value) {
                unique.push(value);
            }
        }
        Self::Constants(unique)
    }

    #[must_use]
    pub fn variable(&self) -> Option<&Symbol> {
        match self {
            Self::Variable(symbol) => Some(symbol),
            Self::Any | Self::Constants(_) | Self::Function(_) => None,
        }
    }
}

impl From<RawTerm> for Term {
    fn from(raw: RawTerm) -> Self {
        match raw {
            RawTerm::Any => Self::Any,
            RawTerm::Variable(symbol) => Self::Variable(symbol),
            RawTerm::Constant(value) => Self::constants([value]),
            RawTerm::Constants(values) => Self::constants(values),
            RawTerm::Function(value) => Self::Function(value),
        }
    }
}

/// A raw `[index entity attribute value created-at]` pattern.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RawPattern {
    pub index: Index,
    pub components: Vec<RawTerm>,
}

impl RawPattern {
    #[must_use]
    pub fn new(index: Index, components: Vec<RawTerm>) -> Self {
        Self { index, components }
    }

    /// Pads missing components with wildcards and gives constants set semantics.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError::TooManyComponents`] when the pattern contains more
    /// than entity, attribute, value, and created-at components.
    pub fn normalize(self) -> Result<Pattern, PatternError> {
        if self.components.len() > COMPONENT_COUNT {
            return Err(PatternError::TooManyComponents {
                actual: self.components.len(),
            });
        }

        for (position, term) in self.components.iter().enumerate() {
            validate_raw_term(position.into(), term)?;
        }

        let mut components: Vec<Term> = self.components.into_iter().map(Term::from).collect();
        components.resize(COMPONENT_COUNT, Term::Any);

        let mut components = components.into_iter();
        Ok(Pattern {
            index: self.index,
            entity: components.next().unwrap_or(Term::Any),
            attribute: components.next().unwrap_or(Term::Any),
            value: components.next().unwrap_or(Term::Any),
            created_at: components.next().unwrap_or(Term::Any),
        })
    }
}

fn validate_raw_term(position: PatternPosition, term: &RawTerm) -> Result<(), PatternError> {
    let values = match term {
        RawTerm::Constant(value) => Some(std::slice::from_ref(value)),
        RawTerm::Constants(values) => Some(values.as_slice()),
        RawTerm::Any | RawTerm::Variable(_) => None,
        RawTerm::Function(function) => {
            return validate_function_position(position, function);
        }
    };

    let Some(values) = values else {
        return Ok(());
    };
    for value in values {
        let valid = match position {
            PatternPosition::Entity => valid_entity_lookup(value),
            PatternPosition::Attribute => json_uuid(value).is_some(),
            PatternPosition::Value => true,
            PatternPosition::CreatedAt => value.is_number(),
        };
        if !valid {
            return Err(PatternError::InvalidRawTerm {
                position,
                value: value.to_string(),
            });
        }
    }
    Ok(())
}

fn validate_function_position(
    position: PatternPosition,
    function: &FunctionClause,
) -> Result<(), PatternError> {
    match (position, function) {
        (PatternPosition::Entity, FunctionClause::Not(value)) => {
            if json_uuid(value).is_some() {
                Ok(())
            } else {
                Err(PatternError::InvalidEntityNot(value.to_string()))
            }
        }
        (PatternPosition::Value, _) => Ok(()),
        _ => Err(PatternError::InvalidRawTerm {
            position,
            value: serde_json::to_string(function)
                .unwrap_or_else(|_| "<invalid function>".to_owned()),
        }),
    }
}

fn valid_entity_lookup(value: &Value) -> bool {
    if json_uuid(value).is_some() {
        return true;
    }
    let Some(parts) = value.as_array() else {
        return false;
    };
    parts.len() == 2 && json_uuid(&parts[0]).is_some()
}

fn json_uuid(value: &Value) -> Option<Uuid> {
    value.as_str()?.parse().ok()
}

/// The normalized equivalent of `instant.db.datalog/->named-patterns` for one pattern.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Pattern {
    pub index: Index,
    pub entity: Term,
    pub attribute: Term,
    pub value: Term,
    pub created_at: Term,
}

impl Pattern {
    /// Returns the symbol bindings used to generate joins between SQL CTEs.
    #[must_use]
    pub fn symbol_map(&self, pattern_index: usize) -> HashMap<Symbol, Vec<BindingPath>> {
        let mut bindings = HashMap::new();
        for (component, term) in [
            (Component::Entity, &self.entity),
            (Component::Attribute, &self.attribute),
            (Component::Value, &self.value),
        ] {
            if let Some(symbol) = term.variable() {
                bindings
                    .entry(symbol.clone())
                    .or_insert_with(Vec::new)
                    .push(BindingPath::new(pattern_index, component, self.index.key));
            }
        }
        bindings
    }

    /// Returns bindings in the recursive representation used after `or` clauses.
    #[must_use]
    pub fn nested_symbol_map(&self, pattern_index: usize) -> NestedSymbolMap {
        self.symbol_map(pattern_index)
            .into_iter()
            .map(|(symbol, paths)| (symbol, paths.into_iter().map(BindingTree::Path).collect()))
            .collect()
    }

    /// Produces the conservative invalidation topic for an unbound pattern.
    #[must_use]
    pub fn coarse_topic(&self) -> CoarseTopic {
        CoarseTopic {
            indexes: vec![self.index.key],
            entity: CoarseTerm::from_term(&self.entity),
            attribute: CoarseTerm::from_term(&self.attribute),
            value: CoarseTerm::from_term(&self.value),
            created_at: CoarseTerm::from_term(&self.created_at),
        }
    }

    fn has_constant(&self) -> bool {
        [&self.entity, &self.attribute, &self.value]
            .into_iter()
            .any(|term| matches!(term, Term::Constants(_)))
    }

    fn variables(&self) -> impl Iterator<Item = &Symbol> {
        self.variables_with_components()
            .map(|(_component, symbol)| symbol)
    }

    pub(crate) fn variables_with_components(&self) -> impl Iterator<Item = (Component, &Symbol)> {
        [
            (Component::Entity, &self.entity),
            (Component::Attribute, &self.attribute),
            (Component::Value, &self.value),
        ]
        .into_iter()
        .filter_map(|(component, term)| term.variable().map(|symbol| (component, symbol)))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Component {
    Entity,
    Attribute,
    Value,
}

/// Position within a raw Datalog pattern, including the non-binding timestamp slot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PatternPosition {
    Entity,
    Attribute,
    Value,
    CreatedAt,
}

impl From<usize> for PatternPosition {
    fn from(value: usize) -> Self {
        match value {
            0 => Self::Entity,
            1 => Self::Attribute,
            2 => Self::Value,
            3 => Self::CreatedAt,
            _ => unreachable!("raw pattern component count is checked before conversion"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BindingPath {
    pub pattern_index: usize,
    pub triple_index: usize,
    pub component: Component,
    pub reference_value: bool,
}

impl BindingPath {
    #[must_use]
    pub const fn new(pattern_index: usize, component: Component, index: IndexKey) -> Self {
        let triple_index = match component {
            Component::Entity => 0,
            Component::Attribute => 1,
            Component::Value => 2,
        };
        Self {
            pattern_index,
            triple_index,
            component,
            // The JVM's join planner only applies reference-value binding
            // conversion to reverse-index (`:vae`) paths. `:eav` constants are
            // UUID-typed for lookup/topics but its variable joins remain JSON.
            reference_value: matches!(component, Component::Value)
                && matches!(index, IndexKey::Vae),
        }
    }
}

/// Recursive binding locations produced while accumulating nested `or` branches.
///
/// Clojure represents a path as a map, conjunctions as sequential collections, and
/// alternatives as sets. Rust names those cases explicitly so invalid shapes cannot
/// enter join planning.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum BindingTree {
    Path(BindingPath),
    And(Vec<BindingTree>),
    Or(Vec<BindingTree>),
}

pub type NestedSymbolMap = HashMap<Symbol, Vec<BindingTree>>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SqlColumn {
    EntityId,
    AttrId,
    Value,
}

impl SqlColumn {
    const fn from_component(component: Component) -> Self {
        match component {
            Component::Entity => Self::EntityId,
            Component::Attribute => Self::AttrId,
            Component::Value => Self::Value,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::EntityId => "entity-id",
            Self::AttrId => "attr-id",
            Self::Value => "value",
        }
    }
}

/// A column in the current pattern CTE or a qualified column in a previous CTE.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ColumnRef {
    pub qualifier: Option<String>,
    pub column: SqlColumn,
}

impl ColumnRef {
    fn origin(binding: &BindingPath) -> Self {
        Self {
            qualifier: None,
            column: SqlColumn::from_component(binding.component),
        }
    }

    fn qualified(prefix: &str, binding: &BindingPath) -> Self {
        Self {
            qualifier: Some(format!(
                "{}{pattern_index}",
                prefix.trim_start_matches(':'),
                pattern_index = binding.pattern_index
            )),
            column: SqlColumn::from_component(binding.component),
        }
    }

    fn honey_sql_keyword(&self) -> Value {
        let name = match &self.qualifier {
            Some(qualifier) => format!(":{qualifier}-{}", self.column.as_str()),
            None => format!(":{}", self.column.as_str()),
        };
        Value::String(name)
    }
}

/// The subset of `HoneySQL` expressions needed to describe Datalog joins.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum SqlExpression {
    Column(ColumnRef),
    JsonUuidToUuid(Box<SqlExpression>),
}

impl SqlExpression {
    fn binding_column(prefix: &str, binding: &BindingPath, qualified: bool) -> Self {
        let column = if qualified {
            ColumnRef::qualified(prefix, binding)
        } else {
            ColumnRef::origin(binding)
        };
        let expression = Self::Column(column);
        if binding.reference_value {
            Self::JsonUuidToUuid(Box::new(expression))
        } else {
            expression
        }
    }

    /// Serializes to the JSON analogue of the Clojure `HoneySQL` expression.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        match self {
            Self::Column(column) => column.honey_sql_keyword(),
            Self::JsonUuidToUuid(argument) => {
                serde_json::json!([":json_uuid_to_uuid", argument.to_honey_sql_json()])
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JoinCondition {
    pub left: SqlExpression,
    pub right: SqlExpression,
}

impl JoinCondition {
    fn between(prefix: &str, origin: &BindingPath, destination: &BindingPath) -> Self {
        Self::between_with_qualification(prefix, origin, destination, false)
    }

    fn between_qualified(prefix: &str, origin: &BindingPath, destination: &BindingPath) -> Self {
        Self::between_with_qualification(prefix, origin, destination, true)
    }

    fn between_with_qualification(
        prefix: &str,
        origin: &BindingPath,
        destination: &BindingPath,
        qualify_origin: bool,
    ) -> Self {
        Self {
            left: SqlExpression::binding_column(prefix, origin, qualify_origin),
            right: SqlExpression::binding_column(prefix, destination, true),
        }
    }

    /// Serializes to the JSON analogue of Clojure's `[:= left right]` form.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        serde_json::json!([
            ":=",
            self.left.to_honey_sql_json(),
            self.right.to_honey_sql_json()
        ])
    }
}

/// Boolean join predicate generated from a recursive binding tree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum JoinPredicate {
    Equals(JoinCondition),
    And(Vec<JoinPredicate>),
    Or(Vec<JoinPredicate>),
}

impl JoinPredicate {
    /// Serializes to the JSON analogue of the equivalent Clojure `HoneySQL` form.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        match self {
            Self::Equals(condition) => condition.to_honey_sql_json(),
            Self::And(predicates) => {
                let mut form = Vec::with_capacity(predicates.len() + 1);
                form.push(Value::String(":and".to_owned()));
                form.extend(Self::forms(predicates));
                Value::Array(form)
            }
            Self::Or(predicates) => {
                let mut form = Vec::with_capacity(predicates.len() + 1);
                form.push(Value::String(":or".to_owned()));
                form.extend(Self::forms(predicates));
                Value::Array(form)
            }
        }
    }

    fn forms(predicates: &[Self]) -> impl Iterator<Item = Value> + '_ {
        predicates.iter().map(Self::to_honey_sql_json)
    }
}

fn join_origin_to_tree(
    prefix: &str,
    origin: &BindingPath,
    destination: &BindingTree,
) -> JoinPredicate {
    match destination {
        BindingTree::Path(path) => {
            JoinPredicate::Equals(JoinCondition::between(prefix, origin, path))
        }
        BindingTree::And(paths) => JoinPredicate::And(
            paths
                .iter()
                .map(|path| join_origin_to_tree(prefix, origin, path))
                .collect(),
        ),
        BindingTree::Or(paths) => JoinPredicate::Or(
            paths
                .iter()
                .map(|path| join_origin_to_tree(prefix, origin, path))
                .collect(),
        ),
    }
}

/// Generates joins against ordinary and nested `or` symbol bindings.
#[must_use]
pub fn nested_join_predicates(
    prefix: &str,
    pattern_index: usize,
    symbol_map: &NestedSymbolMap,
    pattern: &Pattern,
) -> Vec<JoinPredicate> {
    let mut predicates = Vec::new();
    for (component, term) in [
        (Component::Entity, &pattern.entity),
        (Component::Attribute, &pattern.attribute),
        (Component::Value, &pattern.value),
    ] {
        let Some(symbol) = term.variable() else {
            continue;
        };
        let Some(destinations) = symbol_map.get(symbol) else {
            continue;
        };
        let origin = BindingPath::new(pattern_index, component, pattern.index.key);
        predicates.extend(
            destinations
                .iter()
                .map(|destination| join_origin_to_tree(prefix, &origin, destination)),
        );
    }
    predicates
}

/// Merges branch-local symbol maps using Clojure's `[(set xs)]` representation.
///
/// Symbols present in multiple branches become one [`BindingTree::Or`] containing
/// an [`BindingTree::And`] for each branch. Symbols present in only one branch retain
/// their original binding list.
#[must_use]
pub fn merge_or_symbol_maps(branches: &[NestedSymbolMap]) -> NestedSymbolMap {
    let mut occurrences: HashMap<Symbol, Vec<Vec<BindingTree>>> = HashMap::new();
    for branch in branches {
        for (symbol, paths) in branch {
            occurrences
                .entry(symbol.clone())
                .or_default()
                .push(paths.clone());
        }
    }

    occurrences
        .into_iter()
        .map(|(symbol, branch_paths)| {
            let paths = if branch_paths.len() == 1 {
                branch_paths.into_iter().next().unwrap_or_default()
            } else {
                vec![BindingTree::Or(
                    branch_paths.into_iter().map(BindingTree::And).collect(),
                )]
            };
            (symbol, paths)
        })
        .collect()
}

/// Recursively joins two path trees for an `or` gather CTE.
///
/// Both sides are qualified because the gather joins already-materialized CTEs.
#[must_use]
pub fn gather_join_predicates(
    prefix: &str,
    origin: &BindingTree,
    destination: &BindingTree,
) -> Vec<JoinPredicate> {
    match (origin, destination) {
        (BindingTree::Path(origin), BindingTree::Path(destination)) => {
            vec![JoinPredicate::Equals(JoinCondition::between_qualified(
                prefix,
                origin,
                destination,
            ))]
        }
        (BindingTree::Or(origins), _) => vec![JoinPredicate::Or(
            origins
                .iter()
                .flat_map(|origin| gather_join_predicates(prefix, origin, destination))
                .collect(),
        )],
        (_, BindingTree::Or(destinations)) => vec![JoinPredicate::Or(
            destinations
                .iter()
                .flat_map(|destination| gather_join_predicates(prefix, origin, destination))
                .collect(),
        )],
        (BindingTree::Path(_), BindingTree::And(destinations)) => {
            vec![JoinPredicate::And(
                destinations
                    .iter()
                    .flat_map(|destination| gather_join_predicates(prefix, origin, destination))
                    .collect(),
            )]
        }
        (BindingTree::And(origins), _) => vec![JoinPredicate::And(
            origins
                .iter()
                .flat_map(|origin| gather_join_predicates(prefix, origin, destination))
                .collect(),
        )],
    }
}

/// Connects every `or` branch to the bindings accumulated before the `or` group.
#[must_use]
pub fn or_gather_predicate(
    prefix: &str,
    symbol_map: &NestedSymbolMap,
    branch_symbol_maps: &[NestedSymbolMap],
    join_symbol: &Symbol,
) -> Option<JoinPredicate> {
    let destination_paths = symbol_map.get(join_symbol)?;
    let destination = BindingTree::And(destination_paths.clone());
    let mut branches = Vec::new();
    for branch_symbol_map in branch_symbol_maps {
        let Some(origin_paths) = branch_symbol_map.get(join_symbol) else {
            continue;
        };
        let origin = BindingTree::And(origin_paths.clone());
        let predicates = gather_join_predicates(prefix, &origin, &destination);
        if !predicates.is_empty() {
            branches.push(JoinPredicate::And(predicates));
        }
    }
    (!branches.is_empty()).then_some(JoinPredicate::Or(branches))
}

/// Generates join conditions for one pattern against bindings from earlier CTEs.
#[must_use]
pub fn join_conditions<S: BuildHasher>(
    prefix: &str,
    pattern_index: usize,
    symbol_map: &HashMap<Symbol, Vec<BindingPath>, S>,
    pattern: &Pattern,
) -> Vec<JoinCondition> {
    let mut conditions = Vec::new();
    for (component, term) in [
        (Component::Entity, &pattern.entity),
        (Component::Attribute, &pattern.attribute),
        (Component::Value, &pattern.value),
    ] {
        let Some(symbol) = term.variable() else {
            continue;
        };
        let Some(destinations) = symbol_map.get(symbol) else {
            continue;
        };
        let origin = BindingPath::new(pattern_index, component, pattern.index.key);
        conditions.extend(
            destinations
                .iter()
                .map(|destination| JoinCondition::between(prefix, &origin, destination)),
        );
    }
    conditions
}

/// Plans joins for a linear list of normalized patterns in CTE order.
#[must_use]
pub fn plan_join_conditions(prefix: &str, patterns: &[Pattern]) -> Vec<Vec<JoinCondition>> {
    let Some((first, rest)) = patterns.split_first() else {
        return Vec::new();
    };

    let mut symbol_map = first.symbol_map(0);
    let mut planned = Vec::with_capacity(rest.len());
    for (offset, pattern) in rest.iter().enumerate() {
        let pattern_index = offset + 1;
        planned.push(join_conditions(prefix, pattern_index, &symbol_map, pattern));
        for (symbol, paths) in pattern.symbol_map(pattern_index) {
            symbol_map.entry(symbol).or_default().extend(paths);
        }
    }
    planned
}

/// Ensures every pattern is transitively connected to a constant-bearing pattern.
///
/// This is the Rust equivalent of `instant.db.datalog/throw-invalid-patterns`.
///
/// # Errors
///
/// Returns [`PatternError::EmptyQuery`] for no patterns, or
/// [`PatternError::PatternNotJoinable`] for the first unanchored pattern.
pub fn validate_joinable(patterns: &[Pattern]) -> Result<(), PatternError> {
    let patterns = patterns.iter().collect::<Vec<_>>();
    validate_pattern_refs_joinable(&patterns, &[])
}

/// Ensures a recursive clause tree is transitively connected to constants or
/// bindings supplied by its parent nested-query group.
///
/// The reference implementation substitutes each parent join symbol with a
/// placeholder UUID before flattening `and`/`or` groups. Treating those symbols
/// as graph anchors is equivalent and avoids mutating the normalized query.
///
/// # Errors
///
/// Returns [`PatternError::EmptyQuery`] when the clause tree has no patterns, or
/// [`PatternError::PatternNotJoinable`] for the first unanchored flattened pattern.
pub fn validate_clause_joinable(
    clauses: &[NormalizedClause],
    anchored_symbols: &[Symbol],
) -> Result<(), PatternError> {
    let mut patterns = Vec::new();
    collect_clause_patterns(clauses, &mut patterns);
    validate_pattern_refs_joinable(&patterns, anchored_symbols)
}

fn collect_clause_patterns<'a>(clauses: &'a [NormalizedClause], patterns: &mut Vec<&'a Pattern>) {
    for clause in clauses {
        match clause {
            NormalizedClause::Pattern { pattern } => patterns.push(pattern),
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                collect_clause_patterns(clauses, patterns);
            }
        }
    }
}

fn validate_pattern_refs_joinable(
    patterns: &[&Pattern],
    anchored_symbols: &[Symbol],
) -> Result<(), PatternError> {
    if patterns.is_empty() {
        return Err(PatternError::EmptyQuery);
    }

    let mut symbol_map: HashMap<Symbol, Vec<BindingPath>> = HashMap::new();
    for (pattern_index, pattern) in patterns.iter().enumerate() {
        for (symbol, paths) in pattern.symbol_map(pattern_index) {
            symbol_map.entry(symbol).or_default().extend(paths);
        }
    }

    for pattern_index in 0..patterns.len() {
        if !pattern_is_anchored(pattern_index, patterns, &symbol_map, anchored_symbols) {
            return Err(PatternError::PatternNotJoinable { pattern_index });
        }
    }
    Ok(())
}

fn pattern_is_anchored(
    pattern_index: usize,
    patterns: &[&Pattern],
    symbol_map: &HashMap<Symbol, Vec<BindingPath>>,
    anchored_symbols: &[Symbol],
) -> bool {
    let mut visited = vec![false; patterns.len()];
    let mut pending = vec![pattern_index];
    while let Some(current) = pending.pop() {
        if visited[current] {
            continue;
        }
        visited[current] = true;
        if patterns[current].has_constant()
            || patterns[current]
                .variables()
                .any(|symbol| anchored_symbols.contains(symbol))
        {
            return true;
        }
        for symbol in patterns[current].variables() {
            if let Some(paths) = symbol_map.get(symbol) {
                pending.extend(
                    paths
                        .iter()
                        .map(|path| path.pattern_index)
                        .filter(|next| !visited[*next]),
                );
            }
        }
    }
    false
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum CoarseTerm {
    Any,
    Constants(Vec<Value>),
}

impl CoarseTerm {
    fn from_term(term: &Term) -> Self {
        match term {
            Term::Any | Term::Variable(_) => Self::Any,
            Term::Constants(values) => Self::Constants(values.clone()),
            Term::Function(function) => {
                Self::Constants(vec![serde_json::to_value(function).unwrap_or(Value::Null)])
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CoarseTopic {
    pub indexes: Vec<IndexKey>,
    pub entity: CoarseTerm,
    pub attribute: CoarseTerm,
    pub value: CoarseTerm,
    pub created_at: CoarseTerm,
}

/// Nested clauses mirror Datalog's `:and` and `:or` maps.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Clause {
    Pattern {
        pattern: RawPattern,
    },
    And {
        clauses: Vec<Clause>,
    },
    Or {
        join_symbol: Symbol,
        clauses: Vec<Clause>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NormalizedClause {
    Pattern {
        pattern: Pattern,
    },
    And {
        clauses: Vec<NormalizedClause>,
    },
    Or {
        join_symbol: Symbol,
        clauses: Vec<NormalizedClause>,
    },
}

impl Clause {
    /// Normalizes every raw pattern in a recursive clause tree.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError`] when any raw pattern cannot be normalized.
    pub fn normalize(&self) -> Result<NormalizedClause, PatternError> {
        match self {
            Self::Pattern { pattern } => Ok(NormalizedClause::Pattern {
                pattern: pattern.clone().normalize()?,
            }),
            Self::And { clauses } => Ok(NormalizedClause::And {
                clauses: clauses
                    .iter()
                    .map(Self::normalize)
                    .collect::<Result<_, _>>()?,
            }),
            Self::Or {
                join_symbol,
                clauses,
            } => Ok(NormalizedClause::Or {
                join_symbol: join_symbol.clone(),
                clauses: clauses
                    .iter()
                    .map(Self::normalize)
                    .collect::<Result<_, _>>()?,
            }),
        }
    }

    /// Flattens nested groups into the coarse topics used by the invalidator.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError`] when any nested raw pattern cannot be normalized.
    pub fn coarse_topics(&self) -> Result<Vec<CoarseTopic>, PatternError> {
        match self {
            Self::Pattern { pattern } => Ok(vec![pattern.clone().normalize()?.coarse_topic()]),
            Self::And { clauses } | Self::Or { clauses, .. } => {
                let mut topics = Vec::new();
                for clause in clauses {
                    topics.extend(clause.coarse_topics()?);
                }
                Ok(topics)
            }
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PatternError {
    #[error("unknown Datalog index `{0}`")]
    UnknownIndex(String),
    #[error("query symbols must be non-empty and cannot be `_`: `{0}`")]
    InvalidSymbol(String),
    #[error("a Datalog query must contain at least one pattern")]
    EmptyQuery,
    #[error("a Datalog pattern has {actual} components; at most {COMPONENT_COUNT} are allowed")]
    TooManyComponents { actual: usize },
    #[error("invalid raw Datalog term at {position:?}: `{value}`")]
    InvalidRawTerm {
        position: PatternPosition,
        value: String,
    },
    #[error("Datalog pattern {pattern_index} is not joinable")]
    PatternNotJoinable { pattern_index: usize },
    #[error("invalid {component:?} constant `{value}`")]
    InvalidConstant { component: Component, value: String },
    #[error("entity function `$not` requires a UUID, got `{0}`")]
    InvalidEntityNot(String),
    #[error("an `or` clause must contain at least one branch")]
    EmptyOrClause,
    #[error("pagination order symbol `{0}` is not present in its ordering pattern")]
    MissingPageOrderSymbol(String),
    #[error("pagination entity symbol `{0}` has no binding in the preceding query")]
    MissingPageEntityBinding(String),
    #[error("pagination plan has no prior created-at result column")]
    MissingPageCreatedAtColumn,
    #[error("pagination limit is too large to overfetch safely")]
    PageLimitOverflow,
    #[error("nested join symbol `{0}` has no binding in its parent pattern group")]
    MissingNestedJoinSymbol(String),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn symbol(value: &str) -> Symbol {
        Symbol::new(value).unwrap()
    }

    #[test]
    fn normalizes_constants_and_pads_missing_components() {
        let pattern = RawPattern::new(
            Index::new(IndexKey::Av),
            vec![
                RawTerm::Any,
                RawTerm::Variable(symbol("?attr")),
                RawTerm::Constant(json!(5)),
            ],
        )
        .normalize()
        .unwrap();

        assert_eq!(pattern.entity, Term::Any);
        assert_eq!(pattern.attribute, Term::Variable(symbol("?attr")));
        assert_eq!(pattern.value, Term::Constants(vec![json!(5)]));
        assert_eq!(pattern.created_at, Term::Any);
    }

    #[test]
    fn an_index_only_pattern_becomes_all_wildcards() {
        let pattern = RawPattern::new(Index::new(IndexKey::Vae), vec![])
            .normalize()
            .unwrap();

        assert_eq!(
            pattern,
            Pattern {
                index: Index::new(IndexKey::Vae),
                entity: Term::Any,
                attribute: Term::Any,
                value: Term::Any,
                created_at: Term::Any,
            }
        );
    }

    #[test]
    fn constants_have_set_semantics() {
        assert_eq!(
            Term::constants([json!("Jack"), json!("Jill"), json!("Jack")]),
            Term::Constants(vec![json!("Jack"), json!("Jill")])
        );
    }

    #[test]
    fn builds_the_same_binding_paths_as_clojure() {
        let pattern = RawPattern::new(
            Index::new(IndexKey::Vae),
            vec![
                RawTerm::Variable(symbol("?a")),
                RawTerm::Variable(symbol("?b")),
                RawTerm::Variable(symbol("?c")),
            ],
        )
        .normalize()
        .unwrap();

        let bindings = pattern.symbol_map(7);
        assert_eq!(
            bindings[&symbol("?a")],
            vec![BindingPath {
                pattern_index: 7,
                triple_index: 0,
                component: Component::Entity,
                reference_value: false,
            }]
        );
        assert!(bindings[&symbol("?c")][0].reference_value);
    }

    #[test]
    fn coarse_topics_flatten_nested_boolean_groups() {
        let clause = Clause::Or {
            join_symbol: symbol("?e"),
            clauses: vec![
                Clause::And {
                    clauses: vec![Clause::Pattern {
                        pattern: RawPattern::new(
                            Index::new(IndexKey::Eav),
                            vec![
                                RawTerm::Variable(symbol("?e")),
                                RawTerm::Variable(symbol("?a")),
                                RawTerm::Constant(json!("Jack")),
                            ],
                        ),
                    }],
                },
                Clause::Pattern {
                    pattern: RawPattern::new(
                        Index::new(IndexKey::Eav),
                        vec![
                            RawTerm::Variable(symbol("?e")),
                            RawTerm::Variable(symbol("?a")),
                            RawTerm::Constant(json!("Jill")),
                        ],
                    ),
                },
            ],
        };

        let topics = clause.coarse_topics().unwrap();
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].value, CoarseTerm::Constants(vec![json!("Jack")]));
        assert_eq!(topics[1].value, CoarseTerm::Constants(vec![json!("Jill")]));
        assert_eq!(topics[0].entity, CoarseTerm::Any);
    }

    #[test]
    fn rejects_invalid_input_early() {
        assert_eq!(IndexKey::from_str(":eav").unwrap(), IndexKey::Eav);
        assert!(matches!(
            IndexKey::from_str(":wat"),
            Err(PatternError::UnknownIndex(_))
        ));
        assert!(Symbol::new("_").is_err());
        assert!(matches!(
            RawPattern::new(
                Index::new(IndexKey::Ea),
                vec![RawTerm::Any; COMPONENT_COUNT + 1]
            )
            .normalize(),
            Err(PatternError::TooManyComponents { actual: 5 })
        ));
    }

    #[test]
    fn validates_every_raw_component_against_the_clojure_pattern_spec() {
        let entity_id = Uuid::new_v4();
        let attribute_id = Uuid::new_v4();
        let valid = RawPattern::new(
            Index::checked(IndexKey::Ave, DataType::Number),
            vec![
                RawTerm::Constants(vec![
                    json!(entity_id),
                    json!([attribute_id, "lookup-value"]),
                ]),
                RawTerm::Constant(json!(attribute_id)),
                RawTerm::Function(FunctionClause::Comparator(ComparatorClause {
                    operation: ComparatorOperation::GreaterThan,
                    data_type: DataType::Number,
                    value: json!(10),
                })),
                RawTerm::Constants(vec![json!(1), json!(2.5)]),
            ],
        );
        assert!(valid.normalize().is_ok());

        let cases = [
            (
                PatternPosition::Entity,
                vec![RawTerm::Constant(json!("not-a-uuid"))],
            ),
            (
                PatternPosition::Attribute,
                vec![RawTerm::Any, RawTerm::Constant(json!(12))],
            ),
            (
                PatternPosition::CreatedAt,
                vec![
                    RawTerm::Any,
                    RawTerm::Any,
                    RawTerm::Any,
                    RawTerm::Constant(json!("yesterday")),
                ],
            ),
        ];
        for (position, components) in cases {
            assert!(matches!(
                RawPattern::new(Index::new(IndexKey::Ea), components).normalize(),
                Err(PatternError::InvalidRawTerm {
                    position: actual,
                    ..
                }) if actual == position
            ));
        }

        assert!(matches!(
            RawPattern::new(
                Index::new(IndexKey::Ea),
                vec![RawTerm::Function(FunctionClause::Not(json!("not-a-uuid")))]
            )
            .normalize(),
            Err(PatternError::InvalidEntityNot(_))
        ));
        assert!(matches!(
            RawPattern::new(
                Index::new(IndexKey::Ea),
                vec![
                    RawTerm::Any,
                    RawTerm::Function(FunctionClause::Not(json!(attribute_id)))
                ]
            )
            .normalize(),
            Err(PatternError::InvalidRawTerm {
                position: PatternPosition::Attribute,
                ..
            })
        ));
    }
}
