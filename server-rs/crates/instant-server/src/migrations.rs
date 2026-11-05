use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_postgres::Client;

const MIGRATION_LOCK_ID: i64 = 0x496e_7374_616e_744d;

pub(crate) struct EmbeddedMigration {
    version: u64,
    name: &'static str,
    additive: bool,
    up: &'static str,
    down: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationOutcome {
    pub previous_version: u64,
    pub current_version: u64,
    pub applied_versions: Vec<u64>,
}

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("database migration metadata contains multiple schema_migrations rows")]
    CorruptState,
    #[error("database migration {0} is marked dirty; repair or force it before continuing")]
    Dirty(i64),
    #[error(
        "database schema version {database} is newer than this binary's version {binary} and the migrations in between are not all additive; deploy the binary that produced the schema or restore the previous schema before continuing"
    )]
    NewerDatabase { database: i64, binary: u64 },
    #[error(
        "database schema version {current} is behind this binary's required version {required}; run the explicit `instant-server migrate up` command before starting the server"
    )]
    Pending { current: u64, required: u64 },
    #[error(
        "database migration metadata is incomplete ({detail}); run the explicit `instant-server migrate up` command before starting the server"
    )]
    IncompleteMetadata { detail: &'static str },
    #[error("database schema version cannot be negative: {0}")]
    NegativeVersion(i64),
    #[error("embedded migration {version} ({name}) does not match its recorded checksum")]
    ChecksumMismatch { version: u64, name: String },
    #[error(
        "database migration {version} ({name}) has no recorded checksum; run the explicit `instant-server migrate up` command before starting the server"
    )]
    MissingChecksum { version: u64, name: &'static str },
    #[error("migration {version} ({name}) failed: {source}")]
    Apply {
        version: u64,
        name: &'static str,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error(transparent)]
    Database(#[from] tokio_postgres::Error),
}

/// Applies every embedded SQL migration newer than the database's current
/// golang-migrate-compatible `schema_migrations` version.
///
/// # Errors
///
/// Returns for dirty/newer/corrupt metadata, checksum drift, lock failures, or
/// the first SQL migration that PostgreSQL rejects. A failed migration remains
/// dirty, matching the existing operational repair contract.
pub async fn migrate_database(client: &mut Client) -> Result<MigrationOutcome, MigrationError> {
    with_migration_lock(client, |client| Box::pin(migrate_locked(client))).await
}

/// Explicitly rolls back at most `steps` embedded migrations.
///
/// # Errors
///
/// Returns the same metadata, checksum, locking, and SQL errors as
/// [`migrate_database`]. This operation is never invoked by server startup.
pub async fn rollback_database(
    client: &mut Client,
    steps: usize,
) -> Result<MigrationOutcome, MigrationError> {
    with_migration_lock(client, |client| Box::pin(rollback_locked(client, steps))).await
}

/// Explicitly replaces dirty/version metadata after an operator has inspected
/// and repaired a failed migration. Version zero clears the compatibility row.
///
/// # Errors
///
/// Returns for a version newer than this binary, corrupt metadata, lock or
/// database failures, or checksum drift in the adopted migration range.
pub async fn force_migration_version(
    client: &mut Client,
    version: u64,
) -> Result<MigrationOutcome, MigrationError> {
    with_migration_lock(client, |client| {
        Box::pin(force_version_locked(client, version))
    })
    .await
}

/// Verifies that the database is clean, current, and checksum-compatible
/// without applying application-schema migrations.
///
/// # Errors
///
/// Returns for dirty, older/newer/corrupt metadata, checksum drift, lock
/// failures, or database errors.
pub async fn verify_database_migrations(client: &mut Client) -> Result<u64, MigrationError> {
    with_migration_lock(client, |client| Box::pin(verify_locked(client))).await
}

async fn with_migration_lock<'a, T>(
    client: &'a mut Client,
    operation: impl for<'b> FnOnce(
        &'b mut Client,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<T, MigrationError>> + Send + 'b>,
    >,
) -> Result<T, MigrationError> {
    client
        .query_one("SELECT pg_advisory_lock($1)", &[&MIGRATION_LOCK_ID])
        .await?;
    let result = operation(client).await;
    let unlock = client
        .query_one("SELECT pg_advisory_unlock($1)", &[&MIGRATION_LOCK_ID])
        .await;
    match (result, unlock) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(MigrationError::Database(error)),
        (Ok(value), Ok(_)) => Ok(value),
    }
}

async fn migrate_locked(client: &mut Client) -> Result<MigrationOutcome, MigrationError> {
    initialize_metadata(client).await?;
    let previous_version = compatible_current_version(client).await?;
    reconcile_checksums(client, previous_version).await?;
    let mut applied_versions = Vec::new();
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version > previous_version)
    {
        write_state(client, Some((migration.version, true))).await?;
        if let Err(source) = client.batch_execute(migration.up).await {
            return Err(MigrationError::Apply {
                version: migration.version,
                name: migration.name,
                source,
            });
        }
        record_checksum_and_clean(client, migration).await?;
        applied_versions.push(migration.version);
    }
    Ok(MigrationOutcome {
        previous_version,
        current_version: latest_embedded_version().max(previous_version),
        applied_versions,
    })
}

async fn verify_locked(client: &mut Client) -> Result<u64, MigrationError> {
    let required = latest_embedded_version();
    let metadata = client
        .query_one(
            "SELECT
               to_regclass('schema_migrations') IS NOT NULL,
               to_regclass('instant_migration_checksums') IS NOT NULL",
            &[],
        )
        .await?;
    let has_schema_migrations = metadata.get::<_, bool>(0);
    let has_checksums = metadata.get::<_, bool>(1);
    if !has_schema_migrations && !has_checksums {
        return Err(MigrationError::Pending {
            current: 0,
            required,
        });
    }
    if !has_schema_migrations {
        return Err(MigrationError::IncompleteMetadata {
            detail: "schema_migrations is missing",
        });
    }
    if !has_checksums {
        return Err(MigrationError::IncompleteMetadata {
            detail: "instant_migration_checksums is missing",
        });
    }
    let current = compatible_current_version(client).await?;
    if current < required {
        return Err(MigrationError::Pending { current, required });
    }
    verify_checksums(client, current).await?;
    Ok(current)
}

async fn rollback_locked(
    client: &mut Client,
    steps: usize,
) -> Result<MigrationOutcome, MigrationError> {
    initialize_metadata(client).await?;
    let previous_version = current_version(client).await?;
    reconcile_checksums(client, previous_version).await?;
    let mut current = previous_version;
    let mut applied_versions = Vec::new();
    for migration in MIGRATIONS
        .iter()
        .rev()
        .filter(|migration| migration.version <= previous_version)
        .take(steps)
    {
        write_state(client, Some((migration.version, true))).await?;
        if let Err(source) = client.batch_execute(migration.down).await {
            return Err(MigrationError::Apply {
                version: migration.version,
                name: migration.name,
                source,
            });
        }
        current = migration.version - 1;
        let transaction = client.transaction().await?;
        transaction
            .execute(
                "DELETE FROM instant_migration_checksums WHERE version = $1",
                &[&i64::try_from(migration.version).unwrap()],
            )
            .await?;
        replace_state(&transaction, (current != 0).then_some((current, false))).await?;
        transaction.commit().await?;
        applied_versions.push(migration.version);
    }
    Ok(MigrationOutcome {
        previous_version,
        current_version: current,
        applied_versions,
    })
}

async fn force_version_locked(
    client: &mut Client,
    version: u64,
) -> Result<MigrationOutcome, MigrationError> {
    initialize_metadata(client).await?;
    let latest = latest_embedded_version();
    if version > latest {
        return Err(newer_database_error(version, latest));
    }
    let previous_version = raw_version(client).await?;
    write_state(client, (version != 0).then_some((version, false))).await?;
    reconcile_checksums(client, version).await?;
    client
        .execute(
            "DELETE FROM instant_migration_checksums WHERE version > $1",
            &[&i64::try_from(version).unwrap()],
        )
        .await?;
    Ok(MigrationOutcome {
        previous_version,
        current_version: version,
        applied_versions: Vec::new(),
    })
}

async fn initialize_metadata(client: &Client) -> Result<(), tokio_postgres::Error> {
    client
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
               version bigint PRIMARY KEY NOT NULL,
               dirty boolean NOT NULL
             );
             CREATE TABLE IF NOT EXISTS instant_migration_checksums (
               version bigint PRIMARY KEY NOT NULL,
               name text NOT NULL,
               checksum bytea NOT NULL,
               additive boolean NOT NULL DEFAULT false
             );
             DO $$
             BEGIN
               IF NOT EXISTS (
                 SELECT 1 FROM information_schema.columns
                  WHERE table_schema = current_schema()
                    AND table_name = 'instant_migration_checksums'
                    AND column_name = 'additive'
               ) THEN
                 ALTER TABLE instant_migration_checksums
                   ADD COLUMN additive boolean NOT NULL DEFAULT false;
               END IF;
             END $$;",
        )
        .await
}

/// Returns the database version, refusing any schema newer than the binary.
///
/// Mutating operations (`migrate down`, `migrate force`) use this strict form:
/// rolling back or rewriting a schema produced by a newer binary is unsafe even
/// when the newer migrations are additive.
async fn current_version(client: &Client) -> Result<u64, MigrationError> {
    let version = read_current_version(client).await?;
    let latest = latest_embedded_version();
    if version > latest {
        return Err(newer_database_error(version, latest));
    }
    Ok(version)
}

/// Returns the database version, allowing a newer schema only while every
/// migration between this binary's latest and the database version was recorded
/// as additive when it was applied.
async fn compatible_current_version(client: &Client) -> Result<u64, MigrationError> {
    let version = read_current_version(client).await?;
    let latest = latest_embedded_version();
    if version > latest && !recorded_additive_floor(client, latest, version).await? {
        return Err(newer_database_error(version, latest));
    }
    Ok(version)
}

async fn read_current_version(client: &Client) -> Result<u64, MigrationError> {
    let rows = client
        .query("SELECT version, dirty FROM schema_migrations", &[])
        .await?;
    if rows.len() > 1 {
        return Err(MigrationError::CorruptState);
    }
    let Some(row) = rows.first() else {
        return Ok(0);
    };
    let version = row.get::<_, i64>(0);
    if row.get::<_, bool>(1) {
        return Err(MigrationError::Dirty(version));
    }
    u64::try_from(version).map_err(|_| MigrationError::NegativeVersion(version))
}

pub(crate) fn latest_embedded_version() -> u64 {
    MIGRATIONS.last().map_or(0, |migration| migration.version)
}

fn newer_database_error(database: u64, binary: u64) -> MigrationError {
    MigrationError::NewerDatabase {
        database: i64::try_from(database).unwrap_or(i64::MAX),
        binary,
    }
}

async fn recorded_additive_floor(
    client: &Client,
    binary: u64,
    database: u64,
) -> Result<bool, MigrationError> {
    let rows = client
        .query(
            "SELECT version, additive FROM instant_migration_checksums
              WHERE version > $1 AND version <= $2",
            &[
                &i64::try_from(binary).unwrap_or(i64::MAX),
                &i64::try_from(database).unwrap_or(i64::MAX),
            ],
        )
        .await?;
    let recorded = rows
        .iter()
        .map(|row| {
            (
                u64::try_from(row.get::<_, i64>(0)).unwrap_or_default(),
                row.get::<_, bool>(1),
            )
        })
        .collect::<Vec<_>>();
    Ok(additive_floor_allows(binary, database, &recorded))
}

/// Returns whether `database` is a compatible successor to `binary` given the
/// recorded `(version, additive)` pairs.
fn additive_floor_allows(binary: u64, database: u64, recorded: &[(u64, bool)]) -> bool {
    (binary.saturating_add(1)..=database).all(|version| {
        recorded
            .iter()
            .any(|(recorded_version, additive)| *recorded_version == version && *additive)
    })
}

async fn raw_version(client: &Client) -> Result<u64, MigrationError> {
    let rows = client
        .query("SELECT version FROM schema_migrations", &[])
        .await?;
    if rows.len() > 1 {
        return Err(MigrationError::CorruptState);
    }
    let Some(row) = rows.first() else {
        return Ok(0);
    };
    let version = row.get::<_, i64>(0);
    u64::try_from(version).map_err(|_| MigrationError::NegativeVersion(version))
}

async fn reconcile_checksums(client: &Client, current: u64) -> Result<(), MigrationError> {
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= current)
    {
        let checksum = migration_checksum(migration);
        let version = i64::try_from(migration.version).unwrap();
        let row = client
            .query_opt(
                "SELECT name, checksum FROM instant_migration_checksums WHERE version = $1",
                &[&version],
            )
            .await?;
        if let Some(row) = row {
            let name = row.get::<_, String>(0);
            let recorded = row.get::<_, Vec<u8>>(1);
            if name != migration.name || recorded != checksum {
                return Err(MigrationError::ChecksumMismatch {
                    version: migration.version,
                    name,
                });
            }
        } else {
            client
                .execute(
                    "INSERT INTO instant_migration_checksums (version, name, checksum, additive)
                     VALUES ($1, $2, $3, $4)",
                    &[&version, &migration.name, &checksum, &migration.additive],
                )
                .await?;
        }
    }
    Ok(())
}

async fn verify_checksums(client: &Client, current: u64) -> Result<(), MigrationError> {
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= current)
    {
        let version = i64::try_from(migration.version).unwrap();
        let Some(row) = client
            .query_opt(
                "SELECT name, checksum FROM instant_migration_checksums WHERE version = $1",
                &[&version],
            )
            .await?
        else {
            return Err(MigrationError::MissingChecksum {
                version: migration.version,
                name: migration.name,
            });
        };
        let name = row.get::<_, String>(0);
        let recorded = row.get::<_, Vec<u8>>(1);
        if name != migration.name || recorded != migration_checksum(migration) {
            return Err(MigrationError::ChecksumMismatch {
                version: migration.version,
                name,
            });
        }
    }
    Ok(())
}

fn migration_checksum(migration: &EmbeddedMigration) -> Vec<u8> {
    Sha256::digest(migration.up.as_bytes()).to_vec()
}

async fn record_checksum_and_clean(
    client: &mut Client,
    migration: &EmbeddedMigration,
) -> Result<(), MigrationError> {
    let transaction = client.transaction().await?;
    let version = i64::try_from(migration.version).unwrap();
    let checksum = migration_checksum(migration);
    transaction
        .execute(
            "INSERT INTO instant_migration_checksums (version, name, checksum, additive)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (version) DO UPDATE
             SET name = excluded.name, checksum = excluded.checksum,
                 additive = excluded.additive",
            &[&version, &migration.name, &checksum, &migration.additive],
        )
        .await?;
    replace_state(&transaction, Some((migration.version, false))).await?;
    transaction.commit().await?;
    Ok(())
}

async fn write_state(
    client: &mut Client,
    state: Option<(u64, bool)>,
) -> Result<(), tokio_postgres::Error> {
    let transaction = client.transaction().await?;
    replace_state(&transaction, state).await?;
    transaction.commit().await
}

async fn replace_state<C>(
    client: &C,
    state: Option<(u64, bool)>,
) -> Result<(), tokio_postgres::Error>
where
    C: tokio_postgres::GenericClient + Sync,
{
    client.execute("DELETE FROM schema_migrations", &[]).await?;
    if let Some((version, dirty)) = state {
        client
            .execute(
                "INSERT INTO schema_migrations (version, dirty) VALUES ($1, $2)",
                &[&i64::try_from(version).unwrap(), &dirty],
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn embedded_migrations_are_complete_contiguous_and_paired() {
        assert_eq!(MIGRATIONS.len(), 152);
        assert_eq!(MIGRATIONS.first().unwrap().version, 1);
        assert_eq!(MIGRATIONS.last().unwrap().version, 152);
        assert_eq!(
            MIGRATIONS
                .iter()
                .map(|migration| migration.version)
                .collect::<HashSet<_>>()
                .len(),
            MIGRATIONS.len()
        );
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(migration.version, u64::try_from(index).unwrap() + 1);
            assert!(!migration.name.is_empty());
            assert!(!migration.up.trim().is_empty());
            assert!(!migration.down.trim().is_empty());
        }
    }

    #[test]
    fn checksums_are_stable_and_cover_sql_bytes() {
        let first = &MIGRATIONS[0];
        assert_eq!(
            migration_checksum(first),
            Sha256::digest(first.up.as_bytes()).to_vec()
        );
        assert_ne!(
            migration_checksum(first),
            migration_checksum(&MIGRATIONS[1])
        );
    }

    #[test]
    fn additive_compatibility_markers_match_embedded_sql() {
        for migration in MIGRATIONS {
            assert_eq!(
                migration.additive,
                migration.up.lines().next() == Some("-- compat: additive"),
                "migration {}",
                migration.version
            );
        }
        let latest = MIGRATIONS.last().unwrap();
        assert_eq!(latest.version, 152);
        assert!(latest.additive);
    }

    #[test]
    fn additive_floor_accepts_an_equal_schema_version() {
        assert!(additive_floor_allows(151, 151, &[]));
        assert!(additive_floor_allows(151, 151, &[(151, false)]));
    }

    #[test]
    fn additive_floor_accepts_newer_versions_only_when_every_migration_is_additive() {
        assert!(additive_floor_allows(149, 151, &[(150, true), (151, true)]));
        assert!(!additive_floor_allows(
            149,
            151,
            &[(150, true), (151, false)]
        ));
        assert!(!additive_floor_allows(
            149,
            151,
            &[(150, false), (151, true)]
        ));
        assert!(
            !additive_floor_allows(149, 151, &[(150, true)]),
            "a migration without a recorded additive flag is not compatible"
        );
        assert!(
            additive_floor_allows(149, 151, &[(148, false), (150, true), (151, true)]),
            "records at or below the binary's version are not part of the floor"
        );
    }
}
