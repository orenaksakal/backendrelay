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
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls, types::Json};
use uuid::Uuid;

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
        let schema = format!("realtime_notifications_{}", Uuid::new_v4().simple());
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

async fn listener_pid(client: &Client, application_name: &str, previous_pid: Option<i32>) -> i32 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let pid = client
                .query_opt(
                    "SELECT pid FROM pg_stat_activity
                     WHERE application_name = $1 AND pid <> coalesce($2, -1)",
                    &[&application_name, &previous_pid],
                )
                .await
                .unwrap()
                .map(|row| row.get::<_, i32>(0));
            if let Some(pid) = pid {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dedicated listener should connect")
}

async fn next_broadcast(
    cluster: &PostgresRealtimeCluster,
    receiver: &mut tokio::sync::broadcast::Receiver<instant_server::RuntimeRealtimeClusterDelivery>,
) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let delivery = receiver.recv().await.unwrap();
            cluster
                .acknowledge(RealtimeClusterConsumerKind::Runtime, delivery.event_id)
                .await
                .unwrap();
            if let RuntimeRealtimeClusterEvent::Broadcast { payload, .. } = delivery.event {
                return payload.data.data["text"].as_str().unwrap().to_owned();
            }
        }
    })
    .await
    .expect("durable event should arrive")
}

async fn test_notifications(url: &str, fixture: &Fixture) {
    let schema = &fixture.schema;
    let scoped_url = scoped_url(url, schema);
    let listener_machine_id = Uuid::new_v4();
    let listener = PostgresRealtimeCluster::from_pools_with_listener(
        listener_machine_id,
        PostgresPool::connect(&scoped_url, 1).await.unwrap(),
        PostgresPool::connect(&scoped_url, 1).await.unwrap(),
        &scoped_url,
    )
    .await
    .unwrap();
    let publisher = PostgresRealtimeCluster::new(
        Uuid::new_v4(),
        fixture.client(url).await,
        fixture.client(url).await,
    )
    .await
    .unwrap();
    let room = RoomKey::new(Uuid::new_v4(), "notifications");
    let mut receiver = listener
        .subscribe(RealtimeClusterConsumerKind::Runtime)
        .await
        .unwrap();

    publisher
        .publish_broadcast(
            &room,
            BroadcastPayload {
                room_id: room.room_id.clone(),
                topic: json!("topic"),
                data: BroadcastData {
                    peer_id: Uuid::new_v4(),
                    user: None,
                    data: json!({"text": "notified"}),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(next_broadcast(&listener, &mut receiver).await, "notified");

    let payload = BroadcastPayload {
        room_id: room.room_id.clone(),
        topic: json!("topic"),
        data: BroadcastData {
            peer_id: Uuid::new_v4(),
            user: None,
            data: json!({"text": "safety-poll"}),
        },
    };
    fixture
        .setup
        .execute(
            &format!(
                "INSERT INTO {schema}.rust_realtime_room_events
                 (source_machine_id, kind, app_id, room_id, payload)
                 VALUES ($1, 'broadcast', $2, $3, $4)"
            ),
            &[
                &Uuid::new_v4(),
                &room.app_id,
                &room.room_id,
                &Json(&payload),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        next_broadcast(&listener, &mut receiver).await,
        "safety-poll"
    );

    fixture
        .setup
        .batch_execute(&format!(
            "BEGIN;
             INSERT INTO {schema}.rust_realtime_room_events
               (source_machine_id, kind, app_id, room_id)
             VALUES ('00000000-0000-0000-0000-000000000000', 'broadcast', '{}', 'rollback');
             SELECT pg_notify('rust_realtime_room_events', '');
             ROLLBACK;",
            Uuid::new_v4()
        ))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1200), receiver.recv())
            .await
            .is_err()
    );

    let application_name = format!("instant-realtime-listener-{listener_machine_id}");
    let terminated_pid = listener_pid(&fixture.setup, &application_name, None).await;
    assert!(
        fixture
            .setup
            .query_one("SELECT pg_terminate_backend($1)", &[&terminated_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let outage_payload = BroadcastPayload {
        room_id: room.room_id.clone(),
        topic: json!("topic"),
        data: BroadcastData {
            peer_id: Uuid::new_v4(),
            user: None,
            data: json!({"text": "outage-fallback"}),
        },
    };
    let fallback_started = Instant::now();
    fixture
        .setup
        .execute(
            &format!(
                "INSERT INTO {schema}.rust_realtime_room_events
                 (source_machine_id, kind, app_id, room_id, payload)
                 VALUES ($1, 'broadcast', $2, $3, $4)"
            ),
            &[
                &Uuid::new_v4(),
                &room.app_id,
                &room.room_id,
                &Json(&outage_payload),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        next_broadcast(&listener, &mut receiver).await,
        "outage-fallback"
    );
    assert!(fallback_started.elapsed() < Duration::from_millis(750));

    let reconnected_pid =
        listener_pid(&fixture.setup, &application_name, Some(terminated_pid)).await;
    assert_ne!(reconnected_pid, terminated_pid);
    let reconnect_started = Instant::now();
    publisher
        .publish_broadcast(
            &room,
            BroadcastPayload {
                room_id: room.room_id.clone(),
                topic: json!("topic"),
                data: BroadcastData {
                    peer_id: Uuid::new_v4(),
                    user: None,
                    data: json!({"text": "reconnected"}),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        next_broadcast(&listener, &mut receiver).await,
        "reconnected"
    );
    assert!(reconnect_started.elapsed() < Duration::from_millis(750));
}

#[tokio::test]
async fn notifications_wake_subscribers_and_safety_poll_recovers_a_missed_wakeup() {
    let Some(url) = instant_test_support::test_database_url(
        "notifications_wake_subscribers_and_safety_poll_recovers_a_missed_wakeup",
    ) else {
        return;
    };
    let fixture = Fixture::create(&url).await;
    let result = AssertUnwindSafe(test_notifications(&url, &fixture))
        .catch_unwind()
        .await;
    fixture.cleanup().await;
    if let Err(payload) = result {
        resume_unwind(payload);
    }
}
