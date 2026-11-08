use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;

use instant_instaql::{Attribute, AttributeCatalog, AttributeValueType};
use serde_json::{Map, Value as JsonValue, json};
use tokio_postgres::types::{Json, Type};
use tokio_postgres::{Row, Statement, Transaction};
use uuid::Uuid;

use crate::{
    PermissionDataLoader, PermissionEntityKey, PermissionEntityLoad, PermissionError,
    PermissionLoadFuture, RefCache, RefRequest,
};

/// Complete-entity query bound by `load_entities` and its prepared variant.
///
/// The text path parses it on every call; pooled callers can prepare it once
/// per physical connection and reuse the plan while still binding fresh
/// parameters and reading the current transaction snapshot.
pub const PERMISSION_ENTITY_SQL: &str = "SELECT entity_id, attr_id, value
                       FROM triples
                      WHERE app_id = $1 AND entity_id = ANY($2)
                      ORDER BY created_at, attr_id";

/// Projected-entity query used when a compiled view rule provably reads only a
/// known attribute set. It keeps the complete query's app scope and ordering.
pub const PERMISSION_ENTITY_PROJECTED_SQL: &str = "SELECT entity_id, attr_id, value
                       FROM triples
                      WHERE app_id = $1 AND entity_id = ANY($2) AND attr_id = ANY($3)
                      ORDER BY created_at, attr_id";

const PERMISSION_ENTITY_TYPES: [Type; 2] = [Type::UUID, Type::UUID_ARRAY];
const PERMISSION_ENTITY_PROJECTED_TYPES: [Type; 3] =
    [Type::UUID, Type::UUID_ARRAY, Type::UUID_ARRAY];

/// PostgreSQL implementation of complete/query-projected entity and CEL `ref`
/// loading.
pub struct PostgresPermissionDataLoader<'a> {
    pub app_id: Uuid,
    pub catalog: &'a AttributeCatalog,
}

/// Returns the fixed parameter types for [`PERMISSION_ENTITY_SQL`], so a
/// pooled loader can prepare the same shape once per connection.
#[must_use]
pub const fn permission_entity_parameter_types() -> &'static [Type] {
    &PERMISSION_ENTITY_TYPES
}

/// Returns the fixed parameter types for [`PERMISSION_ENTITY_PROJECTED_SQL`].
#[must_use]
pub const fn permission_entity_projected_parameter_types() -> &'static [Type] {
    &PERMISSION_ENTITY_PROJECTED_TYPES
}

impl PostgresPermissionDataLoader<'_> {
    /// Loads query-permission entities, splitting proven projections from
    /// complete entities. All complete groups share one statement and all
    /// projected groups share one statement, so a typical check issues a
    /// single projected query. Every projected entity still decodes to a JSON
    /// object, even when none of its projected attributes exist.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionError::Loader`] for execution failures.
    pub fn load_query_entities<'a>(
        &'a self,
        transaction: &'a tokio_postgres::Transaction<'_>,
        loads: Vec<PermissionEntityLoad>,
    ) -> PermissionLoadFuture<'a, HashMap<PermissionEntityKey, JsonValue>> {
        Box::pin(async move {
            let app_id = self.app_id;
            let catalog = self.catalog;
            load_entity_load_batch(
                loads,
                catalog,
                |entity_ids| async move {
                    transaction
                        .query_typed(
                            PERMISSION_ENTITY_SQL,
                            &[(&app_id, Type::UUID), (&entity_ids, Type::UUID_ARRAY)],
                        )
                        .await
                        .map_err(loader_error)
                },
                |entity_ids, attribute_ids| async move {
                    transaction
                        .query_typed(
                            PERMISSION_ENTITY_PROJECTED_SQL,
                            &[
                                (&app_id, Type::UUID),
                                (&entity_ids, Type::UUID_ARRAY),
                                (&attribute_ids, Type::UUID_ARRAY),
                            ],
                        )
                        .await
                        .map_err(loader_error)
                },
            )
            .await
        })
    }
}

/// One projected group after splitting a query-permission load batch.
struct ProjectedLoadGroup {
    attributes: Vec<Uuid>,
    entities: Vec<PermissionEntityKey>,
}

/// Splits a load batch into the complete-entity union and its projected groups.
fn split_entity_loads(
    loads: Vec<PermissionEntityLoad>,
) -> (Vec<PermissionEntityKey>, Vec<ProjectedLoadGroup>) {
    let mut complete = Vec::new();
    let mut projected = Vec::new();
    for load in loads {
        if load.entities.is_empty() {
            continue;
        }
        match load.attributes {
            Some(attributes) => projected.push(ProjectedLoadGroup {
                attributes,
                entities: load.entities,
            }),
            None => complete.extend(load.entities),
        }
    }
    (complete, projected)
}

fn unique_entity_ids(entities: &[PermissionEntityKey]) -> Vec<Uuid> {
    let mut ids = entities
        .iter()
        .map(|entity| entity.entity_id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Union of every projected entity id and attribute id. Attribute ids stay
/// grouped by their etype during decoding, so the shared filter cannot leak
/// another etype's attributes into an entity.
fn projected_statement_parameters(projected: &[ProjectedLoadGroup]) -> (Vec<Uuid>, Vec<Uuid>) {
    let mut entity_ids = Vec::new();
    let mut attribute_ids = Vec::new();
    for group in projected {
        entity_ids.extend(group.entities.iter().map(|entity| entity.entity_id));
        attribute_ids.extend(group.attributes.iter().copied());
    }
    entity_ids.sort_unstable();
    entity_ids.dedup();
    attribute_ids.sort_unstable();
    attribute_ids.dedup();
    (entity_ids, attribute_ids)
}

/// Executes at most one complete and one projected statement for a load batch.
///
/// The query callbacks let the text and prepared paths share the batching,
/// grouping and decoding rules while keeping their own parameter binding.
async fn load_entity_load_batch<CF, CFut, PF, PFut>(
    loads: Vec<PermissionEntityLoad>,
    catalog: &AttributeCatalog,
    query_complete: CF,
    query_projected: PF,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError>
where
    CF: FnOnce(Vec<Uuid>) -> CFut,
    CFut: Future<Output = Result<Vec<Row>, PermissionError>>,
    PF: FnOnce(Vec<Uuid>, Vec<Uuid>) -> PFut,
    PFut: Future<Output = Result<Vec<Row>, PermissionError>>,
{
    let (complete, projected) = split_entity_loads(loads);
    if complete.is_empty() && projected.is_empty() {
        return Ok(HashMap::new());
    }
    let attributes = attribute_index(catalog);
    let mut loaded = HashMap::new();
    if !complete.is_empty() {
        let rows = query_complete(unique_entity_ids(&complete)).await?;
        loaded.extend(decode_rows(&rows, &complete, &attributes, false, None)?);
    }
    if !projected.is_empty() {
        let (entity_ids, attribute_ids) = projected_statement_parameters(&projected);
        let rows = query_projected(entity_ids, attribute_ids).await?;
        for group in &projected {
            loaded.extend(decode_projected_group(&rows, group, &attributes)?);
        }
    }
    Ok(loaded)
}

async fn load_complete_entities(
    transaction: &tokio_postgres::Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entities: Vec<PermissionEntityKey>,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    let entity_ids = entities
        .iter()
        .map(|entity| entity.entity_id)
        .collect::<Vec<_>>();
    // The fixed UUID bindings let parse/bind/execute share one protocol
    // exchange. Keep the caller's transaction/snapshot and fresh rows;
    // no prepared statement, entity, reference or decision is cached.
    let rows = transaction
        .query_typed(
            PERMISSION_ENTITY_SQL,
            &[(&app_id, Type::UUID), (&entity_ids, Type::UUID_ARRAY)],
        )
        .await
        .map_err(loader_error)?;
    decode_entities(rows, entities, catalog)
}

impl PermissionDataLoader for PostgresPermissionDataLoader<'_> {
    fn load_entities<'a>(
        &'a self,
        transaction: &'a tokio_postgres::Transaction<'_>,
        entities: Vec<PermissionEntityKey>,
    ) -> PermissionLoadFuture<'a, HashMap<PermissionEntityKey, JsonValue>> {
        Box::pin(async move {
            if entities.is_empty() {
                return Ok(HashMap::new());
            }
            load_complete_entities(transaction, self.app_id, self.catalog, entities).await
        })
    }

    fn load_refs<'a>(
        &'a self,
        transaction: &'a tokio_postgres::Transaction<'_>,
        refs: Vec<RefRequest>,
    ) -> PermissionLoadFuture<'a, RefCache> {
        Box::pin(async move { load_ref_batch(transaction, self.app_id, self.catalog, refs).await })
    }
}

/// Executes a statement prepared from [`PERMISSION_ENTITY_SQL`] on a pooled
/// connection. Results, app scoping, ordering, snapshot and freshness are
/// identical to [`PostgresPermissionDataLoader::load_entities`]; only the parse
/// is skipped after the first use on that physical connection.
///
/// # Errors
///
/// Returns [`PermissionError::Loader`] for execution failures.
pub async fn load_permission_entities_with_statement(
    transaction: &Transaction<'_>,
    statement: &Statement,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entities: Vec<PermissionEntityKey>,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    if entities.is_empty() {
        return Ok(HashMap::new());
    }
    let entity_ids = entities
        .iter()
        .map(|entity| entity.entity_id)
        .collect::<Vec<_>>();
    let rows = transaction
        .query(
            statement,
            &[
                &app_id as &(dyn tokio_postgres::types::ToSql + Sync),
                &entity_ids,
            ],
        )
        .await
        .map_err(loader_error)?;
    decode_entities(rows, entities, catalog)
}

/// Executes a statement prepared from [`PERMISSION_ENTITY_PROJECTED_SQL`].
///
/// Unlike the complete path, every requested entity decodes to a JSON object
/// even when the projected attribute set has no rows for it.
///
/// # Errors
///
/// Returns [`PermissionError::Loader`] for execution failures.
pub async fn load_permission_entities_projected_with_statement(
    transaction: &Transaction<'_>,
    statement: &Statement,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    entities: Vec<PermissionEntityKey>,
    attributes: Vec<Uuid>,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    if entities.is_empty() {
        return Ok(HashMap::new());
    }
    let entity_ids = entities
        .iter()
        .map(|entity| entity.entity_id)
        .collect::<Vec<_>>();
    let rows = transaction
        .query(
            statement,
            &[
                &app_id as &(dyn tokio_postgres::types::ToSql + Sync),
                &entity_ids,
                &attributes,
            ],
        )
        .await
        .map_err(loader_error)?;
    decode_projected_entities(rows, entities, catalog)
}

/// Executes complete and projected statements for one prepared query-permission
/// load batch. Results match [`PostgresPermissionDataLoader::load_query_entities`].
///
/// # Errors
///
/// Returns [`PermissionError::Loader`] for execution failures.
pub async fn load_permission_entity_loads_with_statements(
    transaction: &Transaction<'_>,
    complete_statement: &Statement,
    projected_statement: &Statement,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    loads: Vec<PermissionEntityLoad>,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    load_entity_load_batch(
        loads,
        catalog,
        |entity_ids| async move {
            transaction
                .query(
                    complete_statement,
                    &[
                        &app_id as &(dyn tokio_postgres::types::ToSql + Sync),
                        &entity_ids,
                    ],
                )
                .await
                .map_err(loader_error)
        },
        |entity_ids, attribute_ids| async move {
            transaction
                .query(
                    projected_statement,
                    &[
                        &app_id as &(dyn tokio_postgres::types::ToSql + Sync),
                        &entity_ids,
                        &attribute_ids,
                    ],
                )
                .await
                .map_err(loader_error)
        },
    )
    .await
}

fn decode_entities(
    rows: Vec<Row>,
    entities: Vec<PermissionEntityKey>,
    catalog: &AttributeCatalog,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    let attributes = attribute_index(catalog);
    decode_rows(&rows, &entities, &attributes, false, None)
}

/// Projected loads still return an entity object when no projected attribute
/// exists for that entity, matching the complete load's object contract.
fn decode_projected_entities(
    rows: Vec<Row>,
    entities: Vec<PermissionEntityKey>,
    catalog: &AttributeCatalog,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    let attributes = attribute_index(catalog);
    decode_rows(&rows, &entities, &attributes, true, None)
}

fn decode_projected_group(
    rows: &[Row],
    group: &ProjectedLoadGroup,
    attributes: &HashMap<Uuid, &Attribute>,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    let projection = group.attributes.iter().copied().collect::<HashSet<_>>();
    decode_rows(rows, &group.entities, attributes, true, Some(&projection))
}

/// Row decoding must not linearly scan the whole catalog per triple.
/// First-entry-wins matches AttributeCatalog::by_id, even for a synthetic
/// catalog containing duplicate IDs. This index is local to the fresh read;
/// no catalog, decision or entity cache is added.
fn attribute_index(catalog: &AttributeCatalog) -> HashMap<Uuid, &Attribute> {
    let mut attributes = HashMap::with_capacity(catalog.attributes.len());
    for attribute in &catalog.attributes {
        attributes.entry(attribute.id).or_insert(attribute);
    }
    attributes
}

fn decode_rows(
    rows: &[Row],
    entities: &[PermissionEntityKey],
    attributes: &HashMap<Uuid, &Attribute>,
    include_empty: bool,
    projection: Option<&HashSet<Uuid>>,
) -> Result<HashMap<PermissionEntityKey, JsonValue>, PermissionError> {
    let requested = entities.iter().cloned().collect::<HashSet<_>>();
    let mut objects = HashMap::<PermissionEntityKey, Map<String, JsonValue>>::new();
    if include_empty {
        objects.extend(
            requested
                .iter()
                .map(|key| (key.clone(), Map::<String, JsonValue>::new())),
        );
    }
    for row in rows {
        let attr_id: Uuid = row.get("attr_id");
        // A projected group may share its statement with other groups. Keep
        // every entity limited to the projection it was requested under.
        if projection.is_some_and(|ids| !ids.contains(&attr_id)) {
            continue;
        }
        let entity_id: Uuid = row.get("entity_id");
        let Some(attribute) = attributes.get(&attr_id) else {
            continue;
        };
        let key = PermissionEntityKey {
            etype: attribute.etype.clone(),
            entity_id,
        };
        if !requested.contains(&key) {
            continue;
        }
        let Json(value): Json<JsonValue> = row.get("value");
        objects
            .entry(key)
            .or_default()
            .insert(attribute.label.clone(), value);
    }
    Ok(objects
        .into_iter()
        .map(|(key, object)| (key, JsonValue::Object(object)))
        .collect())
}

/// Loads every requested reference path with one statement per path segment and
/// per distinct `(etype, path)`, independent of how many entities are being
/// checked. Requests that share a path are resolved together; each entity keeps
/// the exact ordering and duplicate semantics of an individual lookup.
async fn load_ref_batch(
    transaction: &tokio_postgres::Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    requests: Vec<RefRequest>,
) -> Result<RefCache, PermissionError> {
    let mut groups = BTreeMap::<(String, String), Vec<RefRequest>>::new();
    for request in requests {
        groups
            .entry((request.etype.clone(), request.path.clone()))
            .or_default()
            .push(request);
    }
    let mut loaded = RefCache::new();
    for ((etype, path), group) in groups {
        let values = load_ref_group(transaction, app_id, catalog, &etype, &path, &group).await?;
        for (request, values) in group.into_iter().zip(values) {
            loaded.insert(request, values);
        }
    }
    Ok(loaded)
}

/// Resolves one `(etype, path)` group. `requests` all share `etype` and `path`,
/// and the returned vectors are aligned with that request order.
#[allow(clippy::too_many_lines)]
async fn load_ref_group(
    transaction: &tokio_postgres::Transaction<'_>,
    app_id: Uuid,
    catalog: &AttributeCatalog,
    etype: &str,
    path: &str,
    requests: &[RefRequest],
) -> Result<Vec<Vec<JsonValue>>, PermissionError> {
    let segments = path
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.is_empty() {
        return Ok(vec![Vec::new(); requests.len()]);
    }
    let mut entity_ids = Vec::with_capacity(requests.len());
    for request in requests {
        let Some(entity_id) = request.entity_id.as_uuid() else {
            return Err(PermissionError::Loader(
                "Instant entity references require UUID identifiers".to_owned(),
            ));
        };
        entity_ids.push(vec![entity_id]);
    }

    let mut etype = etype;
    for (index, label) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        if let Some(attribute) = catalog.forward(etype, label) {
            // One query covers the union of every request's current entities.
            // Rows stay in the legacy global `created_at, entity_id` order and
            // are then attributed to each root that requested their entity.
            let mut roots_by_id = HashMap::<Uuid, Vec<usize>>::new();
            let mut union = HashSet::new();
            for (root, ids) in entity_ids.iter().enumerate() {
                for id in ids {
                    union.insert(*id);
                    let roots = roots_by_id.entry(*id).or_default();
                    if roots.last() != Some(&root) {
                        roots.push(root);
                    }
                }
            }
            if union.is_empty() {
                return Ok(vec![Vec::new(); requests.len()]);
            }
            let mut union = union.into_iter().collect::<Vec<_>>();
            union.sort_unstable();
            let rows = transaction
                .query_typed(
                    "SELECT entity_id, value
                       FROM triples
                      WHERE app_id = $1 AND attr_id = $2 AND entity_id = ANY($3)
                      ORDER BY created_at, entity_id",
                    &[
                        (&app_id, Type::UUID),
                        (&attribute.id, Type::UUID),
                        (&union, Type::UUID_ARRAY),
                    ],
                )
                .await
                .map_err(loader_error)?;
            let mut values = vec![Vec::<JsonValue>::new(); requests.len()];
            for row in rows {
                let entity_id = row.get::<_, Uuid>("entity_id");
                let Some(roots) = roots_by_id.get(&entity_id) else {
                    continue;
                };
                let value = row.get::<_, Json<JsonValue>>("value").0;
                for root in roots {
                    values[*root].push(value.clone());
                }
            }
            if last {
                return Ok(values);
            }
            if attribute.value_type != AttributeValueType::Ref {
                return Ok(vec![Vec::new(); requests.len()]);
            }
            let Some(next_etype) = attribute.reverse_etype.as_deref() else {
                return Ok(vec![Vec::new(); requests.len()]);
            };
            entity_ids = values
                .into_iter()
                .map(|values| {
                    values
                        .into_iter()
                        .filter_map(|value| {
                            value.as_str().and_then(|value| Uuid::parse_str(value).ok())
                        })
                        .collect()
                })
                .collect();
            etype = next_etype;
        } else if let Some(attribute) = catalog.reverse(etype, label) {
            // Preserve the legacy concatenation order, including repeated
            // targets: each input occurrence owns one ordered group of matching
            // rows. ANY()/DISTINCT here would incorrectly remove duplicate
            // traversal paths, so the root and its ordinal travel to SQL.
            let mut requested = Vec::new();
            let mut roots = Vec::<i64>::new();
            for (root, ids) in entity_ids.iter().enumerate() {
                let Ok(root) = i64::try_from(root) else {
                    continue;
                };
                for id in ids {
                    requested.push(*id);
                    roots.push(root);
                }
            }
            if requested.is_empty() {
                return Ok(vec![Vec::new(); requests.len()]);
            }
            let rows = transaction
                .query_typed(
                    "SELECT batch.root, matched.entity_id
                       FROM unnest($3::uuid[], $4::bigint[]) WITH ORDINALITY
                            AS batch(entity_id, root, ordinal)
                       CROSS JOIN LATERAL (
                           SELECT entity_id, created_at FROM triples
                            WHERE app_id = $1 AND attr_id = $2
                              AND value = to_jsonb(batch.entity_id)
                       ) AS matched
                      ORDER BY batch.root, batch.ordinal,
                               matched.created_at, matched.entity_id",
                    &[
                        (&app_id, Type::UUID),
                        (&attribute.id, Type::UUID),
                        (&requested, Type::UUID_ARRAY),
                        (&roots, Type::INT8_ARRAY),
                    ],
                )
                .await
                .map_err(loader_error)?;
            let mut next_ids = vec![Vec::<Uuid>::new(); requests.len()];
            for row in rows {
                let root = row.get::<_, i64>("root");
                let Some(root) = usize::try_from(root)
                    .ok()
                    .filter(|root| *root < requests.len())
                else {
                    continue;
                };
                next_ids[root].push(row.get::<_, Uuid>("entity_id"));
            }
            if last {
                return Ok(next_ids
                    .into_iter()
                    .map(|ids| ids.into_iter().map(|id| json!(id)).collect())
                    .collect());
            }
            entity_ids = next_ids;
            etype = &attribute.etype;
        } else {
            return Ok(vec![Vec::new(); requests.len()]);
        }
    }
    Ok(vec![Vec::new(); requests.len()])
}

#[allow(clippy::needless_pass_by_value)] // `Result::map_err` transfers ownership.
fn loader_error(error: tokio_postgres::Error) -> PermissionError {
    PermissionError::Loader(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use instant_instaql::{AttributeValueType, Cardinality};

    use super::*;

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

    fn key(etype: &str, entity_id: Uuid) -> PermissionEntityKey {
        PermissionEntityKey {
            etype: etype.to_owned(),
            entity_id,
        }
    }

    fn catalog() -> AttributeCatalog {
        AttributeCatalog {
            attributes: vec![
                attribute(1, "posts", "id"),
                attribute(2, "posts", "owner"),
                attribute(3, "messages", "id"),
                attribute(4, "messages", "memberIds"),
            ],
        }
    }

    #[tokio::test]
    async fn projected_groups_share_one_statement_and_decode_empty_objects() {
        let catalog = catalog();
        let posts_id = Uuid::from_u128(1);
        let posts_owner = Uuid::from_u128(2);
        let messages_id = Uuid::from_u128(3);
        let messages_member = Uuid::from_u128(4);
        let post_one = Uuid::from_u128(11);
        let post_two = Uuid::from_u128(12);
        let message_one = Uuid::from_u128(21);
        let complete_post = Uuid::from_u128(13);
        let complete_calls = Mutex::new(Vec::<Vec<Uuid>>::new());
        let projected_calls = Mutex::new(Vec::<(Vec<Uuid>, Vec<Uuid>)>::new());
        let complete_recorder = &complete_calls;
        let projected_recorder = &projected_calls;
        let loaded = load_entity_load_batch(
            vec![
                PermissionEntityLoad {
                    entities: vec![key("posts", post_one), key("posts", post_two)],
                    attributes: Some(vec![posts_id, posts_owner]),
                },
                PermissionEntityLoad {
                    entities: vec![key("messages", message_one)],
                    attributes: Some(vec![messages_id, messages_member]),
                },
                PermissionEntityLoad {
                    entities: vec![key("posts", complete_post)],
                    attributes: None,
                },
            ],
            &catalog,
            |entity_ids| async move {
                complete_recorder.lock().unwrap().push(entity_ids);
                Ok(Vec::new())
            },
            |entity_ids, attribute_ids| async move {
                projected_recorder
                    .lock()
                    .unwrap()
                    .push((entity_ids, attribute_ids));
                Ok(Vec::new())
            },
        )
        .await
        .unwrap();
        assert_eq!(
            complete_calls.lock().unwrap().len(),
            1,
            "one complete statement"
        );
        assert_eq!(
            projected_calls.lock().unwrap().len(),
            1,
            "both projected groups share one statement"
        );
        assert_eq!(
            complete_calls.lock().unwrap()[0],
            vec![complete_post],
            "complete statement binds the complete-entity union"
        );
        let (entity_ids, attribute_ids) = projected_calls.lock().unwrap()[0].clone();
        assert_eq!(entity_ids, vec![post_one, post_two, message_one]);
        assert_eq!(
            attribute_ids,
            vec![posts_id, posts_owner, messages_id, messages_member]
        );
        // Complete entities without rows keep the old omission contract;
        // projected entities always decode to an object.
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[&key("posts", post_one)], json!({}));
        assert_eq!(loaded[&key("posts", post_two)], json!({}));
        assert_eq!(loaded[&key("messages", message_one)], json!({}));
        assert!(!loaded.contains_key(&key("posts", complete_post)));
    }

    #[tokio::test]
    async fn only_projected_groups_skip_the_complete_statement() {
        let catalog = catalog();
        let projected_calls = Mutex::new(0usize);
        let recorder = &projected_calls;
        let loaded = load_entity_load_batch(
            vec![PermissionEntityLoad {
                entities: vec![key("posts", Uuid::from_u128(11))],
                attributes: Some(vec![Uuid::from_u128(1)]),
            }],
            &catalog,
            |_| async { panic!("complete statement must not run for projected-only loads") },
            |_entity_ids, _attribute_ids| async move {
                *recorder.lock().unwrap() += 1;
                Ok(Vec::new())
            },
        )
        .await
        .unwrap();
        assert_eq!(*projected_calls.lock().unwrap(), 1);
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn split_keeps_empty_loads_out_and_unions_duplicate_ids() {
        let post = Uuid::from_u128(11);
        let (complete, projected) = split_entity_loads(vec![
            PermissionEntityLoad {
                entities: vec![],
                attributes: Some(vec![Uuid::from_u128(1)]),
            },
            PermissionEntityLoad {
                entities: vec![key("posts", post), key("posts", post)],
                attributes: Some(vec![Uuid::from_u128(1)]),
            },
        ]);
        assert!(complete.is_empty());
        assert_eq!(projected.len(), 1);
        let (entity_ids, attribute_ids) = projected_statement_parameters(&projected);
        assert_eq!(entity_ids, vec![post]);
        assert_eq!(attribute_ids, vec![Uuid::from_u128(1)]);
    }
}
