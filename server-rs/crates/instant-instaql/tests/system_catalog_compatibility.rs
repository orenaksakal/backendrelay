use instant_datalog::{NormalizedClause, Term};
use instant_instaql::{
    AttributeValueType, Cardinality, DeleteAction, SYSTEM_CATALOG_APP_ID, SYSTEM_CATALOG_USER_ID,
    compile_query, decode_system_uuid, is_system_catalog_identity, system_attr_id,
    system_attribute_catalog, system_catalog_attributes, system_ident_id,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

#[derive(Deserialize)]
struct SystemCatalogFixture {
    source: String,
    attribute_count: usize,
    namespaces: Vec<NamespaceFixture>,
}

#[derive(Deserialize)]
struct NamespaceFixture {
    etype: String,
    id_attr: Uuid,
    id_ident: Uuid,
    default_fetch: Vec<String>,
    schema: Vec<String>,
}

fn fixture() -> SystemCatalogFixture {
    serde_json::from_str(include_str!("fixtures/system_catalog.json")).unwrap()
}

#[test]
fn complete_catalog_ids_and_default_fetches_match_the_jvm_source() {
    let fixture = fixture();
    assert_eq!(fixture.source, "server/src/instant/system_catalog.clj");
    assert_eq!(
        SYSTEM_CATALOG_APP_ID.to_string(),
        "a1111111-1111-1111-1111-111111111ca7"
    );
    assert_eq!(
        SYSTEM_CATALOG_USER_ID.to_string(),
        "e1111111-1111-1111-1111-111111111ca7"
    );

    let specs = system_catalog_attributes().unwrap();
    let catalog = system_attribute_catalog().unwrap();
    assert_eq!(specs.len(), fixture.attribute_count);
    assert_eq!(catalog.attributes.len(), fixture.attribute_count);

    let query = Value::Object(
        fixture
            .namespaces
            .iter()
            .map(|namespace| (namespace.etype.clone(), Value::Object(Map::new())))
            .collect(),
    );
    let compiled = compile_query(&query, &catalog, false).unwrap();
    for namespace in fixture.namespaces {
        assert_eq!(
            system_attr_id(&namespace.etype, "id").unwrap(),
            namespace.id_attr
        );
        assert_eq!(
            system_ident_id(&namespace.etype, "id").unwrap(),
            namespace.id_ident
        );
        let root = compiled
            .roots
            .iter()
            .find(|root| root.etype == namespace.etype)
            .unwrap();
        assert!(!root.missing, "{} must be queryable", namespace.etype);
        let labels = root
            .fetch_attr_ids
            .iter()
            .map(|id| catalog.by_id(*id).unwrap().label.clone())
            .collect::<Vec<_>>();
        assert_eq!(labels, namespace.default_fetch, "{}", namespace.etype);
        let schema = catalog
            .attributes
            .iter()
            .filter(|attribute| attribute.etype == namespace.etype)
            .map(|attribute| {
                format!(
                    "{}|{}|{}|{}|{}|{}|{}",
                    attribute.label,
                    format!("{:?}", attribute.value_type).to_lowercase(),
                    format!("{:?}", attribute.cardinality).to_lowercase(),
                    attribute.unique,
                    attribute.indexed,
                    attribute.required,
                    attribute.checked_data_type.map_or_else(
                        || "-".to_owned(),
                        |data_type| format!("{data_type:?}").to_lowercase()
                    )
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(schema, namespace.schema, "{}", namespace.etype);
    }
}

#[test]
#[allow(clippy::too_many_lines)] // Keeps the eight JVM relationship tuples visible as one contract.
fn relationship_identities_and_delete_rules_match_the_jvm_catalog() {
    let specs = system_catalog_attributes().unwrap();
    let expected = [
        (
            "$users",
            "linkedPrimaryUser",
            AttributeValueType::Ref,
            Cardinality::One,
            false,
            "$users",
            "linkedGuestUsers",
            true,
            false,
        ),
        (
            "$userRefreshTokens",
            "$user",
            AttributeValueType::Ref,
            Cardinality::One,
            false,
            "$users",
            "$userRefreshTokens",
            true,
            false,
        ),
        (
            "$oauthUserLinks",
            "$user",
            AttributeValueType::Ref,
            Cardinality::One,
            false,
            "$users",
            "$oauthUserLinks",
            true,
            false,
        ),
        (
            "$oauthUserLinks",
            "$oauthProvider",
            AttributeValueType::Blob,
            Cardinality::One,
            false,
            "$oauthProviders",
            "$oauthUserLinks",
            true,
            false,
        ),
        (
            "$oauthClients",
            "$oauthProvider",
            AttributeValueType::Ref,
            Cardinality::One,
            false,
            "$oauthProviders",
            "$oauthClients",
            true,
            false,
        ),
        (
            "$oauthCodes",
            "$oauthClient",
            AttributeValueType::Ref,
            Cardinality::One,
            false,
            "$oauthClients",
            "$oauthCodes",
            true,
            false,
        ),
        (
            "$oauthRedirects",
            "$oauthClient",
            AttributeValueType::Ref,
            Cardinality::One,
            false,
            "$oauthClients",
            "$oauthRedirects",
            true,
            false,
        ),
        (
            "$streams",
            "$files",
            AttributeValueType::Ref,
            Cardinality::Many,
            true,
            "$files",
            "$stream",
            false,
            true,
        ),
    ];

    for (
        etype,
        label,
        value_type,
        cardinality,
        unique,
        reverse_etype,
        reverse_label,
        cascade,
        cascade_reverse,
    ) in expected
    {
        let spec = specs
            .iter()
            .find(|spec| spec.attribute.etype == etype && spec.attribute.label == label)
            .unwrap();
        assert_eq!(spec.attribute.value_type, value_type, "{etype}.{label}");
        assert_eq!(spec.attribute.cardinality, cardinality, "{etype}.{label}");
        assert_eq!(spec.attribute.unique, unique, "{etype}.{label}");
        let reverse = spec.reverse_identity.as_ref().unwrap();
        assert_eq!(
            (&*reverse.etype, &*reverse.label),
            (reverse_etype, reverse_label)
        );
        assert_eq!(
            reverse.id,
            system_ident_id(reverse_etype, reverse_label).unwrap()
        );
        assert_eq!(spec.on_delete == Some(DeleteAction::Cascade), cascade);
        assert_eq!(
            spec.on_delete_reverse == Some(DeleteAction::Cascade),
            cascade_reverse
        );
        assert!(is_system_catalog_identity(etype, label).unwrap());
        assert!(is_system_catalog_identity(reverse_etype, reverse_label).unwrap());
    }
    assert!(!is_system_catalog_identity("todos", "title").unwrap());
}

#[test]
fn system_uuid_decoding_matches_the_reference_maps() {
    let decoded = decode_system_uuid(system_attr_id("$oauthClients", "clientId").unwrap()).unwrap();
    assert_eq!(decoded.catalog, "system");
    assert_eq!(decoded.etype.as_deref(), Some("$oauthClients"));
    assert_eq!(decoded.label.as_deref(), Some("clientId"));

    let decoded = decode_system_uuid(system_ident_id("$streams", "$files").unwrap()).unwrap();
    assert_eq!(decoded.catalog, "system");
    assert_eq!(decoded.etype.as_deref(), Some("$streams"));
    assert_eq!(
        decoded.label, None,
        "the JVM decoder does not invert entity shortcodes as labels"
    );
}

#[test]
fn system_relationship_where_paths_compile_in_both_directions() {
    let catalog = system_attribute_catalog().unwrap();
    let provider_id = Uuid::new_v4();
    let forward = compile_query(
        &serde_json::json!({
            "$oauthClients": {"$": {"where": {"$oauthProvider.id": provider_id}}}
        }),
        &catalog,
        false,
    )
    .unwrap();
    assert_eq!(
        clause_attribute_ids(&forward.roots[0].selector_clauses),
        vec![
            system_attr_id("$oauthProviders", "id").unwrap(),
            system_attr_id("$oauthClients", "$oauthProvider").unwrap(),
        ]
    );

    let client_id = Uuid::new_v4();
    let reverse = compile_query(
        &serde_json::json!({
            "$oauthProviders": {"$": {"where": {"$oauthClients.id": client_id}}}
        }),
        &catalog,
        false,
    )
    .unwrap();
    assert_eq!(
        clause_attribute_ids(&reverse.roots[0].selector_clauses),
        vec![
            system_attr_id("$oauthClients", "id").unwrap(),
            system_attr_id("$oauthClients", "$oauthProvider").unwrap(),
        ]
    );
}

fn clause_attribute_ids(clauses: &[NormalizedClause]) -> Vec<Uuid> {
    clauses
        .iter()
        .flat_map(|clause| match clause {
            NormalizedClause::Pattern { pattern } => {
                let Term::Constants(values) = &pattern.attribute else {
                    panic!("where-path attributes must compile to constants");
                };
                vec![Uuid::parse_str(values[0].as_str().unwrap()).unwrap()]
            }
            NormalizedClause::And { clauses } | NormalizedClause::Or { clauses, .. } => {
                clause_attribute_ids(clauses)
            }
        })
        .collect()
}
