use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    ComparatorOperation, Component, DataType, FunctionClause, Index, IndexKey, PatternError, Term,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PgType {
    Bigint,
    Jsonb,
    Uuid,
    TimestampWithTimeZone,
    Float8,
    Text,
    Boolean,
    CheckedDataType,
}

impl PgType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Bigint => "bigint",
            Self::Jsonb => "jsonb",
            Self::Uuid => "uuid",
            Self::TimestampWithTimeZone => "timestamptz",
            Self::Float8 => "float8",
            Self::Text => "text",
            Self::Boolean => "boolean",
            Self::CheckedDataType => "checked_data_type",
        }
    }

    fn array_name(self) -> String {
        format!("{}[]", self.as_str())
    }
}

impl DataType {
    const fn pg_type(self) -> PgType {
        match self {
            Self::Date => PgType::TimestampWithTimeZone,
            Self::Number => PgType::Float8,
            Self::String => PgType::Text,
            Self::Boolean => PgType::Boolean,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Date => "date",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WhereColumn {
    AppId,
    EntityId,
    AttrId,
    Value,
    CreatedAt,
    CheckedDataType,
    Index(IndexKey),
}

impl WhereColumn {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AppId => "app-id",
            Self::EntityId => "entity-id",
            Self::AttrId => "attr-id",
            Self::Value => "value",
            Self::CreatedAt => "created-at",
            Self::CheckedDataType => "checked_data_type",
            Self::Index(index) => match index {
                IndexKey::Ea => "ea",
                IndexKey::Eav => "eav",
                IndexKey::Av => "av",
                IndexKey::Ave => "ave",
                IndexKey::Vae => "vae",
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WhereColumnRef {
    pub qualifier: Option<String>,
    pub column: WhereColumn,
}

impl WhereColumnRef {
    #[must_use]
    pub fn unqualified(column: WhereColumn) -> Self {
        Self {
            qualifier: None,
            column,
        }
    }

    #[must_use]
    pub fn qualified(qualifier: impl Into<String>, column: WhereColumn) -> Self {
        Self {
            qualifier: Some(qualifier.into()),
            column,
        }
    }

    fn honey_sql_keyword(&self) -> Value {
        let column = self.column.as_str();
        Value::String(match &self.qualifier {
            Some(qualifier) => format!(":{qualifier}.{column}"),
            None => format!(":{column}"),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SqlFunction {
    JsonNullToNull,
    JsonUuidToUuid,
    ExtractDateValue,
    ExtractNumberValue,
    ExtractStringValue,
    ExtractBooleanValue,
}

impl SqlFunction {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::JsonNullToNull => "json_null_to_null",
            Self::JsonUuidToUuid => "json_uuid_to_uuid",
            Self::ExtractDateValue => "triples_extract_date_value",
            Self::ExtractNumberValue => "triples_extract_number_value",
            Self::ExtractStringValue => "triples_extract_string_value",
            Self::ExtractBooleanValue => "triples_extract_boolean_value",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum WhereExpression {
    Column(WhereColumnRef),
    NamedColumn(String),
    Literal(Value),
    Uuid(Uuid),
    Wildcard(Option<String>),
    Inline(String),
    Cast {
        expression: Box<WhereExpression>,
        pg_type: PgType,
    },
    Function {
        function: SqlFunction,
        arguments: Vec<WhereExpression>,
    },
    Coalesce(Vec<WhereExpression>),
    Any {
        values: Vec<Value>,
        element_type: PgType,
    },
    Subquery(Box<SelectQuery>),
}

impl WhereExpression {
    #[must_use]
    pub fn column(column: WhereColumn) -> Self {
        Self::Column(WhereColumnRef::unqualified(column))
    }

    #[must_use]
    pub fn qualified_column(qualifier: impl Into<String>, column: WhereColumn) -> Self {
        Self::Column(WhereColumnRef::qualified(qualifier, column))
    }

    #[must_use]
    pub fn function(function: SqlFunction, argument: Self) -> Self {
        Self::Function {
            function,
            arguments: vec![argument],
        }
    }

    fn cast_literal(value: Value, pg_type: PgType) -> Self {
        Self::Cast {
            expression: Box::new(Self::Literal(value)),
            pg_type,
        }
    }

    /// Serializes into a stable JSON analogue of the corresponding `HoneySQL` form.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        match self {
            Self::Column(column) => column.honey_sql_keyword(),
            Self::NamedColumn(column) => Value::String(format!(":{column}")),
            Self::Literal(value) => value.clone(),
            Self::Uuid(value) => Value::String(value.to_string()),
            Self::Wildcard(qualifier) => Value::String(match qualifier {
                Some(qualifier) => format!(":{qualifier}.*"),
                None => ":*".to_owned(),
            }),
            Self::Inline(value) => json!([":inline", value]),
            Self::Cast {
                expression,
                pg_type,
            } => json!([
                ":cast",
                expression.to_honey_sql_json(),
                format!(":{}", pg_type.as_str())
            ]),
            Self::Function {
                function,
                arguments,
            } => {
                let mut form = vec![Value::String(format!(":{}", function.as_str()))];
                form.extend(arguments.iter().map(Self::to_honey_sql_json));
                Value::Array(form)
            }
            Self::Coalesce(expressions) => {
                let mut form = vec![Value::String(":coalesce".to_owned())];
                form.extend(expressions.iter().map(Self::to_honey_sql_json));
                Value::Array(form)
            }
            Self::Any {
                values,
                element_type,
            } => json!([
                ":any",
                {
                    "values": values,
                    "pgtype": element_type.array_name()
                }
            ]),
            Self::Subquery(query) => query.to_honey_sql_json(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PredicateOperator {
    #[serde(rename = "=")]
    Equal,
    #[serde(rename = "not=")]
    NotEqual,
    #[serde(rename = "is-distinct-from")]
    IsDistinctFrom,
    #[serde(rename = ">")]
    GreaterThan,
    #[serde(rename = ">=")]
    GreaterThanOrEqual,
    #[serde(rename = "<")]
    LessThan,
    #[serde(rename = "<=")]
    LessThanOrEqual,
    #[serde(rename = "like")]
    Like,
    #[serde(rename = "ilike")]
    CaseInsensitiveLike,
}

impl PredicateOperator {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::NotEqual => "not=",
            Self::IsDistinctFrom => "is-distinct-from",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => ">=",
            Self::LessThan => "<",
            Self::LessThanOrEqual => "<=",
            Self::Like => "like",
            Self::CaseInsensitiveLike => "ilike",
        }
    }
}

impl From<ComparatorOperation> for PredicateOperator {
    fn from(value: ComparatorOperation) -> Self {
        match value {
            ComparatorOperation::GreaterThan => Self::GreaterThan,
            ComparatorOperation::GreaterThanOrEqual => Self::GreaterThanOrEqual,
            ComparatorOperation::LessThan => Self::LessThan,
            ComparatorOperation::LessThanOrEqual => Self::LessThanOrEqual,
            ComparatorOperation::Like => Self::Like,
            ComparatorOperation::CaseInsensitiveLike => Self::CaseInsensitiveLike,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum WherePredicate {
    Compare {
        operator: PredicateOperator,
        left: WhereExpression,
        right: WhereExpression,
    },
    And(Vec<WherePredicate>),
    Or(Vec<WherePredicate>),
    In {
        negated: bool,
        expression: WhereExpression,
        query: SelectQuery,
    },
    Exists {
        negated: bool,
        query: SelectQuery,
    },
    /// Positive EXISTS with an OFFSET 0 optimization fence. This retains an
    /// entity-correlated probe instead of allowing PostgreSQL to pull it into
    /// a whole-partition semi join ahead of an ordered LIMIT. It skips no rows.
    ExistsProbe {
        query: SelectQuery,
    },
    Truthy(WhereExpression),
}

impl WherePredicate {
    pub(crate) fn compare(
        operator: PredicateOperator,
        left: WhereExpression,
        right: WhereExpression,
    ) -> Self {
        Self::Compare {
            operator,
            left,
            right,
        }
    }

    pub(crate) fn equal(left: WhereExpression, right: WhereExpression) -> Self {
        Self::compare(PredicateOperator::Equal, left, right)
    }

    fn false_clause() -> Self {
        Self::equal(
            WhereExpression::Literal(json!(0)),
            WhereExpression::Literal(json!(1)),
        )
    }

    /// Serializes into a stable JSON analogue of the corresponding `HoneySQL` form.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        match self {
            Self::Compare {
                operator,
                left,
                right,
            } => json!([
                format!(":{}", operator.as_str()),
                left.to_honey_sql_json(),
                right.to_honey_sql_json()
            ]),
            Self::And(predicates) => boolean_form(":and", predicates),
            Self::Or(predicates) => boolean_form(":or", predicates),
            Self::In {
                negated,
                expression,
                query,
            } => json!([
                if *negated { ":not-in" } else { ":in" },
                expression.to_honey_sql_json(),
                query.to_honey_sql_json()
            ]),
            Self::Exists { negated, query } => {
                let exists = json!([":exists", query.to_honey_sql_json()]);
                if *negated {
                    json!([":not", exists])
                } else {
                    exists
                }
            }
            Self::ExistsProbe { query } => {
                let mut query = query.to_honey_sql_json();
                query[":offset"] = json!(0);
                json!([":exists", query])
            }
            Self::Truthy(expression) => expression.to_honey_sql_json(),
        }
    }
}

fn boolean_form(operator: &str, predicates: &[WherePredicate]) -> Value {
    let mut form = Vec::with_capacity(predicates.len() + 1);
    form.push(Value::String(operator.to_owned()));
    form.extend(predicates.iter().map(WherePredicate::to_honey_sql_json));
    Value::Array(form)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SelectQuery {
    pub select: Vec<WhereExpression>,
    pub table: String,
    pub alias: String,
    pub predicate: Box<WherePredicate>,
}

impl SelectQuery {
    /// Serializes into the stable JSON analogue used by compatibility fixtures.
    #[must_use]
    pub fn to_honey_sql_json(&self) -> Value {
        json!({
            ":select": self.select.iter().map(WhereExpression::to_honey_sql_json).collect::<Vec<_>>(),
            ":from": [[format!(":{}", self.table), format!(":{}", self.alias)]],
            ":where": self.predicate.to_honey_sql_json()
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum QueryHint {
    IndexScan { table_alias: String, index: String },
    BitmapScan { table_alias: String, index: String },
    Rows { tables: Vec<String>, estimate: u64 },
    DisableJit,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PredicatePlan {
    pub predicate: WherePredicate,
    pub hints: Vec<QueryHint>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PredicateOptions {
    pub disable_is_null_fix: bool,
    /// Use a semijoin/antijoin for forward null checks. The legacy default
    /// preserves the reference SQL; production can opt in with live rollback.
    #[serde(default)]
    pub forward_is_null_exists: bool,
}

/// Builds an index-aware predicate for a normalized constant component.
///
/// # Errors
///
/// Returns [`PatternError::InvalidConstant`] when an entity is neither a UUID nor
/// a two-element lookup reference beginning with an attribute UUID, or when an
/// attribute constant is not a UUID.
pub fn constant_predicate(
    triples_alias: &str,
    index: Index,
    app_id: Uuid,
    component: Component,
    values: &[Value],
) -> Result<PredicatePlan, PatternError> {
    match component {
        Component::Entity => entity_constant_predicate(triples_alias, app_id, values),
        Component::Attribute => attribute_constant_predicate(values),
        Component::Value => Ok(PredicatePlan {
            predicate: value_constants_predicate(index, values),
            hints: Vec::new(),
        }),
    }
}

fn entity_constant_predicate(
    triples_alias: &str,
    app_id: Uuid,
    values: &[Value],
) -> Result<PredicatePlan, PatternError> {
    if values.iter().all(json_uuid).then_some(()).is_some() {
        return Ok(PredicatePlan {
            predicate: in_any(
                WhereExpression::column(WhereColumn::EntityId),
                values,
                PgType::Uuid,
            ),
            hints: Vec::new(),
        });
    }

    let lookup_alias = format!("{triples_alias}-lookup");
    let mut predicates = Vec::with_capacity(values.len());
    for value in values {
        if let Some(entity_id) = parse_json_uuid(value) {
            predicates.push(WherePredicate::equal(
                WhereExpression::column(WhereColumn::EntityId),
                WhereExpression::Uuid(entity_id),
            ));
            continue;
        }
        let Some((attr_id, lookup_value)) = lookup_ref(value) else {
            return Err(PatternError::InvalidConstant {
                component: Component::Entity,
                value: value.to_string(),
            });
        };
        let subquery = SelectQuery {
            select: vec![WhereExpression::qualified_column(
                &lookup_alias,
                WhereColumn::EntityId,
            )],
            table: "triples".to_owned(),
            alias: lookup_alias.clone(),
            predicate: Box::new(WherePredicate::And(vec![
                WherePredicate::equal(
                    WhereExpression::column(WhereColumn::AppId),
                    WhereExpression::Uuid(app_id),
                ),
                WherePredicate::equal(
                    WhereExpression::function(
                        SqlFunction::JsonNullToNull,
                        WhereExpression::column(WhereColumn::Value),
                    ),
                    WhereExpression::cast_literal(lookup_value.clone(), PgType::Jsonb),
                ),
                WherePredicate::equal(
                    WhereExpression::column(WhereColumn::AttrId),
                    WhereExpression::cast_literal(Value::String(attr_id.to_string()), PgType::Uuid),
                ),
                WherePredicate::Truthy(WhereExpression::column(WhereColumn::Index(IndexKey::Av))),
            ])),
        };
        predicates.push(WherePredicate::equal(
            WhereExpression::column(WhereColumn::EntityId),
            WhereExpression::Subquery(Box::new(subquery)),
        ));
    }

    Ok(PredicatePlan {
        predicate: WherePredicate::Or(predicates),
        hints: vec![QueryHint::IndexScan {
            table_alias: lookup_alias,
            index: "av_index".to_owned(),
        }],
    })
}

fn attribute_constant_predicate(values: &[Value]) -> Result<PredicatePlan, PatternError> {
    for value in values {
        if !json_uuid(value) {
            return Err(PatternError::InvalidConstant {
                component: Component::Attribute,
                value: value.to_string(),
            });
        }
    }
    let predicate = match values {
        [] => WherePredicate::false_clause(),
        [value] => {
            let attr_id = parse_json_uuid(value).ok_or_else(|| PatternError::InvalidConstant {
                component: Component::Attribute,
                value: value.to_string(),
            })?;
            WherePredicate::equal(
                WhereExpression::column(WhereColumn::AttrId),
                WhereExpression::Uuid(attr_id),
            )
        }
        _ => WherePredicate::equal(
            WhereExpression::column(WhereColumn::AttrId),
            WhereExpression::Any {
                values: values.to_vec(),
                element_type: PgType::Uuid,
            },
        ),
    };
    Ok(PredicatePlan {
        predicate,
        hints: Vec::new(),
    })
}

fn value_constants_predicate(index: Index, values: &[Value]) -> WherePredicate {
    if values.is_empty() {
        return WherePredicate::false_clause();
    }
    if let Some(data_type) = index.data_type {
        return data_type_comparison(
            data_type,
            PredicateOperator::Equal,
            WhereExpression::column(WhereColumn::Value),
            ComparisonValue::Set(values),
        );
    }

    let column = match index.key {
        IndexKey::Av => WhereExpression::function(
            SqlFunction::JsonNullToNull,
            WhereExpression::column(WhereColumn::Value),
        ),
        IndexKey::Eav | IndexKey::Vae => WhereExpression::function(
            SqlFunction::JsonUuidToUuid,
            WhereExpression::column(WhereColumn::Value),
        ),
        IndexKey::Ea | IndexKey::Ave => WhereExpression::column(WhereColumn::Value),
    };
    let pg_type =
        if matches!(index.key, IndexKey::Eav | IndexKey::Vae) && values.iter().all(json_uuid) {
            PgType::Uuid
        } else {
            PgType::Jsonb
        };
    in_any(column, values, pg_type)
}

fn in_any(column: WhereExpression, values: &[Value], pg_type: PgType) -> WherePredicate {
    match values {
        [] => WherePredicate::false_clause(),
        [value] => WherePredicate::equal(
            column,
            WhereExpression::cast_literal(value.clone(), pg_type),
        ),
        _ => WherePredicate::equal(
            column,
            WhereExpression::Any {
                values: values.to_vec(),
                element_type: pg_type,
            },
        ),
    }
}

#[derive(Clone, Copy)]
enum ComparisonValue<'a> {
    Scalar(&'a Value),
    Set(&'a [Value]),
}

fn data_type_comparison(
    data_type: DataType,
    operator: PredicateOperator,
    column: WhereExpression,
    value: ComparisonValue<'_>,
) -> WherePredicate {
    let extraction = extraction_function(data_type, operator);
    let comparison = if let Some(function) = extraction {
        let extracted = WhereExpression::function(function, column);
        match value {
            ComparisonValue::Set(values) if operator == PredicateOperator::Equal => {
                let non_null: Vec<_> = values
                    .iter()
                    .filter(|value| !value.is_null())
                    .cloned()
                    .collect();
                let in_clause = in_any(extracted.clone(), &non_null, data_type.pg_type());
                if values.iter().any(Value::is_null) {
                    WherePredicate::Or(vec![
                        in_clause,
                        WherePredicate::equal(WhereExpression::Literal(Value::Null), extracted),
                    ])
                } else {
                    in_clause
                }
            }
            ComparisonValue::Set(values) => WherePredicate::compare(
                operator,
                extracted,
                WhereExpression::Literal(Value::Array(values.to_vec())),
            ),
            ComparisonValue::Scalar(value) => WherePredicate::compare(
                operator,
                extracted,
                WhereExpression::Literal(value.clone()),
            ),
        }
    } else {
        match value {
            ComparisonValue::Set(values) if operator == PredicateOperator::Equal => {
                in_any(column, values, PgType::Jsonb)
            }
            ComparisonValue::Set(values) => WherePredicate::compare(
                operator,
                column,
                WhereExpression::cast_literal(Value::Array(values.to_vec()), PgType::Jsonb),
            ),
            ComparisonValue::Scalar(value) => WherePredicate::compare(
                operator,
                column,
                WhereExpression::cast_literal(value.clone(), PgType::Jsonb),
            ),
        }
    };

    if extraction.is_some() {
        WherePredicate::And(vec![
            comparison,
            WherePredicate::equal(
                WhereExpression::column(WhereColumn::CheckedDataType),
                WhereExpression::Cast {
                    expression: Box::new(WhereExpression::Inline(data_type.as_str().to_owned())),
                    pg_type: PgType::CheckedDataType,
                },
            ),
        ])
    } else {
        comparison
    }
}

fn extraction_function(data_type: DataType, operator: PredicateOperator) -> Option<SqlFunction> {
    match data_type {
        DataType::Date => Some(SqlFunction::ExtractDateValue),
        DataType::Number => Some(SqlFunction::ExtractNumberValue),
        DataType::String
            if matches!(
                operator,
                PredicateOperator::Like | PredicateOperator::CaseInsensitiveLike
            ) =>
        {
            Some(SqlFunction::ExtractStringValue)
        }
        DataType::String => None,
        DataType::Boolean => Some(SqlFunction::ExtractBooleanValue),
    }
}

/// Builds value- and entity-function predicates for one normalized pattern.
///
/// # Errors
///
/// Returns [`PatternError::InvalidEntityNot`] when an entity `$not` clause does not
/// contain a UUID.
pub fn function_predicates(
    options: PredicateOptions,
    app_id: Uuid,
    triples_alias: &str,
    index: Index,
    entity: &Term,
    value: &Term,
) -> Result<Vec<WherePredicate>, PatternError> {
    let mut predicates = Vec::new();
    if let Term::Function(function) = value {
        predicates.extend(value_function_predicates(
            options,
            app_id,
            triples_alias,
            index,
            function,
        ));
    }
    if let Term::Function(FunctionClause::Not(entity_id)) = entity {
        let entity_id = parse_json_uuid(entity_id)
            .ok_or_else(|| PatternError::InvalidEntityNot(entity_id.to_string()))?;
        predicates.push(WherePredicate::compare(
            PredicateOperator::NotEqual,
            WhereExpression::column(WhereColumn::EntityId),
            WhereExpression::Uuid(entity_id),
        ));
    }
    Ok(predicates)
}

fn value_function_predicates(
    options: PredicateOptions,
    app_id: Uuid,
    triples_alias: &str,
    index: Index,
    function: &FunctionClause,
) -> Vec<WherePredicate> {
    match function {
        FunctionClause::Not(value) => vec![not_equal_value(index, value)],
        FunctionClause::Comparator(comparator) => vec![data_type_comparison(
            comparator.data_type,
            comparator.operation.into(),
            WhereExpression::column(WhereColumn::Value),
            ComparisonValue::Scalar(&comparator.value),
        )],
        FunctionClause::EntityIdStartsWith(prefix) => {
            let start = prefix_uuid(prefix, '0');
            let end = prefix_uuid(prefix, 'f');
            vec![WherePredicate::And(vec![
                WherePredicate::compare(
                    PredicateOperator::GreaterThanOrEqual,
                    WhereExpression::column(WhereColumn::EntityId),
                    uuid_expression(start),
                ),
                WherePredicate::compare(
                    PredicateOperator::LessThanOrEqual,
                    WhereExpression::column(WhereColumn::EntityId),
                    uuid_expression(end),
                ),
            ])]
        }
        FunctionClause::IsNull(clause) => {
            vec![is_null_predicate(options, app_id, triples_alias, clause)]
        }
    }
}

fn not_equal_value(index: Index, value: &Value) -> WherePredicate {
    if let Some(data_type) = index.data_type {
        return data_type_comparison(
            data_type,
            PredicateOperator::IsDistinctFrom,
            WhereExpression::column(WhereColumn::Value),
            ComparisonValue::Scalar(value),
        );
    }
    let column = if index.key == IndexKey::Av {
        WhereExpression::function(
            SqlFunction::JsonNullToNull,
            WhereExpression::column(WhereColumn::Value),
        )
    } else {
        WhereExpression::column(WhereColumn::Value)
    };
    WherePredicate::compare(
        PredicateOperator::NotEqual,
        column,
        WhereExpression::cast_literal(value.clone(), PgType::Jsonb),
    )
}

fn is_null_predicate(
    options: PredicateOptions,
    app_id: Uuid,
    triples_alias: &str,
    clause: &crate::IsNullClause,
) -> WherePredicate {
    let reverse_reference = clause.reference && clause.reverse;
    let alias = format!("{triples_alias}-subquery");
    let alias_value = WhereExpression::qualified_column(&alias, WhereColumn::Value);
    let mut predicates = vec![WherePredicate::equal(
        WhereExpression::qualified_column(&alias, WhereColumn::AppId),
        WhereExpression::Uuid(app_id),
    )];

    let relation = if options.disable_is_null_fix {
        WherePredicate::equal(
            WhereExpression::qualified_column(&alias, WhereColumn::EntityId),
            WhereExpression::column(WhereColumn::EntityId),
        )
    } else if reverse_reference {
        WherePredicate::equal(
            WhereExpression::function(SqlFunction::JsonUuidToUuid, alias_value.clone()),
            WhereExpression::qualified_column(triples_alias, WhereColumn::EntityId),
        )
    } else {
        WherePredicate::equal(
            WhereExpression::qualified_column(&alias, WhereColumn::EntityId),
            WhereExpression::qualified_column(triples_alias, WhereColumn::EntityId),
        )
    };
    predicates.push(relation);
    predicates.push(WherePredicate::equal(
        WhereExpression::qualified_column(&alias, WhereColumn::AttrId),
        WhereExpression::Uuid(clause.attr_id),
    ));
    if reverse_reference {
        predicates.push(WherePredicate::Truthy(WhereExpression::qualified_column(
            &alias,
            WhereColumn::Index(IndexKey::Eav),
        )));
    }
    if let Some(data_type) = clause.indexed_checked_type {
        predicates.push(WherePredicate::Truthy(WhereExpression::qualified_column(
            &alias,
            WhereColumn::Index(IndexKey::Ave),
        )));
        predicates.push(data_type_comparison(
            data_type,
            PredicateOperator::NotEqual,
            alias_value.clone(),
            ComparisonValue::Scalar(&Value::Null),
        ));
    } else {
        predicates.push(WherePredicate::compare(
            PredicateOperator::NotEqual,
            alias_value.clone(),
            WhereExpression::cast_literal(Value::Null, PgType::Jsonb),
        ));
    }

    let select = if reverse_reference {
        vec![WhereExpression::function(
            SqlFunction::JsonUuidToUuid,
            alias_value,
        )]
    } else {
        vec![WhereExpression::qualified_column(
            &alias,
            WhereColumn::EntityId,
        )]
    };
    let query = SelectQuery {
        select,
        table: "triples".to_owned(),
        alias,
        predicate: Box::new(WherePredicate::And(predicates)),
    };
    if options.forward_is_null_exists && !options.disable_is_null_fix && !reverse_reference {
        // Forward checks select non-null entity_id and already correlate it to
        // the outer non-null entity_id. EXISTS is therefore the same membership
        // test, but PostgreSQL can plan a semijoin/antijoin instead of running a
        // correlated IN SubPlan once per historical entity. Retain every guard
        // and the correlation; reverse refs and the legacy fix stay untouched.
        WherePredicate::Exists {
            negated: clause.nil,
            query,
        }
    } else {
        WherePredicate::In {
            negated: clause.nil,
            expression: WhereExpression::column(WhereColumn::EntityId),
            query,
        }
    }
}

fn prefix_uuid(prefix: &str, fill: char) -> Option<Uuid> {
    let template = if fill == '0' {
        "00000000-0000-0000-0000-000000000000"
    } else {
        "ffffffff-ffff-ffff-ffff-ffffffffffff"
    };
    let candidate = if prefix.len() >= template.len() {
        Cow::Borrowed(prefix)
    } else {
        Cow::Owned(format!("{prefix}{}", template.get(prefix.len()..)?))
    };
    Uuid::parse_str(candidate.trim()).ok()
}

fn uuid_expression(value: Option<Uuid>) -> WhereExpression {
    value.map_or(WhereExpression::Literal(Value::Null), WhereExpression::Uuid)
}

fn json_uuid(value: &Value) -> bool {
    parse_json_uuid(value).is_some()
}

fn parse_json_uuid(value: &Value) -> Option<Uuid> {
    let value = value.as_str()?;
    (value.len() == 36)
        .then(|| Uuid::parse_str(value.trim()).ok())
        .flatten()
}

fn lookup_ref(value: &Value) -> Option<(Uuid, &Value)> {
    let values = value.as_array()?;
    let [attr_id, lookup_value] = values.as_slice() else {
        return None;
    };
    Some((Uuid::parse_str(attr_id.as_str()?).ok()?, lookup_value))
}
use std::borrow::Cow;
