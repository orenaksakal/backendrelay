use std::{
    panic::{AssertUnwindSafe, resume_unwind},
    sync::Mutex,
    time::{Duration, Instant},
};

use futures_util::FutureExt as _;
use instant_realtime::{BroadcastData, BroadcastPayload, RoomKey};
use instant_server::{
    PostgresPool, PostgresRealtimeCluster, RealtimeClusterConsumerKind, RuntimeRealtimeCluster,
    RuntimeRealtimeClusterEvent,
};
use serde_json::json;
use tokio::{sync::broadcast, task::JoinHandle};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

const TRIALS: usize = 3;
const IDLE_PERIOD: Duration = Duration::from_secs(2);
const BURST_EVENTS: usize = 100;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);
const BURST_TIMEOUT: Duration = Duration::from_secs(15);

// These are reporting criteria, deliberately not timing assertions: shared live
// PostgreSQL hosts are not stable enough for timing to be a CI gate.
const MIN_LISTENER_THROUGHPUT_FRACTION: f64 = 0.80;

#[derive(Debug)]
struct Trial {
    idle_ms: f64,
    first_event_ms: f64,
    burst_ms: f64,
    consumed: usize,
}

struct Fixture {
    schema: String,
    setup: Client,
    setup_task: JoinHandle<()>,
    connection_tasks: Mutex<Vec<JoinHandle<()>>>,
}

async fn connect(database_url: &str, schema: &str) -> (Client, JoinHandle<()>) {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    (client, task)
}

impl Fixture {
    async fn create(database_url: &str) -> Self {
        let (setup, setup_task) = connect(database_url, "public").await;
        let schema = format!("realtime_resource_profile_{}", Uuid::new_v4().simple());
        setup
            .batch_execute(&format!(
                "CREATE SCHEMA {schema};
                 CREATE TABLE {schema}.attrs (id uuid PRIMARY KEY, app_id uuid NOT NULL, etype text NOT NULL, label text NOT NULL);
                 CREATE TABLE {schema}.triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL);
                 CREATE TABLE {schema}.config (
                   k text PRIMARY KEY NOT NULL, v json NOT NULL,
                   created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now());
                 CREATE TABLE {schema}.rust_realtime_room_events (
                   id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, source_machine_id uuid NOT NULL,
                   kind text NOT NULL, app_id uuid NOT NULL, room_id text NOT NULL, payload jsonb,
                   created_at timestamptz NOT NULL DEFAULT now());
                 CREATE TABLE {schema}.rust_realtime_machines (
                   machine_id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(),
                   updated_at timestamptz NOT NULL DEFAULT now());
                 CREATE TABLE {schema}.rust_realtime_event_consumers (
                   machine_id uuid NOT NULL REFERENCES {schema}.rust_realtime_machines (machine_id) ON DELETE CASCADE,
                   consumer_kind text NOT NULL, event_cursor bigint NOT NULL,
                   created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
                   PRIMARY KEY (machine_id, consumer_kind));"
            ))
            .await
            .unwrap();
        Self {
            schema,
            setup,
            setup_task,
            connection_tasks: Mutex::new(Vec::new()),
        }
    }

    async fn client(&self, database_url: &str) -> Client {
        let (client, task) = connect(database_url, &self.schema).await;
        self.connection_tasks.lock().unwrap().push(task);
        client
    }

    async fn cleanup(self) {
        for task in self
            .connection_tasks
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())
        {
            task.await.unwrap();
        }
        self.setup
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await
            .unwrap();
        drop(self.setup);
        self.setup_task.await.unwrap();
    }
}

fn scoped_url(database_url: &str, schema: &str) -> String {
    let separator = if database_url.contains('?') { '&' } else { '?' };
    format!("{database_url}{separator}options=-csearch_path%3D{schema}")
}

fn payload(room: &RoomKey, text: String) -> BroadcastPayload {
    BroadcastPayload {
        room_id: room.room_id.clone(),
        topic: json!("resource-profile"),
        data: BroadcastData {
            peer_id: Uuid::new_v4(),
            user: None,
            data: json!({"text": text}),
        },
    }
}

async fn receive_and_ack(
    cluster: &PostgresRealtimeCluster,
    receiver: &mut broadcast::Receiver<instant_server::RuntimeRealtimeClusterDelivery>,
    expected: &str,
) {
    tokio::time::timeout(DELIVERY_TIMEOUT, async {
        loop {
            let delivery = receiver.recv().await.unwrap();
            cluster
                .acknowledge(RealtimeClusterConsumerKind::Runtime, delivery.event_id)
                .await
                .unwrap();
            if matches!(delivery.event, RuntimeRealtimeClusterEvent::Broadcast { payload, .. }
                if payload.data.data["text"].as_str() == Some(expected))
            {
                return;
            }
        }
    })
    .await
    .expect("expected broadcast should arrive");
}

async fn run_trial(
    cluster: &PostgresRealtimeCluster,
    publisher: &PostgresRealtimeCluster,
) -> Trial {
    let room = RoomKey::new(Uuid::new_v4(), format!("profile-{}", Uuid::new_v4()));
    let mut receiver = cluster
        .subscribe(RealtimeClusterConsumerKind::Runtime)
        .await
        .unwrap();

    // Ensure the subscriber and, in listener mode, its LISTEN connection have
    // processed a durable event before measuring its empty-idle behavior.
    let warmup = format!("warmup-{}", Uuid::new_v4());
    publisher
        .publish_broadcast(&room, payload(&room, warmup.clone()))
        .await
        .unwrap();
    receive_and_ack(cluster, &mut receiver, &warmup).await;

    let idle_started = Instant::now();
    tokio::time::sleep(IDLE_PERIOD).await;
    let idle_ms = idle_started.elapsed().as_secs_f64() * 1_000.0;

    let first = format!("first-{}", Uuid::new_v4());
    let first_started = Instant::now();
    publisher
        .publish_broadcast(&room, payload(&room, first.clone()))
        .await
        .unwrap();
    receive_and_ack(cluster, &mut receiver, &first).await;
    let first_event_ms = first_started.elapsed().as_secs_f64() * 1_000.0;

    let burst_started = Instant::now();
    let expected = (0..BURST_EVENTS)
        .map(|index| format!("burst-{index}-{}", Uuid::new_v4()))
        .collect::<Vec<_>>();
    for text in &expected {
        publisher
            .publish_broadcast(&room, payload(&room, text.clone()))
            .await
            .unwrap();
    }
    tokio::time::timeout(BURST_TIMEOUT, async {
        for text in &expected {
            receive_and_ack(cluster, &mut receiver, text).await;
        }
    })
    .await
    .expect("all burst broadcasts should arrive and be acknowledged");

    Trial {
        idle_ms,
        first_event_ms,
        burst_ms: burst_started.elapsed().as_secs_f64() * 1_000.0,
        consumed: expected.len(),
    }
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let index = ((values.len() - 1) as f64 * percentile).round() as usize;
    values[index]
}

fn median(values: &[f64]) -> f64 {
    percentile(values, 0.5)
}

fn print_mode(mode: &str, trials: &[Trial]) {
    let idle = trials.iter().map(|trial| trial.idle_ms).collect::<Vec<_>>();
    let latency = trials
        .iter()
        .map(|trial| trial.first_event_ms)
        .collect::<Vec<_>>();
    let throughput = trials
        .iter()
        .map(|trial| trial.consumed as f64 / (trial.burst_ms / 1_000.0))
        .collect::<Vec<_>>();
    println!(
        "REALTIME_RESOURCE_PROFILE mode={mode} trials={} idle_ms={idle:?} first_event_ms={latency:?} burst_events={} consumed={:?} throughput_events_per_second={throughput:?} median_first_event_ms={:.2} p95_first_event_ms={:.2} median_throughput_events_per_second={:.2} p95_throughput_events_per_second={:.2}",
        trials.len(),
        BURST_EVENTS,
        trials
            .iter()
            .map(|trial| trial.consumed)
            .collect::<Vec<_>>(),
        median(&latency),
        percentile(&latency, 0.95),
        median(&throughput),
        percentile(&throughput, 0.95),
    );
}

#[allow(clippy::too_many_lines)]
async fn profile_realtime_resource_usage(url: &str, fixture: &Fixture) {
    let scoped_url = scoped_url(url, &fixture.schema);
    let publisher = PostgresRealtimeCluster::new(
        Uuid::new_v4(),
        fixture.client(url).await,
        fixture.client(url).await,
    )
    .await
    .unwrap();

    let mut polling_trials = Vec::with_capacity(TRIALS);
    for _ in 0..TRIALS {
        let cluster = PostgresRealtimeCluster::from_pools(
            Uuid::new_v4(),
            PostgresPool::connect(&scoped_url, 1).await.unwrap(),
            PostgresPool::connect(&scoped_url, 1).await.unwrap(),
        )
        .await
        .unwrap();
        polling_trials.push(run_trial(&cluster, &publisher).await);
    }

    let mut listener_trials = Vec::with_capacity(TRIALS);
    for _ in 0..TRIALS {
        let cluster = PostgresRealtimeCluster::from_pools_with_listener(
            Uuid::new_v4(),
            PostgresPool::connect(&scoped_url, 1).await.unwrap(),
            PostgresPool::connect(&scoped_url, 1).await.unwrap(),
            &scoped_url,
        )
        .await
        .unwrap();
        listener_trials.push(run_trial(&cluster, &publisher).await);
    }

    print_mode("from_pools_25ms_poll", &polling_trials);
    print_mode("from_pools_with_listener", &listener_trials);
    let polling_latency = median(
        &polling_trials
            .iter()
            .map(|trial| trial.first_event_ms)
            .collect::<Vec<_>>(),
    );
    let listener_latency = median(
        &listener_trials
            .iter()
            .map(|trial| trial.first_event_ms)
            .collect::<Vec<_>>(),
    );
    let polling_throughput = median(
        &polling_trials
            .iter()
            .map(|trial| trial.consumed as f64 / (trial.burst_ms / 1_000.0))
            .collect::<Vec<_>>(),
    );
    let listener_throughput = median(
        &listener_trials
            .iter()
            .map(|trial| trial.consumed as f64 / (trial.burst_ms / 1_000.0))
            .collect::<Vec<_>>(),
    );
    println!(
        "REALTIME_RESOURCE_PROFILE advisory_thresholds listener_median_latency_lt_poll_median={} listener_median_throughput_at_least_{:.0}_percent_of_poll={} pg_poll_query_count=not_collected",
        listener_latency < polling_latency,
        MIN_LISTENER_THROUGHPUT_FRACTION * 100.0,
        listener_throughput >= polling_throughput * MIN_LISTENER_THROUGHPUT_FRACTION,
    );

    // Correctness is asserted through bounded delivery and acknowledgement above;
    // latency and throughput remain observational rather than flaky CI gates.
    assert!(
        polling_trials
            .iter()
            .chain(&listener_trials)
            .all(|trial| trial.consumed == BURST_EVENTS)
    );
}

#[tokio::test]
#[ignore = "manual live resource profile; run with TEST_DATABASE_URL and --ignored --nocapture"]
async fn compares_polling_and_listener_realtime_resource_profiles() {
    let Some(url) = instant_test_support::test_database_url(
        "compares_polling_and_listener_realtime_resource_profiles",
    ) else {
        return;
    };
    let fixture = Fixture::create(&url).await;
    let result = AssertUnwindSafe(profile_realtime_resource_usage(&url, &fixture))
        .catch_unwind()
        .await;
    fixture.cleanup().await;
    if let Err(payload) = result {
        resume_unwind(payload);
    }
}
