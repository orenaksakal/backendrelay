use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality};
use instant_permissions::{
    PermissionDataLoader, PermissionEntityKey, PostgresPermissionDataLoader, RefEntityId,
    RefRequest,
};
use serde_json::{Value, json};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, IsolationLevel, NoTls};
use uuid::Uuid;

fn attribute(etype: &str, label: &str) -> Attribute {
    Attribute {
        id: Uuid::new_v4(),
        wire: None,
        etype: etype.to_owned(),
        label: label.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique: false,
        setting_unique: false,
        indexed: false,
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
    other_app: Uuid,
    first: Uuid,
    second: Uuid,
    user: Uuid,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let Some(url) = instant_test_support::test_database_url("live_permission_loader_fixture")
        else {
            return None;
        };
        let (client, connection) = tokio_postgres::connect(&url, NoTls).await.unwrap();
        let driver = tokio::spawn(connection);
        client.batch_execute("CREATE TEMP TABLE triples (app_id uuid NOT NULL, entity_id uuid NOT NULL, attr_id uuid NOT NULL, value jsonb NOT NULL, created_at bigint NOT NULL)").await.unwrap();
        let mut owner = attribute("posts", "owner");
        owner.value_type = AttributeValueType::Ref;
        owner.reverse_etype = Some("users".to_owned());
        owner.reverse_label = Some("posts".to_owned());
        let fixture = Self {
            client,
            driver,
            catalog: AttributeCatalog {
                attributes: vec![
                    attribute("posts", "id"),
                    attribute("posts", "title"),
                    attribute("posts", "tags"),
                    attribute("posts", "nullable"),
                    owner,
                    attribute("users", "name"),
                ],
            },
            app: Uuid::new_v4(),
            other_app: Uuid::new_v4(),
            first: Uuid::new_v4(),
            second: Uuid::new_v4(),
            user: Uuid::new_v4(),
        };
        let id = fixture.attr("posts", "id");
        let title = fixture.attr("posts", "title");
        let owner = fixture.attr("posts", "owner");
        let name = fixture.attr("users", "name");
        for (app, text) in [(fixture.app, "own"), (fixture.other_app, "foreign")] {
            fixture
                .insert(app, fixture.first, id, json!(fixture.first), 1)
                .await;
            fixture
                .insert(app, fixture.first, title, json!(format!("{text}-first")), 2)
                .await;
            fixture
                .insert(app, fixture.first, owner, json!(fixture.user), 20)
                .await;
            fixture
                .insert(app, fixture.second, id, json!(fixture.second), 3)
                .await;
            fixture
                .insert(
                    app,
                    fixture.second,
                    title,
                    json!(format!("{text}-second")),
                    4,
                )
                .await;
            fixture
                .insert(app, fixture.second, owner, json!(fixture.user), 10)
                .await;
            fixture
                .insert(app, fixture.user, name, json!(text), 5)
                .await;
        }
        fixture
            .insert(
                fixture.app,
                fixture.first,
                fixture.attr("posts", "tags"),
                json!(["a", null, false]),
                6,
            )
            .await;
        fixture
            .insert(
                fixture.app,
                fixture.first,
                fixture.attr("posts", "nullable"),
                Value::Null,
                7,
            )
            .await;
        fixture
            .insert(fixture.app, fixture.first, name, json!("different-type"), 8)
            .await;
        fixture
            .insert(
                fixture.app,
                fixture.first,
                Uuid::new_v4(),
                json!("unknown-attribute"),
                9,
            )
            .await;
        Some(fixture)
    }

    fn attr(&self, etype: &str, label: &str) -> Uuid {
        self.catalog.forward(etype, label).unwrap().id
    }

    async fn insert(&self, app: Uuid, entity: Uuid, attr: Uuid, value: Value, at: i64) {
        self.client
            .execute(
                "INSERT INTO triples VALUES ($1,$2,$3,$4,$5)",
                &[&app, &entity, &attr, &Json(value), &at],
            )
            .await
            .unwrap();
    }

    async fn close(self) {
        drop(self.client);
        self.driver.await.unwrap().unwrap();
    }
}

fn key(entity_id: Uuid) -> PermissionEntityKey {
    PermissionEntityKey {
        etype: "posts".to_owned(),
        entity_id,
    }
}

fn request(entity: Uuid, etype: &str, path: &str) -> RefRequest {
    RefRequest {
        entity_id: entity.into(),
        etype: etype.to_owned(),
        path: path.to_owned(),
    }
}

#[tokio::test]
async fn bulk_reverse_steps_match_legacy_per_target_reads_with_duplicates_and_tenant_scope() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    let mut members = attribute("groups", "members");
    members.value_type = AttributeValueType::Ref;
    members.cardinality = Cardinality::Many;
    members.reverse_etype = Some("users".to_owned());
    members.reverse_label = Some("groups".to_owned());
    let member_id = members.id;
    f.catalog.attributes.push(members);
    let group = Uuid::new_v4();
    let another_user = Uuid::new_v4();
    let third = Uuid::new_v4();
    let fourth = Uuid::new_v4();
    // The two traversed users' row times deliberately interleave. A global
    // created_at sort, ANY(), or DISTINCT would not match the legacy result.
    for (user, at) in [(another_user, 1), (f.user, 2), (f.user, 3)] {
        f.insert(f.app, group, member_id, json!(user), at).await;
    }
    let owner = f.attr("posts", "owner");
    f.insert(f.app, third, owner, json!(another_user), 15).await;
    f.insert(f.app, fourth, owner, json!(another_user), 5).await;
    f.insert(f.other_app, Uuid::new_v4(), owner, json!(another_user), 0)
        .await;
    f.insert(f.app, Uuid::new_v4(), member_id, json!(another_user), 0)
        .await;
    let reverse = request(group, "groups", "members.posts");
    let tx = f
        .client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    let result = loader.load_refs(&tx, vec![reverse.clone()]).await.unwrap();
    let mut legacy = Vec::<Value>::new();
    for user in [another_user, f.user, f.user] {
        legacy.extend(tx.query(
            "SELECT entity_id FROM triples WHERE app_id=$1 AND attr_id=$2 AND value=$3 ORDER BY created_at, entity_id",
            &[&f.app, &owner, &Json(json!(user))],
        ).await.unwrap().into_iter().map(|row| json!(row.get::<_, Uuid>(0))));
    }
    assert_eq!(
        legacy,
        vec![
            json!(fourth),
            json!(third),
            json!(f.second),
            json!(f.first),
            json!(f.second),
            json!(f.first)
        ]
    );
    assert_eq!(result[&reverse], legacy);
    let foreign = PostgresPermissionDataLoader {
        app_id: f.other_app,
        catalog: &f.catalog,
    };
    assert!(foreign.load_refs(&tx, vec![reverse.clone()]).await.unwrap()[&reverse].is_empty());
    tx.rollback().await.unwrap();
    // Every invocation must read the current rows rather than memoizing the
    // previous path, targets, ordering or permission decisions.
    f.client
        .execute(
            "DELETE FROM triples WHERE app_id=$1 AND entity_id=$2 AND attr_id=$3",
            &[&f.app, &fourth, &owner],
        )
        .await
        .unwrap();
    let tx = f.client.transaction().await.unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    let next = loader.load_refs(&tx, vec![reverse.clone()]).await.unwrap();
    assert_eq!(
        next[&reverse],
        legacy
            .into_iter()
            .filter(|id| id != &json!(fourth))
            .collect::<Vec<_>>()
    );
    tx.rollback().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn indexed_attribute_decoding_preserves_first_catalog_entry_for_duplicate_ids() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    let mut duplicate = f.catalog.forward("posts", "title").unwrap().clone();
    duplicate.label = "wrong-last-entry".to_owned();
    f.catalog.attributes.push(duplicate);
    let tx = f.client.transaction().await.unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    let rows = loader.load_entities(&tx, vec![key(f.first)]).await.unwrap();
    assert_eq!(rows[&key(f.first)]["title"], "own-first");
    assert!(
        !rows[&key(f.first)]
            .as_object()
            .unwrap()
            .contains_key("wrong-last-entry")
    );
    tx.rollback().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn entity_reads_keep_complete_json_app_scope_freshness_and_read_only_snapshots() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    for (app, expected) in [(f.app, "own"), (f.other_app, "foreign"), (f.app, "own")] {
        let tx = f
            .client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .unwrap();
        let loader = PostgresPermissionDataLoader {
            app_id: app,
            catalog: &f.catalog,
        };
        let missing = key(Uuid::new_v4());
        let rows = loader
            .load_entities(
                &tx,
                vec![key(f.first), key(f.first), key(f.second), missing.clone()],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[&key(f.first)]["title"],
            json!(format!("{expected}-first"))
        );
        assert_eq!(rows[&key(f.first)]["owner"], json!(f.user));
        assert!(
            !rows[&key(f.first)]
                .as_object()
                .unwrap()
                .contains_key("name")
        );
        assert!(!rows.contains_key(&missing));
        if app == f.app {
            assert_eq!(rows[&key(f.first)]["tags"], json!(["a", null, false]));
            assert_eq!(rows[&key(f.first)]["nullable"], Value::Null);
            assert_eq!(rows[&key(f.first)].as_object().unwrap().len(), 5);
        }
        assert!(loader.load_entities(&tx, vec![]).await.unwrap().is_empty());
        tx.rollback().await.unwrap();
    }
    f.client
        .execute(
            "UPDATE triples SET value=$1 WHERE app_id=$2 AND entity_id=$3 AND attr_id=$4",
            &[
                &Json(json!("fresh")),
                &f.app,
                &f.first,
                &f.attr("posts", "title"),
            ],
        )
        .await
        .unwrap();
    let tx = f.client.transaction().await.unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    assert_eq!(
        loader.load_entities(&tx, vec![key(f.first)]).await.unwrap()[&key(f.first)]["title"],
        json!("fresh")
    );
    tx.rollback().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn reference_reads_keep_forward_reverse_order_nulls_and_invalid_path_semantics() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    let forward = request(f.first, "posts", "owner.name");
    let reverse = request(f.user, "users", "posts.title");
    let reverse_ids = request(f.user, "users", "posts");
    let tags = request(f.first, "posts", "tags");
    let null = request(f.first, "posts", "nullable");
    let unknown = request(f.first, "posts", "unknown.name");
    let empty = request(f.first, "posts", "...");
    for (app, expected) in [(f.app, "own"), (f.other_app, "foreign")] {
        let tx = f
            .client
            .build_transaction()
            .read_only(true)
            .start()
            .await
            .unwrap();
        let loader = PostgresPermissionDataLoader {
            app_id: app,
            catalog: &f.catalog,
        };
        let refs = loader
            .load_refs(
                &tx,
                vec![
                    forward.clone(),
                    reverse.clone(),
                    reverse_ids.clone(),
                    tags.clone(),
                    null.clone(),
                    unknown.clone(),
                    empty.clone(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(refs[&forward], vec![json!(expected)]);
        assert_eq!(
            refs[&reverse],
            vec![
                json!(format!("{expected}-first")),
                json!(format!("{expected}-second"))
            ]
        );
        assert_eq!(refs[&reverse_ids], vec![json!(f.second), json!(f.first)]);
        assert!(refs[&unknown].is_empty());
        assert!(refs[&empty].is_empty());
        if app == f.app {
            assert_eq!(refs[&tags], vec![json!(["a", null, false])]);
            assert_eq!(refs[&null], vec![Value::Null]);
        } else {
            assert!(refs[&tags].is_empty());
            assert!(refs[&null].is_empty());
        }
        let bad = RefRequest {
            entity_id: RefEntityId::String("not-a-uuid".to_owned()),
            ..forward.clone()
        };
        assert!(loader.load_refs(&tx, vec![bad]).await.is_err());
        tx.rollback().await.unwrap();
    }
    f.client
        .execute(
            "UPDATE triples SET value=$1 WHERE app_id=$2 AND entity_id=$3 AND attr_id=$4",
            &[
                &Json(json!("fresh-name")),
                &f.app,
                &f.user,
                &f.attr("users", "name"),
            ],
        )
        .await
        .unwrap();
    let tx = f.client.transaction().await.unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    assert_eq!(
        loader.load_refs(&tx, vec![forward.clone()]).await.unwrap()[&forward],
        vec![json!("fresh-name")]
    );
    tx.rollback().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn prepared_entity_statement_matches_the_text_loader_and_reads_fresh_snapshots() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    let missing = key(Uuid::new_v4());
    let entities = vec![key(f.first), key(f.first), key(f.second), missing.clone()];
    let tx = f
        .client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .unwrap();
    let statement = tx
        .prepare_typed(
            instant_permissions::PERMISSION_ENTITY_SQL,
            instant_permissions::permission_entity_parameter_types(),
        )
        .await
        .unwrap();
    let prepared = instant_permissions::load_permission_entities_with_statement(
        &tx,
        &statement,
        f.app,
        &f.catalog,
        entities.clone(),
    )
    .await
    .unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    let text = loader.load_entities(&tx, entities).await.unwrap();
    assert_eq!(
        prepared, text,
        "the prepared and text paths must decode identical entity maps"
    );
    assert_eq!(prepared[&key(f.first)]["title"], json!("own-first"));
    assert!(!prepared.contains_key(&missing));
    assert!(
        instant_permissions::load_permission_entities_with_statement(
            &tx,
            &statement,
            f.app,
            &f.catalog,
            vec![]
        )
        .await
        .unwrap()
        .is_empty()
    );
    tx.rollback().await.unwrap();

    // The plan is reused across snapshots; parameters, rows and isolation are
    // not: a later transaction must see the committed update.
    f.client
        .execute(
            "UPDATE triples SET value=$1 WHERE app_id=$2 AND entity_id=$3 AND attr_id=$4",
            &[
                &Json(json!("prepared-fresh")),
                &f.app,
                &f.first,
                &f.attr("posts", "title"),
            ],
        )
        .await
        .unwrap();
    let tx = f.client.transaction().await.unwrap();
    let fresh = instant_permissions::load_permission_entities_with_statement(
        &tx,
        &statement,
        f.app,
        &f.catalog,
        vec![key(f.first)],
    )
    .await
    .unwrap();
    assert_eq!(fresh[&key(f.first)]["title"], json!("prepared-fresh"));
    tx.rollback().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn batched_paths_keep_per_root_results_for_shared_and_distinct_targets() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    let third_post = Uuid::new_v4();
    let other_user = Uuid::new_v4();
    let owner = f.attr("posts", "owner");
    let title = f.attr("posts", "title");
    f.insert(f.app, third_post, owner, json!(other_user), 11)
        .await;
    f.insert(f.app, third_post, title, json!("third"), 12).await;
    f.insert(
        f.app,
        other_user,
        f.attr("users", "name"),
        json!("other"),
        13,
    )
    .await;

    let requests = vec![
        request(f.first, "posts", "owner.name"),
        request(f.second, "posts", "owner.name"),
        request(third_post, "posts", "owner.name"),
        request(f.user, "users", "posts.title"),
        request(other_user, "users", "posts.title"),
    ];
    let tx = f
        .client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    let refs = loader.load_refs(&tx, requests.clone()).await.unwrap();
    // Two roots share one owner: each still receives exactly its own value.
    assert_eq!(refs[&requests[0]], vec![json!("own")]);
    assert_eq!(refs[&requests[1]], vec![json!("own")]);
    assert_eq!(refs[&requests[2]], vec![json!("other")]);
    // Reverse traversal keeps the legacy per-root ordering.
    assert_eq!(
        refs[&requests[3]],
        vec![json!("own-first"), json!("own-second")]
    );
    assert_eq!(refs[&requests[4]], vec![json!("third")]);
    tx.rollback().await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn reference_statement_count_depends_on_paths_not_entities() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    // pg_stat_statements is optional in the local acceptance cluster; skip when
    // it is absent.
    let statement_count = "SELECT coalesce(sum(calls), 0)::bigint
                             FROM pg_stat_statements
                            WHERE query LIKE '%FROM triples%'";
    if f.client.query_one(statement_count, &[]).await.is_err() {
        f.close().await;
        return;
    }

    let owner = f.attr("posts", "owner");
    let mut requests = Vec::new();
    for index in 0..64 {
        let post = Uuid::new_v4();
        f.insert(f.app, post, owner, json!(f.user), 100 + index)
            .await;
        requests.push(request(post, "posts", "owner.name"));
    }
    // Counts are deltas so the test needs no pg_stat_statements_reset
    // privilege. Concurrent triples statements only make the bound more
    // generous, and a per-entity regression is an order of magnitude larger.
    let before: i64 = f
        .client
        .query_one(statement_count, &[])
        .await
        .unwrap()
        .get(0);
    let tx = f
        .client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .unwrap();
    let loader = PostgresPermissionDataLoader {
        app_id: f.app,
        catalog: &f.catalog,
    };
    let refs = loader.load_refs(&tx, requests.clone()).await.unwrap();
    assert_eq!(refs[&requests[0]], vec![json!("own")]);
    tx.rollback().await.unwrap();
    let after: i64 = f
        .client
        .query_one(statement_count, &[])
        .await
        .unwrap()
        .get(0);
    f.close().await;
    let calls = after - before;
    assert!(
        calls < i64::try_from(requests.len()).unwrap(),
        "statements must scale with reference paths, not the {} entities; saw {calls}",
        requests.len()
    );
}
