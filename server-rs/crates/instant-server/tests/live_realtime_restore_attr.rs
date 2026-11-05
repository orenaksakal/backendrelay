use std::sync::Arc;
use std::time::Duration;

use instant_http::{RuntimeRealtimeBackend, RuntimeRealtimeSession, RuntimeTransport};
use instant_server::{
    AdminDataService, PostgresAdminDataAuthorizer, RuntimeRealtimeService, migrate_database,
};
use serde_json::{Value, json};
use tokio_postgres::NoTls;
use uuid::Uuid;

async fn recv_frame(session: &mut RuntimeRealtimeSession) -> Value {
    tokio::time::timeout(Duration::from_secs(2), session.recv())
        .await
        .expect("the realtime session must answer")
        .expect("the realtime session must stay open")
}

/// DASH.fix7: a `restore-attr` transaction must acknowledge exactly like an
/// ordinary transaction. The dashboard awaits `transact-ok` before it reports
/// success, so a missing or reshaped frame leaves the Restore button silent.
#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the fixture, wire exchange and stored row proof together.
async fn restore_attr_transaction_acknowledges_like_an_ordinary_transaction() {
    let Some(database_url) = instant_test_support::test_database_url(
        "restore_attr_transaction_acknowledges_like_an_ordinary_transaction",
    ) else {
        return;
    };
    let (mut setup, setup_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_task = tokio::spawn(setup_connection);
    migrate_database(&mut setup).await.unwrap();

    let creator_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let admin_token = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, $2)",
            &[&creator_id, &format!("{creator_id}@example.com")],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'restore-attr-ack')",
            &[&app_id, &creator_id],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO app_admin_tokens (token, app_id) VALUES ($1, $2)",
            &[&admin_token, &app_id],
        )
        .await
        .unwrap();

    let title_attr = Uuid::new_v4();
    let title_ident = Uuid::new_v4();
    let notes_attr = Uuid::new_v4();
    let notes_ident = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO attrs
               (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
                forward_ident, etype, label)
             VALUES
               ($1, $3, 'blob', 'one', false, false, false, $2, 'todos', 'title'),
               ($4, $3, 'blob', 'one', false, false, false, $5, 'todos', 'notes')",
            &[
                &title_attr,
                &title_ident,
                &app_id,
                &notes_attr,
                &notes_ident,
            ],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO idents (id, app_id, attr_id, etype, label) VALUES
               ($1, $3, $2, 'todos', 'title'),
               ($4, $3, $5, 'todos', 'notes')",
            &[
                &title_ident,
                &title_attr,
                &app_id,
                &notes_ident,
                &notes_attr,
            ],
        )
        .await
        .unwrap();

    let (authorizer_client, authorizer_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let authorizer_task = tokio::spawn(authorizer_connection);
    let (data_client, data_connection) =
        tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let data_task = tokio::spawn(data_connection);
    let authorizer = Arc::new(PostgresAdminDataAuthorizer::new(authorizer_client));
    let data = Arc::new(AdminDataService::new(data_client, Arc::clone(&authorizer)));
    let service =
        RuntimeRealtimeService::new(Arc::clone(&authorizer), Arc::clone(&data), Uuid::new_v4())
            .with_transaction_isn_wait(Duration::ZERO);
    let mut session = service
        .open(Some(app_id), RuntimeTransport::WebSocket)
        .await
        .unwrap();
    service
        .message(
            session.session_id,
            json!({
                "op": "init",
                "app-id": app_id,
                "__admin-token": admin_token.to_string()
            }),
        )
        .await
        .unwrap();
    let initialized = recv_frame(&mut session).await;
    assert_eq!(initialized["op"], "init-ok");

    // An ordinary write establishes the acknowledgement shape the dashboard
    // already relies on (its earlier Schema steps toast on this path).
    service
        .message(
            session.session_id,
            json!({
                "op": "transact",
                "client-event-id": "add-title",
                "tx-steps": [["add-triple", Uuid::new_v4(), title_attr, "first"]]
            }),
        )
        .await
        .unwrap();
    let ordinary = recv_frame(&mut session).await;
    assert_eq!(ordinary["op"], "transact-ok", "{ordinary}");
    assert_eq!(ordinary["client-event-id"], "add-title", "{ordinary}");
    assert!(ordinary["tx-id"].is_number(), "{ordinary}");
    assert!(ordinary["isn"].is_string(), "{ordinary}");
    let ordinary_keys = ordinary
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();

    service
        .message(
            session.session_id,
            json!({
                "op": "transact",
                "client-event-id": "delete-notes",
                "tx-steps": [["delete-attr", notes_attr]]
            }),
        )
        .await
        .unwrap();
    let deleted = recv_frame(&mut session).await;
    assert_eq!(deleted["op"], "transact-ok", "{deleted}");
    assert_eq!(deleted["client-event-id"], "delete-notes", "{deleted}");
    assert_eq!(
        deleted
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ordinary_keys,
        "delete-attr must acknowledge with the ordinary transaction shape"
    );

    service
        .message(
            session.session_id,
            json!({
                "op": "transact",
                "client-event-id": "restore-notes",
                "tx-steps": [["restore-attr", notes_attr]]
            }),
        )
        .await
        .unwrap();
    let restored = recv_frame(&mut session).await;
    assert_eq!(restored["op"], "transact-ok", "{restored}");
    assert_eq!(restored["client-event-id"], "restore-notes", "{restored}");
    assert_eq!(
        restored
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ordinary_keys,
        "restore-attr must acknowledge with the ordinary transaction shape"
    );

    let row = setup
        .query_one(
            "SELECT etype, label, deletion_marked_at IS NULL
             FROM attrs WHERE app_id = $1 AND id = $2",
            &[&app_id, &notes_attr],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "todos");
    assert_eq!(row.get::<_, String>(1), "notes");
    assert!(row.get::<_, bool>(2));

    drop(session);
    drop(service);
    drop(data);
    drop(authorizer);
    setup
        .execute("DELETE FROM instant_users WHERE id = $1", &[&creator_id])
        .await
        .unwrap();
    drop(setup);
    setup_task.await.unwrap().unwrap();
    authorizer_task.abort();
    data_task.abort();
}
