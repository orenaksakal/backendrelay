use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::future::{Future, ready};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use chrono::Utc;
use instant_server::{
    PostgresPool, PostgresWebhookWorker, WebhookAttempt, WebhookDeliveryRequest,
    WebhookPayloadTokenIssuer, WebhookTransport,
};
use instant_wal::{Isn, Lsn};
use tokio::sync::{Mutex, Notify};
use tokio_postgres::{Client, NoTls};
use url::Url;
use uuid::Uuid;

async fn connect_client(database_url: &str) -> Client {
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
        "webhook_worker_test",
    );
    let url = Url::parse(&scoped).unwrap();
    let pairs = url.query_pairs().collect::<Vec<_>>();
    assert!(pairs.contains(&("sslmode".into(), "disable".into())));
    assert!(pairs.contains(&(
        "options".into(),
        "-cstatement_timeout=5s -csearch_path=webhook_worker_test,public".into(),
    )));
}

struct Issuer;

impl WebhookPayloadTokenIssuer for Issuer {
    type Error = Infallible;

    fn issue(&self, app_id: Uuid, webhook_id: Uuid, isn: Isn) -> Result<String, Self::Error> {
        Ok(format!("token:{app_id}:{webhook_id}:{isn}"))
    }
}

#[derive(Default)]
struct Transport {
    requests: StdMutex<Vec<WebhookDeliveryRequest>>,
    statuses: StdMutex<VecDeque<i32>>,
}

impl WebhookTransport for Transport {
    type Error = Infallible;

    fn deliver(
        &self,
        request: WebhookDeliveryRequest,
    ) -> impl Future<Output = Result<WebhookAttempt, Self::Error>> + Send {
        self.requests.lock().unwrap().push(request);
        let status = self.statuses.lock().unwrap().pop_front().unwrap_or(200);
        ready(Ok(WebhookAttempt {
            attempted_at: Utc::now(),
            duration_millis: 5,
            success: (200..300).contains(&status),
            status_code: Some(status),
            response_text: Some("response".to_owned()),
            error_type: None,
            error_message: None,
        }))
    }
}

#[derive(Default)]
struct BlockingTransport {
    requests: StdMutex<Vec<WebhookDeliveryRequest>>,
    started: Notify,
    release: Notify,
}

impl WebhookTransport for BlockingTransport {
    type Error = Infallible;

    async fn deliver(
        &self,
        request: WebhookDeliveryRequest,
    ) -> Result<WebhookAttempt, Self::Error> {
        self.requests.lock().unwrap().push(request);
        self.started.notify_one();
        self.release.notified().await;
        Ok(WebhookAttempt {
            attempted_at: Utc::now(),
            duration_millis: 5,
            success: true,
            status_code: Some(200),
            response_text: Some("response".to_owned()),
            error_type: None,
            error_message: None,
        })
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn webhook_worker_preserves_claim_delivery_retry_and_recovery_contracts() {
    let Some(url) = instant_test_support::test_database_url(
        "webhook_worker_preserves_claim_delivery_retry_and_recovery_contracts",
    ) else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TYPE pg_temp.isn AS (slot_num int, lsn pg_lsn);
             CREATE TYPE pg_temp.webhook_status AS ENUM ('active', 'disabled');
             CREATE TYPE pg_temp.webhook_event_status AS ENUM
               ('pending', 'processing', 'success', 'error', 'failed');
             CREATE TYPE pg_temp.webhook_attempt AS (
               attempt_at timestamptz, duration_ms int, success boolean,
               status_code int, response_text text, error_type text, error_message text
             );
             CREATE TEMP TABLE webhooks (
               id uuid PRIMARY KEY, app_id uuid NOT NULL,
               status webhook_status NOT NULL, sink jsonb NOT NULL,
               disabled_reason text
             );
             CREATE TEMP TABLE webhook_events (
               webhook_id uuid NOT NULL, isn isn NOT NULL, app_id uuid NOT NULL,
               status webhook_event_status NOT NULL, machine_id uuid,
               attempts webhook_attempt[], partition_bucket int NOT NULL,
               next_attempt_after timestamptz, created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (webhook_id, isn, partition_bucket)
             );",
        )
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let webhook_ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    let isns = [
        Isn::new(1, Lsn::from_u64(100)),
        Isn::new(1, Lsn::from_u64(101)),
        Isn::new(1, Lsn::from_u64(102)),
    ];
    for (index, webhook_id) in webhook_ids.iter().enumerate() {
        let status = if index == 2 { "disabled" } else { "active" };
        client
            .execute(
                "INSERT INTO webhooks (id, app_id, status, sink)
                 VALUES ($1, $2, $3::text::webhook_status,
                         jsonb_build_object('url', $4::text))",
                &[
                    webhook_id,
                    &app_id,
                    &status,
                    &format!("https://example.com/{index}"),
                ],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO webhook_events
                   (webhook_id, isn, app_id, status, partition_bucket, created_at)
                 VALUES ($1, $2::text::isn, $3, 'pending'::webhook_event_status,
                   ((extract(epoch from now())::bigint / 86400 / 30) % 13)::int,
                   now() - make_interval(mins => $4))",
                &[
                    webhook_id,
                    &isns[index].to_postgres_composite(),
                    &app_id,
                    &i32::try_from(10 - index).unwrap(),
                ],
            )
            .await
            .unwrap();
    }
    let client = Arc::new(Mutex::new(client));
    let transport = Arc::new(Transport::default());
    transport.statuses.lock().unwrap().extend([200, 410]);
    let worker = PostgresWebhookWorker::with_shared_client(
        Arc::clone(&client),
        Arc::clone(&transport),
        Arc::new(Issuer),
        "https://api.instant.test/",
        Uuid::new_v4(),
    );
    let outcome = worker.work_pending(10, 10).await.unwrap();
    assert_eq!(outcome.claimed, 3);
    assert_eq!(outcome.recorded, 3);
    assert!(!outcome.may_have_more);
    {
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["payloadUrl"],
            format!(
                "https://api.instant.test/webhooks/payload/{}/{}/{}",
                app_id, webhook_ids[0], isns[0]
            )
        );
        assert_eq!(
            body["token"],
            format!("token:{app_id}:{}:{}", webhook_ids[0], isns[0])
        );
    }
    let client_guard = client.lock().await;
    let statuses = client_guard
        .query(
            "SELECT status::text FROM webhook_events ORDER BY created_at",
            &[],
        )
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>();
    assert_eq!(statuses, vec!["success", "failed", "failed"]);
    let disabled: String = client_guard
        .query_one(
            "SELECT status::text FROM webhooks WHERE id = $1",
            &[&webhook_ids[1]],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(disabled, "disabled");
    client_guard
        .execute(
            "UPDATE webhook_events SET status = 'processing', machine_id = $2,
               updated_at = now() - interval '2 minutes' WHERE webhook_id = $1",
            &[&webhook_ids[0], &Uuid::new_v4()],
        )
        .await
        .unwrap();
    drop(client_guard);
    assert_eq!(worker.free_stuck_events().await.unwrap(), 1);
    let client_guard = client.lock().await;
    let released: String = client_guard
        .query_one(
            "SELECT status::text FROM webhook_events WHERE webhook_id = $1",
            &[&webhook_ids[0]],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(released, "pending");
    client_guard
        .execute(
            "UPDATE webhook_events
             SET status = 'error', next_attempt_after = now() - interval '1 second'
             WHERE webhook_id = $1",
            &[&webhook_ids[0]],
        )
        .await
        .unwrap();
    drop(client_guard);
    let retry = worker.work_retries(100, Utc::now()).await.unwrap();
    assert_eq!(retry.claimed, 1);
    assert_eq!(retry.recorded, 1);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pooled_worker_reconnects_between_delivery_and_atomic_attempt_recording() {
    let Some(database_url) = instant_test_support::test_database_url(
        "pooled_worker_reconnects_between_delivery_and_atomic_attempt_recording",
    ) else {
        return;
    };
    let setup = connect_client(&database_url).await;
    let schema = format!("webhook_worker_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TYPE {schema}.isn AS (slot_num int, lsn pg_lsn);
             CREATE TYPE {schema}.webhook_status AS ENUM ('active', 'disabled');
             CREATE TYPE {schema}.webhook_event_status AS ENUM
               ('pending', 'processing', 'success', 'error', 'failed');
             CREATE TYPE {schema}.webhook_attempt AS (
               attempt_at timestamptz, duration_ms int, success boolean,
               status_code int, response_text text, error_type text, error_message text
             );
             CREATE TABLE {schema}.webhooks (
               id uuid PRIMARY KEY, app_id uuid NOT NULL,
               status {schema}.webhook_status NOT NULL, sink jsonb NOT NULL,
               disabled_reason text
             );
             CREATE TABLE {schema}.webhook_events (
               webhook_id uuid NOT NULL, isn {schema}.isn NOT NULL, app_id uuid NOT NULL,
               status {schema}.webhook_event_status NOT NULL, machine_id uuid,
               attempts {schema}.webhook_attempt[], partition_bucket int NOT NULL,
               next_attempt_after timestamptz, created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (webhook_id, isn, partition_bucket)
             );"
        ))
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    let webhook_id = Uuid::new_v4();
    let isn = Isn::new(1, Lsn::from_u64(200));
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.webhooks (id, app_id, status, sink)
                 VALUES ($1, $2, 'active'::{schema}.webhook_status,
                         jsonb_build_object('url', 'https://example.com/reconnect'))"
            ),
            &[&webhook_id, &app_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.webhook_events
                   (webhook_id, isn, app_id, status, partition_bucket)
                 VALUES ($1, $2::text::{schema}.isn, $3,
                         'pending'::{schema}.webhook_event_status,
                         ((extract(epoch from now())::bigint / 86400 / 30) % 13)::int)"
            ),
            &[&webhook_id, &isn.to_postgres_composite(), &app_id],
        )
        .await
        .unwrap();

    let pool = PostgresPool::connect(&scoped_url(&database_url, &schema), 1)
        .await
        .unwrap();
    let transport = Arc::new(BlockingTransport::default());
    let worker = Arc::new(PostgresWebhookWorker::from_pool(
        pool.clone(),
        Arc::clone(&transport),
        Arc::new(Issuer),
        "https://api.instant.test/",
        Uuid::new_v4(),
    ));
    let running_worker = Arc::clone(&worker);
    let work = tokio::spawn(async move { running_worker.work_pending(1, 1).await });
    tokio::time::timeout(Duration::from_secs(10), transport.started.notified())
        .await
        .expect("webhook transport did not start");

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
    transport.release.notify_one();

    let outcome = tokio::time::timeout(Duration::from_secs(20), work)
        .await
        .expect("webhook worker did not reconnect within the bounded deadline")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.claimed, 1);
    assert_eq!(outcome.recorded, 1);
    assert!(outcome.may_have_more);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);

    let row = setup
        .query_one(
            &format!(
                "SELECT status::text, cardinality(attempts), machine_id
                 FROM {schema}.webhook_events WHERE webhook_id = $1"
            ),
            &[&webhook_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "success");
    assert_eq!(row.get::<_, Option<i32>>(1), Some(1));
    assert_eq!(row.get::<_, Option<Uuid>>(2), None);
    assert_eq!(worker.work_pending(1, 1).await.unwrap().claimed, 0);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);

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
