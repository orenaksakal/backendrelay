//! Native PostgreSQL schema introspection for Bring Your Own Postgres applications.

use std::collections::BTreeMap;

use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType, Cardinality};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_postgres::GenericClient;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByopField {
    pub database_type: String,
    pub attribute_id: Uuid,
    pub indexed: bool,
    pub unique: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByopPrimaryKey {
    pub field: String,
    pub attribute_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ByopRelationDirection {
    Forward,
    Reverse,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByopRelation {
    pub direction: ByopRelationDirection,
    pub attribute_id: Uuid,
    pub table: String,
    pub table_field: String,
    pub other_table: String,
    pub other_table_field: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByopTable {
    pub fields: BTreeMap<String, ByopField>,
    pub attribute_id_to_field: BTreeMap<Uuid, String>,
    pub relations: BTreeMap<String, ByopRelation>,
    pub primary_key: ByopPrimaryKey,
    pub created_at_field: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByopSchema {
    pub catalog: AttributeCatalog,
    pub tables: BTreeMap<String, ByopTable>,
}

#[derive(Debug, Error)]
pub enum ByopIntrospectionError {
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
    #[error("introspection returned a field for unknown table OID {0}")]
    UnknownTable(i64),
    #[error("introspection returned a foreign key for an unknown table OID {0}")]
    UnknownForeignTable(i64),
}

#[derive(Debug)]
struct TableBuilder {
    name: String,
    fields: BTreeMap<String, ByopField>,
    attribute_id_to_field: BTreeMap<Uuid, String>,
    relations: BTreeMap<String, ByopRelation>,
    primary_key: ByopPrimaryKey,
    created_at_field: Option<String>,
}

/// Introspects one PostgreSQL namespace into the stable synthetic attribute and
/// relation model used by `InstantDB`'s BYOP query path. Only ordinary tables with
/// exactly one primary-key column are exposed. Foreign keys are exposed only when
/// they contain one column and the referencing column is the leading indexed column.
///
/// # Errors
///
/// Returns a PostgreSQL error or an inconsistent-catalog error.
#[allow(clippy::too_many_lines)] // Keeps the three catalog snapshots and their assembly atomic in one entry point.
pub async fn introspect_byop_schema<C>(
    client: &C,
    namespace: &str,
) -> Result<ByopSchema, ByopIntrospectionError>
where
    C: GenericClient + Sync,
{
    let table_rows = client
        .query(
            "SELECT rel.oid::bigint,
                    rel.relname,
                    pk.attnum::integer,
                    pk.attname
               FROM pg_catalog.pg_class rel
               JOIN pg_catalog.pg_namespace nsp ON nsp.oid = rel.relnamespace
               JOIN pg_catalog.pg_constraint con
                 ON con.conrelid = rel.oid AND con.contype = 'p'
               JOIN LATERAL (
                    SELECT att.attnum, att.attname
                      FROM pg_catalog.pg_attribute att
                     WHERE att.attrelid = rel.oid
                       AND att.attnum = ANY(con.conkey)
                       AND NOT att.attisdropped
               ) pk ON true
              WHERE nsp.nspname = $1
                AND rel.relkind = 'r'
                AND cardinality(con.conkey) = 1
              ORDER BY rel.relname",
            &[&namespace],
        )
        .await?;

    let mut builders = BTreeMap::new();
    for row in table_rows {
        let oid: i64 = row.get(0);
        let name: String = row.get(1);
        let primary_attnum: i32 = row.get(2);
        let primary_field: String = row.get(3);
        builders.insert(
            oid,
            TableBuilder {
                name,
                fields: BTreeMap::new(),
                attribute_id_to_field: BTreeMap::new(),
                relations: BTreeMap::new(),
                primary_key: ByopPrimaryKey {
                    field: primary_field,
                    attribute_id: attribute_uuid(oid, primary_attnum),
                },
                created_at_field: None,
            },
        );
    }
    if builders.is_empty() {
        return Ok(ByopSchema::default());
    }

    let field_rows = client
        .query(
            "SELECT rel.oid::bigint,
                    att.attnum::integer,
                    att.attname,
                    typ.typname,
                     EXISTS (
                       SELECT 1 FROM pg_catalog.pg_index idx
                        WHERE idx.indrelid = rel.oid
                          AND idx.indnkeyatts > 0
                          AND idx.indkey[0] = att.attnum
                     ) AS indexed,
                    EXISTS (
                      SELECT 1 FROM pg_catalog.pg_constraint con
                       WHERE con.conrelid = rel.oid
                         AND con.contype = 'u'
                         AND cardinality(con.conkey) = 1
                         AND att.attnum = ANY(con.conkey)
                     ) AS unique_field,
                    EXISTS (
                      WITH RECURSIVE type_chain(oid) AS (
                        SELECT typ.oid
                        UNION ALL
                        SELECT base.typbasetype
                          FROM pg_catalog.pg_type base
                          JOIN type_chain chain ON chain.oid = base.oid
                         WHERE base.typbasetype <> 0
                      )
                      SELECT 1
                        FROM pg_catalog.pg_type date_type
                        JOIN type_chain chain ON chain.oid = date_type.oid
                       WHERE date_type.typname IN
                             ('time', 'timestamp', 'timetz', 'timestamptz', 'time_stamp', 'date')
                    ) AS date_field
               FROM pg_catalog.pg_class rel
               JOIN pg_catalog.pg_namespace nsp ON nsp.oid = rel.relnamespace
               JOIN pg_catalog.pg_attribute att ON att.attrelid = rel.oid
               JOIN pg_catalog.pg_type typ ON typ.oid = att.atttypid
              WHERE nsp.nspname = $1
                AND rel.oid::bigint = ANY($2::bigint[])
                AND att.attnum > 0
                AND NOT att.attisdropped
              ORDER BY rel.relname, att.attnum",
            &[&namespace, &builders.keys().copied().collect::<Vec<_>>()],
        )
        .await?;

    let mut attributes = Vec::new();
    for row in field_rows {
        let oid: i64 = row.get(0);
        let attnum: i32 = row.get(1);
        let field_name: String = row.get(2);
        let database_type: String = row.get(3);
        let indexed: bool = row.get(4);
        let unique: bool = row.get(5);
        let date_field: bool = row.get(6);
        let builder = builders
            .get_mut(&oid)
            .ok_or(ByopIntrospectionError::UnknownTable(oid))?;
        let attribute_id = attribute_uuid(oid, attnum);
        builder
            .attribute_id_to_field
            .insert(attribute_id, field_name.clone());
        builder.fields.insert(
            field_name.clone(),
            ByopField {
                database_type: database_type.clone(),
                attribute_id,
                indexed,
                unique,
            },
        );
        if builder.created_at_field.is_none()
            && field_name.to_ascii_lowercase().starts_with("created")
            && (date_field || is_date_type(&database_type))
        {
            builder.created_at_field = Some(field_name.clone());
        }
        attributes.push(blob_attribute(
            attribute_id,
            &builder.name,
            &field_name,
            indexed,
            unique,
        ));
    }

    let foreign_key_rows = client
        .query(
            "SELECT con.oid::bigint,
                    source.oid::bigint,
                    source.relname,
                    source_att.attname,
                    target.oid::bigint,
                    target.relname,
                    target_att.attname,
                    EXISTS (
                       SELECT 1 FROM pg_catalog.pg_index idx
                        WHERE idx.indrelid = source.oid
                          AND idx.indnkeyatts > 0
                          AND idx.indkey[0] = source_att.attnum
                     ) AS source_indexed,
                     EXISTS (
                       SELECT 1 FROM pg_catalog.pg_index idx
                        WHERE idx.indrelid = target.oid
                          AND idx.indnkeyatts > 0
                          AND idx.indkey[0] = target_att.attnum
                     ) AS target_indexed
               FROM pg_catalog.pg_constraint con
               JOIN pg_catalog.pg_class source ON source.oid = con.conrelid
               JOIN pg_catalog.pg_class target ON target.oid = con.confrelid
               JOIN pg_catalog.pg_namespace nsp ON nsp.oid = source.relnamespace
               JOIN pg_catalog.pg_attribute source_att
                 ON source_att.attrelid = source.oid
                AND source_att.attnum = con.conkey[1]
               JOIN pg_catalog.pg_attribute target_att
                 ON target_att.attrelid = target.oid
                AND target_att.attnum = con.confkey[1]
              WHERE nsp.nspname = $1
                AND con.contype = 'f'
                AND cardinality(con.conkey) = 1
                AND cardinality(con.confkey) = 1
              ORDER BY con.oid",
            &[&namespace],
        )
        .await?;

    for row in foreign_key_rows {
        let constraint_oid: i64 = row.get(0);
        let source_oid: i64 = row.get(1);
        let source_name: String = row.get(2);
        let source_field: String = row.get(3);
        let target_oid: i64 = row.get(4);
        let target_name: String = row.get(5);
        let target_field: String = row.get(6);
        let source_indexed: bool = row.get(7);
        let target_indexed: bool = row.get(8);
        let attribute_id = constraint_uuid(constraint_oid);
        if source_indexed && let Some(source) = builders.get_mut(&source_oid) {
            source.relations.insert(
                target_name.clone(),
                ByopRelation {
                    direction: ByopRelationDirection::Forward,
                    attribute_id,
                    table: source_name.clone(),
                    table_field: source_field.clone(),
                    other_table: target_name.clone(),
                    other_table_field: target_field.clone(),
                },
            );
            attributes.push(reference_attribute(
                attribute_id,
                &source_name,
                &target_name,
                source_indexed && target_indexed,
            ));
        }
        if target_indexed && let Some(target) = builders.get_mut(&target_oid) {
            target.relations.insert(
                source_name.clone(),
                ByopRelation {
                    direction: ByopRelationDirection::Reverse,
                    attribute_id,
                    table: target_name.clone(),
                    table_field: target_field,
                    other_table: source_name.clone(),
                    other_table_field: source_field,
                },
            );
        }
    }

    let tables = builders
        .into_values()
        .map(|builder| {
            let name = builder.name.clone();
            (
                name,
                ByopTable {
                    fields: builder.fields,
                    attribute_id_to_field: builder.attribute_id_to_field,
                    relations: builder.relations,
                    primary_key: builder.primary_key,
                    created_at_field: builder.created_at_field,
                },
            )
        })
        .collect();
    Ok(ByopSchema {
        catalog: AttributeCatalog { attributes },
        tables,
    })
}

fn attribute_uuid(relation_oid: i64, attribute_number: i32) -> Uuid {
    Uuid::from_u64_pair(
        u64::try_from(relation_oid).expect("PostgreSQL relation OIDs are non-negative"),
        u64::try_from(attribute_number).expect("user attribute numbers are positive"),
    )
}

fn constraint_uuid(constraint_oid: i64) -> Uuid {
    Uuid::from_u64_pair(
        u64::try_from(constraint_oid).expect("PostgreSQL constraint OIDs are non-negative"),
        0,
    )
}

fn blob_attribute(id: Uuid, table: &str, field: &str, indexed: bool, unique: bool) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: table.to_owned(),
        label: field.to_owned(),
        value_type: AttributeValueType::Blob,
        cardinality: Cardinality::One,
        unique,
        setting_unique: false,
        indexed,
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

fn reference_attribute(
    id: Uuid,
    source_table: &str,
    target_table: &str,
    indexed: bool,
) -> Attribute {
    Attribute {
        id,
        wire: None,
        etype: source_table.to_owned(),
        label: target_table.to_owned(),
        value_type: AttributeValueType::Ref,
        cardinality: Cardinality::One,
        unique: false,
        setting_unique: false,
        indexed,
        required: false,
        checked_data_type: None,
        indexing: false,
        checking_data_type: false,
        reverse_etype: Some(target_table.to_owned()),
        reverse_label: Some(source_table.to_owned()),
        on_delete: None,
        on_delete_reverse: None,
    }
}

fn is_date_type(database_type: &str) -> bool {
    matches!(
        database_type,
        "time" | "timestamp" | "timetz" | "timestamptz" | "time_stamp" | "date"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_ids_match_java_uuid_long_pair_construction() {
        assert_eq!(
            attribute_uuid(16_384, 7).to_string(),
            "00000000-0000-4000-0000-000000000007"
        );
        assert_eq!(
            constraint_uuid(50_000).to_string(),
            "00000000-0000-c350-0000-000000000000"
        );
    }

    #[test]
    fn created_timestamp_detection_matches_the_reference_allowlist() {
        assert!(is_date_type("timestamptz"));
        assert!(is_date_type("date"));
        assert!(!is_date_type("varchar"));
    }
}
