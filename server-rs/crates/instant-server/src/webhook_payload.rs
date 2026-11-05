use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;

use instant_datalog::DataType;
use instant_http::{ApiError, ApiErrorType, WebhookPayloadRequest};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, SYSTEM_CATALOG_APP_ID,
};
use instant_wal::{
    EntitySnapshot, Isn, WalRecord, extract_entities_after, extract_entities_before,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::PostgresPool;
use crate::admin_storage::{StorageObjectStore, object_key};
use crate::db_error::database_error;
use crate::legacy_nippy::decode_legacy_wal_record;
use crate::platform::WebhookPayloadStore;
use crate::postgres_pool::PostgresClientSource;
use crate::wal_history::decode_wal_record;

const DEFAULT_HISTORY_OBJECT_LIMIT: usize = 64 * 1024 * 1024;

pub trait WebhookPayloadAuthorizer: Send + Sync {
    fn authorize(
        &self,
        request: &WebhookPayloadRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

pub trait WebhookFileUrlSigner: Send + Sync {
    fn sign(
        &self,
        app_id: Uuid,
        location_id: &str,
    ) -> impl Future<Output = Result<String, ApiError>> + Send;
}

/// Reader/transcoder for history rows whose payloads live in object storage.
/// A legacy adapter may decode JVM Nippy objects into the shared typed record.
pub trait WebhookHistoryArchive: Send + Sync {
    fn load(
        &self,
        app_id: Uuid,
        isn: Isn,
    ) -> impl Future<Output = Result<WalRecord, ApiError>> + Send;
}

/// Bounded object read boundary used for legacy history archives.
pub trait StorageObjectReader: Send + Sync + 'static {
    fn read_object(
        &self,
        bucket: String,
        key: String,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
}

/// Signs `$files` locations using the same key layout as the storage routes.
pub struct StorageWebhookFileUrlSigner<S> {
    objects: Arc<S>,
}

impl<S> StorageWebhookFileUrlSigner<S> {
    #[must_use]
    pub fn new(objects: Arc<S>) -> Self {
        Self { objects }
    }
}

impl<S> WebhookFileUrlSigner for StorageWebhookFileUrlSigner<S>
where
    S: StorageObjectStore,
{
    async fn sign(&self, app_id: Uuid, location_id: &str) -> Result<String, ApiError> {
        self.objects
            .signed_download_url_for_app(app_id, object_key(app_id, location_id))
            .await
            .map_err(internal)
    }
}

/// Loads JVM-era `Zstd(Nippy(WalRecord))` objects from the WAL history bucket.
pub struct S3WebhookHistoryArchive<R> {
    reader: Arc<R>,
    bucket: Option<String>,
    prefix: Option<String>,
    max_bytes: usize,
}

impl<R> S3WebhookHistoryArchive<R> {
    #[must_use]
    pub fn new(reader: Arc<R>, bucket: impl Into<String>, prefix: Option<String>) -> Self {
        Self::configured(reader, Some(bucket.into()), prefix)
    }

    /// Builds the legacy archive fallback for deployments that intentionally
    /// keep all new WAL history in PostgreSQL. `load` is only reached for an
    /// existing `storage = 's3'` row, so a missing bucket becomes a focused
    /// replay error instead of preventing PostgreSQL-backed servers from
    /// starting.
    #[must_use]
    pub fn configured(reader: Arc<R>, bucket: Option<String>, prefix: Option<String>) -> Self {
        Self {
            reader,
            bucket,
            prefix: prefix.map(|value| value.trim_matches('/').to_owned()),
            max_bytes: DEFAULT_HISTORY_OBJECT_LIMIT,
        }
    }

    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    #[must_use]
    pub fn object_key(&self, app_id: Uuid, isn: Isn) -> String {
        let suffix = format!(
            "{app_id}/{:08x}/{:016x}",
            isn.slot_num(),
            isn.lsn().as_u64()
        );
        self.prefix
            .as_ref()
            .filter(|value| !value.is_empty())
            .map_or(suffix.clone(), |prefix| format!("{prefix}/{suffix}"))
    }
}

impl<R> WebhookHistoryArchive for S3WebhookHistoryArchive<R>
where
    R: StorageObjectReader,
{
    async fn load(&self, app_id: Uuid, isn: Isn) -> Result<WalRecord, ApiError> {
        let bucket = self.bucket.clone().ok_or_else(|| {
            internal(
                "WAL history row uses S3, but this deployment has no WAL history bucket configured",
            )
        })?;
        let key = self.object_key(app_id, isn);
        let mut last_error = None;
        for _attempt in 1..=3 {
            match self
                .reader
                .read_object(bucket.clone(), key.clone(), self.max_bytes)
                .await
            {
                Ok(payload) => {
                    return decode_wal_record(&payload)
                        .or_else(|_| decode_legacy_wal_record(&payload).map_err(Into::into))
                        .map_err(|error: crate::WalHistoryError| internal(error.to_string()));
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(internal(format!(
            "WAL history S3 fetch failed after 3 attempts: {}",
            last_error.unwrap_or_else(|| "unknown object-store failure".to_owned())
        )))
    }
}

/// PostgreSQL-backed webhook replay store using native Rust WAL history.
pub struct PostgresWebhookPayloadStore<A, F, H> {
    client: PostgresClientSource,
    authorizer: Arc<A>,
    file_urls: Arc<F>,
    history_archive: Arc<H>,
}

impl<A, F, H> PostgresWebhookPayloadStore<A, F, H>
where
    A: WebhookPayloadAuthorizer,
    F: WebhookFileUrlSigner,
    H: WebhookHistoryArchive,
{
    #[must_use]
    pub fn new(client: Client, authorizer: Arc<A>, file_urls: Arc<F>, archive: Arc<H>) -> Self {
        Self::with_client_source(
            PostgresClientSource::fixed(client),
            authorizer,
            file_urls,
            archive,
        )
    }

    #[must_use]
    pub fn from_pool(
        pool: PostgresPool,
        authorizer: Arc<A>,
        file_urls: Arc<F>,
        archive: Arc<H>,
    ) -> Self {
        Self::with_client_source(
            PostgresClientSource::pooled_read_only(pool),
            authorizer,
            file_urls,
            archive,
        )
    }

    #[must_use]
    pub fn with_shared_client(
        client: Arc<Mutex<Client>>,
        authorizer: Arc<A>,
        file_urls: Arc<F>,
        history_archive: Arc<H>,
    ) -> Self {
        Self::with_client_source(
            PostgresClientSource::fixed_shared(client),
            authorizer,
            file_urls,
            history_archive,
        )
    }

    pub(crate) fn with_client_source(
        client: PostgresClientSource,
        authorizer: Arc<A>,
        file_urls: Arc<F>,
        history_archive: Arc<H>,
    ) -> Self {
        Self {
            client,
            authorizer,
            file_urls,
            history_archive,
        }
    }

    async fn load_payload(&self, request: WebhookPayloadRequest) -> Result<Value, ApiError> {
        self.authorizer.authorize(&request).await?;
        let isn = request
            .isn
            .parse::<Isn>()
            .map_err(|_| validation("Malformed Instant sequence number"))?;
        let (id_attr_ids, actions, catalog, storage, content) = {
            let client = self.client.acquire().await.map_err(database_error)?;
            let webhook = client
                .query_opt(
                    "SELECT id_attr_ids, to_jsonb(actions) FROM webhooks
                     WHERE app_id = $1 AND id = $2",
                    &[&request.app_id, &request.webhook_id],
                )
                .await
                .map_err(database_error)?
                .ok_or_else(|| not_found("webhook"))?;
            let history = client
                .query_opt(
                    "SELECT storage::text, content FROM history
                     WHERE app_id = $1 AND isn = $2::text::isn LIMIT 1",
                    &[&request.app_id, &isn.to_postgres_composite()],
                )
                .await
                .map_err(database_error)?
                .ok_or_else(|| not_found("webhook-history"))?;
            let catalog = load_catalog(&client, request.app_id).await?;
            (
                webhook.get::<_, Option<Vec<Uuid>>>(0).unwrap_or_default(),
                webhook.get::<_, tokio_postgres::types::Json<Value>>(1).0,
                catalog,
                history.get::<_, String>(0),
                history.get::<_, Option<Vec<u8>>>(1),
            )
        };
        let record = if storage == "pg" {
            let payload =
                content.ok_or_else(|| internal("PostgreSQL WAL history has no content"))?;
            decode_wal_record(&payload).map_err(|error| internal(error.to_string()))?
        } else if storage == "s3" {
            self.history_archive.load(request.app_id, isn).await?
        } else {
            return Err(internal("Unknown WAL history storage backend"));
        };
        if record.app_id != request.app_id || record.isn != isn {
            return Err(internal("WAL history identity does not match its row"));
        }
        let namespaces = id_attr_ids
            .iter()
            .filter_map(|id| catalog.by_id(*id).map(|attribute| attribute.etype.clone()))
            .collect::<Vec<_>>();
        let actions = actions
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let mut records = webhook_records(&catalog, &record, &namespaces, &actions)
            .map_err(|error| internal(error.to_string()))?;
        add_file_urls(self.file_urls.as_ref(), request.app_id, &mut records).await?;
        Ok(Value::Array(records))
    }
}

impl<A, F, H> WebhookPayloadStore for PostgresWebhookPayloadStore<A, F, H>
where
    A: WebhookPayloadAuthorizer + 'static,
    F: WebhookFileUrlSigner + 'static,
    H: WebhookHistoryArchive + 'static,
{
    async fn load(&self, request: WebhookPayloadRequest) -> Result<Value, ApiError> {
        self.load_payload(request).await
    }
}

fn webhook_records(
    catalog: &AttributeCatalog,
    record: &WalRecord,
    namespaces: &[String],
    actions: &[&str],
) -> Result<Vec<Value>, instant_wal::TopicError> {
    let after = extract_entities_after(record)?;
    let before = extract_entities_before(catalog, after.clone(), record)?;
    let mut output = Vec::new();
    append_creates_and_updates(
        &mut output,
        catalog,
        record.isn,
        &before,
        &after,
        namespaces,
        actions,
    );
    append_deletes(
        &mut output,
        catalog,
        record.isn,
        &before,
        &after,
        namespaces,
        actions,
    );
    output.sort_by_key(webhook_record_sort_key);
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn append_creates_and_updates(
    output: &mut Vec<Value>,
    catalog: &AttributeCatalog,
    isn: Isn,
    before: &EntitySnapshot,
    after: &EntitySnapshot,
    namespaces: &[String],
    actions: &[&str],
) {
    for (namespace, entities) in after {
        if !namespaces.contains(namespace) {
            continue;
        }
        for (id, entity_after) in entities {
            let entity_before = before.get(namespace).and_then(|entities| entities.get(id));
            let action = if entity_before.is_some() {
                "update"
            } else {
                "create"
            };
            if !actions.contains(&action) {
                continue;
            }
            let before = entity_before.map(|entity| labels(catalog, entity));
            let after = labels(catalog, entity_after);
            if before.as_ref() == Some(&after) {
                continue;
            }
            output.push(json!({
                "namespace": namespace,
                "action": action,
                "id": id,
                "before": before,
                "after": after,
                "idempotencyKey": record_idempotency_key(namespace, action, *id, isn),
            }));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn append_deletes(
    output: &mut Vec<Value>,
    catalog: &AttributeCatalog,
    isn: Isn,
    before: &EntitySnapshot,
    after: &EntitySnapshot,
    namespaces: &[String],
    actions: &[&str],
) {
    if !actions.contains(&"delete") {
        return;
    }
    for (namespace, entities) in before {
        if !namespaces.contains(namespace) {
            continue;
        }
        for (id, entity_before) in entities {
            let still_exists = after
                .get(namespace)
                .is_some_and(|entities| entities.contains_key(id));
            if still_exists {
                continue;
            }
            output.push(json!({
                "namespace": namespace,
                "action": "delete",
                "id": id,
                "before": labels(catalog, entity_before),
                "after": Value::Null,
                "idempotencyKey": record_idempotency_key(namespace, "delete", *id, isn),
            }));
        }
    }
}

fn labels(catalog: &AttributeCatalog, entity: &HashMap<Uuid, Value>) -> Value {
    let values = entity
        .iter()
        .filter_map(|(id, value)| {
            let attribute = catalog.by_id(*id)?;
            (attribute.value_type == AttributeValueType::Blob)
                .then(|| (attribute.label.clone(), value.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    serde_json::to_value(values).expect("string-keyed webhook entity maps serialize")
}

async fn add_file_urls<F>(signer: &F, app_id: Uuid, records: &mut [Value]) -> Result<(), ApiError>
where
    F: WebhookFileUrlSigner,
{
    let locations = records
        .iter()
        .filter(|record| record.get("namespace").and_then(Value::as_str) == Some("$files"))
        .flat_map(|record| [record.get("before"), record.get("after")])
        .flatten()
        .filter_map(|entity| entity.get("location-id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    let mut urls = HashMap::new();
    for location in locations {
        urls.insert(location.clone(), signer.sign(app_id, &location).await?);
    }
    for record in records {
        if record.get("namespace").and_then(Value::as_str) != Some("$files") {
            continue;
        }
        for field in ["before", "after"] {
            let Some(entity) = record.get_mut(field).and_then(Value::as_object_mut) else {
                continue;
            };
            let Some(location) = entity.get("location-id").and_then(Value::as_str) else {
                continue;
            };
            if let Some(url) = urls.get(location) {
                entity.insert("url".to_owned(), Value::String(url.clone()));
            }
        }
    }
    Ok(())
}

async fn load_catalog(client: &Client, app_id: Uuid) -> Result<AttributeCatalog, ApiError> {
    let rows = client
        .query(
            "SELECT id, etype, label, value_type::text, cardinality::text,
                    is_unique, is_indexed, is_required, checked_data_type::text,
                    coalesce(indexing, false), coalesce(checking_data_type, false)
             FROM attrs
             WHERE (app_id = $1 OR app_id = $2) AND deletion_marked_at IS NULL",
            &[&app_id, &SYSTEM_CATALOG_APP_ID],
        )
        .await
        .map_err(database_error)?;
    let attributes = rows
        .into_iter()
        .map(|row| {
            Ok(Attribute {
                id: row.get(0),
                wire: None,
                etype: row.get(1),
                label: row.get(2),
                value_type: match row.get::<_, String>(3).as_str() {
                    "blob" => AttributeValueType::Blob,
                    "ref" => AttributeValueType::Ref,
                    _ => return Err(internal("Unknown attribute value type")),
                },
                cardinality: match row.get::<_, String>(4).as_str() {
                    "one" => Cardinality::One,
                    "many" => Cardinality::Many,
                    _ => return Err(internal("Unknown attribute cardinality")),
                },
                unique: row.get(5),
                setting_unique: false,
                indexed: row.get(6),
                required: row.get(7),
                checked_data_type: checked_type(row.get::<_, Option<String>>(8).as_deref())?,
                indexing: row.get(9),
                checking_data_type: row.get(10),
                reverse_etype: None,
                reverse_label: None,
                on_delete: None,
                on_delete_reverse: None,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(AttributeCatalog { attributes })
}

fn checked_type(value: Option<&str>) -> Result<Option<DataType>, ApiError> {
    match value {
        None => Ok(None),
        Some("string") => Ok(Some(DataType::String)),
        Some("number") => Ok(Some(DataType::Number)),
        Some("boolean") => Ok(Some(DataType::Boolean)),
        Some("date") => Ok(Some(DataType::Date)),
        Some(_) => Err(internal("Unknown checked attribute type")),
    }
}

fn record_idempotency_key(namespace: &str, action: &str, id: Uuid, isn: Isn) -> Uuid {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update(action.as_bytes());
    digest.update(id.as_bytes());
    digest.update(isn.to_bytes());
    let digest = digest.finalize();
    Uuid::from_slice(&digest[..16]).expect("SHA-256 always contains sixteen UUID bytes")
}

fn webhook_record_sort_key(record: &Value) -> (String, String, String) {
    (
        record
            .get("namespace")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        record
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        record
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
    )
}

fn internal(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorType::Unknown, message)
}

fn validation(message: impl Into<String>) -> ApiError {
    ApiError::new(ApiErrorType::ValidationFailed, message)
}

fn not_found(record: &'static str) -> ApiError {
    ApiError::new(
        ApiErrorType::RecordNotFound,
        format!("Could not find {record}"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Utc;
    use instant_wal::{Lsn, WalAction, WalColumn, WalEntry};

    use super::*;

    struct FakeReader {
        payload: Vec<u8>,
        failures: usize,
        calls: AtomicUsize,
    }

    impl StorageObjectReader for FakeReader {
        fn read_object(
            &self,
            _bucket: String,
            _key: String,
            _max_bytes: usize,
        ) -> impl Future<Output = Result<Vec<u8>, String>> + Send {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(if call < self.failures {
                Err("temporary".to_owned())
            } else {
                Ok(self.payload.clone())
            })
        }
    }

    #[tokio::test]
    async fn s3_history_uses_the_jvm_key_retries_and_decodes_legacy_payloads() {
        let app_id = Uuid::parse_str("12345678-1234-5678-9abc-def012345678").unwrap();
        let isn = Isn::new(17, Lsn::from_u64(0x2d4_0000_0010));
        let reader = Arc::new(FakeReader {
            payload: include_bytes!("../tests/fixtures/jvm-wal-record-nippy.zstd").to_vec(),
            failures: 2,
            calls: AtomicUsize::new(0),
        });
        let archive =
            S3WebhookHistoryArchive::new(reader.clone(), "history-bucket", Some("prod".to_owned()));

        assert_eq!(
            archive.object_key(app_id, isn),
            "prod/12345678-1234-5678-9abc-def012345678/00000011/000002d400000010"
        );
        assert_eq!(archive.load(app_id, isn).await.unwrap().app_id, app_id);
        assert_eq!(reader.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn disabled_s3_fallback_fails_only_when_a_legacy_row_needs_it() {
        let reader = Arc::new(FakeReader {
            payload: Vec::new(),
            failures: 0,
            calls: AtomicUsize::new(0),
        });
        let archive = S3WebhookHistoryArchive::configured(reader.clone(), None, None);

        let error = archive
            .load(Uuid::new_v4(), Isn::new(1, Lsn::from_u64(2)))
            .await
            .unwrap_err();

        assert!(
            error
                .body()
                .message
                .contains("no WAL history bucket configured")
        );
        assert_eq!(reader.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn idempotency_keys_include_namespace_action_entity_and_isn() {
        let id = Uuid::new_v4();
        let isn = Isn::new(1, Lsn::from_u64(2));
        let key = record_idempotency_key("users", "update", id, isn);
        assert_eq!(key, record_idempotency_key("users", "update", id, isn));
        assert_ne!(key, record_idempotency_key("users", "delete", id, isn));
    }

    #[test]
    fn payload_records_label_blobs_and_filter_refs() {
        let entity = Uuid::new_v4();
        let id_attr = Uuid::new_v4();
        let name_attr = Uuid::new_v4();
        let ref_attr = Uuid::new_v4();
        let catalog = AttributeCatalog {
            attributes: vec![
                attribute(id_attr, "id", AttributeValueType::Blob),
                attribute(name_attr, "name", AttributeValueType::Blob),
                attribute(ref_attr, "friend", AttributeValueType::Ref),
            ],
        };
        let mut attributes = serde_json::Map::new();
        attributes.insert(id_attr.to_string(), json!(entity));
        attributes.insert(name_attr.to_string(), json!("New"));
        attributes.insert(ref_attr.to_string(), json!(Uuid::new_v4()));
        let content = json!([["users", entity, attributes]]).to_string();
        let record = record_with_message(entity, name_attr, content, json!(r#""Old""#));
        let records = webhook_records(
            &catalog,
            &record,
            &["users".to_owned()],
            &["create", "update", "delete"],
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["action"], "update");
        assert_eq!(records[0]["before"]["name"], "Old");
        assert_eq!(records[0]["after"]["name"], "New");
        assert!(records[0]["after"].get("friend").is_none());
    }

    fn attribute(id: Uuid, label: &str, value_type: AttributeValueType) -> Attribute {
        Attribute {
            id,
            wire: None,
            etype: "users".to_owned(),
            label: label.to_owned(),
            value_type,
            cardinality: Cardinality::One,
            unique: false,
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

    fn record_with_message(entity: Uuid, attr: Uuid, content: String, old: Value) -> WalRecord {
        let columns = vec![
            WalColumn {
                name: "entity_id".to_owned(),
                value: json!(entity),
            },
            WalColumn {
                name: "attr_id".to_owned(),
                value: json!(attr),
            },
            WalColumn {
                name: "value".to_owned(),
                value: json!("New"),
            },
        ];
        let mut identity = columns.clone();
        identity[2].value = old;
        WalRecord {
            app_id: Uuid::new_v4(),
            tx_id: 1,
            isn: Isn::new(1, Lsn::from_u64(2)),
            previous_isn: None,
            tx_created_at: Some(Utc::now()),
            tx_bytes: 1,
            next_lsn: Lsn::from_u64(2),
            attr_changes: Vec::new(),
            ident_changes: Vec::new(),
            triple_changes: vec![WalEntry {
                action: WalAction::Update,
                tx_bytes: 1,
                table: Some("triples".to_owned()),
                columns,
                identity,
                prefix: None,
                content: None,
                lsn: None,
                nextlsn: None,
            }],
            messages: vec![WalEntry {
                action: WalAction::Message,
                tx_bytes: 1,
                table: None,
                columns: Vec::new(),
                identity: Vec::new(),
                prefix: Some("update_ents".to_owned()),
                content: Some(content),
                lsn: None,
                nextlsn: None,
            }],
            wal_logs: Vec::new(),
            combined_count: 1,
        }
    }
}
