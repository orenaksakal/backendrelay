use std::sync::Arc;
use std::time::Duration;

use instant_http::{RuntimeRealtimeBackend, RuntimeTransport};
use instant_server::{
    AdminDataService, PostgresAdminDataAuthorizer, RuntimeRealtimeService, migrate_database,
};
use serde_json::{Value, json};
use tokio_postgres::NoTls;
use uuid::Uuid;

fn attribute_by_id(attrs: &Value, id: Uuid) -> &Value {
    attrs
        .as_array()
        .unwrap()
        .iter()
        .find(|attribute| attribute["id"] == json!(id))
        .unwrap()
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the migrated wire and rename regression end to end.
async fn realtime_catalog_preserves_persisted_wire_metadata_for_schema_edits() {
    let Some(database_url) = instant_test_support::test_database_url(
        "realtime_catalog_preserves_persisted_wire_metadata_for_schema_edits",
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
            "INSERT INTO apps (id, creator_id, title) VALUES ($1, $2, 'realtime-wire')",
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

    let ref_attr_id = Uuid::new_v4();
    let ref_forward_id = Uuid::new_v4();
    let ref_reverse_id = Uuid::new_v4();
    let blob_attr_id = Uuid::new_v4();
    let blob_forward_id = Uuid::new_v4();
    setup
        .execute(
            "INSERT INTO attrs
               (id, app_id, value_type, cardinality, is_unique, is_indexed, is_required,
                forward_ident, etype, label, reverse_ident, reverse_etype, reverse_label,
                inferred_types)
             VALUES
               ($1, $2, 'ref', 'one', false, false, false,
                $3, 'posts', 'author', $4, 'users', 'posts', NULL),
               ($5, $2, 'blob', 'one', false, false, false,
                $6, 'posts', 'score', NULL, NULL, NULL,
                B'00000000000000000000000000000011')",
            &[
                &ref_attr_id,
                &app_id,
                &ref_forward_id,
                &ref_reverse_id,
                &blob_attr_id,
                &blob_forward_id,
            ],
        )
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO idents (id, app_id, attr_id, etype, label) VALUES
               ($1, $2, $3, 'posts', 'author'),
               ($4, $2, $3, 'users', 'posts'),
               ($5, $2, $6, 'posts', 'score')",
            &[
                &ref_forward_id,
                &app_id,
                &ref_attr_id,
                &ref_reverse_id,
                &blob_forward_id,
                &blob_attr_id,
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
    let initialized = tokio::time::timeout(Duration::from_secs(2), session.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initialized["op"], "init-ok");

    let ref_attr = attribute_by_id(&initialized["attrs"], ref_attr_id);
    assert_eq!(
        ref_attr,
        &json!({
            "id": ref_attr_id,
            "value-type": "ref",
            "cardinality": "one",
            "forward-identity": [ref_forward_id, "posts", "author"],
            "reverse-identity": [ref_reverse_id, "users", "posts"],
            "unique?": false,
            "index?": false,
            "required?": false,
            "inferred-types": null,
            "catalog": "user",
            "on-delete": null,
            "on-delete-reverse": null,
            "checked-data-type": null,
            "indexing?": false,
            "setting-unique?": false
        })
    );
    assert_eq!(
        attribute_by_id(&initialized["attrs"], blob_attr_id),
        &json!({
            "id": blob_attr_id,
            "value-type": "blob",
            "cardinality": "one",
            "forward-identity": [blob_forward_id, "posts", "score"],
            "reverse-identity": null,
            "unique?": false,
            "index?": false,
            "required?": false,
            "inferred-types": ["number", "string"],
            "catalog": "user",
            "on-delete": null,
            "on-delete-reverse": null,
            "checked-data-type": null,
            "indexing?": false,
            "setting-unique?": false
        })
    );

    service
        .message(
            session.session_id,
            json!({
                "op": "transact",
                "client-event-id": "rename-link",
                "tx-steps": [["update-attr", {
                    "id": ref_attr_id,
                    "forward-identity": [ref_attr["forward-identity"][0], "articles", "author"],
                    "reverse-identity": [ref_attr["reverse-identity"][0], "users", "articles"]
                }]]
            }),
        )
        .await
        .unwrap();
    let transacted = tokio::time::timeout(Duration::from_secs(2), session.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(transacted["op"], "transact-ok", "{transacted}");

    let attr_names = setup
        .query_one(
            "SELECT etype, label, reverse_etype, reverse_label
             FROM attrs WHERE app_id = $1 AND id = $2",
            &[&app_id, &ref_attr_id],
        )
        .await
        .unwrap();
    assert_eq!(attr_names.get::<_, String>(0), "articles");
    assert_eq!(attr_names.get::<_, String>(1), "author");
    assert_eq!(
        attr_names.get::<_, Option<String>>(2).as_deref(),
        Some("users")
    );
    assert_eq!(
        attr_names.get::<_, Option<String>>(3).as_deref(),
        Some("articles")
    );
    let identity_names = setup
        .query(
            "SELECT id, etype, label FROM idents WHERE id = ANY($1) ORDER BY id",
            &[&vec![ref_forward_id, ref_reverse_id]],
        )
        .await
        .unwrap();
    assert_eq!(identity_names.len(), 2);
    for row in identity_names {
        let id = row.get::<_, Uuid>(0);
        if id == ref_forward_id {
            assert_eq!(row.get::<_, String>(1), "articles");
            assert_eq!(row.get::<_, String>(2), "author");
        } else {
            assert_eq!(id, ref_reverse_id);
            assert_eq!(row.get::<_, String>(1), "users");
            assert_eq!(row.get::<_, String>(2), "articles");
        }
    }

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
