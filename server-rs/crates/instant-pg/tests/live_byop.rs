use instant_instaql::{AttributeValueType, coerce_forms};
use instant_pg::{
    ByopRelationDirection, execute_byop_instaql, introspect_byop_schema, query_byop_reference,
    render_byop_query,
};
use serde_json::json;
use tokio_postgres_rustls::MakeRustlsConnect;
use uuid::Uuid;

#[tokio::test]
async fn cleanup_managed_provider_schemas() {
    if std::env::var("INSTANT_BYOP_PROVIDER_CLEANUP_ONLY").as_deref() != Ok("1") {
        return;
    }
    assert_eq!(
        std::env::var("INSTANT_BYOP_PROVIDER_GATE_CONFIRM").as_deref(),
        Ok("dedicated-provider-test-databases")
    );
    let database_url =
        instant_test_support::test_database_url("cleanup_managed_provider_schemas").unwrap();
    let run_id = provider_run_id();
    let (client, connection) =
        tokio_postgres::connect(&database_url, MakeRustlsConnect::with_webpki_roots())
            .await
            .unwrap();
    let connection_task = tokio::spawn(connection);
    let schemas = client
        .query(
            "SELECT nspname FROM pg_catalog.pg_namespace
             WHERE nspname LIKE $1 ESCAPE '\\'",
            &[&format!("ibyop\\_{run_id}\\_%")],
        )
        .await
        .unwrap();
    for row in schemas {
        let schema = row.get::<_, String>(0);
        assert!(
            schema
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        );
        client
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .await
            .unwrap();
    }
    drop(client);
    connection_task.await.unwrap().unwrap();
}

fn provider_run_id() -> String {
    let run_id = std::env::var("INSTANT_BYOP_PROVIDER_RUN_ID").unwrap();
    assert!(!run_id.is_empty() && run_id.len() <= 12);
    assert!(
        run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    );
    run_id
}

fn schema_name(local_prefix: &str, kind: &str) -> String {
    std::env::var("INSTANT_BYOP_PROVIDER_RUN_ID").map_or_else(
        |_| format!("{local_prefix}_{}", Uuid::new_v4().simple()),
        |_| {
            format!(
                "ibyop_{}_{}_{}",
                provider_run_id(),
                kind,
                Uuid::new_v4().simple()
            )
        },
    )
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // One disposable schema verifies introspection and nested execution together.
async fn introspection_builds_stable_fields_primary_keys_and_queryable_relations() {
    let Some(database_url) = instant_test_support::test_database_url(
        "introspection_builds_stable_fields_primary_keys_and_queryable_relations",
    ) else {
        return;
    };
    let (client, connection) =
        tokio_postgres::connect(&database_url, MakeRustlsConnect::with_webpki_roots())
            .await
            .unwrap();
    let connection_task = tokio::spawn(connection);
    let namespace = schema_name("instant_byop", "basic");
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {namespace};
             CREATE TYPE {namespace}.post_status AS ENUM ('draft', 'published');
             CREATE DOMAIN {namespace}.positive_score AS integer CHECK (VALUE > 0);
             CREATE DOMAIN {namespace}.instant_timestamp AS timestamptz;
             CREATE TABLE {namespace}.users (
               id uuid PRIMARY KEY,
               email text UNIQUE NOT NULL,
               nickname text,
               created_at {namespace}.instant_timestamp NOT NULL
             );
             CREATE INDEX ON {namespace}.users(nickname);
             CREATE TABLE {namespace}.posts (
               id bigint PRIMARY KEY,
               author_id uuid NOT NULL REFERENCES {namespace}.users(id),
               title text NOT NULL,
               tags text[] NOT NULL,
               status {namespace}.post_status NOT NULL,
               score {namespace}.positive_score NOT NULL,
               created_on date NOT NULL
             );
             CREATE INDEX ON {namespace}.posts(author_id);
             CREATE UNIQUE INDEX posts_title_unique ON {namespace}.posts(title);
             CREATE TABLE {namespace}.comments (
               id bigint PRIMARY KEY,
               author_id uuid NOT NULL REFERENCES {namespace}.users(id),
               body text NOT NULL
             );
             CREATE TABLE {namespace}.array_keys (
               id text[] PRIMARY KEY,
               label text NOT NULL
             );
             CREATE TABLE {namespace}.compound (
               left_id bigint NOT NULL,
               right_id bigint NOT NULL,
               PRIMARY KEY (left_id, right_id)
             );
             CREATE TABLE {namespace}.compound_links (
               left_id bigint NOT NULL,
               right_id bigint NOT NULL,
               user_id uuid NOT NULL REFERENCES {namespace}.users(id),
               PRIMARY KEY (left_id, right_id)
             );
             CREATE TABLE {namespace}.partial_uniques (
               id bigint PRIMARY KEY,
               email text,
               active boolean NOT NULL
             );
             CREATE UNIQUE INDEX partial_uniques_email_unique
               ON {namespace}.partial_uniques(email) WHERE active;"
        ))
        .await
        .unwrap();

    let schema = introspect_byop_schema(&client, &namespace).await.unwrap();
    assert_eq!(schema.tables.len(), 5);
    assert!(!schema.tables.contains_key("compound"));
    assert!(!schema.tables.contains_key("compound_links"));
    assert!(!schema.tables["partial_uniques"].fields["email"].unique);

    let users = &schema.tables["users"];
    assert_eq!(users.primary_key.field, "id");
    assert_eq!(users.created_at_field.as_deref(), Some("created_at"));
    assert!(users.fields["id"].indexed);
    assert!(users.fields["email"].unique);
    assert_eq!(
        users.attribute_id_to_field[&users.fields["email"].attribute_id],
        "email"
    );

    let posts = &schema.tables["posts"];
    assert_eq!(posts.created_at_field.as_deref(), Some("created_on"));
    assert!(!posts.fields["title"].unique);
    assert_eq!(
        posts.relations["users"].direction,
        ByopRelationDirection::Forward
    );
    assert_eq!(posts.relations["users"].table_field, "author_id");
    assert_eq!(posts.relations["users"].other_table_field, "id");
    assert_eq!(
        users.relations["posts"].direction,
        ByopRelationDirection::Reverse
    );
    assert!(!schema.tables["comments"].relations.contains_key("users"));
    assert!(users.relations.contains_key("comments"));
    assert!(users.relations.contains_key("compound_links"));

    let relation_attribute = schema
        .catalog
        .forward("posts", "users")
        .expect("indexed foreign key becomes a reference attribute");
    assert_eq!(relation_attribute.value_type, AttributeValueType::Ref);
    assert_eq!(relation_attribute.reverse_etype.as_deref(), Some("users"));
    assert_eq!(relation_attribute.reverse_label.as_deref(), Some("posts"));
    assert!(schema.catalog.forward("comments", "users").is_none());

    let users_oid: u32 = client
        .query_one(
            "SELECT c.oid
               FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = 'users'",
            &[&namespace],
        )
        .await
        .unwrap()
        .get(0);
    let email_attnum: i16 = client
        .query_one(
            "SELECT a.attnum
               FROM pg_catalog.pg_attribute a
              WHERE a.attrelid = $1 AND a.attname = 'email'",
            &[&users_oid],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        users.fields["email"].attribute_id,
        Uuid::from_u64_pair(u64::from(users_oid), u64::try_from(email_attnum).unwrap())
    );

    let user_id = Uuid::new_v4();
    let second_user_id = Uuid::new_v4();
    let null_nickname_user_id = Uuid::new_v4();
    client
        .execute(
            &format!(
                "INSERT INTO {namespace}.users (id, email, nickname, created_at)
                 VALUES ($1, $2, 'alpha', now()),
                        ($3, $4, 'zulu', now()),
                        ($5, 'null@example.com', NULL, now())"
            ),
            &[
                &user_id,
                &"oren@example.com",
                &second_user_id,
                &"zara@example.com",
                &null_nickname_user_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!(
                "INSERT INTO {namespace}.array_keys (id, label)
                 VALUES (ARRAY['compound', 'key'], 'array primary key')"
            ),
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!(
                "INSERT INTO {namespace}.posts
                   (id, author_id, title, tags, status, score, created_on)
                 VALUES (1, $1, 'Rust database port', ARRAY['rust', 'db'], 'published', 7, current_date),
                        (2, $1, 'Rust query engine', ARRAY['rust', 'query'], 'draft', 3, current_date)"
            ),
            &[&user_id],
        )
        .await
        .unwrap();
    let forms = coerce_forms(
        &json!({
            "users": {
                "$": {
                    "where": {"posts.title": {"$ilike": "rust%"}},
                    "order": {"email": "asc"},
                    "limit": 1
                },
                "posts": {
                    "$": {
                        "where": {
                            "title": {"$like": "Rust%"},
                            "status": "published",
                            "score": {"$gt": 5}
                        },
                        "order": {"id": "desc"},
                        "limit": 2
                    }
                }
            }
        }),
        &schema.catalog,
    )
    .unwrap();
    let rendered = render_byop_query(&namespace, &schema, &forms, false).unwrap();
    assert!(!rendered.sql.contains("rust%"));
    let execution = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "users": {
                "$": {
                    "where": {"posts.title": {"$ilike": "rust%"}},
                    "order": {"email": "asc"},
                    "limit": 1
                },
                "posts": {
                    "$": {
                        "where": {
                            "title": {"$like": "Rust%"},
                            "status": "published",
                            "score": {"$gt": 5}
                        },
                        "order": {"id": "desc"},
                        "limit": 2
                    }
                }
            }
        }),
        false,
        false,
    )
    .await
    .unwrap();
    let result = &execution.raw;
    assert_eq!(result[0]["k"], "users");
    assert_eq!(
        result[0]["data"]["rows"][0]["row"]["email"],
        "oren@example.com"
    );
    assert_eq!(
        result[0]["data"]["rows"][0]["children"][0]["data"]["rows"][0]["row"]["id"],
        1
    );
    assert_eq!(
        execution.response.data["users"][0]["email"],
        "oren@example.com"
    );
    assert!(!execution.topics.is_empty());

    // BYOP cursors contain the arbitrary JSON primary key, the synthetic
    // attribute id, and the ordered field value.  Exercise the actual SQL
    // boundary rather than only the renderer: the second page must advance
    // strictly past the first user's email while retaining the typed UUID key.
    let second_page = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "users": {
                "$": {
                    "order": {"email": "asc"},
                    "after": [user_id, users.fields["email"].attribute_id, "oren@example.com"],
                    "limit": 1
                }
            }
        }),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        second_page.response.data["users"][0]["id"],
        json!(second_user_id)
    );
    assert_eq!(
        second_page.response.data["users"][0]["email"],
        "zara@example.com"
    );
    assert_eq!(
        second_page.response.page_info["users"]
            .start_cursor
            .as_ref()
            .unwrap()[0],
        json!(second_user_id)
    );

    // PostgreSQL sorts NULL after non-NULL values in ascending order. The
    // boundary predicate must explicitly cross that partition because
    // `nickname > 'zulu'` alone evaluates to UNKNOWN for the final row.
    let null_page = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "users": {
                "$": {
                    "order": {"nickname": "asc"},
                    "after": [
                        second_user_id,
                        users.fields["nickname"].attribute_id,
                        "zulu"
                    ],
                    "limit": 1
                }
            }
        }),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        null_page.response.data["users"][0]["id"],
        json!(null_nickname_user_id)
    );
    assert!(null_page.response.data["users"][0]["nickname"].is_null());

    assert_eq!(
        query_byop_reference(
            &client,
            &namespace,
            &schema,
            "posts",
            json!(1),
            "users.email",
        )
        .await
        .unwrap(),
        vec![json!("oren@example.com")]
    );
    assert_eq!(
        query_byop_reference(
            &client,
            &namespace,
            &schema,
            "users",
            json!(user_id),
            "posts.id",
        )
        .await
        .unwrap(),
        vec![json!(1), json!(2)]
    );
    assert_eq!(
        query_byop_reference(
            &client,
            &namespace,
            &schema,
            "array_keys",
            json!(["compound", "key"]),
            "label",
        )
        .await
        .unwrap(),
        vec![json!("array primary key")]
    );

    client
        .batch_execute(&format!("DROP SCHEMA {namespace} CASCADE"))
        .await
        .unwrap();
    drop(client);
    connection_task.await.unwrap().unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // This disposable namespace is the PostgreSQL type and catalog eligibility matrix.
async fn postgres_schema_matrix_coerces_values_and_refreshes_ddl() {
    let Some(database_url) = instant_test_support::test_database_url(
        "postgres_schema_matrix_coerces_values_and_refreshes_ddl",
    ) else {
        return;
    };
    let (client, connection) =
        tokio_postgres::connect(&database_url, MakeRustlsConnect::with_webpki_roots())
            .await
            .unwrap();
    let connection_task = tokio::spawn(connection);
    let namespace = schema_name("instant_byop_matrix", "matrix");
    let other_namespace = schema_name("instant_byop_other", "other");
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {namespace};
             CREATE SCHEMA {other_namespace};
             CREATE TYPE {namespace}.state AS ENUM ('draft', 'published');
             CREATE DOMAIN {namespace}.positive_integer AS integer CHECK (VALUE > 0);
             CREATE TABLE {namespace}.parents (
               id uuid PRIMARY KEY,
               label text NOT NULL
             );
             CREATE TABLE {namespace}.coercions (
               id uuid PRIMARY KEY,
               boolean_value boolean NOT NULL,
               small_value smallint NOT NULL,
               integer_value integer NOT NULL,
               decimal_value numeric(6,2) NOT NULL,
               short_text varchar(5) NOT NULL,
               fixed_code char(3) NOT NULL,
               labels text[] NOT NULL,
               numbers integer[] NOT NULL,
               state {namespace}.state NOT NULL,
               score {namespace}.positive_integer NOT NULL,
               span int4range NOT NULL,
               created_at timestamptz(0) NOT NULL
             );
             CREATE INDEX coercions_short_text_idx ON {namespace}.coercions(short_text);
             CREATE TABLE {namespace}.unique_matrix (
               id uuid PRIMARY KEY,
               by_constraint text UNIQUE,
               by_index text,
               partial_value text,
               left_value integer,
               right_value integer,
               UNIQUE (left_value, right_value)
             );
             CREATE UNIQUE INDEX unique_matrix_by_index_idx
               ON {namespace}.unique_matrix(by_index);
             CREATE UNIQUE INDEX unique_matrix_partial_idx
               ON {namespace}.unique_matrix(partial_value) WHERE partial_value IS NOT NULL;
             CREATE TABLE {namespace}.eligible_children (
               id uuid PRIMARY KEY,
               parent_id uuid NOT NULL REFERENCES {namespace}.parents(id),
               label text NOT NULL
             );
             CREATE INDEX eligible_children_parent_idx
               ON {namespace}.eligible_children(parent_id, label);
             CREATE TABLE {namespace}.partial_children (
               id uuid PRIMARY KEY,
               parent_id uuid NOT NULL REFERENCES {namespace}.parents(id),
               active boolean NOT NULL
             );
             CREATE INDEX partial_children_parent_idx
               ON {namespace}.partial_children(parent_id) WHERE active;
             CREATE TABLE {namespace}.trailing_children (
               id uuid PRIMARY KEY,
               parent_id uuid NOT NULL REFERENCES {namespace}.parents(id),
               label text NOT NULL
             );
             CREATE INDEX trailing_children_parent_idx
               ON {namespace}.trailing_children(label, parent_id);
             CREATE TABLE {namespace}.unindexed_children (
               id uuid PRIMARY KEY,
               parent_id uuid NOT NULL REFERENCES {namespace}.parents(id)
             );
             CREATE TABLE {namespace}.one_to_one_primary (
               id uuid PRIMARY KEY REFERENCES {namespace}.parents(id)
             );
             CREATE TABLE {namespace}.one_to_one_unique (
               id uuid PRIMARY KEY,
               parent_id uuid UNIQUE NOT NULL REFERENCES {namespace}.parents(id)
             );
             CREATE TABLE {other_namespace}.foreign_parent (id uuid PRIMARY KEY);
             CREATE TABLE {other_namespace}.isolated (id uuid PRIMARY KEY);
             CREATE TABLE {namespace}.cross_namespace_child (
               id uuid PRIMARY KEY,
               parent_id uuid NOT NULL REFERENCES {other_namespace}.foreign_parent(id)
             );
             CREATE INDEX cross_namespace_child_parent_idx
               ON {namespace}.cross_namespace_child(parent_id);
             CREATE TABLE {namespace}.array_primary_keys (
               id text[] PRIMARY KEY,
               label text NOT NULL
             );
             CREATE TABLE {namespace}.composite_primary_keys (
               left_id uuid NOT NULL,
               right_id uuid NOT NULL,
               PRIMARY KEY (left_id, right_id)
             );
             CREATE TABLE {namespace}.without_primary_key (id uuid NOT NULL);
             CREATE VIEW {namespace}.parent_view AS SELECT * FROM {namespace}.parents;
             CREATE TABLE {namespace}.cursor_rows (
               id uuid PRIMARY KEY,
               sortable text
             );
             CREATE INDEX cursor_rows_sortable_idx ON {namespace}.cursor_rows(sortable);"
        ))
        .await
        .unwrap();

    let initial = introspect_byop_schema(&client, &namespace).await.unwrap();
    assert!(initial.tables.contains_key("array_primary_keys"));
    assert!(!initial.tables.contains_key("composite_primary_keys"));
    assert!(!initial.tables.contains_key("without_primary_key"));
    assert!(!initial.tables.contains_key("parent_view"));
    assert!(!initial.tables.contains_key("isolated"));
    assert!(!initial.tables.contains_key("ddl_added"));

    let coercions = &initial.tables["coercions"];
    assert!(coercions.fields["short_text"].indexed);
    assert_eq!(coercions.fields["state"].database_type, "state");
    assert_eq!(coercions.fields["score"].database_type, "positive_integer");
    assert_eq!(coercions.created_at_field.as_deref(), Some("created_at"));
    let unique_matrix = &initial.tables["unique_matrix"];
    assert!(unique_matrix.fields["by_constraint"].unique);
    assert!(!unique_matrix.fields["by_index"].unique);
    assert!(!unique_matrix.fields["partial_value"].unique);
    assert!(unique_matrix.fields["partial_value"].indexed);
    assert!(!unique_matrix.fields["left_value"].unique);
    assert!(!unique_matrix.fields["right_value"].unique);

    let eligible = &initial.tables["eligible_children"];
    assert!(eligible.fields["parent_id"].indexed);
    assert_eq!(
        eligible.relations["parents"].direction,
        ByopRelationDirection::Forward
    );
    assert_eq!(
        initial.tables["parents"].relations["eligible_children"].direction,
        ByopRelationDirection::Reverse
    );
    assert!(initial.tables["partial_children"].fields["parent_id"].indexed);
    assert!(
        initial.tables["partial_children"]
            .relations
            .contains_key("parents")
    );
    for table in ["trailing_children", "unindexed_children"] {
        assert!(
            !initial.tables[table].fields["parent_id"].indexed,
            "{table}"
        );
        assert!(initial.tables["parents"].relations.contains_key(table));
        assert!(
            !initial.tables[table].relations.contains_key("parents"),
            "{table}"
        );
    }
    assert!(
        initial.tables["one_to_one_primary"]
            .relations
            .contains_key("parents")
    );
    assert!(
        initial.tables["one_to_one_unique"]
            .relations
            .contains_key("parents")
    );
    assert!(
        initial.tables["cross_namespace_child"]
            .relations
            .contains_key("foreign_parent")
    );

    client
        .batch_execute(&format!(
            "CREATE TABLE {namespace}.ddl_added (
               id uuid PRIMARY KEY,
               created_on date NOT NULL
             )"
        ))
        .await
        .unwrap();
    let schema = introspect_byop_schema(&client, &namespace).await.unwrap();
    assert!(schema.tables.contains_key("ddl_added"));
    assert_eq!(
        schema.tables["ddl_added"].created_at_field.as_deref(),
        Some("created_on")
    );

    let parent_id = Uuid::new_v4();
    let coercion_id = Uuid::new_v4();
    let unique_link_id = Uuid::new_v4();
    client
        .execute(
            &format!("INSERT INTO {namespace}.parents (id, label) VALUES ($1, 'parent')"),
            &[&parent_id],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!(
                "INSERT INTO {namespace}.coercions
                   (id, boolean_value, small_value, integer_value, decimal_value, short_text,
                    fixed_code, labels, numbers, state, score, span, created_at)
                 VALUES ($1, true, 7, 42, 12.34, 'short', 'abc', ARRAY['one', 'two'],
                         ARRAY[3, 5], 'published', 9, '[1,10)', '2024-01-02T03:04:05Z')"
            ),
            &[&coercion_id],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!("INSERT INTO {namespace}.one_to_one_primary (id) VALUES ($1)"),
            &[&parent_id],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!("INSERT INTO {namespace}.one_to_one_unique (id, parent_id) VALUES ($1, $2)"),
            &[&unique_link_id, &parent_id],
        )
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "INSERT INTO {namespace}.array_primary_keys (id, label)
             VALUES (ARRAY['array', 'primary'], 'array coercion')"
        ))
        .await
        .unwrap();

    let coerced = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "coercions": {
                "$": {
                    "where": {
                        "boolean_value": true,
                        "small_value": 7,
                        "integer_value": 42,
                        "decimal_value": 12.34,
                        "short_text": "short",
                        "fixed_code": "abc",
                        "state": "published",
                        "score": 9,
                        "span": "[1,10)",
                        "created_at": "2024-01-02T03:04:05Z"
                    }
                }
            }
        }),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        coerced.response.data["coercions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        coerced.response.data["coercions"][0]["id"],
        json!(coercion_id)
    );
    assert_eq!(
        coerced.response.data["coercions"][0]["labels"],
        json!(["one", "two"])
    );
    assert_eq!(
        coerced.response.data["coercions"][0]["numbers"],
        json!([3, 5])
    );
    assert_eq!(coerced.response.data["coercions"][0]["state"], "published");
    assert_eq!(coerced.response.data["coercions"][0]["span"], "[1,10)");
    assert_eq!(
        query_byop_reference(
            &client,
            &namespace,
            &schema,
            "array_primary_keys",
            json!(["array", "primary"]),
            "label",
        )
        .await
        .unwrap(),
        vec![json!("array coercion")]
    );

    let inferred = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "parents": {
                "$": {"where": {"id": parent_id}},
                "one_to_one_primary": {},
                "one_to_one_unique": {}
            }
        }),
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        inferred.response.data["parents"][0]["one_to_one_primary"][0]["id"],
        json!(parent_id)
    );
    assert_eq!(
        inferred.response.data["parents"][0]["one_to_one_unique"]["id"],
        json!(unique_link_id)
    );

    let alpha_id = Uuid::new_v4();
    let zulu_id = Uuid::new_v4();
    let null_id = Uuid::new_v4();
    for (id, sortable) in [
        (alpha_id, Some("alpha")),
        (zulu_id, Some("zulu")),
        (null_id, None),
    ] {
        client
            .execute(
                &format!("INSERT INTO {namespace}.cursor_rows (id, sortable) VALUES ($1, $2)"),
                &[&id, &sortable],
            )
            .await
            .unwrap();
    }
    let sortable_attribute = schema.tables["cursor_rows"].fields["sortable"].attribute_id;
    let asc_after_zulu = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "cursor_rows": {"$": {
                "order": {"sortable": "asc"},
                "after": [zulu_id, sortable_attribute, "zulu"],
                "limit": 2
            }}
        }),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        asc_after_zulu.response.data["cursor_rows"][0]["id"],
        json!(null_id)
    );
    let desc_after_null = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "cursor_rows": {"$": {
                "order": {"sortable": "desc"},
                "after": [null_id, sortable_attribute, null],
                "limit": 2
            }}
        }),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        desc_after_null.response.data["cursor_rows"][0]["id"],
        json!(zulu_id)
    );
    let asc_before_null = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "cursor_rows": {"$": {
                "order": {"sortable": "asc"},
                "before": [null_id, sortable_attribute, null],
                "limit": 2
            }}
        }),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        asc_before_null.response.data["cursor_rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].clone())
            .collect::<Vec<_>>(),
        vec![json!(alpha_id), json!(zulu_id)]
    );
    let desc_before_alpha = execute_byop_instaql(
        &client,
        &namespace,
        &schema,
        &json!({
            "cursor_rows": {"$": {
                "order": {"sortable": "desc"},
                "before": [alpha_id, sortable_attribute, "alpha"],
                "limit": 2
            }}
        }),
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        desc_before_alpha.response.data["cursor_rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].clone())
            .collect::<Vec<_>>(),
        vec![json!(null_id), json!(zulu_id)]
    );

    client
        .batch_execute(&format!(
            "DROP SCHEMA {namespace} CASCADE; DROP SCHEMA {other_namespace} CASCADE"
        ))
        .await
        .unwrap();
    drop(client);
    connection_task.await.unwrap().unwrap();
}
