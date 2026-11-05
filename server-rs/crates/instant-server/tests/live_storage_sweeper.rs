use std::collections::HashSet;
use std::fmt::Write as _;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use instant_server::{
    PostgresPool, PostgresStorageSweeper, StorageObjectMetadata, StorageObjectStore,
    StoragePutRequest, object_key,
};
use tokio_postgres::NoTls;
use url::Url;
use uuid::Uuid;

fn scoped_url(url: &str, schema: &str) -> String {
    let mut url = Url::parse(url).expect("TEST_DATABASE_URL must be a PostgreSQL URL");
    let mut pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let mut options = pairs
        .iter()
        .filter(|(key, _)| key == "options")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if !options.is_empty() {
        options.push(' ');
    }
    write!(options, "-csearch_path={schema},public").expect("writing to a String cannot fail");
    pairs.retain(|(key, _)| key != "options");
    url.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("options", &options);
    url.into()
}

#[derive(Default)]
struct RecordingObjects {
    deleted: Mutex<HashSet<String>>,
    batches: Mutex<Vec<Vec<String>>>,
}

impl StorageObjectStore for RecordingObjects {
    fn put(
        &self,
        _request: StoragePutRequest,
        _body: Body,
    ) -> impl Future<Output = Result<StorageObjectMetadata, String>> + Send {
        std::future::ready(Err("unused".to_owned()))
    }

    fn delete(&self, key: String) -> impl Future<Output = Result<(), String>> + Send {
        self.deleted.lock().unwrap().insert(key);
        std::future::ready(Ok(()))
    }

    fn delete_many(&self, keys: Vec<String>) -> impl Future<Output = Result<(), String>> + Send {
        self.deleted.lock().unwrap().extend(keys.iter().cloned());
        self.batches.lock().unwrap().push(keys);
        std::future::ready(Ok(()))
    }

    fn signed_download_url(
        &self,
        _key: String,
    ) -> impl Future<Output = Result<String, String>> + Send {
        std::future::ready(Err("unused".to_owned()))
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn sweeper_honors_grace_scope_abandoned_claims_and_loop_bounds() {
    let Some(url) = instant_test_support::test_database_url(
        "sweeper_honors_grace_scope_abandoned_claims_and_loop_bounds",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("storage_sweeper_{}", Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.app_files_to_sweep (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, location_id text NOT NULL,
               created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL,
               process_id text
             );"
        ))
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let other_app = Uuid::new_v4();
    let ids = [
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    ];
    client
        .execute(
            &format!(
                "INSERT INTO {schema}.app_files_to_sweep
               (id, app_id, location_id, created_at, updated_at, process_id)
             VALUES
               ($1, $5, 'old', now() - interval '33 days', now(), NULL),
               ($2, $5, 'stale', now() - interval '33 days', now() - interval '6 minutes', 'dead'),
               ($3, $5, 'young', now() - interval '31 days', now(), NULL),
               ($4, $6, 'other', now() - interval '33 days', now(), NULL)"
            ),
            &[&ids[0], &ids[1], &ids[2], &ids[3], &app_id, &other_app],
        )
        .await
        .unwrap();
    let objects = Arc::new(RecordingObjects::default());
    let pool = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let sweeper =
        PostgresStorageSweeper::from_pool(pool.clone(), Arc::clone(&objects), "sweeper-test");
    let outcome = sweeper.sweep(Some(app_id), 10, 1_000).await.unwrap();
    assert_eq!(outcome.deleted, 2);
    assert_eq!(outcome.loops, 1);
    assert!(!outcome.reached_loop_limit);
    assert_eq!(
        *objects.deleted.lock().unwrap(),
        HashSet::from([object_key(app_id, "old"), object_key(app_id, "stale")])
    );
    {
        let batches = objects.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(
            batches[0].iter().cloned().collect::<HashSet<_>>(),
            HashSet::from([object_key(app_id, "old"), object_key(app_id, "stale")])
        );
    }

    let doomed = pool.acquire().await.unwrap();
    let doomed_pid = doomed
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    assert!(
        client
            .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(doomed);
    let resumed_id = Uuid::new_v4();
    client
        .execute(
            &format!(
                "INSERT INTO {schema}.app_files_to_sweep
                   (id, app_id, location_id, created_at, updated_at, process_id)
                 VALUES ($1, $2, 'after-reconnect', now() - interval '33 days', now(), NULL)"
            ),
            &[&resumed_id, &app_id],
        )
        .await
        .unwrap();
    let resumed = tokio::time::timeout(
        Duration::from_secs(20),
        sweeper.process_batch(Some(app_id), 1_000),
    )
    .await
    .expect("storage sweeper did not reconnect within the bounded deadline")
    .unwrap();
    assert_eq!(resumed, 1);
    assert!(
        objects
            .deleted
            .lock()
            .unwrap()
            .contains(&object_key(app_id, "after-reconnect"))
    );
    let replacement = pool.acquire().await.unwrap();
    let replacement_row = replacement
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(replacement_row.get::<_, i32>(0), doomed_pid);
    assert_eq!(replacement_row.get::<_, String>(1), schema);
    drop(replacement);
    drop(sweeper);
    drop(pool);
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
