use std::fmt::Write as _;

use instant_server::{
    MigrationError, PostgresConnectionDrivers, SystemCatalogMigrationError,
    SystemCatalogVerificationError, ensure_system_catalog, force_migration_version,
    migrate_database, rollback_database, verify_database_migrations, verify_system_catalog,
};
use sha2::{Digest, Sha256};
use tokio_postgres::NoTls;
use tokio_util::sync::CancellationToken;
use url::Url;

#[path = "../src/native_allocator.rs"]
mod native_allocator;

fn scoped_url(url: &str, schema: &str) -> String {
    let mut url = Url::parse(url).expect("TEST_MIGRATION_DATABASE_URL must be a PostgreSQL URL");
    let mut pairs = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    let mut options = pairs
        .iter()
        .filter(|(key, _)| key == "options")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if !options.is_empty() {
        options.push(' ');
    }
    write!(options, "-csearch_path={schema},public").expect("writing to String cannot fail");
    pairs.retain(|(key, _)| key != "options");
    url.query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("options", &options);
    url.into()
}

#[tokio::test]
async fn cron_timezone_migration_upgrades_utc_only_rows_and_guards_rollback() {
    let Some(database_url) = instant_test_support::test_migration_database_url(
        "cron_timezone_migration_upgrades_utc_only_rows_and_guards_rollback",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("cron_timezone_migration_{}", uuid::Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; CREATE TABLE {schema}.apps (id uuid PRIMARY KEY);"
        ))
        .await
        .unwrap();
    let scoped_url = scoped_url(&database_url, &schema);
    let (client, connection) = tokio_postgres::connect(&scoped_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(include_str!("../migrations/132_execution_outbox.up.sql"))
        .await
        .unwrap();
    let app = uuid::Uuid::new_v4();
    client
        .execute("INSERT INTO apps(id) VALUES($1)", &[&app])
        .await
        .unwrap();
    client.execute("INSERT INTO execution_crons(app_id,name,cron,timezone,target,next_at) VALUES($1,'utc','* * * * *','UTC','{}',now())", &[&app]).await.unwrap();
    assert!(client.execute("INSERT INTO execution_crons(app_id,name,cron,timezone,target,next_at) VALUES($1,'before','* * * * *','Asia/Kolkata','{}',now())", &[&app]).await.is_err());

    client
        .batch_execute(include_str!("../migrations/136_cron_timezones.up.sql"))
        .await
        .unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT timezone FROM execution_crons WHERE app_id=$1 AND name='utc'",
                &[&app],
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "UTC"
    );
    client.execute("INSERT INTO execution_crons(app_id,name,cron,target,next_at) VALUES($1,'default','* * * * *','{}',now())", &[&app]).await.unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT timezone FROM execution_crons WHERE app_id=$1 AND name='default'",
                &[&app],
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "UTC"
    );
    client.execute("INSERT INTO execution_crons(app_id,name,cron,timezone,target,next_at) VALUES($1,'kolkata','* * * * *','Asia/Kolkata','{}',now())", &[&app]).await.unwrap();
    assert!(
        client
            .batch_execute(include_str!("../migrations/136_cron_timezones.down.sql"))
            .await
            .is_err()
    );
    assert_eq!(
        client
            .query_one(
                "SELECT timezone FROM execution_crons WHERE app_id=$1 AND name='kolkata'",
                &[&app],
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "Asia/Kolkata"
    );

    client
        .execute(
            "DELETE FROM execution_crons WHERE app_id=$1 AND name='kolkata'",
            &[&app],
        )
        .await
        .unwrap();
    client
        .batch_execute(include_str!("../migrations/136_cron_timezones.down.sql"))
        .await
        .unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT timezone FROM execution_crons WHERE app_id=$1 AND name='utc'",
                &[&app],
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "UTC"
    );
    assert!(client.execute("INSERT INTO execution_crons(app_id,name,cron,timezone,target,next_at) VALUES($1,'after','* * * * *','Asia/Kolkata','{}',now())", &[&app]).await.is_err());
    drop(client);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn dashboard_refresh_token_migration_hashes_legacy_plaintext_tokens() {
    let Some(database_url) = instant_test_support::test_migration_database_url(
        "dashboard_refresh_token_migration_hashes_legacy_plaintext_tokens",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let schema = format!("dashboard_token_hash_{}", uuid::Uuid::new_v4().simple());
    setup
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; CREATE EXTENSION IF NOT EXISTS pgcrypto;"
        ))
        .await
        .unwrap();
    let scoped_url = scoped_url(&database_url, &schema);
    let (client, connection) = tokio_postgres::connect(&scoped_url, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(
            "CREATE TABLE instant_users (
               id uuid PRIMARY KEY,
               email text NOT NULL UNIQUE,
               created_at timestamp DEFAULT NOW()
             );
             CREATE TABLE instant_user_refresh_tokens (
               id uuid primary key,
               user_id uuid NOT NULL REFERENCES instant_users(id) ON DELETE CASCADE,
               created_at TIMESTAMP NOT NULL DEFAULT NOW()
             );",
        )
        .await
        .unwrap();
    let user_id = uuid::Uuid::new_v4();
    let token = uuid::Uuid::new_v4();
    client
        .execute(
            "INSERT INTO instant_users (id, email) VALUES ($1, 'legacy@example.com')",
            &[&user_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, user_id) VALUES ($1, $2)",
            &[&token, &user_id],
        )
        .await
        .unwrap();

    client
        .batch_execute(include_str!(
            "../migrations/147_hash_dashboard_refresh_tokens.up.sql"
        ))
        .await
        .unwrap();

    let row = client
        .query_one(
            "SELECT token_hash, last_used_at IS NOT NULL
               FROM instant_user_refresh_tokens WHERE id = $1",
            &[&token],
        )
        .await
        .unwrap();
    assert_eq!(
        row.get::<_, Vec<u8>>(0),
        Sha256::digest(token.to_string().as_bytes()).to_vec(),
        "existing tokens are backfilled with the hash of their canonical text"
    );
    assert!(
        row.get::<_, bool>(1),
        "existing tokens start their 30-day sliding expiry"
    );
    client
        .execute(
            "INSERT INTO instant_user_refresh_tokens (id, token_hash, user_id)
             VALUES ($1, $2, $3)",
            &[
                &uuid::Uuid::new_v4(),
                &Sha256::digest(b"new-token").to_vec(),
                &user_id,
            ],
        )
        .await
        .unwrap();

    client
        .batch_execute(include_str!(
            "../migrations/147_hash_dashboard_refresh_tokens.down.sql"
        ))
        .await
        .unwrap();
    let columns: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.columns
              WHERE table_schema = $1
                AND table_name = 'instant_user_refresh_tokens'
                AND column_name IN ('token_hash', 'last_used_at')",
            &[&schema],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        columns, 0,
        "the down migration restores the plaintext schema"
    );
    drop(client);
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keeps the full migration lifecycle contract in one database.
async fn embedded_migrations_create_upgrade_verify_and_rollback_a_fresh_database() {
    const LATEST_MIGRATION: u64 = 152;
    let Some(database_url) = instant_test_support::test_migration_database_url(
        "embedded_migrations_create_upgrade_verify_and_rollback_a_fresh_database",
    ) else {
        return;
    };
    let (mut clients, drivers) = PostgresConnectionDrivers::connect_many(&database_url, 1)
        .await
        .expect("connect to empty migration database");
    let mut client = clients.pop().unwrap();

    let initial_verification = verify_database_migrations(&mut client).await;
    assert!(
        matches!(
            initial_verification,
            Err(MigrationError::Pending {
                current: 0,
                required: LATEST_MIGRATION
            })
        ),
        "fresh database migration state: {initial_verification:?}"
    );
    let empty_metadata = client
        .query_one(
            "SELECT
               to_regclass('schema_migrations')::text,
               to_regclass('instant_migration_checksums')::text",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(empty_metadata.get::<_, Option<String>>(0), None);
    assert_eq!(empty_metadata.get::<_, Option<String>>(1), None);

    let first = migrate_database(&mut client)
        .await
        .expect("migrate fresh database");
    assert_eq!(first.previous_version, 0);
    assert_eq!(first.current_version, LATEST_MIGRATION);
    let checksum_count_before_successful_verification = client
        .query_one("SELECT count(*) FROM instant_migration_checksums", &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(
        verify_database_migrations(&mut client).await.unwrap(),
        LATEST_MIGRATION
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM instant_migration_checksums", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        checksum_count_before_successful_verification,
        "successful migration verification backfilled checksum metadata",
    );
    assert_eq!(
        first.applied_versions,
        (1..=LATEST_MIGRATION).collect::<Vec<_>>()
    );
    let catalog_counts_before = client
        .query_one(
            "SELECT
                 (SELECT count(*) FROM attrs
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7'),
                 (SELECT count(*) FROM idents
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7')",
            &[],
        )
        .await
        .unwrap();
    let catalog_counts_before = (
        catalog_counts_before.get::<_, i64>(0),
        catalog_counts_before.get::<_, i64>(1),
    );
    assert!(matches!(
        verify_system_catalog(&mut client).await,
        Err(SystemCatalogVerificationError::Missing {
            ref missing_attribute_ids,
            ..
        }) if missing_attribute_ids.len() == 58
    ));
    let catalog_counts_after_failed_verification = client
        .query_one(
            "SELECT
                 (SELECT count(*) FROM attrs
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7'),
                 (SELECT count(*) FROM idents
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        (
            catalog_counts_after_failed_verification.get::<_, i64>(0),
            catalog_counts_after_failed_verification.get::<_, i64>(1),
        ),
        catalog_counts_before,
        "read-only startup verification mutated the missing catalog",
    );
    let catalog = ensure_system_catalog(&mut client)
        .await
        .expect("create complete system catalog");
    assert_eq!(catalog.created_attribute_ids.len(), 58);
    assert!(catalog.mismatched_attribute_ids.is_empty());
    assert!(catalog.extra_attribute_ids.is_empty());
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM attrs WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        58
    );
    assert_eq!(
        ensure_system_catalog(&mut client).await.unwrap(),
        instant_server::SystemCatalogMigrationOutcome::default()
    );
    let complete_catalog_counts = client
        .query_one(
            "SELECT
                 (SELECT count(*) FROM attrs
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7'),
                 (SELECT count(*) FROM idents
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7')",
            &[],
        )
        .await
        .unwrap();
    let complete_catalog_counts = (
        complete_catalog_counts.get::<_, i64>(0),
        complete_catalog_counts.get::<_, i64>(1),
    );
    verify_system_catalog(&mut client)
        .await
        .expect("complete system catalog verifies without reconciliation");
    let verified_catalog_counts = client
        .query_one(
            "SELECT
                 (SELECT count(*) FROM attrs
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7'),
                 (SELECT count(*) FROM idents
                   WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        (
            verified_catalog_counts.get::<_, i64>(0),
            verified_catalog_counts.get::<_, i64>(1),
        ),
        complete_catalog_counts,
        "successful startup verification mutated the complete catalog",
    );
    let corrupted_attribute = client
        .query_one(
            "SELECT id, is_indexed FROM attrs
             WHERE app_id = 'a1111111-1111-1111-1111-111111111ca7'
             ORDER BY id LIMIT 1",
            &[],
        )
        .await
        .unwrap();
    let corrupted_attribute_id = corrupted_attribute.get::<_, uuid::Uuid>(0);
    let expected_is_indexed = corrupted_attribute.get::<_, bool>(1);
    let corruption = client.transaction().await.unwrap();
    corruption
        .batch_execute("SET LOCAL instant.allow_system_catalog_app_attr_update = 'true'")
        .await
        .unwrap();
    corruption
        .execute(
            "UPDATE attrs SET is_indexed = $2 WHERE id = $1",
            &[&corrupted_attribute_id, &(!expected_is_indexed)],
        )
        .await
        .unwrap();
    corruption.commit().await.unwrap();
    assert!(matches!(
        verify_system_catalog(&mut client).await,
        Err(SystemCatalogVerificationError::Incompatible {
            ref mismatched_attribute_ids,
            ..
        }) if mismatched_attribute_ids == &[corrupted_attribute_id]
    ));
    assert!(matches!(
        ensure_system_catalog(&mut client).await,
        Err(SystemCatalogMigrationError::IncompatibleAttributes {
            ref mismatched_attribute_ids,
        }) if mismatched_attribute_ids == &[corrupted_attribute_id]
    ));
    assert_eq!(
        client
            .query_one(
                "SELECT is_indexed FROM attrs WHERE id = $1",
                &[&corrupted_attribute_id]
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        !expected_is_indexed,
        "explicit catalog migration falsely repaired an incompatible row",
    );
    let repair = client.transaction().await.unwrap();
    repair
        .batch_execute("SET LOCAL instant.allow_system_catalog_app_attr_update = 'true'")
        .await
        .unwrap();
    repair
        .execute(
            "UPDATE attrs SET is_indexed = $2 WHERE id = $1",
            &[&corrupted_attribute_id, &expected_is_indexed],
        )
        .await
        .unwrap();
    repair.commit().await.unwrap();
    verify_system_catalog(&mut client)
        .await
        .expect("manually repaired catalog verifies");
    client
        .execute(
            "DELETE FROM instant_migration_checksums WHERE version = $1",
            &[&i64::try_from(LATEST_MIGRATION).unwrap()],
        )
        .await
        .unwrap();
    let checksum_count_before_failed_verification = client
        .query_one("SELECT count(*) FROM instant_migration_checksums", &[])
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert!(matches!(
        verify_database_migrations(&mut client).await,
        Err(MigrationError::MissingChecksum {
            version: LATEST_MIGRATION,
            ..
        })
    ));
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM instant_migration_checksums", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        checksum_count_before_failed_verification,
        "failed migration verification backfilled missing checksum metadata",
    );

    let second = migrate_database(&mut client)
        .await
        .expect("migration is idempotent");
    assert!(second.applied_versions.is_empty());
    let state = client
        .query_one("SELECT version, dirty FROM schema_migrations", &[])
        .await
        .unwrap();
    assert_eq!(
        state.get::<_, i64>(0),
        i64::try_from(LATEST_MIGRATION).unwrap()
    );
    assert!(!state.get::<_, bool>(1));
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM instant_migration_checksums", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        i64::try_from(LATEST_MIGRATION).unwrap()
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_realtime_room_members')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("rust_realtime_room_members")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_realtime_sse_sessions')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("rust_realtime_sse_sessions")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_realtime_machines')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("rust_realtime_machines")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_realtime_stream_buffers')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("rust_realtime_stream_buffers")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_database_failovers')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("rust_database_failovers")
    );
    let byop_app_id = uuid::Uuid::new_v4();
    client
        .execute(
            "INSERT INTO rust_realtime_room_events
               (source_machine_id, kind, app_id, room_id, payload)
             VALUES ($1, 'byop-wal', $2, '42', '{}'::jsonb)
             ON CONFLICT (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal')
             DO NOTHING",
            &[&uuid::Uuid::new_v4(), &byop_app_id],
        )
        .await
        .expect("latest schema accepts a deduplicated BYOP WAL event");
    client
        .execute(
            "INSERT INTO rust_realtime_room_events
               (source_machine_id, kind, app_id, room_id, payload)
             VALUES ($1, 'byop-wal', $2, '42', '{}'::jsonb)
             ON CONFLICT (kind, app_id, room_id) WHERE kind IN ('wal', 'byop-wal')
             DO NOTHING",
            &[&uuid::Uuid::new_v4(), &byop_app_id],
        )
        .await
        .expect("duplicate BYOP publication is accepted as a no-op");
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM rust_realtime_room_events
                 WHERE kind = 'byop-wal' AND app_id = $1 AND room_id = '42'",
                &[&byop_app_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    let machine_id = uuid::Uuid::new_v4();
    client
        .execute(
            "INSERT INTO rust_realtime_machines (machine_id) VALUES ($1)",
            &[&machine_id],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO rust_realtime_event_consumers
               (machine_id, consumer_kind, event_cursor)
             VALUES ($1, 'runtime', 42)",
            &[&machine_id],
        )
        .await
        .expect("latest schema persists a non-negative per-consumer event cursor");
    assert_eq!(
        client
            .query_one(
                "SELECT event_cursor FROM rust_realtime_event_consumers
                 WHERE machine_id = $1 AND consumer_kind = 'runtime'",
                &[&machine_id],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        42
    );
    assert_eq!(
        client
            .query_one("SELECT to_regclass('public.execution_outbox')::text", &[],)
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("execution_outbox")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_task_deployments')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("execution_task_deployments")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_app_limits')::text",
                &[]
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("execution_app_limits")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_rate_limit_counters')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("execution_rate_limit_counters")
    );
    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_audit_events')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("execution_audit_events")
    );
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.app_backup_schedules') IS NOT NULL",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.indexing_jobs_available') IS NOT NULL",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert!(
        client
            .query_one("SELECT to_regclass('public.triples_app_id') IS NULL", &[])
            .await
            .unwrap()
            .get::<_, bool>(0),
        "the duplicate app_id index duplicates the primary key's leading column"
    );
    assert!(
        client
            .query_one(
                "SELECT reloptions @> ARRAY[
                   'autovacuum_vacuum_scale_factor=0.02',
                   'autovacuum_analyze_scale_factor=0.01'
                 ] FROM pg_class WHERE oid = 'public.triples'::regclass",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        "triples carries the tighter autovacuum thresholds"
    );
    assert_eq!(
        client
            .query_one(
                "SELECT stxstattarget::integer FROM pg_statistic_ext
                 WHERE stxname='triples_attr_value_mcv' AND stxrelid='public.triples'::regclass",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<i32>>(0),
        Some(100)
    );
    assert_eq!(
        client
            .query_one(
                "SELECT email FROM instant_users
                 WHERE id='dda67f26-6962-479e-b476-60e4b6963b74'::uuid",
                &[],
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "ephemeral-apps-dev@backendrelay.invalid"
    );
    let config_app = client
        .query_one(
            "SELECT creator_id, org_id, title FROM apps
             WHERE id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        config_app.get::<_, uuid::Uuid>(0),
        uuid::Uuid::parse_str("e1111111-1111-1111-1111-111111111ca7").unwrap()
    );
    assert_eq!(config_app.get::<_, Option<uuid::Uuid>>(1), None);
    assert_eq!(config_app.get::<_, String>(2), "Deployment config");
    assert_eq!(
        client
            .query_one(
                "SELECT
                   (SELECT count(*) FROM app_admin_tokens
                     WHERE app_id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid)
                   + (SELECT count(*) FROM app_members
                     WHERE app_id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid)
                   + (SELECT count(*) FROM app_member_invites
                     WHERE app_id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid)",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT code FROM rules
                 WHERE app_id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid",
                &[],
            )
            .await
            .unwrap()
            .get::<_, tokio_postgres::types::Json<serde_json::Value>>(0)
            .0,
        serde_json::json!({"$default": {"allow": {"$default": "false"}}})
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback app restore job source");
    assert_eq!(rollback.previous_version, 152);
    assert_eq!(rollback.current_version, 151);
    assert_eq!(rollback.applied_versions, vec![152]);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM information_schema.columns
                  WHERE table_schema = 'public'
                    AND table_name = 'app_restore_jobs'
                    AND column_name = 'source_app_id'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "rolling back the restore source migration drops its column"
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback triples autovacuum thresholds");
    assert_eq!(rollback.previous_version, 151);
    assert_eq!(rollback.current_version, 150);
    assert_eq!(rollback.applied_versions, vec![151]);
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback duplicate triples app_id index removal");
    assert_eq!(rollback.previous_version, 150);
    assert_eq!(rollback.current_version, 149);
    assert_eq!(rollback.applied_versions, vec![150]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.triples_app_id') IS NOT NULL",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        "rolling back recreates the duplicate index"
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback additive compatibility bookkeeping");
    assert_eq!(rollback.previous_version, 149);
    assert_eq!(rollback.current_version, 148);
    assert_eq!(rollback.applied_versions, vec![149]);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM information_schema.columns
                  WHERE table_schema = 'public'
                    AND table_name = 'instant_migration_checksums'
                    AND column_name = 'additive'",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "rolling back the additive bookkeeping migration drops its column"
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback per-app sender domains");
    assert_eq!(rollback.previous_version, 148);
    assert_eq!(rollback.current_version, 147);
    assert_eq!(rollback.applied_versions, vec![148]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.app_sender_domains') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        "rolling back the sender domain migration drops its table"
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback dashboard refresh token hashing");
    assert_eq!(rollback.previous_version, 147);
    assert_eq!(rollback.current_version, 146);
    assert_eq!(rollback.applied_versions, vec![147]);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM information_schema.columns
                  WHERE table_schema = 'public'
                    AND table_name = 'instant_user_refresh_tokens'
                    AND column_name IN ('token_hash', 'last_used_at')",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "rolling back the refresh-token hash migration restores the plaintext schema"
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback config app lock");
    assert_eq!(rollback.previous_version, 146);
    assert_eq!(rollback.current_version, 145);
    assert_eq!(rollback.applied_versions, vec![146]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.apps') IS NOT NULL
                        AND EXISTS (SELECT 1 FROM apps
                          WHERE id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid
                            AND creator_id='e1111111-1111-1111-1111-111111111ca7'::uuid
                            AND org_id IS NULL
                            AND title='Deployment config')",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0),
        "rolling back the config app lock must keep the locked app"
    );
    assert_eq!(
        client
            .query_one(
                "SELECT code FROM rules
                 WHERE app_id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid",
                &[],
            )
            .await
            .unwrap()
            .get::<_, tokio_postgres::types::Json<serde_json::Value>>(0)
            .0,
        serde_json::json!({"$default": {"allow": {"$default": "false"}}}),
        "rolling back the config app lock must keep its deny-all rules"
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback neutral system user emails");
    assert_eq!(rollback.previous_version, 145);
    assert_eq!(rollback.current_version, 144);
    assert_eq!(rollback.applied_versions, vec![145]);
    assert_eq!(
        client
            .query_one(
                "SELECT email FROM instant_users
                 WHERE id='dda67f26-6962-479e-b476-60e4b6963b74'::uuid",
                &[],
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "hello+ephemeralappsdev@instantdb.com"
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback bounded joint statistics target");
    assert_eq!(rollback.previous_version, 144);
    assert_eq!(rollback.current_version, 143);
    assert_eq!(rollback.applied_versions, vec![144]);
    assert_eq!(
        client
            .query_one(
                "SELECT stxstattarget::integer FROM pg_statistic_ext
                 WHERE stxname='triples_attr_value_mcv' AND stxrelid='public.triples'::regclass",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<i32>>(0),
        None
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback available jobs index");
    assert_eq!(rollback.previous_version, 143);
    assert_eq!(rollback.current_version, 142);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.indexing_jobs_available') IS NULL",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback backup schedule migration");
    assert_eq!(rollback.previous_version, 142);
    assert_eq!(rollback.current_version, 141);
    assert_eq!(rollback.applied_versions, vec![142]);
    assert!(client.query_one("SELECT to_regclass('public.app_backup_schedules') IS NULL AND to_regclass('public.application_logs') IS NOT NULL", &[]).await.unwrap().get::<_,bool>(0));
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback application logs migration");
    assert_eq!(rollback.previous_version, 141);
    assert_eq!(rollback.current_version, 140);
    assert_eq!(rollback.applied_versions, vec![141]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.application_logs') IS NULL
                 AND to_regclass('public.execution_audit_events') IS NOT NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback execution audit migration");
    assert_eq!(rollback.previous_version, 140);
    assert_eq!(rollback.current_version, 139);
    assert_eq!(rollback.applied_versions, vec![140]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_audit_events') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback execution policy migration");
    assert_eq!(rollback.previous_version, 139);
    assert_eq!(rollback.current_version, 138);
    assert_eq!(rollback.applied_versions, vec![139]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_app_limits') IS NULL
                 AND to_regclass('public.execution_rate_limit_counters') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback hosted task migration");
    assert_eq!(rollback.previous_version, 138);
    assert_eq!(rollback.current_version, 137);
    assert_eq!(rollback.applied_versions, vec![138]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_task_deployments') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback manual cron run migration");
    assert_eq!(rollback.previous_version, 137);
    assert_eq!(rollback.current_version, 136);
    assert_eq!(rollback.applied_versions, vec![137]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_cron_manual_runs') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback cron timezone migration");
    assert_eq!(rollback.previous_version, 136);
    assert_eq!(rollback.current_version, 135);
    assert_eq!(rollback.applied_versions, vec![136]);
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback execution app cleanup migration");
    assert_eq!(rollback.previous_version, 135);
    assert_eq!(rollback.current_version, 134);
    assert_eq!(rollback.applied_versions, vec![135]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_app_cleanup_tombstones') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback execution secrets migration");
    assert_eq!(rollback.previous_version, 134);
    assert_eq!(rollback.current_version, 133);
    assert_eq!(rollback.applied_versions, vec![134]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_secrets') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback hosted deployments migration");
    assert_eq!(rollback.previous_version, 133);
    assert_eq!(rollback.current_version, 132);
    assert_eq!(rollback.applied_versions, vec![133]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.execution_deployments') IS NULL",
                &[]
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback execution outbox migration");
    assert_eq!(rollback.previous_version, 132);
    assert_eq!(rollback.current_version, 131);
    assert_eq!(rollback.applied_versions, vec![132]);
    assert!(
        client
            .query_one("SELECT to_regclass('public.execution_outbox') IS NULL", &[],)
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback realtime event-cursor migration");
    assert_eq!(rollback.previous_version, 131);
    assert_eq!(rollback.current_version, 130);
    assert_eq!(rollback.applied_versions, vec![131]);
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback BYOP realtime-event migration");
    assert_eq!(rollback.previous_version, 130);
    assert_eq!(rollback.current_version, 129);
    assert_eq!(rollback.applied_versions, vec![130]);
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback native database failover migration");
    assert_eq!(rollback.previous_version, 129);
    assert_eq!(rollback.current_version, 128);
    assert_eq!(rollback.applied_versions, vec![129]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_database_failovers') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback native realtime migration");
    assert_eq!(rollback.previous_version, 128);
    assert_eq!(rollback.current_version, 127);
    assert_eq!(rollback.applied_versions, vec![128]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_realtime_room_members') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );

    assert_eq!(
        client
            .query_one("SELECT to_regclass('public.custodian')::text", &[])
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("custodian")
    );

    assert_eq!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_runtime_session_reports')::text",
                &[],
            )
            .await
            .unwrap()
            .get::<_, Option<String>>(0)
            .as_deref(),
        Some("rust_runtime_session_reports")
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback native coordination migration");
    assert_eq!(rollback.previous_version, 127);
    assert_eq!(rollback.current_version, 126);
    assert_eq!(rollback.applied_versions, vec![127]);
    assert!(
        client
            .query_one(
                "SELECT to_regclass('public.rust_runtime_session_reports') IS NULL",
                &[],
            )
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback BackendRelay marketing compatibility marker");
    assert_eq!(rollback.previous_version, 126);
    assert_eq!(rollback.current_version, 125);
    assert_eq!(rollback.applied_versions, vec![126]);
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback email sender provider identity migration");
    assert_eq!(rollback.previous_version, 125);
    assert_eq!(rollback.current_version, 124);
    assert_eq!(rollback.applied_versions, vec![125]);
    let rollback = rollback_database(&mut client, 1)
        .await
        .expect("rollback latest JVM migration");
    assert_eq!(rollback.previous_version, 124);
    assert_eq!(rollback.current_version, 123);
    assert_eq!(rollback.applied_versions, vec![124]);
    assert!(
        client
            .query_one("SELECT to_regclass('public.custodian') IS NULL", &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert_eq!(
        migrate_database(&mut client)
            .await
            .expect("reapply rolled-back migration")
            .applied_versions,
        (124..=LATEST_MIGRATION).collect::<Vec<_>>()
    );
    assert_eq!(
        client
            .query_one(
                "SELECT code FROM rules
                 WHERE app_id='24a4d71b-7bb2-4630-9aee-01146af26239'::uuid",
                &[],
            )
            .await
            .unwrap()
            .get::<_, tokio_postgres::types::Json<serde_json::Value>>(0)
            .0,
        serde_json::json!({"$default": {"allow": {"$default": "false"}}}),
        "re-applying the config app lock over the retained app must be idempotent"
    );

    client
        .execute("UPDATE schema_migrations SET dirty = true", &[])
        .await
        .unwrap();
    assert!(matches!(
        migrate_database(&mut client).await,
        Err(MigrationError::Dirty(version)) if version == i64::try_from(LATEST_MIGRATION).unwrap()
    ));
    let forced = force_migration_version(&mut client, LATEST_MIGRATION)
        .await
        .expect("explicit force clears inspected dirty state");
    assert_eq!(forced.previous_version, LATEST_MIGRATION);
    assert_eq!(forced.current_version, LATEST_MIGRATION);
    assert!(
        migrate_database(&mut client)
            .await
            .unwrap()
            .applied_versions
            .is_empty()
    );

    client
        .execute(
            "UPDATE instant_migration_checksums SET checksum = '\\x00' WHERE version = $1",
            &[&i64::try_from(LATEST_MIGRATION).unwrap()],
        )
        .await
        .unwrap();
    assert!(matches!(
        migrate_database(&mut client).await,
        Err(MigrationError::ChecksumMismatch {
            version: LATEST_MIGRATION,
            ..
        })
    ));
    client
        .execute(
            "DELETE FROM instant_migration_checksums WHERE version = $1",
            &[&i64::try_from(LATEST_MIGRATION).unwrap()],
        )
        .await
        .unwrap();
    assert!(
        migrate_database(&mut client)
            .await
            .expect("restore checksum after corruption fixture")
            .applied_versions
            .is_empty()
    );
    assert_eq!(
        verify_database_migrations(&mut client).await.unwrap(),
        LATEST_MIGRATION
    );

    // N-1 binary compatibility: a newer schema verifies only while every
    // migration past this binary's latest is recorded as additive.
    let synthetic_version = LATEST_MIGRATION + 1;
    let synthetic_version_i64 = i64::try_from(synthetic_version).unwrap();
    client
        .execute(
            "INSERT INTO instant_migration_checksums (version, name, checksum, additive)
             VALUES ($1, 'synthetic_additive', '\\x00', true)",
            &[&synthetic_version_i64],
        )
        .await
        .unwrap();
    client
        .execute(
            "UPDATE schema_migrations SET version = $1",
            &[&synthetic_version_i64],
        )
        .await
        .unwrap();
    assert_eq!(
        verify_database_migrations(&mut client).await.unwrap(),
        synthetic_version
    );
    let newer_migration = migrate_database(&mut client)
        .await
        .expect("migrate up is a no-op on a newer additive schema");
    assert!(newer_migration.applied_versions.is_empty());
    assert_eq!(newer_migration.current_version, synthetic_version);
    assert!(matches!(
        rollback_database(&mut client, 1).await,
        Err(MigrationError::NewerDatabase { database, binary })
            if database == synthetic_version_i64 && binary == LATEST_MIGRATION
    ));
    client
        .execute(
            "UPDATE instant_migration_checksums SET additive = false WHERE version = $1",
            &[&synthetic_version_i64],
        )
        .await
        .unwrap();
    assert!(matches!(
        verify_database_migrations(&mut client).await,
        Err(MigrationError::NewerDatabase { database, binary })
            if database == synthetic_version_i64 && binary == LATEST_MIGRATION
    ));
    client
        .execute(
            "DELETE FROM instant_migration_checksums WHERE version = $1",
            &[&synthetic_version_i64],
        )
        .await
        .unwrap();
    assert!(matches!(
        verify_database_migrations(&mut client).await,
        Err(MigrationError::NewerDatabase { .. })
    ));
    client
        .execute(
            "UPDATE schema_migrations SET version = $1",
            &[&i64::try_from(LATEST_MIGRATION).unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(
        verify_database_migrations(&mut client).await.unwrap(),
        LATEST_MIGRATION
    );

    // Unknown additive system catalog attributes are compatible with an older
    // binary; missing or changed attributes still fail verification.
    let extra_attribute_id = uuid::Uuid::new_v4();
    let extra_identity_id = uuid::Uuid::new_v4();
    let system_catalog_app_id =
        uuid::Uuid::parse_str("a1111111-1111-1111-1111-111111111ca7").unwrap();
    client
        .execute(
            "INSERT INTO attrs
               (id, app_id, value_type, cardinality, is_unique, is_indexed,
                is_required, forward_ident, etype, label, setting_unique, indexing,
                checking_data_type, inferred_types)
             VALUES ($1, $2, 'blob', 'one', false, false, false, $3,
                     '$users', 'futureAdditive', false, false, false,
                     2::integer::bit(32))",
            &[
                &extra_attribute_id,
                &system_catalog_app_id,
                &extra_identity_id,
            ],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO idents (id, app_id, attr_id, etype, label)
             VALUES ($1, $2, $3, '$users', 'futureAdditive')",
            &[
                &extra_identity_id,
                &system_catalog_app_id,
                &extra_attribute_id,
            ],
        )
        .await
        .unwrap();
    verify_system_catalog(&mut client)
        .await
        .expect("unknown additive system catalog attributes are compatible");
    let cleanup = client.transaction().await.unwrap();
    cleanup
        .batch_execute(
            "SET LOCAL instant.allow_system_catalog_app_attr_delete = 'true';
             SET LOCAL instant.allow_system_catalog_app_ident_delete = 'true';",
        )
        .await
        .unwrap();
    cleanup
        .execute("DELETE FROM attrs WHERE id = $1", &[&extra_attribute_id])
        .await
        .unwrap();
    cleanup.commit().await.unwrap();
    verify_system_catalog(&mut client)
        .await
        .expect("catalog verification remains complete after cleanup");

    drop(client);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    drivers.supervise(cancellation).await.unwrap();
}

#[tokio::test]
async fn bounded_mcv_statistics_migration_preserves_rows_column_targets_and_rollback() {
    let Some(database_url) = instant_test_support::test_migration_database_url(
        "bounded_mcv_statistics_migration_preserves_rows_column_targets_and_rollback",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_driver = tokio::spawn(connection);
    let schema = format!("bounded_mcv_migration_{}", uuid::Uuid::new_v4().simple());
    setup
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let (client, connection) = tokio_postgres::connect(&scoped_url(&database_url, &schema), NoTls)
        .await
        .unwrap();
    let driver = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TYPE checked_data_type AS ENUM ('number','string','boolean','date');
             CREATE TABLE triples (
               entity_id uuid PRIMARY KEY,
               attr_id uuid NOT NULL,
               value jsonb NOT NULL,
               ave boolean NOT NULL,
               checked_data_type checked_data_type
             );
             INSERT INTO triples
               SELECT md5(n::text)::uuid, md5((n%8)::text)::uuid, to_jsonb(n%1000),
                      true, 'number'::checked_data_type
                 FROM generate_series(1,40000) n;",
        )
        .await
        .unwrap();
    client
        .batch_execute(include_str!("../migrations/60_statistics.up.sql"))
        .await
        .unwrap();
    client.batch_execute("ANALYZE triples").await.unwrap();

    let data_digest = "SELECT count(*),md5(string_agg(row_to_json(t)::text,E'\\n' ORDER BY entity_id)) FROM triples t";
    let before = client.query_one(data_digest, &[]).await.unwrap();
    assert_eq!(before.get::<_, i64>(0), 40000);
    let stats_query = "SELECT s.stxstattarget::integer,pg_column_size(d.stxdmcv),
                      (SELECT count(*) FROM pg_mcv_list_items(d.stxdmcv))
                        FROM pg_statistic_ext s
                        JOIN pg_statistic_ext_data d ON d.stxoid=s.oid
                       WHERE s.stxname='triples_attr_value_mcv'
                         AND s.stxrelid='triples'::regclass";
    let original = client.query_one(stats_query, &[]).await.unwrap();
    assert_eq!(original.get::<_, Option<i32>>(0), None);
    assert!(original.get::<_, i64>(2) > 100);

    client
        .batch_execute(include_str!(
            "../migrations/144_bounded_mcv_statistics.up.sql"
        ))
        .await
        .unwrap();
    let bounded = client.query_one(stats_query, &[]).await.unwrap();
    assert_eq!(bounded.get::<_, Option<i32>>(0), Some(100));
    assert!((1..=100).contains(&bounded.get::<_, i64>(2)));
    assert!(bounded.get::<_, i32>(1) < original.get::<_, i32>(1));

    for phase in ["bounded", "restored"] {
        if phase == "restored" {
            client
                .batch_execute(include_str!(
                    "../migrations/144_bounded_mcv_statistics.down.sql"
                ))
                .await
                .unwrap();
            let restored = client.query_one(stats_query, &[]).await.unwrap();
            assert_eq!(restored.get::<_, Option<i32>>(0), None);
            assert_eq!(restored.get::<_, i64>(2), original.get::<_, i64>(2));
        }
        let after = client.query_one(data_digest, &[]).await.unwrap();
        assert_eq!(after.get::<_, i64>(0), before.get::<_, i64>(0), "{phase}");
        assert_eq!(
            after.get::<_, String>(1),
            before.get::<_, String>(1),
            "{phase}: migration changed triple bytes"
        );
        let targets = client
            .query(
                "SELECT attname,attstattarget::integer FROM pg_attribute
                  WHERE attrelid='triples'::regclass AND attname IN ('attr_id','value')
                  ORDER BY attname",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].get::<_, String>(0), "attr_id");
        assert_eq!(targets[1].get::<_, String>(0), "value");
        for target in targets {
            assert_eq!(target.get::<_, Option<i32>>(1), Some(2500), "{phase}");
        }
    }
    drop(client);
    driver.await.unwrap().unwrap();
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(setup);
    setup_driver.await.unwrap().unwrap();
}

#[tokio::test]
async fn neutral_system_user_emails_migration_renames_seeded_users_and_rolls_back() {
    let Some(database_url) = instant_test_support::test_migration_database_url(
        "neutral_system_user_emails_migration_renames_seeded_users_and_rolls_back",
    ) else {
        return;
    };
    let (setup, connection) = tokio_postgres::connect(&database_url, NoTls).await.unwrap();
    let setup_driver = tokio::spawn(connection);
    let schema = format!(
        "neutral_system_user_emails_{}",
        uuid::Uuid::new_v4().simple()
    );
    setup
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let (client, connection) = tokio_postgres::connect(&scoped_url(&database_url, &schema), NoTls)
        .await
        .unwrap();
    let driver = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TABLE instant_users (id uuid PRIMARY KEY, email text NOT NULL UNIQUE);
             INSERT INTO instant_users (id, email) VALUES
               ('dda67f26-6962-479e-b476-60e4b6963b74','hello+ephemeralappsdev@instantdb.com'),
               ('ee25bd4e-b968-4c1c-bde4-df046ef0dade','hello+ephemeralapps@instantdb.com'),
               ('9bacaf3d-0c7d-46e6-bb1b-4b9fdb9c5b61','hello+getadbapps@instantdb.com');",
        )
        .await
        .unwrap();
    let users = [
        (
            "dda67f26-6962-479e-b476-60e4b6963b74",
            "hello+ephemeralappsdev@instantdb.com",
            "ephemeral-apps-dev@backendrelay.invalid",
        ),
        (
            "ee25bd4e-b968-4c1c-bde4-df046ef0dade",
            "hello+ephemeralapps@instantdb.com",
            "ephemeral-apps@backendrelay.invalid",
        ),
        (
            "9bacaf3d-0c7d-46e6-bb1b-4b9fdb9c5b61",
            "hello+getadbapps@instantdb.com",
            "get-a-db@backendrelay.invalid",
        ),
    ];

    client
        .batch_execute(include_str!(
            "../migrations/145_neutral_system_user_emails.up.sql"
        ))
        .await
        .unwrap();
    for (id, _, renamed) in users {
        assert_eq!(
            client
                .query_one(
                    "SELECT email FROM instant_users WHERE id::text = $1",
                    &[&id]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            renamed
        );
    }

    client
        .batch_execute(include_str!(
            "../migrations/145_neutral_system_user_emails.down.sql"
        ))
        .await
        .unwrap();
    for (id, seeded, _) in users {
        assert_eq!(
            client
                .query_one(
                    "SELECT email FROM instant_users WHERE id::text = $1",
                    &[&id]
                )
                .await
                .unwrap()
                .get::<_, String>(0),
            seeded
        );
    }

    drop(client);
    driver.await.unwrap().unwrap();
    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(setup);
    setup_driver.await.unwrap().unwrap();
}
