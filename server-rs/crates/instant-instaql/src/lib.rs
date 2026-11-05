//! Typed coercion and Datalog compilation for `InstantDB`'s public `InstaQL` object shape.

use std::hash::BuildHasher;

use instant_datalog::{
    DataType, DatalogResult, Index, IndexKey, MatchQueryContext, MatchQueryPlan, NormalizedClause,
    OrderDirection, PageCursor, PageInfo, PageOrderType, PatternError, RawPattern, RawTerm, Symbol,
    add_page_info, missing_attribute_result, plan_match_query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

mod candidate_entities;
mod dates;
mod invalidation_predicate;
mod nested;
mod ordered_candidates;
mod query_hash;
mod system_catalog;
mod where_compiler;

pub use dates::parse_date_millis;
pub use invalidation_predicate::*;
pub use nested::*;
use query_hash::clojure_map_entries;
pub use query_hash::{normalized_query_hash, validate_query_numbers};
pub use system_catalog::*;
pub use where_compiler::*;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cardinality {
    One,
    Many,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AttributeValueType {
    Blob,
    Ref,
}

/// Source metadata required to reproduce the persisted attribute wire shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttributeWireMetadata {
    pub forward_identity_id: Uuid,
    pub reverse_identity_id: Option<Uuid>,
    pub inferred_types: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Mirrors persisted attribute lifecycle flags.
pub struct Attribute {
    pub id: Uuid,
    #[serde(skip)]
    pub wire: Option<AttributeWireMetadata>,
    pub etype: String,
    pub label: String,
    pub value_type: AttributeValueType,
    pub cardinality: Cardinality,
    pub unique: bool,
    #[serde(default)]
    pub setting_unique: bool,
    pub indexed: bool,
    pub required: bool,
    pub checked_data_type: Option<DataType>,
    pub indexing: bool,
    pub checking_data_type: bool,
    pub reverse_etype: Option<String>,
    pub reverse_label: Option<String>,
    #[serde(default)]
    pub on_delete: Option<DeleteAction>,
    #[serde(default)]
    pub on_delete_reverse: Option<DeleteAction>,
}

impl Attribute {
    /// Synthetic catalogs historically use the attribute ID as their identity ID.
    #[must_use]
    pub fn forward_identity_id(&self) -> Uuid {
        self.wire
            .as_ref()
            .map_or(self.id, |metadata| metadata.forward_identity_id)
    }

    #[must_use]
    pub fn reverse_identity_id(&self) -> Option<Uuid> {
        self.reverse_etype.as_ref()?;
        self.reverse_label.as_ref()?;
        Some(
            self.wire
                .as_ref()
                .and_then(|metadata| metadata.reverse_identity_id)
                .unwrap_or(self.id),
        )
    }

    #[must_use]
    pub fn inferred_types(&self) -> Option<&[String]> {
        self.wire
            .as_ref()
            .and_then(|metadata| metadata.inferred_types.as_deref())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttributeCatalog {
    pub attributes: Vec<Attribute>,
}

impl AttributeCatalog {
    #[must_use]
    pub fn forward(&self, etype: &str, label: &str) -> Option<&Attribute> {
        self.attributes
            .iter()
            .find(|attribute| attribute.etype == etype && attribute.label == label)
    }

    #[must_use]
    pub fn by_id(&self, id: Uuid) -> Option<&Attribute> {
        self.attributes.iter().find(|attribute| attribute.id == id)
    }

    #[must_use]
    pub fn reverse(&self, etype: &str, label: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|attribute| {
            attribute.reverse_etype.as_deref() == Some(etype)
                && attribute.reverse_label.as_deref() == Some(label)
        })
    }

    #[must_use]
    pub fn link_etype(&self, etype: &str, label: &str) -> Option<&str> {
        self.forward(etype, label)
            .and_then(|attribute| attribute.reverse_etype.as_deref())
            .or_else(|| {
                self.attributes.iter().find_map(|attribute| {
                    (attribute.reverse_etype.as_deref() == Some(etype)
                        && attribute.reverse_label.as_deref() == Some(label))
                    .then_some(attribute.etype.as_str())
                })
            })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub enum QueryPathSegment {
    Index(usize),
    Field(String),
}

impl From<String> for QueryPathSegment {
    fn from(value: String) -> Self {
        Self::Field(value)
    }
}

impl From<usize> for QueryPathSegment {
    fn from(value: usize) -> Self {
        Self::Index(value)
    }
}

impl PartialEq<&str> for QueryPathSegment {
    fn eq(&self, other: &&str) -> bool {
        matches!(self, Self::Field(value) if value == other)
    }
}

impl PartialEq<String> for QueryPathSegment {
    fn eq(&self, other: &String) -> bool {
        matches!(self, Self::Field(value) if value == other)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ValidationError {
    pub expected: String,
    pub path: Vec<QueryPathSegment>,
    pub message: String,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InstaqlError {
    #[error("InstaQL validation failed: {0:?}")]
    Validation(Vec<ValidationError>),
    #[error(transparent)]
    Datalog(#[from] PatternError),
    #[error("a missing-attribute query has no PostgreSQL match plan")]
    MissingAttributePlan,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregate {
    Count,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Cursor {
    Normal {
        entity_id: Uuid,
        attr_id: Uuid,
        value: Value,
        created_at: i64,
    },
    Byop {
        entity_id: Value,
        attr_id: Uuid,
        value: Value,
    },
}

impl Cursor {
    #[must_use]
    pub const fn attr_id(&self) -> Uuid {
        match self {
            Self::Normal { attr_id, .. } | Self::Byop { attr_id, .. } => *attr_id,
        }
    }

    #[must_use]
    pub fn value(&self) -> &Value {
        match self {
            Self::Normal { value, .. } | Self::Byop { value, .. } => value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Order {
    pub field: String,
    pub direction: OrderDirection,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct FormOptions {
    pub where_clause: Vec<WhereCondition>,
    pub order: Option<Order>,
    pub limit: Option<u64>,
    pub first: Option<u64>,
    pub last: Option<u64>,
    pub offset: Option<u64>,
    pub before: Option<Cursor>,
    pub after: Option<Cursor>,
    pub before_inclusive: Option<bool>,
    pub after_inclusive: Option<bool>,
    pub aggregate: Option<Aggregate>,
    pub fields: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Form {
    pub key: String,
    pub etype: String,
    pub level: usize,
    pub options: FormOptions,
    pub children: Vec<Form>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompiledRoot {
    pub form: Form,
    pub clauses: Vec<NormalizedClause>,
    pub missing: bool,
    pub page: Option<PageInfo>,
    pub aggregate: Option<Aggregate>,
    pub selected_attr_ids: Vec<Uuid>,
}

impl CompiledRoot {
    /// Returns the synthetic empty Datalog result for a missing-attribute root.
    #[must_use]
    pub fn missing_result(&self) -> Option<DatalogResult> {
        self.missing
            .then(|| missing_attribute_result(&self.clauses))
    }

    /// Produces the executable lower-level Datalog plan for this root form.
    ///
    /// # Errors
    ///
    /// Returns [`InstaqlError::MissingAttributePlan`] for a sentinel query, or
    /// [`InstaqlError::Datalog`] when clause or page planning fails.
    pub fn plan<S: BuildHasher>(
        &self,
        context: &MatchQueryContext<'_, S>,
    ) -> Result<MatchQueryPlan, InstaqlError> {
        if self.missing {
            return Err(InstaqlError::MissingAttributePlan);
        }
        let mut plan = plan_match_query(context, &self.clauses)?;
        if let Some(page) = &self.page {
            add_page_info(&mut plan, context, page)?;
        }
        Ok(plan)
    }
}

/// Coerces the public query object into typed forms using JVM-compatible option rules.
///
/// # Errors
///
/// Returns all validation errors discovered before attribute-dependent compilation.
pub fn coerce_forms(query: &Value, catalog: &AttributeCatalog) -> Result<Vec<Form>, InstaqlError> {
    let root = object(query, &[], "map?").map_err(|error| InstaqlError::Validation(vec![error]))?;
    let mut forms = Vec::new();
    let mut errors = Vec::new();
    for (form_index, (key, value)) in clojure_map_entries(root)
        .into_iter()
        .filter(|(key, _)| key.as_str() != "$$ruleParams")
        .enumerate()
    {
        let path = vec![key.clone()];
        match coerce_form(
            key,
            value,
            key,
            0,
            catalog,
            &path,
            &[QueryPathSegment::Index(form_index)],
        ) {
            Ok(form) => forms.push(form),
            Err(mut form_errors) => errors.append(&mut form_errors),
        }
    }
    if errors.is_empty() {
        Ok(forms)
    } else {
        if errors.iter().any(|error| !error.message.is_empty()) {
            errors.retain(|error| !error.message.is_empty());
        }
        if errors.iter().all(|error| error.message.is_empty())
            && let Some(error) = errors
                .iter()
                .max_by_key(|error| (error.path.len(), error.path.last().cloned()))
                .cloned()
        {
            return Err(InstaqlError::Validation(vec![error]));
        }
        Err(InstaqlError::Validation(errors))
    }
}

/// Compiles root forms into all-ID Datalog scans plus validated pagination metadata.
///
/// `where` coercion is retained on the typed form but is rejected here until the
/// relationship-aware condition compiler is selected, preventing silent broad scans.
///
/// # Errors
///
/// Returns validation errors for unknown entity IDs, unsupported order attributes,
/// invalid cursor provenance, non-admin aggregates, aggregate children, or `where`.
pub fn compile_roots(
    query: &Value,
    catalog: &AttributeCatalog,
    admin: bool,
) -> Result<Vec<CompiledRoot>, InstaqlError> {
    let mut roots = Vec::new();
    for form in coerce_forms(query, catalog)? {
        let root = compile_root(form, catalog).map_err(InstaqlError::Validation)?;
        validate_aggregate(&root.form, admin, std::slice::from_ref(&root.form.key))
            .map_err(|error| InstaqlError::Validation(vec![error]))?;
        roots.push(root);
    }
    Ok(roots)
}

fn coerce_form(
    key: &str,
    value: &Value,
    etype: &str,
    level: usize,
    catalog: &AttributeCatalog,
    path: &[String],
    spec_path: &[QueryPathSegment],
) -> Result<Form, Vec<ValidationError>> {
    let form = object(value, path, "map?").map_err(|error| vec![error])?;
    let option_path = appended(path, "$".to_owned());
    let mut errors = Vec::new();
    let options = match form.get("$") {
        Some(options) => {
            match coerce_options(options, level, etype, catalog, &option_path, spec_path) {
                Ok(options) => Some(options),
                Err(mut option_errors) => {
                    errors.append(&mut option_errors);
                    None
                }
            }
        }
        None => Some(FormOptions::default()),
    };
    let mut children = Vec::new();
    for (child_index, (child_key, child_value)) in clojure_map_entries(form)
        .into_iter()
        .filter(|(key, _)| key.as_str() != "$")
        .enumerate()
    {
        let child_etype = catalog
            .link_etype(etype, child_key)
            .unwrap_or(child_key)
            .to_owned();
        match coerce_form(
            child_key,
            child_value,
            &child_etype,
            level + 1,
            catalog,
            &appended(path, child_key.clone()),
            &spec_path
                .iter()
                .cloned()
                .chain([
                    QueryPathSegment::Field("child-forms".to_owned()),
                    QueryPathSegment::Index(child_index),
                ])
                .collect::<Vec<_>>(),
        ) {
            Ok(child) => children.push(child),
            Err(mut child_errors) => errors.append(&mut child_errors),
        }
    }
    let Some(options) = options.filter(|_| errors.is_empty()) else {
        return Err(errors);
    };
    Ok(Form {
        key: key.to_owned(),
        etype: etype.to_owned(),
        level,
        options,
        children,
    })
}

fn collect_validation<T>(
    result: Result<T, ValidationError>,
    errors: &mut Vec<ValidationError>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

#[allow(clippy::too_many_lines)] // Mirrors the public option-map coercion boundary.
fn coerce_options(
    value: &Value,
    level: usize,
    etype: &str,
    catalog: &AttributeCatalog,
    path: &[String],
    spec_form_path: &[QueryPathSegment],
) -> Result<FormOptions, Vec<ValidationError>> {
    let options = object(value, path, "map?").map_err(|error| vec![error])?;
    let mut errors = Vec::new();
    let supported = [
        "where",
        "order",
        "limit",
        "first",
        "last",
        "offset",
        "before",
        "after",
        "beforeInclusive",
        "afterInclusive",
        "aggregate",
        "fields",
    ];
    let unknown: Vec<_> = options
        .keys()
        .filter(|key| !supported.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        errors.push(validation(
            "supported-options?",
            path,
            "We only support `where`, `order`, `limit`, `offset`, `before`, and `after` clauses.",
        ));
    }
    let limit = collect_validation(positive_option(options, "limit", path), &mut errors).flatten();
    let first = collect_validation(positive_option(options, "first", path), &mut errors).flatten();
    let last = collect_validation(positive_option(options, "last", path), &mut errors).flatten();
    let provided: Vec<_> = [("`limit`", limit), ("`first`", first), ("`last`", last)]
        .into_iter()
        .filter_map(|(name, value)| value.map(|_| name))
        .collect();
    if provided.len() > 1 {
        errors.push(validation(
            "supported-options?",
            path,
            format!("Only provide one of {}.", provided.join(" or ")),
        ));
    }
    let offset = options
        .get("offset")
        .filter(|value| clojure_truthy(value))
        .and_then(|value| collect_validation(nonnegative(value, "offset", path), &mut errors));
    let before = options
        .get("before")
        .filter(|value| clojure_truthy(value))
        .and_then(|value| collect_validation(cursor(value, "before", path), &mut errors));
    let after = options
        .get("after")
        .filter(|value| clojure_truthy(value))
        .and_then(|value| collect_validation(cursor(value, "after", path), &mut errors));
    if level > 0 && (offset.is_some() || before.is_some() || after.is_some()) {
        errors.push(validation(
            "supported-options?",
            path,
            "We currently only support `offset`, `before`, and `after` clauses on the top-level field. Limit fields will be ignored",
        ));
    }
    let before_inclusive = collect_validation(
        boolean_option(options, "beforeInclusive", path),
        &mut errors,
    )
    .flatten()
    .filter(|inclusive| *inclusive);
    let after_inclusive =
        collect_validation(boolean_option(options, "afterInclusive", path), &mut errors)
            .flatten()
            .filter(|inclusive| *inclusive);
    let fields = options
        .get("fields")
        .filter(|value| clojure_truthy(value))
        .and_then(|value| collect_validation(fields_array(value, path), &mut errors));
    let aggregate = options
        .get("aggregate")
        .filter(|value| clojure_truthy(value))
        .and_then(|value| {
            if value == "count" {
                Some(Aggregate::Count)
            } else {
                errors.push(validation(
                    "supported-options?",
                    &appended(path, "aggregate".to_owned()),
                    format!("The aggregate field only accepts \"count\", got {value}."),
                ));
                None
            }
        });
    let where_clause = options
        .get("where")
        .filter(|value| clojure_truthy(value))
        .and_then(|value| {
            collect_validation(
                coerce_where(
                    value,
                    &appended(path, "where".to_owned()),
                    etype,
                    catalog,
                    &spec_form_path
                        .iter()
                        .cloned()
                        .chain([
                            QueryPathSegment::Field("option-map".to_owned()),
                            QueryPathSegment::Field("where-conds".to_owned()),
                        ])
                        .collect::<Vec<_>>(),
                ),
                &mut errors,
            )
        })
        .unwrap_or_default();
    let order = options
        .get("order")
        .filter(|value| clojure_truthy(value))
        .and_then(|value| collect_validation(order(value, path), &mut errors))
        .flatten();
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(FormOptions {
        where_clause,
        order,
        limit,
        first,
        last,
        offset,
        before,
        after,
        before_inclusive,
        after_inclusive,
        aggregate,
        fields,
    })
}

fn compile_root(
    form: Form,
    catalog: &AttributeCatalog,
) -> Result<CompiledRoot, Vec<ValidationError>> {
    let entity_symbol = level_symbol(&form.etype, form.level).map_err(|error| vec![error])?;
    let Some(id_attr) = catalog.forward(&form.etype, "id") else {
        return missing_root(form, entity_symbol).map_err(|error| vec![error]);
    };
    let clauses = if form.options.where_clause.is_empty() {
        let pattern = RawPattern::new(
            Index::new(IndexKey::Ea),
            vec![
                RawTerm::Variable(entity_symbol.clone()),
                RawTerm::Constant(Value::String(id_attr.id.to_string())),
            ],
        )
        .normalize()
        .map_err(|error| vec![datalog_validation(&error)])?;
        vec![NormalizedClause::Pattern { pattern }]
    } else {
        match compile_where(&form, catalog, &entity_symbol) {
            Ok(clauses) => clauses,
            Err(error) if error.expected == "known-attribute?" => {
                return missing_root(form, entity_symbol).map_err(|error| vec![error]);
            }
            Err(error) => return Err(vec![error]),
        }
    };
    let selected_attr_ids = selected_attrs(&form, catalog, id_attr.id);
    let page = page_info(&form, catalog, id_attr, entity_symbol)?;
    let aggregate = form.options.aggregate;
    Ok(CompiledRoot {
        form,
        clauses,
        missing: false,
        page,
        aggregate,
        selected_attr_ids,
    })
}

fn validate_aggregate(
    form: &Form,
    admin: bool,
    form_path: &[String],
) -> Result<(), ValidationError> {
    let option_path = appended(form_path, "$".to_owned());
    if form.options.aggregate.is_some() && !admin {
        return Err(validation(
            "admin?",
            &appended(&option_path, "aggregate".to_owned()),
            "Aggregates are currently only available for admin queries.",
        ));
    }
    if form.options.aggregate.is_some() && !form.children.is_empty() {
        return Err(validation(
            "valid-query?",
            &appended(&option_path, "aggregate".to_owned()),
            "You can not combine aggregates with child queries at this time.",
        ));
    }
    Ok(())
}

fn missing_root(form: Form, entity_symbol: Symbol) -> Result<CompiledRoot, ValidationError> {
    let clauses = missing_clauses(entity_symbol)?;
    let aggregate = form.options.aggregate;
    Ok(CompiledRoot {
        form,
        clauses,
        missing: true,
        page: None,
        aggregate,
        selected_attr_ids: Vec::new(),
    })
}

fn missing_clauses(entity_symbol: Symbol) -> Result<Vec<NormalizedClause>, ValidationError> {
    [
        RawPattern::new(
            Index::new(IndexKey::Ea),
            vec![RawTerm::Variable(entity_symbol)],
        ),
        RawPattern::new(Index::new(IndexKey::Eav), Vec::new()),
    ]
    .into_iter()
    .map(|pattern| {
        pattern
            .normalize()
            .map(|pattern| NormalizedClause::Pattern { pattern })
            .map_err(|error| datalog_validation(&error))
    })
    .collect()
}

fn selected_attrs(form: &Form, catalog: &AttributeCatalog, id_attr: Uuid) -> Vec<Uuid> {
    let Some(fields) = &form.options.fields else {
        return catalog
            .attributes
            .iter()
            .filter(|attribute| {
                attribute.etype == form.etype && attribute.value_type == AttributeValueType::Blob
            })
            .map(|attribute| attribute.id)
            .collect();
    };
    let mut ids = vec![id_attr];
    for field in fields {
        if let Some(attribute) = field
            .as_str()
            .and_then(|field| catalog.forward(&form.etype, field))
            && attribute.cardinality == Cardinality::One
            && !ids.contains(&attribute.id)
        {
            ids.push(attribute.id);
        }
    }
    if form.etype == FILES_ETYPE
        && let Some(url) = catalog.forward(FILES_ETYPE, "url")
        && ids.contains(&url.id)
        && let Some(location_id) = catalog.forward(FILES_ETYPE, "location-id")
        && !ids.contains(&location_id.id)
    {
        ids.push(location_id.id);
    }
    ids
}

fn page_info(
    form: &Form,
    catalog: &AttributeCatalog,
    id_attr: &Attribute,
    entity_symbol: Symbol,
) -> Result<Option<PageInfo>, Vec<ValidationError>> {
    let options = &form.options;
    let requested = options.limit.is_some()
        || options.first.is_some()
        || options.last.is_some()
        || options.offset.is_some()
        || options.before.is_some()
        || options.after.is_some()
        || options.order.is_some();
    if !requested || form.level != 0 {
        return Ok(None);
    }
    let (field, direction) = options.order.as_ref().map_or_else(
        || ("serverCreatedAt", OrderDirection::Asc),
        |order| (order.field.as_str(), order.direction),
    );
    let order_attr = if field == "serverCreatedAt" {
        id_attr
    } else {
        catalog.forward(&form.etype, field).ok_or_else(|| {
            vec![validation(
                "supported-order?",
                &[form.key.clone(), "$".to_owned(), "order".to_owned()],
                format!("There is no `{field}` attribute for {}.", form.etype),
            )]
        })?
    };
    if field != "serverCreatedAt" {
        validate_order_attr(order_attr, &form.key)?;
    }
    validate_cursor_attr(options.before.as_ref(), order_attr, catalog, form, "before")
        .map_err(|error| vec![error])?;
    validate_cursor_attr(options.after.as_ref(), order_attr, catalog, form, "after")
        .map_err(|error| vec![error])?;
    let order_symbol = Symbol::new(if field == "serverCreatedAt" {
        "?t-0".to_owned()
    } else {
        format!("?{field}-0")
    })
    .map_err(|error| vec![datalog_validation(&error)])?;
    let pattern = if field == "serverCreatedAt" {
        RawPattern::new(
            Index::new(IndexKey::Ea),
            vec![
                RawTerm::Variable(entity_symbol),
                RawTerm::Constant(Value::String(order_attr.id.to_string())),
                RawTerm::Any,
                RawTerm::Variable(order_symbol.clone()),
            ],
        )
    } else {
        RawPattern::new(
            Index::checked(
                IndexKey::Ave,
                order_attr.checked_data_type.expect("validated order type"),
            ),
            vec![
                RawTerm::Variable(entity_symbol),
                RawTerm::Constant(Value::String(order_attr.id.to_string())),
                RawTerm::Variable(order_symbol.clone()),
            ],
        )
    }
    .normalize()
    .map_err(|error| vec![datalog_validation(&error)])?;
    let cursor_value = |cursor: &Cursor| triple_store_page_cursor(cursor, field, form);
    let before = options
        .before
        .as_ref()
        .map(cursor_value)
        .transpose()
        .map_err(|error| vec![error])?;
    let after = options
        .after
        .as_ref()
        .map(cursor_value)
        .transpose()
        .map_err(|error| vec![error])?;
    Ok(Some(PageInfo {
        limit: options.limit.or(options.first).or(options.last),
        last: options.last.is_some(),
        offset: options.offset,
        direction,
        order_symbol,
        order_type: order_attr
            .checked_data_type
            .map_or(PageOrderType::CreatedAtTimestamp, PageOrderType::Checked),
        order_required: field == "serverCreatedAt" || order_attr.required,
        attr_id: order_attr.id,
        pattern,
        before,
        before_inclusive: options.before_inclusive.unwrap_or(false),
        after,
        after_inclusive: options.after_inclusive.unwrap_or(false),
    }))
}

fn triple_store_page_cursor(
    cursor: &Cursor,
    field: &str,
    form: &Form,
) -> Result<PageCursor, ValidationError> {
    let Cursor::Normal {
        entity_id,
        value,
        created_at,
        ..
    } = cursor
    else {
        return Err(validation(
            "valid-cursor?",
            &[form.key.clone(), "$".to_owned()],
            "Three-item cursors are only valid for Bring Your Own Postgres queries.",
        ));
    };
    Ok(PageCursor {
        entity_id: *entity_id,
        order_value: if field == "serverCreatedAt" {
            Value::from(*created_at)
        } else {
            value.clone()
        },
    })
}

fn validate_order_attr(attribute: &Attribute, root: &str) -> Result<(), Vec<ValidationError>> {
    let path = [root.to_owned(), "$".to_owned(), "order".to_owned()];
    let friendly = format!("{}.{}", attribute.etype, attribute.label);
    let mut messages = Vec::new();
    if attribute.checking_data_type {
        messages.push(format!(
            "The `{friendly}` attribute is still in the process of validating its type. It must finish before ordering by the attribute."
        ));
    }
    if attribute.indexing {
        messages.push(format!(
            "The `{friendly}` attribute is still in the process of indexing. It must finish before ordering by the attribute."
        ));
    }
    if !attribute.indexed {
        messages.push(format!(
            "The `{friendly}` attribute is not indexed. Only indexed and typed attributes can be used to order by."
        ));
    }
    if attribute.checked_data_type.is_none() {
        messages.push(format!(
            "The `{friendly}` attribute is not typed. Only typed and indexed attributes can be used to order by."
        ));
    }
    if attribute.cardinality != Cardinality::One {
        messages.push(format!(
            "The `{friendly}` attribute has cardinality `many`. Only attributes with cardinality `one` can be used to order by."
        ));
    }
    if messages.is_empty() {
        Ok(())
    } else {
        Err(messages
            .into_iter()
            .map(|message| validation("supported-order?", &path, message))
            .collect())
    }
}

fn validate_cursor_attr(
    cursor: Option<&Cursor>,
    order_attr: &Attribute,
    catalog: &AttributeCatalog,
    form: &Form,
    side: &str,
) -> Result<(), ValidationError> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    if cursor.attr_id() == order_attr.id {
        return Ok(());
    }
    let requested_order_label = order_label(order_attr);
    let cursor_label = catalog.by_id(cursor.attr_id()).map_or_else(
        || "a missing attribute".to_owned(),
        |attribute| format!("`{}`", order_label(attribute)),
    );
    Err(validation(
        "valid-cursor?",
        &[form.key.clone(), "$".to_owned(), side.to_owned()],
        format!(
            "Invalid {side} cursor. The query orders by `{requested_order_label}`, but the query that returned the cursor orders by {cursor_label}."
        ),
    ))
}

fn order_label(attribute: &Attribute) -> &str {
    if attribute.label == "id" {
        "serverCreatedAt"
    } else {
        &attribute.label
    }
}

fn order(value: &Value, path: &[String]) -> Result<Option<Order>, ValidationError> {
    let map = object(value, &appended(path, "order".to_owned()), "map?")?;
    if map.is_empty() {
        return Ok(None);
    }
    if map.len() != 1 {
        return Err(validation(
            "single-key?",
            &appended(path, "order".to_owned()),
            format!(
                "We only support a single key in the `order` clause. Got {}.",
                map.keys().cloned().collect::<Vec<_>>().join(",")
            ),
        ));
    }
    let (field, direction) = map.iter().next().expect("one order entry");
    let direction = match direction.as_str() {
        Some("asc") => OrderDirection::Asc,
        Some("desc") => OrderDirection::Desc,
        _ => {
            return Err(validation(
                "valid-direction?",
                &appended(&appended(path, "order".to_owned()), field.clone()),
                format!(
                    "We only support \"asc\" or \"desc\" in the `order` clause. Got {direction}."
                ),
            ));
        }
    };
    Ok(Some(Order {
        field: field.clone(),
        direction,
    }))
}

fn cursor(value: &Value, name: &str, path: &[String]) -> Result<Cursor, ValidationError> {
    let cursor_path = appended(path, name.to_owned());
    let Some(values) = value.as_array() else {
        return Err(validation(
            "join-row?",
            &cursor_path,
            format!("Expected a join row for the cursor, got {value}."),
        ));
    };
    if !matches!(values.len(), 3 | 4) {
        return Err(validation(
            "join-row?",
            &cursor_path,
            format!("Expected a join row with 4 items for the cursor, got {value}."),
        ));
    }
    let attr_id = values[1].as_str().and_then(coerce_public_uuid);
    let Some(attr_id) = attr_id else {
        return Err(validation(
            "join-row?",
            &cursor_path,
            format!(
                "Expected a join row with a uuid attribute id in the second position, got {}.",
                values[1]
            ),
        ));
    };
    if values.len() == 3 {
        let entity_id = values[0].as_str().and_then(coerce_public_uuid).map_or_else(
            || values[0].clone(),
            |value| Value::String(value.to_string()),
        );
        return Ok(Cursor::Byop {
            entity_id,
            attr_id,
            value: values[2].clone(),
        });
    }
    let entity_id = values[0].as_str().and_then(coerce_public_uuid);
    let Some(entity_id) = entity_id else {
        return Err(validation(
            "join-row?",
            &cursor_path,
            format!(
                "Expected a join row with a uuid entity id in the first position, got {}.",
                values[0]
            ),
        ));
    };
    let created_at = values[3].as_i64();
    let Some(created_at) = created_at else {
        return Err(validation(
            "join-row?",
            &cursor_path,
            format!(
                "Expected a join row with an integer created_at in the last position, got {}.",
                values[3]
            ),
        ));
    };
    Ok(Cursor::Normal {
        entity_id,
        attr_id,
        value: values[2].clone(),
        created_at,
    })
}

fn positive_option(
    options: &Map<String, Value>,
    name: &str,
    path: &[String],
) -> Result<Option<u64>, ValidationError> {
    options
        .get(name)
        .filter(|value| clojure_truthy(value))
        .map(|value| {
            value.as_u64().filter(|value| *value > 0).ok_or_else(|| {
                validation(
                    "supported-options?",
                    &appended(path, name.to_owned()),
                    format!("The {name} field must be a positive integer. Got {value}."),
                )
            })
        })
        .transpose()
}

fn clojure_truthy(value: &Value) -> bool {
    !value.is_null() && value != &Value::Bool(false)
}

fn coerce_public_uuid(value: &str) -> Option<Uuid> {
    let value = value.trim();
    (value.len() == 36)
        .then(|| Uuid::parse_str(value).ok())
        .flatten()
}

fn nonnegative(value: &Value, name: &str, path: &[String]) -> Result<u64, ValidationError> {
    value.as_u64().ok_or_else(|| {
        validation(
            "supported-options?",
            &appended(path, name.to_owned()),
            format!("The {name} field must be a non-negative integer. Got {value}."),
        )
    })
}

fn boolean_option(
    options: &Map<String, Value>,
    name: &str,
    path: &[String],
) -> Result<Option<bool>, ValidationError> {
    options
        .get(name)
        .filter(|value| !value.is_null())
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                validation(
                    "boolean?",
                    &appended(path, name.to_owned()),
                    format!("Expected a boolean, got {value}."),
                )
            })
        })
        .transpose()
}

fn fields_array(value: &Value, path: &[String]) -> Result<Vec<Value>, ValidationError> {
    value.as_array().cloned().ok_or_else(|| {
        validation(
            "array?",
            &appended(path, "fields".to_owned()),
            "The `fields` property should be an array of field names.",
        )
    })
}

fn object<'a>(
    value: &'a Value,
    path: &[String],
    expected: &str,
) -> Result<&'a Map<String, Value>, ValidationError> {
    value
        .as_object()
        .ok_or_else(|| validation(expected, path, format!("Expected an object, got {value}.")))
}

fn appended(path: &[String], segment: String) -> Vec<String> {
    path.iter().cloned().chain([segment]).collect()
}

fn validation(
    expected: impl Into<String>,
    path: &[String],
    message: impl Into<String>,
) -> ValidationError {
    ValidationError {
        expected: expected.into(),
        path: path.iter().cloned().map(QueryPathSegment::Field).collect(),
        message: message.into(),
    }
}

fn spec_validation(expected: impl Into<String>, path: Vec<QueryPathSegment>) -> ValidationError {
    ValidationError {
        expected: expected.into(),
        path,
        message: String::new(),
    }
}

fn datalog_validation(error: &PatternError) -> ValidationError {
    validation("valid-pattern?", &[], error.to_string())
}

fn level_symbol(etype: &str, level: usize) -> Result<Symbol, ValidationError> {
    Symbol::new(format!("?{etype}-{level}")).map_err(|error| datalog_validation(&error))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use instant_datalog::MatchCteOptions;
    use serde_json::json;

    use super::*;

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).unwrap()
    }

    fn catalog() -> AttributeCatalog {
        AttributeCatalog {
            attributes: vec![
                Attribute {
                    id: uuid("11111111-1111-1111-1111-111111111111"),
                    wire: None,
                    etype: "users".to_owned(),
                    label: "id".to_owned(),
                    value_type: AttributeValueType::Blob,
                    cardinality: Cardinality::One,
                    unique: true,
                    setting_unique: false,
                    indexed: true,
                    required: true,
                    checked_data_type: None,
                    indexing: false,
                    checking_data_type: false,
                    reverse_etype: None,
                    reverse_label: None,
                    on_delete: None,
                    on_delete_reverse: None,
                },
                Attribute {
                    id: uuid("22222222-2222-2222-2222-222222222222"),
                    wire: None,
                    etype: "users".to_owned(),
                    label: "handle".to_owned(),
                    value_type: AttributeValueType::Blob,
                    cardinality: Cardinality::One,
                    unique: true,
                    setting_unique: false,
                    indexed: true,
                    required: false,
                    checked_data_type: Some(DataType::String),
                    indexing: false,
                    checking_data_type: false,
                    reverse_etype: None,
                    reverse_label: None,
                    on_delete: None,
                    on_delete_reverse: None,
                },
            ],
        }
    }

    #[test]
    fn accumulates_coercion_errors_but_stops_at_the_first_compile_error() {
        let catalog = catalog();
        let query = json!({
            "posts": false,
            "users": {
                "$": {
                    "limit": 0,
                    "offset": -1,
                    "beforeInclusive": "yes"
                },
                "friends": {
                    "$": {
                        "first": 0,
                        "afterInclusive": 1
                    }
                }
            }
        });
        let InstaqlError::Validation(errors) = coerce_forms(&query, &catalog).unwrap_err() else {
            panic!("expected accumulated validation errors");
        };
        assert_eq!(errors.len(), 6);
        assert!(errors.iter().any(|error| error.path == ["posts"]));
        assert!(
            errors
                .iter()
                .any(|error| error.path == ["users", "$", "limit"])
        );
        assert!(
            errors
                .iter()
                .any(|error| error.path == ["users", "$", "offset"])
        );
        assert!(
            errors
                .iter()
                .any(|error| error.path == ["users", "friends", "$", "first"])
        );
        assert!(
            errors
                .iter()
                .any(|error| { error.path == ["users", "friends", "$", "afterInclusive"] })
        );

        let InstaqlError::Validation(errors) = compile_roots(
            &json!({
                "posts": {"$": {"aggregate": "count"}},
                "users": {"$": {"aggregate": "count"}}
            }),
            &catalog,
            false,
        )
        .unwrap_err() else {
            panic!("expected accumulated compilation errors");
        };
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].expected, "admin?");
        assert_eq!(errors[0].path, ["posts", "$", "aggregate"]);
    }

    #[test]
    fn compiles_default_and_checked_order_pages_into_datalog() {
        let catalog = catalog();
        let roots = compile_roots(
            &json!({"users": {"$": {"first": 10, "order": {"handle": "desc"}}}}),
            &catalog,
            false,
        )
        .unwrap();
        let root = &roots[0];
        let page = root.page.as_ref().unwrap();
        assert_eq!(page.limit, Some(10));
        assert_eq!(page.direction, OrderDirection::Desc);
        assert_eq!(page.order_type, PageOrderType::Checked(DataType::String));
        assert!(!page.order_required);
        let additional = HashMap::new();
        let options = MatchCteOptions::default();
        let context = MatchQueryContext {
            prefix: "query-",
            app_id: uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            additional_joins: &additional,
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        };
        let plan = root.plan(&context).unwrap();
        let page_plan = plan
            .page
            .as_ref()
            .expect("ordered query must carry page metadata");
        assert_eq!(plan.final_table, "query-2");
        assert_eq!(page_plan.table, plan.final_table);

        let optional_order_gather = plan
            .ctes
            .iter()
            .find_map(|cte| match cte {
                instant_datalog::PlannedCte::OrGather(gather)
                    if gather.name == "query-2-optional-source" =>
                {
                    Some(gather)
                }
                _ => None,
            })
            .expect("optional ordering must preserve the base scan through a gather CTE");
        assert_eq!(optional_order_gather.from.table, "query-0");
        assert_eq!(optional_order_gather.left_joins.len(), 1);
        assert_eq!(optional_order_gather.left_joins[0].table, "query-1");
    }

    #[test]
    fn enforces_limit_cursor_order_and_aggregate_contracts() {
        let catalog = catalog();
        let error = compile_roots(
            &json!({"users": {"$": {"limit": 2, "first": 1}}}),
            &catalog,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("Only provide one"));

        let error = compile_roots(
            &json!({"users": {"$": {"order": {"handle": "DESC"}}}}),
            &catalog,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("valid-direction"));

        let error = compile_roots(
            &json!({"users": {"$": {"aggregate": "count"}}}),
            &catalog,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("admin"));

        let aggregate = compile_roots(
            &json!({"users": {"$": {"aggregate": "count"}}}),
            &catalog,
            true,
        )
        .unwrap();
        assert_eq!(aggregate[0].aggregate, Some(Aggregate::Count));
        let additional = HashMap::new();
        let options = MatchCteOptions::default();
        let context = MatchQueryContext {
            prefix: "aggregate-",
            app_id: uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            additional_joins: &additional,
            initial_symbol_cardinalities: &HashMap::new(),
            options: &options,
        };
        let plan = aggregate[0].plan(&context).unwrap();
        let rendered = instant_datalog::render_count_query(&plan).unwrap();
        assert!(
            rendered
                .sql
                .ends_with("SELECT count(*) AS \"count\" FROM \"aggregate_0\"")
        );
    }

    #[test]
    fn coerces_three_field_byop_cursors_but_rejects_them_from_triple_store_plans() {
        let catalog = catalog();
        let attr_id = uuid("22222222-2222-2222-2222-222222222222");
        let forms = coerce_forms(
            &json!({
                "users": {"$": {
                    "order": {"handle": "asc"},
                    "after": [42, attr_id, "oren"]
                }}
            }),
            &catalog,
        )
        .unwrap();
        assert_eq!(
            forms[0].options.after,
            Some(Cursor::Byop {
                entity_id: json!(42),
                attr_id,
                value: json!("oren"),
            })
        );

        let error = compile_roots(
            &json!({
                "users": {"$": {
                    "order": {"handle": "asc"},
                    "after": [42, attr_id, "oren"]
                }}
            }),
            &catalog,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("Bring Your Own Postgres"));
    }

    #[test]
    fn validates_cursor_provenance_and_selects_one_cardinality_fields() {
        let catalog = catalog();
        let entity = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let wrong_attr = "33333333-3333-3333-3333-333333333333";
        let error = compile_roots(
            &json!({"users": {"$": {
                "order": {"handle": "asc"},
                "after": [entity, wrong_attr, "x", 10]
            }}}),
            &catalog,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing attribute"));

        let roots = compile_roots(
            &json!({"users": {"$": {"fields": ["handle", "missing"]}}}),
            &catalog,
            false,
        )
        .unwrap();
        assert_eq!(roots[0].selected_attr_ids.len(), 2);
    }

    #[test]
    fn rejects_invalid_where_types_indexes_and_operators() {
        let catalog = catalog();
        let cases = [
            (
                json!({"users": {"$": {"where": {"handle": 10}}}}),
                "string?",
            ),
            (
                json!({"users": {"$": {"where": {"handle": {"$wat": "x"}}}}}),
                "instant.db.instaql/where-value-valid-keys?",
            ),
        ];
        for (query, expected) in cases {
            let InstaqlError::Validation(errors) =
                compile_roots(&query, &catalog, false).unwrap_err()
            else {
                panic!("expected validation error");
            };
            assert_eq!(errors[0].expected, expected);
        }

        let missing = compile_roots(
            &json!({"users": {"$": {"where": {"missing": "x"}}}}),
            &catalog,
            false,
        )
        .unwrap();
        assert!(missing[0].missing);
        assert_eq!(missing[0].missing_result().unwrap().topics.len(), 2);
        assert!(matches!(
            missing[0].plan(&MatchQueryContext {
                prefix: "missing-",
                app_id: uuid("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
                additional_joins: &HashMap::new(),
                initial_symbol_cardinalities: &HashMap::new(),
                options: &MatchCteOptions::default(),
            }),
            Err(InstaqlError::MissingAttributePlan)
        ));
    }
}
