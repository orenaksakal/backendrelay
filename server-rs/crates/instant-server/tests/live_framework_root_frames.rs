//! Differential checks for the root-frame split of native framework queries.
//!
//! Every case runs the existing implementation and the production path on the
//! same catalog, rules, data and actor, then requires identical responses,
//! topics and errors. Cases that cannot use frames assert that they stayed on
//! the existing path.
use std::sync::Arc;
use std::time::Duration;

use instant_http::{AdminActor, AdminRequestContext, ApiError, RuntimeFrameworkResponse};
use instant_instaql::{
    AsyncFileUrlBatchFuture, AsyncFileUrlSigner, Attribute, AttributeCatalog, AttributeValueType,
    Cardinality, DeleteAction, system_attribute_catalog,
};
use instant_server::{
    AdminDataAuthorizer, AdminDataScope, AdminDataService, PostgresEffectiveAppStatusPolicy,
    PostgresPool, PreparedInstantQuery, QueryModifierPolicy, QueryModifierPolicyFuture,
    QueryPlannerPolicy, QueryPlannerPolicyFuture, QueryPreparationPolicy,
    QueryPreparationPolicyFuture, QuerySettingsPolicy, QuerySettingsPolicyFuture, RootFrameCaches,
    StaticDatalogCachePolicy, migrate_database,
};
use serde_json::{Value, json};
use tokio_postgres::{NoTls, types::Json};
use uuid::Uuid;

#[derive(Clone, Debug)]
struct TestAuthorizer {
    catalog: AttributeCatalog,
}

impl AdminDataAuthorizer for TestAuthorizer {
    async fn authorize(
        &self,
        _: AdminRequestContext,
        _: AdminDataScope,
    ) -> Result<AttributeCatalog, ApiError> {
        Ok(self.catalog.clone())
    }

    async fn load_catalog(&self, _: Uuid) -> Result<AttributeCatalog, ApiError> {
        Ok(self.catalog.clone())
    }
}

struct TestFileSigner;

impl AsyncFileUrlSigner for TestFileSigner {
    fn signed_download_urls(
        &self,
        _app_id: Uuid,
        location_ids: Vec<String>,
    ) -> AsyncFileUrlBatchFuture<'_> {
        Box::pin(async move {
            Ok(location_ids
                .into_iter()
                .map(|location| format!("https://files.test/{location}"))
                .collect())
        })
    }
}

/// Mirrors the default preparation controls with the shared-frame rollback
/// toggle applied, without needing a live config-app toggle row.
struct DisableSharedFrames;

impl QueryModifierPolicy for DisableSharedFrames {
    fn apply<'a>(
        &'a self,
        _app_id: Uuid,
        _original_query_hash: i32,
        _query: &'a Value,
    ) -> QueryModifierPolicyFuture<'a> {
        unreachable!("the combined preparation policy runs")
    }
}

impl QuerySettingsPolicy for DisableSharedFrames {
    fn settings(&self, _original_query_hash: i32) -> QuerySettingsPolicyFuture<'_> {
        unreachable!("the combined preparation policy runs")
    }
}

impl QueryPlannerPolicy for DisableSharedFrames {
    fn options<'a>(&'a self, _query: &'a Value) -> QueryPlannerPolicyFuture<'a> {
        unreachable!("the combined preparation policy runs")
    }
}

impl QueryPreparationPolicy for DisableSharedFrames {
    fn prepare<'a>(
        &'a self,
        _app_id: Uuid,
        original: &'a Value,
    ) -> QueryPreparationPolicyFuture<'a> {
        Box::pin(async move {
            Ok(PreparedInstantQuery {
                effective: original.clone(),
                original_hash: instant_instaql::normalized_query_hash(original).unwrap(),
                planner_options: instant_datalog::MatchCteOptions::default(),
                postgres_settings: Vec::new(),
                permission_attribute_projection: true,
                shared_root_frames: false,
            })
        })
    }
}

fn attribute(id: Uuid, label: &str, unique: bool) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: "posts".to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique,
        setting_unique: false,
        indexed: unique,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: None,
        reverse_label: None,
        on_delete: None,
        on_delete_reverse: None,
    }
}

fn named_attribute(id: Uuid, etype: &str, label: &str, unique: bool) -> Attribute {
    let mut attribute = attribute(id, label, unique);
    attribute.etype = etype.to_owned();
    attribute
}

fn reference_attribute(
    id: Uuid,
    etype: &str,
    label: &str,
    reverse_etype: &str,
    reverse_label: &str,
) -> Attribute {
    let mut attribute = named_attribute(id, etype, label, false);
    attribute.value_type = AttributeValueType::Ref;
    attribute.reverse_etype = Some(reverse_etype.to_owned());
    attribute.reverse_label = Some(reverse_label.to_owned());
    attribute.on_delete = Some(DeleteAction::Cascade);
    attribute
}

fn context(app_id: Uuid, actor: AdminActor) -> AdminRequestContext {
    AdminRequestContext {
        app_id,
        bearer_token: Some("admin-secret".to_owned()),
        actor,
        client_ip: None,
    }
}

struct Fixture {
    writer: tokio_postgres::Client,
    driver: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
    pool: PostgresPool,
    app: Uuid,
    catalog: AttributeCatalog,
    user_a: Uuid,
    token_a: Uuid,
    user_b: Uuid,
    token_b: Uuid,
    room_a: Uuid,
    room_b: Uuid,
    message_a: Uuid,
    message_both: Uuid,
    message_b: Uuid,
    message_none: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let Some(url) =
            instant_test_support::test_database_url("live_framework_root_frames_fixture")
        else {
            return None;
        };
        let (mut setup, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
        let setup_driver = tokio::spawn(connection);
        migrate_database(&mut setup).await.unwrap();
        drop(setup);
        setup_driver.await.unwrap().unwrap();
        let scoped = install_shared_schema(&url).await;
        let (mut writer, connection) = tokio_postgres::connect(&scoped, NoTls).await.unwrap();
        let driver = tokio::spawn(connection);
        let app = Uuid::new_v4();
        writer
            .execute("INSERT INTO apps(id) VALUES($1)", &[&app])
            .await
            .unwrap();

        let mut catalog = system_attribute_catalog().unwrap();
        let mut created_at = named_attribute(Uuid::new_v4(), "todos", "createdAt", false);
        created_at.indexed = true;
        created_at.checked_data_type = Some(instant_datalog::DataType::Number);
        catalog.attributes.extend([
            named_attribute(Uuid::new_v4(), "todos", "id", true),
            named_attribute(Uuid::new_v4(), "todos", "text", false),
            named_attribute(Uuid::new_v4(), "todos", "ownerId", false),
            named_attribute(Uuid::new_v4(), "todos", "secret", false),
            created_at,
            reference_attribute(Uuid::new_v4(), "todos", "owner", "people", "todos"),
            named_attribute(Uuid::new_v4(), "people", "id", true),
            named_attribute(Uuid::new_v4(), "people", "name", false),
            named_attribute(Uuid::new_v4(), "messages", "id", true),
            named_attribute(Uuid::new_v4(), "messages", "body", false),
            named_attribute(Uuid::new_v4(), "messages", "roomId", false),
            named_attribute(Uuid::new_v4(), "messages", "memberIds", false),
            reference_attribute(Uuid::new_v4(), "messages", "room", "rooms", "messages"),
            named_attribute(Uuid::new_v4(), "rooms", "id", true),
            named_attribute(Uuid::new_v4(), "rooms", "name", false),
        ]);
        insert_catalog_attrs(&writer, app, &catalog).await;

        let user_a = instant_auth::sign_in_guest(&mut writer, app, &catalog)
            .await
            .unwrap();
        let user_b = instant_auth::sign_in_guest(&mut writer, app, &catalog)
            .await
            .unwrap();
        let mut fixture = Self {
            writer,
            driver,
            pool: PostgresPool::connect(&scoped, 1).await.unwrap(),
            app,
            catalog,
            user_a: user_a.user.id,
            token_a: user_a.refresh_token,
            user_b: user_b.user.id,
            token_b: user_b.refresh_token,
            room_a: Uuid::new_v4(),
            room_b: Uuid::new_v4(),
            message_a: Uuid::new_v4(),
            message_both: Uuid::new_v4(),
            message_b: Uuid::new_v4(),
            message_none: Uuid::new_v4(),
        };
        fixture.seed().await;
        Some(fixture)
    }

    async fn seed(&mut self) {
        let todo_a1 = Uuid::new_v4();
        let todo_a2 = Uuid::new_v4();
        let todo_hidden = Uuid::new_v4();
        let todo_blocked = Uuid::new_v4();
        let todo_b1 = Uuid::new_v4();
        let room_shared = self.room_a;
        let room_other = self.room_b;
        let message_a = self.message_a;
        let message_both = self.message_both;
        let message_b = self.message_b;
        let message_none = self.message_none;
        let file = Uuid::new_v4();
        for (entity, etype, label, value) in [
            (self.user_a, "people", "id", json!(self.user_a)),
            (self.user_a, "people", "name", json!("alice")),
            (self.user_b, "people", "id", json!(self.user_b)),
            (self.user_b, "people", "name", json!("bob")),
            (todo_a1, "todos", "id", json!(todo_a1)),
            (todo_a1, "todos", "text", json!("alice-one")),
            (todo_a1, "todos", "ownerId", json!(self.user_a)),
            (todo_a1, "todos", "secret", json!("visible")),
            (todo_a1, "todos", "createdAt", json!(1)),
            (todo_a1, "todos", "owner", json!(self.user_a)),
            (todo_a2, "todos", "id", json!(todo_a2)),
            (todo_a2, "todos", "text", json!("alice-two")),
            (todo_a2, "todos", "ownerId", json!(self.user_a)),
            (todo_a2, "todos", "secret", json!("visible")),
            (todo_a2, "todos", "createdAt", json!(2)),
            (todo_a2, "todos", "owner", json!(self.user_a)),
            (todo_hidden, "todos", "id", json!(todo_hidden)),
            (todo_hidden, "todos", "text", json!("alice-hidden-field")),
            (todo_hidden, "todos", "ownerId", json!(self.user_a)),
            (todo_hidden, "todos", "secret", json!("hidden")),
            (todo_hidden, "todos", "createdAt", json!(3)),
            (todo_hidden, "todos", "owner", json!(self.user_a)),
            (todo_blocked, "todos", "id", json!(todo_blocked)),
            (todo_blocked, "todos", "text", json!("blocked")),
            (todo_blocked, "todos", "ownerId", json!(self.user_a)),
            (todo_blocked, "todos", "secret", json!("visible")),
            (todo_blocked, "todos", "createdAt", json!(4)),
            (todo_blocked, "todos", "owner", json!(self.user_a)),
            (todo_b1, "todos", "id", json!(todo_b1)),
            (todo_b1, "todos", "text", json!("bob-one")),
            (todo_b1, "todos", "ownerId", json!(self.user_b)),
            (todo_b1, "todos", "secret", json!("visible")),
            (todo_b1, "todos", "createdAt", json!(5)),
            (todo_b1, "todos", "owner", json!(self.user_b)),
            (room_shared, "rooms", "id", json!(room_shared)),
            (room_shared, "rooms", "name", json!("shared")),
            (room_other, "rooms", "id", json!(room_other)),
            (room_other, "rooms", "name", json!("other")),
            (message_a, "messages", "id", json!(message_a)),
            (message_a, "messages", "body", json!("alice-only")),
            (message_a, "messages", "roomId", json!(room_shared)),
            (message_a, "messages", "memberIds", json!([self.user_a])),
            (message_a, "messages", "room", json!(room_shared)),
            (message_both, "messages", "id", json!(message_both)),
            (message_both, "messages", "body", json!("both")),
            (message_both, "messages", "roomId", json!(room_shared)),
            (
                message_both,
                "messages",
                "memberIds",
                json!([self.user_a, self.user_b]),
            ),
            (message_both, "messages", "room", json!(room_shared)),
            (message_b, "messages", "id", json!(message_b)),
            (message_b, "messages", "body", json!("bob-only")),
            (message_b, "messages", "roomId", json!(room_other)),
            (message_b, "messages", "memberIds", json!([self.user_b])),
            (message_b, "messages", "room", json!(room_other)),
            (message_none, "messages", "id", json!(message_none)),
            (message_none, "messages", "body", json!("nobody")),
            (message_none, "messages", "roomId", json!(room_other)),
            (message_none, "messages", "memberIds", json!([])),
            (message_none, "messages", "room", json!(room_other)),
            (file, "$files", "id", json!(file)),
            (file, "$files", "path", json!("/uploads/report.pdf")),
            (file, "$files", "location-id", json!("file-location")),
            (file, "$files", "size", json!(42)),
            (file, "$files", "content-type", json!("application/pdf")),
        ] {
            self.insert(entity, etype, label, value).await;
        }
        self.rules().await;
    }

    async fn insert(&self, entity: Uuid, etype: &str, label: &str, value: Value) {
        let attr = self.catalog.forward(etype, label).unwrap();
        let checked_type = attr
            .checked_data_type
            .map(|data_type| format!("{data_type:?}").to_lowercase());
        self.writer
            .execute(
                "INSERT INTO triples(app_id,entity_id,attr_id,value,value_md5,ea,eav,av,ave,vae,\
                 checked_data_type) VALUES($1,$2,$3,$4,$5,true,$6,$7,$8,$6,$9::text::checked_data_type)",
                &[
                    &self.app,
                    &entity,
                    &attr.id,
                    &Json(&value),
                    &value.to_string(),
                    &(attr.value_type == AttributeValueType::Ref),
                    &attr.unique,
                    &attr.indexed,
                    &checked_type,
                ],
            )
            .await
            .unwrap();
    }

    /// Replaces one cardinality-one triple, mirroring the WAL update topics the
    /// tests build for it.
    async fn update(&self, entity: Uuid, etype: &str, label: &str, value: Value) {
        let attr = self.catalog.forward(etype, label).unwrap();
        let updated = self
            .writer
            .execute(
                "UPDATE triples SET value=$4, value_md5=$5
                 WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3",
                &[
                    &self.app,
                    &entity,
                    &attr.id,
                    &Json(&value),
                    &value.to_string(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(updated, 1, "expected one {etype}.{label} triple");
    }

    fn frame_stats(&self, service: &AdminDataService<TestAuthorizer>) -> (u64, u64, u64) {
        let stats = service
            .root_frame_cache_stats(self.app)
            .expect("shared frame registry is configured");
        (stats.hits, stats.misses, stats.loads)
    }

    async fn rules(&self) {
        self.writer
            .execute(
                "INSERT INTO rules(app_id,code) VALUES($1,$2) \
                 ON CONFLICT(app_id) DO UPDATE SET code=excluded.code",
                &[
                    &self.app,
                    &json!({
                        "todos": {
                            "allow": {"view": "auth.id == data.ownerId && data.text != 'blocked'"},
                            "fields": {"secret": "data.secret != 'hidden'"}
                        },
                        "messages": {"allow": {"view": "auth.id in data.memberIds"}},
                        "rooms": {"allow": {"view": "true"}},
                        "people": {"allow": {"view": "true"}},
                        "$files": {"allow": {"view": "true"}}
                    }),
                ],
            )
            .await
            .unwrap();
    }

    async fn set_rules(&self, rules: Value) {
        self.writer
            .execute(
                "INSERT INTO rules(app_id,code) VALUES($1,$2) \
                 ON CONFLICT(app_id) DO UPDATE SET code=excluded.code",
                &[&self.app, &Json(&rules)],
            )
            .await
            .unwrap();
    }

    fn service(&self) -> AdminDataService<TestAuthorizer> {
        AdminDataService::from_pool(
            self.pool.clone(),
            Arc::new(TestAuthorizer {
                catalog: self.catalog.clone(),
            }),
        )
        .with_app_status_policy(Arc::new(PostgresEffectiveAppStatusPolicy::from_pool(
            self.pool.clone(),
        )))
        .with_file_url_signer(Arc::new(TestFileSigner))
    }

    fn shared_service(&self) -> (Arc<RootFrameCaches>, AdminDataService<TestAuthorizer>) {
        let caches = Arc::new(RootFrameCaches::new(Arc::new(
            StaticDatalogCachePolicy::default(),
        )));
        let service = self.service().with_root_frame_caches(Arc::clone(&caches));
        (caches, service)
    }

    /// Extracts the permitted messages' body triples from the framework result
    /// envelope, which is a flat list of `[entity, attr, value, created]` rows.
    fn message_bodies(&self, response: &RuntimeFrameworkResponse) -> Vec<String> {
        let body_attr = self.catalog.forward("messages", "body").unwrap().id;
        let Some(rows) = response
            .result
            .get(0)
            .and_then(|root| root.get("data"))
            .and_then(|data| data.get("datalog-result"))
            .and_then(|result| result.get("join-rows"))
            .and_then(|rows| rows.get(0))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut bodies = rows
            .iter()
            .filter_map(|triple| {
                let triple = triple.as_array()?;
                let attr = Uuid::parse_str(triple.get(1)?.as_str()?).ok()?;
                (attr == body_attr)
                    .then(|| triple.get(2)?.as_str().map(str::to_owned))
                    .flatten()
            })
            .collect::<Vec<_>>();
        bodies.sort();
        bodies
    }

    async fn differential(
        &self,
        service: &AdminDataService<TestAuthorizer>,
        actor: AdminActor,
        query: Value,
    ) -> (
        Result<RuntimeFrameworkResponse, ApiError>,
        Result<RuntimeFrameworkResponse, ApiError>,
        bool,
    ) {
        let (existing, production, used_root_frames) =
            Box::pin(service.differential_native_framework_query(
                context(self.app, actor),
                query.as_object().unwrap().clone(),
                &self.catalog,
                true,
            ))
            .await;
        assert_eq!(
            existing, production,
            "existing and frame paths diverged for {query}"
        );
        (existing, production, used_root_frames)
    }

    /// Runs both paths with the interactive workload an initial add-query uses.
    async fn differential_initial(
        &self,
        service: &AdminDataService<TestAuthorizer>,
        actor: AdminActor,
        query: Value,
    ) -> (
        Result<RuntimeFrameworkResponse, ApiError>,
        Result<RuntimeFrameworkResponse, ApiError>,
        bool,
    ) {
        let (existing, production, used_root_frames) =
            Box::pin(service.differential_initial_native_framework_query(
                context(self.app, actor),
                query.as_object().unwrap().clone(),
                &self.catalog,
                true,
            ))
            .await;
        assert_eq!(
            existing, production,
            "existing and per-request frame paths diverged for {query}"
        );
        (existing, production, used_root_frames)
    }

    /// Runs both paths with the refresh workload and shared reuse disabled, as
    /// a coarse-selected refresh batch does.
    async fn differential_coarse(
        &self,
        service: &AdminDataService<TestAuthorizer>,
        actor: AdminActor,
        query: Value,
    ) -> (
        Result<RuntimeFrameworkResponse, ApiError>,
        Result<RuntimeFrameworkResponse, ApiError>,
        bool,
    ) {
        let (existing, production, used_root_frames) =
            Box::pin(service.differential_coarse_native_framework_query(
                context(self.app, actor),
                query.as_object().unwrap().clone(),
                &self.catalog,
                true,
            ))
            .await;
        assert_eq!(
            existing, production,
            "existing and coarse per-request frame paths diverged for {query}"
        );
        (existing, production, used_root_frames)
    }

    async fn close(self) {
        let lease = tokio::time::timeout(Duration::from_secs(2), self.pool.acquire())
            .await
            .expect("framework query leaked the only pool lease")
            .unwrap();
        lease.simple_query("SELECT 1").await.unwrap();
        drop(lease);
        drop(self.pool);
        drop(self.writer);
        self.driver.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn native_framework_forward_owner_edges_survive_permission_filtering() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_, service) = fixture.shared_service();
    let owner_attr = json!(fixture.catalog.forward("todos", "owner").unwrap().id);
    let text_attr = json!(fixture.catalog.forward("todos", "text").unwrap().id);
    let triples = |response: &RuntimeFrameworkResponse, attr: &Value| {
        response.result[0]["data"]["datalog-result"]["join-rows"][0]
            .as_array()
            .unwrap()
            .iter()
            .filter(|triple| &triple[1] == attr)
            .cloned()
            .collect::<Vec<_>>()
    };
    for ref_rule in [false, true] {
        fixture
            .set_rules(json!({
                "todos": {
                    "allow": {"view": if ref_rule {
                        "auth.id in data.ref('owner.id') && data.text != 'blocked'"
                    } else {
                        "auth.id == data.ownerId && data.text != 'blocked'"
                    }},
                    "fields": {"secret": "data.secret != 'hidden'"}
                },
                "people": {"allow": {"view": "true"}}
            }))
            .await;
        for (token, user, count) in [
            (fixture.token_a, fixture.user_a, 3),
            (fixture.token_b, fixture.user_b, 1),
        ] {
            for query in [
                json!({"todos": {"$": {"where": {"owner.id": user}}, "owner": {}}}),
                json!({"people": {"$": {"where": {"id": user}}, "todos": {}}}),
            ] {
                // Initial/owned results and refresh/shared borrowed frames must
                // both include the edges the real SDK uses to re-run its query.
                for initial in [true, false, false] {
                    let actor = AdminActor::RefreshToken(token);
                    let (response, _, used_frames) = if initial {
                        fixture
                            .differential_initial(&service, actor, query.clone())
                            .await
                    } else {
                        fixture.differential(&service, actor, query.clone()).await
                    };
                    assert_eq!(used_frames, !ref_rule);
                    let response = response.unwrap();
                    let edges = triples(&response, &owner_attr);
                    assert_eq!(edges.len(), count, "missing owner edges for {query}");
                    assert!(edges.iter().all(|edge| edge[2] == json!(user)));
                    assert_eq!(triples(&response, &text_attr).len(), count);
                    assert!(!response.result.to_string().contains("\"blocked\""));
                    assert!(!response.result.to_string().contains("\"hidden\""));
                }
            }
        }
    }
    fixture
        .set_rules(json!({
            "todos": {"allow": {"view": "auth.id == data.ownerId"}},
            "people": {"allow": {"view": "size(data) == 0"}}
        }))
        .await;
    let (response, _, _) = fixture
        .differential_initial(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {"owner": {}}}),
        )
        .await;
    let response = response.unwrap();
    assert!(!triples(&response, &text_attr).is_empty());
    assert!(
        triples(&response, &owner_attr).is_empty(),
        "denied owner links must stay hidden"
    );
    fixture.close().await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn native_framework_root_frames_match_the_existing_path() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let service = fixture.service();
    let users = [
        ("alice", AdminActor::RefreshToken(fixture.token_a)),
        ("bob", AdminActor::RefreshToken(fixture.token_b)),
        ("guest", AdminActor::Guest),
    ];

    for (name, actor) in users {
        for query in [
            json!({"todos": {}}),
            json!({"messages": {}}),
            json!({"todos": {}, "messages": {}, "rooms": {}}),
            json!({"todos": {"$": {"fields": ["id", "text", "secret", "ownerId"]}}}),
            json!({"todos": {"$": {"limit": 2, "order": {"createdAt": "asc"}}}}),
            json!({"rooms": {"messages": {}}, "todos": {"owner": {}}}),
            json!({"messages": {"$": {"fields": ["id", "body", "memberIds"]}}}),
        ] {
            let (result, _, used_root_frames) =
                fixture.differential(&service, actor.clone(), query).await;
            assert!(used_root_frames, "{name} should use frames");
            assert!(
                result.is_ok(),
                "{name} query failed on both paths: {result:?}"
            );
        }
    }

    // The shared fixture must actually exercise row and field denial, not just
    // return identical empty results.
    let (alice_todos, _, _) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {}}),
        )
        .await;
    let alice_todos = alice_todos.unwrap().result.to_string();
    assert!(alice_todos.contains("alice-one"));
    assert!(!alice_todos.contains("bob-one"));
    assert!(!alice_todos.contains("\"blocked\""));
    assert!(!alice_todos.contains("\"hidden\""));
    let (bob_messages, _, _) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_b),
            json!({"messages": {}}),
        )
        .await;
    let bob_messages = bob_messages.unwrap().result.to_string();
    assert!(bob_messages.contains("bob-only"));
    assert!(bob_messages.contains("\"both\""));
    assert!(!bob_messages.contains("alice-only"));
    assert!(!bob_messages.contains("nobody"));
    let (paged, _, _) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {"$": {"limit": 2, "order": {"createdAt": "asc"}}}}),
        )
        .await;
    assert!(
        paged
            .unwrap()
            .result
            .to_string()
            .contains("\"has-next-page?\":true")
    );

    // $files goes through the same transform on both paths and must sign the
    // same URL while keeping location-id visibility identical.
    let (files, _, used_root_frames) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"$files": {"$": {"fields": ["id", "path", "location-id", "url"]}}}),
        )
        .await;
    assert!(used_root_frames);
    assert!(files.unwrap().result.to_string().contains("files.test"));

    // A rule that calls ref must stay on the existing snapshot-backed path.
    fixture
        .set_rules(json!({
            "todos": {
                "allow": {"view": "auth.id == data.ownerId && data.ref('owner.id') == []"}
            },
            "messages": {"allow": {"view": "auth.id in data.memberIds"}},
            "rooms": {"allow": {"view": "true"}},
            "people": {"allow": {"view": "true"}},
            "$files": {"allow": {"view": "true"}}
        }))
        .await;
    let (_, _, used_root_frames) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {}}),
        )
        .await;
    assert!(
        !used_root_frames,
        "a ref-calling rule must keep the existing path"
    );

    // Deferred query rate limits likewise keep the existing path.
    fixture
        .set_rules(json!({
            "$rateLimits": {"reads": {"limits": [{"capacity": 100}]}},
            "todos": {
                "allow": {
                    "view": "rateLimit.reads.limit('all', 1) && auth.id == data.ownerId"
                }
            },
            "messages": {"allow": {"view": "auth.id in data.memberIds"}},
            "rooms": {"allow": {"view": "true"}},
            "people": {"allow": {"view": "true"}},
            "$files": {"allow": {"view": "true"}}
        }))
        .await;
    let (rate_limited, _, used_root_frames) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {}}),
        )
        .await;
    assert!(
        !used_root_frames,
        "a query with deferred rate limits must keep the existing path"
    );
    assert!(rate_limited.is_ok());

    // Malformed rules are reported identically and never crash the frame path.
    fixture
        .set_rules(json!({"todos": {"allow": {"view": "auth."}}}))
        .await;
    let (existing, production, used_root_frames) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {}}),
        )
        .await;
    assert!(!used_root_frames);
    assert!(existing.is_err() && production.is_err());

    fixture.close().await;
}

#[tokio::test]
async fn shared_root_frames_differential_matches_on_miss_and_hit() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (caches, service) = fixture.shared_service();
    let users = [
        ("alice", AdminActor::RefreshToken(fixture.token_a)),
        ("bob", AdminActor::RefreshToken(fixture.token_b)),
        ("guest", AdminActor::Guest),
    ];
    for (name, actor) in users {
        for query in [
            json!({"todos": {}}),
            json!({"messages": {}}),
            json!({"todos": {}, "messages": {}, "rooms": {}}),
            json!({"todos": {"$": {"fields": ["id", "text", "secret", "ownerId"]}}}),
            json!({"todos": {"$": {"limit": 2, "order": {"createdAt": "asc"}}}}),
            json!({"rooms": {"messages": {}}, "todos": {"owner": {}}}),
            json!({"messages": {"$": {"fields": ["id", "body", "memberIds"]}}}),
        ] {
            // The differential helper asserts the existing and frame paths
            // agree for this call. Run every case twice: the first frame path
            // is a miss and the second must reuse the shared frame.
            let (first, _, first_used_frames) = fixture
                .differential(&service, actor.clone(), query.clone())
                .await;
            assert!(first_used_frames, "{name} should use frames");
            assert!(first.is_ok(), "{name} query failed: {first:?}");
            let before = service
                .root_frame_cache_stats(fixture.app)
                .expect("shared frame registry is configured");
            let (_, _, used_root_frames) = fixture
                .differential(&service, actor.clone(), query.clone())
                .await;
            assert!(used_root_frames, "{name} should use frames");
            let after = service
                .root_frame_cache_stats(fixture.app)
                .expect("shared frame registry is configured");
            assert!(
                after.hits > before.hits,
                "{name} second query did not hit the frame cache for {query}"
            );
        }
    }
    let stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert!(stats.loads > 0);
    assert!(stats.hits > 0);

    // A data WAL retirement must still produce matching responses.
    let new_message = Uuid::new_v4();
    fixture
        .insert(new_message, "messages", "id", json!(new_message))
        .await;
    fixture
        .insert(new_message, "messages", "body", json!("after-write"))
        .await;
    fixture
        .insert(
            new_message,
            "messages",
            "memberIds",
            json!([fixture.user_a]),
        )
        .await;
    caches.retire(fixture.app);
    for actor in [
        AdminActor::RefreshToken(fixture.token_a),
        AdminActor::RefreshToken(fixture.token_b),
        AdminActor::Guest,
    ] {
        let (result, _, used_root_frames) = fixture
            .differential(&service, actor, json!({"messages": {}}))
            .await;
        assert!(used_root_frames);
        assert!(result.is_ok());
    }
    let loads_before_ineligible = service.root_frame_cache_stats(fixture.app).unwrap().loads;

    // Callback-capable rules and deferred limits stay on the existing path and
    // never populate or read the shared frame cache.
    fixture
        .set_rules(json!({
            "todos": {
                "allow": {"view": "auth.id == data.ownerId && data.ref('owner.id') == []"}
            },
            "messages": {"allow": {"view": "auth.id in data.memberIds"}},
            "rooms": {"allow": {"view": "true"}},
            "people": {"allow": {"view": "true"}},
            "$files": {"allow": {"view": "true"}}
        }))
        .await;
    for _ in 0..2 {
        let (result, _, used_root_frames) = fixture
            .differential(
                &service,
                AdminActor::RefreshToken(fixture.token_a),
                json!({"todos": {}}),
            )
            .await;
        assert!(!used_root_frames);
        assert!(result.is_ok());
    }
    fixture
        .set_rules(json!({
            "$rateLimits": {"reads": {"limits": [{"capacity": 100}]}},
            "todos": {
                "allow": {
                    "view": "rateLimit.reads.limit('all', 1) && auth.id == data.ownerId"
                }
            },
            "messages": {"allow": {"view": "auth.id in data.memberIds"}},
            "rooms": {"allow": {"view": "true"}},
            "people": {"allow": {"view": "true"}},
            "$files": {"allow": {"view": "true"}}
        }))
        .await;
    let (rate_limited, _, used_root_frames) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {}}),
        )
        .await;
    assert!(!used_root_frames);
    assert!(rate_limited.is_ok());
    assert_eq!(
        service.root_frame_cache_stats(fixture.app).unwrap().loads,
        loads_before_ineligible
    );

    fixture
        .set_rules(json!({"todos": {"allow": {"view": "auth."}}}))
        .await;
    let (existing, production, used_root_frames) = fixture
        .differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"todos": {}}),
        )
        .await;
    assert!(!used_root_frames);
    assert!(existing.is_err() && production.is_err());
    assert_eq!(
        service.root_frame_cache_stats(fixture.app).unwrap().loads,
        loads_before_ineligible
    );

    fixture.close().await;
}

#[tokio::test]
async fn shared_root_frames_do_not_leak_rows_between_users() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (caches, service) = fixture.shared_service();
    let alice = AdminActor::RefreshToken(fixture.token_a);
    let bob = AdminActor::RefreshToken(fixture.token_b);

    let (alice_first, _, used_root_frames) = fixture
        .differential(&service, alice.clone(), json!({"messages": {}}))
        .await;
    assert!(used_root_frames);
    assert_eq!(
        fixture.message_bodies(&alice_first.unwrap()),
        vec!["alice-only".to_owned(), "both".to_owned()]
    );
    let first_stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(first_stats.loads, 1);
    assert_eq!(first_stats.misses, 1);
    assert_eq!(first_stats.hits, 0);

    let (bob_first, _, _) = fixture
        .differential(&service, bob.clone(), json!({"messages": {}}))
        .await;
    assert_eq!(
        fixture.message_bodies(&bob_first.unwrap()),
        vec!["bob-only".to_owned(), "both".to_owned()]
    );
    let hit_stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(
        hit_stats.loads, 1,
        "the second user must reuse the first user's frame"
    );
    assert_eq!(hit_stats.hits, 1);

    // A write that changes visibility is picked up after WAL retirement.
    let new_message = Uuid::new_v4();
    for (label, value) in [
        ("id", json!(new_message)),
        ("body", json!("alice-new")),
        ("memberIds", json!([fixture.user_a])),
    ] {
        fixture.insert(new_message, "messages", label, value).await;
    }
    caches.retire(fixture.app);

    let (alice_after_write, _, _) = fixture
        .differential(&service, alice.clone(), json!({"messages": {}}))
        .await;
    assert_eq!(
        fixture.message_bodies(&alice_after_write.unwrap()),
        vec![
            "alice-new".to_owned(),
            "alice-only".to_owned(),
            "both".to_owned()
        ]
    );
    let retired_stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(retired_stats.loads, 2);
    assert_eq!(retired_stats.misses, 2);
    let (bob_after_write, _, _) = fixture
        .differential(&service, bob.clone(), json!({"messages": {}}))
        .await;
    assert_eq!(
        fixture.message_bodies(&bob_after_write.unwrap()),
        vec!["bob-only".to_owned(), "both".to_owned()]
    );
    let after_write_stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(after_write_stats.loads, 2);
    assert_eq!(after_write_stats.hits, 2);

    // A security change clears the frame cache; the next load sees the new
    // rules and applies them per user to the same shared frame.
    fixture
        .set_rules(json!({
            "todos": {
                "allow": {"view": "auth.id == data.ownerId && data.text != 'blocked'"},
                "fields": {"secret": "data.secret != 'hidden'"}
            },
            "messages": {
                "allow": {"view": "auth.id in data.memberIds && size(data.memberIds) > 1"}
            },
            "rooms": {"allow": {"view": "true"}},
            "people": {"allow": {"view": "true"}},
            "$files": {"allow": {"view": "true"}}
        }))
        .await;
    caches.clear(fixture.app);
    let (alice_after_rules, _, _) = fixture
        .differential(&service, alice.clone(), json!({"messages": {}}))
        .await;
    assert_eq!(
        fixture.message_bodies(&alice_after_rules.unwrap()),
        vec!["both".to_owned()]
    );
    let cleared_stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(cleared_stats.loads, 3);
    assert_eq!(cleared_stats.misses, 3);
    let (bob_after_rules, _, _) = fixture
        .differential(&service, bob, json!({"messages": {}}))
        .await;
    assert_eq!(
        fixture.message_bodies(&bob_after_rules.unwrap()),
        vec!["both".to_owned()]
    );
    assert_eq!(service.root_frame_cache_stats(fixture.app).unwrap().hits, 3);

    fixture.close().await;
}

#[tokio::test]
async fn concurrent_identical_roots_share_one_frame_load() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_caches, service) = fixture.shared_service();
    let (first, second) = tokio::join!(
        fixture.differential(
            &service,
            AdminActor::RefreshToken(fixture.token_a),
            json!({"messages": {}}),
        ),
        fixture.differential(
            &service,
            AdminActor::RefreshToken(fixture.token_b),
            json!({"messages": {}}),
        ),
    );
    assert_eq!(
        fixture.message_bodies(&first.0.unwrap()),
        vec!["alice-only".to_owned(), "both".to_owned()]
    );
    assert_eq!(
        fixture.message_bodies(&second.0.unwrap()),
        vec!["bob-only".to_owned(), "both".to_owned()]
    );
    let stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(stats.loads, 1, "concurrent sessions must share one load");
    assert_eq!(stats.hits + stats.misses, 2);

    fixture.close().await;
}

#[tokio::test]
async fn cancelled_shared_frame_load_falls_back_to_a_private_load() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (caches, service) = fixture.shared_service();
    // A guest needs no refresh-token lookup, which would read the locked table
    // before the frame load starts.
    let query = json!({"rooms": {}});
    // Also warms the pooled connection's cached statements, so below only the
    // frame load itself reads the locked table.
    let (expected, _, used_root_frames) = fixture
        .differential(&service, AdminActor::Guest, query.clone())
        .await;
    assert!(used_root_frames);
    let expected = expected.unwrap();
    let rooms = serde_json::to_string(&expected.result).unwrap();
    assert!(
        rooms.contains(&fixture.room_a.to_string()) && rooms.contains(&fixture.room_b.to_string())
    );
    caches.clear(fixture.app);

    // Hold the shared load behind a table lock, cancel it as a security clear
    // or cluster-lag recovery does, then let the refresh finish.
    fixture
        .writer
        .batch_execute("BEGIN; LOCK TABLE triples IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let refresh = Box::pin(service.refresh_native_framework_query(
        context(fixture.app, AdminActor::Guest),
        query.as_object().unwrap().clone(),
        &fixture.catalog,
        true,
    ));
    let cancel = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let loading = fixture.frame_stats(&service).2 == 2;
            let blocked: i64 = fixture
                .writer
                .query_one(
                    "SELECT count(*) FROM pg_locks \
                     WHERE NOT granted AND relation = 'triples'::regclass",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            if loading && blocked > 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the shared frame load never blocked on the lock"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        caches.clear(fixture.app);
        fixture.writer.batch_execute("COMMIT").await.unwrap();
    };
    let ((result, used_root_frames), ()) = tokio::join!(refresh, cancel);
    assert!(used_root_frames);
    assert_eq!(result, Ok(expected));
    let stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(
        stats.loads, 2,
        "the fallback must not start another shared load"
    );
    assert_eq!(stats.entries, 0, "the private load must not be published");

    fixture.close().await;
}

#[tokio::test]
async fn disabling_shared_root_frames_keeps_frames_without_cache_use() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (caches, service) = fixture.shared_service();
    let service = service.with_query_preparation_policy(Arc::new(DisableSharedFrames));
    for _ in 0..2 {
        let (result, _, used_root_frames) = fixture
            .differential(
                &service,
                AdminActor::RefreshToken(fixture.token_a),
                json!({"messages": {}}),
            )
            .await;
        assert!(used_root_frames);
        assert_eq!(
            fixture.message_bodies(&result.unwrap()),
            vec!["alice-only".to_owned(), "both".to_owned()]
        );
    }
    assert_eq!(
        service.root_frame_cache_stats(fixture.app),
        Some(instant_server::RootFrameCacheStats::default()),
        "the rollback toggle must not touch the frame cache"
    );
    assert_eq!(
        caches.totals(),
        instant_server::RootFrameCacheStats::default()
    );

    fixture.close().await;
}

fn room_query(room: Uuid) -> Value {
    json!({"messages": {"$": {"where": {"roomId": room}}}})
}

/// One WAL topic for a changed triple. Indexes are broad on purpose: a wider
/// topic can only retire more frames, never keep a stale one.
fn wal_topic(entity: Uuid, attr: Uuid, values: Vec<Value>) -> instant_wal::Topic {
    instant_wal::Topic {
        indexes: vec![
            instant_datalog::IndexKey::Ea,
            instant_datalog::IndexKey::Eav,
            instant_datalog::IndexKey::Av,
            instant_datalog::IndexKey::Ave,
            instant_datalog::IndexKey::Vae,
        ],
        entities: instant_wal::MatchSet::Values(vec![entity]),
        attributes: instant_wal::MatchSet::Values(vec![attr]),
        values: instant_wal::MatchSet::Values(
            values
                .into_iter()
                .map(instant_wal::TopicValue::Json)
                .collect(),
        ),
    }
}

/// Topic-precise retirement keeps unrelated rooms reusable after every kind of
/// data write, while still picking up rows, moves and visibility changes.
#[tokio::test]
async fn shared_root_frames_retire_only_the_rooms_a_wal_can_change() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // Keep the large scenario future on the heap instead of the test stack.
    Box::pin(room_retirement_scenario(fixture)).await;
}

#[allow(clippy::too_many_lines)]
async fn room_retirement_scenario(fixture: Fixture) {
    let (caches, service) = fixture.shared_service();
    let alice = AdminActor::RefreshToken(fixture.token_a);
    let bob = AdminActor::RefreshToken(fixture.token_b);
    let room_a_query = room_query(fixture.room_a);
    let room_b_query = room_query(fixture.room_b);

    // Prime one frame per room. Alice's room A is message_a + message_both;
    // bob's room B is message_b.
    let (loaded, _, used_root_frames) = fixture
        .differential(&service, alice.clone(), room_a_query.clone())
        .await;
    assert!(used_root_frames);
    assert_eq!(
        fixture.message_bodies(&loaded.unwrap()),
        vec!["alice-only".to_owned(), "both".to_owned()]
    );
    let (loaded, _, used_root_frames) = fixture
        .differential(&service, bob.clone(), room_b_query.clone())
        .await;
    assert!(used_root_frames);
    assert_eq!(
        fixture.message_bodies(&loaded.unwrap()),
        vec!["bob-only".to_owned()]
    );
    assert_eq!(fixture.frame_stats(&service), (0, 2, 2));

    // A new message in room A retires room A only.
    let new_message = Uuid::new_v4();
    let new_body = Value::from("alice-new");
    let new_members = json!([fixture.user_a]);
    for (label, value) in [
        ("id", json!(new_message)),
        ("body", new_body.clone()),
        ("roomId", json!(fixture.room_a)),
        ("memberIds", new_members.clone()),
    ] {
        fixture.insert(new_message, "messages", label, value).await;
    }
    caches.retire_wal(
        fixture.app,
        &[
            wal_topic(
                new_message,
                fixture.catalog.forward("messages", "id").unwrap().id,
                vec![json!(new_message)],
            ),
            wal_topic(
                new_message,
                fixture.catalog.forward("messages", "body").unwrap().id,
                vec![new_body.clone()],
            ),
            wal_topic(
                new_message,
                fixture.catalog.forward("messages", "roomId").unwrap().id,
                vec![json!(fixture.room_a)],
            ),
            wal_topic(
                new_message,
                fixture.catalog.forward("messages", "memberIds").unwrap().id,
                vec![new_members.clone()],
            ),
        ],
    );
    let before = fixture.frame_stats(&service);
    let (after_insert, _, _) = fixture
        .differential(&service, alice.clone(), room_a_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&after_insert.unwrap()),
        vec![
            "alice-new".to_owned(),
            "alice-only".to_owned(),
            "both".to_owned()
        ]
    );
    let after_room_a = fixture.frame_stats(&service);
    assert_eq!(after_room_a, (before.0, before.1 + 1, before.2 + 1));
    let (untouched, _, _) = fixture
        .differential(&service, bob.clone(), room_b_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&untouched.unwrap()),
        vec!["bob-only".to_owned()]
    );
    let after_room_b = fixture.frame_stats(&service);
    assert_eq!(
        after_room_b,
        (after_room_a.0 + 1, after_room_a.1, after_room_a.2),
        "room B's frame must survive a room A write"
    );

    // Updating text in room B retires room B only.
    let edited_body = Value::from("bob-edited");
    fixture
        .update(fixture.message_b, "messages", "body", edited_body.clone())
        .await;
    caches.retire_wal(
        fixture.app,
        &[wal_topic(
            fixture.message_b,
            fixture.catalog.forward("messages", "body").unwrap().id,
            vec![edited_body.clone(), json!("bob-only")],
        )],
    );
    let before = fixture.frame_stats(&service);
    let (after_edit, _, _) = fixture
        .differential(&service, bob.clone(), room_b_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&after_edit.unwrap()),
        vec!["bob-edited".to_owned()]
    );
    let after_room_b = fixture.frame_stats(&service);
    assert_eq!(after_room_b, (before.0, before.1 + 1, before.2 + 1));
    let (still_cached, _, _) = fixture
        .differential(&service, alice.clone(), room_a_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&still_cached.unwrap()),
        vec![
            "alice-new".to_owned(),
            "alice-only".to_owned(),
            "both".to_owned()
        ]
    );
    let after_room_a = fixture.frame_stats(&service);
    assert_eq!(
        after_room_a,
        (after_room_b.0 + 1, after_room_b.1, after_room_b.2),
        "room A's frame must survive a room B write"
    );

    // Moving message_a from room A to room B retires both rooms.
    fixture
        .update(
            fixture.message_a,
            "messages",
            "roomId",
            json!(fixture.room_b),
        )
        .await;
    caches.retire_wal(
        fixture.app,
        &[wal_topic(
            fixture.message_a,
            fixture.catalog.forward("messages", "roomId").unwrap().id,
            vec![json!(fixture.room_b), json!(fixture.room_a)],
        )],
    );
    let before = fixture.frame_stats(&service);
    let (moved_a, _, _) = fixture
        .differential(&service, alice.clone(), room_a_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&moved_a.unwrap()),
        vec!["alice-new".to_owned(), "both".to_owned()]
    );
    let (moved_b, _, _) = fixture
        .differential(&service, bob.clone(), room_b_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&moved_b.unwrap()),
        vec!["bob-edited".to_owned()]
    );
    let after_move = fixture.frame_stats(&service);
    assert_eq!(
        after_move,
        (before.0, before.1 + 2, before.2 + 2),
        "a move must retire the source and destination frames"
    );
    let (alice_moved_b, _, _) = fixture
        .differential(&service, alice.clone(), room_b_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&alice_moved_b.unwrap()),
        vec!["alice-only".to_owned()]
    );

    // A todos write for alice retires her todos frame and leaves messages alone.
    let todo = Uuid::new_v4();
    let todo_text = Value::from("new-todo");
    let mut todo_topics = Vec::new();
    for (label, value) in [
        ("id", json!(todo)),
        ("text", todo_text.clone()),
        ("ownerId", json!(fixture.user_a)),
        ("secret", json!("visible")),
        ("createdAt", json!(9)),
    ] {
        fixture.insert(todo, "todos", label, value.clone()).await;
        todo_topics.push(wal_topic(
            todo,
            fixture.catalog.forward("todos", label).unwrap().id,
            vec![value],
        ));
    }
    caches.retire_wal(fixture.app, &todo_topics);
    let before = fixture.frame_stats(&service);
    let (todos, _, used_root_frames) = fixture
        .differential(&service, alice.clone(), json!({"todos": {}}))
        .await;
    assert!(used_root_frames);
    assert!(todos.unwrap().result.to_string().contains("new-todo"));
    let after_todos = fixture.frame_stats(&service);
    assert_eq!(after_todos.2, before.2 + 1, "the todos frame must reload");
    let (room_a_kept, _, _) = fixture
        .differential(&service, alice.clone(), room_a_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&room_a_kept.unwrap()),
        vec!["alice-new".to_owned(), "both".to_owned()]
    );
    let (room_b_kept, _, _) = fixture
        .differential(&service, bob.clone(), room_b_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&room_b_kept.unwrap()),
        vec!["bob-edited".to_owned()]
    );
    let after_messages = fixture.frame_stats(&service);
    assert_eq!(
        after_messages,
        (after_todos.0 + 2, after_todos.1, after_todos.2),
        "a todos write must not retire message frames"
    );

    // Changing memberIds retires the frame carrying that message; alice loses
    // the row while bob still sees it from the same shared frame.
    fixture
        .update(
            fixture.message_both,
            "messages",
            "memberIds",
            json!([fixture.user_b]),
        )
        .await;
    caches.retire_wal(
        fixture.app,
        &[wal_topic(
            fixture.message_both,
            fixture.catalog.forward("messages", "memberIds").unwrap().id,
            vec![
                json!([fixture.user_b]),
                json!([fixture.user_a, fixture.user_b]),
            ],
        )],
    );
    let before = fixture.frame_stats(&service);
    let (hidden, _, _) = fixture
        .differential(&service, alice.clone(), room_a_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&hidden.unwrap()),
        vec!["alice-new".to_owned()],
        "alice lost access to message_both but keeps her own message"
    );
    assert_eq!(
        fixture.frame_stats(&service),
        (before.0, before.1 + 1, before.2 + 1)
    );
    let before = fixture.frame_stats(&service);
    let (visible, _, _) = fixture
        .differential(&service, bob.clone(), room_a_query.clone())
        .await;
    assert_eq!(
        fixture.message_bodies(&visible.unwrap()),
        vec!["both".to_owned()],
        "bob still has access from the same frame"
    );
    assert_eq!(
        fixture.frame_stats(&service),
        (before.0 + 1, before.1, before.2),
        "the refreshed frame is shared with bob"
    );
    let (room_b_visible, _, _) = fixture.differential(&service, bob, room_b_query).await;
    assert_eq!(
        fixture.message_bodies(&room_b_visible.unwrap()),
        vec!["bob-edited".to_owned()]
    );

    fixture.close().await;
}

/// Listener scans and initial subscriptions both run on the interactive
/// workload. They may use the per-request frame split, but must not create
/// shared entries that no later refresh will look up.
#[tokio::test]
async fn initial_add_query_does_not_populate_the_shared_frame_cache() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (_caches, service) = fixture.shared_service();
    let alice = AdminActor::RefreshToken(fixture.token_a);
    let query = room_query(fixture.room_a);

    let (initial, _, used_root_frames) = fixture
        .differential_initial(&service, alice.clone(), query.clone())
        .await;
    assert!(used_root_frames, "initial add-query should use frames");
    assert_eq!(
        fixture.message_bodies(&initial.unwrap()),
        vec!["alice-only".to_owned(), "both".to_owned()]
    );
    let initial_stats = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(
        (
            initial_stats.hits,
            initial_stats.misses,
            initial_stats.loads,
            initial_stats.entries
        ),
        (0, 0, 0, 0),
        "an initial add-query must not touch the shared frame cache"
    );

    let (refreshed, _, used_root_frames) = fixture
        .differential(&service, alice.clone(), query.clone())
        .await;
    assert!(used_root_frames);
    assert_eq!(
        fixture.message_bodies(&refreshed.unwrap()),
        vec!["alice-only".to_owned(), "both".to_owned()]
    );
    let after_refresh = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(
        (after_refresh.misses, after_refresh.loads),
        (1, 1),
        "a refresh populates the shared cache"
    );
    assert_eq!(after_refresh.entries, 1);

    let (hit, _, _) = fixture.differential(&service, alice, query).await;
    assert_eq!(
        fixture.message_bodies(&hit.unwrap()),
        vec!["alice-only".to_owned(), "both".to_owned()]
    );
    let after_hit = service.root_frame_cache_stats(fixture.app).unwrap();
    assert_eq!(
        (after_hit.hits, after_hit.misses, after_hit.loads),
        (1, 1, 1),
        "the second refresh reuses the frame"
    );

    fixture.close().await;
}

/// A coarse-selected refresh batch must keep using the per-request frame split
/// without reading or populating the shared frame cache: its queries were
/// selected by coarse topics, so the triggering WAL may not have retired a
/// precise frame it would otherwise reuse.
#[tokio::test]
async fn coarse_refresh_keeps_frames_without_shared_cache_use() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (caches, service) = fixture.shared_service();
    let alice = AdminActor::RefreshToken(fixture.token_a);
    let query = room_query(fixture.room_a);

    let (result, _, used_root_frames) = fixture.differential_coarse(&service, alice, query).await;
    assert!(
        used_root_frames,
        "a coarse refresh should still use the per-request frame split"
    );
    assert_eq!(
        fixture.message_bodies(&result.unwrap()),
        vec!["alice-only".to_owned(), "both".to_owned()]
    );
    assert_eq!(
        service.root_frame_cache_stats(fixture.app),
        Some(instant_server::RootFrameCacheStats::default()),
        "a coarse refresh must not read or populate shared frames"
    );
    assert_eq!(
        caches.totals(),
        instant_server::RootFrameCacheStats::default()
    );

    fixture.close().await;
}

const TEST_SCHEMA: &str =
    "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TYPE pg_temp.topics_idx AS ENUM ('any', 'ea', 'eav', 'av', 'ave', 'vae');
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               status text NOT NULL DEFAULT 'active',
               deletion_marked_at timestamptz,
               connection_string text
             );
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE attrs (
               id uuid NOT NULL,
               app_id uuid NOT NULL,
               value_type text NOT NULL,
               cardinality text NOT NULL,
               is_unique boolean NOT NULL,
               is_indexed boolean NOT NULL,
               is_required boolean,
               forward_ident uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               reverse_ident uuid,
               reverse_etype text,
               reverse_label text,
               on_delete attr_on_delete,
               on_delete_reverse attr_on_delete,
               checked_data_type checked_data_type,
               setting_unique boolean,
               indexing boolean,
               checking_data_type boolean,
               inferred_types bit(32),
               metadata jsonb,
               deletion_marked_at timestamptz,
               PRIMARY KEY (app_id, id)
             );
             CREATE UNIQUE INDEX server_perms_attrs_id ON attrs(id);
             CREATE TEMP TABLE rules (app_id uuid PRIMARY KEY, code jsonb NOT NULL);
             CREATE TEMP TABLE app_upload_urls (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               path text NOT NULL,
               created_at timestamp NOT NULL DEFAULT now(),
               expired_at timestamp NOT NULL DEFAULT (now() + interval '5 minutes')
             );
             CREATE TEMP TABLE sync_subs (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               query text NOT NULL,
               token_hash bytea,
               is_admin boolean NOT NULL,
               user_id uuid,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE sync_sub_topics (
               sync_sub_id uuid NOT NULL REFERENCES sync_subs(id) ON DELETE CASCADE,
               topic_num integer NOT NULL,
               idx topics_idx[] NOT NULL,
               e bytea[] NOT NULL,
               a bytea[] NOT NULL,
               v jsonb[] NOT NULL,
               v_filter jsonb,
               PRIMARY KEY(sync_sub_id, topic_num)
             );
             CREATE TEMP TABLE rust_realtime_stream_buffers (
               app_id uuid NOT NULL,
               stream_id uuid NOT NULL,
               machine_id uuid NOT NULL,
               content text NOT NULL DEFAULT '',
               size bigint NOT NULL DEFAULT 0,
               created_at timestamptz NOT NULL DEFAULT now(),
               updated_at timestamptz NOT NULL DEFAULT now(),
               PRIMARY KEY(app_id, stream_id)
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
               created_at bigint NOT NULL DEFAULT ((extract(epoch from clock_timestamp()) * 1000)::bigint),
               PRIMARY KEY (app_id, entity_id, attr_id, value_md5)
             );
             CREATE UNIQUE INDEX server_perms_ea ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE OR REPLACE FUNCTION public.json_null_to_null(v jsonb)
             RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
               SELECT CASE WHEN v = 'null'::jsonb THEN NULL ELSE v END
             $$;
             CREATE UNIQUE INDEX server_perms_av
               ON triples(app_id, attr_id, public.json_null_to_null(value)) WHERE av;
             CREATE OR REPLACE FUNCTION public.jsonb_deep_merge(obj1 jsonb, obj2 jsonb)
             RETURNS jsonb LANGUAGE plpgsql IMMUTABLE AS $$
             DECLARE key text; item jsonb;
             BEGIN
               IF obj1 IS NULL OR obj2 IS NULL OR jsonb_typeof(obj1) != 'object' OR jsonb_typeof(obj2) != 'object'
               THEN RETURN obj2; END IF;
               FOR key, item IN SELECT * FROM jsonb_each(obj2) LOOP
                 obj1 := CASE
                   WHEN obj1 ? key AND jsonb_typeof(obj1->key) = 'object' AND jsonb_typeof(item) = 'object'
                     THEN jsonb_set(obj1, ARRAY[key], public.jsonb_deep_merge(obj1->key, item))
                   WHEN jsonb_typeof(item) = 'null' THEN obj1 - key
                   ELSE jsonb_set(obj1, ARRAY[key], item)
                 END;
               END LOOP;
               RETURN obj1;
              END $$;";

const TEST_SCHEMA_DDL_LOCK: i64 = 7_843_311_947;

async fn install_shared_schema(database_url: &str) -> String {
    let schema = format!("root_frames_{}", Uuid::new_v4().simple());
    let (client, connection) = tokio_postgres::connect(database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}, public"
        ))
        .await
        .unwrap();
    let ddl = TEST_SCHEMA
        .replace("pg_temp.", &format!("{schema}."))
        .replace("CREATE TEMP TABLE", "CREATE TABLE");
    client
        .batch_execute(&format!(
            "BEGIN; SELECT pg_advisory_xact_lock({TEST_SCHEMA_DDL_LOCK}); {ddl} COMMIT;"
        ))
        .await
        .unwrap();
    drop(client);
    connection_task.await.unwrap().unwrap();
    let mut url = url::Url::parse(database_url).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema},public"));
    url.into()
}

async fn insert_catalog_attrs(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    catalog: &AttributeCatalog,
) {
    for attribute in &catalog.attributes {
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        let cardinality = match attribute.cardinality {
            Cardinality::One => "one",
            Cardinality::Many => "many",
        };
        client
            .execute(
                "INSERT INTO attrs
                 (id, app_id, value_type, cardinality, is_unique, is_indexed,
                  forward_ident, etype, label)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                 ON CONFLICT DO NOTHING",
                &[
                    &attribute.id,
                    &app_id,
                    &value_type,
                    &cardinality,
                    &attribute.unique,
                    &attribute.indexed,
                    &Uuid::new_v4(),
                    &attribute.etype,
                    &attribute.label,
                ],
            )
            .await
            .unwrap();
    }
}
