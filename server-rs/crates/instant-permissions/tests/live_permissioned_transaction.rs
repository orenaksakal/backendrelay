use std::sync::Arc;

use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality};
use instant_permissions::{
    EvaluationBindings, InMemoryRateLimiter, PermissionError, PermissionedTransactionError,
    PermissionedTransactionOptions, PostgresPermissionDataLoader, RateLimitConsumer, RuleSet,
    execute_permissioned_transaction,
};
use instant_transact::{TransactionOptions, compile_transaction, execute_transaction};
use serde_json::json;
use tokio_postgres::NoTls;
use tokio_postgres::types::Json;
use uuid::Uuid;

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

async fn stored_title(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    entity_id: Uuid,
    title_attr: Uuid,
) -> Option<String> {
    client
        .query_opt(
            "SELECT value FROM triples
              WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&app_id, &entity_id, &title_attr],
        )
        .await
        .unwrap()
        .and_then(|row| {
            row.get::<_, Json<serde_json::Value>>(0)
                .0
                .as_str()
                .map(str::to_owned)
        })
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pre_and_post_permission_denials_roll_back_their_writes() {
    let Some(database_url) =
        instant_test_support::test_database_url("live_permissioned_transaction")
    else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
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
             CREATE UNIQUE INDEX permission_attrs_id ON attrs(id);
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
             CREATE UNIQUE INDEX permission_ea ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE OR REPLACE FUNCTION public.json_null_to_null(v jsonb)
             RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
               SELECT CASE WHEN v = 'null'::jsonb THEN NULL ELSE v END
             $$;
             CREATE UNIQUE INDEX permission_av
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
             END $$;",
        )
        .await
        .unwrap();

    let app_id = Uuid::new_v4();
    let id_attr = Uuid::new_v4();
    let title_attr = Uuid::new_v4();
    let slug_attr = Uuid::new_v4();
    let catalog = AttributeCatalog {
        attributes: vec![
            attribute(id_attr, "id", true),
            attribute(title_attr, "title", false),
            attribute(slug_attr, "slug", true),
        ],
    };
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
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
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
    let existing = Uuid::new_v4();
    let seed = compile_transaction(
        &json!([
            ["add-triple", existing, id_attr, existing],
            ["add-triple", existing, title_attr, "old"]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();

    let update = compile_transaction(
        &json!([["add-triple", existing, title_attr, "forbidden"]]),
        &catalog,
    )
    .unwrap();
    let deny_update = RuleSet::new(json!({
        "posts": {"allow": {"update": "false"}}
    }));
    let loader = PostgresPermissionDataLoader {
        app_id,
        catalog: &catalog,
    };
    let denied = execute_permissioned_transaction(
        &mut client,
        app_id,
        &catalog,
        &deny_update,
        &update,
        &EvaluationBindings::default(),
        PermissionedTransactionOptions::default(),
        &loader,
    )
    .await;
    assert!(matches!(
        denied,
        Err(PermissionedTransactionError::Permission(
            PermissionError::PermissionDenied { .. }
        ))
    ));
    assert_eq!(
        stored_title(&client, app_id, existing, title_attr).await,
        Some("old".to_owned())
    );

    let created = Uuid::new_v4();
    let create = compile_transaction(
        &json!([
            ["add-triple", created, id_attr, created],
            ["add-triple", created, title_attr, "new"]
        ]),
        &catalog,
    )
    .unwrap();
    let deny_create = RuleSet::new(json!({
        "posts": {"allow": {"create": "false"}}
    }));
    let denied = execute_permissioned_transaction(
        &mut client,
        app_id,
        &catalog,
        &deny_create,
        &create,
        &EvaluationBindings::default(),
        PermissionedTransactionOptions::default(),
        &loader,
    )
    .await;
    assert!(matches!(
        denied,
        Err(PermissionedTransactionError::Permission(
            PermissionError::PermissionDenied { .. }
        ))
    ));
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &created],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let lookup_create = compile_transaction(
        &json!([[
            "add-triple",
            [slug_attr, "lookup-created"],
            title_attr,
            "allowed"
        ]]),
        &catalog,
    )
    .unwrap();
    let lookup_rules = RuleSet::new(json!({
        "posts": {"allow": {"create":
            "newData.slug == 'lookup-created' && newData.title == 'allowed' && request.modifiedFields == ['title']"
        }}
    }));
    let receipt = execute_permissioned_transaction(
        &mut client,
        app_id,
        &catalog,
        &lookup_rules,
        &lookup_create,
        &EvaluationBindings::default(),
        PermissionedTransactionOptions::default(),
        &loader,
    )
    .await
    .unwrap();
    let lookup_entity: Uuid = client
        .query_one(
            "SELECT entity_id FROM triples WHERE app_id = $1 AND attr_id = $2 AND value = $3",
            &[&app_id, &slug_attr, &Json(json!("lookup-created"))],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        stored_title(&client, app_id, lookup_entity, title_attr).await,
        Some("allowed".to_owned())
    );
    assert!(
        receipt
            .transaction
            .mutations
            .iter()
            .all(|mutation| mutation.attr_id != slug_attr),
        "implicit lookup insertion must stay out of the user mutation receipt"
    );

    let denied_lookup = compile_transaction(
        &json!([[
            "add-triple",
            [slug_attr, "rolled-back"],
            title_attr,
            "denied"
        ]]),
        &catalog,
    )
    .unwrap();
    let denied = execute_permissioned_transaction(
        &mut client,
        app_id,
        &catalog,
        &deny_create,
        &denied_lookup,
        &EvaluationBindings::default(),
        PermissionedTransactionOptions::default(),
        &loader,
    )
    .await;
    assert!(matches!(
        denied,
        Err(PermissionedTransactionError::Permission(
            PermissionError::PermissionDenied { .. }
        ))
    ));
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2 AND value = $3",
                &[&app_id, &slug_attr, &Json(json!("rolled-back"))],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );

    let rate_limiter: Arc<dyn RateLimitConsumer> = Arc::new(InMemoryRateLimiter::default());
    let rate_rules = RuleSet::try_new(json!({
        "$rateLimits": {"createPosts": {"limits": [{"capacity": 2}]}},
        "posts": {"allow": {"create": "rateLimit.createPosts.limit(auth.id)"}}
    }))
    .unwrap();
    let rate_bindings = EvaluationBindings {
        auth: json!({"id": Uuid::new_v4()}),
        ..EvaluationBindings::default()
    };
    for index in 0..2 {
        let entity_id = Uuid::new_v4();
        let create = compile_transaction(
            &json!([
                ["add-triple", entity_id, id_attr, entity_id],
                [
                    "add-triple",
                    entity_id,
                    title_attr,
                    format!("limited-{index}")
                ]
            ]),
            &catalog,
        )
        .unwrap();
        execute_permissioned_transaction(
            &mut client,
            app_id,
            &catalog,
            &rate_rules,
            &create,
            &rate_bindings,
            PermissionedTransactionOptions {
                rate_limit_consumer: Some(Arc::clone(&rate_limiter)),
                ..PermissionedTransactionOptions::default()
            },
            &loader,
        )
        .await
        .unwrap();
    }
    let rate_limited_entity = Uuid::new_v4();
    let create = compile_transaction(
        &json!([
            [
                "add-triple",
                rate_limited_entity,
                id_attr,
                rate_limited_entity
            ],
            ["add-triple", rate_limited_entity, title_attr, "limited-2"]
        ]),
        &catalog,
    )
    .unwrap();
    assert!(matches!(
        execute_permissioned_transaction(
            &mut client,
            app_id,
            &catalog,
            &rate_rules,
            &create,
            &rate_bindings,
            PermissionedTransactionOptions {
                rate_limit_consumer: Some(Arc::clone(&rate_limiter)),
                ..PermissionedTransactionOptions::default()
            },
            &loader,
        )
        .await,
        Err(PermissionedTransactionError::Permission(
            PermissionError::RateLimited { .. }
        ))
    ));
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &rate_limited_entity],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "a post-create rate-limit denial must roll back the entity write"
    );

    drop(client);
    connection_task.await.unwrap().unwrap();
}
