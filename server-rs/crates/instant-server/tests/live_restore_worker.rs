use std::collections::HashMap;
use std::future::Future;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use axum::body::{Body, to_bytes};
use instant_server::{
    PostgresPool, PostgresRestoreWorker, RestoreJobOutcome, StorageObjectMetadata,
    StorageObjectStore, StoragePutRequest,
};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_postgres::NoTls;
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

#[derive(Default)]
struct MemoryObjects(StdMutex<HashMap<String, Vec<u8>>>);

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

fn scoped_url(url: &str, schema: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}%2Cpublic")
}

#[tokio::test]
async fn pooled_worker_replaces_a_terminated_backend_before_the_next_claim() {
    let Some(url) = instant_test_support::test_database_url(
        "pooled_worker_replaces_a_terminated_backend_before_the_next_claim",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("restore_reconnect_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.app_restore_jobs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, creator_id uuid, org_id uuid,
               title text, zip_path text NOT NULL, job_status text NOT NULL,
               progress text, error text, created_at timestamptz NOT NULL DEFAULT now(),
               done_at timestamptz, updated_at timestamptz NOT NULL DEFAULT now()
             )"
        ))
        .await
        .unwrap();
    let pool = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let doomed = pool.acquire().await.unwrap();
    let doomed_pid = doomed
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    assert!(
        setup
            .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(doomed);
    let worker = PostgresRestoreWorker::from_pool(
        pool.clone(),
        Arc::new(MemoryObjects::default()),
        "staging@example.test",
    );
    assert_eq!(
        worker.run_job(Uuid::new_v4()).await.unwrap(),
        RestoreJobOutcome::NotClaimed
    );
    let replacement = pool.acquire().await.unwrap();
    let row = replacement
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(row.get::<_, i32>(0), doomed_pid);
    assert_eq!(row.get::<_, String>(1), schema);
    drop(replacement);
    drop(worker);
    drop(pool);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

fn write_backup(path: &Path, entity_id: Uuid) {
    let file = std::fs::File::create(path).unwrap();
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    archive.start_file("config.json", options).unwrap();
    let config = json!({
        "title": "Worker source",
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
        "rules": {},
        "emailTemplates": [{
            "type": "magic-code",
            "body": "Your code is {{code}}",
            "subject": "Sign in",
            "name": "Restored template"
        }],
        "authorizedRedirectOrigins": [{
            "service": "generic",
            "params": ["app.example.test"]
        }]
    });
    archive
        .write_all(&serde_json::to_vec(&config).unwrap())
        .unwrap();
    archive.start_file("entities/items.jsonl", options).unwrap();
    writeln!(
        archive,
        "{}",
        json!({
            "entity": {"id": entity_id, "name": "restored"},
            "createdAt": 1_726_000_000_123_i64
        })
    )
    .unwrap();
    archive.finish().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn worker_restores_schema_rules_metadata_and_triples_then_transfers_owner() {
    let Some(url) = instant_test_support::test_database_url(
        "worker_restores_schema_rules_metadata_and_triples_then_transfers_owner",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TYPE pg_temp.attr_on_delete AS ENUM ('cascade');
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE instant_users (
               id uuid PRIMARY KEY,
               email text NOT NULL UNIQUE
             );
             CREATE TEMP TABLE orgs (id uuid PRIMARY KEY);
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               creator_id uuid,
               org_id uuid,
               title text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(),
               status text NOT NULL DEFAULT 'active',
               deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE app_admin_tokens (
               token uuid PRIMARY KEY,
               app_id uuid NOT NULL UNIQUE REFERENCES apps(id) ON DELETE CASCADE
             );
             CREATE TEMP TABLE attrs (
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
             CREATE TEMP TABLE idents (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               UNIQUE (app_id, etype, label)
             );
             CREATE TEMP TABLE triples (
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
             CREATE TEMP TABLE rules (
               app_id uuid PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE,
               code jsonb NOT NULL
             );
             CREATE TEMP TABLE app_email_templates (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               email_type text NOT NULL,
               sender_id uuid,
               body text NOT NULL,
               subject text NOT NULL,
               name text,
               UNIQUE (app_id, email_type)
             );
             CREATE TEMP TABLE app_authorized_redirect_origins (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
               service text NOT NULL,
               params text[] NOT NULL,
               UNIQUE (app_id, service, params)
             );
             CREATE TEMP TABLE app_restore_jobs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               creator_id uuid,
               org_id uuid,
               title text,
               zip_path text NOT NULL,
               job_status text NOT NULL,
               progress text,
               error text,
               created_at timestamptz NOT NULL DEFAULT now(),
               done_at timestamptz,
               updated_at timestamptz NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();

    let owner_id = Uuid::new_v4();
    let staging_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES
               ($1, 'owner@example.test'), ($2, 'ephemeral-apps-dev@backendrelay.invalid')",
            &[&owner_id, &staging_id],
        )
        .await
        .unwrap();
    let job_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let entity_id = Uuid::new_v4();
    let zip_path = std::env::temp_dir().join(format!("instant-restore-live-{job_id}.zip"));
    write_backup(&zip_path, entity_id);
    client
        .execute(
            "INSERT INTO app_restore_jobs
               (id, app_id, creator_id, title, zip_path, job_status)
             VALUES ($1, $2, $3, 'Chosen title', $4, 'waiting')",
            &[
                &job_id,
                &app_id,
                &owner_id,
                &zip_path.to_string_lossy().as_ref(),
            ],
        )
        .await
        .unwrap();

    let shared = Arc::new(Mutex::new(client));
    let objects = Arc::new(MemoryObjects::default());
    let worker = PostgresRestoreWorker::with_shared_client(
        Arc::clone(&shared),
        Arc::clone(&objects),
        "ephemeral-apps-dev@backendrelay.invalid",
    );
    // The backup adds schema while the worker-owned staging app is disabled.
    assert_eq!(
        worker.run_available_jobs(10).await.unwrap(),
        vec![(
            job_id,
            RestoreJobOutcome::Completed {
                app_id,
                triples: 2,
                files: 0
            }
        )]
    );
    assert!(!zip_path.exists());

    let client = shared.lock().await;
    let job = client
        .query_one(
            "SELECT job_status, error, done_at IS NOT NULL FROM app_restore_jobs WHERE id = $1",
            &[&job_id],
        )
        .await
        .unwrap();
    assert_eq!(job.get::<_, String>(0), "completed");
    assert_eq!(job.get::<_, Option<String>>(1), None);
    assert!(job.get::<_, bool>(2));
    let app = client
        .query_one(
            "SELECT creator_id, org_id, title, status FROM apps WHERE id = $1",
            &[&app_id],
        )
        .await
        .unwrap();
    assert_eq!(app.get::<_, Option<Uuid>>(0), Some(owner_id));
    assert_eq!(app.get::<_, Option<Uuid>>(1), None);
    assert_eq!(app.get::<_, String>(2), "Chosen title");
    assert_eq!(app.get::<_, String>(3), "active");
    let triples = client
        .query_one(
            "SELECT count(*), min(t.created_at),
                    max(t.value #>> '{}') FILTER (WHERE a.label = 'name')
             FROM triples t JOIN attrs a ON a.id = t.attr_id
             WHERE t.app_id = $1 AND t.entity_id = $2",
            &[&app_id, &entity_id],
        )
        .await
        .unwrap();
    assert_eq!(triples.get::<_, i64>(0), 2);
    assert_eq!(triples.get::<_, Option<i64>>(1), Some(1_726_000_000_123));
    assert_eq!(
        triples.get::<_, Option<String>>(2).as_deref(),
        Some("restored")
    );
    assert_eq!(
        client
            .query_one("SELECT code FROM rules WHERE app_id = $1", &[&app_id])
            .await
            .unwrap()
            .get::<_, serde_json::Value>(0),
        json!({})
    );
    assert!(
        client
            .query_one(
                "SELECT sender_id IS NULL FROM app_email_templates WHERE app_id = $1",
                &[&app_id]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert_eq!(
        client
            .query_one(
                "SELECT params FROM app_authorized_redirect_origins WHERE app_id = $1",
                &[&app_id]
            )
            .await
            .unwrap()
            .get::<_, Vec<String>>(0),
        vec!["app.example.test"]
    );
}
