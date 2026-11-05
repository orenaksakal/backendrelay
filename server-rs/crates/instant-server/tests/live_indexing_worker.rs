use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use instant_server::{IndexingJobOutcome, PostgresIndexingWorker, PostgresPool};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls, types::Json};
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

async fn create_reconnect_fixture(
    setup: &Client,
    schema: &str,
    app_id: Uuid,
    attr_id: Uuid,
    job_id: Uuid,
) {
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL
             );
             CREATE TABLE {schema}.attrs (
               id uuid NOT NULL, app_id uuid NOT NULL,
               is_required boolean NOT NULL,
               PRIMARY KEY (app_id, id)
             );
             CREATE TABLE {schema}.indexing_jobs (
               id uuid PRIMARY KEY, app_id uuid NOT NULL, attr_id uuid NOT NULL,
               job_serial_key text NOT NULL, job_type text NOT NULL,
               job_status text NOT NULL, job_dependency uuid,
               job_stage text, checked_data_type public.checked_data_type,
               error text, worker_id text,
               created_at timestamptz NOT NULL DEFAULT now(), done_at timestamptz
             );"
        ))
        .await
        .unwrap();
    setup
        .execute(
            &format!("INSERT INTO {schema}.attrs (id, app_id, is_required) VALUES ($1, $2, true)"),
            &[&attr_id, &app_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.indexing_jobs
                   (id, app_id, attr_id, job_serial_key, job_type, job_status, job_stage)
                 VALUES ($1, $2, $3, 'required', 'remove-required', 'waiting', 'update-attr')"
            ),
            &[&job_id, &app_id, &attr_id],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn pooled_worker_replaces_a_terminated_backend_before_the_next_job() {
    let Some(database_url) = instant_test_support::test_database_url(
        "pooled_worker_replaces_a_terminated_backend_before_the_next_job",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("indexing_reconnect_{}", Uuid::new_v4().simple());
    let app_id = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    let job_id = Uuid::new_v4();
    create_reconnect_fixture(&setup, &schema, app_id, attr_id, job_id).await;
    let pool = PostgresPool::connect(&scoped_url(&database_url, &schema), 1)
        .await
        .unwrap();
    let worker = PostgresIndexingWorker::from_pool(pool.clone(), "reconnect-worker");
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

    let outcome = tokio::time::timeout(Duration::from_secs(20), worker.run_job(job_id))
        .await
        .expect("indexing worker did not reconnect within the bounded deadline")
        .unwrap();
    assert_eq!(outcome, IndexingJobOutcome::Completed);
    let state = setup
        .query_one(
            &format!(
                "SELECT a.is_required, j.job_status
                 FROM {schema}.attrs a JOIN {schema}.indexing_jobs j ON j.attr_id = a.id
                 WHERE a.app_id = $1 AND a.id = $2"
            ),
            &[&app_id, &attr_id],
        )
        .await
        .unwrap();
    assert!(!state.get::<_, bool>(0));
    assert_eq!(state.get::<_, String>(1), "completed");
    let replacement = pool.acquire().await.unwrap();
    let replacement_row = replacement
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(replacement_row.get::<_, i32>(0), doomed_pid);
    assert_eq!(replacement_row.get::<_, String>(1), schema);
    drop(replacement);
    drop(worker);
    drop(pool);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

async fn test_database() -> Option<Arc<Mutex<Client>>> {
    let url = instant_test_support::test_database_url("live_indexing_worker_test_database")?;
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "SET search_path TO pg_temp, public;
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now()
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
               reverse_ident uuid,
               etype text NOT NULL,
               label text NOT NULL,
               checked_data_type checked_data_type,
               checking_data_type boolean,
               indexing boolean,
               setting_unique boolean,
               deletion_marked_at timestamptz,
               PRIMARY KEY (app_id, id)
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
               checked_data_type checked_data_type,
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5),
               CONSTRAINT indexed_values_are_constrained
                 CHECK (NOT (eav OR av OR ave OR vae) OR pg_column_size(value) <= 1024)
             );
             CREATE UNIQUE INDEX indexing_worker_ea
               ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE UNIQUE INDEX indexing_worker_av
               ON triples(app_id, attr_id, value) WHERE av;
             CREATE TEMP TABLE indexing_jobs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               group_id uuid,
               job_serial_key text NOT NULL,
               job_type text NOT NULL,
               job_status text NOT NULL,
               job_dependency uuid REFERENCES indexing_jobs(id) ON DELETE CASCADE,
               job_stage text,
               checked_data_type checked_data_type,
               error text,
               work_estimate integer,
               work_completed integer,
               worker_id text,
               created_at timestamptz NOT NULL DEFAULT now(),
               done_at timestamptz,
               updated_at timestamptz NOT NULL DEFAULT now(),
               invalid_unique_value jsonb,
               invalid_entity_id uuid,
               error_detail text,
               error_data jsonb,
               context jsonb
             );
             CREATE UNIQUE INDEX indexing_worker_active
               ON indexing_jobs(app_id, attr_id, job_serial_key)
               WHERE job_status = 'processing';",
        )
        .await
        .unwrap();
    Some(Arc::new(Mutex::new(client)))
}

async fn insert_attr(client: &Client, app_id: Uuid, attr_id: Uuid, label: &str) {
    client
        .execute(
            "INSERT INTO attrs (
               id, app_id, value_type, cardinality, forward_ident, etype, label
             ) VALUES ($1, $2, 'blob', 'one', $3, 'items', $4)",
            &[&attr_id, &app_id, &Uuid::new_v4(), &label],
        )
        .await
        .unwrap();
}

async fn insert_triple(
    client: &Client,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
    value: Value,
    ea: bool,
) {
    client
        .execute(
            "INSERT INTO triples (
               app_id, entity_id, attr_id, value, value_md5, ea, eav, av, ave, vae
             ) VALUES ($1, $2, $3, $4, $5, $6, false, false, false, false)",
            &[
                &app_id,
                &entity_id,
                &attr_id,
                &Json(value),
                &Uuid::new_v4().to_string(),
                &ea,
            ],
        )
        .await
        .unwrap();
}

fn job_spec(job_type: &str) -> (&'static str, &'static str) {
    match job_type {
        "check-data-type" => ("data-type", "estimate-work"),
        "remove-data-type" => ("data-type", "update-attr-start"),
        "index" | "remove-index" => ("index", "update-attr-start"),
        "unique" | "remove-unique" => ("unique", "update-attr-start"),
        "required" => ("required", "estimate-work"),
        "remove-required" => ("required", "update-attr"),
        other => panic!("unknown job type {other}"),
    }
}

async fn insert_job(
    client: &Arc<Mutex<Client>>,
    app_id: Uuid,
    attr_id: Uuid,
    job_type: &str,
    dependency: Option<Uuid>,
    status: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    let (serial_key, stage) = job_spec(job_type);
    let checked_type = (job_type == "check-data-type").then_some("string");
    client
        .lock()
        .await
        .execute(
            "INSERT INTO indexing_jobs (
               id, app_id, attr_id, job_serial_key, job_type, job_status,
               job_dependency, job_stage, checked_data_type
             ) VALUES (
               $1, $2, $3, $4, $5, $6, $7, $8,
               CASE WHEN $9::text IS NULL THEN NULL ELSE $9::text::checked_data_type END
             )",
            &[
                &id,
                &app_id,
                &attr_id,
                &serial_key,
                &job_type,
                &status,
                &dependency,
                &stage,
                &checked_type,
            ],
        )
        .await
        .unwrap();
    id
}

async fn run_job_type(
    client: &Arc<Mutex<Client>>,
    worker: &PostgresIndexingWorker,
    app_id: Uuid,
    attr_id: Uuid,
    job_type: &str,
) -> Uuid {
    let id = insert_job(client, app_id, attr_id, job_type, None, "waiting").await;
    let outcome = worker.run_job(id).await.unwrap();
    let error_detail = client
        .lock()
        .await
        .query_one(
            "SELECT error_detail FROM indexing_jobs WHERE id = $1",
            &[&id],
        )
        .await
        .unwrap()
        .get::<_, Option<String>>(0);
    assert_eq!(
        outcome,
        IndexingJobOutcome::Completed,
        "{job_type} did not complete: {error_detail:?}"
    );
    id
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn worker_executes_all_eight_schema_constraint_operations() {
    let Some(client) = test_database().await else {
        return;
    };
    let worker = PostgresIndexingWorker::with_shared_client(Arc::clone(&client), "test-worker");
    let app_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let target_attr = Uuid::new_v4();
    let entities = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    {
        let database = client.lock().await;
        insert_attr(&database, app_id, id_attr, "id").await;
        insert_attr(&database, app_id, target_attr, "name").await;
        for entity in entities {
            insert_triple(&database, app_id, entity, id_attr, json!(entity), true).await;
        }
        insert_triple(
            &database,
            app_id,
            entities[0],
            target_attr,
            json!("one"),
            true,
        )
        .await;
        insert_triple(
            &database,
            app_id,
            entities[1],
            target_attr,
            json!("two"),
            true,
        )
        .await;
    }

    run_job_type(&client, &worker, app_id, target_attr, "check-data-type").await;
    {
        let database = client.lock().await;
        let row = database
            .query_one(
                "SELECT checked_data_type::text, checking_data_type FROM attrs
                 WHERE app_id = $1 AND id = $2",
                &[&app_id, &target_attr],
            )
            .await
            .unwrap();
        let attr = (
            row.get::<_, Option<String>>(0),
            row.get::<_, Option<bool>>(1),
        );
        assert_eq!(attr, (Some("string".to_owned()), Some(false)));
        let checked: i64 = database
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2
                   AND checked_data_type = 'string'",
                &[&app_id, &target_attr],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(checked, 2);
    }

    run_job_type(&client, &worker, app_id, target_attr, "remove-data-type").await;
    run_job_type(&client, &worker, app_id, target_attr, "index").await;
    {
        let database = client.lock().await;
        let row = database
            .query_one(
                "SELECT is_indexed, indexing FROM attrs WHERE app_id = $1 AND id = $2",
                &[&app_id, &target_attr],
            )
            .await
            .unwrap();
        let attr = (row.get::<_, bool>(0), row.get::<_, Option<bool>>(1));
        assert_eq!(attr, (true, Some(false)));
        let indexed: i64 = database
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2 AND ave",
                &[&app_id, &target_attr],
            )
            .await
            .unwrap()
            .get(0);
        let nulls: i64 = database
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2
                   AND value = 'null'::jsonb",
                &[&app_id, &target_attr],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!((indexed, nulls), (3, 1));
    }

    run_job_type(&client, &worker, app_id, target_attr, "remove-index").await;
    run_job_type(&client, &worker, app_id, target_attr, "unique").await;
    run_job_type(&client, &worker, app_id, target_attr, "remove-unique").await;
    {
        let database = client.lock().await;
        let row = database
            .query_one(
                "SELECT is_indexed, is_unique, indexing, setting_unique FROM attrs
                 WHERE app_id = $1 AND id = $2",
                &[&app_id, &target_attr],
            )
            .await
            .unwrap();
        let flags = (
            row.get::<_, bool>(0),
            row.get::<_, bool>(1),
            row.get::<_, Option<bool>>(2),
            row.get::<_, Option<bool>>(3),
        );
        assert_eq!(flags, (false, false, Some(false), Some(false)));
        database
            .execute(
                "DELETE FROM triples WHERE app_id = $1 AND attr_id = $2
                   AND entity_id = $3 AND value = 'null'::jsonb",
                &[&app_id, &target_attr, &entities[2]],
            )
            .await
            .unwrap();
        insert_triple(
            &database,
            app_id,
            entities[2],
            target_attr,
            json!("three"),
            true,
        )
        .await;
    }

    run_job_type(&client, &worker, app_id, target_attr, "required").await;
    run_job_type(&client, &worker, app_id, target_attr, "remove-required").await;
    let database = client.lock().await;
    let required: bool = database
        .query_one(
            "SELECT is_required FROM attrs WHERE app_id = $1 AND id = $2",
            &[&app_id, &target_attr],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!required);
    let completed: i64 = database
        .query_one(
            "SELECT count(*) FROM indexing_jobs WHERE app_id = $1 AND job_status = 'completed'",
            &[&app_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(completed, 8);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn worker_preserves_dependencies_cancellation_and_authoritative_failures() {
    let Some(client) = test_database().await else {
        return;
    };
    let worker = PostgresIndexingWorker::with_shared_client(Arc::clone(&client), "test-worker");
    let app_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let type_attr = Uuid::new_v4();
    let unique_attr = Uuid::new_v4();
    let required_attr = Uuid::new_v4();
    let large_attr = Uuid::new_v4();
    let dependency_attr = Uuid::new_v4();
    let entities = [Uuid::new_v4(), Uuid::new_v4()];
    {
        let database = client.lock().await;
        insert_attr(&database, app_id, id_attr, "id").await;
        insert_attr(&database, app_id, type_attr, "typed").await;
        insert_attr(&database, app_id, unique_attr, "unique-value").await;
        insert_attr(&database, app_id, required_attr, "required-value").await;
        insert_attr(&database, app_id, large_attr, "large-value").await;
        insert_attr(&database, app_id, dependency_attr, "dependency-value").await;
        for entity in entities {
            insert_triple(&database, app_id, entity, id_attr, json!(entity), true).await;
            insert_triple(
                &database,
                app_id,
                entity,
                unique_attr,
                json!("duplicate"),
                true,
            )
            .await;
            insert_triple(
                &database,
                app_id,
                entity,
                large_attr,
                json!(
                    (0..1_000)
                        .map(|_| Uuid::new_v4().to_string())
                        .collect::<String>()
                ),
                true,
            )
            .await;
        }
        insert_triple(&database, app_id, entities[0], type_attr, json!(42), true).await;
    }

    let type_job = insert_job(
        &client,
        app_id,
        type_attr,
        "check-data-type",
        None,
        "waiting",
    )
    .await;
    assert_eq!(
        worker.run_job(type_job).await.unwrap(),
        IndexingJobOutcome::Errored {
            error: "invalid-triple-error".to_owned()
        }
    );

    let unique_job = insert_job(&client, app_id, unique_attr, "unique", None, "waiting").await;
    assert_eq!(
        worker.run_job(unique_job).await.unwrap(),
        IndexingJobOutcome::Errored {
            error: "triple-not-unique-error".to_owned()
        }
    );

    let large_job = insert_job(&client, app_id, large_attr, "index", None, "waiting").await;
    assert_eq!(
        worker.run_job(large_job).await.unwrap(),
        IndexingJobOutcome::Errored {
            error: "triple-too-large-error".to_owned()
        }
    );

    let required_job =
        insert_job(&client, app_id, required_attr, "required", None, "waiting").await;
    assert_eq!(
        worker.run_job(required_job).await.unwrap(),
        IndexingJobOutcome::Errored {
            error: "missing-required-error".to_owned()
        }
    );

    let dependency = insert_job(
        &client,
        app_id,
        dependency_attr,
        "remove-required",
        None,
        "waiting",
    )
    .await;
    let dependent = insert_job(
        &client,
        app_id,
        dependency_attr,
        "remove-required",
        Some(dependency),
        "waiting",
    )
    .await;
    assert_eq!(
        worker.run_job(dependent).await.unwrap(),
        IndexingJobOutcome::NotClaimed
    );
    client
        .lock()
        .await
        .execute(
            "UPDATE indexing_jobs SET job_status = 'completed' WHERE id = $1",
            &[&dependency],
        )
        .await
        .unwrap();
    assert_eq!(
        worker.run_available_jobs(100).await.unwrap(),
        vec![(dependent, IndexingJobOutcome::Completed)]
    );

    let canceled = insert_job(
        &client,
        app_id,
        dependency_attr,
        "remove-required",
        None,
        "canceled",
    )
    .await;
    assert_eq!(
        worker.run_job(canceled).await.unwrap(),
        IndexingJobOutcome::Cancelled
    );

    let database = client.lock().await;
    let row = database
        .query_one(
            "SELECT a.is_unique, a.setting_unique,
                    (SELECT count(*) FROM triples t
                     WHERE t.app_id = a.app_id AND t.attr_id = a.id AND t.av)
             FROM attrs a WHERE a.app_id = $1 AND a.id = $2",
            &[&app_id, &unique_attr],
        )
        .await
        .unwrap();
    let unique_state = (
        row.get::<_, bool>(0),
        row.get::<_, Option<bool>>(1),
        row.get::<_, i64>(2),
    );
    assert_eq!(unique_state, (false, Some(false), 0));
    let large_state: (bool, Option<bool>, i64) = {
        let row = database
            .query_one(
                "SELECT a.is_indexed, a.indexing,
                        (SELECT count(*) FROM triples t
                         WHERE t.app_id = a.app_id AND t.attr_id = a.id AND t.ave)
                 FROM attrs a WHERE a.app_id = $1 AND a.id = $2",
                &[&app_id, &large_attr],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1), row.get(2))
    };
    assert_eq!(large_state, (false, Some(false), 0));
    let invalid_entity_id: Option<Uuid> = database
        .query_one(
            "SELECT invalid_entity_id FROM indexing_jobs WHERE id = $1",
            &[&large_job],
        )
        .await
        .unwrap()
        .get(0);
    assert!(invalid_entity_id.is_some_and(|entity_id| entities.contains(&entity_id)));
    let invalid_unique_value: Option<Value> = database
        .query_one(
            "SELECT invalid_unique_value FROM indexing_jobs WHERE id = $1",
            &[&unique_job],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(invalid_unique_value, Some(json!("duplicate")));
    let required_error_data: Option<Value> = database
        .query_one(
            "SELECT error_data FROM indexing_jobs WHERE id = $1",
            &[&required_job],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(required_error_data.unwrap()["count"], 2);
}

#[tokio::test]
async fn worker_reclaims_only_processing_jobs_past_the_lease() {
    let Some(client) = test_database().await else {
        return;
    };
    let worker = PostgresIndexingWorker::with_shared_client(Arc::clone(&client), "lease-worker");
    let app_id = Uuid::new_v4();
    let stuck_attr = Uuid::new_v4();
    let fresh_attr = Uuid::new_v4();
    {
        let database = client.lock().await;
        insert_attr(&database, app_id, stuck_attr, "stuck").await;
        insert_attr(&database, app_id, fresh_attr, "fresh").await;
    }
    let stuck = insert_job(
        &client,
        app_id,
        stuck_attr,
        "remove-required",
        None,
        "processing",
    )
    .await;
    let fresh = insert_job(
        &client,
        app_id,
        fresh_attr,
        "remove-required",
        None,
        "processing",
    )
    .await;
    {
        let database = client.lock().await;
        database
            .execute(
                "UPDATE indexing_jobs
                 SET worker_id = 'dead-worker', updated_at = now() - interval '11 minutes'
                 WHERE id = $1",
                &[&stuck],
            )
            .await
            .unwrap();
        database
            .execute(
                "UPDATE indexing_jobs SET worker_id = 'dead-worker' WHERE id = $1",
                &[&fresh],
            )
            .await
            .unwrap();
    }

    let outcomes = worker.run_available_jobs(100).await.unwrap();
    assert!(
        outcomes.contains(&(stuck, IndexingJobOutcome::Completed)),
        "{outcomes:?}"
    );
    assert!(!outcomes.iter().any(|(job_id, _)| *job_id == fresh));

    let database = client.lock().await;
    let fresh_row = database
        .query_one(
            "SELECT job_status, worker_id FROM indexing_jobs WHERE id = $1",
            &[&fresh],
        )
        .await
        .unwrap();
    assert_eq!(fresh_row.get::<_, String>(0), "processing");
    assert_eq!(
        fresh_row.get::<_, Option<String>>(1),
        Some("dead-worker".to_owned())
    );
}
