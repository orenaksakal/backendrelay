use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::Body;
use instant_http::{AdminActor, AdminRequestContext, ApiError, ApiErrorType};
use instant_instaql::{AttributeCatalog, system_attr_id};
use instant_transact::{
    EntityLookup, TransactionMode, TransactionOptions, TransactionStep, TripleOptions, TripleStep,
    execute_transaction_in,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_postgres::types::Json;
use tokio_postgres::{Client, GenericClient, Transaction};
use uuid::Uuid;

use crate::INSTANT_CONFIG_APP_ID;
use crate::PostgresPool;
use crate::admin_data::{AdminDataAuthorizer, AdminDataScope, internal_error};
use crate::admin_storage::{
    StorageObjectMetadata, StorageObjectStore, StoragePutRequest, object_key,
};
use crate::postgres_pool::PostgresClientSource;

pub type RuntimeStreamFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeStreamStartRequest {
    pub context: AdminRequestContext,
    pub client_id: String,
    pub reconnect_token: Uuid,
    pub machine_id: Uuid,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeStreamAppendRequest {
    pub context: AdminRequestContext,
    pub stream_id: Uuid,
    pub machine_id: Uuid,
    pub expected_offset: u64,
    pub chunks: Vec<String>,
    pub done: bool,
    pub abort_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeStreamLoadRequest {
    pub context: AdminRequestContext,
    pub stream_id: Option<Uuid>,
    pub client_id: Option<String>,
    pub requested_offset: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStreamFile {
    pub id: Uuid,
    pub size: u64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeStreamRecord {
    pub stream_id: Uuid,
    pub client_id: String,
    pub machine_id: Uuid,
    pub size: u64,
    pub done: bool,
    pub abort_reason: Option<String>,
    pub replay_offset: u64,
    pub files: Vec<RuntimeStreamFile>,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuntimeStreamAppendResult {
    pub stream_id: Uuid,
    pub client_id: String,
    pub offset: u64,
    pub new_offset: u64,
    pub content: String,
    pub done: bool,
    pub abort_reason: Option<String>,
    pub flushed_offset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeStreamDisconnectRequest {
    pub context: AdminRequestContext,
    pub stream_id: Uuid,
    pub machine_id: Uuid,
}

/// Durable stream identity, part storage, replay, and committed-offset boundary.
pub trait RuntimeStreamBackend: Send + Sync + 'static {
    fn start_stream(
        &self,
        request: RuntimeStreamStartRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamRecord>;

    fn append_stream(
        &self,
        request: RuntimeStreamAppendRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamAppendResult>;

    fn load_stream(
        &self,
        request: RuntimeStreamLoadRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamRecord>;

    fn disconnect_stream(
        &self,
        request: RuntimeStreamDisconnectRequest,
    ) -> RuntimeStreamFuture<'_, ()>;
}

pub struct InMemoryRuntimeStreamBackend {
    state: StdMutex<MemoryStreamState>,
    flush_limit: u64,
}

const DEFAULT_STREAM_FLUSH_LIMIT: u64 = 1024 * 1024;

impl Default for InMemoryRuntimeStreamBackend {
    fn default() -> Self {
        Self {
            state: StdMutex::new(MemoryStreamState::default()),
            flush_limit: DEFAULT_STREAM_FLUSH_LIMIT,
        }
    }
}

impl InMemoryRuntimeStreamBackend {
    #[must_use]
    pub fn with_flush_limit(mut self, flush_limit: u64) -> Self {
        self.flush_limit = flush_limit.max(1);
        self
    }
}

#[derive(Default)]
struct MemoryStreamState {
    records: HashMap<Uuid, MemoryStream>,
    by_client: HashMap<(Uuid, String), Uuid>,
}

struct MemoryStream {
    app_id: Uuid,
    stream_id: Uuid,
    client_id: String,
    token_hash: [u8; 32],
    machine_id: Uuid,
    chunks: Vec<(u64, String)>,
    size: u64,
    flushed_size: u64,
    done: bool,
    abort_reason: Option<String>,
}

impl RuntimeStreamBackend for InMemoryRuntimeStreamBackend {
    fn start_stream(
        &self,
        request: RuntimeStreamStartRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamRecord> {
        Box::pin(async move {
            let mut state = memory_state(&self.state);
            if let Some(stream_id) = state
                .by_client
                .get(&(request.context.app_id, request.client_id.clone()))
                .copied()
            {
                let stream = state
                    .records
                    .get_mut(&stream_id)
                    .expect("stream index is valid");
                let token_hash: [u8; 32] =
                    Sha256::digest(request.reconnect_token.as_bytes()).into();
                if !constant_bytes_equal(&stream.token_hash, &token_hash) {
                    return Err(validation(
                        "A stream with that clientId already exists. Reconnect token is invalid.",
                    ));
                }
                if stream.done {
                    return Err(validation("Stream is closed."));
                }
                stream.machine_id = request.machine_id;
                return Ok(memory_record(stream, 0));
            }
            let stream_id = Uuid::new_v4();
            let stream = MemoryStream {
                app_id: request.context.app_id,
                stream_id,
                client_id: request.client_id.clone(),
                token_hash: Sha256::digest(request.reconnect_token.as_bytes()).into(),
                machine_id: request.machine_id,
                chunks: Vec::new(),
                size: 0,
                flushed_size: 0,
                done: false,
                abort_reason: None,
            };
            let record = memory_record(&stream, 0);
            state
                .by_client
                .insert((request.context.app_id, request.client_id), stream_id);
            state.records.insert(stream_id, stream);
            Ok(record)
        })
    }

    fn append_stream(
        &self,
        request: RuntimeStreamAppendRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamAppendResult> {
        Box::pin(async move {
            let mut state = memory_state(&self.state);
            let stream = state
                .records
                .get_mut(&request.stream_id)
                .filter(|stream| stream.app_id == request.context.app_id)
                .ok_or_else(|| validation("Stream is missing."))?;
            validate_append(stream.machine_id, stream.size, stream.done, &request)?;
            let content = request.chunks.concat();
            let content_size = request
                .chunks
                .iter()
                .map(|chunk| chunk.len() as u64)
                .sum::<u64>();
            if !content.is_empty() {
                stream.chunks.push((stream.size, content.clone()));
            }
            let offset = stream.size;
            stream.size = stream.size.saturating_add(content_size);
            stream.done = request.done;
            stream.abort_reason.clone_from(&request.abort_reason);
            let flushed_offset = if stream.done
                || stream.size.saturating_sub(stream.flushed_size) >= self.flush_limit
            {
                stream.flushed_size = stream.size;
                Some(stream.size)
            } else {
                None
            };
            Ok(RuntimeStreamAppendResult {
                stream_id: stream.stream_id,
                client_id: stream.client_id.clone(),
                offset,
                new_offset: stream.size,
                content,
                done: stream.done,
                abort_reason: stream.abort_reason.clone(),
                flushed_offset,
            })
        })
    }

    fn load_stream(
        &self,
        request: RuntimeStreamLoadRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamRecord> {
        Box::pin(async move {
            let state = memory_state(&self.state);
            let stream_id = request
                .stream_id
                .or_else(|| {
                    request
                        .client_id
                        .as_ref()
                        .and_then(|client_id| {
                            state
                                .by_client
                                .get(&(request.context.app_id, client_id.clone()))
                        })
                        .copied()
                })
                .ok_or_else(|| validation("Stream is missing."))?;
            let stream = state
                .records
                .get(&stream_id)
                .filter(|stream| stream.app_id == request.context.app_id)
                .ok_or_else(|| validation("Stream is missing."))?;
            Ok(memory_record(stream, request.requested_offset))
        })
    }

    fn disconnect_stream(
        &self,
        request: RuntimeStreamDisconnectRequest,
    ) -> RuntimeStreamFuture<'_, ()> {
        Box::pin(async move {
            let mut state = memory_state(&self.state);
            if let Some(stream) = state
                .records
                .get_mut(&request.stream_id)
                .filter(|stream| stream.app_id == request.context.app_id)
                .filter(|stream| stream.machine_id == request.machine_id)
            {
                stream.flushed_size = stream.size;
            }
            Ok(())
        })
    }
}

fn memory_record(stream: &MemoryStream, requested_offset: u64) -> RuntimeStreamRecord {
    let selected = stream
        .chunks
        .iter()
        .filter(|(offset, content)| offset.saturating_add(content.len() as u64) > requested_offset)
        .collect::<Vec<_>>();
    RuntimeStreamRecord {
        stream_id: stream.stream_id,
        client_id: stream.client_id.clone(),
        machine_id: stream.machine_id,
        size: stream.size,
        done: stream.done,
        abort_reason: stream.abort_reason.clone(),
        replay_offset: selected.first().map_or(stream.size, |(offset, _)| *offset),
        files: Vec::new(),
        content: selected
            .into_iter()
            .map(|(_, content)| content.as_str())
            .collect(),
    }
}

async fn load_stream_flush_limit<C>(client: &C, config_app_id: Uuid) -> Result<u64, ApiError>
where
    C: GenericClient + Sync,
{
    let value = client
        .query_opt(
            "SELECT flag_value.value
             FROM attrs setting_attr
             JOIN triples flag_setting
               ON flag_setting.app_id = $1 AND flag_setting.attr_id = setting_attr.id
             JOIN attrs value_attr
               ON value_attr.app_id = $1 AND value_attr.etype = 'flags'
              AND value_attr.label = 'value'
             JOIN triples flag_value
               ON flag_value.app_id = $1
              AND flag_value.entity_id = flag_setting.entity_id
              AND flag_value.attr_id = value_attr.id
             WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'flags'
               AND setting_attr.label = 'setting'
               AND flag_setting.value = to_jsonb('stream-flush-byte-limit'::text)
             LIMIT 1",
            &[&config_app_id],
        )
        .await
        .map_err(internal_error)?
        .map(|row| row.get::<_, Value>(0));
    stream_flush_limit(value.as_ref())
}

fn stream_flush_limit(value: Option<&Value>) -> Result<u64, ApiError> {
    let Some(value) = value else {
        return Ok(DEFAULT_STREAM_FLUSH_LIMIT);
    };
    let Value::Number(number) = value else {
        return Err(internal_error(
            "config flag `stream-flush-byte-limit` must be numeric",
        ));
    };
    if let Some(limit) = number.as_u64() {
        return Ok(limit);
    }
    if number.as_i64().is_some_and(|limit| limit <= 0) {
        return Ok(0);
    }
    let Some(limit) = number.as_f64() else {
        return Err(internal_error(
            "config flag `stream-flush-byte-limit` is outside the supported numeric range",
        ));
    };
    if limit <= 0.0 {
        return Ok(0);
    }
    Ok(limit.ceil().to_string().parse::<u64>().unwrap_or(u64::MAX))
}

pub struct PostgresRuntimeStreamBackend<A, S> {
    client: PostgresClientSource,
    authorizer: Arc<A>,
    objects: Arc<S>,
    operations: Mutex<()>,
    flush_limit: u64,
    dynamic_flush_limit: bool,
    config_app_id: Uuid,
}

#[derive(Clone, Default)]
struct PendingStream {
    machine_id: Uuid,
    content: String,
    size: u64,
}

impl<A, S> PostgresRuntimeStreamBackend<A, S>
where
    A: AdminDataAuthorizer,
    S: StorageObjectStore,
{
    #[must_use]
    pub fn new(client: Client, authorizer: Arc<A>, objects: Arc<S>) -> Self {
        Self {
            client: PostgresClientSource::fixed(client),
            authorizer,
            objects,
            operations: Mutex::new(()),
            flush_limit: DEFAULT_STREAM_FLUSH_LIMIT,
            dynamic_flush_limit: false,
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    /// Builds the production backend over a bounded reconnecting pool.
    ///
    /// Runtime stream persistence uses a dedicated one-connection pool in the
    /// production composition so object-store callbacks never consume request
    /// pool capacity and a dropped PostgreSQL connection can be recreated.
    #[must_use]
    pub fn from_pool(pool: PostgresPool, authorizer: Arc<A>, objects: Arc<S>) -> Self {
        Self {
            client: PostgresClientSource::pooled(pool),
            authorizer,
            objects,
            operations: Mutex::new(()),
            flush_limit: DEFAULT_STREAM_FLUSH_LIMIT,
            dynamic_flush_limit: false,
            config_app_id: INSTANT_CONFIG_APP_ID,
        }
    }

    #[must_use]
    pub fn with_flush_limit(mut self, flush_limit: u64) -> Self {
        self.flush_limit = flush_limit.max(1);
        self
    }

    /// Reloads the config-app `stream-flush-byte-limit` flag before every
    /// append, matching the live JVM flag rather than taking a startup-only
    /// snapshot. A missing flag retains the historical 1 MiB default.
    #[must_use]
    pub const fn with_dynamic_flush_limit(mut self) -> Self {
        self.dynamic_flush_limit = true;
        self
    }

    #[must_use]
    pub const fn with_config_app_id(mut self, config_app_id: Uuid) -> Self {
        self.config_app_id = config_app_id;
        self
    }

    async fn flush_limit(&self, client: &Client) -> Result<u64, ApiError> {
        if !self.dynamic_flush_limit {
            return Ok(self.flush_limit);
        }
        load_stream_flush_limit(client, self.config_app_id).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_buffer(
        &self,
        context: &AdminRequestContext,
        catalog: &AttributeCatalog,
        initial: &RawStream,
        machine_id: Uuid,
        payload: &str,
        done: bool,
        abort_reason: Option<&str>,
    ) -> Result<(), ApiError> {
        let content_size = payload.len() as u64;
        let part_number = initial.files.len();
        let location_id = Uuid::new_v4().to_string();
        let key = object_key(context.app_id, &location_id);
        let metadata = self
            .objects
            .put(
                StoragePutRequest {
                    key: key.clone(),
                    content_type: Some("application/octet-stream".to_owned()),
                    content_disposition: None,
                    content_encoding: None,
                    content_length: Some(content_size),
                    tags: BTreeMap::new(),
                },
                Body::from(payload.to_owned()),
            )
            .await
            .map_err(object_store_error)?;
        if metadata.size != content_size {
            let _ = self.objects.delete(key).await;
            return Err(validation(
                "Uploaded stream part size did not match content length.",
            ));
        }
        let persisted = {
            let mut client = self
                .client
                .acquire()
                .await
                .map_err(runtime_stream_database_error)?;
            commit_stream_part(
                &mut client,
                catalog,
                context,
                initial,
                machine_id,
                &location_id,
                &metadata,
                part_number,
                done,
                abort_reason,
                payload,
                content_size,
            )
            .await
        };
        if let Err(error) = persisted {
            let _ = self.objects.delete(key).await;
            return Err(error);
        }
        Ok(())
    }

    async fn catalog(
        &self,
        context: &AdminRequestContext,
        scope: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        match context.actor {
            AdminActor::Admin | AdminActor::Email(_) => {
                self.authorizer.authorize(context.clone(), scope).await
            }
            AdminActor::Guest | AdminActor::RefreshToken(_) => {
                self.authorizer.load_catalog(context.app_id).await
            }
        }
    }

    async fn record_with_urls(
        &self,
        raw: RawStream,
        requested_offset: u64,
    ) -> Result<RuntimeStreamRecord, ApiError> {
        let (selected, replay_offset) = select_files(&raw.files, requested_offset);
        let mut files = Vec::with_capacity(selected.len());
        for file in selected {
            let url = self
                .objects
                .signed_download_url_for_app(raw.app_id, object_key(raw.app_id, &file.location_id))
                .await
                .map_err(object_store_error)?;
            files.push(RuntimeStreamFile {
                id: file.id,
                size: file.size,
                url,
            });
        }
        Ok(RuntimeStreamRecord {
            stream_id: raw.stream_id,
            client_id: raw.client_id,
            machine_id: raw.machine_id,
            size: raw.size,
            done: raw.done,
            abort_reason: raw.abort_reason,
            replay_offset,
            files,
            content: String::new(),
        })
    }

    async fn record_with_pending(
        &self,
        raw: RawStream,
        pending: Option<PendingStream>,
        requested_offset: u64,
    ) -> Result<RuntimeStreamRecord, ApiError> {
        let persisted_size = raw.size;
        let mut record = self.record_with_urls(raw, requested_offset).await?;
        if let Some(pending) = pending {
            let pending_end = persisted_size.saturating_add(pending.size);
            record.size = pending_end;
            if pending_end > requested_offset {
                if record.files.is_empty() {
                    record.replay_offset = persisted_size;
                }
                record.content = pending.content;
            }
        }
        Ok(record)
    }
}

impl<A, S> RuntimeStreamBackend for PostgresRuntimeStreamBackend<A, S>
where
    A: AdminDataAuthorizer,
    S: StorageObjectStore,
{
    fn start_stream(
        &self,
        request: RuntimeStreamStartRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamRecord> {
        Box::pin(async move {
            let _operation = self.operations.lock().await;
            let catalog = self
                .catalog(&request.context, AdminDataScope::Write)
                .await?;
            let (raw, pending) = {
                let mut client = self
                    .client
                    .acquire()
                    .await
                    .map_err(runtime_stream_database_error)?;
                let raw = claim_stream(&mut client, &catalog, &request).await?;
                let pending =
                    load_pending_stream(&*client, request.context.app_id, raw.stream_id).await?;
                (raw, pending)
            };
            self.record_with_pending(raw, pending, 0).await
        })
    }

    fn append_stream(
        &self,
        request: RuntimeStreamAppendRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamAppendResult> {
        Box::pin(async move {
            let _operation = self.operations.lock().await;
            let catalog = self
                .catalog(&request.context, AdminDataScope::Write)
                .await?;
            let content = request.chunks.concat();
            let (initial, pending, flush_limit) = {
                let mut client = self
                    .client
                    .acquire()
                    .await
                    .map_err(runtime_stream_database_error)?;
                let flush_limit = self.flush_limit(&client).await?;
                let (initial, pending) =
                    append_pending_stream(&mut client, &catalog, &request, &content).await?;
                (initial, pending, flush_limit)
            };
            let new_offset = initial.size.saturating_add(pending.size);
            let should_flush = request.done || pending.size >= flush_limit;
            let flushed_offset = if should_flush {
                self.persist_buffer(
                    &request.context,
                    &catalog,
                    &initial,
                    request.machine_id,
                    &pending.content,
                    request.done,
                    request.abort_reason.as_deref(),
                )
                .await?;
                Some(new_offset)
            } else {
                None
            };
            Ok(RuntimeStreamAppendResult {
                stream_id: request.stream_id,
                client_id: initial.client_id,
                offset: request.expected_offset,
                new_offset,
                content,
                done: request.done,
                abort_reason: request.abort_reason,
                flushed_offset,
            })
        })
    }

    fn load_stream(
        &self,
        request: RuntimeStreamLoadRequest,
    ) -> RuntimeStreamFuture<'_, RuntimeStreamRecord> {
        Box::pin(async move {
            let catalog = self.catalog(&request.context, AdminDataScope::Read).await?;
            let (raw, pending) = {
                let mut client = self
                    .client
                    .acquire()
                    .await
                    .map_err(runtime_stream_database_error)?;
                load_stream_snapshot(&mut client, &catalog, &request).await?
            };
            self.record_with_pending(raw, pending, request.requested_offset)
                .await
        })
    }

    fn disconnect_stream(
        &self,
        request: RuntimeStreamDisconnectRequest,
    ) -> RuntimeStreamFuture<'_, ()> {
        Box::pin(async move {
            let _operation = self.operations.lock().await;
            let catalog = self
                .catalog(&request.context, AdminDataScope::Write)
                .await?;
            let (initial, pending) = {
                let client = self
                    .client
                    .acquire()
                    .await
                    .map_err(runtime_stream_database_error)?;
                let initial = load_raw_stream(
                    &*client,
                    request.context.app_id,
                    &catalog,
                    Some(request.stream_id),
                    None,
                )
                .await?
                .ok_or_else(|| validation("Stream is missing."))?;
                let pending =
                    load_pending_stream(&*client, request.context.app_id, request.stream_id)
                        .await?;
                (initial, pending)
            };
            if initial.machine_id != request.machine_id {
                return Ok(());
            }
            let Some(pending) = pending.filter(|pending| pending.size > 0) else {
                return Ok(());
            };
            self.persist_buffer(
                &request.context,
                &catalog,
                &initial,
                request.machine_id,
                &pending.content,
                false,
                None,
            )
            .await?;
            Ok(())
        })
    }
}

struct RawStream {
    app_id: Uuid,
    stream_id: Uuid,
    client_id: String,
    reconnect_token_hash: String,
    machine_id: Uuid,
    size: u64,
    done: bool,
    abort_reason: Option<String>,
    files: Vec<RawStreamFile>,
}

#[derive(Clone)]
struct RawStreamFile {
    id: Uuid,
    path: String,
    location_id: String,
    size: u64,
}

async fn claim_stream(
    client: &mut Client,
    catalog: &AttributeCatalog,
    request: &RuntimeStreamStartRequest,
) -> Result<RawStream, ApiError> {
    let transaction = client.transaction().await.map_err(internal_error)?;
    lock_stream_client(&transaction, request.context.app_id, &request.client_id).await?;
    let existing = load_raw_stream(
        &transaction,
        request.context.app_id,
        catalog,
        None,
        Some(&request.client_id),
    )
    .await?;
    let raw = if let Some(existing) = existing {
        lock_stream(&transaction, request.context.app_id, existing.stream_id).await?;
        let mut current = load_raw_stream(
            &transaction,
            request.context.app_id,
            catalog,
            Some(existing.stream_id),
            None,
        )
        .await?
        .ok_or_else(|| validation("Stream is missing."))?;
        validate_reconnect(&current, request.reconnect_token)?;
        transaction
            .execute(
                "UPDATE rust_realtime_stream_buffers SET machine_id = $3, updated_at = now()
                 WHERE app_id = $1 AND stream_id = $2",
                &[
                    &request.context.app_id,
                    &current.stream_id,
                    &request.machine_id,
                ],
            )
            .await
            .map_err(internal_error)?;
        execute_transaction_in(
            &transaction,
            request.context.app_id,
            catalog,
            &[stream_value(
                current.stream_id,
                catalog,
                "machineId",
                json!(request.machine_id),
                None,
            )?],
            stream_transaction_options(true),
        )
        .await
        .map_err(internal_error)?;
        current.machine_id = request.machine_id;
        current
    } else {
        create_stream(&transaction, catalog, request).await?
    };
    transaction.commit().await.map_err(internal_error)?;
    Ok(raw)
}

fn validate_reconnect(stream: &RawStream, reconnect_token: Uuid) -> Result<(), ApiError> {
    if !constant_bytes_equal(
        stream.reconnect_token_hash.as_bytes(),
        token_hash_hex(reconnect_token).as_bytes(),
    ) {
        return Err(validation(
            "A stream with that clientId already exists. Reconnect token is invalid.",
        ));
    }
    if stream.done {
        return Err(validation("Stream is closed."));
    }
    Ok(())
}

async fn create_stream(
    transaction: &Transaction<'_>,
    catalog: &AttributeCatalog,
    request: &RuntimeStreamStartRequest,
) -> Result<RawStream, ApiError> {
    let stream_id = Uuid::new_v4();
    let create = Some(TransactionMode::Create);
    let reconnect_token_hash = token_hash_hex(request.reconnect_token);
    let steps = [
        stream_value(stream_id, catalog, "id", json!(stream_id), create)?,
        stream_value(
            stream_id,
            catalog,
            "clientId",
            json!(request.client_id),
            create,
        )?,
        stream_value(
            stream_id,
            catalog,
            "machineId",
            json!(request.machine_id),
            create,
        )?,
        stream_value(
            stream_id,
            catalog,
            "hashedReconnectToken",
            json!(reconnect_token_hash),
            create,
        )?,
    ];
    execute_transaction_in(
        transaction,
        request.context.app_id,
        catalog,
        &steps,
        stream_transaction_options(false),
    )
    .await
    .map_err(internal_error)?;
    Ok(RawStream {
        app_id: request.context.app_id,
        stream_id,
        client_id: request.client_id.clone(),
        reconnect_token_hash,
        machine_id: request.machine_id,
        size: 0,
        done: false,
        abort_reason: None,
        files: Vec::new(),
    })
}

async fn lock_stream_client(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    client_id: &str,
) -> Result<(), ApiError> {
    transaction
        .query_one(
            "SELECT pg_advisory_xact_lock(
               hashtextextended($1::uuid::text || ':stream-client:' || $2, 0))",
            &[&app_id, &client_id],
        )
        .await
        .map_err(internal_error)?;
    Ok(())
}

async fn lock_stream(
    transaction: &Transaction<'_>,
    app_id: Uuid,
    stream_id: Uuid,
) -> Result<(), ApiError> {
    transaction
        .query_one(
            "SELECT pg_advisory_xact_lock(
               hashtextextended($1::uuid::text || ':stream:' || $2::uuid::text, 0))",
            &[&app_id, &stream_id],
        )
        .await
        .map_err(internal_error)?;
    Ok(())
}

async fn load_pending_stream<C>(
    client: &C,
    app_id: Uuid,
    stream_id: Uuid,
) -> Result<Option<PendingStream>, ApiError>
where
    C: GenericClient + Sync,
{
    client
        .query_opt(
            "SELECT machine_id, content, size
             FROM rust_realtime_stream_buffers
             WHERE app_id = $1 AND stream_id = $2",
            &[&app_id, &stream_id],
        )
        .await
        .map_err(internal_error)?
        .map(|row| {
            let size = row.get::<_, i64>(2);
            Ok(PendingStream {
                machine_id: row.get(0),
                content: row.get(1),
                size: u64::try_from(size).map_err(|error| internal_error(error.to_string()))?,
            })
        })
        .transpose()
}

async fn append_pending_stream(
    client: &mut Client,
    catalog: &AttributeCatalog,
    request: &RuntimeStreamAppendRequest,
    content: &str,
) -> Result<(RawStream, PendingStream), ApiError> {
    let transaction = client.transaction().await.map_err(internal_error)?;
    lock_stream(&transaction, request.context.app_id, request.stream_id).await?;
    let initial = load_raw_stream(
        &transaction,
        request.context.app_id,
        catalog,
        Some(request.stream_id),
        None,
    )
    .await?
    .ok_or_else(|| validation("Stream is missing."))?;
    let previous = load_pending_stream(&transaction, request.context.app_id, request.stream_id)
        .await?
        .unwrap_or_default();
    if previous.size > 0 && previous.machine_id != request.machine_id {
        return Err(validation("Stream updated from a different machine."));
    }
    validate_append(
        initial.machine_id,
        initial.size.saturating_add(previous.size),
        initial.done,
        request,
    )?;
    let content_size = content.len() as u64;
    let pending = PendingStream {
        machine_id: request.machine_id,
        content: format!("{}{content}", previous.content),
        size: previous.size.saturating_add(content_size),
    };
    transaction
        .execute(
            "INSERT INTO rust_realtime_stream_buffers
               (app_id, stream_id, machine_id, content, size)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (app_id, stream_id) DO UPDATE SET
               machine_id = EXCLUDED.machine_id,
               content = EXCLUDED.content,
               size = EXCLUDED.size,
               updated_at = now()",
            &[
                &request.context.app_id,
                &request.stream_id,
                &request.machine_id,
                &pending.content,
                &i64::try_from(pending.size).map_err(|error| validation(&error.to_string()))?,
            ],
        )
        .await
        .map_err(internal_error)?;
    transaction.commit().await.map_err(internal_error)?;
    Ok((initial, pending))
}

async fn load_stream_snapshot(
    client: &mut Client,
    catalog: &AttributeCatalog,
    request: &RuntimeStreamLoadRequest,
) -> Result<(RawStream, Option<PendingStream>), ApiError> {
    let transaction = client.transaction().await.map_err(internal_error)?;
    let selected = load_raw_stream(
        &transaction,
        request.context.app_id,
        catalog,
        request.stream_id,
        request.client_id.as_deref(),
    )
    .await?
    .ok_or_else(|| validation("Stream is missing."))?;
    lock_stream(&transaction, request.context.app_id, selected.stream_id).await?;
    let raw = load_raw_stream(
        &transaction,
        request.context.app_id,
        catalog,
        Some(selected.stream_id),
        None,
    )
    .await?
    .ok_or_else(|| validation("Stream is missing."))?;
    let pending = load_pending_stream(&transaction, request.context.app_id, raw.stream_id).await?;
    transaction.commit().await.map_err(internal_error)?;
    Ok((raw, pending))
}

async fn load_raw_stream<C>(
    client: &C,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    stream_id: Option<Uuid>,
    client_id: Option<&str>,
) -> Result<Option<RawStream>, ApiError>
where
    C: GenericClient + Sync,
{
    let stream_id = if let Some(stream_id) = stream_id {
        stream_id
    } else {
        let client_id = client_id.ok_or_else(|| validation("Stream selector is missing."))?;
        let client_attr = required_attr(catalog, "$streams", "clientId")?;
        let Some(row) = client
            .query_opt(
                "select entity_id from triples \
                 where app_id = $1 and attr_id = $2 and value = $3::jsonb limit 1",
                &[&app_id, &client_attr, &Json(json!(client_id))],
            )
            .await
            .map_err(internal_error)?
        else {
            return Ok(None);
        };
        row.get(0)
    };
    let client_attr = required_attr(catalog, "$streams", "clientId")?;
    let token_attr = required_attr(catalog, "$streams", "hashedReconnectToken")?;
    let machine_attr = required_attr(catalog, "$streams", "machineId")?;
    let done_attr = required_attr(catalog, "$streams", "done")?;
    let size_attr = required_attr(catalog, "$streams", "size")?;
    let abort_attr = required_attr(catalog, "$streams", "abortReason")?;
    let file_link_attr = required_attr(catalog, "$streams", "$files")?;
    let rows = client
        .query(
            "select attr_id, value from triples where app_id = $1 and entity_id = $2",
            &[&app_id, &stream_id],
        )
        .await
        .map_err(internal_error)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut client_value = None;
    let mut token_hash = None;
    let mut machine_id = None;
    let mut done = false;
    let mut declared_size = None;
    let mut abort_reason = None;
    let mut file_ids = Vec::new();
    for row in rows {
        let attribute_id = row.get::<_, Uuid>(0);
        let value = row.get::<_, Json<Value>>(1).0;
        if attribute_id == client_attr {
            client_value = value.as_str().map(str::to_owned);
        } else if attribute_id == token_attr {
            token_hash = value.as_str().map(str::to_owned);
        } else if attribute_id == machine_attr {
            machine_id = value.as_str().and_then(|value| Uuid::parse_str(value).ok());
        } else if attribute_id == done_attr {
            done = value.as_bool().unwrap_or(false);
        } else if attribute_id == size_attr {
            declared_size = value.as_u64();
        } else if attribute_id == abort_attr {
            abort_reason = value.as_str().map(str::to_owned);
        } else if attribute_id == file_link_attr
            && let Some(file_id) = value.as_str().and_then(|value| Uuid::parse_str(value).ok())
        {
            file_ids.push(file_id);
        }
    }
    let client_id = client_value.ok_or_else(|| validation("Stream is invalid."))?;
    let reconnect_token_hash = token_hash.ok_or_else(|| validation("Stream is invalid."))?;
    let machine_id = machine_id.ok_or_else(|| validation("Stream is invalid."))?;
    let mut files = load_stream_files(client, app_id, catalog, &file_ids).await?;
    files.sort_by_key(|file| stream_part_number(stream_id, &file.path).unwrap_or(usize::MAX));
    let file_size = files.iter().map(|file| file.size).sum::<u64>();
    let size = declared_size.unwrap_or(file_size);
    if size != file_size {
        return Err(validation(
            "Stream is corrupted. Size does not match files.",
        ));
    }
    Ok(Some(RawStream {
        app_id,
        stream_id,
        client_id,
        reconnect_token_hash,
        machine_id,
        size,
        done,
        abort_reason,
        files,
    }))
}

async fn load_stream_files<C>(
    client: &C,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    file_ids: &[Uuid],
) -> Result<Vec<RawStreamFile>, ApiError>
where
    C: GenericClient + Sync,
{
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    let path_attr = required_attr(catalog, "$files", "path")?;
    let location_attr = required_attr(catalog, "$files", "location-id")?;
    let size_attr = required_attr(catalog, "$files", "size")?;
    let rows = client
        .query(
            "select entity_id, attr_id, value from triples \
             where app_id = $1 and entity_id = any($2) \
               and attr_id = any($3::uuid[])",
            &[
                &app_id,
                &file_ids,
                &vec![path_attr, location_attr, size_attr],
            ],
        )
        .await
        .map_err(internal_error)?;
    let mut files = HashMap::<Uuid, (Option<String>, Option<String>, Option<u64>)>::new();
    for row in rows {
        let file_id = row.get::<_, Uuid>(0);
        let attribute_id = row.get::<_, Uuid>(1);
        let value = row.get::<_, Json<Value>>(2).0;
        let entry = files.entry(file_id).or_default();
        if attribute_id == path_attr {
            entry.0 = value.as_str().map(str::to_owned);
        } else if attribute_id == location_attr {
            entry.1 = value.as_str().map(str::to_owned);
        } else if attribute_id == size_attr {
            entry.2 = value.as_u64();
        }
    }
    file_ids
        .iter()
        .map(|file_id| {
            let (path, location_id, size) = files
                .remove(file_id)
                .ok_or_else(|| validation("Stream is corrupted. Missing file."))?;
            Ok(RawStreamFile {
                id: *file_id,
                path: path.ok_or_else(|| validation("Stream is corrupted. Missing file path."))?,
                location_id: location_id
                    .ok_or_else(|| validation("Stream is corrupted. Missing file location."))?,
                size: size.ok_or_else(|| validation("Stream is corrupted. Missing file size."))?,
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn commit_stream_part(
    client: &mut Client,
    catalog: &AttributeCatalog,
    context: &AdminRequestContext,
    initial: &RawStream,
    machine_id: Uuid,
    location_id: &str,
    metadata: &StorageObjectMetadata,
    part_number: usize,
    done: bool,
    abort_reason: Option<&str>,
    payload: &str,
    content_size: u64,
) -> Result<(), ApiError> {
    let transaction = client.transaction().await.map_err(internal_error)?;
    lock_stream(&transaction, context.app_id, initial.stream_id).await?;
    let current = load_raw_stream(
        &transaction,
        context.app_id,
        catalog,
        Some(initial.stream_id),
        None,
    )
    .await?
    .ok_or_else(|| validation("Stream is missing."))?;
    if current.machine_id != machine_id {
        return Err(validation("Stream updated from a different machine."));
    }
    if current.done || current.size != initial.size || current.files.len() != part_number {
        return Err(validation("Invalid offset for stream."));
    }
    let pending = load_pending_stream(&transaction, context.app_id, initial.stream_id)
        .await?
        .ok_or_else(|| validation("Stream buffer is missing."))?;
    if pending.machine_id != machine_id
        || pending.size != content_size
        || pending.content != payload
    {
        return Err(validation("Stream buffer changed while it was flushing."));
    }
    persist_stream_part(
        &transaction,
        catalog,
        context,
        initial.stream_id,
        location_id,
        metadata,
        part_number,
        done,
        abort_reason,
        initial.size.saturating_add(content_size),
    )
    .await?;
    transaction
        .execute(
            "DELETE FROM rust_realtime_stream_buffers
             WHERE app_id = $1 AND stream_id = $2 AND machine_id = $3",
            &[&context.app_id, &initial.stream_id, &machine_id],
        )
        .await
        .map_err(internal_error)?;
    transaction.commit().await.map_err(internal_error)
}

#[allow(clippy::too_many_arguments)]
async fn persist_stream_part(
    transaction: &Transaction<'_>,
    catalog: &AttributeCatalog,
    context: &AdminRequestContext,
    stream_id: Uuid,
    location_id: &str,
    metadata: &StorageObjectMetadata,
    part_number: usize,
    done: bool,
    abort_reason: Option<&str>,
    final_size: u64,
) -> Result<(), ApiError> {
    let file_id = Uuid::new_v4();
    let path = format!("$stream/{stream_id}/{part_number}");
    let create = Some(TransactionMode::Create);
    let mut steps = vec![
        value_step("$files", file_id, catalog, "id", json!(file_id), create)?,
        value_step("$files", file_id, catalog, "path", json!(path), create)?,
        value_step(
            "$files",
            file_id,
            catalog,
            "size",
            json!(metadata.size),
            create,
        )?,
        value_step(
            "$files",
            file_id,
            catalog,
            "content-type",
            json!("application/octet-stream"),
            create,
        )?,
        value_step(
            "$files",
            file_id,
            catalog,
            "location-id",
            json!(location_id),
            create,
        )?,
        value_step("$files", file_id, catalog, "key-version", json!(1), create)?,
        TransactionStep::AddTriple(TripleStep {
            entity: EntityLookup::Id(stream_id),
            etype: Some("$streams".to_owned()),
            attr_id: required_attr(catalog, "$streams", "$files")?,
            reverse_etype: Some("$files".to_owned()),
            value: json!(file_id),
            options: TripleOptions::default(),
        }),
    ];
    if done {
        steps.push(stream_value(stream_id, catalog, "done", json!(true), None)?);
        steps.push(stream_value(
            stream_id,
            catalog,
            "size",
            json!(final_size),
            None,
        )?);
        if let Some(abort_reason) = abort_reason {
            steps.push(stream_value(
                stream_id,
                catalog,
                "abortReason",
                json!(abort_reason),
                None,
            )?);
        }
    }
    execute_transaction_in(
        transaction,
        context.app_id,
        catalog,
        &steps,
        stream_file_transaction_options(),
    )
    .await
    .map_err(internal_error)?;
    Ok(())
}

fn stream_value(
    stream_id: Uuid,
    catalog: &AttributeCatalog,
    label: &str,
    value: Value,
    mode: Option<TransactionMode>,
) -> Result<TransactionStep, ApiError> {
    value_step("$streams", stream_id, catalog, label, value, mode)
}

fn value_step(
    etype: &str,
    entity_id: Uuid,
    catalog: &AttributeCatalog,
    label: &str,
    value: Value,
    mode: Option<TransactionMode>,
) -> Result<TransactionStep, ApiError> {
    Ok(TransactionStep::AddTriple(TripleStep {
        entity: EntityLookup::Id(entity_id),
        etype: Some(etype.to_owned()),
        attr_id: required_attr(catalog, etype, label)?,
        reverse_etype: None,
        value,
        options: TripleOptions { mode },
    }))
}

fn required_attr(catalog: &AttributeCatalog, etype: &str, label: &str) -> Result<Uuid, ApiError> {
    catalog
        .forward(etype, label)
        .map(|attribute| attribute.id)
        .or_else(|| system_attr_id(etype, label).ok())
        .ok_or_else(|| internal_error(format!("missing {etype}.{label} system attribute")))
}

fn validate_append(
    machine_id: Uuid,
    size: u64,
    done: bool,
    request: &RuntimeStreamAppendRequest,
) -> Result<(), ApiError> {
    if machine_id != request.machine_id {
        return Err(validation("Stream updated from a different machine."));
    }
    if done {
        return Err(validation("Stream is completed."));
    }
    if size != request.expected_offset {
        return Err(validation("Invalid offset for stream."));
    }
    Ok(())
}

fn select_files(files: &[RawStreamFile], requested_offset: u64) -> (Vec<RawStreamFile>, u64) {
    let mut cursor = 0_u64;
    let mut replay_offset = 0_u64;
    let mut selected = Vec::new();
    for file in files {
        let next_offset = cursor.saturating_add(file.size);
        if !selected.is_empty() || next_offset > requested_offset {
            selected.push(file.clone());
        } else {
            replay_offset = next_offset;
        }
        cursor = next_offset;
    }
    (selected, replay_offset)
}

fn stream_part_number(stream_id: Uuid, path: &str) -> Option<usize> {
    path.strip_prefix(&format!("$stream/{stream_id}/"))?
        .parse()
        .ok()
}

fn token_hash_hex(token: Uuid) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

const fn stream_transaction_options(overwrite_created_at: bool) -> TransactionOptions {
    TransactionOptions {
        allow_system_catalog_updates: false,
        admin: false,
        allow_file_metadata_updates: false,
        allow_stream_metadata_updates: true,
        overwrite_created_at,
        invalidator_slot_num: 0,
        wal_entity_log: instant_transact::WalEntityLog::Disabled,
    }
}

const fn stream_file_transaction_options() -> TransactionOptions {
    TransactionOptions {
        allow_system_catalog_updates: false,
        admin: false,
        allow_file_metadata_updates: true,
        allow_stream_metadata_updates: true,
        overwrite_created_at: false,
        invalidator_slot_num: 0,
        wal_entity_log: instant_transact::WalEntityLog::Disabled,
    }
}

fn memory_state(
    state: &StdMutex<MemoryStreamState>,
) -> std::sync::MutexGuard<'_, MemoryStreamState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn constant_bytes_equal(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        difference |= usize::from(
            left.get(index).copied().unwrap_or_default()
                ^ right.get(index).copied().unwrap_or_default(),
        );
    }
    difference == 0
}

fn validation(message: &str) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn runtime_stream_database_error(error: impl std::fmt::Display) -> ApiError {
    crate::db_error::database_error(error)
}

fn object_store_error(error: impl std::fmt::Display) -> ApiError {
    internal_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use instant_http::AdminActor;
    use tokio_postgres::{NoTls, types::Json};

    fn context(app_id: Uuid) -> AdminRequestContext {
        AdminRequestContext {
            app_id,
            bearer_token: Some("admin-secret".to_owned()),
            actor: AdminActor::Admin,
            client_ip: None,
        }
    }

    #[test]
    fn dynamic_flush_limit_matches_numeric_threshold_semantics() {
        assert_eq!(stream_flush_limit(None).unwrap(), 1024 * 1024);
        assert_eq!(stream_flush_limit(Some(&json!(5))).unwrap(), 5);
        assert_eq!(stream_flush_limit(Some(&json!(4.2))).unwrap(), 5);
        assert_eq!(stream_flush_limit(Some(&json!(0))).unwrap(), 0);
        assert_eq!(stream_flush_limit(Some(&json!(-1))).unwrap(), 0);
        assert!(stream_flush_limit(Some(&json!("5"))).is_err());
    }

    #[tokio::test]
    async fn dynamic_flush_limit_reloads_live_postgres_flag() {
        let Some(database_url) = instant_test_support::test_database_url(
            "dynamic_flush_limit_reloads_live_postgres_flag",
        ) else {
            return;
        };
        let (client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
        let connection_task = tokio::spawn(connection);
        client
            .batch_execute(
                "CREATE TEMP TABLE attrs (
                   id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL
                 );
                 CREATE TEMP TABLE triples (
                   app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL,
                   value jsonb NOT NULL
                 );",
            )
            .await
            .unwrap();
        let config_app_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();
        let setting_attr_id = Uuid::new_v4();
        let value_attr_id = Uuid::new_v4();
        client
            .execute(
                "INSERT INTO attrs (id, app_id, etype, label) VALUES
                   ($1, $3, 'flags', 'setting'), ($2, $3, 'flags', 'value')",
                &[&setting_attr_id, &value_attr_id, &config_app_id],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO triples (app_id, entity_id, attr_id, value) VALUES
                   ($1, $2, $3, $4), ($1, $2, $5, $6)",
                &[
                    &config_app_id,
                    &entity_id,
                    &setting_attr_id,
                    &Json(json!("stream-flush-byte-limit")),
                    &value_attr_id,
                    &Json(json!(5)),
                ],
            )
            .await
            .unwrap();

        assert_eq!(
            load_stream_flush_limit(&client, config_app_id)
                .await
                .unwrap(),
            5
        );
        client
            .execute(
                "UPDATE triples SET value = $4
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&config_app_id, &entity_id, &value_attr_id, &Json(json!(9))],
            )
            .await
            .unwrap();
        assert_eq!(
            load_stream_flush_limit(&client, config_app_id)
                .await
                .unwrap(),
            9
        );

        drop(client);
        connection_task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn memory_backend_survives_coordinator_replacement_and_validates_resume() {
        let backend = InMemoryRuntimeStreamBackend::default();
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let started = backend
            .start_stream(RuntimeStreamStartRequest {
                context: context(app_id),
                client_id: "client-stream".to_owned(),
                reconnect_token: token,
                machine_id,
            })
            .await
            .unwrap();
        backend
            .append_stream(RuntimeStreamAppendRequest {
                context: context(app_id),
                stream_id: started.stream_id,
                machine_id,
                expected_offset: 0,
                chunks: vec!["hel".to_owned(), "lo".to_owned()],
                done: false,
                abort_reason: None,
            })
            .await
            .unwrap();

        let replacement_machine = Uuid::new_v4();
        let resumed = backend
            .start_stream(RuntimeStreamStartRequest {
                context: context(app_id),
                client_id: "client-stream".to_owned(),
                reconnect_token: token,
                machine_id: replacement_machine,
            })
            .await
            .unwrap();
        assert_eq!(resumed.stream_id, started.stream_id);
        assert_eq!(resumed.machine_id, replacement_machine);
        assert_eq!(resumed.size, 5);
        assert_eq!(resumed.content, "hello");

        let replay = backend
            .load_stream(RuntimeStreamLoadRequest {
                context: context(app_id),
                stream_id: Some(started.stream_id),
                client_id: None,
                requested_offset: 2,
            })
            .await
            .unwrap();
        assert_eq!(replay.replay_offset, 0);
        assert_eq!(replay.content, "hello");

        let invalid_token = backend
            .start_stream(RuntimeStreamStartRequest {
                context: context(app_id),
                client_id: "client-stream".to_owned(),
                reconnect_token: Uuid::new_v4(),
                machine_id: Uuid::new_v4(),
            })
            .await
            .unwrap_err();
        assert_eq!(invalid_token.body().kind, ApiErrorType::ValidationFailed);
    }

    #[tokio::test]
    async fn memory_backend_flushes_only_at_limit_completion_or_disconnect() {
        let backend = InMemoryRuntimeStreamBackend::default().with_flush_limit(5);
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let started = backend
            .start_stream(RuntimeStreamStartRequest {
                context: context(app_id),
                client_id: "buffered".to_owned(),
                reconnect_token: Uuid::new_v4(),
                machine_id,
            })
            .await
            .unwrap();
        let buffered = backend
            .append_stream(RuntimeStreamAppendRequest {
                context: context(app_id),
                stream_id: started.stream_id,
                machine_id,
                expected_offset: 0,
                chunks: vec!["four".to_owned()],
                done: false,
                abort_reason: None,
            })
            .await
            .unwrap();
        assert_eq!(buffered.flushed_offset, None);
        let threshold = backend
            .append_stream(RuntimeStreamAppendRequest {
                context: context(app_id),
                stream_id: started.stream_id,
                machine_id,
                expected_offset: 4,
                chunks: vec!["!".to_owned()],
                done: false,
                abort_reason: None,
            })
            .await
            .unwrap();
        assert_eq!(threshold.flushed_offset, Some(5));
        backend
            .append_stream(RuntimeStreamAppendRequest {
                context: context(app_id),
                stream_id: started.stream_id,
                machine_id,
                expected_offset: 5,
                chunks: vec!["x".to_owned()],
                done: false,
                abort_reason: None,
            })
            .await
            .unwrap();
        backend
            .disconnect_stream(RuntimeStreamDisconnectRequest {
                context: context(app_id),
                stream_id: started.stream_id,
                machine_id,
            })
            .await
            .unwrap();
        assert_eq!(
            memory_state(&backend.state).records[&started.stream_id].flushed_size,
            6
        );
    }

    #[test]
    fn file_selection_replays_the_whole_part_containing_the_offset() {
        let stream_id = Uuid::new_v4();
        let files = [
            raw_file(stream_id, 0, 4),
            raw_file(stream_id, 1, 6),
            raw_file(stream_id, 2, 3),
        ];
        let (selected, replay_offset) = select_files(&files, 7);
        assert_eq!(replay_offset, 4);
        assert_eq!(selected.len(), 2);
        assert_eq!(stream_part_number(stream_id, &selected[0].path), Some(1));

        let (selected, replay_offset) = select_files(&files, 13);
        assert!(selected.is_empty());
        assert_eq!(replay_offset, 13);
    }

    fn raw_file(stream_id: Uuid, part: usize, size: u64) -> RawStreamFile {
        RawStreamFile {
            id: Uuid::new_v4(),
            path: format!("$stream/{stream_id}/{part}"),
            location_id: Uuid::new_v4().to_string(),
            size,
        }
    }
}
