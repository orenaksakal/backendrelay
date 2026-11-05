use std::collections::BTreeMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::Body;
use axum::http::StatusCode;
use futures_util::{StreamExt, stream};
use instant_http::{
    AdminActor, AdminRequestContext, AdminStorageBackend, ApiError, ApiErrorType,
    RuntimeStorageBackend, RuntimeStorageContext,
};
use instant_instaql::{
    AsyncFileUrlBatchFuture, AsyncFileUrlSigner, AttributeCatalog, system_attr_id,
};
use instant_permissions::{
    EvaluationBindings, EvaluationItem, InMemoryRateLimiter, PermissionAction,
    PermissionDataLoader, PermissionError, PostgresPermissionDataLoader, RateLimitConsumer,
    RateLimitDecision, RateLimitRequest, RefCache, RuleSet, evaluate_batch_async,
};
use instant_transact::{
    DeleteEntityStep, EntityLookup, LookupRef, PreparedTransaction, TransactionOptions,
    TransactionStep, TripleOptions, TripleStep, execute_prepared_transaction_in,
    prepare_transaction_lookups,
};
use serde_json::{Value, json};
use tokio_postgres::Client;
use tokio_postgres::types::Json;
use uuid::Uuid;

use crate::PostgresPool;
use crate::admin_data::{
    AdminDataAuthorizer, AdminDataScope, impersonated_auth, internal_error, load_rules,
    typed_permission_error,
};
use crate::config::DEFAULT_STORAGE_MAX_UPLOAD_BYTES;
use crate::postgres_pool::PostgresClientSource;
use crate::storage_content::{stored_content_disposition, stored_content_type};
use crate::{
    AppStatus, AppStatusFuture, AppStatusSnapshot, DashboardControlHookOperation,
    DashboardControlHooks, EffectiveAppStatusPolicy,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoragePutRequest {
    pub key: String,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
    pub content_encoding: Option<String>,
    pub content_length: Option<u64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageObjectMetadata {
    pub size: u64,
    pub content_type: Option<String>,
    pub content_disposition: Option<String>,
    pub etag: Option<String>,
    pub last_modified_ms: Option<i64>,
}

struct UploadInput {
    app_id: Uuid,
    path: String,
    content_type: Option<String>,
    content_disposition: Option<String>,
    content_length: Option<u64>,
    body: Body,
}

/// Object-store boundary used by the live storage coordinator.
pub trait StorageObjectStore: Send + Sync + 'static {
    fn put(
        &self,
        request: StoragePutRequest,
        body: Body,
    ) -> impl Future<Output = Result<StorageObjectMetadata, String>> + Send;

    fn delete(&self, key: String) -> impl Future<Output = Result<(), String>> + Send;

    fn delete_many(&self, keys: Vec<String>) -> impl Future<Output = Result<(), String>> + Send {
        async move {
            for key in keys {
                self.delete(key).await?;
            }
            Ok(())
        }
    }

    fn signed_download_url(
        &self,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send;

    fn signed_download_url_for_app(
        &self,
        app_id: Uuid,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        let _ = app_id;
        self.signed_download_url(key)
    }

    fn signed_download_urls_for_app(
        &self,
        app_id: Uuid,
        keys: Vec<String>,
    ) -> impl Future<Output = Result<Vec<String>, String>> + Send {
        async move {
            let mut urls = Vec::with_capacity(keys.len());
            for key in keys {
                urls.push(self.signed_download_url_for_app(app_id, key).await?);
            }
            Ok(urls)
        }
    }
}

/// Adapts the production object store to batched `InstaQL` file URL shaping.
pub struct StorageFileUrlSigner<S> {
    objects: Arc<S>,
}

impl<S> StorageFileUrlSigner<S> {
    #[must_use]
    pub fn new(objects: Arc<S>) -> Self {
        Self { objects }
    }
}

impl<S> AsyncFileUrlSigner for StorageFileUrlSigner<S>
where
    S: StorageObjectStore,
{
    fn signed_download_urls(
        &self,
        app_id: Uuid,
        location_ids: Vec<String>,
    ) -> AsyncFileUrlBatchFuture<'_> {
        Box::pin(async move {
            let keys = location_ids
                .into_iter()
                .map(|location_id| object_key(app_id, &location_id))
                .collect();
            self.objects
                .signed_download_urls_for_app(app_id, keys)
                .await
        })
    }
}

pub trait StorageAccessPolicy: Send + Sync + 'static {
    fn authorize(&self, app_id: Uuid) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn detect_content_type(
        &self,
        app_id: Uuid,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send;

    /// Soft per-app storage quota in bytes. `None` means unlimited; existing
    /// objects are never deleted when a quota is introduced.
    fn storage_quota_bytes(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<Option<u64>, ApiError>> + Send {
        std::future::ready(Ok(None))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllStorage;

impl StorageAccessPolicy for AllowAllStorage {
    fn authorize(&self, _app_id: Uuid) -> impl Future<Output = Result<(), ApiError>> + Send {
        std::future::ready(Ok(()))
    }

    fn detect_content_type(
        &self,
        _app_id: Uuid,
    ) -> impl Future<Output = Result<bool, ApiError>> + Send {
        std::future::ready(Ok(false))
    }
}

/// PostgreSQL and object-store implementation of all six admin storage routes.
pub struct AdminStorageService<A, S, P = AllowAllStorage> {
    client: PostgresClientSource,
    authorizer: Arc<A>,
    objects: Arc<S>,
    server_origin: String,
    max_upload_bytes: u64,
    rate_limit_consumer: Arc<dyn RateLimitConsumer>,
    policy: Arc<P>,
    app_status_policy: Arc<dyn EffectiveAppStatusPolicy>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ActiveAppStatusPolicy;

impl EffectiveAppStatusPolicy for ActiveAppStatusPolicy {
    fn status(&self, _app_id: Uuid) -> AppStatusFuture<'_> {
        Box::pin(std::future::ready(Ok(AppStatusSnapshot {
            own_status: AppStatus::Active,
            effective_status: AppStatus::Active,
        })))
    }
}

/// Records CEL rate-limit calls while permission references are read. The
/// durable consumer is replayed only after the database lease is released,
/// avoiding a nested checkout when the primary pool has capacity one.
#[derive(Debug, Default)]
struct DeferredRateLimitConsumer {
    requests: StdMutex<Vec<RateLimitRequest>>,
}

impl DeferredRateLimitConsumer {
    fn take(&self) -> Vec<RateLimitRequest> {
        std::mem::take(
            &mut *self
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl RateLimitConsumer for DeferredRateLimitConsumer {
    fn consume(&self, request: &RateLimitRequest) -> Result<RateLimitDecision, String> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        Ok(RateLimitDecision::Allowed {
            remaining_tokens: u64::MAX,
        })
    }
}

/// Connects dashboard storage routes to the same native storage coordinator
/// used by the admin and runtime APIs. Dashboard authorization is performed by
/// `DashboardControlService` before these operations reach this adapter.
pub struct DashboardStorageControlHooks<D, A, S, P = AllowAllStorage> {
    inner: D,
    storage: Arc<AdminStorageService<A, S, P>>,
}

impl<D, A, S, P> DashboardStorageControlHooks<D, A, S, P> {
    #[must_use]
    pub const fn new(inner: D, storage: Arc<AdminStorageService<A, S, P>>) -> Self {
        Self { inner, storage }
    }
}

impl<D, A, S, P> DashboardControlHooks for DashboardStorageControlHooks<D, A, S, P>
where
    D: DashboardControlHooks,
    A: AdminDataAuthorizer,
    S: StorageObjectStore,
    P: StorageAccessPolicy,
{
    async fn execute(&self, operation: DashboardControlHookOperation) -> Result<Value, ApiError> {
        match operation {
            DashboardControlHookOperation::UploadDashboardFile {
                app_id,
                path,
                content_type,
                content_length,
                body,
            } => {
                let result = async {
                    let catalog = self.storage.authorizer.load_catalog(app_id).await?;
                    self.storage
                        .upload_file(
                            &catalog,
                            UploadInput {
                                app_id,
                                path,
                                content_type,
                                content_disposition: None,
                                content_length,
                                body: Body::from(body),
                            },
                        )
                        .await
                }
                .await;
                record_storage_op(crate::metrics::StorageOp::Upload, result)
            }
            DashboardControlHookOperation::DeleteDashboardFiles { app_id, paths } => {
                let result = async {
                    self.storage.policy.authorize(app_id).await?;
                    let catalog = self.storage.authorizer.load_catalog(app_id).await?;
                    let mut client = self
                        .storage
                        .client
                        .acquire()
                        .await
                        .map_err(internal_error)?;
                    let ids = delete_file_records(&mut client, app_id, &catalog, &paths).await?;
                    Ok(json!({"ids": ids}))
                }
                .await;
                record_storage_op(crate::metrics::StorageOp::Delete, result)
            }
            other => self.inner.execute(other).await,
        }
    }
}

impl<A, S> AdminStorageService<A, S, AllowAllStorage>
where
    A: AdminDataAuthorizer,
    S: StorageObjectStore,
{
    #[must_use]
    pub fn new(
        client: Client,
        authorizer: Arc<A>,
        objects: Arc<S>,
        server_origin: impl Into<String>,
    ) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            authorizer,
            objects,
            server_origin: server_origin.into().trim_end_matches('/').to_owned(),
            max_upload_bytes: DEFAULT_STORAGE_MAX_UPLOAD_BYTES,
            rate_limit_consumer: Arc::new(InMemoryRateLimiter::default()),
            policy: Arc::new(AllowAllStorage),
            app_status_policy: Arc::new(ActiveAppStatusPolicy),
        }
    }

    #[must_use]
    pub fn from_pool(
        pool: PostgresPool,
        authorizer: Arc<A>,
        objects: Arc<S>,
        server_origin: impl Into<String>,
    ) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            authorizer,
            objects,
            server_origin: server_origin.into().trim_end_matches('/').to_owned(),
            max_upload_bytes: DEFAULT_STORAGE_MAX_UPLOAD_BYTES,
            rate_limit_consumer: Arc::new(InMemoryRateLimiter::default()),
            policy: Arc::new(AllowAllStorage),
            app_status_policy: Arc::new(ActiveAppStatusPolicy),
        }
    }
}

/// Per-app counters for the storage surface. A successful catalog read also
/// registers the app as resolved; failures only count for registered apps.
fn record_storage_result(app_id: Uuid, failed: bool) {
    if failed {
        crate::metrics::record_app_error(app_id, crate::metrics::AppErrorSurface::Storage);
    } else {
        crate::metrics::register_app(app_id);
    }
}

/// Client errors mean the request was refused before it could become a storage
/// failure; everything else is counted as an error so the ratio alert sees it.
fn storage_op_outcome<T>(result: &Result<T, ApiError>) -> crate::metrics::StorageOpOutcome {
    use crate::metrics::StorageOpOutcome;
    match result {
        Ok(_) => StorageOpOutcome::Ok,
        Err(error) if error.status().is_client_error() => StorageOpOutcome::Rejected,
        Err(_) => StorageOpOutcome::Error,
    }
}

/// Counts one storage API operation using only the fixed op/outcome vocabulary.
fn record_storage_op<T>(
    op: crate::metrics::StorageOp,
    result: Result<T, ApiError>,
) -> Result<T, ApiError> {
    crate::metrics::record_storage_op(op, storage_op_outcome(&result));
    result
}

impl<A, S, P> AdminStorageService<A, S, P>
where
    A: AdminDataAuthorizer,
    S: StorageObjectStore,
    P: StorageAccessPolicy,
{
    #[must_use]
    pub fn with_storage_policy<P2>(self, policy: Arc<P2>) -> AdminStorageService<A, S, P2>
    where
        P2: StorageAccessPolicy,
    {
        AdminStorageService {
            client: self.client,
            authorizer: self.authorizer,
            objects: self.objects,
            server_origin: self.server_origin,
            max_upload_bytes: self.max_upload_bytes,
            rate_limit_consumer: self.rate_limit_consumer,
            policy,
            app_status_policy: self.app_status_policy,
        }
    }

    /// Caps a single stored object. Oversized uploads are rejected with 413
    /// before the body is forwarded to the object store.
    #[must_use]
    pub const fn with_max_upload_bytes(mut self, max_upload_bytes: u64) -> Self {
        self.max_upload_bytes = max_upload_bytes;
        self
    }

    #[must_use]
    pub fn with_effective_app_status_policy(
        mut self,
        policy: Arc<dyn EffectiveAppStatusPolicy>,
    ) -> Self {
        self.app_status_policy = policy;
        self
    }

    #[must_use]
    pub fn with_rate_limit_consumer(mut self, consumer: Arc<dyn RateLimitConsumer>) -> Self {
        self.rate_limit_consumer = consumer;
        self
    }

    async fn admin_catalog(
        &self,
        context: AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        let app_id = context.app_id;
        let result = async {
            self.policy.authorize(app_id).await?;
            if context.bearer_token.is_none() {
                return Err(ApiError::new(
                    ApiErrorType::ParamMissing,
                    "Missing parameter: [headers authorization]",
                )
                .unauthorized());
            }
            self.authorizer.authorize(context, scope).await
        }
        .await;
        record_storage_result(app_id, result.is_err());
        result
    }

    async fn storage_context(
        &self,
        context: &AdminRequestContext,
        scope: AdminDataScope,
        action: PermissionAction,
        path: &str,
    ) -> Result<AttributeCatalog, ApiError> {
        let catalog = self.authorized_storage_catalog(context, scope).await?;
        let app_id = context.app_id;
        let result = self
            .authorize_storage_action(context, &catalog, action, path)
            .await;
        record_storage_result(app_id, result.is_err());
        result.map(|()| catalog)
    }

    async fn authorized_storage_catalog(
        &self,
        context: &AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        let app_id = context.app_id;
        let result = async {
            self.policy.authorize(app_id).await?;
            match &context.actor {
                AdminActor::Admin | AdminActor::Email(_) => {
                    self.authorizer.authorize(context.clone(), scope).await
                }
                AdminActor::Guest | AdminActor::RefreshToken(_) => {
                    self.authorizer.load_catalog(app_id).await
                }
            }
        }
        .await;
        record_storage_result(app_id, result.is_err());
        result
    }

    async fn authorize_storage_action(
        &self,
        context: &AdminRequestContext,
        catalog: &AttributeCatalog,
        action: PermissionAction,
        path: &str,
    ) -> Result<(), ApiError> {
        if context.actor == AdminActor::Admin {
            return Ok(());
        }
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let auth = impersonated_auth(&client, context.app_id, catalog, &context.actor).await?;
        let rules = load_rules(&client, context.app_id, None).await?;
        let (allowed, rate_limit_requests) = self
            .evaluate_permission(
                &mut client,
                context.app_id,
                catalog,
                &rules,
                auth,
                action,
                path,
                context.client_ip,
            )
            .await?;
        drop(client);
        consume_rate_limits(&rate_limit_requests, self.rate_limit_consumer.as_ref())
            .map_err(typed_permission_error)?;
        if !allowed {
            return Err(storage_permission_denied(action));
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn evaluate_permission(
        &self,
        client: &mut Client,
        app_id: Uuid,
        catalog: &AttributeCatalog,
        rules: &RuleSet,
        auth: Value,
        action: PermissionAction,
        path: &str,
        client_ip: Option<IpAddr>,
    ) -> Result<(bool, Vec<RateLimitRequest>), ApiError> {
        let Some(program) = rules
            .program("$files", action)
            .map_err(typed_permission_error)?
        else {
            return Err(storage_permission_denied(action));
        };
        let mut bindings = EvaluationBindings {
            data: json!({"path": path}),
            data_etype: Some("$files".to_owned()),
            auth,
            ..EvaluationBindings::default()
        };
        bindings.request.ip = client_ip.map(|ip| ip.to_string());
        let deferred_rate_limits = Arc::new(DeferredRateLimitConsumer::default());
        rules
            .configure_rate_limits(
                app_id,
                &mut bindings,
                Arc::clone(&deferred_rate_limits) as Arc<dyn RateLimitConsumer>,
            )
            .map_err(typed_permission_error)?;
        let transaction = client.transaction().await.map_err(internal_error)?;
        let loader = PostgresPermissionDataLoader { app_id, catalog };
        let allowed = evaluate_batch_async(
            &[EvaluationItem {
                program: &program,
                bindings: &bindings,
            }],
            &mut RefCache::new(),
            |requests| loader.load_refs(&transaction, requests),
        )
        .await
        .map_err(typed_permission_error)?
        .into_iter()
        .next()
        .unwrap_or(false);
        transaction.rollback().await.map_err(internal_error)?;
        Ok((allowed, deferred_rate_limits.take()))
    }

    async fn upload_file(
        &self,
        catalog: &AttributeCatalog,
        input: UploadInput,
    ) -> Result<Value, ApiError> {
        let UploadInput {
            app_id,
            path,
            content_type,
            content_disposition: requested_disposition,
            content_length,
            body,
        } = input;
        self.policy.authorize(app_id).await?;
        reject_declared_oversized_upload(content_length, self.max_upload_bytes)?;
        let (content_type, body) =
            if content_type.is_none() && self.policy.detect_content_type(app_id).await? {
                let (detected, replayable_body) = detect_content_type(body).await?;
                (Some(detected), replayable_body)
            } else {
                (content_type, body)
            };
        let content_type = stored_content_type(content_type.as_deref());
        // Decided once at upload so signing never reads object metadata. The
        // client can only tighten it (ask for `attachment`), never loosen it.
        let content_disposition = Some(stored_content_disposition(
            &content_type,
            requested_disposition.as_deref(),
        ));
        self.reject_uploads_over_quota(app_id, content_length)
            .await?;
        let location_id = Uuid::new_v4().to_string();
        let key = object_key(app_id, &location_id);
        let (body, upload_limit) = limit_upload_body(body, self.max_upload_bytes);
        let stored = self
            .objects
            .put(
                StoragePutRequest {
                    key: key.clone(),
                    content_type: Some(content_type),
                    content_disposition,
                    content_encoding: None,
                    content_length,
                    tags: BTreeMap::new(),
                },
                body,
            )
            .await;
        if upload_limit.overflowed.load(Ordering::Relaxed) {
            let _ = self.objects.delete(key).await;
            return Err(upload_too_large(self.max_upload_bytes, content_length));
        }
        let metadata = stored.map_err(object_store_error)?;
        if metadata.size > self.max_upload_bytes {
            let _ = self.objects.delete(key).await;
            return Err(upload_too_large(self.max_upload_bytes, content_length));
        }
        if content_length.is_some_and(|expected| expected != metadata.size) {
            let _ = self.objects.delete(key).await;
            return Err(ApiError::new(
                ApiErrorType::ValidationFailed,
                "Uploaded object size did not match content-length",
            )
            .with_hint(json!({"expected": content_length, "actual": metadata.size})));
        }
        let mut client = self.client.acquire().await.map_err(internal_error)?;
        let result =
            create_file_record(&mut client, app_id, catalog, &path, &location_id, &metadata).await;
        match result {
            Ok(id) => Ok(json!({
                "id": id,
                "location-id": location_id,
                "size": metadata.size,
            })),
            Err(error) => {
                let _ = self.objects.delete(key).await;
                Err(error)
            }
        }
    }

    /// Refuses an upload whose declared size would exceed the app's soft quota.
    /// Usage is the same `$files.size` aggregate the dashboard reports.
    async fn reject_uploads_over_quota(
        &self,
        app_id: Uuid,
        declared_bytes: Option<u64>,
    ) -> Result<(), ApiError> {
        let Some(quota) = self.policy.storage_quota_bytes(app_id).await? else {
            return Ok(());
        };
        let Some(declared_bytes) = declared_bytes else {
            return Ok(());
        };
        let client = self.client.acquire().await.map_err(internal_error)?;
        let used = client
            .query_one(
                "SELECT coalesce(sum(files_size), 0)::bigint
                   FROM triples_size_aggregate
                  WHERE app_id = $1",
                &[&app_id],
            )
            .await
            .map_err(internal_error)?
            .get::<_, i64>(0);
        let used = u64::try_from(used).unwrap_or_default();
        if used.saturating_add(declared_bytes) > quota {
            return Err(ApiError::new(
                ApiErrorType::PermissionDenied,
                format!("App storage quota of {quota} bytes exceeded"),
            )
            .with_status(StatusCode::FORBIDDEN)
            .with_hint(json!({
                "quota-bytes": quota,
                "used-bytes": used,
                "requested-bytes": declared_bytes,
            })));
        }
        Ok(())
    }
}

fn reject_declared_oversized_upload(
    content_length: Option<u64>,
    max_upload_bytes: u64,
) -> Result<(), ApiError> {
    if content_length.is_some_and(|declared| declared > max_upload_bytes) {
        return Err(upload_too_large(max_upload_bytes, content_length));
    }
    Ok(())
}

fn upload_too_large(max_upload_bytes: u64, content_length: Option<u64>) -> ApiError {
    ApiError::new(
        ApiErrorType::ParameterLimitExceeded,
        format!("Upload exceeds the {max_upload_bytes}-byte storage limit"),
    )
    .with_status(StatusCode::PAYLOAD_TOO_LARGE)
    .with_hint(json!({
        "max-upload-bytes": max_upload_bytes,
        "content-length": content_length,
    }))
}

#[derive(Default)]
struct UploadLimit {
    total: AtomicU64,
    overflowed: AtomicBool,
}

/// Wraps an upload body so it fails as soon as the counted bytes pass the cap.
/// The caller inspects `overflowed` after `put` returns, because object-store
/// errors only carry the aborted stream as an opaque message.
fn limit_upload_body(body: Body, max_upload_bytes: u64) -> (Body, Arc<UploadLimit>) {
    let limit = Arc::new(UploadLimit::default());
    let state = Arc::clone(&limit);
    let stream = body.into_data_stream().map(move |chunk| {
        let chunk = chunk.map_err(|error| std::io::Error::other(error.to_string()))?;
        let length = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        let total = state
            .total
            .fetch_add(length, Ordering::Relaxed)
            .saturating_add(length);
        if total > max_upload_bytes {
            state.overflowed.store(true, Ordering::Relaxed);
            return Err(std::io::Error::other(format!(
                "upload exceeds the {max_upload_bytes}-byte storage limit"
            )));
        }
        Ok(chunk)
    });
    (Body::from_stream(stream), limit)
}

const CONTENT_TYPE_SNIFF_BYTES: usize = 8192;

async fn detect_content_type(body: Body) -> Result<(String, Body), ApiError> {
    let mut body_stream = body.into_data_stream();
    let mut prefix = Vec::with_capacity(CONTENT_TYPE_SNIFF_BYTES);
    let mut buffered = Vec::new();
    while prefix.len() < CONTENT_TYPE_SNIFF_BYTES {
        let Some(chunk) = body_stream.next().await else {
            break;
        };
        let chunk = chunk.map_err(internal_error)?;
        let remaining = CONTENT_TYPE_SNIFF_BYTES - prefix.len();
        prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        buffered.push(Ok::<_, axum::Error>(chunk));
    }
    let detected = infer::get(&prefix).map_or("application/octet-stream", |kind| kind.mime_type());
    let replay = stream::iter(buffered).chain(body_stream);
    Ok((detected.to_owned(), Body::from_stream(replay)))
}

impl<A, S, P> AdminStorageBackend for AdminStorageService<A, S, P>
where
    A: AdminDataAuthorizer,
    S: StorageObjectStore,
    P: StorageAccessPolicy,
{
    async fn upload(
        &self,
        context: AdminRequestContext,
        path: String,
        content_type: Option<String>,
        content_disposition: Option<String>,
        content_length: Option<u64>,
        body: Body,
    ) -> Result<Value, ApiError> {
        let result = async {
            let app_id = context.app_id;
            let catalog = self
                .storage_context(
                    &context,
                    AdminDataScope::StorageWrite,
                    PermissionAction::Create,
                    &path,
                )
                .await?;
            self.upload_file(
                &catalog,
                UploadInput {
                    app_id,
                    path,
                    content_type,
                    content_disposition,
                    content_length,
                    body,
                },
            )
            .await
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::Upload, result)
    }

    async fn delete_file(
        &self,
        context: AdminRequestContext,
        path: String,
    ) -> Result<Value, ApiError> {
        let result = async {
            let app_id = context.app_id;
            let catalog = self
                .storage_context(
                    &context,
                    AdminDataScope::StorageWrite,
                    PermissionAction::Delete,
                    &path,
                )
                .await?;
            let mut client = self.client.acquire().await.map_err(internal_error)?;
            let ids = delete_file_records(&mut client, app_id, &catalog, &[path]).await?;
            Ok(json!({"id": ids.into_iter().next()}))
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::Delete, result)
    }

    async fn delete_files(
        &self,
        context: AdminRequestContext,
        paths: Vec<String>,
    ) -> Result<Value, ApiError> {
        let result = async {
            let app_id = context.app_id;
            let catalog = self
                .authorized_storage_catalog(&context, AdminDataScope::StorageWrite)
                .await?;
            for path in &paths {
                self.authorize_storage_action(&context, &catalog, PermissionAction::Delete, path)
                    .await?;
            }
            let mut client = self.client.acquire().await.map_err(internal_error)?;
            let ids = delete_file_records(&mut client, app_id, &catalog, &paths).await?;
            Ok(json!({"ids": ids}))
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::Delete, result)
    }

    async fn list_files(&self, context: AdminRequestContext) -> Result<Value, ApiError> {
        let result = async {
            let app_id = context.app_id;
            // The pinned legacy handler first requires storage-write, then delegates
            // to the ordinary admin query path, which independently requires
            // data-read before loading `$files`.
            self.admin_catalog(context.clone(), AdminDataScope::StorageWrite)
                .await?;
            let catalog = self.admin_catalog(context, AdminDataScope::Read).await?;
            assert_effective_read_allowed(self.app_status_policy.as_ref(), app_id).await?;
            let client = self.client.acquire().await.map_err(internal_error)?;
            list_file_records(&client, app_id, &catalog).await
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::List, result)
    }

    async fn create_upload_url(
        &self,
        context: AdminRequestContext,
        path: String,
    ) -> Result<Value, ApiError> {
        let app_id = context.app_id;
        self.admin_catalog(context, AdminDataScope::StorageWrite)
            .await?;
        let upload_id = Uuid::new_v4();
        let client = self.client.acquire().await.map_err(internal_error)?;
        client
            .execute(
                "INSERT INTO app_upload_urls (id, app_id, path) VALUES ($1, $2, $3)",
                &[&upload_id, &app_id, &path],
            )
            .await
            .map_err(internal_error)?;
        Ok(Value::String(format!(
            "{}/storage/{upload_id}/consume-upload-url",
            self.server_origin
        )))
    }

    async fn create_download_url(
        &self,
        context: AdminRequestContext,
        path: String,
    ) -> Result<Value, ApiError> {
        let result = async {
            let app_id = context.app_id;
            let catalog = self
                .admin_catalog(context, AdminDataScope::StorageRead)
                .await?;
            let client = self.client.acquire().await.map_err(internal_error)?;
            let Some(location_id) = file_location(&client, app_id, &catalog, &path).await? else {
                return Ok(Value::Null);
            };
            drop(client);
            self.objects
                .signed_download_url_for_app(app_id, object_key(app_id, &location_id))
                .await
                .map(Value::String)
                .map_err(object_store_error)
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::DownloadUrl, result)
    }
}

async fn assert_effective_read_allowed(
    policy: &dyn EffectiveAppStatusPolicy,
    app_id: Uuid,
) -> Result<(), ApiError> {
    policy
        .status(app_id)
        .await?
        .effective_status
        .assert_read_allowed()
}

impl<A, S, P> RuntimeStorageBackend for AdminStorageService<A, S, P>
where
    A: AdminDataAuthorizer,
    S: StorageObjectStore,
    P: StorageAccessPolicy,
{
    async fn upload(
        &self,
        context: RuntimeStorageContext,
        path: String,
        content_type: Option<String>,
        content_disposition: Option<String>,
        content_length: Option<u64>,
        body: Body,
    ) -> Result<Value, ApiError> {
        let result = async {
            let context = runtime_admin_context(context);
            let catalog = self
                .storage_context(
                    &context,
                    AdminDataScope::Write,
                    PermissionAction::Create,
                    &path,
                )
                .await?;
            self.upload_file(
                &catalog,
                UploadInput {
                    app_id: context.app_id,
                    path,
                    content_type,
                    content_disposition,
                    content_length,
                    body,
                },
            )
            .await
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::Upload, result)
    }

    async fn delete_file(
        &self,
        context: RuntimeStorageContext,
        path: String,
    ) -> Result<Value, ApiError> {
        let result = async {
            let context = runtime_admin_context(context);
            let catalog = self
                .storage_context(
                    &context,
                    AdminDataScope::Write,
                    PermissionAction::Delete,
                    &path,
                )
                .await?;
            let mut client = self.client.acquire().await.map_err(internal_error)?;
            let ids = delete_file_records(&mut client, context.app_id, &catalog, &[path]).await?;
            Ok(json!({"id": ids.into_iter().next()}))
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::Delete, result)
    }

    async fn create_upload_url(
        &self,
        context: RuntimeStorageContext,
        path: String,
    ) -> Result<Value, ApiError> {
        let context = runtime_admin_context(context);
        self.storage_context(
            &context,
            AdminDataScope::Write,
            PermissionAction::Create,
            &path,
        )
        .await?;
        let upload_id = Uuid::new_v4();
        let client = self.client.acquire().await.map_err(internal_error)?;
        client
            .execute(
                "INSERT INTO app_upload_urls (id, app_id, path) VALUES ($1, $2, $3)",
                &[&upload_id, &context.app_id, &path],
            )
            .await
            .map_err(internal_error)?;
        Ok(Value::String(format!(
            "{}/storage/{upload_id}/consume-upload-url",
            self.server_origin
        )))
    }

    async fn consume_upload_url(
        &self,
        upload_id: Uuid,
        content_type: String,
        content_length: Option<u64>,
        body: Body,
    ) -> Result<Value, ApiError> {
        let result = async {
            let record = {
                let client = self.client.acquire().await.map_err(internal_error)?;
                client
                    .query_opt(
                        "DELETE FROM app_upload_urls WHERE id = $1
                         RETURNING app_id, path, expired_at",
                        &[&upload_id],
                    )
                    .await
                    .map_err(internal_error)?
            };
            let Some(record) = record else {
                return Err(invalid_upload_url(upload_id));
            };
            let expired_at = record.get::<_, chrono::NaiveDateTime>("expired_at");
            if expired_at < chrono::Utc::now().naive_utc() {
                return Err(invalid_upload_url(upload_id));
            }
            let app_id = record.get("app_id");
            let path = record.get("path");
            let catalog = self.authorizer.load_catalog(app_id).await?;
            self.upload_file(
                &catalog,
                UploadInput {
                    app_id,
                    path,
                    content_type: Some(content_type),
                    content_disposition: None,
                    content_length,
                    body,
                },
            )
            .await
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::Upload, result)
    }

    async fn create_download_url(
        &self,
        context: RuntimeStorageContext,
        path: String,
    ) -> Result<Value, ApiError> {
        let result = async {
            let context = runtime_admin_context(context);
            let catalog = self
                .storage_context(
                    &context,
                    AdminDataScope::Read,
                    PermissionAction::View,
                    &path,
                )
                .await?;
            let client = self.client.acquire().await.map_err(internal_error)?;
            let Some(location_id) = file_location(&client, context.app_id, &catalog, &path).await?
            else {
                return Ok(Value::Null);
            };
            drop(client);
            self.objects
                .signed_download_url_for_app(
                    context.app_id,
                    object_key(context.app_id, &location_id),
                )
                .await
                .map(Value::String)
                .map_err(object_store_error)
        }
        .await;
        record_storage_op(crate::metrics::StorageOp::DownloadUrl, result)
    }
}

fn runtime_admin_context(context: RuntimeStorageContext) -> AdminRequestContext {
    let actor = context
        .refresh_token
        .and_then(|token| Uuid::parse_str(&token).ok())
        .map_or(AdminActor::Guest, AdminActor::RefreshToken);
    AdminRequestContext {
        app_id: context.app_id,
        bearer_token: None,
        actor,
        client_ip: context.client_ip,
    }
}

fn invalid_upload_url(upload_id: Uuid) -> ApiError {
    ApiError::new(
        ApiErrorType::ValidationFailed,
        "The upload URL is expired or invalid.",
    )
    .with_hint(json!({"in": "app-upload-url", "value": upload_id}))
}

async fn create_file_record(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    path: &str,
    location_id: &str,
    metadata: &StorageObjectMetadata,
) -> Result<Uuid, ApiError> {
    let path_attr = required_file_attr(catalog, "path")?;
    let entity = EntityLookup::Lookup(LookupRef {
        attr_id: path_attr,
        value: json!(path),
    });
    let mut steps = vec![
        add_file_value(entity.clone(), catalog, "size", json!(metadata.size))?,
        add_file_value(entity.clone(), catalog, "location-id", json!(location_id))?,
        add_file_value(entity.clone(), catalog, "key-version", json!(1))?,
    ];
    if let Some(content_type) = &metadata.content_type {
        steps.push(add_file_value(
            entity.clone(),
            catalog,
            "content-type",
            json!(content_type),
        )?);
    }
    if let Some(content_disposition) = &metadata.content_disposition {
        steps.push(add_file_value(
            entity,
            catalog,
            "content-disposition",
            json!(content_disposition),
        )?);
    }
    let transaction = client.transaction().await.map_err(internal_error)?;
    let mut prepared = prepare_transaction_lookups(&transaction, app_id, catalog, &steps)
        .await
        .map_err(internal_error)?;
    let entity_id = resolved_entity_id(&prepared)?;
    prepared.steps.push(add_file_value(
        EntityLookup::Id(entity_id),
        catalog,
        "id",
        json!(entity_id),
    )?);
    execute_prepared_transaction_in(
        &transaction,
        app_id,
        catalog,
        &prepared,
        storage_transaction_options(),
    )
    .await
    .map_err(internal_error)?;
    transaction.commit().await.map_err(internal_error)?;
    Ok(entity_id)
}

async fn delete_file_records(
    client: &mut Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    paths: &[String],
) -> Result<Vec<Uuid>, ApiError> {
    let path_attr = required_file_attr(catalog, "path")?;
    let steps = paths
        .iter()
        .map(|path| {
            TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: EntityLookup::Lookup(LookupRef {
                    attr_id: path_attr,
                    value: json!(path),
                }),
                etype: Some("$files".to_owned()),
            })
        })
        .collect::<Vec<_>>();
    let transaction = client.transaction().await.map_err(internal_error)?;
    let prepared = prepare_transaction_lookups(&transaction, app_id, catalog, &steps)
        .await
        .map_err(internal_error)?;
    let ids = prepared
        .steps
        .iter()
        .filter_map(|step| match step {
            TransactionStep::DeleteEntity(DeleteEntityStep {
                entity: EntityLookup::Id(id),
                ..
            }) => Some(*id),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !prepared.steps.is_empty() {
        execute_prepared_transaction_in(
            &transaction,
            app_id,
            catalog,
            &prepared,
            storage_transaction_options(),
        )
        .await
        .map_err(internal_error)?;
    }
    transaction.commit().await.map_err(internal_error)?;
    Ok(ids)
}

async fn list_file_records(
    client: &Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
) -> Result<Value, ApiError> {
    let path_attr = required_file_attr(catalog, "path")?;
    let size_attr = required_file_attr(catalog, "size")?;
    let location_attr = required_file_attr(catalog, "location-id")?;
    let rows = client
        .query(
            "SELECT entity_id, attr_id, value
               FROM triples
              WHERE app_id = $1 AND attr_id = ANY($2)
              ORDER BY entity_id, created_at, attr_id",
            &[&app_id, &vec![path_attr, size_attr, location_attr]],
        )
        .await
        .map_err(internal_error)?;
    let mut files = BTreeMap::<Uuid, BTreeMap<Uuid, Value>>::new();
    for row in rows {
        files
            .entry(row.get("entity_id"))
            .or_default()
            .insert(row.get("attr_id"), row.get::<_, Json<Value>>("value").0);
    }
    Ok(Value::Array(
        files
            .into_values()
            .filter_map(|values| {
                Some(json!({
                    "key": object_key(app_id, values.get(&location_attr)?.as_str()?),
                    "name": values.get(&path_attr)?.as_str()?,
                    "size": values.get(&size_attr)?,
                    "etag": null,
                    "last_modified": null,
                }))
            })
            .collect(),
    ))
}

async fn file_location(
    client: &Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    path: &str,
) -> Result<Option<String>, ApiError> {
    let path_attr = required_file_attr(catalog, "path")?;
    let location_attr = required_file_attr(catalog, "location-id")?;
    let row = client
        .query_opt(
            "SELECT location.value
               FROM triples path
               JOIN triples location
                 ON location.app_id = path.app_id
                AND location.entity_id = path.entity_id
                AND location.attr_id = $4
              WHERE path.app_id = $1 AND path.attr_id = $2 AND path.value = $3::jsonb
              LIMIT 1",
            &[&app_id, &path_attr, &json!(path), &location_attr],
        )
        .await
        .map_err(internal_error)?;
    Ok(row.and_then(|row| row.get::<_, Json<Value>>(0).0.as_str().map(str::to_owned)))
}

fn add_file_value(
    entity: EntityLookup,
    catalog: &AttributeCatalog,
    label: &str,
    value: Value,
) -> Result<TransactionStep, ApiError> {
    Ok(TransactionStep::AddTriple(TripleStep {
        entity,
        etype: Some("$files".to_owned()),
        attr_id: required_file_attr(catalog, label)?,
        reverse_etype: None,
        value,
        options: TripleOptions::default(),
    }))
}

fn resolved_entity_id(prepared: &PreparedTransaction) -> Result<Uuid, ApiError> {
    prepared
        .steps
        .iter()
        .find_map(|step| match step {
            TransactionStep::AddTriple(TripleStep {
                entity: EntityLookup::Id(id),
                ..
            }) => Some(*id),
            _ => None,
        })
        .or_else(|| {
            prepared
                .implicit_lookups
                .first()
                .map(|lookup| lookup.entity_id)
        })
        .ok_or_else(|| internal_error("storage lookup preparation produced no entity id"))
}

fn required_file_attr(catalog: &AttributeCatalog, label: &str) -> Result<Uuid, ApiError> {
    catalog
        .forward("$files", label)
        .map(|attribute| attribute.id)
        .or_else(|| system_attr_id("$files", label).ok())
        .ok_or_else(|| internal_error(format!("missing $files.{label} system attribute")))
}

const fn storage_transaction_options() -> TransactionOptions {
    TransactionOptions {
        allow_system_catalog_updates: false,
        admin: false,
        allow_file_metadata_updates: true,
        allow_stream_metadata_updates: false,
        overwrite_created_at: false,
        invalidator_slot_num: 0,
        wal_entity_log: instant_transact::WalEntityLog::Disabled,
    }
}

fn storage_permission_denied(action: PermissionAction) -> ApiError {
    ApiError::new(
        ApiErrorType::PermissionDenied,
        format!("Permission denied for $files.{}", action.as_str()),
    )
}

fn consume_rate_limits(
    requests: &[RateLimitRequest],
    consumer: &dyn RateLimitConsumer,
) -> Result<(), PermissionError> {
    for request in requests {
        match consumer
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

fn object_store_error(error: impl std::fmt::Display) -> ApiError {
    internal_error(error)
}

#[must_use]
pub fn object_key(app_id: Uuid, location_id: &str) -> String {
    format!("{app_id}/{}/{location_id}", object_key_bin(location_id))
}

/// JVM `String.hashCode` plus `Math.abs` binning used by the production bucket.
#[must_use]
pub fn object_key_bin(location_id: &str) -> i32 {
    let hash = location_id.encode_utf16().fold(0_i32, |hash, unit| {
        hash.wrapping_mul(31).wrapping_add(i32::from(unit))
    });
    hash.wrapping_abs() % 10
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::body::{Bytes, to_bytes};
    use instant_permissions::{RateLimit, RateLimitBucketConfig, RefillType};

    use super::*;

    #[derive(Default)]
    struct RecordingBatchObjectStore {
        batches: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl StorageObjectStore for RecordingBatchObjectStore {
        fn put(
            &self,
            _request: StoragePutRequest,
            _body: Body,
        ) -> impl Future<Output = Result<StorageObjectMetadata, String>> + Send {
            std::future::ready(Err("unused test upload".to_owned()))
        }

        fn delete(&self, _key: String) -> impl Future<Output = Result<(), String>> + Send {
            std::future::ready(Ok(()))
        }

        fn signed_download_url(
            &self,
            key: String,
        ) -> impl Future<Output = Result<String, String>> + Send {
            std::future::ready(Ok(format!("https://objects.test/{key}")))
        }

        fn signed_download_urls_for_app(
            &self,
            _app_id: Uuid,
            keys: Vec<String>,
        ) -> impl Future<Output = Result<Vec<String>, String>> + Send {
            self.batches.lock().unwrap().push(keys.clone());
            std::future::ready(Ok(keys
                .into_iter()
                .map(|key| format!("https://objects.test/{key}"))
                .collect()))
        }
    }

    struct RecordingAppStatusPolicy {
        status: AppStatus,
        calls: AtomicUsize,
        app_ids: StdMutex<Vec<Uuid>>,
    }

    impl EffectiveAppStatusPolicy for RecordingAppStatusPolicy {
        fn status(&self, app_id: Uuid) -> AppStatusFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.app_ids.lock().unwrap().push(app_id);
            Box::pin(std::future::ready(Ok(AppStatusSnapshot {
                own_status: self.status,
                effective_status: self.status,
            })))
        }
    }

    #[tokio::test]
    async fn legacy_file_list_read_gate_uses_effective_status() {
        let app_id = Uuid::new_v4();
        for status in [AppStatus::Active, AppStatus::ReadOnly, AppStatus::Disabled] {
            let policy = RecordingAppStatusPolicy {
                status,
                calls: AtomicUsize::new(0),
                app_ids: StdMutex::new(Vec::new()),
            };

            let result = assert_effective_read_allowed(&policy, app_id).await;
            if status == AppStatus::Disabled {
                assert_eq!(result.unwrap_err().body().kind, ApiErrorType::AppDisabled);
            } else {
                result.unwrap();
            }
            assert_eq!(policy.calls.load(Ordering::SeqCst), 1);
            assert_eq!(policy.app_ids.lock().unwrap().as_slice(), &[app_id]);
        }
    }

    #[test]
    fn object_bins_use_java_utf16_string_hashes() {
        assert_eq!(object_key_bin(""), 0);
        assert_eq!(object_key_bin("abc"), 4);
        assert_eq!(object_key_bin("😀"), 9);
        let app_id = Uuid::nil();
        assert_eq!(object_key(app_id, "abc"), format!("{app_id}/4/abc"));
    }

    fn rate_limit_request(tokens: u64) -> RateLimitRequest {
        RateLimitRequest {
            app_id: Uuid::nil(),
            bucket_name: "storage".to_owned(),
            config: RateLimitBucketConfig {
                limits: vec![RateLimit {
                    capacity: 10,
                    refill_amount: 1,
                    refill_period: Duration::from_secs(1),
                    refill_type: RefillType::Greedy,
                }],
            },
            bucket_key: json!("user"),
            tokens,
        }
    }

    #[test]
    fn deferred_rate_limits_preserve_calls_until_database_release() {
        let deferred = DeferredRateLimitConsumer::default();
        deferred.consume(&rate_limit_request(2)).unwrap();
        deferred.consume(&rate_limit_request(3)).unwrap();

        let requests = deferred.take();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tokens, 2);
        assert_eq!(requests[1].tokens, 3);
        assert!(deferred.take().is_empty());

        let limiter = InMemoryRateLimiter::default();
        consume_rate_limits(&requests, &limiter).unwrap();
        assert!(matches!(
            limiter.consume(&rate_limit_request(6)).unwrap(),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn instaql_file_signer_batches_locations_with_production_object_keys() {
        let objects = Arc::new(RecordingBatchObjectStore::default());
        let signer = StorageFileUrlSigner::new(Arc::clone(&objects));
        let app_id = Uuid::new_v4();

        let urls = signer
            .signed_download_urls(app_id, vec!["first".to_owned(), "second".to_owned()])
            .await
            .unwrap();

        let expected = vec![object_key(app_id, "first"), object_key(app_id, "second")];
        assert_eq!(
            objects.batches.lock().unwrap().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(
            urls,
            expected
                .into_iter()
                .map(|key| format!("https://objects.test/{key}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn content_type_detection_replays_every_upload_byte() {
        let chunks = [
            Bytes::from_static(b"\x89P"),
            Bytes::from_static(b"NG\r\n\x1a\n"),
            Bytes::from_static(b"payload"),
        ];
        let expected = chunks.concat();
        let body = Body::from_stream(stream::iter(chunks.into_iter().map(Ok::<_, Infallible>)));

        let (content_type, replay) = detect_content_type(body).await.unwrap();

        assert_eq!(content_type, "image/png");
        assert_eq!(
            to_bytes(replay, CONTENT_TYPE_SNIFF_BYTES * 2)
                .await
                .unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn upload_body_inspection_errors_hide_the_transport_cause() {
        let body = Body::from_stream(stream::iter([Err::<Bytes, _>(std::io::Error::other(
            "failed reading request body",
        ))]));

        let error = detect_content_type(body).await.unwrap_err();

        assert_eq!(error.body().kind, ApiErrorType::Unknown);
        assert_eq!(error.body().message, "Internal error");
    }

    #[tokio::test]
    async fn unknown_content_defaults_to_octet_stream() {
        let body = Body::from("not a recognized file format");

        let (content_type, replay) = detect_content_type(body).await.unwrap();

        assert_eq!(content_type, "application/octet-stream");
        assert_eq!(
            to_bytes(replay, CONTENT_TYPE_SNIFF_BYTES).await.unwrap(),
            "not a recognized file format"
        );
    }

    #[test]
    fn storage_operation_outcomes_split_client_rejections_from_server_errors() {
        use crate::metrics::StorageOpOutcome;

        assert_eq!(
            storage_op_outcome(&Ok::<_, ApiError>(())),
            StorageOpOutcome::Ok
        );
        assert_eq!(
            storage_op_outcome(&Err::<(), _>(ApiError::new(
                ApiErrorType::PermissionDenied,
                "no"
            ))),
            StorageOpOutcome::Rejected
        );
        assert_eq!(
            storage_op_outcome(&Err::<(), _>(upload_too_large(5, Some(6)))),
            StorageOpOutcome::Rejected
        );
        assert_eq!(
            storage_op_outcome(&Err::<(), _>(object_store_error("object store down"))),
            StorageOpOutcome::Error
        );
    }

    #[test]
    fn declared_upload_sizes_over_the_cap_are_rejected_with_413() {
        let error = reject_declared_oversized_upload(Some(6), 5).unwrap_err();
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.body().kind, ApiErrorType::ParameterLimitExceeded);
        reject_declared_oversized_upload(Some(5), 5).unwrap();
        reject_declared_oversized_upload(None, 5).unwrap();
    }

    #[tokio::test]
    async fn streamed_uploads_abort_once_they_pass_the_cap() {
        let chunks = [Bytes::from_static(b"123"), Bytes::from_static(b"456")];
        let body = Body::from_stream(stream::iter(chunks.into_iter().map(Ok::<_, Infallible>)));
        let (body, limit) = limit_upload_body(body, 5);
        assert!(to_bytes(body, usize::MAX).await.is_err());
        assert!(limit.overflowed.load(Ordering::Relaxed));

        let body = Body::from_stream(stream::iter([Ok::<_, Infallible>(Bytes::from_static(
            b"12345",
        ))]));
        let (body, limit) = limit_upload_body(body, 5);
        assert_eq!(to_bytes(body, usize::MAX).await.unwrap(), "12345");
        assert!(!limit.overflowed.load(Ordering::Relaxed));
    }
}
