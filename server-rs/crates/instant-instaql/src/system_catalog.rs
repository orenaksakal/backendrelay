use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{Attribute, AttributeCatalog, AttributeValueType, AttributeWireMetadata, Cardinality};
use instant_datalog::DataType;

pub const SYSTEM_CATALOG_APP_ID: Uuid = Uuid::from_u128(0xa111_1111_1111_1111_1111_1111_1111_1ca7);
pub const SYSTEM_CATALOG_USER_ID: Uuid = Uuid::from_u128(0xe111_1111_1111_1111_1111_1111_1111_1ca7);
pub const FILES_ETYPE: &str = "$files";
pub const SYSTEM_ETYPES: [&str; 10] = [
    "$users",
    "$magicCodes",
    "$userRefreshTokens",
    "$oauthProviders",
    "$oauthUserLinks",
    "$oauthClients",
    "$oauthCodes",
    "$oauthRedirects",
    FILES_ETYPE,
    "$streams",
];

const NAME_CHARS: &str = "abcdefghijklmnopqrstuvwxzy/";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemCatalogIdKind {
    Attribute,
    Identity,
}

impl SystemCatalogIdKind {
    const fn shortcode(self) -> &'static str {
        match self {
            Self::Attribute => "at",
            Self::Identity => "id",
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SystemCatalogError {
    #[error("unknown system entity type {0}")]
    UnknownEntityType(String),
    #[error("unknown system attribute label {0}")]
    UnknownLabel(String),
    #[error("encoded system catalog component is longer than 12 characters: {0}")]
    ComponentTooLong(String),
    #[error("encoded system catalog component contains unsupported character {character}: {value}")]
    UnsupportedCharacter { value: String, character: char },
    #[error("invalid encoded system catalog UUID {0}")]
    InvalidUuid(Uuid),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteAction {
    Cascade,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SystemIdentity {
    pub id: Uuid,
    pub etype: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SystemCatalogAttribute {
    pub attribute: Attribute,
    pub forward_identity: SystemIdentity,
    pub reverse_identity: Option<SystemIdentity>,
    pub on_delete: Option<DeleteAction>,
    pub on_delete_reverse: Option<DeleteAction>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecodedSystemCatalogId {
    pub catalog: String,
    pub kind: Option<SystemCatalogIdKind>,
    pub etype: Option<String>,
    pub label: Option<String>,
}

/// Generates the deterministic UUID used by the JVM system catalog.
///
/// # Errors
///
/// Returns [`SystemCatalogError`] when the entity type/label lacks a shortcode
/// or an encoded component violates the reference bit-packing alphabet.
pub fn system_catalog_id(
    kind: SystemCatalogIdKind,
    etype: &str,
    label: &str,
) -> Result<Uuid, SystemCatalogError> {
    let etype = etype_shortcode(etype)
        .ok_or_else(|| SystemCatalogError::UnknownEntityType(etype.to_owned()))?;
    let label = etype_shortcode(label)
        .or_else(|| label_shortcode(label))
        .ok_or_else(|| SystemCatalogError::UnknownLabel(label.to_owned()))?;
    let high = encode_component(&format!("system{}", kind.shortcode()))?;
    let low = encode_component(&format!("{etype}/{label}"))?;
    Ok(Uuid::from_u64_pair(high, low))
}

/// Returns the stable attribute UUID for a system namespace field.
///
/// # Errors
///
/// Returns [`SystemCatalogError`] for unknown system names.
pub fn system_attr_id(etype: &str, label: &str) -> Result<Uuid, SystemCatalogError> {
    system_catalog_id(SystemCatalogIdKind::Attribute, etype, label)
}

/// Returns the stable identity UUID for a system namespace field.
///
/// # Errors
///
/// Returns [`SystemCatalogError`] for unknown system names.
pub fn system_ident_id(etype: &str, label: &str) -> Result<Uuid, SystemCatalogError> {
    system_catalog_id(SystemCatalogIdKind::Identity, etype, label)
}

/// Decodes the descriptive components embedded in a JVM system UUID.
///
/// Unknown type/entity/label shortcodes are represented as `None`, matching
/// Clojure map lookup semantics. Labels encoded through an entity-type
/// shortcode retain the reference decoder's `None` label quirk.
///
/// # Errors
///
/// Returns [`SystemCatalogError::InvalidUuid`] when the packed strings do not
/// have the expected type suffix and entity/label separator shape.
pub fn decode_system_uuid(id: Uuid) -> Result<DecodedSystemCatalogId, SystemCatalogError> {
    let (high, low) = id.as_u64_pair();
    let system = decode_component(high);
    let Some(catalog_length) = system.len().checked_sub(2) else {
        return Err(SystemCatalogError::InvalidUuid(id));
    };
    let (catalog, kind) = system.split_at(catalog_length);
    let kind = match kind {
        "at" => Some(SystemCatalogIdKind::Attribute),
        "id" => Some(SystemCatalogIdKind::Identity),
        _ => None,
    };
    let encoded_name = decode_component(low);
    let Some((etype, label)) = encoded_name.split_once('/') else {
        return Err(SystemCatalogError::InvalidUuid(id));
    };
    Ok(DecodedSystemCatalogId {
        catalog: catalog.to_owned(),
        kind,
        etype: etype_from_shortcode(etype).map(str::to_owned),
        label: label_from_shortcode(label).map(str::to_owned),
    })
}

/// Builds every built-in system attribute and relationship lifecycle rule.
///
/// # Errors
///
/// Returns [`SystemCatalogError`] if an internal shortcode is missing.
pub fn system_catalog_attributes() -> Result<Vec<SystemCatalogAttribute>, SystemCatalogError> {
    SYSTEM_ATTRIBUTE_DEFINITIONS
        .iter()
        .map(SystemAttributeDefinition::build)
        .collect()
}

/// Projects the complete system catalog into the query compiler's attribute view.
///
/// # Errors
///
/// Returns [`SystemCatalogError`] if an internal shortcode is missing.
pub fn system_attribute_catalog() -> Result<AttributeCatalog, SystemCatalogError> {
    Ok(AttributeCatalog {
        attributes: system_catalog_attributes()?
            .into_iter()
            .map(|spec| spec.attribute)
            .collect(),
    })
}

/// Builds only the built-in `$files` query attributes.
///
/// # Errors
///
/// Returns [`SystemCatalogError`] if an internal shortcode is missing.
pub fn files_attributes() -> Result<Vec<Attribute>, SystemCatalogError> {
    Ok(system_catalog_attributes()?
        .into_iter()
        .filter(|spec| spec.attribute.etype == FILES_ETYPE)
        .map(|spec| spec.attribute)
        .collect())
}

/// Returns whether an identity name is owned by the built-in system catalog.
///
/// # Errors
///
/// Returns [`SystemCatalogError`] if an internal shortcode is missing.
pub fn is_system_catalog_identity(etype: &str, label: &str) -> Result<bool, SystemCatalogError> {
    Ok(system_catalog_attributes()?.iter().any(|spec| {
        (spec.forward_identity.etype == etype && spec.forward_identity.label == label)
            || spec
                .reverse_identity
                .as_ref()
                .is_some_and(|identity| identity.etype == etype && identity.label == label)
    }))
}

#[derive(Clone, Copy)]
struct SystemAttributeDefinition {
    etype: &'static str,
    label: &'static str,
    unique: bool,
    indexed: bool,
    required: bool,
    checked_data_type: Option<DataType>,
    value_type: AttributeValueType,
    cardinality: Cardinality,
    reverse_identity: Option<(&'static str, &'static str)>,
    on_delete: Option<DeleteAction>,
    on_delete_reverse: Option<DeleteAction>,
}

impl SystemAttributeDefinition {
    const fn new(etype: &'static str, label: &'static str) -> Self {
        Self {
            etype,
            label,
            unique: false,
            indexed: false,
            required: false,
            checked_data_type: None,
            value_type: AttributeValueType::Blob,
            cardinality: Cardinality::One,
            reverse_identity: None,
            on_delete: None,
            on_delete_reverse: None,
        }
    }

    const fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    const fn indexed(mut self) -> Self {
        self.indexed = true;
        self
    }

    const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    const fn checked(mut self, data_type: DataType) -> Self {
        self.checked_data_type = Some(data_type);
        self
    }

    const fn reference(mut self) -> Self {
        self.value_type = AttributeValueType::Ref;
        self
    }

    const fn many(mut self) -> Self {
        self.cardinality = Cardinality::Many;
        self
    }

    const fn reverse(mut self, etype: &'static str, label: &'static str) -> Self {
        self.reverse_identity = Some((etype, label));
        self
    }

    const fn cascade(mut self) -> Self {
        self.on_delete = Some(DeleteAction::Cascade);
        self
    }

    const fn cascade_reverse(mut self) -> Self {
        self.on_delete_reverse = Some(DeleteAction::Cascade);
        self
    }

    fn build(&self) -> Result<SystemCatalogAttribute, SystemCatalogError> {
        let forward_identity = identity(self.etype, self.label)?;
        let reverse_identity = self
            .reverse_identity
            .map(|(etype, label)| identity(etype, label))
            .transpose()?;
        Ok(SystemCatalogAttribute {
            attribute: Attribute {
                id: system_attr_id(self.etype, self.label)?,
                wire: Some(AttributeWireMetadata {
                    forward_identity_id: forward_identity.id,
                    reverse_identity_id: reverse_identity.as_ref().map(|identity| identity.id),
                    inferred_types: None,
                }),
                etype: self.etype.to_owned(),
                label: self.label.to_owned(),
                value_type: self.value_type,
                cardinality: self.cardinality,
                unique: self.unique,
                setting_unique: false,
                indexed: self.indexed,
                required: self.required,
                checked_data_type: self.checked_data_type,
                indexing: false,
                checking_data_type: false,
                reverse_etype: reverse_identity
                    .as_ref()
                    .map(|identity| identity.etype.clone()),
                reverse_label: reverse_identity
                    .as_ref()
                    .map(|identity| identity.label.clone()),
                on_delete: self.on_delete,
                on_delete_reverse: self.on_delete_reverse,
            },
            forward_identity,
            reverse_identity,
            on_delete: self.on_delete,
            on_delete_reverse: self.on_delete_reverse,
        })
    }
}

fn identity(etype: &str, label: &str) -> Result<SystemIdentity, SystemCatalogError> {
    Ok(SystemIdentity {
        id: system_ident_id(etype, label)?,
        etype: etype.to_owned(),
        label: label.to_owned(),
    })
}

const SYSTEM_ATTRIBUTE_DEFINITIONS: &[SystemAttributeDefinition] = &[
    SystemAttributeDefinition::new("$users", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$users", "email")
        .unique()
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$users", "type").checked(DataType::String),
    SystemAttributeDefinition::new("$users", "imageURL").checked(DataType::String),
    SystemAttributeDefinition::new("$users", "linkedPrimaryUser")
        .reference()
        .reverse("$users", "linkedGuestUsers")
        .cascade(),
    SystemAttributeDefinition::new("$magicCodes", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$magicCodes", "codeHash")
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$magicCodes", "email")
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$userRefreshTokens", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$userRefreshTokens", "hashedToken")
        .unique()
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$userRefreshTokens", "$user")
        .indexed()
        .reference()
        .reverse("$users", "$userRefreshTokens")
        .cascade(),
    SystemAttributeDefinition::new("$oauthProviders", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$oauthProviders", "name")
        .unique()
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthUserLinks", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$oauthUserLinks", "sub")
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthUserLinks", "$user")
        .indexed()
        .reference()
        .reverse("$users", "$oauthUserLinks")
        .cascade(),
    SystemAttributeDefinition::new("$oauthUserLinks", "$oauthProvider")
        .indexed()
        .reverse("$oauthProviders", "$oauthUserLinks")
        .cascade(),
    SystemAttributeDefinition::new("$oauthUserLinks", "sub+$oauthProvider")
        .unique()
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthClients", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$oauthClients", "$oauthProvider")
        .reference()
        .reverse("$oauthProviders", "$oauthClients")
        .cascade(),
    SystemAttributeDefinition::new("$oauthClients", "name")
        .unique()
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthClients", "clientId").indexed(),
    SystemAttributeDefinition::new("$oauthClients", "encryptedClientSecret")
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthClients", "extraAudiences"),
    SystemAttributeDefinition::new("$oauthClients", "discoveryEndpoint").checked(DataType::String),
    SystemAttributeDefinition::new("$oauthClients", "meta"),
    SystemAttributeDefinition::new("$oauthClients", "redirectTo").checked(DataType::String),
    SystemAttributeDefinition::new("$oauthClients", "useSharedCredentials")
        .checked(DataType::Boolean),
    SystemAttributeDefinition::new("$oauthCodes", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$oauthCodes", "codeHash")
        .unique()
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthCodes", "codeChallengeMethod").checked(DataType::String),
    SystemAttributeDefinition::new("$oauthCodes", "codeChallenge").checked(DataType::String),
    SystemAttributeDefinition::new("$oauthCodes", "userInfo"),
    SystemAttributeDefinition::new("$oauthCodes", "$oauthClient")
        .reference()
        .reverse("$oauthClients", "$oauthCodes")
        .cascade(),
    SystemAttributeDefinition::new("$oauthRedirects", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$oauthRedirects", "stateHash")
        .unique()
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthRedirects", "cookieHash").checked(DataType::String),
    SystemAttributeDefinition::new("$oauthRedirects", "redirectUrl").checked(DataType::String),
    SystemAttributeDefinition::new("$oauthRedirects", "redirectTo").checked(DataType::String),
    SystemAttributeDefinition::new("$oauthRedirects", "$oauthClient")
        .reference()
        .reverse("$oauthClients", "$oauthRedirects")
        .cascade(),
    SystemAttributeDefinition::new("$oauthRedirects", "codeChallengeMethod")
        .checked(DataType::String),
    SystemAttributeDefinition::new("$oauthRedirects", "codeChallenge").checked(DataType::String),
    SystemAttributeDefinition::new(FILES_ETYPE, "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new(FILES_ETYPE, "path")
        .unique()
        .indexed()
        .required()
        .checked(DataType::String),
    SystemAttributeDefinition::new(FILES_ETYPE, "size")
        .indexed()
        .required()
        .checked(DataType::Number),
    SystemAttributeDefinition::new(FILES_ETYPE, "content-type")
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new(FILES_ETYPE, "content-disposition")
        .indexed()
        .checked(DataType::String),
    SystemAttributeDefinition::new(FILES_ETYPE, "location-id")
        .unique()
        .indexed()
        .required()
        .checked(DataType::String),
    SystemAttributeDefinition::new(FILES_ETYPE, "key-version").checked(DataType::Number),
    SystemAttributeDefinition::new(FILES_ETYPE, "url").checked(DataType::String),
    SystemAttributeDefinition::new("$streams", "id")
        .unique()
        .indexed(),
    SystemAttributeDefinition::new("$streams", "clientId")
        .unique()
        .indexed()
        .required()
        .checked(DataType::String),
    SystemAttributeDefinition::new("$streams", "machineId").checked(DataType::String),
    SystemAttributeDefinition::new("$streams", "$files")
        .unique()
        .reference()
        .many()
        .reverse(FILES_ETYPE, "$stream")
        .cascade_reverse(),
    SystemAttributeDefinition::new("$streams", "done").checked(DataType::Boolean),
    SystemAttributeDefinition::new("$streams", "size").checked(DataType::Number),
    SystemAttributeDefinition::new("$streams", "hashedReconnectToken").checked(DataType::String),
    SystemAttributeDefinition::new("$streams", "abortReason").checked(DataType::String),
];

fn encode_component(value: &str) -> Result<u64, SystemCatalogError> {
    if value.len() >= 13 {
        return Err(SystemCatalogError::ComponentTooLong(value.to_owned()));
    }
    let mut encoded = 0_u64;
    let mut bit_count = 0_u32;
    for character in value.chars() {
        let index =
            NAME_CHARS
                .find(character)
                .ok_or_else(|| SystemCatalogError::UnsupportedCharacter {
                    value: value.to_owned(),
                    character,
                })?;
        encoded = (encoded << 5) | u64::try_from(index).expect("alphabet index fits in five bits");
        bit_count += 5;
    }
    let padding = 64 - bit_count;
    let padding_mask = if padding == 64 {
        u64::MAX
    } else {
        (1_u64 << padding) - 1
    };
    Ok((encoded << padding) | padding_mask)
}

fn decode_component(value: u64) -> String {
    (0..12)
        .filter_map(|group| {
            let shift = 59 - group * 5;
            let index = usize::try_from((value >> shift) & 0x1f).ok()?;
            (index != 31)
                .then(|| NAME_CHARS.chars().nth(index))
                .flatten()
        })
        .collect()
}

fn etype_shortcode(value: &str) -> Option<&'static str> {
    match value {
        "$users" => Some("us"),
        "$magicCodes" => Some("mc"),
        "$userRefreshTokens" => Some("ur"),
        "$oauthProviders" => Some("op"),
        "$oauthUserLinks" => Some("ol"),
        "$oauthClients" => Some("oc"),
        "$oauthCodes" => Some("co"),
        "$oauthRedirects" => Some("or"),
        "$files" => Some("fi"),
        "$streams" => Some("st"),
        _ => None,
    }
}

fn label_shortcode(value: &str) -> Option<&'static str> {
    match value {
        "$files" => Some("$files"),
        "$oauthClient" => Some("oauclient"),
        "$oauthProvider" => Some("oprovider"),
        "$stream" => Some("stream"),
        "$user" => Some("user"),
        "abortReason" => Some("abrtreasn"),
        "authCode" => Some("authcode"),
        "clientId" => Some("clientid"),
        "codeChallenge" => Some("codechall"),
        "codeChallengeMethod" => Some("cchalmeth"),
        "codeHash" => Some("codehash"),
        "content-disposition" => Some("cdisp"),
        "content-type" => Some("ctype"),
        "cookieHash" => Some("cookihash"),
        "discoveryEndpoint" => Some("discovend"),
        "done" => Some("done"),
        "email" => Some("email"),
        "encryptedClientSecret" => Some("encclisec"),
        "extraAudiences" => Some("extraaud"),
        "hashedToken" => Some("hashedtok"),
        "id" => Some("id"),
        "imageURL" => Some("imageurl"),
        "key-version" => Some("kv"),
        "hashedReconnectToken" => Some("hashretok"),
        "linkedGuestUsers" => Some("lgu"),
        "linkedPrimaryUser" => Some("lpu"),
        "location-id" => Some("lid"),
        "machineId" => Some("machineid"),
        "meta" => Some("meta"),
        "name" => Some("name"),
        "path" => Some("path"),
        "redirectUrl" => Some("redireurl"),
        "redirectTo" => Some("redirecto"),
        "size" => Some("size"),
        "stateHash" => Some("statehash"),
        "sub" => Some("sub"),
        "sub+$oauthProvider" => Some("subprovid"),
        "type" => Some("type"),
        "url" => Some("url"),
        "useSharedCredentials" => Some("usesharcr"),
        "userInfo" => Some("usernfo"),
        _ => None,
    }
}

fn etype_from_shortcode(value: &str) -> Option<&'static str> {
    SYSTEM_ETYPES
        .iter()
        .copied()
        .find(|etype| etype_shortcode(etype) == Some(value))
}

fn label_from_shortcode(value: &str) -> Option<&'static str> {
    const LABELS: &[&str] = &[
        "$files",
        "$oauthClient",
        "$oauthProvider",
        "$stream",
        "$user",
        "abortReason",
        "authCode",
        "clientId",
        "codeChallenge",
        "codeChallengeMethod",
        "codeHash",
        "content-disposition",
        "content-type",
        "cookieHash",
        "discoveryEndpoint",
        "done",
        "email",
        "encryptedClientSecret",
        "extraAudiences",
        "hashedToken",
        "id",
        "imageURL",
        "key-version",
        "hashedReconnectToken",
        "linkedGuestUsers",
        "linkedPrimaryUser",
        "location-id",
        "machineId",
        "meta",
        "name",
        "path",
        "redirectUrl",
        "redirectTo",
        "size",
        "stateHash",
        "sub",
        "sub+$oauthProvider",
        "type",
        "url",
        "useSharedCredentials",
        "userInfo",
    ];
    LABELS
        .iter()
        .copied()
        .find(|label| label_shortcode(label) == Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_attribute_ids_match_the_jvm_bit_encoding() {
        let expected = [
            ("id", "96653230-13ff-ffff-2a34-81ffffffffff"),
            ("path", "96653230-13ff-ffff-2a34-f04cffffffff"),
            ("size", "96653230-13ff-ffff-2a35-24609fffffff"),
            ("content-type", "96653230-13ff-ffff-2a34-29e5e4ffffff"),
            (
                "content-disposition",
                "96653230-13ff-ffff-2a34-21a24fffffff",
            ),
            ("location-id", "96653230-13ff-ffff-2a34-b40fffffffff"),
            ("key-version", "96653230-13ff-ffff-2a34-aaffffffffff"),
            ("url", "96653230-13ff-ffff-2a35-48afffffffff"),
        ];
        for (label, expected) in expected {
            assert_eq!(
                system_attr_id(FILES_ETYPE, label).unwrap().to_string(),
                expected
            );
        }
        assert_eq!(files_attributes().unwrap().len(), expected.len());
    }

    #[test]
    fn query_catalog_retains_deterministic_system_identity_ids() {
        for spec in system_catalog_attributes().unwrap() {
            assert_eq!(
                spec.attribute.forward_identity_id(),
                spec.forward_identity.id
            );
            assert_eq!(
                spec.attribute.reverse_identity_id(),
                spec.reverse_identity.as_ref().map(|identity| identity.id)
            );
            assert_eq!(spec.attribute.inferred_types(), None);
        }
    }
}
