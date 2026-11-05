use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, HeaderValue, Method, header};
use instant_http::{DashboardRequest, DashboardResponse};
use instant_server::{
    BackupObjectEntry, BackupObjectStore, DashboardControlService, DashboardSubsystem,
    NoopDashboardControlHooks, PostgresBackupRestoreControlHooks, PostgresPool,
    PostgresRestoreControlHooks, PostgresRestoreWorker, StorageObjectMetadata, StorageObjectStore,
    StoragePutRequest,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use uuid::Uuid;

const STAGING_EMAIL: &str = "ephemeral-apps-dev@backendrelay.invalid";

#[derive(Default)]
struct MemoryObjects(Mutex<HashMap<String, Vec<u8>>>);

impl MemoryObjects {
    fn seed(&self, key: String, bytes: Vec<u8>) {
        self.0.lock().unwrap().insert(key, bytes);
    }
}

impl StorageObjectStore for MemoryObjects {
    async fn put(
        &self,
        request: StoragePutRequest,
        body: Body,
    ) -> Result<StorageObjectMetadata, String> {
        let bytes = to_bytes(body, usize::MAX)
            .await
            .map_err(|error| error.to_string())?;
        self.0.lock().unwrap().insert(request.key, bytes.to_vec());
        Ok(StorageObjectMetadata {
            size: bytes.len() as u64,
            content_type: request.content_type,
            content_disposition: request.content_disposition,
            etag: None,
            last_modified_ms: None,
        })
    }

    fn delete(&self, key: String) -> impl Future<Output = Result<(), String>> + Send {
        self.0.lock().unwrap().remove(&key);
        std::future::ready(Ok(()))
    }

    fn signed_download_url(
        &self,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        std::future::ready(Ok(format!("memory://{key}")))
    }
}

impl BackupObjectStore for MemoryObjects {
    fn list_backup_objects(
        &self,
        prefix: String,
    ) -> impl Future<Output = Result<Vec<BackupObjectEntry>, String>> + Send {
        let mut objects = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, bytes)| BackupObjectEntry {
                key: key.clone(),
                size: bytes.len() as u64,
            })
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| left.key.cmp(&right.key));
        std::future::ready(Ok(objects))
    }

    fn sign_backup_object(
        &self,
        key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        std::future::ready(Ok(format!("memory://{key}")))
    }

    fn read_backup_object(
        &self,
        key: String,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, String>> + Send {
        let entry = self.0.lock().unwrap().get(&key).cloned();
        std::future::ready(match entry {
            None => Ok(None),
            Some(bytes) if bytes.len() > max_bytes => Err(format!(
                "backup object {key} exceeds the {max_bytes}-byte limit"
            )),
            Some(bytes) => Ok(Some(bytes)),
        })
    }
}

fn scoped_url(url: &str, schema: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}%2Cpublic")
}

fn request(method: Method, path: &str, token: Uuid, body: Vec<u8>) -> DashboardRequest {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    DashboardRequest {
        method,
        path: path.to_owned(),
        query: None,
        headers,
        body: Bytes::from(body),
        staged_body_path: None,
        remote_addr: None,
    }
}

#[allow(clippy::needless_pass_by_value)]
fn json_body(response: DashboardResponse) -> Value {
    serde_json::from_slice(response.body.buffered().unwrap()).unwrap()
}

fn compressed(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(bytes, 3).unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the full dashboard-to-worker restore contract in one database.
async fn owner_restore_of_a_stored_backup_creates_a_new_app_with_the_backup_triples() {
    let Some(url) = instant_test_support::test_database_url(
        "owner_restore_of_a_stored_backup_creates_a_new_app_with_the_backup_triples",
    ) else {
        return;
    };
    let schema = format!("backup_restore_{}", Uuid::new_v4().simple());
    let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    setup
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let scoped = scoped_url(&url, &schema);
    let (client, connection) = tokio_postgres::connect(&scoped, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TYPE checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TYPE attr_on_delete AS ENUM ('cascade');
             CREATE TABLE instant_users (
               id uuid PRIMARY KEY,
               email text NOT NULL UNIQUE
             );
             CREATE TABLE instant_user_refresh_tokens (
               id uuid PRIMARY KEY,
               token_hash bytea NOT NULL,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               last_used_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE apps (
               id uuid PRIMARY KEY,
               creator_id uuid,
               org_id uuid,
               title text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(),
               status text NOT NULL DEFAULT 'active',
               magic_code_expiry_minutes integer,
               deletion_marked_at timestamptz
             );
             CREATE TABLE app_admin_tokens (
               token uuid PRIMARY KEY,
               app_id uuid NOT NULL UNIQUE REFERENCES apps(id) ON DELETE CASCADE
             );
             CREATE TABLE app_backups (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               isn isn,
               backup_at timestamptz NOT NULL,
               storage_prefix text NOT NULL,
               files_size bigint,
               db_size bigint,
               uncompressed_size bigint,
               description text,
               expires_at timestamptz,
               deletion_marked_at timestamptz
             );
             CREATE TABLE app_restore_jobs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               source_app_id uuid,
               creator_id uuid,
               org_id uuid,
               title text,
               zip_path text NOT NULL,
               job_status text NOT NULL DEFAULT 'waiting',
               progress text,
               error text,
               created_at timestamptz NOT NULL DEFAULT now(),
               done_at timestamptz,
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TABLE attrs (
               id uuid NOT NULL,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL DEFAULT false,
               is_indexed boolean NOT NULL DEFAULT false,
               is_required boolean NOT NULL DEFAULT false,
               forward_ident uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               reverse_ident uuid,
               reverse_etype text,
               reverse_label text,
               inferred_types bit(32),
               on_delete attr_on_delete,
               on_delete_reverse attr_on_delete,
               checked_data_type checked_data_type,
               checking_data_type boolean,
               indexing boolean,
               setting_unique boolean,
               metadata jsonb,
               deletion_marked_at timestamptz,
               PRIMARY KEY (app_id, id)
             );
             CREATE UNIQUE INDEX restore_worker_attrs_id ON attrs(id);
             CREATE TABLE idents (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               UNIQUE (app_id, etype, label)
             );
             CREATE TABLE triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL,
               value_md5 text NOT NULL,
               ea boolean NOT NULL,
               eav boolean NOT NULL,
               av boolean NOT NULL,
               ave boolean NOT NULL,
               vae boolean NOT NULL,
               created_at bigint NOT NULL,
               checked_data_type checked_data_type,
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5)
             );
             CREATE UNIQUE INDEX restore_worker_ea
               ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE UNIQUE INDEX restore_worker_av
               ON triples(app_id, attr_id, value) WHERE av;
             CREATE TABLE rules (
               app_id uuid PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
               code jsonb NOT NULL
             );",
        )
        .await
        .unwrap();

    let owner_id = Uuid::new_v4();
    let staging_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'owner@example.test'), ($2, $3)",
            &[&owner_id, &staging_id, &STAGING_EMAIL],
        )
        .await
        .unwrap();
    let token = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id)
             VALUES ($1, $2, $3)",
            &[
                &Uuid::new_v4(),
                &Sha256::digest(token.to_string().as_bytes()).to_vec(),
                &owner_id,
            ],
        )
        .await
        .unwrap();

    let source_app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'Source app')",
            &[&source_app_id, &owner_id],
        )
        .await
        .unwrap();
    let backup_id = Uuid::new_v4();
    let storage_prefix = format!("{source_app_id}/{}", Uuid::new_v4());
    client
        .execute(
            "INSERT INTO app_backups (id, app_id, backup_at, storage_prefix)
             VALUES ($1, $2, now(), $3)",
            &[&backup_id, &source_app_id, &storage_prefix],
        )
        .await
        .unwrap();

    let entity_id = Uuid::new_v4();
    let config = json!({
        "title": "Source app",
        "tripleCount": 2,
        "counts": {"items": 1},
        "schema": {
            "entities": {
                "items": {"attrs": {
                    "name": {
                        "valueType": "string",
                        "config": {"unique": false, "indexed": true},
                        "required": true
                    }
                }}
            },
            "links": {}
        },
        "rules": {}
    });
    let entity = json!({
        "entity": {"id": entity_id, "name": "restored"},
        "createdAt": 1_726_000_000_123_i64
    });
    let objects = Arc::new(MemoryObjects::default());
    objects.seed(
        format!("{storage_prefix}/config.json"),
        compressed(&serde_json::to_vec(&config).unwrap()),
    );
    objects.seed(
        format!("{storage_prefix}/entities/items.jsonl"),
        compressed(format!("{entity}\n").as_bytes()),
    );

    let pool = PostgresPool::connect(&scoped, 6).await.unwrap();
    let worker = Arc::new(PostgresRestoreWorker::from_pool(
        pool.clone(),
        Arc::clone(&objects),
        STAGING_EMAIL,
    ));
    let stage = Arc::new(PostgresBackupRestoreControlHooks::new(
        Arc::new(NoopDashboardControlHooks),
        Arc::clone(&objects),
    ));
    let service = DashboardControlService::from_pool(
        pool,
        Arc::new(PostgresRestoreControlHooks::new(stage, worker)),
    );

    let path = format!("/dash/apps/{source_app_id}/backups/{backup_id}/restore");
    let created = service
        .handle(request(
            Method::POST,
            &path,
            token,
            br#"{"title":"Restored app"}"#.to_vec(),
        ))
        .await
        .unwrap();
    let created = json_body(created);
    let target_app_id = Uuid::parse_str(created["app"]["id"].as_str().unwrap()).unwrap();
    let job_id = Uuid::parse_str(created["job"]["id"].as_str().unwrap()).unwrap();
    assert_eq!(created["app"]["title"], "Restored app");
    assert_ne!(target_app_id, source_app_id);

    let listed = service
        .handle(request(
            Method::GET,
            &format!("/dash/apps/{source_app_id}/restore-jobs"),
            token,
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(listed)["restore-jobs"][0]["id"], json!(job_id));

    let mut status = String::new();
    let mut error = None;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let row = client
            .query_one(
                "SELECT job_status, error FROM app_restore_jobs WHERE id = $1",
                &[&job_id],
            )
            .await
            .unwrap();
        status = row.get::<_, String>(0);
        error = row.get::<_, Option<String>>(1);
        if status == "completed" || status == "errored" {
            break;
        }
    }
    assert_eq!(status, "completed", "restore error: {error:?}");

    let triples = client
        .query_one(
            "SELECT count(*), max(value #>> '{}') FILTER (WHERE value #>> '{}' = 'restored')
             FROM triples WHERE app_id = $1 AND entity_id = $2",
            &[&target_app_id, &entity_id],
        )
        .await
        .unwrap();
    assert_eq!(triples.get::<_, i64>(0), 2);
    assert_eq!(
        triples.get::<_, Option<String>>(1).as_deref(),
        Some("restored")
    );

    let target_listed = service
        .handle(request(
            Method::GET,
            &format!("/dash/apps/{target_app_id}/restore-jobs"),
            token,
            Vec::new(),
        ))
        .await
        .unwrap();
    assert_eq!(
        json_body(target_listed)["restore-jobs"][0]["id"],
        json!(job_id)
    );

    drop(client);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
