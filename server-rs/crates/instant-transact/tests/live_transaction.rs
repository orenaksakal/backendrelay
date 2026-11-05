use std::time::Duration;

use instant_instaql::{AttributeCatalog, AttributeValueType, Cardinality, DeleteAction};
use instant_transact::{
    AppStatusWriteCheck, EntityLookup, ExecutionError, TransactionOptions, TransactionStep,
    WalEntityLog, compile_admin_transaction, compile_transaction,
    configure_transaction_statement_timeout, execute_transaction,
    execute_transaction_in_with_app_status_write_check,
    execute_transaction_with_app_status_write_check, prepare_transaction_lookups,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::NoTls;
use tokio_postgres::types::Json;
use uuid::Uuid;

#[derive(Deserialize)]
struct CatalogFixture {
    catalog: AttributeCatalog,
}

#[derive(Deserialize)]
struct UpdateModeFixture {
    oracle: String,
    cases: Vec<UpdateModeCase>,
}

#[derive(Deserialize)]
struct UpdateModeCase {
    name: String,
    input: Value,
    outcome: String,
}

fn catalog() -> AttributeCatalog {
    serde_json::from_str::<CatalogFixture>(include_str!(
        "../../instant-instaql/tests/fixtures/where_compilation.json"
    ))
    .unwrap()
    .catalog
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

async fn value(
    client: &tokio_postgres::Client,
    app_id: Uuid,
    entity_id: Uuid,
    attr_id: Uuid,
) -> Option<Value> {
    client
        .query_opt(
            "SELECT value FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&app_id, &entity_id, &attr_id],
        )
        .await
        .unwrap()
        .map(|row| row.get::<_, Json<Value>>(0).0)
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One database lifecycle proves atomic cross-operation semantics.
async fn compiled_transactions_mutate_production_shaped_postgres_atomically() {
    let Some(database_url) = instant_test_support::test_database_url(
        "compiled_transactions_mutate_production_shaped_postgres_atomically",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TYPE pg_temp.checked_data_type AS ENUM ('string', 'number', 'boolean', 'date');
             CREATE TYPE pg_temp.attr_on_delete AS ENUM ('cascade');
             CREATE OR REPLACE FUNCTION public.current_unix_timestamp_ms()
             RETURNS bigint LANGUAGE sql VOLATILE AS $$
               SELECT (extract(epoch FROM now() AT TIME ZONE 'UTC') * 1000)::bigint
             $$;
             CREATE TEMP TABLE transactions (
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
               app_id uuid NOT NULL,
               created_at timestamp NOT NULL DEFAULT now()
             );
             CREATE TEMP TABLE apps (
               id uuid PRIMARY KEY,
               status text NOT NULL DEFAULT 'active',
               deletion_marked_at timestamptz
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
             CREATE UNIQUE INDEX tx_attrs_id ON attrs(id);
             CREATE TEMP TABLE idents (
               id uuid PRIMARY KEY,
               app_id uuid NOT NULL,
               attr_id uuid NOT NULL,
               etype text NOT NULL,
               label text NOT NULL,
               UNIQUE (app_id, etype, label)
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
             CREATE UNIQUE INDEX tx_ea_index ON triples(app_id, entity_id, attr_id) WHERE ea;
             CREATE OR REPLACE FUNCTION public.json_null_to_null(v jsonb)
             RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
               SELECT CASE WHEN v = 'null'::jsonb THEN NULL ELSE v END
             $$;
             CREATE UNIQUE INDEX tx_av_index
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
             END $$;
             CREATE OR REPLACE FUNCTION public.jsonb_deep_merge_many(base jsonb, objs jsonb)
             RETURNS jsonb LANGUAGE plpgsql AS $$
             DECLARE accum jsonb := base; idx int := 0;
             BEGIN
               WHILE idx < jsonb_array_length(objs) LOOP
                 accum := public.jsonb_deep_merge(accum, objs->idx);
                 idx := idx + 1;
               END LOOP;
               RETURN accum;
             END $$;
             CREATE OR REPLACE FUNCTION public.raise_exception_message(message text)
             RETURNS boolean LANGUAGE plpgsql AS $$
             BEGIN
               RAISE EXCEPTION '%', message USING CONSTRAINT = message;
               RETURN false;
             END $$;",
        )
        .await
        .unwrap();

    let catalog = catalog();
    let app_id = uuid("aaaaaaaa-0000-0000-0000-000000000001");
    client
        .execute(
            "INSERT INTO apps (id, status) VALUES ($1, 'active')",
            &[&app_id],
        )
        .await
        .unwrap();
    let user_id = uuid("aaaaaaaa-0000-0000-0000-000000000002");
    let shelf_id = uuid("aaaaaaaa-0000-0000-0000-000000000003");
    let second_shelf_id = uuid("aaaaaaaa-0000-0000-0000-000000000004");
    let superseded_entity_id = uuid("aaaaaaaa-0000-0000-0000-000000000007");
    let user_id_attr = uuid("11111111-1111-1111-1111-111111111111");
    let handle_attr = uuid("22222222-2222-2222-2222-222222222222");
    let age_attr = uuid("33333333-3333-3333-3333-333333333333");
    let shelves_attr = uuid("44444444-4444-4444-4444-444444444444");
    let shelf_id_attr = uuid("55555555-5555-5555-5555-555555555555");
    let nickname_attr = uuid("99999999-9999-9999-9999-999999999999");
    let bio_attr = uuid("bbbbbbbb-0000-0000-0000-000000000001");
    let bio_ident = uuid("bbbbbbbb-0000-0000-0000-000000000002");
    let slug_attr = uuid("bbbbbbbb-0000-0000-0000-000000000003");
    let slug_ident = uuid("bbbbbbbb-0000-0000-0000-000000000004");
    let unique_profile_attr = uuid("bbbbbbbb-0000-0000-0000-000000000011");
    let unique_profile_ident = uuid("bbbbbbbb-0000-0000-0000-000000000012");
    let merged_entity_id = uuid("aaaaaaaa-0000-0000-0000-000000000008");
    let conflicting_profile_owner = uuid("aaaaaaaa-0000-0000-0000-000000000009");
    for attribute in &catalog.attributes {
        let forward_ident = Uuid::new_v4();
        let reverse_ident = attribute.reverse_etype.as_ref().map(|_| Uuid::new_v4());
        let value_type = match attribute.value_type {
            AttributeValueType::Blob => "blob",
            AttributeValueType::Ref => "ref",
        };
        let cardinality = match attribute.cardinality {
            Cardinality::One => "one",
            Cardinality::Many => "many",
        };
        let checked_data_type = attribute
            .checked_data_type
            .map(|data_type| format!("{data_type:?}").to_lowercase());
        client
            .execute(
                "INSERT INTO attrs
                 (app_id, id, value_type, cardinality, is_unique, is_indexed, is_required,
                  forward_ident, etype, label, reverse_ident, reverse_etype, reverse_label,
                  on_delete, on_delete_reverse, checked_data_type, setting_unique, indexing,
                  checking_data_type)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                         $14::text::attr_on_delete, $15::text::attr_on_delete,
                         $16::text::checked_data_type, $17, $18, $19)",
                &[
                    &app_id,
                    &attribute.id,
                    &value_type,
                    &cardinality,
                    &attribute.unique,
                    &attribute.indexed,
                    &attribute.required,
                    &forward_ident,
                    &attribute.etype,
                    &attribute.label,
                    &reverse_ident,
                    &attribute.reverse_etype,
                    &attribute.reverse_label,
                    &Option::<String>::None,
                    &Option::<String>::None,
                    &checked_data_type,
                    &attribute.setting_unique,
                    &attribute.indexing,
                    &attribute.checking_data_type,
                ],
            )
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO idents (id, app_id, attr_id, etype, label)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &forward_ident,
                    &app_id,
                    &attribute.id,
                    &attribute.etype,
                    &attribute.label,
                ],
            )
            .await
            .unwrap();
        if let (Some(reverse_ident), Some(reverse_etype), Some(reverse_label)) = (
            reverse_ident,
            &attribute.reverse_etype,
            &attribute.reverse_label,
        ) {
            client
                .execute(
                    "INSERT INTO idents (id, app_id, attr_id, etype, label)
                     VALUES ($1, $2, $3, $4, $5)",
                    &[
                        &reverse_ident,
                        &app_id,
                        &attribute.id,
                        reverse_etype,
                        reverse_label,
                    ],
                )
                .await
                .unwrap();
        }
    }

    let initial = compile_transaction(
        &json!([
          ["add-triple", user_id, user_id_attr, user_id],
          ["add-triple", user_id, handle_attr, "alex"],
          ["add-triple", user_id, nickname_attr, {"prefs": {"a": 1, "remove": true}}],
          ["add-triple", shelf_id, shelf_id_attr, shelf_id],
          ["add-triple", user_id, shelves_attr, shelf_id]
        ]),
        &catalog,
    )
    .unwrap();
    let receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &initial,
        TransactionOptions {
            invalidator_slot_num: 7,
            wal_entity_log: WalEntityLog::LogicalMessage,
            ..TransactionOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(receipt.id, 1);
    assert_eq!(receipt.fallback_isn.slot_num(), 7);
    assert_ne!(receipt.fallback_isn.lsn().as_u64(), 0);
    assert_eq!(
        receipt.mutations.len(),
        7,
        "five requested writes plus users.age and bookshelves.order null sentinels"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/model/triple.clj:812-1156 insert-multi-new!
    // Indexed nulls share the final add-group statement with requested values;
    // a requested indexed value suppresses its null candidate in that statement.
    let complete_user_id = uuid("aaaaaaaa-0000-0000-0000-000000000010");
    let complete_user = compile_transaction(
        &json!([
            [
                "add-triple",
                complete_user_id,
                user_id_attr,
                complete_user_id
            ],
            ["add-triple", complete_user_id, age_attr, 42]
        ]),
        &catalog,
    )
    .unwrap();
    let receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &complete_user,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        receipt
            .mutations
            .iter()
            .filter(|mutation| mutation.entity_id == complete_user_id)
            .count(),
        3,
        "the ID and age writes plus only the missing handle null are receipted"
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &complete_user_id, &age_attr],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "a same-group value must prevent a duplicate indexed-null row"
    );
    assert_eq!(
        value(&client, app_id, complete_user_id, age_attr).await,
        Some(json!(42))
    );
    assert_eq!(
        value(&client, app_id, complete_user_id, handle_attr).await,
        Some(Value::Null)
    );

    // `rule-params` carries authorization-engine inputs through the transaction
    // pipeline but deliberately does not mutate triples.  Keep this on the live
    // PostgreSQL path: preprocessing coverage alone cannot prove that the
    // executor records the transaction and commits without inventing a WAL
    // mutation or touching the entity.
    let rule_params = compile_transaction(
        &json!([[
            "rule-params",
            user_id,
            "users",
            {"localId": "native-live-test", "source": "transaction"}
        ]]),
        &catalog,
    )
    .unwrap();
    let rule_receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &rule_params,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert!(rule_receipt.mutations.is_empty());
    assert!(rule_receipt.schema_mutations.is_empty());
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        3,
        "rule params still commits a transaction record for WAL ordering"
    );
    assert_eq!(
        value(&client, app_id, user_id, handle_attr).await,
        Some(json!("alex")),
        "rule params must not alter entity data"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/model/triple.clj:1262-1322 delete-multi!
    // resolves missing entity/value lookups to no matching SQL row and hashes
    // arbitrary direct values. Retractions are idempotent cleanup operations:
    // none of these public inputs is an insert-style reference validation
    // failure, and the existing relationship must remain untouched.
    let idempotent_retracts = compile_transaction(
        &json!([
            [
                "retract-triple",
                [handle_attr, "missing-user"],
                shelves_attr,
                shelf_id
            ],
            [
                "retract-triple",
                [nickname_attr, {"prefs": {"not": "present"}}],
                shelves_attr,
                shelf_id
            ],
            [
                "retract-triple",
                user_id,
                shelves_attr,
                [shelf_id_attr, "aaaaaaaa-0000-0000-0000-ffffffffffff"]
            ],
            ["retract-triple", user_id, shelves_attr, "not-a-uuid"]
        ]),
        &catalog,
    )
    .unwrap();
    let retract_receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &idempotent_retracts,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert!(retract_receipt.mutations.is_empty());
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3 AND value = $4",
                &[&app_id, &user_id, &shelves_attr, &Json(&json!(shelf_id))],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "a missing non-unique lookup is admitted and preserves the relationship"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/model/triple.clj:570-620 applies DISTINCT ON
    // (entity-id, attr-id) ordered by descending input index before inserting
    // cardinality-one rows. The superseded value conflicts with alex's unique
    // handle, but it must never reach the unique AV index because the final
    // value for this entity/attribute pair wins inside the operation group.
    let last_cardinality_one_value_wins = compile_transaction(
        &json!([
            [
                "add-triple",
                superseded_entity_id,
                user_id_attr,
                superseded_entity_id
            ],
            ["add-triple", superseded_entity_id, handle_attr, "alex"],
            ["add-triple", superseded_entity_id, handle_attr, "available"]
        ]),
        &catalog,
    )
    .unwrap();
    let collapsed_receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &last_cardinality_one_value_wins,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, superseded_entity_id, handle_attr).await,
        Some(json!("available"))
    );
    assert_eq!(
        collapsed_receipt
            .mutations
            .iter()
            .filter(|mutation| {
                mutation.entity_id == superseded_entity_id && mutation.attr_id == handle_attr
            })
            .count(),
        1,
        "the receipt matches the one persisted cardinality-one row"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/transaction.clj:580-614 executes consecutive
    // operation groups in request order. server/src/instant/model/triple.clj:
    // 344-370 reads cardinality and value type from the persisted attrs row at
    // the point the triple group runs. A partial update therefore changes only
    // its supplied column; it cannot turn a persisted ref/many attribute into
    // the parser's blob/one defaults for the following write group.
    let later_shelf_id = uuid("aaaaaaaa-0000-0000-0000-000000000005");
    let last_shelf_id = uuid("aaaaaaaa-0000-0000-0000-000000000006");
    let partial_schema_update_then_writes = compile_transaction(
        &json!([
            ["update-attr", {"id": shelves_attr, "on-delete": "cascade"}],
            ["add-triple", user_id, shelves_attr, later_shelf_id],
            ["add-triple", user_id, shelves_attr, last_shelf_id]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &partial_schema_update_then_writes,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let persisted_links = client
        .query_one(
            "SELECT count(*), bool_and(vae) AND NOT bool_or(ea)
             FROM triples
             WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&app_id, &user_id, &shelves_attr],
        )
        .await
        .unwrap();
    assert_eq!(
        persisted_links.get::<_, i64>(0),
        3,
        "the partial schema update preserves many cardinality for both following writes"
    );
    assert!(
        persisted_links.get::<_, bool>(1),
        "the following write group still materializes the persisted ref index flags"
    );

    // A duplicate value on a unique attribute is a database constraint error,
    // not a successful upsert.  Keep this fixture alongside the JVM-shaped
    // schema because it proves both public error provenance and atomicity:
    // the transaction row and the attempted entity write must disappear when
    // PostgreSQL rejects the unique AV index.
    let transactions_before_unique_error: i64 = client
        .query_one("SELECT count(*) FROM transactions", &[])
        .await
        .unwrap()
        .get(0);
    let duplicate_handle = compile_transaction(
        &json!([[
            "add-triple",
            second_shelf_id,
            handle_attr,
            "alex",
            {"mode": "create"}
        ]]),
        &catalog,
    )
    .unwrap();
    let unique_error = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &duplicate_handle,
        TransactionOptions::default(),
    )
    .await
    .unwrap_err();
    let ExecutionError::Postgres(postgres_error) = unique_error else {
        panic!("expected unique PostgreSQL error, got {unique_error:?}");
    };
    assert_eq!(
        postgres_error.code(),
        Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION),
        "unique AV constraint remains distinguishable for the public error mapper"
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_unique_error,
        "a uniqueness failure rolls back its transaction record"
    );
    assert_eq!(
        value(&client, app_id, second_shelf_id, handle_attr).await,
        None,
        "a uniqueness failure rolls back the attempted entity write"
    );

    let bio_definition = json!({
        "id": bio_attr,
        "forward-identity": [bio_ident, "users", "bio"],
        "value-type": "blob",
        "cardinality": "one",
        "unique?": false,
        "index?": true
    });
    let add_schema_and_write = compile_transaction(
        &json!([
            ["add-attr", bio_definition.clone()],
            ["add-triple", user_id, bio_attr, "writer"]
        ]),
        &catalog,
    )
    .unwrap();
    let schema_receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &add_schema_and_write,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(schema_receipt.schema_mutations.len(), 1);
    assert!(schema_receipt.schema_mutations[0].changed);
    assert_eq!(
        schema_receipt.schema_mutations[0].affected_entity_ids,
        vec![user_id, superseded_entity_id, complete_user_id],
        "adding an indexed blob backfills existing namespace entities before the write"
    );
    assert_eq!(
        value(&client, app_id, user_id, bio_attr).await,
        Some(json!("writer"))
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // transaction.clj:580-614 runs the add-attr group first, while
    // triple.clj:440-520 creates missing entity lookup rows inside the later
    // add-triple group. The lookup must therefore observe the newly persisted
    // schema without moving any write ahead of the transaction record.
    let add_lookup_attr_then_use_it = compile_transaction(
        &json!([
            ["add-attr", {
                "id": slug_attr,
                "forward-identity": [slug_ident, "users", "slug"],
                "value-type": "blob",
                "cardinality": "one",
                "unique?": true,
                "index?": false
            }],
            [
                "add-triple",
                [slug_attr, "new-user"],
                handle_attr,
                "created-after-schema"
            ],
            [
                "add-triple",
                [slug_attr, "new-user"],
                user_id_attr,
                [slug_attr, "new-user"]
            ]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &add_lookup_attr_then_use_it,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let lookup_entity_id = client
        .query_one(
            "SELECT entity_id FROM triples
             WHERE app_id = $1 AND attr_id = $2 AND value = $3::jsonb AND av",
            &[&app_id, &slug_attr, &Json(&json!("new-user"))],
        )
        .await
        .unwrap()
        .get::<_, Uuid>(0);
    assert_eq!(
        value(&client, app_id, lookup_entity_id, handle_attr).await,
        Some(json!("created-after-schema")),
        "the consuming add-triple group sees its just-created implicit lookup entity"
    );

    let idempotent_add =
        compile_transaction(&json!([["add-attr", bio_definition.clone()]]), &catalog).unwrap();
    let idempotent_receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &idempotent_add,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert!(!idempotent_receipt.schema_mutations[0].changed);

    let update_steps = compile_transaction(
        &json!([["update-attr", {
          "id": bio_attr,
          "forward-identity": [bio_ident, "users", "biography"],
          "index?": false
        }]]),
        &catalog,
    )
    .unwrap();
    let update_receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &update_steps,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        update_receipt.schema_mutations[0].affected_entity_ids,
        vec![user_id, superseded_entity_id, complete_user_id]
    );
    let updated_row = client
        .query_one(
            "SELECT attrs.label, attrs.is_indexed, triples.ave, idents.label
             FROM attrs
             JOIN triples ON triples.attr_id = attrs.id AND triples.app_id = attrs.app_id
             JOIN idents ON idents.id = attrs.forward_ident
             WHERE attrs.app_id = $1 AND attrs.id = $2 AND triples.entity_id = $3",
            &[&app_id, &bio_attr, &user_id],
        )
        .await
        .unwrap();
    assert_eq!(updated_row.get::<_, String>(0), "biography");
    assert!(!updated_row.get::<_, bool>(1));
    assert!(!updated_row.get::<_, bool>(2));
    assert_eq!(updated_row.get::<_, String>(3), "biography");

    let delete_steps = compile_transaction(&json!([["delete-attr", bio_attr]]), &catalog).unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &delete_steps,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let deleted_row = client
        .query_one(
            "SELECT etype, label, deletion_marked_at IS NOT NULL,
                    metadata->'soft_delete_snapshot'->>'id_attr_id'
             FROM attrs WHERE app_id = $1 AND id = $2",
            &[&app_id, &bio_attr],
        )
        .await
        .unwrap();
    assert!(
        deleted_row
            .get::<_, String>(0)
            .starts_with(&format!("{bio_attr}_deleted$"))
    );
    assert!(
        deleted_row
            .get::<_, String>(1)
            .starts_with(&format!("{bio_attr}_deleted$"))
    );
    assert!(deleted_row.get::<_, bool>(2));
    assert_eq!(
        deleted_row.get::<_, Option<String>>(3),
        Some(user_id_attr.to_string())
    );
    assert_eq!(
        value(&client, app_id, user_id, bio_attr).await,
        Some(json!("writer")),
        "soft deletion preserves triples"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // transaction.clj:580-614 executes restore before the later triple group,
    // attr.clj:657-725 restores the persisted row, and triple.clj:570-600
    // derives the write flags from that now-active row. The request's input
    // catalog intentionally predates this dynamically added attribute.
    let restore_steps = compile_transaction(
        &json!([
            ["restore-attr", bio_attr],
            ["add-triple", user_id, bio_attr, "restored writer"]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &restore_steps,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let restored_row = client
        .query_one(
            "SELECT etype, label, is_indexed, is_required,
                    metadata ? 'soft_delete_snapshot', deletion_marked_at IS NULL
             FROM attrs WHERE app_id = $1 AND id = $2",
            &[&app_id, &bio_attr],
        )
        .await
        .unwrap();
    assert_eq!(restored_row.get::<_, String>(0), "users");
    assert_eq!(restored_row.get::<_, String>(1), "biography");
    assert!(!restored_row.get::<_, bool>(2));
    assert!(!restored_row.get::<_, bool>(3));
    assert!(!restored_row.get::<_, bool>(4));
    assert!(restored_row.get::<_, bool>(5));
    assert_eq!(
        value(&client, app_id, user_id, bio_attr).await,
        Some(json!("restored writer")),
        "the restored attribute metadata is available to the later write group"
    );

    let conflicting_definition = json!({
        "id": bio_attr,
        "forward-identity": [bio_ident, "users", "conflicting"],
        "value-type": "blob",
        "cardinality": "one",
        "unique?": false,
        "index?": false
    });
    let conflict =
        compile_transaction(&json!([["add-attr", conflicting_definition]]), &catalog).unwrap();
    assert!(matches!(
        execute_transaction(
            &mut client,
            app_id,
            &catalog,
            &conflict,
            TransactionOptions::default()
        )
        .await,
        Err(ExecutionError::AttributeConflict(id)) if id == bio_attr
    ));

    let required_attr = uuid("bbbbbbbb-0000-0000-0000-000000000009");
    let required_ident = uuid("bbbbbbbb-0000-0000-0000-000000000010");
    let invalid_required = compile_transaction(
        &json!([["add-attr", {
          "id": required_attr,
          "forward-identity": [required_ident, "users", "requiredLater"],
          "value-type": "blob",
          "cardinality": "one",
          "unique?": false,
          "index?": false,
          "required?": true
        }]]),
        &catalog,
    )
    .unwrap();
    assert!(matches!(
        execute_transaction(
            &mut client,
            app_id,
            &catalog,
            &invalid_required,
            TransactionOptions::default()
        )
        .await,
        Err(ExecutionError::RequiredAttributeOnExistingEntities { .. })
    ));
    let original_created_at: i64 = client
        .query_one(
            "SELECT created_at FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&app_id, &user_id, &handle_attr],
        )
        .await
        .unwrap()
        .get(0);

    let update = compile_transaction(
        &json!([
          ["add-triple", user_id, handle_attr, "alexander", {"mode": "update"}],
          ["deep-merge-triple", user_id, nickname_attr, {"prefs": {"b": 2, "remove": null}}]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &update,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, user_id, nickname_attr).await,
        Some(json!({"prefs": {"a": 1, "b": 2}}))
    );
    let updated_created_at: i64 = client
        .query_one(
            "SELECT created_at FROM triples WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
            &[&app_id, &user_id, &handle_attr],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(updated_created_at, original_created_at);

    let unique_profile_definition = json!({
        "id": unique_profile_attr,
        "forward-identity": [unique_profile_ident, "users", "uniqueProfile"],
        "value-type": "blob",
        "cardinality": "one",
        "unique?": true,
        "index?": false
    });
    let add_unique_profile =
        compile_transaction(&json!([["add-attr", unique_profile_definition]]), &catalog).unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &add_unique_profile,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let TransactionStep::AddAttr(definition) = &add_unique_profile[0] else {
        panic!("expected add-attr")
    };
    let mut catalog_with_unique_profile = catalog.clone();
    catalog_with_unique_profile
        .attributes
        .push(definition.query_attribute().clone());
    let seed_unique_profile = compile_transaction(
        &json!([
            ["add-triple", user_id, unique_profile_attr, {"key": "taken"}],
            ["add-triple", conflicting_profile_owner, user_id_attr, conflicting_profile_owner],
            ["add-triple", merged_entity_id, user_id_attr, merged_entity_id],
            ["add-triple", conflicting_profile_owner, unique_profile_attr, "occupied-two"]
        ]),
        &catalog_with_unique_profile,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog_with_unique_profile,
        &seed_unique_profile,
        TransactionOptions::default(),
    )
    .await
    .unwrap();

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/model/triple.clj:249-405 groups patches by
    // entity/attribute and calls jsonb_deep_merge_many once. Persisting the
    // first patch independently would collide with the seeded unique value;
    // only the final merged value is allowed to reach the AV constraint.
    let grouped_deep_merge = compile_transaction(
        &json!([
            ["deep-merge-triple", merged_entity_id, unique_profile_attr, {"key": "taken"}],
            ["deep-merge-triple", superseded_entity_id, unique_profile_attr, {"key": "other"}],
            ["deep-merge-triple", merged_entity_id, unique_profile_attr, {"key": "free"}],
            ["deep-merge-triple", superseded_entity_id, unique_profile_attr, {"nested": {"second": true}}],
            ["deep-merge-triple", merged_entity_id, unique_profile_attr, {"nested": {"kept": true}}]
        ]),
        &catalog_with_unique_profile,
    )
    .unwrap();
    let merge_receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog_with_unique_profile,
        &grouped_deep_merge,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, merged_entity_id, unique_profile_attr).await,
        Some(json!({"key": "free", "nested": {"kept": true}}))
    );
    assert_eq!(
        value(&client, app_id, superseded_entity_id, unique_profile_attr).await,
        Some(json!({"key": "other", "nested": {"second": true}}))
    );
    assert_eq!(
        merge_receipt
            .mutations
            .iter()
            .filter(|mutation| mutation.attr_id == unique_profile_attr)
            .map(|mutation| mutation.entity_id)
            .collect::<Vec<_>>(),
        vec![superseded_entity_id, merged_entity_id],
        "one set-based statement returns one mutation per final group in deterministic order"
    );

    let deterministic_conflict = compile_transaction(
        &json!([
            ["deep-merge-triple", merged_entity_id, unique_profile_attr, {"key": "taken", "nested": null}],
            ["deep-merge-triple", superseded_entity_id, unique_profile_attr, "occupied-two"]
        ]),
        &catalog_with_unique_profile,
    )
    .unwrap();
    let conflict = execute_transaction(
        &mut client,
        app_id,
        &catalog_with_unique_profile,
        &deterministic_conflict,
        TransactionOptions::default(),
    )
    .await
    .unwrap_err();
    let ExecutionError::Postgres(postgres_error) = conflict else {
        panic!("expected unique PostgreSQL error, got {conflict:?}")
    };
    assert_eq!(
        postgres_error.code(),
        Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION)
    );
    assert!(
        postgres_error
            .as_db_error()
            .and_then(|error| error.detail())
            .is_some_and(|detail| detail.contains("occupied-two")),
        "the lower entity UUID is checked first by the upstream candidate ordering"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/model/triple.clj:769-850,1120-1156 keeps lookup
    // creation and requested writes in one command. A missing lookup cannot
    // simultaneously change the lookup attribute; the validation failure rolls
    // back both the implicit row and transaction record.
    let transactions_before_lookup_collision: i64 = client
        .query_one("SELECT count(*) FROM transactions", &[])
        .await
        .unwrap()
        .get(0);
    let lookup_collision = compile_transaction(
        &json!([
            ["add-triple", [handle_attr, "alexander"], age_attr, 99],
            [
                "add-triple",
                [handle_attr, "lookup-collision"],
                handle_attr,
                "lookup-moved"
            ]
        ]),
        &catalog,
    )
    .unwrap();
    let collision = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &lookup_collision,
        TransactionOptions::default(),
    )
    .await
    .unwrap_err();
    let ExecutionError::LookupUpdateCollision(lookup_refs) = collision else {
        panic!("expected lookup collision, got {collision:?}")
    };
    assert_eq!(
        lookup_refs,
        vec![
            instant_transact::LookupRef {
                attr_id: handle_attr,
                value: json!("alexander"),
            },
            instant_transact::LookupRef {
                attr_id: handle_attr,
                value: json!("lookup-collision"),
            },
        ],
        "the public payload retains existing and missing lookups in original distinct order"
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND attr_id = $2 AND value IN ($3, $4)",
                &[
                    &app_id,
                    &handle_attr,
                    &Json(json!("lookup-collision")),
                    &Json(json!("lookup-moved")),
                ],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "lookup collision rolls back both implicit and requested values"
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_lookup_collision,
        "lookup collision rolls back its transaction record"
    );

    let lookup_writes = compile_transaction(
        &json!([
            [
                "add-triple",
                [handle_attr, "new-user"],
                nickname_attr,
                "created by lookup"
            ],
            [
                "add-triple",
                [handle_attr, "new-user"],
                user_id_attr,
                [handle_attr, "new-user"]
            ],
            [
                "add-triple",
                second_shelf_id,
                shelf_id_attr,
                second_shelf_id
            ],
            [
                "add-triple",
                user_id,
                shelves_attr,
                [shelf_id_attr, second_shelf_id]
            ]
        ]),
        &catalog,
    )
    .unwrap();
    let receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &lookup_writes,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        receipt.mutations.len(),
        6,
        "four requested writes plus two indexed nulls; implicit lookup creation is omitted"
    );
    let new_user: Uuid = client
        .query_one(
            "SELECT entity_id FROM triples WHERE app_id = $1 AND attr_id = $2 AND value = $3",
            &[&app_id, &handle_attr, &Json(json!("new-user"))],
        )
        .await
        .unwrap()
        .get(0);
    assert_ne!(new_user, user_id);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3 AND value = $4",
                &[&app_id, &new_user, &handle_attr, &Json(json!("new-user")),],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "repeated identical entity lookups materialize one unique lookup row"
    );
    assert!(
        !receipt
            .mutations
            .iter()
            .any(|mutation| { mutation.entity_id == new_user && mutation.attr_id == handle_attr }),
        "implicit lookup materialization must stay out of the receipt"
    );
    assert_eq!(
        value(&client, app_id, new_user, nickname_attr).await,
        Some(json!("created by lookup"))
    );
    assert_eq!(
        value(&client, app_id, new_user, user_id_attr).await,
        Some(json!(new_user))
    );

    let id_lookup_user = Uuid::new_v4();
    let id_lookup_write = compile_transaction(
        &json!([[
            "add-triple",
            [user_id_attr, id_lookup_user],
            nickname_attr,
            "id-created"
        ]]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &id_lookup_write,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples
                 WHERE app_id = $1 AND entity_id = $2 AND attr_id = $3",
                &[&app_id, &id_lookup_user, &nickname_attr],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "an id lookup plus a requested indexed value suppresses the candidate null"
    );
    assert_eq!(
        value(&client, app_id, id_lookup_user, nickname_attr).await,
        Some(json!("id-created"))
    );
    assert_eq!(
        client
            .query_one(
                "SELECT inferred_types::integer FROM attrs WHERE app_id = $1 AND id = $2",
                &[&app_id, &nickname_attr],
            )
            .await
            .unwrap()
            .get::<_, i32>(0),
        10,
        "nickname accumulated JSON and string inferred-type bits"
    );

    let no_op = compile_transaction(
        &json!([
            ["add-triple", user_id, user_id_attr, user_id],
            ["add-triple", user_id, handle_attr, "alexander"]
        ]),
        &catalog,
    )
    .unwrap();
    let receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &no_op,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert!(
        receipt.mutations.is_empty(),
        "a fully no-op add group must not rewrite the ID triple"
    );

    let changed_with_id = compile_transaction(
        &json!([
            ["add-triple", user_id, user_id_attr, user_id],
            ["add-triple", user_id, handle_attr, "alexandra"]
        ]),
        &catalog,
    )
    .unwrap();
    let receipt = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &changed_with_id,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(receipt.mutations.len(), 2);
    assert!(
        receipt
            .mutations
            .iter()
            .any(|mutation| { mutation.entity_id == user_id && mutation.attr_id == user_id_attr })
    );

    let retract_and_delete = compile_transaction(
        &json!([
            [
                "retract-triple",
                user_id,
                shelves_attr,
                [shelf_id_attr, second_shelf_id]
            ],
            ["delete-entity", shelf_id]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &retract_and_delete,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(value(&client, app_id, shelf_id, shelf_id_attr).await, None);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND attr_id = $2",
                &[&app_id, &shelves_attr],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2,
        "the two unrelated links written after the schema update remain"
    );

    let reverse_user = uuid("aaaaaaaa-0000-0000-0000-000000000006");
    let reverse_shelf = uuid("aaaaaaaa-0000-0000-0000-000000000007");
    let mut reverse_cascade_catalog = catalog.clone();
    reverse_cascade_catalog
        .attributes
        .iter_mut()
        .find(|attribute| attribute.id == shelves_attr)
        .unwrap()
        .on_delete_reverse = Some(DeleteAction::Cascade);
    let reverse_setup = compile_transaction(
        &json!([
            ["add-triple", reverse_user, user_id_attr, reverse_user],
            ["add-triple", reverse_shelf, shelf_id_attr, reverse_shelf],
            ["add-triple", reverse_user, shelves_attr, reverse_shelf]
        ]),
        &reverse_cascade_catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &reverse_cascade_catalog,
        &reverse_setup,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let reverse_delete = compile_transaction(
        &json!([["delete-entity", reverse_user, "users"]]),
        &reverse_cascade_catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &reverse_cascade_catalog,
        &reverse_delete,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, reverse_shelf, shelf_id_attr).await,
        None
    );

    let forward_user = uuid("aaaaaaaa-0000-0000-0000-000000000008");
    let forward_shelf = uuid("aaaaaaaa-0000-0000-0000-000000000009");
    let mut forward_cascade_catalog = catalog.clone();
    forward_cascade_catalog
        .attributes
        .iter_mut()
        .find(|attribute| attribute.id == shelves_attr)
        .unwrap()
        .on_delete = Some(DeleteAction::Cascade);
    let forward_setup = compile_transaction(
        &json!([
            ["add-triple", forward_user, user_id_attr, forward_user],
            ["add-triple", forward_shelf, shelf_id_attr, forward_shelf],
            ["add-triple", forward_user, shelves_attr, forward_shelf]
        ]),
        &forward_cascade_catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &forward_cascade_catalog,
        &forward_setup,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let forward_delete = compile_transaction(
        &json!([["delete-entity", forward_shelf, "bookshelves"]]),
        &forward_cascade_catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &forward_cascade_catalog,
        &forward_delete,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, forward_user, user_id_attr).await,
        None
    );

    // Clojure expands cascades while preprocessing, against the pre-write
    // snapshot. A link introduced earlier in the same request therefore must
    // not cause its existing source entity to be deleted. The delete still
    // removes the newly-added dangling link when it removes the target.
    let late_link_user = uuid("aaaaaaaa-0000-0000-0000-00000000000a");
    let late_link_shelf = uuid("aaaaaaaa-0000-0000-0000-00000000000b");
    let late_link_setup = compile_transaction(
        &json!([
            ["add-triple", late_link_user, user_id_attr, late_link_user],
            [
                "add-triple",
                late_link_shelf,
                shelf_id_attr,
                late_link_shelf
            ]
        ]),
        &forward_cascade_catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &forward_cascade_catalog,
        &late_link_setup,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let add_link_then_delete = compile_transaction(
        &json!([
            ["add-triple", late_link_user, shelves_attr, late_link_shelf],
            ["delete-entity", late_link_shelf, "bookshelves"]
        ]),
        &forward_cascade_catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &forward_cascade_catalog,
        &add_link_then_delete,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, late_link_user, user_id_attr).await,
        Some(json!(late_link_user)),
        "same-transaction links are not inputs to pre-write cascade expansion"
    );
    assert_eq!(
        value(&client, app_id, late_link_shelf, shelf_id_attr).await,
        None
    );
    assert_eq!(
        value(&client, app_id, late_link_user, shelves_attr).await,
        None,
        "deleting the target still cleans its incoming reference"
    );

    // Permission projection consumes the same prepared steps as execution.
    // Cascaded children and inherited rule params must be visible before any
    // mutation occurs, rather than being discovered only by the executor.
    let permission_parent = uuid("aaaaaaaa-0000-0000-0000-00000000000c");
    let permission_child = uuid("aaaaaaaa-0000-0000-0000-00000000000d");
    let permission_setup = compile_transaction(
        &json!([
            [
                "add-triple",
                permission_child,
                user_id_attr,
                permission_child
            ],
            [
                "add-triple",
                permission_parent,
                shelf_id_attr,
                permission_parent
            ],
            [
                "add-triple",
                permission_child,
                shelves_attr,
                permission_parent
            ]
        ]),
        &forward_cascade_catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &forward_cascade_catalog,
        &permission_setup,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let permission_delete = compile_transaction(
        &json!([
            ["delete-entity", permission_parent, "bookshelves"],
            ["rule-params", permission_parent, "bookshelves", {"ticket": "cascade"}]
        ]),
        &forward_cascade_catalog,
    )
    .unwrap();
    let prepare_tx = client.transaction().await.unwrap();
    let prepared = prepare_transaction_lookups(
        &prepare_tx,
        app_id,
        &forward_cascade_catalog,
        &permission_delete,
    )
    .await
    .unwrap();
    assert!(prepared.steps.iter().any(|step| matches!(
        step,
        TransactionStep::DeleteEntity(delete)
            if delete.entity == EntityLookup::Id(permission_child)
                && delete.etype.as_deref() == Some("users")
    )));
    assert!(prepared.steps.iter().any(|step| matches!(
        step,
        TransactionStep::RuleParams(params)
            if params.entity == EntityLookup::Id(permission_child)
                && params.etype.as_deref() == Some("users")
                && params.params.get("ticket") == Some(&json!("cascade"))
    )));
    prepare_tx.rollback().await.unwrap();

    let transaction_count: i64 = client
        .query_one("SELECT count(*) FROM transactions", &[])
        .await
        .unwrap()
        .get(0);
    let invalid_create = compile_transaction(
        &json!([["add-triple", user_id, handle_attr, "again", {"mode": "create"}]]),
        &catalog,
    )
    .unwrap();
    assert!(matches!(
        execute_transaction(
            &mut client,
            app_id,
            &catalog,
            &invalid_create,
            TransactionOptions::default()
        )
        .await,
        Err(ExecutionError::CreateExisting(_))
    ));
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        transaction_count,
        "failed transactions roll back their transaction record"
    );

    let missing_required_id = uuid("aaaaaaaa-0000-0000-0000-000000000005");
    let mut required_catalog = catalog.clone();
    required_catalog
        .attributes
        .iter_mut()
        .find(|attribute| attribute.id == nickname_attr)
        .unwrap()
        .required = true;
    let missing_required = compile_transaction(
        &json!([
            [
                "add-triple",
                missing_required_id,
                user_id_attr,
                missing_required_id
            ],
            [
                "add-triple",
                missing_required_id,
                handle_attr,
                "missing-nickname"
            ]
        ]),
        &required_catalog,
    )
    .unwrap();
    let error = execute_transaction(
        &mut client,
        app_id,
        &required_catalog,
        &missing_required,
        TransactionOptions::default(),
    )
    .await
    .unwrap_err();
    let ExecutionError::MissingRequired(missing) = error else {
        panic!("expected a missing-required error, got {error:?}");
    };
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].entity_id, missing_required_id);
    assert_eq!(missing[0].etype, "users");
    assert_eq!(missing[0].label, "nickname");
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &missing_required_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "required validation rolls back requested writes and indexed nulls"
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        transaction_count,
        "required validation also rolls back its transaction record"
    );

    let admin_user = uuid("aaaaaaaa-0000-0000-0000-000000000101");
    let admin_shelf = uuid("aaaaaaaa-0000-0000-0000-000000000102");
    let admin_steps = compile_admin_transaction(
        &json!([
            ["create", "users", admin_user, {"handle": "admin-tuple"}],
            ["create", "bookshelves", admin_shelf, {}],
            ["link", "users", admin_user, {"bookshelves": admin_shelf}]
        ]),
        &catalog,
        true,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &admin_steps,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, admin_user, handle_attr).await,
        Some(json!("admin-tuple"))
    );
    assert_eq!(
        value(&client, app_id, admin_user, shelves_attr).await,
        Some(json!(admin_shelf))
    );
    let admin_followup = compile_admin_transaction(
        &json!([
            ["merge", "users", admin_user, {"handle": "admin-merged"}],
            ["unlink", "users", admin_user, {"bookshelves": admin_shelf}],
            ["ruleParams", "users", admin_user, {"source": "admin"}]
        ]),
        &catalog,
        true,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &admin_followup,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, admin_user, handle_attr).await,
        Some(json!("admin-merged"))
    );
    assert_eq!(value(&client, app_id, admin_user, shelves_attr).await, None);
    let lookup_update = compile_admin_transaction(
        &json!([[
            "update",
            "users",
            "lookup__handle__\"admin-merged\"",
            {"handle": "lookup-updated"}
        ]]),
        &catalog,
        true,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &lookup_update,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        value(&client, app_id, admin_user, user_id_attr).await,
        Some(json!(admin_user)),
        "lookup updates must preserve the UUID ID marker"
    );
    assert_eq!(
        value(&client, app_id, admin_user, handle_attr).await,
        Some(json!("lookup-updated"))
    );

    // A UUID may legally be used in more than one entity namespace. The
    // legacy transaction API permits delete-entity without an etype and must
    // remove every typed projection of that UUID, not just the first one.
    let shared_entity_id = uuid("aaaaaaaa-0000-0000-0000-000000000201");
    let shared_entity = compile_transaction(
        &json!([
            [
                "add-triple",
                shared_entity_id,
                user_id_attr,
                shared_entity_id
            ],
            [
                "add-triple",
                shared_entity_id,
                shelf_id_attr,
                shared_entity_id
            ]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &shared_entity,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &shared_entity_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        5,
        "two requested ID markers plus three indexed-null sentinels"
    );
    let delete_all_namespaces =
        compile_transaction(&json!([["delete-entity", shared_entity_id]]), &catalog).unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &delete_all_namespaces,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM triples WHERE app_id = $1 AND entity_id = $2",
                &[&app_id, &shared_entity_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "delete-entity without etype removes every namespace projection"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/src/instant/db/transaction.clj:564-578 transact-without-tx-conn-impl!
    // Status rejection happens at the base execution boundary before the
    // transaction record (the first write) and before any triples.
    let status_entity_id = uuid("aaaaaaaa-0000-0000-0000-000000000250");
    let status_write = compile_transaction(
        &json!([
            [
                "add-triple",
                status_entity_id,
                user_id_attr,
                status_entity_id
            ],
            ["add-triple", status_entity_id, handle_attr, "status-check"]
        ]),
        &catalog,
    )
    .unwrap();
    let transactions_before_status = client
        .query_one("SELECT count(*) FROM transactions", &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    let triples_before_status = client
        .query_one("SELECT count(*) FROM triples", &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    client
        .execute(
            "UPDATE apps SET status = 'read-only' WHERE id = $1",
            &[&app_id],
        )
        .await
        .unwrap();
    assert!(matches!(
        execute_transaction(
            &mut client,
            app_id,
            &catalog,
            &status_write,
            TransactionOptions::default(),
        )
        .await,
        Err(ExecutionError::AppReadOnly)
    ));
    client
        .execute(
            "UPDATE apps SET status = 'disabled' WHERE id = $1",
            &[&app_id],
        )
        .await
        .unwrap();
    assert!(matches!(
        execute_transaction(
            &mut client,
            app_id,
            &catalog,
            &status_write,
            TransactionOptions::default(),
        )
        .await,
        Err(ExecutionError::AppDisabled)
    ));
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_status
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM triples", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        triples_before_status
    );
    execute_transaction_with_app_status_write_check(
        &mut client,
        app_id,
        &catalog,
        &status_write,
        TransactionOptions::default(),
        AppStatusWriteCheck::Skip,
    )
    .await
    .unwrap();
    client
        .execute(
            "UPDATE apps SET status = 'active' WHERE id = $1",
            &[&app_id],
        )
        .await
        .unwrap();

    // The caller's catalog can become stale between planning and execution.
    // Pinned upstream uses a LEFT JOIN plus raise_exception_message so a
    // concurrently soft-deleted attribute aborts the whole transaction rather
    // than silently filtering the requested write out of the insert pipeline.
    let transactions_before_stale_attr = client
        .query_one("SELECT count(*) FROM transactions", &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    client
        .execute(
            "UPDATE attrs SET deletion_marked_at = now() WHERE app_id = $1 AND id = $2",
            &[&app_id, &handle_attr],
        )
        .await
        .unwrap();
    let stale_entity_id = uuid("aaaaaaaa-0000-0000-0000-000000000301");
    let stale_write = compile_transaction(
        &json!([[
            "add-triple",
            stale_entity_id,
            handle_attr,
            "must-not-disappear"
        ]]),
        &catalog,
    )
    .unwrap();
    let stale_error = execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &stale_write,
        TransactionOptions::default(),
    )
    .await
    .unwrap_err();
    let ExecutionError::Postgres(stale_error) = stale_error else {
        panic!("expected stale attribute to fail in PostgreSQL, got {stale_error:?}");
    };
    assert!(
        stale_error
            .as_db_error()
            .is_some_and(|error| error.message().contains(&handle_attr.to_string()))
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        transactions_before_stale_attr,
        "the stale-attribute failure rolls back its transaction record"
    );
    assert_eq!(
        value(&client, app_id, stale_entity_id, handle_attr).await,
        None,
        "a stale attribute cannot turn an accepted write into a silent no-op"
    );

    // Oracle: instantdb/instant@4e0a1bd8
    // server/test/instant/db/transaction_test.clj:409-516 update-modes
    // Exercise the complete mode/entity-shape matrix through the native SQL
    // executor. In particular, lookup creation must not make `update` pass,
    // and a failed mode check must precede the durable transaction record.
    let mode_book_id = uuid("aaaaaaaa-1000-0000-0000-000000000300");
    let book_id_attr = uuid("77777777-7777-7777-7777-777777777777");
    let book_title_attr = uuid("88888888-8888-8888-8888-888888888888");
    let mode_seed = compile_transaction(
        &json!([
            ["add-triple", mode_book_id, book_id_attr, mode_book_id],
            ["add-triple", mode_book_id, book_title_attr, "mode-existing"]
        ]),
        &catalog,
    )
    .unwrap();
    execute_transaction(
        &mut client,
        app_id,
        &catalog,
        &mode_seed,
        TransactionOptions::default(),
    )
    .await
    .unwrap();
    let mode_fixture: UpdateModeFixture =
        serde_json::from_str(include_str!("fixtures/update_modes.json")).unwrap();
    assert!(mode_fixture.oracle.starts_with("4e0a1bd8 "));
    for case in mode_fixture.cases {
        let steps = compile_transaction(&case.input, &catalog)
            .unwrap_or_else(|error| panic!("{} did not compile: {error}", case.name));
        let tuple = case.input.as_array().unwrap()[0].as_array().unwrap();
        let requested_entity = &tuple[1];
        let expected_value = tuple[3].clone();
        let transactions_before: i64 = client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get(0);
        let result = execute_transaction(
            &mut client,
            app_id,
            &catalog,
            &steps,
            TransactionOptions::default(),
        )
        .await;

        match case.outcome.as_str() {
            "ok" => {
                let receipt =
                    result.unwrap_or_else(|error| panic!("{} failed: {error}", case.name));
                assert!(!receipt.mutations.is_empty(), "{}", case.name);
                let entity_id = if let Some(entity_id) = requested_entity.as_str() {
                    uuid(entity_id)
                } else {
                    let lookup = requested_entity.as_array().unwrap();
                    client
                        .query_one(
                            "SELECT entity_id FROM triples
                             WHERE app_id = $1 AND attr_id = $2 AND value = $3 AND av",
                            &[
                                &app_id,
                                &uuid(lookup[0].as_str().unwrap()),
                                &Json(lookup[1].clone()),
                            ],
                        )
                        .await
                        .unwrap()
                        .get(0)
                };
                assert_eq!(
                    value(&client, app_id, entity_id, book_title_attr).await,
                    Some(expected_value),
                    "{}",
                    case.name
                );
                assert_eq!(
                    client
                        .query_one("SELECT count(*) FROM transactions", &[])
                        .await
                        .unwrap()
                        .get::<_, i64>(0),
                    transactions_before + 1,
                    "{}",
                    case.name
                );
            }
            "create-existing" => {
                assert!(
                    matches!(result, Err(ExecutionError::CreateExisting(_))),
                    "{}: {result:?}",
                    case.name
                );
                assert_eq!(
                    client
                        .query_one("SELECT count(*) FROM transactions", &[])
                        .await
                        .unwrap()
                        .get::<_, i64>(0),
                    transactions_before,
                    "{}",
                    case.name
                );
            }
            "update-missing" => {
                assert!(
                    matches!(result, Err(ExecutionError::UpdateMissing(_))),
                    "{}: {result:?}",
                    case.name
                );
                assert_eq!(
                    client
                        .query_one("SELECT count(*) FROM transactions", &[])
                        .await
                        .unwrap()
                        .get::<_, i64>(0),
                    transactions_before,
                    "{}",
                    case.name
                );
            }
            outcome => panic!("{} has unknown outcome {outcome}", case.name),
        }
    }

    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn statement_timeout_rolls_back_and_releases_the_transaction_connection() {
    let Some(database_url) = instant_test_support::test_database_url(
        "statement_timeout_rolls_back_and_releases_the_transaction_connection",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    let transaction = client.transaction().await.unwrap();
    configure_transaction_statement_timeout(&transaction, Duration::from_millis(10))
        .await
        .unwrap();

    let error = transaction
        .query_one("SELECT pg_sleep(10)", &[])
        .await
        .unwrap_err();
    assert!(matches!(
        ExecutionError::from(error),
        ExecutionError::QueryTimeout
    ));
    transaction.rollback().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT 1::bigint", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );

    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn caller_owned_transaction_remains_usable_after_prewrite_rejection() {
    let Some(database_url) = instant_test_support::test_database_url(
        "caller_owned_transaction_remains_usable_after_prewrite_rejection",
    ) else {
        return;
    };
    let (mut client, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let connection_task = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TEMP TABLE apps (
                 id uuid PRIMARY KEY,
                 status text NOT NULL,
                 deletion_marked_at timestamptz
             );
             CREATE TEMP TABLE attrs (app_id uuid, id uuid, etype text, label text);
             CREATE TEMP TABLE triples (app_id uuid, entity_id uuid, attr_id uuid, value jsonb);
             CREATE TEMP TABLE transactions (
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 app_id uuid NOT NULL,
                 created_at timestamp NOT NULL DEFAULT now()
             );",
        )
        .await
        .unwrap();
    let app_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO apps (id, status) VALUES ($1, 'read-only')",
            &[&app_id],
        )
        .await
        .unwrap();

    let transaction = client.transaction().await.unwrap();
    let result = execute_transaction_in_with_app_status_write_check(
        &transaction,
        app_id,
        &AttributeCatalog::default(),
        &[],
        TransactionOptions::default(),
        AppStatusWriteCheck::default(),
    )
    .await;
    assert!(matches!(result, Err(ExecutionError::AppReadOnly)));
    assert_eq!(
        transaction
            .query_one("SELECT 1::bigint", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1,
        "the caller, rather than the inner executor, owns rollback or commit after an error"
    );
    transaction.commit().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM transactions", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "pre-write app-status rejection does not create a transaction record"
    );

    drop(client);
    connection_task.await.unwrap().unwrap();
}
