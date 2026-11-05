use instant_server::{PostgresPool, PostgresTriplesSizeCollector};
use std::fmt::Write as _;
use std::time::Duration;
use tokio_postgres::{Client, NoTls};
use url::Url;
use uuid::Uuid;

async fn client(database_url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

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

#[test]
fn scoped_url_preserves_existing_query_and_session_options() {
    let scoped = scoped_url(
        "postgresql://localhost/instant?sslmode=disable&options=-cstatement_timeout%3D5s",
        "triples_size_test",
    );
    let url = Url::parse(&scoped).unwrap();
    let pairs = url.query_pairs().collect::<Vec<_>>();
    assert!(pairs.contains(&("sslmode".into(), "disable".into())));
    assert!(pairs.contains(&(
        "options".into(),
        "-cstatement_timeout=5s -csearch_path=triples_size_test,public".into(),
    )));
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps one temporary-schema lifecycle atomic and auditable.
async fn collector_claims_aggregates_filters_and_tracks_backlog_runs() {
    let Some(database_url) = instant_test_support::test_database_url(
        "collector_claims_aggregates_filters_and_tracks_backlog_runs",
    ) else {
        return;
    };
    let setup = client(&database_url).await;
    let schema = format!("triples_size_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.apps (id uuid PRIMARY KEY);
             CREATE TABLE {schema}.attrs (id uuid PRIMARY KEY);
             CREATE TABLE {schema}.triples_size_updates (
               id bigserial PRIMARY KEY,
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               pg_size bigint NOT NULL,
               files_size bigint
             );
             CREATE TABLE {schema}.triples_size_aggregate (
               app_id uuid NOT NULL REFERENCES {schema}.apps(id) ON DELETE CASCADE,
               attr_id uuid NOT NULL REFERENCES {schema}.attrs(id) ON DELETE CASCADE,
               pg_size bigint NOT NULL,
               files_size bigint,
               CONSTRAINT triples_size_aggregate_pkey PRIMARY KEY (app_id, attr_id)
             );"
        ))
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let attr_id = Uuid::new_v4();
    let deleted_app_id = Uuid::new_v4();
    setup
        .execute(
            &format!("INSERT INTO {schema}.apps VALUES ($1)"),
            &[&app_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!("INSERT INTO {schema}.attrs VALUES ($1)"),
            &[&attr_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.triples_size_updates
                   (app_id, attr_id, pg_size, files_size) VALUES
                   ($1, $2, 10, NULL),
                   ($1, $2, 20, 5),
                   ($1, $2, -3, NULL),
                   ($3, $2, 99, 99)"
            ),
            &[&app_id, &attr_id, &deleted_app_id],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&scoped_url(&database_url, &schema), 1)
        .await
        .unwrap();
    let collector = PostgresTriplesSizeCollector::from_pool(pool.clone());

    let first = collector.collect_batches(1, 2).await.unwrap();
    assert_eq!(first.loops, 1);
    assert_eq!(first.total_collected, 2);
    assert_eq!(first.total_aggregated, 1);
    assert!(first.reached_loop_limit);
    assert_eq!(first.consecutive_limit_hits, 1);

    let second = collector.collect_batches(1, 2).await.unwrap();
    assert_eq!(second.loops, 1);
    assert_eq!(second.total_collected, 2);
    assert_eq!(second.total_aggregated, 1);
    assert!(second.reached_loop_limit);
    assert_eq!(second.consecutive_limit_hits, 2);
    let aggregate = setup
        .query_one(
            &format!(
                "SELECT pg_size, files_size FROM {schema}.triples_size_aggregate
                 WHERE app_id = $1 AND attr_id = $2"
            ),
            &[&app_id, &attr_id],
        )
        .await
        .unwrap();
    assert_eq!(aggregate.get::<_, i64>(0), 27);
    assert_eq!(aggregate.get::<_, Option<i64>>(1), Some(5));
    assert_eq!(
        setup
            .query_one(
                &format!("SELECT count(*) FROM {schema}.triples_size_updates"),
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let drained = collector.collect_batches(1, 2).await.unwrap();
    assert_eq!(drained.loops, 0);
    assert_eq!(drained.total_collected, 0);
    assert_eq!(drained.total_aggregated, 0);
    assert!(!drained.reached_loop_limit);
    assert_eq!(drained.consecutive_limit_hits, 0);

    let doomed = pool.acquire().await.unwrap();
    let doomed_pid = doomed
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    let terminated = setup
        .query_one("SELECT pg_terminate_backend($1)", &[&doomed_pid])
        .await
        .unwrap()
        .get::<_, bool>(0);
    assert!(
        terminated,
        "the pooled backend must be terminated for this test"
    );
    drop(doomed);

    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.triples_size_updates
                   (app_id, attr_id, pg_size, files_size) VALUES ($1, $2, 1, 4)"
            ),
            &[&app_id, &attr_id],
        )
        .await
        .unwrap();
    let resumed = tokio::time::timeout(Duration::from_secs(20), collector.collect_batches(1, 2))
        .await
        .expect("collector did not reconnect within the bounded test deadline")
        .unwrap();
    assert_eq!(resumed.loops, 0);
    assert_eq!(resumed.total_collected, 1);
    assert_eq!(resumed.total_aggregated, 1);
    assert!(!resumed.reached_loop_limit);
    assert_eq!(resumed.consecutive_limit_hits, 0);
    let replacement = pool.acquire().await.unwrap();
    let replacement_row = replacement
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(replacement_row.get::<_, i32>(0), doomed_pid);
    assert_eq!(replacement_row.get::<_, String>(1), schema);
    drop(replacement);
    let aggregate = setup
        .query_one(
            &format!(
                "SELECT pg_size, files_size FROM {schema}.triples_size_aggregate
                 WHERE app_id = $1 AND attr_id = $2"
            ),
            &[&app_id, &attr_id],
        )
        .await
        .unwrap();
    assert_eq!(aggregate.get::<_, i64>(0), 28);
    assert_eq!(aggregate.get::<_, Option<i64>>(1), Some(9));

    drop(collector);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
