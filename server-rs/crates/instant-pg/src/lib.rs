//! PostgreSQL binding and execution primitives shared by the Rust server.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use instant_datalog::{
    BatchResultKind, DataType, DatalogResult, DatalogValue, FunctionClause, IndexKey,
    IsNullCardinalities, MatchCteOptions, MatchQueryContext, MatchQueryPlan, NormalizedClause,
    PageInfo, PagePlanMetadata, Pattern, PatternError, PatternMetadata, PatternMetadataNode,
    PgType, QueryCell, QueryRow, RenderedQuery, ResultDecodeError, ResultDecodeOptions,
    SqlParameter, SqlRenderError, Symbol, Term, UuidGroupedJsonRows, UuidGroupedRawJsonRows,
    add_page_info, bind_pattern_metadata_uuid_symbol, decode_json_paged_query_rows_with_options,
    decode_json_query_rows_with_options, decode_query_rows_with_options,
    decode_raw_json_paged_query_rows_with_options, decode_raw_json_query_rows_with_options,
    missing_attribute_result, plan_match_query, render_count_query, render_match_query,
    render_mixed_batch_match_query, render_nested_match_query,
    render_nested_match_query_with_structural_constants,
};
use instant_instaql::{
    CompiledQuery, ExecutedEntity, ExecutedNode, InstaqlError, PlannedNestedGroup,
    PlannedNestedQuery, plan_nested_query,
};
use serde_json::Value;
use serde_json::value::RawValue;
use thiserror::Error;
use tokio_postgres::types::{Format, FromSql, IsNull, Json, Kind, ToSql, Type, private::BytesMut};
use tokio_postgres::{CancelToken, Client, GenericClient, Row, Transaction};
use tokio_postgres_rustls::MakeRustlsConnect;
use uuid::Uuid;

mod attr_sketch;
mod attr_sketch_cache;
mod byop;
mod byop_query;
mod byop_topics;
mod loader;
mod planner_sketch_cache;
mod replication;
mod replication_slot;
mod sketch_aggregator;

pub use attr_sketch::*;
pub use byop::*;
pub use byop_query::*;
pub use byop_topics::*;
pub use loader::*;
pub use pg_walstream::{
    CancellationToken as ReplicationCancellationToken,
    ReplicationError as ReplicationTransportError,
};
pub use replication::{
    ExportedSnapshotSlot, Wal2JsonReplicationConnection, create_exported_snapshot_slot,
};
pub use replication_slot::*;
pub use sketch_aggregator::*;

/// JVM-compatible upper bound for ordinary Datalog/InstaQL PostgreSQL queries.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const CANCEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends an out-of-band PostgreSQL cancellation request within `timeout`.
///
/// This low-level primitive is shared with higher-level request cancellation
/// trackers. A `false` result means the request failed or exceeded its network
/// deadline; callers must still discard a connection they cannot safely drain.
pub async fn request_query_cancel(cancel_token: &CancelToken, timeout: Duration) -> bool {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    matches!(
        tokio::time::timeout(
            timeout,
            cancel_token.cancel_query(MakeRustlsConnect::with_webpki_roots()),
        )
        .await,
        Ok(Ok(()))
    )
}

/// A PostgreSQL query owner that can issue an out-of-band cancellation request.
pub trait CancelableGenericClient: GenericClient + Sync {
    fn query_cancel_token(&self) -> CancelToken;
}

/// Optional connection-local statement preparation for a rendered query.
/// Implementations must prepare on the same physical connection as the query
/// client. Only SQL plans may be retained, never parameters or returned rows.
pub trait QueryStatementPreparer: Sync {
    fn prepare(
        &self,
        sql: &str,
    ) -> impl std::future::Future<
        Output = Result<Option<tokio_postgres::Statement>, tokio_postgres::Error>,
    > + Send;
}

struct UncachedQueryPreparer;

impl QueryStatementPreparer for UncachedQueryPreparer {
    fn prepare(
        &self,
        _: &str,
    ) -> impl std::future::Future<
        Output = Result<Option<tokio_postgres::Statement>, tokio_postgres::Error>,
    > + Send {
        std::future::ready(Ok(None))
    }
}

struct QueryCancellationGuard {
    cancel_token: Option<CancelToken>,
}

impl QueryCancellationGuard {
    fn new(cancel_token: CancelToken) -> Self {
        Self {
            cancel_token: Some(cancel_token),
        }
    }

    fn disarm(&mut self) {
        self.cancel_token = None;
    }

    async fn cancel(&mut self) {
        let Some(cancel_token) = self.cancel_token.take() else {
            return;
        };
        let _request_was_delivered =
            request_query_cancel(&cancel_token, CANCEL_REQUEST_TIMEOUT).await;
    }
}

impl Drop for QueryCancellationGuard {
    fn drop(&mut self) {
        let Some(cancel_token) = self.cancel_token.take() else {
            return;
        };
        tokio::spawn(async move {
            let _request_was_delivered =
                request_query_cancel(&cancel_token, CANCEL_REQUEST_TIMEOUT).await;
        });
    }
}

impl CancelableGenericClient for Client {
    fn query_cancel_token(&self) -> CancelToken {
        self.cancel_token()
    }
}

impl CancelableGenericClient for Transaction<'_> {
    fn query_cancel_token(&self) -> CancelToken {
        self.cancel_token()
    }
}

/// An owned value that can be borrowed as a `tokio-postgres` parameter.
#[derive(Debug)]
pub enum BoundParameter {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    Text(String),
    Json(Json<Value>),
    Uuid(Uuid),
    Timestamp(DateTime<Utc>),
    BooleanArray(Vec<Option<bool>>),
    IntegerArray(Vec<Option<i64>>),
    FloatArray(Vec<Option<f64>>),
    TextArray(Vec<Option<String>>),
    JsonArray(Vec<Option<Json<Value>>>),
    UuidArray(Vec<Option<Uuid>>),
    TimestampArray(Vec<Option<DateTime<Utc>>>),
    GenericArray(GenericArrayParameter),
}

impl BoundParameter {
    #[must_use]
    pub fn as_to_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            Self::Boolean(value) => value,
            Self::Integer(value) => value,
            Self::Float(value) => value,
            Self::Text(value) => value,
            Self::Json(value) => value,
            Self::Uuid(value) => value,
            Self::Timestamp(value) => value,
            Self::BooleanArray(value) => value,
            Self::IntegerArray(value) => value,
            Self::FloatArray(value) => value,
            Self::TextArray(value) => value,
            Self::JsonArray(value) => value,
            Self::UuidArray(value) => value,
            Self::TimestampArray(value) => value,
            Self::GenericArray(value) => value,
        }
    }

    // Exact native binder types, not SQL inference or a remembered statement.
    // Schema-defined arrays must retain PostgreSQL's describe/binding path.
    fn builtin_type(&self) -> Option<Type> {
        Some(match self {
            Self::Boolean(_) => Type::BOOL,
            Self::Integer(_) => Type::INT8,
            Self::Float(_) => Type::FLOAT8,
            Self::Text(_) => Type::TEXT,
            Self::Json(_) => Type::JSONB,
            Self::Uuid(_) => Type::UUID,
            Self::Timestamp(_) => Type::TIMESTAMPTZ,
            Self::BooleanArray(_) => Type::BOOL_ARRAY,
            Self::IntegerArray(_) => Type::INT8_ARRAY,
            Self::FloatArray(_) => Type::FLOAT8_ARRAY,
            Self::TextArray(_) => Type::TEXT_ARRAY,
            Self::JsonArray(_) => Type::JSONB_ARRAY,
            Self::UuidArray(_) => Type::UUID_ARRAY,
            Self::TimestampArray(_) => Type::TIMESTAMPTZ_ARRAY,
            Self::GenericArray(_) => return None,
        })
    }
}

/// An array encoded using PostgreSQL's text protocol for its driver-provided type.
///
/// PostgreSQL applies the array element type's input function, allowing custom
/// enum, domain, and extension types without adding a Rust binding for each one.
#[derive(Debug)]
pub struct GenericArrayParameter(Vec<Value>);

impl GenericArrayParameter {
    fn array_literal(&self) -> String {
        array_literal(&self.0)
    }
}

impl ToSql for GenericArrayParameter {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Send + Sync>> {
        if !matches!(ty.kind(), Kind::Array(_)) {
            return Err(Box::new(tokio_postgres::types::WrongType::new::<Self>(
                ty.clone(),
            )));
        }
        out.extend_from_slice(self.array_literal().as_bytes());
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Array(_))
    }

    fn encode_format(&self, _: &Type) -> Format {
        Format::Text
    }

    tokio_postgres::types::to_sql_checked!();
}

fn array_literal(values: &[Value]) -> String {
    let mut literal = String::from("{");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            literal.push(',');
        }
        match value {
            Value::Null => literal.push_str("NULL"),
            Value::Array(values) => literal.push_str(&array_literal(values)),
            value => {
                literal.push('"');
                let value = value
                    .as_str()
                    .map_or_else(|| value.to_string(), ToOwned::to_owned);
                for character in value.chars() {
                    if matches!(character, '"' | '\\') {
                        literal.push('\\');
                    }
                    literal.push(character);
                }
                literal.push('"');
            }
        }
    }
    literal.push('}');
    literal
}

#[derive(Debug, Error)]
pub enum PgQueryError {
    #[error("invalid {element_type:?} array element at index {index}: {value}")]
    InvalidArrayElement {
        index: usize,
        element_type: PgType,
        value: Value,
    },
    #[error("PostgreSQL query was cancelled")]
    QueryCancelled,
    #[error(transparent)]
    Postgres(tokio_postgres::Error),
    #[error(transparent)]
    ResultDecode(#[from] ResultDecodeError),
    #[error("a batched query must return exactly one row, got {0}")]
    UnexpectedBatchRowCount(usize),
    #[error("a batched query result must be a JSON object")]
    InvalidBatchEnvelope,
    #[error("a batched query result is missing key {0}")]
    MissingBatchKey(String),
    #[error("batch count result for {0} is not a PostgreSQL bigint JSON value")]
    InvalidBatchCount(String),
    #[error("count requests cannot include pagination")]
    CountRequestHasPage,
    #[error("pagination flag table {0} did not return one boolean `exists` row")]
    InvalidPageFlag(String),
    #[error("an aggregate query must return exactly one row, got {0}")]
    UnexpectedAggregateRowCount(usize),
    #[error("PostgreSQL query exceeded its {milliseconds}ms execution timeout")]
    QueryTimeout { milliseconds: u64 },
    #[error("a nested query must return exactly one row, got {0}")]
    UnexpectedNestedRowCount(usize),
    #[error("a nested query batch produced {actual} roots for {expected} requested roots")]
    UnexpectedNestedBatchResultCount { expected: usize, actual: usize },
    #[error("a nested query result must be a JSON array of objects")]
    InvalidNestedEnvelope,
    #[error("nested result table {0} contains an invalid count")]
    InvalidNestedCount(String),
    #[error("a non-missing nested result group has no result table")]
    MissingNestedResultTable,
    #[error("nested result group {table} expected one entity fetch group, got {actual}")]
    InvalidNestedFetchCardinality { table: String, actual: usize },
    #[error("nested result group {table} expected one child-selector set, got {actual}")]
    InvalidNestedChildCardinality { table: String, actual: usize },
    #[error("EXPLAIN expected exactly one JSON row, got {0}")]
    UnexpectedExplainRowCount(usize),
    #[error("PostgreSQL returned a malformed JSON EXPLAIN envelope")]
    InvalidExplainEnvelope,
    #[error(transparent)]
    Pattern(#[from] PatternError),
    #[error(transparent)]
    Instaql(#[from] InstaqlError),
    #[error(transparent)]
    SqlRender(#[from] SqlRenderError),
    #[error(transparent)]
    AttrSketch(#[from] AttrSketchDecodeError),
}

impl From<tokio_postgres::Error> for PgQueryError {
    fn from(error: tokio_postgres::Error) -> Self {
        if error.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED)
            && error
                .as_db_error()
                .is_some_and(|error| error.message() == "canceling statement due to user request")
        {
            Self::QueryCancelled
        } else {
            Self::Postgres(error)
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ExplainIndexUse {
    pub index: String,
    pub node: Option<String>,
    pub rows: Option<f64>,
    pub loops: Option<f64>,
    pub time_ms: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct InstaqlExplainAnalysis {
    pub time_ms: f64,
    pub execution_time_ms: f64,
    pub planning_time_ms: f64,
    pub indexes: BTreeMap<String, Vec<ExplainIndexUse>>,
}

/// Owned input accepted by the coalescing Datalog query path.
#[derive(Clone, Debug)]
pub struct DatalogBatchRequest {
    pub app_id: Uuid,
    pub clauses: Vec<NormalizedClause>,
    pub options: MatchCteOptions,
    pub page: Option<PageInfo>,
}

/// One request accepted by the heterogeneous coalescing Datalog loader.
#[derive(Clone, Debug)]
pub enum MixedDatalogBatchRequest {
    Rows(DatalogBatchRequest),
    Count(DatalogBatchRequest),
}

/// Position-matched output from a heterogeneous Datalog loader tranche.
#[derive(Clone, Debug, PartialEq)]
pub enum MixedDatalogBatchResult {
    Rows(Box<DatalogResult>),
    Count(i64),
}

/// Converts the renderer's transport-neutral values into owned driver parameters.
///
/// # Errors
///
/// Returns [`PgQueryError`] when a typed array contains an incompatible JSON value.
pub fn bind_parameters(parameters: &[SqlParameter]) -> Result<Vec<BoundParameter>, PgQueryError> {
    parameters.iter().map(bind_parameter).collect()
}

/// Executes a rendered query with either a PostgreSQL client or transaction.
///
/// # Errors
///
/// Returns [`PgQueryError`] when parameter conversion or query execution fails.
pub async fn query<C>(client: &C, query: &RenderedQuery) -> Result<Vec<Row>, PgQueryError>
where
    C: CancelableGenericClient,
{
    query_with_timeout(client, query, DEFAULT_QUERY_TIMEOUT).await
}

/// Executes a rendered query with a bounded deadline and cancels the server-side
/// PostgreSQL statement before returning a timeout error.
///
/// # Errors
///
/// Returns [`PgQueryError`] when binding/execution fails or the deadline expires.
pub async fn query_with_timeout<C>(
    client: &C,
    query: &RenderedQuery,
    timeout: Duration,
) -> Result<Vec<Row>, PgQueryError>
where
    C: CancelableGenericClient,
{
    query_with_timeout_and_preparer(client, query, timeout, &UncachedQueryPreparer).await
}

/// Executes with optional connection-local plan reuse under the same total
/// preparation/execution deadline and cancellation/drain guarantees as
/// [`query_with_timeout`]. A preparer returning `None` selects the original path.
///
/// # Errors
///
/// Returns binding/preparation/execution errors or a bounded query timeout.
pub async fn query_with_timeout_and_preparer<C, P>(
    client: &C,
    query: &RenderedQuery,
    timeout: Duration,
    preparer: &P,
) -> Result<Vec<Row>, PgQueryError>
where
    C: CancelableGenericClient,
    P: QueryStatementPreparer,
{
    let bound = bind_parameters(&query.parameters)?;
    let borrowed: Vec<&(dyn ToSql + Sync)> = bound.iter().map(BoundParameter::as_to_sql).collect();
    let query = async {
        match preparer.prepare(&query.sql).await? {
            Some(statement) => client.query(&statement, &borrowed).await,
            None => client.query(&query.sql, &borrowed).await,
        }
    };
    execute_with_timeout(client, query, timeout).await
}

async fn query_with_builtin_parameter_types<C>(
    client: &C,
    query: &RenderedQuery,
    timeout: Duration,
) -> Result<Vec<Row>, PgQueryError>
where
    C: CancelableGenericClient,
{
    let bound = bind_parameters(&query.parameters)?;
    let Some(types) = bound
        .iter()
        .map(BoundParameter::builtin_type)
        .collect::<Option<Vec<_>>>()
    else {
        // Never guess a schema-defined type OID or retry a failed statement.
        return query_with_timeout(client, query, timeout).await;
    };
    let typed = bound
        .iter()
        .zip(types)
        .map(|(value, kind)| (value.as_to_sql(), kind))
        .collect::<Vec<_>>();
    // Fresh parse/bind/execute in one exchange, with no persistent statement or
    // generic-plan reuse. Keep the existing cancellation/deadline/drain owner.
    execute_with_timeout(client, client.query_typed(&query.sql, &typed), timeout).await
}

async fn execute_with_timeout<C, F>(
    client: &C,
    query: F,
    timeout: Duration,
) -> Result<Vec<Row>, PgQueryError>
where
    C: CancelableGenericClient,
    F: std::future::Future<Output = Result<Vec<Row>, tokio_postgres::Error>>,
{
    let mut cancellation = QueryCancellationGuard::new(client.query_cancel_token());
    tokio::pin!(query);
    if let Ok(result) = tokio::time::timeout(timeout, &mut query).await {
        cancellation.disarm();
        match result {
            Ok(rows) => Ok(rows),
            Err(error) => {
                tracing::error!(error = ?error, "PostgreSQL query execution failed");
                Err(error.into())
            }
        }
    } else {
        cancellation.cancel().await;
        // The cancel request is out-of-band. Drain the cancelled statement's
        // response before returning so a caller can safely reuse this client;
        // otherwise a fast follow-up query can receive SQLSTATE 57014 intended
        // for the timed-out statement.
        let _ = tokio::time::timeout(CANCEL_REQUEST_TIMEOUT, &mut query).await;
        Err(PgQueryError::QueryTimeout {
            milliseconds: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        })
    }
}

/// Executes a rendered Datalog query and reconstructs triples, symbol values, and topics.
///
/// # Errors
///
/// Returns [`PgQueryError`] when binding, PostgreSQL execution, row conversion, or
/// metadata-driven result decoding fails.
#[tracing::instrument(name = "datalog/send-query-single", skip_all, err)]
pub async fn query_datalog<C>(
    client: &C,
    query: &RenderedQuery,
    plan: &MatchQueryPlan,
) -> Result<DatalogResult, PgQueryError>
where
    C: CancelableGenericClient,
{
    let rows = self::query(client, query).await?;
    decode_postgres_rows_with_options(
        &rows,
        &plan.pattern_metadata,
        result_decode_options(plan.ensure_empty_symbol_values_for_topics.unwrap_or(false)),
    )
}

/// Executes a planned count aggregate and returns its scalar value.
///
/// # Errors
///
/// Returns [`PgQueryError`] for rendering, binding, execution, row-count, or
/// PostgreSQL column-decoding errors.
#[tracing::instrument(name = "datalog/send-query-single-count", skip_all, err)]
pub async fn query_datalog_count<C>(client: &C, plan: &MatchQueryPlan) -> Result<i64, PgQueryError>
where
    C: CancelableGenericClient,
{
    let rendered = render_count_query(plan)?;
    let rows = self::query(client, &rendered).await?;
    if rows.len() != 1 {
        return Err(PgQueryError::UnexpectedAggregateRowCount(rows.len()));
    }
    Ok(rows[0].try_get("count")?)
}

/// Executes a rendered batch and returns results in the same order as its match plans.
///
/// # Errors
///
/// Returns [`PgQueryError`] for binding/execution errors, an invalid JSON batch
/// envelope, missing plan keys, or metadata-driven decode failures.
#[tracing::instrument(name = "datalog/send-query-batch", skip_all, err)]
pub async fn query_datalog_batch<C>(
    client: &C,
    query: &RenderedQuery,
    plans: &[MatchQueryPlan],
) -> Result<Vec<DatalogResult>, PgQueryError>
where
    C: CancelableGenericClient,
{
    let rows = self::query(client, query).await?;
    if rows.len() != 1 {
        return Err(PgQueryError::UnexpectedBatchRowCount(rows.len()));
    }
    let Json(envelope) = rows[0].try_get::<_, Json<Value>>(0)?;
    decode_datalog_batch_envelope(&envelope, plans)
}

/// Decodes an already-fetched row-only Datalog batch envelope.
///
/// This is the transport-neutral half of [`query_datalog_batch`], exposed so
/// alternate PostgreSQL transports and differential fixtures exercise the same
/// production decoder rather than duplicating its envelope logic.
///
/// # Errors
///
/// Returns [`PgQueryError`] for malformed or missing envelope entries, invalid
/// pagination flags, or metadata-driven row decoding failures.
pub fn decode_datalog_batch_envelope(
    envelope: &Value,
    plans: &[MatchQueryPlan],
) -> Result<Vec<DatalogResult>, PgQueryError> {
    let kinds = vec![BatchResultKind::Rows; plans.len()];
    decode_mixed_datalog_batch_envelope(envelope, plans, &kinds)?
        .into_iter()
        .map(|result| match result {
            MixedDatalogBatchResult::Rows(result) => Ok(*result),
            MixedDatalogBatchResult::Count(_) => unreachable!("row-only decoder returned count"),
        })
        .collect()
}

/// Decodes an already-fetched heterogeneous Datalog row/count batch envelope.
///
/// # Errors
///
/// Returns [`PgQueryError`] when plan/result-kind cardinalities differ or for
/// malformed row, page-flag, or count entries.
pub fn decode_mixed_datalog_batch_envelope(
    envelope: &Value,
    plans: &[MatchQueryPlan],
    kinds: &[BatchResultKind],
) -> Result<Vec<MixedDatalogBatchResult>, PgQueryError> {
    if plans.len() != kinds.len() {
        return Err(SqlRenderError::BatchResultKindCount {
            plans: plans.len(),
            kinds: kinds.len(),
        }
        .into());
    }
    let envelope = envelope
        .as_object()
        .ok_or(PgQueryError::InvalidBatchEnvelope)?;
    plans
        .iter()
        .zip(kinds)
        .map(|(plan, kind)| match kind {
            BatchResultKind::Rows => {
                let rows = envelope
                    .get(&plan.final_table)
                    .ok_or_else(|| PgQueryError::MissingBatchKey(plan.final_table.clone()))?;
                let result = if let Some(page) = &plan.page {
                    decode_json_paged_query_rows_with_options(
                        rows,
                        &plan.pattern_metadata,
                        page,
                        batch_page_flag(envelope, &page.has_next_table)?,
                        batch_page_flag(envelope, &page.has_previous_table)?,
                        result_decode_options(
                            plan.ensure_empty_symbol_values_for_topics.unwrap_or(false),
                        ),
                    )?
                } else {
                    decode_json_query_rows_with_options(
                        rows,
                        &plan.pattern_metadata,
                        result_decode_options(
                            plan.ensure_empty_symbol_values_for_topics.unwrap_or(false),
                        ),
                    )?
                };
                Ok(MixedDatalogBatchResult::Rows(Box::new(result)))
            }
            BatchResultKind::Count => envelope
                .get(&plan.final_table)
                .and_then(Value::as_i64)
                .map(MixedDatalogBatchResult::Count)
                .ok_or_else(|| PgQueryError::InvalidBatchCount(plan.final_table.clone())),
        })
        .collect()
}

/// Executes and decodes a heterogeneous row/count Datalog batch envelope.
///
/// # Errors
///
/// Returns [`PgQueryError`] for binding/execution failures, malformed or missing
/// envelope entries, invalid count scalars, pagination flags, or row decoding.
#[tracing::instrument(name = "datalog/send-query-batch", skip_all, err)]
pub async fn query_mixed_datalog_batch<C>(
    client: &C,
    query: &RenderedQuery,
    plans: &[MatchQueryPlan],
    kinds: &[BatchResultKind],
) -> Result<Vec<MixedDatalogBatchResult>, PgQueryError>
where
    C: CancelableGenericClient,
{
    if plans.len() != kinds.len() {
        return Err(SqlRenderError::BatchResultKindCount {
            plans: plans.len(),
            kinds: kinds.len(),
        }
        .into());
    }
    let rows = self::query(client, query).await?;
    if rows.len() != 1 {
        return Err(PgQueryError::UnexpectedBatchRowCount(rows.len()));
    }
    let Json(envelope) = rows[0].try_get::<_, Json<Value>>(0)?;
    decode_mixed_datalog_batch_envelope(&envelope, plans, kinds)
}

/// Plans and executes a complete recursive `InstaQL` query in one PostgreSQL round trip.
///
/// The returned tree is ready for [`instant_instaql::collect_nodes`] or
/// [`instant_instaql::shape_response`]. Parent bindings are applied while decoding,
/// matching the JVM nested-result scoping behavior.
///
/// # Errors
///
/// Returns [`PgQueryError`] for planning/rendering/binding failures, malformed
/// nested envelopes, invalid join bindings, or a selector/fetch shape mismatch.
#[tracing::instrument(name = "datalog/send-query-nested", skip_all, err, fields(app.id = %app_id))]
pub async fn query_instaql<C>(
    client: &C,
    query: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
{
    query_instaql_with_timeout(client, query, app_id, options, DEFAULT_QUERY_TIMEOUT).await
}

/// Executes native InstaQL with explicit built-in parameter types for the main
/// statement, avoiding its describe round trip without retaining a prepared plan.
/// Sketch reads, SQL, bound values, snapshots and result decoding are unchanged.
/// A schema-defined parameter falls back before execution to normal description.
///
/// # Errors
///
/// Returns the same planning, binding, execution, timeout and decoding errors as
/// [`query_instaql_with_timeout`], under the same total deadline.
pub async fn query_instaql_with_typed_parameters<C>(
    client: &C,
    compiled: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
    timeout: Duration,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
{
    query_instaql_with_timeout_and_preparer(
        client,
        compiled,
        app_id,
        options,
        timeout,
        &UncachedQueryPreparer,
        MainQueryProtocol::Typed,
    )
    .await
}

/// Uses bounded, connection-local structural SQL plans for small native pages.
/// Fresh cardinalities, compilation, bind values, snapshots, and decoding remain
/// inside the ordinary total deadline. Other query shapes keep typed execution.
/// PostgreSQL retains its automatic generic/custom choice; no mode is forced.
///
/// # Errors
///
/// Returns the same errors and cancellation semantics as [`query_instaql_with_timeout`].
pub async fn query_instaql_with_structural_preparer<C, P>(
    client: &C,
    compiled: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
    timeout: Duration,
    preparer: &P,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
    P: QueryStatementPreparer,
{
    let protocol = if structural_plan_eligible(compiled, options) {
        MainQueryProtocol::Structural
    } else {
        MainQueryProtocol::Typed
    };
    query_instaql_with_timeout_and_preparer(
        client, compiled, app_id, options, timeout, preparer, protocol,
    )
    .await
}

#[derive(Clone, Copy)]
enum MainQueryProtocol {
    Described,
    Typed,
    Structural,
}

fn structural_plan_eligible(query: &CompiledQuery, options: &MatchCteOptions) -> bool {
    options.pg_hints_enabled == Some(true)
        && options.candidate_entity_scans == Some(true)
        && (1..=2).contains(&query.roots.len())
        && query.roots.iter().all(|root| {
            !root.missing
                && root.aggregate.is_none()
                && root.children.is_empty()
                && structural_pattern_count(&root.selector_clauses).is_some()
                && root.page.as_ref().is_some_and(|page| {
                    page.limit.is_some_and(|limit| (1..=100).contains(&limit))
                        && page.offset.unwrap_or_default() <= 100
                })
        })
}

fn structural_pattern_count(clauses: &[NormalizedClause]) -> Option<usize> {
    clauses.iter().try_fold(0_usize, |count, clause| {
        let added = match clause {
            NormalizedClause::Pattern { pattern } => {
                if !matches!(pattern.index.key, IndexKey::Ea | IndexKey::Av | IndexKey::Ave)
                    || matches!(&pattern.value, Term::Function(FunctionClause::IsNull(clause)) if clause.reference)
                {
                    return None;
                }
                1
            }
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                structural_pattern_count(clauses)?
            }
        };
        count.checked_add(added).filter(|count| *count <= 8)
    })
}

/// Coalesces compatible complete `InstaQL` requests into one recursive SQL
/// statement and returns one position-matched result tree per request.
///
/// Callers are responsible for grouping only requests that share an app,
/// authorization boundary, catalog version, and planner options. Empty queries
/// retain their position in the returned batch.
///
/// # Errors
///
/// Returns [`PgQueryError`] for planning, execution, or decoding failures, or
/// when the merged root result cardinality does not match the submitted queries.
pub async fn execute_instaql_requests<C>(
    client: &C,
    requests: &[CompiledQuery],
    app_id: Uuid,
    options: &MatchCteOptions,
) -> Result<Vec<Vec<ExecutedNode>>, PgQueryError>
where
    C: CancelableGenericClient,
{
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let root_counts = requests
        .iter()
        .map(|request| request.roots.len())
        .collect::<Vec<_>>();
    let expected = root_counts.iter().sum();
    let merged = CompiledQuery {
        roots: requests
            .iter()
            .flat_map(|request| request.roots.iter().cloned())
            .collect(),
    };
    let executed = query_instaql(client, &merged, app_id, options).await?;
    let actual = executed.len();
    if actual != expected {
        return Err(PgQueryError::UnexpectedNestedBatchResultCount { expected, actual });
    }
    let mut executed = executed.into_iter();
    Ok(root_counts
        .into_iter()
        .map(|count| executed.by_ref().take(count).collect())
        .collect())
}

/// Plans and executes a complete recursive `InstaQL` query within one total
/// deadline, including the cardinality/sketch reads performed before planning.
///
/// # Errors
///
/// Returns [`PgQueryError`] for planning/rendering/binding/execution failures or
/// when the complete operation exceeds `timeout`.
pub async fn query_instaql_with_timeout<C>(
    client: &C,
    compiled: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
    timeout: Duration,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
{
    query_instaql_with_timeout_and_preparer(
        client,
        compiled,
        app_id,
        options,
        timeout,
        &UncachedQueryPreparer,
        MainQueryProtocol::Described,
    )
    .await
}

/// Executes fresh native rows using a caller-owned, connection-local SQL plan
/// preparer. Catalog, permissions, parameters and decoded results are not cached.
///
/// # Errors
///
/// Returns normal planning, preparation, execution, timeout or decoding errors.
#[tracing::instrument(name = "datalog/send-query-nested", skip_all)]
pub async fn query_instaql_with_preparer<C, P>(
    client: &C,
    compiled: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
    preparer: &P,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
    P: QueryStatementPreparer,
{
    query_instaql_with_timeout_and_preparer(
        client,
        compiled,
        app_id,
        options,
        DEFAULT_QUERY_TIMEOUT,
        preparer,
        MainQueryProtocol::Described,
    )
    .await
}

async fn query_instaql_with_timeout_and_preparer<C, P>(
    client: &C,
    compiled: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
    timeout: Duration,
    preparer: &P,
    protocol: MainQueryProtocol,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
    P: QueryStatementPreparer,
{
    let deadline = query_deadline(timeout)?;
    let result = async {
        let mut native_options = options.clone();
        if matches!(protocol, MainQueryProtocol::Structural) {
            native_options.keyed_small_page_scans = Some(true);
        }
        let options = load_attribute_cardinalities(
            client,
            compiled,
            app_id,
            &native_options,
            deadline,
            timeout,
        )
        .await?;
        let plan = plan_nested_query(compiled, app_id, &options)?;
        query_planned_instaql_with_timeout_and_preparer(
            client,
            &plan,
            remaining_query_timeout(deadline, timeout)?,
            preparer,
            protocol,
        )
        .await
    }
    .await;
    result.map_err(|error| normalize_query_timeout(error, timeout))
}

/// Plans and executes PostgreSQL `EXPLAIN (ANALYZE, FORMAT JSON)` for the
/// complete recursive `InstaQL` statement.
///
/// # Errors
///
/// Returns [`PgQueryError`] for planning, rendering, binding, execution,
/// timeout, or malformed PostgreSQL EXPLAIN output.
pub async fn explain_instaql<C>(
    client: &C,
    query: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
    timeout: Duration,
) -> Result<InstaqlExplainAnalysis, PgQueryError>
where
    C: CancelableGenericClient,
{
    let deadline = query_deadline(timeout)?;
    let options = load_attribute_cardinalities(client, query, app_id, options, deadline, timeout)
        .await
        .map_err(|error| normalize_query_timeout(error, timeout))?;
    let plan = plan_nested_query(query, app_id, &options)?;
    if plan.groups.is_empty() || plan.ctes.is_empty() {
        remaining_query_timeout(deadline, timeout)?;
        return Ok(InstaqlExplainAnalysis {
            time_ms: 0.0,
            execution_time_ms: 0.0,
            planning_time_ms: 0.0,
            indexes: BTreeMap::new(),
        });
    }
    let rendered = render_nested_match_query(&plan.sql_plan())?;
    let explain = RenderedQuery {
        sql: format!("EXPLAIN (ANALYZE, FORMAT JSON) {}", rendered.sql),
        parameters: rendered.parameters,
    };
    let rows = query_with_timeout(
        client,
        &explain,
        remaining_query_timeout(deadline, timeout)?,
    )
    .await
    .map_err(|error| normalize_query_timeout(error, timeout))?;
    if rows.len() != 1 {
        return Err(PgQueryError::UnexpectedExplainRowCount(rows.len()));
    }
    let Json(value) = rows[0].try_get::<_, Json<Value>>(0)?;
    decode_explain_analysis(&value)
}

fn decode_explain_analysis(value: &Value) -> Result<InstaqlExplainAnalysis, PgQueryError> {
    let root = value
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_object)
        .ok_or(PgQueryError::InvalidExplainEnvelope)?;
    let execution_time_ms = root
        .get("Execution Time")
        .and_then(Value::as_f64)
        .ok_or(PgQueryError::InvalidExplainEnvelope)?;
    let planning_time_ms = root
        .get("Planning Time")
        .and_then(Value::as_f64)
        .ok_or(PgQueryError::InvalidExplainEnvelope)?;
    let mut indexes = BTreeMap::new();
    collect_explain_indexes(value, None, &mut indexes);
    Ok(InstaqlExplainAnalysis {
        time_ms: execution_time_ms + planning_time_ms,
        execution_time_ms,
        planning_time_ms,
        indexes,
    })
}

fn collect_explain_indexes(
    value: &Value,
    inherited_alias: Option<&str>,
    indexes: &mut BTreeMap<String, Vec<ExplainIndexUse>>,
) {
    match value {
        Value::Object(object) => {
            let alias = object
                .get("Alias")
                .or_else(|| object.get("Relation Name"))
                .and_then(Value::as_str)
                .or(inherited_alias);
            if let (Some(alias), Some(index)) =
                (alias, object.get("Index Name").and_then(Value::as_str))
            {
                let loops = object.get("Actual Loops").and_then(Value::as_f64);
                let total = object.get("Actual Total Time").and_then(Value::as_f64);
                indexes
                    .entry(alias.to_owned())
                    .or_default()
                    .push(ExplainIndexUse {
                        index: index.to_owned(),
                        node: object
                            .get("Node Type")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        rows: object.get("Actual Rows").and_then(Value::as_f64),
                        loops,
                        time_ms: total.map(|total| total * loops.unwrap_or(1.0)),
                    });
            }
            for child in object.values() {
                collect_explain_indexes(child, alias, indexes);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_explain_indexes(child, inherited_alias, indexes);
            }
        }
        _ => {}
    }
}

#[tracing::instrument(name = "datalog/read-sketches", skip_all)]
async fn load_attribute_cardinalities<C>(
    client: &C,
    query: &CompiledQuery,
    app_id: Uuid,
    options: &MatchCteOptions,
    deadline: tokio::time::Instant,
    timeout: Duration,
) -> Result<MatchCteOptions, PgQueryError>
where
    C: CancelableGenericClient,
{
    // With PostgreSQL hints disabled, don't load sketches just for hint costing.
    // Candidate planning can use estimates supplied by the caller, or fall back
    // to PostgreSQL's own probe-vs-hash-join choice when estimates are absent.
    if options.pg_hints_enabled == Some(false) {
        remaining_query_timeout(deadline, timeout)?;
        return Ok(options.clone());
    }
    let mut patterns = Vec::new();
    for root in &query.roots {
        collect_node_patterns(root, &mut patterns);
    }
    let mut attribute_ids = patterns
        .iter()
        .flat_map(pattern_attribute_ids)
        .collect::<HashSet<_>>();
    if options.keyed_small_page_scans == Some(true) {
        // A field fetch is also costed by the nested planner. Read its totals
        // in the same fresh snapshot/exchange, without fetching unused bins.
        // Previously only selector/order attributes were loaded, so the fetch
        // could not choose an entity index even after a ten-row page bound.
        attribute_ids.extend(
            query
                .roots
                .iter()
                .flat_map(|root| root.fetch_attr_ids.iter())
                .copied(),
        );
    }
    let value_attribute_ids = patterns
        .iter()
        .filter(|pattern| {
            matches!(
                pattern.value,
                Term::Constants(_) | Term::Function(FunctionClause::Not(_))
            )
        })
        .flat_map(pattern_attribute_ids)
        .collect::<HashSet<_>>();
    let lookup_attribute_ids = patterns
        .iter()
        .flat_map(pattern_lookup_attribute_ids)
        .collect::<HashSet<_>>();
    let is_null_attribute_ids = patterns
        .iter()
        .filter_map(|pattern| match &pattern.value {
            Term::Function(FunctionClause::IsNull(clause)) => Some(clause.attr_id),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let forward_attribute_ids = value_attribute_ids
        .union(&lookup_attribute_ids)
        .copied()
        .chain(is_null_attribute_ids)
        .collect::<HashSet<_>>();
    let reverse_attribute_ids = patterns
        .iter()
        .filter(|pattern| {
            matches!(pattern.index.key, IndexKey::Eav | IndexKey::Vae)
                && matches!(pattern.entity, Term::Constants(_))
        })
        .flat_map(pattern_attribute_ids)
        .collect::<HashSet<_>>();
    let selective_attribute_ids = forward_attribute_ids
        .union(&reverse_attribute_ids)
        .copied()
        .collect::<HashSet<_>>();
    let total_attribute_ids = attribute_ids
        .difference(&selective_attribute_ids)
        .filter(|attribute_id| !options.attribute_cardinalities.contains_key(attribute_id))
        .copied()
        .collect::<Vec<_>>();
    if total_attribute_ids.is_empty()
        && forward_attribute_ids.is_empty()
        && reverse_attribute_ids.is_empty()
    {
        return Ok(options.clone());
    }

    let forward_attribute_ids = forward_attribute_ids.into_iter().collect::<Vec<_>>();
    let reverse_attribute_ids = reverse_attribute_ids.into_iter().collect::<Vec<_>>();
    let snapshot_key = planner_sketch_cache::SketchSnapshotKey::new(
        app_id,
        &total_attribute_ids,
        &forward_attribute_ids,
        &reverse_attribute_ids,
    );
    if let Some(snapshot) = planner_sketch_cache::get(&snapshot_key) {
        // A hit performs no I/O, but a caller-supplied deadline still bounds
        // how long the remaining planning may run.
        remaining_query_timeout(deadline, timeout)?;
        let mut enriched = options.clone();
        apply_sketch_snapshot(&snapshot, patterns, &mut enriched);
        return Ok(enriched);
    }
    // These independent projections used to take three sequential exchanges.
    // One fresh statement keeps their snapshot, native UUID bindings and total
    // deadline without caching sketch rows or running competing cancel guards
    // on the same connection. Fetch compressed blobs only where requested.
    let snapshot = sketch_snapshot_query(
        app_id,
        &total_attribute_ids,
        &forward_attribute_ids,
        &reverse_attribute_ids,
    );
    let rows = execute_sketch_snapshot(
        client,
        &snapshot,
        remaining_query_timeout(deadline, timeout)?,
    )
    .await?;
    let mut enriched = options.clone();
    decode_total_sketch_rows(&rows, &total_attribute_ids, &mut enriched)?;
    let forward_sketches =
        decode_forward_sketch_rows(&rows, &forward_attribute_ids, &mut enriched)?;
    let reverse_sketches =
        decode_reverse_sketch_rows(&rows, &reverse_attribute_ids, &mut enriched)?;
    planner_sketch_cache::insert(
        snapshot_key,
        planner_sketch_cache::SketchSnapshot {
            totals: row_totals(&rows),
            forward: forward_sketches.clone(),
            reverse: reverse_sketches.clone(),
        },
    );
    enrich_pattern_cardinalities(
        patterns,
        &forward_sketches,
        &reverse_sketches,
        &mut enriched,
    );
    Ok(enriched)
}

/// Every requested row's total, independent of which projection decoded it.
fn row_totals(rows: &[Row]) -> Vec<(Uuid, u64)> {
    rows.iter()
        .filter_map(|row| {
            let attribute_id = row.try_get("attr_id").ok()?;
            let total = u64::try_from(row.try_get::<_, i64>("total").ok()?).ok()?;
            Some((attribute_id, total))
        })
        .collect()
}

fn apply_sketch_snapshot(
    snapshot: &planner_sketch_cache::SketchSnapshot,
    patterns: Vec<Pattern>,
    options: &mut MatchCteOptions,
) {
    for (attribute_id, total) in &snapshot.totals {
        options
            .attribute_cardinalities
            .insert(*attribute_id, *total);
    }
    enrich_pattern_cardinalities(patterns, &snapshot.forward, &snapshot.reverse, options);
}

async fn execute_sketch_snapshot<C>(
    client: &C,
    snapshot: &RenderedQuery,
    timeout: Duration,
) -> Result<Vec<Row>, PgQueryError>
where
    C: CancelableGenericClient,
{
    // This fixed statement has only UUID/UUID[] inputs. Supplying their exact
    // types sends parse/bind/execute together instead of first describing a
    // statement. Rows stay fresh, and no persistent/generic SQL plan is cached.
    let bound = bind_parameters(&snapshot.parameters)?;
    let typed = bound
        .iter()
        .zip([
            tokio_postgres::types::Type::UUID,
            tokio_postgres::types::Type::UUID_ARRAY,
            tokio_postgres::types::Type::UUID_ARRAY,
            tokio_postgres::types::Type::UUID_ARRAY,
        ])
        .map(|(value, kind)| (value.as_to_sql(), kind))
        .collect::<Vec<_>>();
    execute_with_timeout(client, client.query_typed(&snapshot.sql, &typed), timeout).await
}

pub(crate) type PlannerAttrSketch = AttrSketch<Arc<[u64]>>;

fn enrich_pattern_cardinalities(
    patterns: Vec<Pattern>,
    forward_sketches: &HashMap<Uuid, PlannerAttrSketch>,
    reverse_sketches: &HashMap<Uuid, PlannerAttrSketch>,
    enriched: &mut MatchCteOptions,
) {
    for pattern in patterns {
        for attribute_id in pattern_attribute_ids(&pattern) {
            if let Term::Function(FunctionClause::IsNull(clause)) = &pattern.value {
                let id_total = enriched
                    .attribute_cardinalities
                    .get(&attribute_id)
                    .copied()
                    .unwrap_or_default();
                let target_sketch = forward_sketches.get(&clause.attr_id);
                let target_total = target_sketch.map_or(0, |sketch| sketch.total);
                let explicit_nulls =
                    target_sketch.map_or(0, |sketch| sketch.estimate(AttrSketchInput::Nil));
                enriched
                    .attribute_is_null_cardinalities
                    .entry(attribute_id)
                    .or_default()
                    .insert(
                        clause.attr_id,
                        is_null_cardinalities(
                            id_total,
                            target_total,
                            explicit_nulls,
                            clause.indexed && !clause.reference,
                        ),
                    );
            }
            if let (Term::Constants(values), Some(sketch)) =
                (&pattern.value, forward_sketches.get(&attribute_id))
            {
                for value in values {
                    if let Some(estimate) =
                        estimate_json_value(sketch, value, pattern.index.data_type)
                    {
                        let key = serde_json::to_string(value).unwrap_or_default();
                        enriched
                            .attribute_value_cardinalities
                            .entry(attribute_id)
                            .or_default()
                            .insert(key, estimate);
                    }
                }
            }
            if let (Term::Function(FunctionClause::Not(value)), Some(sketch)) =
                (&pattern.value, forward_sketches.get(&attribute_id))
                && let Some(estimate) = estimate_json_value(sketch, value, pattern.index.data_type)
            {
                let key = serde_json::to_string(value).unwrap_or_default();
                enriched
                    .attribute_value_cardinalities
                    .entry(attribute_id)
                    .or_default()
                    .insert(key, estimate);
            }
            if matches!(pattern.index.key, IndexKey::Eav | IndexKey::Vae)
                && let (Term::Constants(entities), Some(sketch)) =
                    (&pattern.entity, reverse_sketches.get(&attribute_id))
            {
                for entity in entities {
                    let estimate = estimate_entity_uuid(sketch, entity)
                        .or_else(|| estimate_entity_lookup(forward_sketches, entity));
                    let Some(estimate) = estimate else {
                        continue;
                    };
                    let key = serde_json::to_string(entity).unwrap_or_default();
                    enriched
                        .attribute_entity_cardinalities
                        .entry(attribute_id)
                        .or_default()
                        .insert(key, estimate);
                }
            }
        }
    }
}

fn is_null_cardinalities(
    id_total: u64,
    target_total: u64,
    explicit_nulls: u64,
    indexed_non_reference: bool,
) -> IsNullCardinalities {
    let undefined = if indexed_non_reference {
        0
    } else {
        id_total.saturating_sub(target_total)
    };
    let null = explicit_nulls.saturating_add(undefined);
    let population = if indexed_non_reference {
        target_total
    } else {
        id_total.max(null)
    };
    IsNullCardinalities {
        null,
        not_null: population.saturating_sub(null),
    }
}

fn decode_total_sketch_rows(
    rows: &[Row],
    attribute_ids: &[Uuid],
    options: &mut MatchCteOptions,
) -> Result<(), PgQueryError> {
    if attribute_ids.is_empty() {
        return Ok(());
    }
    let requested = attribute_ids.iter().copied().collect::<HashSet<_>>();
    for row in rows {
        let attribute_id: Uuid = row.try_get("attr_id")?;
        if !requested.contains(&attribute_id) {
            continue;
        }
        let total: i64 = row.try_get("total")?;
        if let Ok(total) = u64::try_from(total) {
            options.attribute_cardinalities.insert(attribute_id, total);
        }
    }
    Ok(())
}

fn decode_forward_sketch_rows(
    rows: &[Row],
    attribute_ids: &[Uuid],
    options: &mut MatchCteOptions,
) -> Result<HashMap<Uuid, PlannerAttrSketch>, PgQueryError> {
    if attribute_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let requested = attribute_ids.iter().copied().collect::<HashSet<_>>();
    let mut sketches = HashMap::new();
    for row in rows {
        let attribute_id: Uuid = row.try_get("attr_id")?;
        if !requested.contains(&attribute_id) {
            continue;
        }
        let width = usize::try_from(row.try_get::<_, i32>("width")?).unwrap_or_default();
        let depth = usize::try_from(row.try_get::<_, i32>("depth")?).unwrap_or_default();
        let total = u64::try_from(row.try_get::<_, i64>("total")?).ok();
        let total_not_binned = u64::try_from(row.try_get::<_, i64>("total_not_binned")?).ok();
        let compressed: &[u8] = row.try_get("bins")?;
        if let (Some(total), Some(total_not_binned)) = (total, total_not_binned) {
            options.attribute_cardinalities.insert(attribute_id, total);
            sketches.insert(
                attribute_id,
                AttrSketch {
                    width,
                    depth,
                    bins: attr_sketch_cache::decode_shared_bins(width, depth, compressed)?,
                    total,
                    total_not_binned,
                },
            );
        }
    }
    Ok(sketches)
}

fn decode_reverse_sketch_rows(
    rows: &[Row],
    attribute_ids: &[Uuid],
    options: &mut MatchCteOptions,
) -> Result<HashMap<Uuid, PlannerAttrSketch>, PgQueryError> {
    if attribute_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let requested = attribute_ids.iter().copied().collect::<HashSet<_>>();
    let mut sketches = HashMap::new();
    for row in rows {
        let attribute_id: Uuid = row.try_get("attr_id")?;
        if !requested.contains(&attribute_id) {
            continue;
        }
        if let Ok(total) = u64::try_from(row.try_get::<_, i64>("total")?) {
            options.attribute_cardinalities.insert(attribute_id, total);
        }
        let width = row
            .try_get::<_, Option<i32>>("reverse_width")?
            .and_then(|value| usize::try_from(value).ok());
        let depth = row
            .try_get::<_, Option<i32>>("reverse_depth")?
            .and_then(|value| usize::try_from(value).ok());
        let total = row
            .try_get::<_, Option<i64>>("reverse_total")?
            .and_then(|value| u64::try_from(value).ok());
        let compressed: Option<&[u8]> = row.try_get("reverse_bins")?;
        if let (Some(width), Some(depth), Some(total), Some(compressed)) =
            (width, depth, total, compressed)
        {
            sketches.insert(
                attribute_id,
                AttrSketch {
                    width,
                    depth,
                    bins: attr_sketch_cache::decode_shared_bins(width, depth, compressed)?,
                    total,
                    total_not_binned: 0,
                },
            );
        }
    }
    Ok(sketches)
}

fn sketch_snapshot_query(
    app_id: Uuid,
    total: &[Uuid],
    forward: &[Uuid],
    reverse: &[Uuid],
) -> RenderedQuery {
    let ids = total
        .iter()
        .chain(forward)
        .chain(reverse)
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut query = attribute_cardinality_query(
        "/* native-sketch-snapshot */ SELECT attr_id, total,
         CASE WHEN attr_id = ANY($3) THEN width END AS width,
         CASE WHEN attr_id = ANY($3) THEN depth END AS depth,
         CASE WHEN attr_id = ANY($3) THEN total_not_binned END AS total_not_binned,
         CASE WHEN attr_id = ANY($3) THEN bins END AS bins,
         CASE WHEN attr_id = ANY($4) THEN reverse_width END AS reverse_width,
         CASE WHEN attr_id = ANY($4) THEN reverse_depth END AS reverse_depth,
         CASE WHEN attr_id = ANY($4) THEN reverse_total END AS reverse_total,
         CASE WHEN attr_id = ANY($4) THEN reverse_bins END AS reverse_bins
         FROM attr_sketches WHERE app_id = $1 AND attr_id = ANY($2)",
        app_id,
        &ids,
    );
    for requested in [forward, reverse] {
        query.parameters.push(SqlParameter::Array {
            values: requested
                .iter()
                .map(|id| Value::String(id.to_string()))
                .collect(),
            element_type: PgType::Uuid,
        });
    }
    query
}

fn attribute_cardinality_query(sql: &str, app_id: Uuid, attribute_ids: &[Uuid]) -> RenderedQuery {
    RenderedQuery {
        sql: sql.to_owned(),
        parameters: vec![
            SqlParameter::Uuid(app_id),
            SqlParameter::Array {
                values: attribute_ids
                    .iter()
                    .map(|attribute_id| Value::String(attribute_id.to_string()))
                    .collect(),
                element_type: PgType::Uuid,
            },
        ],
    }
}

fn collect_node_patterns(node: &instant_instaql::CompiledNode, patterns: &mut Vec<Pattern>) {
    collect_clause_patterns(&node.selector_clauses, patterns);
    if let Some(page) = &node.page {
        patterns.push(page.pattern.clone());
    }
    for child in &node.children {
        collect_node_patterns(child, patterns);
    }
}

fn collect_clause_patterns(clauses: &[NormalizedClause], patterns: &mut Vec<Pattern>) {
    for clause in clauses {
        match clause {
            NormalizedClause::Pattern { pattern } => patterns.push(pattern.clone()),
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                collect_clause_patterns(clauses, patterns);
            }
        }
    }
}

fn pattern_attribute_ids(pattern: &Pattern) -> impl Iterator<Item = Uuid> + '_ {
    let values = match &pattern.attribute {
        Term::Constants(values) => values.as_slice(),
        Term::Any | Term::Variable(_) | Term::Function(_) => &[],
    };
    values
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|value| value.parse().ok())
}

fn pattern_lookup_attribute_ids(pattern: &Pattern) -> impl Iterator<Item = Uuid> + '_ {
    let entities = match &pattern.entity {
        Term::Constants(values) => values.as_slice(),
        Term::Any | Term::Variable(_) | Term::Function(_) => &[],
    };
    entities.iter().filter_map(|entity| {
        let parts = entity.as_array()?;
        (parts.len() == 2)
            .then(|| parts[0].as_str()?.parse::<Uuid>().ok())
            .flatten()
    })
}

fn estimate_json_value<B: AsRef<[u64]>>(
    sketch: &AttrSketch<B>,
    value: &Value,
    data_type: Option<DataType>,
) -> Option<u64> {
    if value.is_null() {
        return Some(sketch.estimate(AttrSketchInput::Nil));
    }
    match data_type {
        Some(DataType::String) => Some(sketch.estimate(AttrSketchInput::String(value.as_str()?))),
        Some(DataType::Boolean) => {
            Some(sketch.estimate(AttrSketchInput::Boolean(value.as_bool()?)))
        }
        Some(DataType::Date) => {
            let millis = DateTime::parse_from_rfc3339(value.as_str()?)
                .ok()?
                .timestamp_millis();
            Some(sketch.estimate(AttrSketchInput::Long(millis)))
        }
        Some(DataType::Number) | None => estimate_unchecked_json_value(sketch, value),
    }
}

fn estimate_unchecked_json_value<B: AsRef<[u64]>>(
    sketch: &AttrSketch<B>,
    value: &Value,
) -> Option<u64> {
    match value {
        Value::String(value) => Some(sketch.estimate(AttrSketchInput::String(value))),
        Value::Bool(value) => Some(sketch.estimate(AttrSketchInput::Boolean(*value))),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Some(sketch.estimate(AttrSketchInput::Long(value)))
            } else if let Some(value) = value.as_u64() {
                let mut bytes = Vec::with_capacity(9);
                bytes.push(0);
                bytes.extend(value.to_be_bytes());
                Some(sketch.estimate(AttrSketchInput::BigInteger(&bytes)))
            } else {
                Some(sketch.estimate(AttrSketchInput::Double(value.as_f64()?)))
            }
        }
        Value::Null => Some(sketch.estimate(AttrSketchInput::Nil)),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn estimate_entity_uuid<B: AsRef<[u64]>>(sketch: &AttrSketch<B>, value: &Value) -> Option<u64> {
    let entity_id = value.as_str()?.parse::<Uuid>().ok()?;
    Some(sketch.estimate(AttrSketchInput::String(&entity_id.to_string())))
}

fn estimate_entity_lookup<B: AsRef<[u64]>>(
    sketches: &HashMap<Uuid, AttrSketch<B>>,
    value: &Value,
) -> Option<u64> {
    let parts = value.as_array()?;
    if parts.len() != 2 {
        return None;
    }
    let attribute_id = parts[0].as_str()?.parse::<Uuid>().ok()?;
    let sketch = sketches.get(&attribute_id)?;
    estimate_json_value(sketch, &parts[1], None).map(|estimate| estimate.min(1))
}

/// Executes an already planned recursive `InstaQL` query.
///
/// # Errors
///
/// Returns [`PgQueryError`] for rendering, execution, envelope decoding, join
/// scoping, or an invalid selector/fetch hierarchy.
pub async fn query_planned_instaql<C>(
    client: &C,
    plan: &PlannedNestedQuery,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
{
    query_planned_instaql_with_timeout(client, plan, DEFAULT_QUERY_TIMEOUT).await
}

async fn query_planned_instaql_with_timeout<C>(
    client: &C,
    plan: &PlannedNestedQuery,
    timeout: Duration,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
{
    query_planned_instaql_with_timeout_and_preparer(
        client,
        plan,
        timeout,
        &UncachedQueryPreparer,
        MainQueryProtocol::Described,
    )
    .await
}

async fn query_planned_instaql_with_timeout_and_preparer<C, P>(
    client: &C,
    plan: &PlannedNestedQuery,
    timeout: Duration,
    preparer: &P,
    protocol: MainQueryProtocol,
) -> Result<Vec<ExecutedNode>, PgQueryError>
where
    C: CancelableGenericClient,
    P: QueryStatementPreparer,
{
    if plan.groups.is_empty() {
        return Ok(Vec::new());
    }
    if plan.ctes.is_empty() {
        return decode_planned_instaql_envelope(
            plan,
            &Value::Array(vec![Value::Object(serde_json::Map::new())]),
        );
    }
    let rendered = if matches!(protocol, MainQueryProtocol::Structural) {
        render_nested_match_query_with_structural_constants(&plan.sql_plan())?
    } else {
        render_nested_match_query(&plan.sql_plan())?
    };
    let rows = if matches!(protocol, MainQueryProtocol::Typed) {
        query_with_builtin_parameter_types(client, &rendered, timeout).await?
    } else {
        query_with_timeout_and_preparer(client, &rendered, timeout, preparer).await?
    };
    if rows.len() != 1 {
        return Err(PgQueryError::UnexpectedNestedRowCount(rows.len()));
    }
    let raw = rows[0].try_get::<_, RawJsonText<'_>>(0)?;
    decode_planned_instaql_envelope_text(plan, raw.text())
}

/// Raw text of one PostgreSQL `json`/`jsonb` result cell.
///
/// Reading the cell as text lets the nested decoder borrow the envelope instead
/// of materializing an owned [`Value`] tree for every subscription refresh.
#[derive(Debug)]
struct RawJsonText<'a>(&'a str);

impl<'a> RawJsonText<'a> {
    fn text(&self) -> &'a str {
        self.0
    }
}

impl<'a> FromSql<'a> for RawJsonText<'a> {
    fn from_sql(
        ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        // `json` is transmitted verbatim; `jsonb` prefixes a format version.
        let text = if *ty == Type::JSON {
            raw
        } else if *ty == Type::JSONB {
            match raw.split_first() {
                Some((1, body)) => body,
                _ => return Err("unsupported jsonb binary format version".into()),
            }
        } else {
            return Err(format!("cannot read PostgreSQL type {ty} as JSON text").into());
        };
        Ok(Self(std::str::from_utf8(text)?))
    }

    fn accepts(ty: &Type) -> bool {
        *ty == Type::JSON || *ty == Type::JSONB
    }
}

fn remaining_query_timeout(
    deadline: tokio::time::Instant,
    requested: Duration,
) -> Result<Duration, PgQueryError> {
    deadline
        .checked_duration_since(tokio::time::Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| query_timeout_error(requested))
}

fn normalize_query_timeout(error: PgQueryError, requested: Duration) -> PgQueryError {
    if matches!(error, PgQueryError::QueryTimeout { .. }) {
        query_timeout_error(requested)
    } else {
        error
    }
}

fn query_deadline(timeout: Duration) -> Result<tokio::time::Instant, PgQueryError> {
    tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| query_timeout_error(timeout))
}

fn query_timeout_error(timeout: Duration) -> PgQueryError {
    PgQueryError::QueryTimeout {
        milliseconds: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    }
}

/// Decodes the JSON value produced by a planned nested `InstaQL` statement.
///
/// This transport boundary is public so alternate PostgreSQL clients and fixture
/// tests can reuse the exact recursive regrouping contract.
///
/// # Errors
///
/// Returns [`PgQueryError`] for an invalid chunked envelope, missing result table,
/// malformed Datalog row, invalid entity join, or selector/fetch shape mismatch.
pub fn decode_planned_instaql_envelope(
    plan: &PlannedNestedQuery,
    value: &Value,
) -> Result<Vec<ExecutedNode>, PgQueryError> {
    let envelope = merge_nested_envelope(value)?;
    decode_planned_instaql_groups(plan, &envelope)
}

/// Decodes the raw JSON text of the result cell produced by a planned nested
/// `InstaQL` statement, without materializing an owned [`Value`] tree.
///
/// This is the production transport boundary; it returns exactly what
/// [`decode_planned_instaql_envelope`] returns for the same JSON document.
/// Malformed JSON text is reported as [`PgQueryError::InvalidNestedEnvelope`].
///
/// # Errors
///
/// Returns the same errors as [`decode_planned_instaql_envelope`].
pub fn decode_planned_instaql_envelope_text(
    plan: &PlannedNestedQuery,
    text: &str,
) -> Result<Vec<ExecutedNode>, PgQueryError> {
    let envelope = parse_nested_envelope_text(text)?;
    decode_planned_instaql_groups(plan, &envelope)
}

fn decode_planned_instaql_groups(
    plan: &PlannedNestedQuery,
    envelope: &NestedEnvelope<'_>,
) -> Result<Vec<ExecutedNode>, PgQueryError> {
    let mut scoped_rows = HashMap::new();
    decode_nested_groups(&plan.groups, envelope, None, &mut scoped_rows)?
        .into_iter()
        .map(decoded_selector_to_executed)
        .collect()
}

/// One result table in a chunked nested envelope, either already parsed as a
/// [`Value`] or borrowed raw JSON text.
#[derive(Clone, Copy)]
enum NestedTable<'a> {
    Value(&'a Value),
    Text(&'a str),
}

impl NestedTable<'_> {
    fn count(self) -> Option<i64> {
        match self {
            Self::Value(value) => value.as_i64(),
            Self::Text(text) => text.trim().parse().ok(),
        }
    }

    fn page_flag(self, table: &str) -> Result<bool, PgQueryError> {
        match self {
            Self::Value(value) => page_flag_value(Some(value), table),
            Self::Text(text) => {
                raw_page_flag(text).ok_or_else(|| PgQueryError::InvalidPageFlag(table.to_owned()))
            }
        }
    }

    fn decode(
        self,
        metadata: &[PatternMetadataNode],
        options: ResultDecodeOptions,
    ) -> Result<DatalogResult, ResultDecodeError> {
        match self {
            Self::Value(value) => decode_json_query_rows_with_options(value, metadata, options),
            Self::Text(text) => decode_raw_json_query_rows_with_options(text, metadata, options),
        }
    }

    fn decode_paged(
        self,
        metadata: &[PatternMetadataNode],
        page: &PagePlanMetadata,
        has_next: bool,
        has_previous: bool,
        options: ResultDecodeOptions,
    ) -> Result<DatalogResult, ResultDecodeError> {
        match self {
            Self::Value(value) => decode_json_paged_query_rows_with_options(
                value,
                metadata,
                page,
                has_next,
                has_previous,
                options,
            ),
            Self::Text(text) => decode_raw_json_paged_query_rows_with_options(
                text,
                metadata,
                page,
                has_next,
                has_previous,
                options,
            ),
        }
    }
}

/// Per-parent rows for one nested group, indexed on either representation.
enum ScopedNestedRows<'a> {
    Value(UuidGroupedJsonRows<'a>),
    Text(UuidGroupedRawJsonRows<'a>),
}

impl<'a> ScopedNestedRows<'a> {
    fn new(
        table: NestedTable<'a>,
        metadata: &[PatternMetadataNode],
        symbol: &Symbol,
    ) -> Result<Self, ResultDecodeError> {
        match table {
            NestedTable::Value(value) => {
                UuidGroupedJsonRows::new(value, metadata, symbol).map(Self::Value)
            }
            NestedTable::Text(text) => {
                UuidGroupedRawJsonRows::new(text, metadata, symbol).map(Self::Text)
            }
        }
    }

    fn decode_with_options(
        &self,
        parent: Uuid,
        metadata: &[PatternMetadataNode],
        options: ResultDecodeOptions,
    ) -> Result<DatalogResult, ResultDecodeError> {
        match self {
            Self::Value(rows) => rows.decode_with_options(parent, metadata, options),
            Self::Text(rows) => rows.decode_with_options(parent, metadata, options),
        }
    }

    fn decode_paged_with_options(
        &self,
        parent: Uuid,
        metadata: &[PatternMetadataNode],
        page: &PagePlanMetadata,
        has_next: bool,
        has_previous: bool,
        options: ResultDecodeOptions,
    ) -> Result<DatalogResult, ResultDecodeError> {
        match self {
            Self::Value(rows) => rows.decode_paged_with_options(
                parent,
                metadata,
                page,
                has_next,
                has_previous,
                options,
            ),
            Self::Text(rows) => rows.decode_paged_with_options(
                parent,
                metadata,
                page,
                has_next,
                has_previous,
                options,
            ),
        }
    }
}

type NestedEnvelope<'a> = BTreeMap<Cow<'a, str>, NestedTable<'a>>;
type NestedRowGroups<'a> = HashMap<(usize, Symbol), ScopedNestedRows<'a>>;

fn merge_nested_envelope(value: &Value) -> Result<NestedEnvelope<'_>, PgQueryError> {
    let chunks = value
        .as_array()
        .ok_or(PgQueryError::InvalidNestedEnvelope)?;
    let mut envelope = BTreeMap::new();
    for chunk in chunks {
        let object = chunk
            .as_object()
            .ok_or(PgQueryError::InvalidNestedEnvelope)?;
        // The PostgreSQL response outlives all regrouping. Merge only borrowed
        // table references, retaining the same last-chunk-wins semantics, not
        // another owned copy of every JSON row and field name.
        envelope.extend(
            object
                .iter()
                .map(|(key, value)| (Cow::Borrowed(key.as_str()), NestedTable::Value(value))),
        );
    }
    Ok(envelope)
}

fn parse_nested_envelope_text(text: &str) -> Result<NestedEnvelope<'_>, PgQueryError> {
    // One borrowed pass: chunks stay raw table slices, avoiding an owned row
    // tree for every subscription refresh.
    let chunks = serde_json::from_str::<Vec<BTreeMap<Cow<'_, str>, &RawValue>>>(text)
        .map_err(|_| PgQueryError::InvalidNestedEnvelope)?;
    let mut envelope = BTreeMap::new();
    for chunk in chunks {
        // Same chunk order and last-chunk-wins merge as the `Value` envelope.
        envelope.extend(
            chunk
                .into_iter()
                .map(|(key, value)| (key, NestedTable::Text(value.get()))),
        );
    }
    Ok(envelope)
}

/// Reads the boolean `exists` row of a raw JSON page-flag table.
fn raw_page_flag(text: &str) -> Option<bool> {
    let rows: Vec<&RawValue> = serde_json::from_str(text).ok()?;
    let row: BTreeMap<Cow<'_, str>, &RawValue> = serde_json::from_str(rows.first()?.get()).ok()?;
    serde_json::from_str(row.get("exists")?.get()).ok()
}

#[derive(Debug)]
struct DecodedNestedGroup {
    table: String,
    result: DatalogResult,
    aggregate: Option<i64>,
    children: Vec<Vec<DecodedNestedGroup>>,
}

fn decode_nested_groups<'a>(
    groups: &[PlannedNestedGroup],
    envelope: &NestedEnvelope<'a>,
    parent: Option<(&Symbol, Uuid)>,
    scoped_rows: &mut NestedRowGroups<'a>,
) -> Result<Vec<DecodedNestedGroup>, PgQueryError> {
    groups
        .iter()
        .map(|group| decode_nested_group(group, envelope, parent, scoped_rows))
        .collect()
}

fn decode_nested_group<'a>(
    group: &PlannedNestedGroup,
    envelope: &NestedEnvelope<'a>,
    parent: Option<(&Symbol, Uuid)>,
    scoped_rows: &mut NestedRowGroups<'a>,
) -> Result<DecodedNestedGroup, PgQueryError> {
    if let Some(clauses) = &group.missing_clauses {
        return Ok(DecodedNestedGroup {
            table: "missing-attribute".to_owned(),
            result: missing_attribute_result(clauses),
            aggregate: None,
            children: Vec::new(),
        });
    }
    let table = group
        .table
        .as_ref()
        .ok_or(PgQueryError::MissingNestedResultTable)?;
    let value = envelope
        .get(table.as_str())
        .copied()
        .ok_or_else(|| PgQueryError::MissingBatchKey(table.clone()))?;
    if group.aggregate.is_some() {
        let count = value
            .count()
            .ok_or_else(|| PgQueryError::InvalidNestedCount(table.clone()))?;
        return Ok(DecodedNestedGroup {
            table: table.clone(),
            result: DatalogResult::default(),
            aggregate: Some(count),
            children: Vec::new(),
        });
    }

    let (indexed, metadata) = if let Some((symbol, entity_id)) = parent {
        // Exact group identity, not just its table name: alternate plans can
        // interpret one table with different metadata. This index lives only
        // inside this decode of one fresh PostgreSQL response.
        let key = (std::ptr::from_ref(group).addr(), symbol.clone());
        let rows = match scoped_rows.entry(key) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(ScopedNestedRows::new(value, &group.metadata, symbol)?)
            }
        };
        (
            Some((rows, entity_id)),
            Cow::Owned(bind_pattern_metadata_uuid_symbol(
                &group.metadata,
                symbol,
                entity_id,
            )),
        )
    } else {
        // An unscoped root needs no filtering or metadata binding. Decode the
        // original immutable response directly, with identical validation.
        (None, Cow::Borrowed(group.metadata.as_slice()))
    };
    let options =
        result_decode_options(group.ensure_empty_symbol_values_for_topics.unwrap_or(false));
    let result = if let Some(page) = &group.page {
        let has_next = nested_page_flag(envelope, &page.has_next_table)?;
        let has_previous = nested_page_flag(envelope, &page.has_previous_table)?;
        match indexed {
            Some((rows, parent)) => rows.decode_paged_with_options(
                parent,
                &metadata,
                page,
                has_next,
                has_previous,
                options,
            )?,
            None => value.decode_paged(&metadata, page, has_next, has_previous, options)?,
        }
    } else {
        match indexed {
            Some((rows, parent)) => rows.decode_with_options(parent, &metadata, options)?,
            None => value.decode(&metadata, options)?,
        }
    };

    let children = if let Some(children) = &group.children {
        let mut entity_ids = result
            .symbol_values
            .get(&children.join_symbol)
            .into_iter()
            .flatten()
            .map(|value| match value {
                DatalogValue::Uuid(value) => Ok(*value),
                DatalogValue::Json(value) => Err(ResultDecodeError::InvalidNestedJoinValue {
                    symbol: children.join_symbol.as_str().to_owned(),
                    value: value.to_string(),
                }),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if entity_ids.is_empty()
            && let Some((parent_symbol, entity_id)) = parent
            && parent_symbol == &children.join_symbol
        {
            entity_ids.push(entity_id);
        }
        entity_ids
            .into_iter()
            .map(|entity_id| {
                decode_nested_groups(
                    &children.groups,
                    envelope,
                    Some((&children.join_symbol, entity_id)),
                    scoped_rows,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(DecodedNestedGroup {
        table: table.clone(),
        result,
        aggregate: None,
        children,
    })
}

fn decoded_selector_to_executed(
    selector: DecodedNestedGroup,
) -> Result<ExecutedNode, PgQueryError> {
    let entities = selector
        .children
        .into_iter()
        .map(|mut fetch_groups| {
            if fetch_groups.len() != 1 {
                return Err(PgQueryError::InvalidNestedFetchCardinality {
                    table: selector.table.clone(),
                    actual: fetch_groups.len(),
                });
            }
            let mut fetch = fetch_groups.remove(0);
            let child_selectors = match fetch.children.len() {
                0 => Vec::new(),
                1 => fetch.children.remove(0),
                actual => {
                    return Err(PgQueryError::InvalidNestedChildCardinality {
                        table: fetch.table,
                        actual,
                    });
                }
            };
            Ok(ExecutedEntity {
                fetch: fetch.result,
                children: child_selectors
                    .into_iter()
                    .map(decoded_selector_to_executed)
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(ExecutedNode {
        selector: selector.result,
        aggregate: selector.aggregate,
        entities,
    })
}

fn batch_page_flag(
    envelope: &serde_json::Map<String, Value>,
    table: &str,
) -> Result<bool, PgQueryError> {
    page_flag_value(envelope.get(table), table)
}

fn nested_page_flag(envelope: &NestedEnvelope<'_>, table: &str) -> Result<bool, PgQueryError> {
    envelope.get(table).copied().map_or_else(
        || Err(PgQueryError::InvalidPageFlag(table.to_owned())),
        |value| value.page_flag(table),
    )
}

fn page_flag_value(value: Option<&Value>, table: &str) -> Result<bool, PgQueryError> {
    value
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(Value::as_object)
        .and_then(|row| row.get("exists"))
        .and_then(Value::as_bool)
        .ok_or_else(|| PgQueryError::InvalidPageFlag(table.to_owned()))
}

/// Plans and executes one loader tranche, preserving request order.
///
/// A single request uses the direct row path; multiple requests use the JSON
/// batch envelope. This mirrors the JVM loader's optimized single-item behavior.
///
/// # Errors
///
/// Returns [`PgQueryError`] when planning, rendering, binding, execution, or
/// decoding any request fails. The caller should fan the error out to the tranche.
pub async fn execute_datalog_requests<C>(
    client: &C,
    requests: &[DatalogBatchRequest],
) -> Result<Vec<DatalogResult>, PgQueryError>
where
    C: CancelableGenericClient,
{
    let mixed = requests
        .iter()
        .cloned()
        .map(MixedDatalogBatchRequest::Rows)
        .collect::<Vec<_>>();
    execute_mixed_datalog_requests(client, &mixed)
        .await?
        .into_iter()
        .map(|result| match result {
            MixedDatalogBatchResult::Rows(result) => Ok(*result),
            MixedDatalogBatchResult::Count(_) => unreachable!("row-only request returned count"),
        })
        .collect()
}

/// Plans and executes one heterogeneous loader tranche, preserving request order.
///
/// A single unpaged row request and a single count request retain their direct
/// optimized paths. Every other tranche uses one mixed JSONB statement.
///
/// # Errors
///
/// Returns [`PgQueryError`] when planning, rendering, binding, execution, or
/// decoding any request fails, or when a count request contains pagination.
pub async fn execute_mixed_datalog_requests<C>(
    client: &C,
    requests: &[MixedDatalogBatchRequest],
) -> Result<Vec<MixedDatalogBatchResult>, PgQueryError>
where
    C: CancelableGenericClient,
{
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let additional_joins = HashMap::new();
    let plans = requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let (request, kind) = match request {
                MixedDatalogBatchRequest::Rows(request) => (request, BatchResultKind::Rows),
                MixedDatalogBatchRequest::Count(request) => (request, BatchResultKind::Count),
            };
            if kind == BatchResultKind::Count && request.page.is_some() {
                return Err(PgQueryError::CountRequestHasPage);
            }
            let prefix = format!("match-{index}-");
            let context = MatchQueryContext {
                prefix: &prefix,
                app_id: request.app_id,
                additional_joins: &additional_joins,
                initial_symbol_cardinalities: &HashMap::new(),
                options: &request.options,
            };
            let mut plan = plan_match_query(&context, &request.clauses)?;
            if let Some(page) = &request.page {
                add_page_info(&mut plan, &context, page)?;
            }
            Ok::<_, PgQueryError>(plan)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let kinds = requests
        .iter()
        .map(|request| match request {
            MixedDatalogBatchRequest::Rows(_) => BatchResultKind::Rows,
            MixedDatalogBatchRequest::Count(_) => BatchResultKind::Count,
        })
        .collect::<Vec<_>>();

    if let ([plan], [kind]) = (plans.as_slice(), kinds.as_slice()) {
        return match kind {
            BatchResultKind::Rows if plan.page.is_none() => {
                let query = render_match_query(plan)?;
                Ok(vec![MixedDatalogBatchResult::Rows(Box::new(
                    query_datalog(client, &query, plan).await?,
                ))])
            }
            BatchResultKind::Count => Ok(vec![MixedDatalogBatchResult::Count(
                query_datalog_count(client, plan).await?,
            )]),
            BatchResultKind::Rows => {
                let query = render_mixed_batch_match_query(&plans, &kinds)?;
                query_mixed_datalog_batch(client, &query, &plans, &kinds).await
            }
        };
    }
    let query = render_mixed_batch_match_query(&plans, &kinds)?;
    query_mixed_datalog_batch(client, &query, &plans, &kinds).await
}

/// Builds a cancellation-safe per-client loader that coalesces heterogeneous
/// Datalog row and count requests into batches of at most 50.
#[must_use]
pub fn mixed_datalog_batch_loader<C>(
    client: Arc<C>,
) -> BatchLoader<MixedDatalogBatchRequest, MixedDatalogBatchResult, PgQueryError>
where
    C: CancelableGenericClient + Send + Sync + 'static,
{
    BatchLoader::new(move |requests| {
        let client = Arc::clone(&client);
        async move { execute_mixed_datalog_requests(client.as_ref(), &requests).await }
    })
}

/// Converts already-fetched PostgreSQL rows into the Datalog result contract.
///
/// # Errors
///
/// Returns [`PgQueryError`] when a projected column is missing, has an unexpected
/// PostgreSQL type, or violates the Datalog metadata contract.
pub fn decode_postgres_rows(
    rows: &[Row],
    metadata: &[PatternMetadataNode],
) -> Result<DatalogResult, PgQueryError> {
    decode_postgres_rows_with_options(rows, metadata, ResultDecodeOptions::default())
}

/// Converts fetched PostgreSQL rows with live result-compatibility controls.
///
/// # Errors
///
/// Returns [`PgQueryError`] under the same conditions as [`decode_postgres_rows`].
pub fn decode_postgres_rows_with_options(
    rows: &[Row],
    metadata: &[PatternMetadataNode],
    options: ResultDecodeOptions,
) -> Result<DatalogResult, PgQueryError> {
    let patterns = flattened_metadata(metadata);
    let query_rows = rows
        .iter()
        .map(|row| postgres_query_row(row, &patterns))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(decode_query_rows_with_options(
        &query_rows,
        metadata,
        options,
    )?)
}

const fn result_decode_options(ensure_empty_symbol_values_for_topics: bool) -> ResultDecodeOptions {
    ResultDecodeOptions {
        ensure_empty_symbol_values_for_topics,
    }
}

fn flattened_metadata(metadata: &[PatternMetadataNode]) -> Vec<&PatternMetadata> {
    let mut patterns = Vec::new();
    for node in metadata {
        match node {
            PatternMetadataNode::Pattern(pattern) => patterns.push(pattern),
            PatternMetadataNode::Or(branches) => {
                for branch in branches {
                    patterns.extend(flattened_metadata(branch));
                }
            }
        }
    }
    patterns
}

fn postgres_query_row(row: &Row, metadata: &[&PatternMetadata]) -> Result<QueryRow, PgQueryError> {
    let mut query_row = QueryRow::default();
    for pattern in metadata {
        let [entity, attribute, value, reference, created_at] = pattern.cte_columns.as_slice()
        else {
            return Err(
                ResultDecodeError::InvalidMetadataColumns(pattern.cte_columns.len()).into(),
            );
        };
        query_row.cells.insert(
            entity.clone(),
            QueryCell::Uuid(row.try_get::<_, Option<Uuid>>(entity.as_str())?),
        );
        query_row.cells.insert(
            attribute.clone(),
            QueryCell::Uuid(row.try_get::<_, Option<Uuid>>(attribute.as_str())?),
        );
        query_row.cells.insert(
            value.clone(),
            QueryCell::Json(
                row.try_get::<_, Option<Json<Value>>>(value.as_str())?
                    .map(|Json(value)| value),
            ),
        );
        query_row.cells.insert(
            reference.clone(),
            QueryCell::Boolean(row.try_get::<_, Option<bool>>(reference.as_str())?),
        );
        query_row.cells.insert(
            created_at.clone(),
            QueryCell::Integer(row.try_get::<_, Option<i64>>(created_at.as_str())?),
        );
    }
    Ok(query_row)
}

fn bind_parameter(parameter: &SqlParameter) -> Result<BoundParameter, PgQueryError> {
    match parameter {
        SqlParameter::Boolean(value) => Ok(BoundParameter::Boolean(*value)),
        SqlParameter::Integer(value) => Ok(BoundParameter::Integer(*value)),
        SqlParameter::Float(value) => Ok(BoundParameter::Float(*value)),
        SqlParameter::Text(value) => Ok(BoundParameter::Text(value.clone())),
        SqlParameter::Json(value) => Ok(BoundParameter::Json(Json(value.clone()))),
        SqlParameter::Uuid(value) => Ok(BoundParameter::Uuid(*value)),
        SqlParameter::Timestamp(value) => Ok(BoundParameter::Timestamp(*value)),
        SqlParameter::Array {
            values,
            element_type,
        } => bind_array(values, *element_type),
    }
}

fn bind_array(values: &[Value], element_type: PgType) -> Result<BoundParameter, PgQueryError> {
    match element_type {
        PgType::Bigint => Ok(BoundParameter::IntegerArray(map_nullable(
            values,
            element_type,
            Value::as_i64,
        )?)),
        PgType::Jsonb => Ok(BoundParameter::JsonArray(
            values
                .iter()
                .map(|value| Some(Json(value.clone())))
                .collect(),
        )),
        PgType::Uuid => Ok(BoundParameter::UuidArray(map_nullable(
            values,
            element_type,
            |value| value.as_str().and_then(|value| Uuid::parse_str(value).ok()),
        )?)),
        PgType::TimestampWithTimeZone => Ok(BoundParameter::TimestampArray(map_nullable(
            values,
            element_type,
            |value| {
                value
                    .as_str()
                    .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.with_timezone(&Utc))
            },
        )?)),
        PgType::Float8 => Ok(BoundParameter::FloatArray(map_nullable(
            values,
            element_type,
            Value::as_f64,
        )?)),
        PgType::Text => Ok(BoundParameter::TextArray(map_nullable(
            values,
            element_type,
            |value| value.as_str().map(ToOwned::to_owned),
        )?)),
        PgType::Boolean => Ok(BoundParameter::BooleanArray(map_nullable(
            values,
            element_type,
            Value::as_bool,
        )?)),
        // `checked_data_type` is a schema-defined enum. Encode it with the
        // server-provided array metadata rather than assuming a Rust enum type.
        PgType::CheckedDataType => Ok(BoundParameter::GenericArray(GenericArrayParameter(
            values.to_vec(),
        ))),
    }
}

fn map_nullable<T>(
    values: &[Value],
    element_type: PgType,
    convert: impl Fn(&Value) -> Option<T>,
) -> Result<Vec<Option<T>>, PgQueryError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            if value.is_null() {
                Ok(None)
            } else {
                convert(value)
                    .map(Some)
                    .ok_or_else(|| PgQueryError::InvalidArrayElement {
                        index,
                        element_type,
                        value: value.clone(),
                    })
            }
        })
        .collect()
}

#[cfg(test)]
mod nested_envelope_borrow_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_builtin_types_match_scalar_and_nullable_array_binders() {
        let timestamp = DateTime::parse_from_rfc3339("2026-10-20T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut parameters = vec![
            SqlParameter::Boolean(true),
            SqlParameter::Integer(7),
            SqlParameter::Float(1.5),
            SqlParameter::Text("value".into()),
            SqlParameter::Json(Value::Null),
            SqlParameter::Uuid(Uuid::nil()),
            SqlParameter::Timestamp(timestamp),
        ];
        let kinds = [
            PgType::Boolean,
            PgType::Bigint,
            PgType::Float8,
            PgType::Text,
            PgType::Jsonb,
            PgType::Uuid,
            PgType::TimestampWithTimeZone,
        ];
        for element_type in kinds {
            parameters.push(SqlParameter::Array {
                values: vec![Value::Null],
                element_type,
            });
        }
        let bound = bind_parameters(&parameters).unwrap();
        assert_eq!(
            bound
                .iter()
                .map(BoundParameter::builtin_type)
                .collect::<Option<Vec<_>>>()
                .unwrap(),
            vec![
                Type::BOOL,
                Type::INT8,
                Type::FLOAT8,
                Type::TEXT,
                Type::JSONB,
                Type::UUID,
                Type::TIMESTAMPTZ,
                Type::BOOL_ARRAY,
                Type::INT8_ARRAY,
                Type::FLOAT8_ARRAY,
                Type::TEXT_ARRAY,
                Type::JSONB_ARRAY,
                Type::UUID_ARRAY,
                Type::TIMESTAMPTZ_ARRAY
            ]
        );
        let custom = bind_parameters(&[SqlParameter::Array {
            values: vec![json!("number")],
            element_type: PgType::CheckedDataType,
        }])
        .unwrap();
        assert!(
            custom[0].builtin_type().is_none(),
            "never guess a schema-defined OID"
        );
    }

    #[tokio::test]
    async fn native_typed_protocol_preserves_fresh_values_custom_type_fallback_and_cancel_drain() {
        let Some(url) = instant_test_support::test_database_url(
            "native_typed_protocol_preserves_fresh_values_custom_type_fallback_and_cancel_drain",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let connection = tokio::spawn(connection);
        for value in [7, 11] {
            let query = RenderedQuery {
                sql: "SELECT jsonb_build_array($1::bigint, $2::jsonb, $3::uuid[], $4::text[], $5::double precision)".into(),
                parameters: vec![SqlParameter::Integer(value), SqlParameter::Json(Value::Null),
                    SqlParameter::Array { values: vec![json!(Uuid::nil()), Value::Null], element_type: PgType::Uuid },
                    SqlParameter::Array { values: vec![json!("same query, fresh values"), Value::Null], element_type: PgType::Text },
                    SqlParameter::Float(1.5)],
            };
            let original = query_with_timeout(&client, &query, Duration::from_secs(2))
                .await
                .unwrap();
            let typed = query_with_builtin_parameter_types(&client, &query, Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(
                original[0].get::<_, Json<Value>>(0).0,
                typed[0].get::<_, Json<Value>>(0).0
            );
            assert_eq!(typed[0].get::<_, Json<Value>>(0).0[0], json!(value));
        }
        let custom = RenderedQuery {
            sql: "SELECT to_jsonb($1::checked_data_type[])".into(),
            parameters: vec![SqlParameter::Array {
                values: vec![json!("number"), Value::Null],
                element_type: PgType::CheckedDataType,
            }],
        };
        let rows = query_with_builtin_parameter_types(&client, &custom, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, Json<Value>>(0).0, json!(["number", null]));
        let slow = RenderedQuery {
            sql: "SELECT pg_sleep(5), $1::bigint".into(),
            parameters: vec![SqlParameter::Integer(1)],
        };
        let error = query_with_builtin_parameter_types(&client, &slow, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PgQueryError::QueryTimeout { milliseconds: 20 }
        ));
        assert_eq!(
            client
                .query_one("SELECT 1::bigint", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            1
        );
        drop(client);
        connection.await.unwrap().unwrap();
    }

    #[test]
    fn binds_all_array_types_emitted_by_datalog() {
        let parameters = vec![
            SqlParameter::Array {
                values: vec![json!("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"), Value::Null],
                element_type: PgType::Uuid,
            },
            SqlParameter::Array {
                values: vec![json!({"nested": true}), Value::Null],
                element_type: PgType::Jsonb,
            },
            SqlParameter::Array {
                values: vec![json!("2026-08-27T15:30:00-06:00")],
                element_type: PgType::TimestampWithTimeZone,
            },
            SqlParameter::Array {
                values: vec![json!(1), json!(2.5)],
                element_type: PgType::Float8,
            },
            SqlParameter::Array {
                values: vec![json!("one"), Value::Null],
                element_type: PgType::Text,
            },
            SqlParameter::Array {
                values: vec![json!(true), Value::Null],
                element_type: PgType::Boolean,
            },
        ];

        let bound = bind_parameters(&parameters).unwrap();
        assert!(matches!(&bound[0], BoundParameter::UuidArray(values) if values.len() == 2));
        assert!(matches!(&bound[1], BoundParameter::JsonArray(values) if values.len() == 2));
        assert!(matches!(&bound[2], BoundParameter::TimestampArray(values) if values.len() == 1));
        assert!(matches!(&bound[3], BoundParameter::FloatArray(values) if values.len() == 2));
        assert!(matches!(&bound[4], BoundParameter::TextArray(values) if values.len() == 2));
        assert!(matches!(&bound[5], BoundParameter::BooleanArray(values) if values.len() == 2));
    }

    #[test]
    fn rejects_malformed_typed_array_elements_before_querying() {
        let error = bind_parameters(&[SqlParameter::Array {
            values: vec![json!("not-a-uuid")],
            element_type: PgType::Uuid,
        }])
        .unwrap_err();
        assert!(matches!(
            error,
            PgQueryError::InvalidArrayElement {
                index: 0,
                element_type: PgType::Uuid,
                ..
            }
        ));
    }

    #[test]
    fn binds_unknown_array_types_with_driver_metadata_and_postgres_text_format() {
        let bound = bind_parameters(&[SqlParameter::Array {
            values: vec![json!("draft"), json!("contains, punctuation"), Value::Null],
            element_type: PgType::CheckedDataType,
        }])
        .unwrap();
        let BoundParameter::GenericArray(array) = &bound[0] else {
            panic!("checked data type array should use the generic binder");
        };
        let custom_array = Type::new(
            "_post_status".to_owned(),
            42_001,
            Kind::Array(Type::new(
                "post_status".to_owned(),
                42_000,
                Kind::Enum(vec!["draft".to_owned(), "published".to_owned()]),
                "public".to_owned(),
            )),
            "public".to_owned(),
        );

        assert!(GenericArrayParameter::accepts(&custom_array));
        assert!(!GenericArrayParameter::accepts(&Type::TEXT));
        let mut encoded = BytesMut::new();
        assert!(matches!(
            array.to_sql(&custom_array, &mut encoded),
            Ok(IsNull::No)
        ));
        assert_eq!(
            encoded.as_ref(),
            br#"{"draft","contains, punctuation",NULL}"#
        );
        assert!(matches!(array.encode_format(&custom_array), Format::Text));
    }

    #[test]
    fn cardinality_reads_use_native_uuid_bindings() {
        let app_id = Uuid::new_v4();
        let attribute_ids = [Uuid::new_v4(), Uuid::new_v4()];
        let query = attribute_cardinality_query(
            "SELECT attr_id, total FROM attr_sketches WHERE app_id = $1 AND attr_id = ANY($2)",
            app_id,
            &attribute_ids,
        );

        assert_eq!(query.parameters[0], SqlParameter::Uuid(app_id));
        assert_eq!(
            query.parameters[1],
            SqlParameter::Array {
                values: attribute_ids
                    .iter()
                    .map(|attribute_id| json!(attribute_id.to_string()))
                    .collect(),
                element_type: PgType::Uuid,
            }
        );
        let bound = bind_parameters(&query.parameters).unwrap();
        assert!(matches!(&bound[0], BoundParameter::Uuid(value) if *value == app_id));
        assert!(matches!(
            &bound[1],
            BoundParameter::UuidArray(values)
                if values == &attribute_ids.iter().copied().map(Some).collect::<Vec<_>>()
        ));
    }

    #[test]
    fn sketch_snapshot_binds_one_app_and_deduplicated_uuid_projections() {
        let app = Uuid::new_v4();
        let total = Uuid::new_v4();
        let forward = Uuid::new_v4();
        let reverse = Uuid::new_v4();
        let query = sketch_snapshot_query(app, &[total], &[forward], &[forward, reverse]);
        let bound = bind_parameters(&query.parameters).unwrap();
        assert_eq!(bound.len(), 4);
        assert!(matches!(&bound[0],BoundParameter::Uuid(id)if *id==app));
        let BoundParameter::UuidArray(all) = &bound[1] else {
            panic!("native UUID array required")
        };
        assert_eq!(
            all.iter().flatten().copied().collect::<HashSet<_>>(),
            HashSet::from([total, forward, reverse])
        );
        assert_eq!(all.len(), 3);
        assert!(matches!(&bound[2],BoundParameter::UuidArray(ids)if ids==&vec![Some(forward)]));
        assert!(
            matches!(&bound[3],BoundParameter::UuidArray(ids)if ids==&vec![Some(forward),Some(reverse)])
        );
        assert!(query.sql.contains("CASE WHEN attr_id = ANY($3) THEN bins"));
        assert!(
            query
                .sql
                .contains("CASE WHEN attr_id = ANY($4) THEN reverse_bins")
        );
    }

    async fn read_test_sketches(
        client: &Client,
        compiled: &CompiledQuery,
        app: Uuid,
        options: &MatchCteOptions,
    ) -> MatchCteOptions {
        let timeout = Duration::from_secs(1);
        load_attribute_cardinalities(
            client,
            compiled,
            app,
            options,
            query_deadline(timeout).unwrap(),
            timeout,
        )
        .await
        .unwrap()
    }

    async fn assert_snapshot_calls(
        observer: &Client,
        pid: i32,
        expected: i64,
        initial_calls: &HashMap<i64, i64>,
    ) {
        let activity = observer
            .query_one(
                "SELECT query_id, query FROM pg_stat_activity WHERE pid=$1",
                &[&pid],
            )
            .await
            .unwrap();
        let sql: String = activity.get(1);
        assert!(
            sql.contains("CASE WHEN attr_id = ANY($3) THEN bins")
                && sql.contains("CASE WHEN attr_id = ANY($4) THEN reverse_bins"),
            "planner must finish with the combined sketch snapshot, not a separate projection"
        );
        // Ordinary test databases need no preloaded extension. Opt in on
        // disposable databases to independently count server executions.
        if std::env::var("TEST_DATABASE_STATEMENT_COUNTS").as_deref() != Ok("1") {
            return;
        }
        let id = activity.get::<_, Option<i64>>(0).unwrap();
        let row = observer
            .query_one(
                "SELECT calls FROM pg_stat_statements WHERE queryid=$1",
                &[&id],
            )
            .await
            .unwrap();
        assert_eq!(
            row.get::<_, i64>(0) - initial_calls.get(&id).copied().unwrap_or_default(),
            expected,
            "exactly one snapshot statement per planner invocation"
        );
    }

    #[tokio::test]
    #[expect(
        clippy::too_many_lines,
        reason = "One connection-local fixture verifies statement counts, freshness and legacy projection parity without cross-test state."
    )]
    async fn sketch_snapshot_is_one_fresh_app_scoped_read_and_skips_unrequested_bins() {
        let Some(url) = instant_test_support::test_database_url(
            "sketch_snapshot_is_one_fresh_app_scoped_read_and_skips_unrequested_bins",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        client.batch_execute("CREATE TEMP TABLE attr_sketches(app_id uuid,attr_id uuid,width int,depth int,total bigint,total_not_binned bigint,bins bytea,reverse_width int,reverse_depth int,reverse_total bigint,reverse_bins bytea,PRIMARY KEY(app_id,attr_id))").await.unwrap();
        let (observer, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .unwrap();
        let observer_driver = tokio::spawn(connection);
        let pid = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0);
        let app = Uuid::new_v4();
        let other = Uuid::new_v4();
        let preserved = Uuid::new_v4();
        let catalog = instant_instaql::system_attribute_catalog().unwrap();
        let id = catalog.forward("$users", "id").unwrap().id;
        let email = catalog.forward("$users", "email").unwrap().id;
        let mut sketch = AttrSketch::empty(2, 2).unwrap();
        sketch.bins = vec![7; 4];
        sketch.total = 20;
        let bins = encode_attr_sketch_bins(&sketch).unwrap();
        for (owner, attribute, total, payload) in [
            (app, id, 10_i64, vec![0xff]),
            (app, email, 20, bins.clone()),
            (other, email, 999, bins.clone()),
        ] {
            client.execute("INSERT INTO attr_sketches(app_id,attr_id,width,depth,total,total_not_binned,bins) VALUES($1,$2,2,2,$3,0,$4)",&[&owner,&attribute,&total,&payload]).await.unwrap();
        }
        let compiled = instant_instaql::compile_query(
            &json!({"$users":{"$":{"where":{"email":"snapshot@example.invalid"},"limit":2}}}),
            &catalog,
            false,
        )
        .unwrap();
        let mut options = MatchCteOptions::default();
        options.attribute_cardinalities.insert(preserved, 123);
        let mut initial_calls = HashMap::new();
        if std::env::var("TEST_DATABASE_STATEMENT_COUNTS").as_deref() == Ok("1") {
            for row in observer
                .query("SELECT queryid,calls FROM pg_stat_statements", &[])
                .await
                .unwrap()
            {
                initial_calls.insert(row.get::<_, i64>(0), row.get::<_, i64>(1));
            }
        }
        let first = read_test_sketches(&client, &compiled, app, &options).await;
        assert_eq!(first.attribute_cardinalities[&id], 10);
        assert_eq!(first.attribute_cardinalities[&email], 20);
        assert_eq!(first.attribute_cardinalities[&preserved], 123);
        assert_snapshot_calls(&observer, pid, 1, &initial_calls).await;
        sketch.bins = vec![9; 4];
        sketch.total = 30;
        let updated = encode_attr_sketch_bins(&sketch).unwrap();
        client
            .execute(
                "UPDATE attr_sketches SET total=30,bins=$3 WHERE app_id=$1 AND attr_id=$2",
                &[&app, &email, &updated],
            )
            .await
            .unwrap();
        let second = {
            // Freshness is explicit now: planner snapshots are retained briefly,
            // so this contract test clears them before asking for new rows.
            planner_sketch_cache::clear();
            read_test_sketches(&client, &compiled, app, &options).await
        };
        assert_eq!(
            second.attribute_cardinalities[&email], 30,
            "clearing planner snapshots must make the next read observe new sketch rows"
        );
        assert_ne!(
            first.attribute_value_cardinalities,
            second.attribute_value_cardinalities
        );
        assert_snapshot_calls(&observer, pid, 2, &initial_calls).await;
        options.attribute_cardinalities.insert(id, 321);
        let third = read_test_sketches(&client, &compiled, app, &options).await;
        assert_eq!(
            third.attribute_cardinalities[&id], 321,
            "provided total-only estimates retain precedence"
        );
        assert_snapshot_calls(&observer, pid, 3, &initial_calls).await;
        let empty = CompiledQuery { roots: vec![] };
        let untouched = read_test_sketches(&client, &empty, app, &options).await;
        assert_eq!(
            untouched.attribute_cardinalities,
            options.attribute_cardinalities
        );
        assert_snapshot_calls(&observer, pid, 3, &initial_calls).await;
        // Compare all three decoders with their former independent projections,
        // including overlap, absent rows and nullable reverse sketches. The id
        // row deliberately has malformed forward bins: totals/reverse reads
        // must never decode an unrequested forward payload.
        client.execute("UPDATE attr_sketches SET reverse_width=2,reverse_depth=2,reverse_total=40,reverse_bins=$3 WHERE app_id=$1 AND attr_id=$2", &[&app, &email, &updated]).await.unwrap();
        let missing = Uuid::new_v4();
        let totals = [id, missing];
        let forwards = [email, missing];
        let reverses = [email, id, missing];
        let rows = query(
            &client,
            &sketch_snapshot_query(app, &totals, &forwards, &reverses),
        )
        .await
        .unwrap();
        let mut combined = MatchCteOptions::default();
        decode_total_sketch_rows(&rows, &totals, &mut combined).unwrap();
        let forward = decode_forward_sketch_rows(&rows, &forwards, &mut combined).unwrap();
        let reverse = decode_reverse_sketch_rows(&rows, &reverses, &mut combined).unwrap();
        let mut legacy = MatchCteOptions::default();
        let old_total = query(
            &client,
            &attribute_cardinality_query(
                "SELECT attr_id,total FROM attr_sketches WHERE app_id=$1 AND attr_id=ANY($2)",
                app,
                &totals,
            ),
        )
        .await
        .unwrap();
        decode_total_sketch_rows(&old_total, &totals, &mut legacy).unwrap();
        let old_forward = query(&client, &attribute_cardinality_query("SELECT attr_id,width,depth,total,total_not_binned,bins FROM attr_sketches WHERE app_id=$1 AND attr_id=ANY($2)",app,&forwards)).await.unwrap();
        let old_forward = decode_forward_sketch_rows(&old_forward, &forwards, &mut legacy).unwrap();
        let old_reverse = query(&client, &attribute_cardinality_query("SELECT attr_id,total,reverse_width,reverse_depth,reverse_total,reverse_bins FROM attr_sketches WHERE app_id=$1 AND attr_id=ANY($2)",app,&reverses)).await.unwrap();
        let old_reverse = decode_reverse_sketch_rows(&old_reverse, &reverses, &mut legacy).unwrap();
        assert_eq!(
            combined.attribute_cardinalities,
            legacy.attribute_cardinalities
        );
        assert_eq!(forward, old_forward);
        assert_eq!(reverse, old_reverse);
        assert_eq!(reverse[&email].total, 40);
        assert!(!reverse.contains_key(&id));
        assert!(!combined.attribute_cardinalities.contains_key(&missing));
        assert!(!forward.contains_key(&id));

        // Reuse is strictly the pure bin decode. Even when bytes are identical,
        // both forward/reverse totals and unbinned counters come from THIS read.
        client.execute("UPDATE attr_sketches SET total=77,total_not_binned=5,reverse_total=88 WHERE app_id=$1 AND attr_id=$2", &[&app, &email]).await.unwrap();
        let fresh_rows = query(
            &client,
            &sketch_snapshot_query(app, &[], &forwards, &reverses),
        )
        .await
        .unwrap();
        let mut fresh_options = MatchCteOptions::default();
        let fresh_forward =
            decode_forward_sketch_rows(&fresh_rows, &forwards, &mut fresh_options).unwrap();
        let fresh_reverse =
            decode_reverse_sketch_rows(&fresh_rows, &reverses, &mut fresh_options).unwrap();
        assert!(Arc::ptr_eq(
            &forward[&email].bins,
            &fresh_forward[&email].bins
        ));
        assert!(Arc::ptr_eq(
            &reverse[&email].bins,
            &fresh_reverse[&email].bins
        ));
        assert_eq!(fresh_forward[&email].total, 77);
        assert_eq!(fresh_forward[&email].total_not_binned, 5);
        assert_eq!(fresh_reverse[&email].total, 88);
        assert_eq!(forward[&email].total, 30);

        let other_rows = query(&client, &sketch_snapshot_query(other, &[], &forwards, &[]))
            .await
            .unwrap();
        let other_forward =
            decode_forward_sketch_rows(&other_rows, &forwards, &mut MatchCteOptions::default())
                .unwrap();
        assert_eq!(other_forward[&email].total, 999);
        assert_eq!(other_forward[&email].estimate(AttrSketchInput::Nil), 7);
        assert_eq!(fresh_forward[&email].estimate(AttrSketchInput::Nil), 9);

        client.execute("UPDATE attr_sketches SET width=1,depth=4,reverse_bins=$3 WHERE app_id=$1 AND attr_id=$2", &[&app, &email, &bins]).await.unwrap();
        let reshaped_rows = query(
            &client,
            &sketch_snapshot_query(app, &[], &forwards, &reverses),
        )
        .await
        .unwrap();
        let reshaped =
            decode_forward_sketch_rows(&reshaped_rows, &forwards, &mut MatchCteOptions::default())
                .unwrap();
        let changed_reverse =
            decode_reverse_sketch_rows(&reshaped_rows, &reverses, &mut MatchCteOptions::default())
                .unwrap();
        assert_eq!((reshaped[&email].width, reshaped[&email].depth), (1, 4));
        assert!(!Arc::ptr_eq(
            &fresh_forward[&email].bins,
            &reshaped[&email].bins
        ));
        assert_eq!(changed_reverse[&email].estimate(AttrSketchInput::Nil), 7);
        assert_eq!(fresh_reverse[&email].estimate(AttrSketchInput::Nil), 9);

        client
            .execute(
                "UPDATE attr_sketches SET bins=$3 WHERE app_id=$1 AND attr_id=$2",
                &[&app, &email, &vec![0xff_u8]],
            )
            .await
            .unwrap();
        let corrupt_rows = query(&client, &sketch_snapshot_query(app, &[], &forwards, &[]))
            .await
            .unwrap();
        assert!(
            decode_forward_sketch_rows(&corrupt_rows, &forwards, &mut MatchCteOptions::default())
                .is_err()
        );
        client
            .execute("DELETE FROM attr_sketches WHERE app_id=$1", &[&app])
            .await
            .unwrap();
        planner_sketch_cache::clear();
        let removed =
            read_test_sketches(&client, &compiled, app, &MatchCteOptions::default()).await;
        assert!(removed.attribute_cardinalities.is_empty());
        assert!(removed.attribute_value_cardinalities.is_empty());
        drop(client);
        driver.await.unwrap().unwrap();
        drop(observer);
        observer_driver.await.unwrap().unwrap();
    }

    #[test]
    fn arbitrary_public_timeout_cannot_overflow_deadline() {
        assert!(matches!(
            query_deadline(Duration::MAX),
            Err(PgQueryError::QueryTimeout {
                milliseconds: u64::MAX
            })
        ));
    }

    #[test]
    fn reverse_entity_estimates_hash_canonical_uuid_text() {
        let entity_id = Uuid::new_v4();
        let sketch: AttrSketch = AttrSketch {
            width: 5,
            depth: 2,
            bins: (0..10).collect(),
            total: 10,
            total_not_binned: 0,
        };
        let uppercase = json!(entity_id.to_string().to_uppercase());

        assert_eq!(
            estimate_entity_uuid(&sketch, &uppercase),
            Some(sketch.estimate(AttrSketchInput::String(&entity_id.to_string())))
        );
    }

    #[test]
    fn entity_lookup_estimates_use_the_lookup_sketch_and_unique_cap() {
        let lookup_attribute_id = Uuid::new_v4();
        let sketch = AttrSketch {
            width: 3,
            depth: 2,
            bins: vec![8; 6],
            total: 8,
            total_not_binned: 0,
        };
        let sketches = HashMap::from([(lookup_attribute_id, sketch)]);

        assert_eq!(
            estimate_entity_lookup(
                &sketches,
                &json!([lookup_attribute_id, "account@example.com"])
            ),
            Some(1)
        );
        assert_eq!(
            estimate_entity_lookup(&sketches, &json!([lookup_attribute_id])),
            None
        );
    }

    #[test]
    fn is_null_estimates_include_absent_and_explicit_null_values() {
        assert_eq!(
            is_null_cardinalities(100, 30, 3, false),
            IsNullCardinalities {
                null: 73,
                not_null: 27,
            }
        );
    }

    #[test]
    fn indexed_scalar_is_null_estimates_only_the_indexed_population() {
        assert_eq!(
            is_null_cardinalities(100, 30, 3, true),
            IsNullCardinalities {
                null: 3,
                not_null: 27,
            }
        );
    }

    #[test]
    fn explain_analysis_collects_inherited_aliases_and_loop_weighted_time() {
        let analysis = decode_explain_analysis(&json!([{
            "Planning Time": 1.25,
            "Execution Time": 2.75,
            "Plan": {
                "Node Type": "Nested Loop",
                "Plans": [{
                    "Node Type": "Bitmap Heap Scan",
                    "Alias": "t10",
                    "Plans": [{
                        "Node Type": "Bitmap Index Scan",
                        "Index Name": "triples_ave_idx",
                        "Actual Rows": 3.0,
                        "Actual Loops": 2.0,
                        "Actual Total Time": 0.5
                    }]
                }]
            }
        }]))
        .unwrap();
        assert!((analysis.time_ms - 4.0).abs() < f64::EPSILON);
        assert_eq!(analysis.indexes["t10"][0].index, "triples_ave_idx");
        assert!((analysis.indexes["t10"][0].time_ms.unwrap() - 1.0).abs() < f64::EPSILON);
    }
}
