use std::time::Duration;

use instant_pg::{ByopMatchSet, ByopTopic, ByopWalRecord};
use instant_realtime::{BroadcastData, BroadcastPayload, PeerPresence, PeerUser, RoomKey};
use instant_server::{
    MAX_WAL_REFRESH_BATCH, MAX_WAL_REFRESH_WINDOWS, PostgresPool, PostgresRealtimeCluster,
    RealtimeClusterConsumerKind, RuntimeRealtimeCluster, RuntimeRealtimeClusterDelivery,
    RuntimeRealtimeClusterEvent, RuntimeStreamAppendResult,
};
use instant_wal::{Isn, Lsn, WalRecord};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_postgres::{Client, NoTls, types::Json};
use uuid::Uuid;

async fn client(database_url: &str, schema: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(&format!("SET search_path TO {schema}"))
        .await
        .unwrap();
    client
}

fn scoped_url(database_url: &str, schema: &str) -> String {
    let separator = if database_url.contains('?') { '&' } else { '?' };
    format!("{database_url}{separator}options=-csearch_path%3D{schema}")
}

async fn next_event(
    cluster: &PostgresRealtimeCluster,
    consumer: RealtimeClusterConsumerKind,
    receiver: &mut broadcast::Receiver<RuntimeRealtimeClusterDelivery>,
    predicate: impl Fn(&RuntimeRealtimeClusterEvent) -> bool,
) -> RuntimeRealtimeClusterEvent {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let delivery = receiver.recv().await.unwrap();
            let matches = predicate(&delivery.event);
            cluster
                .acknowledge(consumer, delivery.event_id)
                .await
                .unwrap();
            if matches {
                return delivery.event;
            }
        }
    })
    .await
    .expect("cluster event should arrive")
}

async fn wait_for_cursor(client: &Client, schema: &str, machine_id: Uuid, event_id: i64) {
    if tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let cursor = client
                .query_one(
                    &format!(
                        "SELECT event_cursor FROM {schema}.rust_realtime_event_consumers
                         WHERE machine_id = $1 AND consumer_kind = 'runtime'"
                    ),
                    &[&machine_id],
                )
                .await
                .unwrap()
                .get::<_, i64>(0);
            if cursor >= event_id {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_err()
    {
        let cursors = client
            .query(
                &format!(
                    "SELECT consumer_kind, event_cursor FROM {schema}.rust_realtime_event_consumers
                     WHERE machine_id = $1 ORDER BY consumer_kind"
                ),
                &[&machine_id],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, i64>(1)))
            .collect::<Vec<_>>();
        let events = client
            .query(
                &format!(
                    "SELECT id, kind, source_machine_id, room_id
                     FROM {schema}.rust_realtime_room_events
                     WHERE id <= $1 ORDER BY id"
                ),
                &[&event_id],
            )
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                (
                    row.get::<_, i64>(0),
                    row.get::<_, String>(1),
                    row.get::<_, Uuid>(2),
                    row.get::<_, String>(3),
                )
            })
            .collect::<Vec<_>>();
        panic!(
            "machine {machine_id} did not acknowledge event {event_id}; cursors: {cursors:?}; events: {events:?}"
        );
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // The isolated PostgreSQL schema keeps this regression self-contained.
async fn postgres_rejoining_a_session_uses_clojure_presence_data_truthiness() {
    let Some(url) = instant_test_support::test_database_url(
        "postgres_rejoining_a_session_uses_clojure_presence_data_truthiness",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("realtime_cluster_rejoin_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );
             CREATE TABLE {schema}.rust_realtime_room_members (
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               session_id uuid NOT NULL,
               machine_id uuid NOT NULL,
               instance_id text NOT NULL,
               user_id uuid,
               data jsonb NOT NULL DEFAULT '{{}}'::jsonb,
               PRIMARY KEY (app_id, room_id, session_id)
             );
             CREATE TABLE {schema}.rust_realtime_room_events (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               source_machine_id uuid NOT NULL,
               kind text NOT NULL,
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               payload jsonb,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.config (
               k text PRIMARY KEY NOT NULL,
               v json NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_machines (
               machine_id uuid PRIMARY KEY,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_event_consumers (
               machine_id uuid NOT NULL REFERENCES {schema}.rust_realtime_machines (machine_id)
                 ON DELETE CASCADE,
               consumer_kind text NOT NULL,
               event_cursor bigint NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (machine_id, consumer_kind)
             );"
        ))
        .await
        .unwrap();

    let first_machine = Uuid::new_v4();
    let second_machine = Uuid::new_v4();
    let first_writer_pool = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let first_reader_pool = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let first = PostgresRealtimeCluster::from_pools(
        first_machine,
        first_writer_pool.clone(),
        first_reader_pool.clone(),
    )
    .await
    .unwrap();
    let writer = first_writer_pool.acquire().await.unwrap();
    let reader = first_reader_pool.acquire().await.unwrap();
    let writer_pid = writer
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    let reader_pid = reader
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    assert_ne!(writer_pid, reader_pid);
    assert!(
        setup
            .query_one("SELECT pg_terminate_backend($1)", &[&writer_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert!(
        setup
            .query_one("SELECT pg_terminate_backend($1)", &[&reader_pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(writer);
    drop(reader);
    let second = PostgresRealtimeCluster::new(
        second_machine,
        client(&url, &schema).await,
        client(&url, &schema).await,
    )
    .await
    .unwrap();
    let mut first_events = first
        .subscribe(RealtimeClusterConsumerKind::Runtime)
        .await
        .unwrap();
    let replacement_writer = first_writer_pool.acquire().await.unwrap();
    let replacement_reader = first_reader_pool.acquire().await.unwrap();
    let replacement_writer_row = replacement_writer
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    let replacement_reader_row = replacement_reader
        .query_one("SELECT pg_backend_pid(), current_schema()", &[])
        .await
        .unwrap();
    assert_ne!(replacement_writer_row.get::<_, i32>(0), writer_pid);
    assert_ne!(replacement_reader_row.get::<_, i32>(0), reader_pid);
    assert_eq!(replacement_writer_row.get::<_, String>(1), schema);
    assert_eq!(replacement_reader_row.get::<_, String>(1), schema);
    drop(replacement_writer);
    drop(replacement_reader);
    let room = RoomKey::new(Uuid::new_v4(), "lobby");
    let session = Uuid::new_v4();
    let user_id = Uuid::new_v4();

    first
        .join(
            &room,
            PeerPresence {
                peer_id: session,
                instance_id: "first-instance".to_owned(),
                user: Some(PeerUser { id: user_id }),
                data: json!({"v": 1}),
            },
        )
        .await
        .unwrap();
    let snapshot = second
        .join(
            &room,
            PeerPresence {
                peer_id: session,
                instance_id: "second-instance".to_owned(),
                user: None,
                data: json!({"v": 2}),
            },
        )
        .await
        .unwrap();
    assert_eq!(snapshot[&session].data, json!({"v": 1}));
    assert_eq!(snapshot[&session].instance_id, "second-instance");
    assert_eq!(snapshot[&session].user, None);

    let event = next_event(
        &first,
        RealtimeClusterConsumerKind::Runtime,
        &mut first_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::Presence { source_machine_id, peers, .. }
                if *source_machine_id == second_machine
                    && peers.get(&session).is_some_and(|peer|
                        peer.data == json!({"v": 1})
                            && peer.instance_id == "second-instance"
                            && peer.user.is_none()))
        },
    )
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::Presence { .. }
    ));

    second
        .set_presence(&room, session, json!(false))
        .await
        .unwrap();
    let snapshot = first
        .join(
            &room,
            PeerPresence {
                peer_id: session,
                instance_id: "third-instance".to_owned(),
                user: Some(PeerUser { id: user_id }),
                data: json!({"v": 3}),
            },
        )
        .await
        .unwrap();
    assert_eq!(snapshot[&session].data, json!({"v": 3}));

    first
        .set_presence(&room, session, Value::Null)
        .await
        .unwrap();
    let snapshot = second
        .join(
            &room,
            PeerPresence {
                peer_id: session,
                instance_id: "fourth-instance".to_owned(),
                user: None,
                data: json!(false),
            },
        )
        .await
        .unwrap();
    assert_eq!(snapshot[&session].data, json!({}));

    let owner = setup
        .query_one(
            &format!(
                "SELECT machine_id, instance_id, user_id, data
                 FROM {schema}.rust_realtime_room_members
                 WHERE app_id = $1 AND room_id = $2 AND session_id = $3"
            ),
            &[&room.app_id, &room.room_id, &session],
        )
        .await
        .unwrap();
    assert_eq!(owner.get::<_, Uuid>(0), second_machine);
    assert_eq!(owner.get::<_, String>(1), "fourth-instance");
    assert_eq!(owner.get::<_, Option<Uuid>>(2), None);
    assert_eq!(owner.get::<_, Json<Value>>(3).0, json!({}));
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One scenario preserves cross-machine event ordering.
async fn postgres_rooms_replicate_presence_and_broadcasts_between_rust_servers() {
    let Some(url) = instant_test_support::test_database_url(
        "postgres_rooms_replicate_presence_and_broadcasts_between_rust_servers",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("realtime_cluster_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );
             CREATE TABLE {schema}.rust_realtime_room_members (
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               session_id uuid NOT NULL,
               machine_id uuid NOT NULL,
               instance_id text NOT NULL,
               user_id uuid,
               data jsonb NOT NULL DEFAULT '{{}}'::jsonb,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (app_id, room_id, session_id)
             );
             CREATE TABLE {schema}.rust_realtime_room_events (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               source_machine_id uuid NOT NULL,
               kind text NOT NULL CHECK (
                 kind IN (
                   'presence', 'broadcast', 'stream-append', 'stream-disconnect',
                   'sse-push', 'wal', 'byop-wal'
                 )
               ),
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               payload jsonb,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE UNIQUE INDEX rust_realtime_room_events_wal_idx
               ON {schema}.rust_realtime_room_events (kind, app_id, room_id)
               WHERE kind IN ('wal', 'byop-wal');
             CREATE TABLE {schema}.rust_realtime_sse_sessions (
               session_id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               machine_id uuid NOT NULL,
               token_hash bytea NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.config (
               k text PRIMARY KEY NOT NULL,
               v json NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_machines (
               machine_id uuid PRIMARY KEY,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_event_consumers (
               machine_id uuid NOT NULL REFERENCES {schema}.rust_realtime_machines (machine_id)
                 ON DELETE CASCADE,
               consumer_kind text NOT NULL CHECK (consumer_kind IN ('runtime', 'admin')),
               event_cursor bigint NOT NULL CHECK (event_cursor >= 0),
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (machine_id, consumer_kind)
             );"
        ))
        .await
        .unwrap();
    let first_machine = Uuid::new_v4();
    let second_machine = Uuid::new_v4();
    let first = PostgresRealtimeCluster::new(
        first_machine,
        client(&url, &schema).await,
        client(&url, &schema).await,
    )
    .await
    .unwrap();
    let second = PostgresRealtimeCluster::new(
        second_machine,
        client(&url, &schema).await,
        client(&url, &schema).await,
    )
    .await
    .unwrap();
    let mut first_events = first
        .subscribe(RealtimeClusterConsumerKind::Runtime)
        .await
        .unwrap();
    let mut second_events = second
        .subscribe(RealtimeClusterConsumerKind::Runtime)
        .await
        .unwrap();
    let room = RoomKey::new(Uuid::new_v4(), "lobby");
    let first_session = Uuid::new_v4();
    let second_session = Uuid::new_v4();

    assert_eq!(
        first
            .join(
                &room,
                PeerPresence {
                    peer_id: first_session,
                    instance_id: first_machine.to_string(),
                    user: None,
                    data: json!({"status": "first"}),
                },
            )
            .await
            .unwrap()
            .len(),
        1
    );
    let event = next_event(&second, RealtimeClusterConsumerKind::Runtime, &mut second_events, |event| {
        matches!(event, RuntimeRealtimeClusterEvent::Presence { source_machine_id, .. } if *source_machine_id == first_machine)
    })
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::Presence { peers, .. }
            if peers[&first_session].data["status"] == "first"
    ));

    assert_eq!(
        second
            .join(
                &room,
                PeerPresence {
                    peer_id: second_session,
                    instance_id: second_machine.to_string(),
                    user: None,
                    data: json!({"status": "second"}),
                },
            )
            .await
            .unwrap()
            .len(),
        2
    );
    let event = next_event(&first, RealtimeClusterConsumerKind::Runtime, &mut first_events, |event| {
        matches!(event, RuntimeRealtimeClusterEvent::Presence { source_machine_id, peers, .. } if *source_machine_id == second_machine && peers.len() == 2)
    })
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::Presence { .. }
    ));

    second
        .set_presence(&room, second_session, json!({"status": "updated"}))
        .await
        .unwrap();
    let event = next_event(
        &first,
        RealtimeClusterConsumerKind::Runtime,
        &mut first_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::Presence { source_machine_id, peers, .. }
            if *source_machine_id == second_machine
                && peers.get(&second_session).is_some_and(|peer| peer.data["status"] == "updated"))
        },
    )
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::Presence { .. }
    ));

    first
        .publish_broadcast(
            &room,
            BroadcastPayload {
                room_id: "lobby".to_owned(),
                topic: json!("chat"),
                data: BroadcastData {
                    peer_id: first_session,
                    user: None,
                    data: json!({"text": "hello"}),
                },
            },
        )
        .await
        .unwrap();
    let event = next_event(&second, RealtimeClusterConsumerKind::Runtime, &mut second_events, |event| {
        matches!(event, RuntimeRealtimeClusterEvent::Broadcast { source_machine_id, .. } if *source_machine_id == first_machine)
    })
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::Broadcast { payload, .. }
            if payload.data.data["text"] == "hello"
    ));

    let stream_id = Uuid::new_v4();
    first
        .publish_stream_append(
            room.app_id,
            RuntimeStreamAppendResult {
                stream_id,
                client_id: "writer".to_owned(),
                offset: 5,
                new_offset: 11,
                content: " world".to_owned(),
                done: false,
                abort_reason: None,
                flushed_offset: None,
            },
        )
        .await
        .unwrap();
    let event = next_event(&second, RealtimeClusterConsumerKind::Runtime, &mut second_events, |event| {
        matches!(event, RuntimeRealtimeClusterEvent::StreamAppend { source_machine_id, append, .. }
            if *source_machine_id == first_machine && append.stream_id == stream_id)
    })
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::StreamAppend { append, .. }
            if append.content == " world" && append.offset == 5
    ));

    first
        .publish_stream_disconnect(room.app_id, stream_id)
        .await
        .unwrap();
    let event = next_event(
        &second,
        RealtimeClusterConsumerKind::Runtime,
        &mut second_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::StreamWriterDisconnected {
            source_machine_id,
            stream_id: event_stream_id,
            ..
        } if *source_machine_id == first_machine && *event_stream_id == stream_id)
        },
    )
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::StreamWriterDisconnected { .. }
    ));

    let sse_session_id = Uuid::new_v4();
    let sse_token = Uuid::new_v4();
    second
        .register_sse_session(
            room.app_id,
            sse_session_id,
            Sha256::digest(sse_token.as_bytes()).into(),
        )
        .await
        .unwrap();
    assert!(
        first
            .forward_sse_push(instant_http::RuntimeSsePushRequest {
                app_id: room.app_id,
                machine_id: second_machine,
                session_id: sse_session_id,
                sse_token,
                messages: vec![json!({"op": "refresh-sync-table"})],
            })
            .await
            .unwrap()
    );
    let event = next_event(
        &second,
        RealtimeClusterConsumerKind::Runtime,
        &mut second_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::SsePush {
            target_machine_id,
            session_id,
            ..
        } if *target_machine_id == second_machine && *session_id == sse_session_id)
        },
    )
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::SsePush { messages, .. }
            if messages[0]["op"] == "refresh-sync-table"
    ));
    assert!(
        !first
            .forward_sse_push(instant_http::RuntimeSsePushRequest {
                app_id: room.app_id,
                machine_id: second_machine,
                session_id: sse_session_id,
                sse_token: Uuid::new_v4(),
                messages: Vec::new(),
            })
            .await
            .unwrap()
    );
    second.unregister_sse_session(sse_session_id).await.unwrap();

    let wal_lsn = Lsn::from_u64(0x1234);
    let wal_record = WalRecord {
        app_id: room.app_id,
        tx_id: 99,
        isn: Isn::new(7, wal_lsn),
        previous_isn: None,
        tx_created_at: None,
        tx_bytes: 0,
        next_lsn: wal_lsn,
        attr_changes: Vec::new(),
        ident_changes: Vec::new(),
        triple_changes: Vec::new(),
        messages: Vec::new(),
        wal_logs: Vec::new(),
        combined_count: 1,
    };
    first.publish_wal(wal_record.clone()).await.unwrap();
    first.publish_wal(wal_record.clone()).await.unwrap();
    let event = next_event(
        &second,
        RealtimeClusterConsumerKind::Runtime,
        &mut second_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::Wal { record, .. }
            if record.isn == wal_record.isn)
        },
    )
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::Wal { record, .. } if record.tx_id == 99
    ));
    let first_wal = next_event(
        &first,
        RealtimeClusterConsumerKind::Runtime,
        &mut first_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::Wal { record, .. }
            if record.isn == wal_record.isn)
        },
    )
    .await;
    assert!(matches!(first_wal, RuntimeRealtimeClusterEvent::Wal { .. }));
    assert_eq!(
        setup
            .query_one(
                &format!(
                    "SELECT count(*) FROM {schema}.rust_realtime_room_events
                     WHERE kind = 'wal' AND app_id = $1"
                ),
                &[&room.app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );

    let byop_record = ByopWalRecord {
        tx_id: 0x1235,
        next_lsn: Lsn::from_u64(0x1235),
        changes: Vec::new(),
    };
    let byop_topic = ByopTopic {
        entities: ByopMatchSet::Values(vec![json!("post-1")]),
        attributes: ByopMatchSet::Values(vec![Uuid::new_v4()]),
        values: ByopMatchSet::Any,
    };
    first
        .publish_byop_wal(room.app_id, byop_record.clone(), vec![byop_topic.clone()])
        .await
        .unwrap();
    second
        .publish_byop_wal(room.app_id, byop_record.clone(), vec![byop_topic.clone()])
        .await
        .unwrap();
    let event = next_event(
        &second,
        RealtimeClusterConsumerKind::Runtime,
        &mut second_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::ByopWal { record, .. }
            if record.tx_id == byop_record.tx_id)
        },
    )
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::ByopWal { app_id, record, topics, .. }
            if app_id == room.app_id && record == byop_record && topics == vec![byop_topic]
    ));
    let first_byop = next_event(
        &first,
        RealtimeClusterConsumerKind::Runtime,
        &mut first_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::ByopWal { record, .. }
            if record.tx_id == byop_record.tx_id)
        },
    )
    .await;
    assert!(matches!(
        first_byop,
        RuntimeRealtimeClusterEvent::ByopWal { .. }
    ));
    assert_eq!(
        setup
            .query_one(
                &format!(
                    "SELECT count(*) FROM {schema}.rust_realtime_room_events
                     WHERE kind = 'byop-wal' AND app_id = $1 AND room_id = $2"
                ),
                &[&room.app_id, &byop_record.tx_id.to_string()],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "the same external transaction published by two machines must fan out once"
    );
    let mut first_admin_events = first
        .subscribe(RealtimeClusterConsumerKind::Admin)
        .await
        .unwrap();
    let mut second_admin_events = second
        .subscribe(RealtimeClusterConsumerKind::Admin)
        .await
        .unwrap();
    let first_admin_replay = next_event(
        &first,
        RealtimeClusterConsumerKind::Admin,
        &mut first_admin_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::ByopWal { record, .. }
                if record.tx_id == byop_record.tx_id)
        },
    )
    .await;
    let second_admin_replay = next_event(
        &second,
        RealtimeClusterConsumerKind::Admin,
        &mut second_admin_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::ByopWal { record, .. }
                if record.tx_id == byop_record.tx_id)
        },
    )
    .await;
    assert!(matches!(
        (first_admin_replay, second_admin_replay),
        (
            RuntimeRealtimeClusterEvent::ByopWal { .. },
            RuntimeRealtimeClusterEvent::ByopWal { .. }
        )
    ));

    let wal_event_id = setup
        .query_one(
            &format!(
                "SELECT id FROM {schema}.rust_realtime_room_events
                 WHERE kind = 'wal' AND app_id = $1"
            ),
            &[&room.app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    let byop_event_id = setup
        .query_one(
            &format!(
                "SELECT id FROM {schema}.rust_realtime_room_events
                 WHERE kind = 'byop-wal' AND app_id = $1"
            ),
            &[&room.app_id],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    wait_for_cursor(&setup, &schema, first_machine, byop_event_id).await;
    wait_for_cursor(&setup, &schema, second_machine, byop_event_id).await;

    let resumed_machine = Uuid::new_v4();
    setup
        .execute(
            &format!("INSERT INTO {schema}.rust_realtime_machines (machine_id) VALUES ($1)"),
            &[&resumed_machine],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.rust_realtime_event_consumers
                   (machine_id, consumer_kind, event_cursor)
                 VALUES ($1, 'runtime', $2)"
            ),
            &[&resumed_machine, &(wal_event_id - 1)],
        )
        .await
        .unwrap();
    let resumed = PostgresRealtimeCluster::new(
        resumed_machine,
        client(&url, &schema).await,
        client(&url, &schema).await,
    )
    .await
    .unwrap();
    let mut resumed_events = resumed
        .subscribe(RealtimeClusterConsumerKind::Runtime)
        .await
        .unwrap();
    let replayed = next_event(
        &resumed,
        RealtimeClusterConsumerKind::Runtime,
        &mut resumed_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::Wal { record, .. }
            if record.isn == wal_record.isn)
        },
    )
    .await;
    assert!(matches!(
        replayed,
        RuntimeRealtimeClusterEvent::Wal { record, .. } if record == wal_record
    ));
    let replayed_byop = next_event(
        &resumed,
        RealtimeClusterConsumerKind::Runtime,
        &mut resumed_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::ByopWal { record, .. }
                if record.tx_id == byop_record.tx_id)
        },
    )
    .await;
    assert!(matches!(
        replayed_byop,
        RuntimeRealtimeClusterEvent::ByopWal { .. }
    ));
    wait_for_cursor(&setup, &schema, resumed_machine, byop_event_id).await;

    let lagging_machine = Uuid::new_v4();
    setup
        .execute(
            &format!("INSERT INTO {schema}.rust_realtime_machines (machine_id) VALUES ($1)"),
            &[&lagging_machine],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.rust_realtime_event_consumers
                   (machine_id, consumer_kind, event_cursor)
                 VALUES ($1, 'runtime', $2)"
            ),
            &[&lagging_machine, &(wal_event_id - 1)],
        )
        .await
        .unwrap();
    setup
        .execute(
            &format!(
                "UPDATE {schema}.rust_realtime_room_events
                 SET created_at = now() - interval '11 minutes'"
            ),
            &[],
        )
        .await
        .unwrap();
    first.prune_acknowledged_events().await.unwrap();
    assert_eq!(
        setup
            .query_one(
                &format!("SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE id = $1"),
                &[&wal_event_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "a lagging live machine must pin unacknowledged retained events"
    );
    setup
        .execute(
            &format!(
                "UPDATE {schema}.rust_realtime_machines
                 SET updated_at = now() - interval '31 seconds'
                 WHERE machine_id = $1"
            ),
            &[&lagging_machine],
        )
        .await
        .unwrap();
    first.prune_acknowledged_events().await.unwrap();
    assert_eq!(
        setup
            .query_one(
                &format!("SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE id = $1"),
                &[&wal_event_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "a stale machine lease must stop pinning acknowledged event retention"
    );

    setup
        .execute(
            &format!(
                "UPDATE {schema}.rust_realtime_machines
                 SET updated_at = now() - interval '31 seconds'
                 WHERE machine_id = $1"
            ),
            &[&first_machine],
        )
        .await
        .unwrap();
    let stale_wal_owner = first_machine.simple().to_string();
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.config (k, v)
                 VALUES ('wal-errors', jsonb_build_object(
                   $1::text, now()::text,
                   'legacy-jvm', (now() - interval '11 minutes')::text,
                   'unparseable', 'not a timestamp'
                 )::json)"
            ),
            &[&stale_wal_owner],
        )
        .await
        .unwrap();
    assert_eq!(second.reap_stale_machines().await.unwrap(), 1);
    let wal_errors: Value = setup
        .query_one(
            &format!("SELECT v::jsonb FROM {schema}.config WHERE k = 'wal-errors'"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        wal_errors,
        json!({}),
        "stale machine, old and unparseable non-UUID WAL markers must be swept"
    );
    let event = next_event(
        &second,
        RealtimeClusterConsumerKind::Runtime,
        &mut second_events,
        |event| {
            matches!(event, RuntimeRealtimeClusterEvent::Presence {
            source_machine_id,
            peers,
            ..
        } if source_machine_id.is_nil() && peers.len() == 1)
        },
    )
    .await;
    assert!(matches!(
        event,
        RuntimeRealtimeClusterEvent::Presence { peers, .. }
            if peers.contains_key(&second_session) && !peers.contains_key(&first_session)
    ));

    let peers = first.leave(&room, first_session).await.unwrap();
    assert_eq!(peers.len(), 1);
    assert!(peers.contains_key(&second_session));
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn committed_acknowledgements_drain_burst_without_poll_delay() {
    let Some(url) = instant_test_support::test_database_url(
        "committed_acknowledgements_drain_burst_without_poll_delay",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("realtime_ack_burst_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );
             CREATE TABLE {schema}.rust_realtime_room_members (
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               session_id uuid NOT NULL,
               machine_id uuid NOT NULL,
               instance_id text NOT NULL,
               user_id uuid,
               data jsonb NOT NULL DEFAULT '{{}}'::jsonb,
               PRIMARY KEY (app_id, room_id, session_id)
             );
             CREATE TABLE {schema}.rust_realtime_room_events (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               source_machine_id uuid NOT NULL,
               kind text NOT NULL,
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               payload jsonb,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.config (
               k text PRIMARY KEY NOT NULL,
               v json NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_machines (
               machine_id uuid PRIMARY KEY,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_event_consumers (
               machine_id uuid NOT NULL REFERENCES {schema}.rust_realtime_machines (machine_id)
                 ON DELETE CASCADE,
               consumer_kind text NOT NULL,
               event_cursor bigint NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (machine_id, consumer_kind)
             );"
        ))
        .await
        .unwrap();

    let machine_id = Uuid::new_v4();
    let cluster = PostgresRealtimeCluster::new(
        machine_id,
        client(&url, &schema).await,
        client(&url, &schema).await,
    )
    .await
    .unwrap();
    let consumer = RealtimeClusterConsumerKind::Runtime;
    let mut receiver = cluster.subscribe(consumer).await.unwrap();
    let app_id = Uuid::new_v4();
    // A committed burst exercises the real SQL poller, durable cursor, and delivery channel.
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.rust_realtime_room_events
         (source_machine_id, kind, app_id, room_id)
         SELECT $1, 'stream-disconnect', $2, $3 FROM generate_series(1, 64)"
            ),
            &[&machine_id, &app_id, &Uuid::new_v4().to_string()],
        )
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    // Delivery must still wait for this consumer's durable acknowledgement.
    cluster
        .acknowledge(RealtimeClusterConsumerKind::Admin, first.event_id)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(60), receiver.recv())
            .await
            .is_err()
    );
    setup
        .batch_execute(&format!(
            "ALTER TABLE {schema}.rust_realtime_event_consumers
         ADD CONSTRAINT reject_runtime_ack CHECK (consumer_kind <> 'runtime' OR event_cursor = 0)"
        ))
        .await
        .unwrap();
    assert!(cluster.acknowledge(consumer, first.event_id).await.is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(60), receiver.recv())
            .await
            .is_err(),
        "failed durable acknowledgement must not release delivery"
    );
    setup
        .batch_execute(&format!(
            "ALTER TABLE {schema}.rust_realtime_event_consumers DROP CONSTRAINT reject_runtime_ack"
        ))
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let mut last_id = first.event_id;
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        cluster.acknowledge(consumer, first.event_id).await.unwrap();
        for _ in 1..64 {
            let next = receiver.recv().await.unwrap();
            assert!(next.event_id > last_id, "delivery must remain ordered");
            last_id = next.event_id;
            cluster.acknowledge(consumer, next.event_id).await.unwrap();
        }
    })
    .await;
    let elapsed = started.elapsed();
    let record = WalRecord {
        app_id,
        tx_id: 100,
        isn: Isn::new(7, Lsn::from_u64(100)),
        previous_isn: None,
        tx_created_at: None,
        tx_bytes: 0,
        next_lsn: Lsn::from_u64(100),
        attr_changes: vec![],
        ident_changes: vec![],
        triple_changes: vec![],
        messages: vec![],
        wal_logs: vec![],
        combined_count: 1,
    };
    // Exceed the read-ahead limit with full batches. The non-WAL row fences all
    // previous WAL completions, including short SQL-fetch boundary windows.
    let wal_prefix_records = (MAX_WAL_REFRESH_WINDOWS + 1) * MAX_WAL_REFRESH_BATCH;
    let fence_event = wal_prefix_records + 1;
    let total_events = fence_event + 2;
    setup
        .execute(
            &format!(
                "INSERT INTO {schema}.rust_realtime_room_events
         (source_machine_id, kind, app_id, room_id, payload)
         SELECT $1, CASE WHEN n = {fence_event} THEN 'stream-disconnect' ELSE 'wal' END,
         $2, $3, CASE WHEN n = {fence_event} THEN NULL ELSE $4::jsonb END
         FROM generate_series(1, {total_events}) AS n ORDER BY n"
            ),
            &[
                &machine_id,
                &app_id,
                &Uuid::new_v4().to_string(),
                &Json(&record),
            ],
        )
        .await
        .unwrap();
    let mut unacknowledged = Vec::new();
    let mut wal_records = 0;
    for _ in 0..MAX_WAL_REFRESH_WINDOWS {
        let delivery = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeRealtimeClusterEvent::WalBatch { records } = &delivery.event else {
            panic!("expected WAL window, got {:?}", delivery.event);
        };
        assert!(records.len() <= MAX_WAL_REFRESH_BATCH);
        wal_records += records.len();
        unacknowledged.push(delivery.event_id);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(60), receiver.recv())
            .await
            .is_err(),
        "the poller exceeded its bounded unacknowledged window"
    );
    let durable_cursor: i64 = setup
        .query_one(
            &format!("SELECT event_cursor FROM {schema}.rust_realtime_event_consumers WHERE machine_id = $1 AND consumer_kind = 'runtime'"),
            &[&machine_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        durable_cursor, last_id,
        "dispatch alone must not advance the durable cursor"
    );
    cluster
        .acknowledge(consumer, unacknowledged[0])
        .await
        .unwrap();
    let next_window = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let RuntimeRealtimeClusterEvent::WalBatch { records } = &next_window.event else {
        panic!("expected the next WAL window after acknowledgement");
    };
    wal_records += records.len();
    assert!(
        tokio::time::timeout(Duration::from_millis(60), receiver.recv())
            .await
            .is_err(),
        "non-WAL fence escaped unfinished WAL windows"
    );
    // Complete the acknowledged prefix, then consume the remaining windows
    // caused by the SQL poller's 1,000-row fetch boundary.
    cluster
        .acknowledge(consumer, next_window.event_id)
        .await
        .unwrap();
    last_id = next_window.event_id;
    while wal_records < wal_prefix_records {
        let delivery = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeRealtimeClusterEvent::WalBatch { records } = &delivery.event else {
            panic!("expected remaining WAL records");
        };
        wal_records += records.len();
        last_id = delivery.event_id;
        if wal_records == wal_prefix_records {
            assert!(
                tokio::time::timeout(Duration::from_millis(60), receiver.recv())
                    .await
                    .is_err(),
                "non-WAL fence escaped the final unfinished WAL window"
            );
        }
        cluster.acknowledge(consumer, last_id).await.unwrap();
    }
    assert_eq!(wal_records, wal_prefix_records);
    for expected_size in [0, 2] {
        let delivery = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.event_id, last_id + expected_size.max(1));
        assert!(
            matches!(
                &delivery.event,
                RuntimeRealtimeClusterEvent::StreamWriterDisconnected { .. }
            ) && expected_size == 0
                || matches!(&delivery.event, RuntimeRealtimeClusterEvent::WalBatch { records } if records.len() == 2)
                    && expected_size == 2
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(60), receiver.recv())
                .await
                .is_err(),
            "next delivery escaped a non-WAL boundary"
        );
        cluster
            .acknowledge(consumer, delivery.event_id)
            .await
            .unwrap();
        last_id = delivery.event_id;
    }
    drop(receiver);
    drop(cluster);
    // Allow the idle poller to release its schema before fixture cleanup.
    tokio::time::sleep(Duration::from_millis(100)).await;
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    eprintln!("64 durable acknowledgements drained in {elapsed:?}");
    assert!(
        result.is_ok(),
        "committed acknowledgements must wake delivery immediately; burst exceeded one second"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn acknowledgements_do_not_block_wal_publication() {
    let Some(url) =
        instant_test_support::test_database_url("acknowledgements_do_not_block_wal_publication")
    else {
        return;
    };
    let setup = client(&url, "public").await;
    let schema = format!("realtime_ack_isolation_{}", Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             CREATE TABLE {schema}.attrs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL
             );
             CREATE TABLE {schema}.triples (
               app_id uuid NOT NULL,
               entity_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL
             );
             CREATE TABLE {schema}.rust_realtime_room_members (
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               session_id uuid NOT NULL,
               machine_id uuid NOT NULL,
               instance_id text NOT NULL,
               user_id uuid,
               data jsonb NOT NULL DEFAULT '{{}}'::jsonb,
               PRIMARY KEY (app_id, room_id, session_id)
             );
             CREATE TABLE {schema}.rust_realtime_room_events (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               source_machine_id uuid NOT NULL,
               kind text NOT NULL,
               app_id uuid NOT NULL,
               room_id text NOT NULL,
               payload jsonb,
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.config (
               k text PRIMARY KEY NOT NULL,
               v json NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_machines (
               machine_id uuid PRIMARY KEY,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TABLE {schema}.rust_realtime_event_consumers (
               machine_id uuid NOT NULL REFERENCES {schema}.rust_realtime_machines (machine_id)
                 ON DELETE CASCADE,
               consumer_kind text NOT NULL,
               event_cursor bigint NOT NULL,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY (machine_id, consumer_kind)
             );"
        ))
        .await
        .unwrap();

    setup.batch_execute(&format!("CREATE UNIQUE INDEX ON {schema}.rust_realtime_room_events (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal')")).await.unwrap();
    let machine_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let writer = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let reader = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let acknowledgements = PostgresPool::connect(&scoped_url(&url, &schema), 1)
        .await
        .unwrap();
    let cluster = PostgresRealtimeCluster::from_pools(machine_id, writer.clone(), reader)
        .await
        .unwrap()
        .with_acknowledgement_pool(acknowledgements.clone());
    let record = WalRecord {
        app_id,
        tx_id: 100,
        isn: Isn::new(7, Lsn::from_u64(100)),
        previous_isn: None,
        tx_created_at: None,
        tx_bytes: 0,
        next_lsn: Lsn::from_u64(100),
        attr_changes: vec![],
        ident_changes: vec![],
        triple_changes: vec![],
        messages: vec![],
        wal_logs: vec![],
        combined_count: 1,
    };

    // Occupying the acknowledgement connection cannot stall committed WAL publication.
    let held_ack = acknowledgements.acquire().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), cluster.publish_wal(record.clone()))
        .await
        .expect("WAL publication must not wait for the acknowledgement pool")
        .unwrap();
    let event_id: i64 = setup
        .query_one(
            &format!("SELECT id FROM {schema}.rust_realtime_room_events WHERE kind='wal'"),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            cluster.acknowledge(RealtimeClusterConsumerKind::Runtime, event_id)
        )
        .await
        .is_err()
    );
    let cursor: i64 = setup.query_one(&format!("SELECT event_cursor FROM {schema}.rust_realtime_event_consumers WHERE machine_id=$1 AND consumer_kind='runtime'"), &[&machine_id]).await.unwrap().get(0);
    assert!(
        cursor < event_id,
        "uncommitted acknowledgement must not advance the cursor"
    );
    drop(held_ack);
    // Conversely, a busy event writer cannot delay durable consumer progress.
    let held_writer = writer.acquire().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        cluster.acknowledge(RealtimeClusterConsumerKind::Runtime, event_id),
    )
    .await
    .expect("acknowledgement must not wait for the event writer")
    .unwrap();
    wait_for_cursor(&setup, &schema, machine_id, event_id).await;
    drop(held_writer);
    let lease = acknowledgements.acquire().await.unwrap();
    drop(deadpool_postgres::Object::take(lease));
    cluster
        .acknowledge(RealtimeClusterConsumerKind::Runtime, event_id)
        .await
        .unwrap();
    cluster.publish_wal(record.clone()).await.unwrap();
    assert_eq!(
        setup
            .query_one(
                &format!(
                    "SELECT count(*) FROM {schema}.rust_realtime_room_events WHERE kind='wal'"
                ),
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    if std::env::var_os("BENCHMARK_WAL_PUBLICATION").is_some() {
        let started = std::time::Instant::now();
        for n in 0..1000 {
            let mut next = record.clone();
            next.isn = Isn::new(7, Lsn::from_u64(1000 + n));
            next.next_lsn = next.isn.lsn();
            cluster.publish_wal(next).await.unwrap();
        }
        eprintln!(
            "wal-publication-benchmark records=1000 elapsed_ms={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    let lease = writer.acquire().await.unwrap();
    let prepared: i64 = lease
        .query_one(
            "SELECT count(*) FROM pg_prepared_statements WHERE statement LIKE 'WITH event AS (%'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        prepared, 1,
        "WAL publication must reuse its connection-local statement"
    );
    drop(lease);
    let lease = writer.acquire().await.unwrap();
    drop(deadpool_postgres::Object::take(lease));
    cluster.publish_wal(record).await.unwrap();
    drop(cluster);
    drop(writer);
    drop(acknowledgements);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
