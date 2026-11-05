//! Live checks that projected query-permission loads read only the attributes
//! a view rule can observe and keep end-to-end decisions identical.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use instant_datalog::{DatalogResult, DatalogTriple, DatalogValue};
use instant_instaql::{
    Attribute, AttributeCatalog, AttributeValueType, Cardinality, CompiledQuery, ExecutedEntity,
    ExecutedNode, compile_query,
};
use instant_permissions::{
    EvaluationBindings, PermissionEntityKey, PermissionEntityLoad, PostgresPermissionDataLoader,
    QueryPermissionOptions, RefCache, RuleSet, filter_executed_query_owned,
};
use serde_json::{Value, json};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, IsolationLevel, NoTls, Statement};
use uuid::Uuid;

fn attribute(id: u128, etype: &str, label: &str) -> Attribute {
    Attribute {
        id: Uuid::from_u128(id),
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique: label == "id",
        setting_unique: false,
        indexed: label == "id",
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

struct Fixture {
    client: Client,
    driver: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
    catalog: AttributeCatalog,
    app: Uuid,
    first: Uuid,
    second: Uuid,
    bare: Uuid,
    owner: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let Some(url) =
            instant_test_support::test_database_url("live_permission_projection_fixture")
        else {
            return None;
        };
        let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
        let driver = tokio::spawn(connection);
        client.batch_execute("CREATE TEMP TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL, created_at bigint NOT NULL)").await.unwrap();
        let fixture = Self {
            client,
            driver,
            catalog: AttributeCatalog {
                attributes: vec![
                    attribute(1, "posts", "id"),
                    attribute(2, "posts", "title"),
                    attribute(3, "posts", "secret"),
                    attribute(4, "posts", "owner"),
                    attribute(5, "messages", "id"),
                    attribute(6, "messages", "body"),
                    attribute(7, "messages", "memberIds"),
                ],
            },
            app: Uuid::new_v4(),
            first: Uuid::new_v4(),
            second: Uuid::new_v4(),
            bare: Uuid::new_v4(),
            owner: "user-a".to_owned(),
        };
        for (entity, id, title, secret, owner, at) in [
            (
                fixture.first,
                json!(fixture.first),
                json!("first"),
                json!("s1"),
                json!(fixture.owner),
                1,
            ),
            (
                fixture.second,
                json!(fixture.second),
                json!("second"),
                json!("s2"),
                json!("user-b"),
                2,
            ),
        ] {
            fixture.insert(entity, "id", id, at).await;
            fixture.insert(entity, "title", title, at).await;
            fixture.insert(entity, "secret", secret, at).await;
            fixture.insert(entity, "owner", owner, at).await;
        }
        // No projected attribute (`id`/`owner`) exists for this entity.
        fixture
            .insert(fixture.bare, "title", json!("bare"), 3)
            .await;
        fixture.insert(fixture.bare, "secret", json!("s3"), 3).await;
        Some(fixture)
    }

    async fn insert(&self, entity: Uuid, label: &str, value: Value, at: i64) {
        self.insert_etype("posts", entity, label, value, at).await;
    }

    async fn insert_etype(&self, etype: &str, entity: Uuid, label: &str, value: Value, at: i64) {
        self.client
            .execute(
                "INSERT INTO triples VALUES ($1,$2,$3,$4,$5)",
                &[
                    &self.app,
                    &entity,
                    &self.catalog.forward(etype, label).unwrap().id,
                    &Json(value),
                    &at,
                ],
            )
            .await
            .unwrap();
    }

    async fn executed(&self) -> (CompiledQuery, Vec<ExecutedNode>) {
        self.executed_roots(&["posts"]).await
    }

    async fn executed_roots(&self, roots: &[&str]) -> (CompiledQuery, Vec<ExecutedNode>) {
        let mut query_value = serde_json::Map::new();
        for root in roots {
            query_value.insert((*root).to_owned(), json!({}));
        }
        let query = compile_query(&Value::Object(query_value), &self.catalog, false).unwrap();
        let rows = self
            .client
            .query(
                "SELECT entity_id, attr_id, value, created_at FROM triples WHERE app_id=$1 ORDER BY created_at, attr_id",
                &[&self.app],
            )
            .await
            .unwrap();
        let mut by_etype = HashMap::<String, ExecutedNode>::new();
        for root in roots {
            let attribute_ids = self
                .catalog
                .attributes
                .iter()
                .filter(|attribute| attribute.etype == *root)
                .map(|attribute| attribute.id)
                .collect::<HashSet<_>>();
            let mut by_entity = HashMap::<Uuid, Vec<DatalogTriple>>::new();
            for row in &rows {
                let attr_id: Uuid = row.get("attr_id");
                if !attribute_ids.contains(&attr_id) {
                    continue;
                }
                let entity_id: Uuid = row.get("entity_id");
                let Json(value): Json<Value> = row.get("value");
                by_entity.entry(entity_id).or_default().push(DatalogTriple {
                    entity_id,
                    attr_id,
                    value: DatalogValue::Json(value),
                    created_at: row.get("created_at"),
                });
            }
            let mut entities = by_entity
                .into_iter()
                .map(|(_, triples)| ExecutedEntity {
                    fetch: DatalogResult {
                        join_rows: triples.into_iter().map(|triple| vec![triple]).collect(),
                        ..DatalogResult::default()
                    },
                    children: Vec::new(),
                })
                .collect::<Vec<_>>();
            entities.sort_by_key(|entity| entity.fetch.join_rows[0][0].entity_id);
            let selector = DatalogResult {
                join_rows: entities
                    .iter()
                    .map(|entity| entity.fetch.join_rows[0].clone())
                    .collect(),
                ..DatalogResult::default()
            };
            by_etype.insert(
                (*root).to_owned(),
                ExecutedNode {
                    selector,
                    aggregate: None,
                    entities,
                },
            );
        }
        let nodes = query
            .roots
            .iter()
            .map(|node| by_etype.remove(&node.etype).unwrap())
            .collect();
        (query, nodes)
    }

    async fn close(self) {
        drop(self.client);
        self.driver.await.unwrap().unwrap();
    }
}

fn attr(catalog: &AttributeCatalog, label: &str) -> Uuid {
    catalog.forward("posts", label).unwrap().id
}

fn key(entity_id: Uuid) -> PermissionEntityKey {
    key_for("posts", entity_id)
}

fn key_for(etype: &str, entity_id: Uuid) -> PermissionEntityKey {
    PermissionEntityKey {
        etype: etype.to_owned(),
        entity_id,
    }
}

fn projected_load(
    catalog: &AttributeCatalog,
    entities: Vec<PermissionEntityKey>,
) -> PermissionEntityLoad {
    let mut attributes = vec![attr(catalog, "id"), attr(catalog, "owner")];
    attributes.sort_unstable();
    PermissionEntityLoad {
        entities,
        attributes: Some(attributes),
    }
}

#[tokio::test]
async fn projected_loader_reads_only_the_projected_triples_and_keeps_empty_objects() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let transaction = fixture
        .client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: fixture.app,
        catalog: &fixture.catalog,
    };
    let entities = vec![key(fixture.first), key(fixture.second), key(fixture.bare)];
    let loaded = loader
        .load_query_entities(
            &transaction,
            vec![projected_load(&fixture.catalog, entities.clone())],
        )
        .await
        .unwrap();
    let first = loaded[&key(fixture.first)].as_object().unwrap();
    assert_eq!(first.len(), 2, "only id and owner are loaded");
    assert_eq!(first["id"], json!(fixture.first));
    assert_eq!(first["owner"], json!(fixture.owner));
    assert!(!first.contains_key("title"));
    assert!(!first.contains_key("secret"));
    // An entity with no projected attribute still decodes as a JSON object.
    assert_eq!(
        loaded[&key(fixture.bare)],
        json!({}),
        "projected entities with no matching triples stay objects"
    );
    assert_eq!(loaded[&key(fixture.second)]["owner"], json!("user-b"));

    let complete = loader
        .load_query_entities(
            &transaction,
            vec![PermissionEntityLoad {
                entities: vec![key(fixture.first)],
                attributes: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(complete[&key(fixture.first)].as_object().unwrap().len(), 4);

    let statement = transaction
        .prepare_typed(
            instant_permissions::PERMISSION_ENTITY_PROJECTED_SQL,
            instant_permissions::permission_entity_projected_parameter_types(),
        )
        .await
        .unwrap();
    let prepared = instant_permissions::load_permission_entities_projected_with_statement(
        &transaction,
        &statement,
        fixture.app,
        &fixture.catalog,
        entities,
        projected_load(&fixture.catalog, Vec::new())
            .attributes
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(prepared, loaded);
    transaction.rollback().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn query_permission_results_match_with_projection_on_and_off() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let (query, executed) = fixture.executed().await;
    let rules = RuleSet::new(json!({
        "posts": {"allow": {"view": "auth.id != null && auth.id == data.owner"}}
    }));
    let bindings = EvaluationBindings {
        auth: json!({"id": fixture.owner}),
        ..EvaluationBindings::default()
    };
    let mut results = Vec::new();
    for projection in [true, false] {
        let transaction = fixture
            .client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .unwrap();
        let loader = PostgresPermissionDataLoader {
            app_id: fixture.app,
            catalog: &fixture.catalog,
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&observed);
        let filtered = filter_executed_query_owned(
            &rules,
            &query,
            executed.clone(),
            &fixture.catalog,
            &bindings,
            QueryPermissionOptions {
                attribute_projection: projection,
            },
            &mut RefCache::new(),
            |loads| {
                recorded.lock().unwrap().extend(loads.iter().cloned());
                let transaction = &transaction;
                let loader = &loader;
                async move { loader.load_query_entities(transaction, loads).await }
            },
            |_| async { Ok(RefCache::new()) },
        )
        .await
        .unwrap();
        let observed = observed.lock().unwrap().clone();
        assert!(!observed.is_empty());
        if projection {
            assert!(
                observed
                    .iter()
                    .all(|load| load.attributes.as_ref().is_some_and(|ids| {
                        ids.len() == 2
                            && ids.contains(&attr(&fixture.catalog, "id"))
                            && ids.contains(&attr(&fixture.catalog, "owner"))
                    })),
                "projection-on loads only the analyzed attributes"
            );
        } else {
            assert!(
                observed.iter().all(|load| load.attributes.is_none()),
                "projection-off loads complete entities"
            );
        }
        transaction.rollback().await.unwrap();
        assert_eq!(
            filtered[0].entities.len(),
            1,
            "only the owner's entity is visible"
        );
        results.push(filtered);
    }
    assert_eq!(
        results[0], results[1],
        "projection must not change query permission results"
    );
    fixture.close().await;
}

async fn insert_multi_etype_rows(fixture: &Fixture) -> (Uuid, Uuid, Uuid) {
    let shared = Uuid::new_v4();
    let message_one = Uuid::new_v4();
    let message_two = Uuid::new_v4();
    // The same entity id exists as both a post and a message. Each etype's
    // attributes must decode only under that etype's key.
    fixture
        .insert_etype("posts", shared, "id", json!(shared), 10)
        .await;
    fixture
        .insert_etype("posts", shared, "owner", json!(fixture.owner), 10)
        .await;
    fixture
        .insert_etype("posts", shared, "title", json!("shared-post"), 10)
        .await;
    fixture
        .insert_etype("posts", shared, "secret", json!("shared-secret"), 10)
        .await;
    for (entity, member, body) in [
        (message_one, fixture.owner.clone(), "one"),
        (message_two, "user-b".to_owned(), "two"),
        (shared, fixture.owner.clone(), "shared-message"),
    ] {
        fixture
            .insert_etype("messages", entity, "id", json!(entity), 11)
            .await;
        fixture
            .insert_etype("messages", entity, "body", json!(body), 11)
            .await;
        fixture
            .insert_etype("messages", entity, "memberIds", json!([member]), 11)
            .await;
    }
    (shared, message_one, message_two)
}

async fn decoded_per_group(
    transaction: &tokio_postgres::Transaction<'_>,
    complete_statement: &Statement,
    projected_statement: &Statement,
    app: Uuid,
    catalog: &AttributeCatalog,
    loads: &[PermissionEntityLoad],
) -> HashMap<PermissionEntityKey, Value> {
    let mut loaded = HashMap::new();
    for load in loads {
        let decoded = match &load.attributes {
            Some(attributes) => {
                instant_permissions::load_permission_entities_projected_with_statement(
                    transaction,
                    projected_statement,
                    app,
                    catalog,
                    load.entities.clone(),
                    attributes.clone(),
                )
                .await
                .unwrap()
            }
            None => instant_permissions::load_permission_entities_with_statement(
                transaction,
                complete_statement,
                app,
                catalog,
                load.entities.clone(),
            )
            .await
            .unwrap(),
        };
        loaded.extend(decoded);
    }
    loaded
}

#[tokio::test]
async fn merged_multi_etype_projected_loads_match_per_group_decoding() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let (shared, message_one, _message_two) = insert_multi_etype_rows(&fixture).await;
    let (query, executed) = fixture.executed_roots(&["posts", "messages"]).await;
    let rules = RuleSet::new(json!({
        "posts": {"allow": {"view": "auth.id != null && auth.id == data.owner"}},
        "messages": {"allow": {"view": "auth.id != null && auth.id in data.memberIds"}}
    }));
    let bindings = EvaluationBindings {
        auth: json!({"id": fixture.owner}),
        ..EvaluationBindings::default()
    };
    let transaction = fixture
        .client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: fixture.app,
        catalog: &fixture.catalog,
    };
    let observed = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&observed);
    let filtered = filter_executed_query_owned(
        &rules,
        &query,
        executed,
        &fixture.catalog,
        &bindings,
        QueryPermissionOptions::default(),
        &mut RefCache::new(),
        |loads| {
            recorded.lock().unwrap().extend(loads.iter().cloned());
            let transaction = &transaction;
            let loader = &loader;
            async move { loader.load_query_entities(transaction, loads).await }
        },
        |_| async { Ok(RefCache::new()) },
    )
    .await
    .unwrap();
    let loads = observed.lock().unwrap().clone();
    assert_eq!(loads.len(), 2, "one projected group per etype");
    assert!(
        loads
            .iter()
            .all(|load| load.attributes.as_ref().is_some_and(|ids| ids.len() == 2)),
        "both groups keep their analyzed projection"
    );
    let complete_statement = transaction
        .prepare_typed(
            instant_permissions::PERMISSION_ENTITY_SQL,
            instant_permissions::permission_entity_parameter_types(),
        )
        .await
        .unwrap();
    let projected_statement = transaction
        .prepare_typed(
            instant_permissions::PERMISSION_ENTITY_PROJECTED_SQL,
            instant_permissions::permission_entity_projected_parameter_types(),
        )
        .await
        .unwrap();
    let merged = loader
        .load_query_entities(&transaction, loads.clone())
        .await
        .unwrap();
    let per_group = decoded_per_group(
        &transaction,
        &complete_statement,
        &projected_statement,
        fixture.app,
        &fixture.catalog,
        &loads,
    )
    .await;
    assert_eq!(
        merged, per_group,
        "the shared statement must decode exactly like per-group loads"
    );

    let shared_post = key_for("posts", shared);
    let shared_message = key_for("messages", shared);
    assert_eq!(merged[&shared_post]["owner"], json!(fixture.owner));
    assert!(
        !merged[&shared_post]
            .as_object()
            .unwrap()
            .contains_key("memberIds")
    );
    assert_eq!(merged[&shared_message]["memberIds"], json!([fixture.owner]));
    assert!(
        !merged[&shared_message]
            .as_object()
            .unwrap()
            .contains_key("owner")
    );
    assert_eq!(
        merged[&key_for("messages", message_one)]["memberIds"],
        json!([fixture.owner])
    );
    assert!(
        !merged[&key_for("messages", message_one)]
            .as_object()
            .unwrap()
            .contains_key("body"),
        "the rule-proven projection omits unfetched message attributes"
    );
    assert_eq!(
        merged[&key(fixture.bare)],
        json!({}),
        "a projected entity with no projected rows stays an object"
    );
    let posts_index = query
        .roots
        .iter()
        .position(|node| node.etype == "posts")
        .unwrap();
    assert_eq!(
        filtered[posts_index].entities.len(),
        2,
        "the owner's post and shared post are visible"
    );
    transaction.rollback().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn mixed_complete_and_projected_groups_decode_like_per_group_loads() {
    let Some(mut fixture) = Fixture::new().await else {
        return;
    };
    let (shared, _message_one, _message_two) = insert_multi_etype_rows(&fixture).await;
    let (query, executed) = fixture.executed_roots(&["posts", "messages"]).await;
    // A field rule on a fetched post label keeps posts complete while messages
    // stays projected.
    let rules = RuleSet::new(json!({
        "posts": {
            "allow": {"view": "auth.id != null && auth.id == data.owner"},
            "fields": {"title": "auth.admin"}
        },
        "messages": {"allow": {"view": "auth.id != null && auth.id in data.memberIds"}}
    }));
    let bindings = EvaluationBindings {
        auth: json!({"id": fixture.owner, "admin": false}),
        ..EvaluationBindings::default()
    };
    let transaction = fixture
        .client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: fixture.app,
        catalog: &fixture.catalog,
    };
    let observed = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&observed);
    filter_executed_query_owned(
        &rules,
        &query,
        executed,
        &fixture.catalog,
        &bindings,
        QueryPermissionOptions::default(),
        &mut RefCache::new(),
        |loads| {
            recorded.lock().unwrap().extend(loads.iter().cloned());
            let transaction = &transaction;
            let loader = &loader;
            async move { loader.load_query_entities(transaction, loads).await }
        },
        |_| async { Ok(RefCache::new()) },
    )
    .await
    .unwrap();
    let loads = observed.lock().unwrap().clone();
    assert_eq!(loads.len(), 2, "one complete and one projected group");
    assert_eq!(
        loads
            .iter()
            .filter(|load| load.attributes.is_none())
            .count(),
        1
    );
    assert_eq!(
        loads
            .iter()
            .filter(|load| load.attributes.is_some())
            .count(),
        1
    );
    let complete_statement = transaction
        .prepare_typed(
            instant_permissions::PERMISSION_ENTITY_SQL,
            instant_permissions::permission_entity_parameter_types(),
        )
        .await
        .unwrap();
    let projected_statement = transaction
        .prepare_typed(
            instant_permissions::PERMISSION_ENTITY_PROJECTED_SQL,
            instant_permissions::permission_entity_projected_parameter_types(),
        )
        .await
        .unwrap();
    let merged = loader
        .load_query_entities(&transaction, loads.clone())
        .await
        .unwrap();
    let per_group = decoded_per_group(
        &transaction,
        &complete_statement,
        &projected_statement,
        fixture.app,
        &fixture.catalog,
        &loads,
    )
    .await;
    assert_eq!(merged, per_group);
    assert_eq!(
        merged[&key_for("posts", shared)]["title"],
        json!("shared-post"),
        "complete posts keep every attribute"
    );
    assert!(
        !merged[&key_for("messages", shared)]
            .as_object()
            .unwrap()
            .contains_key("title"),
        "projected messages never receive post attributes from the shared statement"
    );
    transaction.rollback().await.unwrap();
    fixture.close().await;
}
