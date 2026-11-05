use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{BroadcastPayload, PeerUser, PresenceDelivery, PresenceEdit};

const CORE_VERSION: &str = "@instantdb/core";
const PATCH_PRESENCE_MIN: (u64, u64, u64) = (0, 17, 5);
const SKIP_ATTRS_MIN: (u64, u64, u64) = (0, 20, 4);
const BATCH_MESSAGES_MIN: (u64, u64, u64) = (0, 22, 75);

/// Newest core SDK release this server is tested against, as (major, minor,
/// patch).
///
/// Bump this in the same pull request that releases an SDK version; the test
/// below fails if it falls behind the negotiated feature thresholds. See
/// `docs/sdk.md` for the compatibility promise. Sessions from newer versions
/// are still served, but nothing is promised about features they add.
pub const MAX_TESTED_SDK_VERSION: (u64, u64, u64) = (1, 1, 0);

/// Returns whether `version` is newer than [`MAX_TESTED_SDK_VERSION`].
#[must_use]
pub fn is_above_max_tested_sdk_version(version: &Version) -> bool {
    let (major, minor, patch) = MAX_TESTED_SDK_VERSION;
    *version > Version::new(major, minor, patch)
}

/// Fixed SDK family labels. Every init maps to one of these.
pub const SDK_LABELS: [&str; 5] = ["core", "react", "react-native", "admin", "other"];

/// Version label shared by unparseable and overflow versions.
pub const SDK_VERSION_OTHER: &str = "other";

/// Wrappers in the order that a client's `versions` map should be read.
///
/// A React client reports both `@instantdb/react` and its bundled
/// `@instantdb/core`, so the most specific wrapper wins.
const SDK_PACKAGES: [(&str, &str); 4] = [
    ("@instantdb/admin", "admin"),
    ("@instantdb/react-native", "react-native"),
    ("@instantdb/react", "react"),
    (CORE_VERSION, "core"),
];

/// Returns the SDK family and normalized `major.minor.patch` version for one
/// init `versions` map.
#[must_use]
pub fn sdk_session(versions: &BTreeMap<String, String>) -> (&'static str, String) {
    for (package, sdk) in SDK_PACKAGES {
        if let Some(version) = versions.get(package) {
            return (
                sdk,
                normalize_sdk_version(version).unwrap_or_else(|| SDK_VERSION_OTHER.to_owned()),
            );
        }
    }
    ("other", SDK_VERSION_OTHER.to_owned())
}

/// Strips a leading `v`, prerelease and build metadata. Anything that is not
/// semver becomes [`SDK_VERSION_OTHER`] when used as a label.
#[must_use]
pub fn normalize_sdk_version(version: &str) -> Option<String> {
    let version = Version::parse(version.trim().trim_start_matches('v')).ok()?;
    Some(format!(
        "{}.{}.{}",
        version.major, version.minor, version.patch
    ))
}

/// Whether a normalized version is newer than [`MAX_TESTED_SDK_VERSION`].
#[must_use]
pub fn sdk_version_is_newer(version: &str) -> bool {
    Version::parse(version).is_ok_and(|version| is_above_max_tested_sdk_version(&version))
}

/// Backward-compatible capabilities negotiated from the core client version.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ClientFeature {
    PatchPresence,
    SkipAttrs,
    BatchMessages,
}

/// Feature set for one realtime connection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ClientFeatures(BTreeSet<ClientFeature>);

impl ClientFeatures {
    /// Derives features using the JVM server's strict `version > threshold` rules.
    #[must_use]
    pub fn from_versions(versions: &BTreeMap<String, String>) -> Self {
        let Some(version) = versions
            .get(CORE_VERSION)
            .and_then(|version| Version::parse(version.trim_start_matches('v')).ok())
        else {
            return Self::default();
        };
        let mut features = BTreeSet::new();
        for (threshold, feature) in [
            (SKIP_ATTRS_MIN, ClientFeature::SkipAttrs),
            (PATCH_PRESENCE_MIN, ClientFeature::PatchPresence),
            (BATCH_MESSAGES_MIN, ClientFeature::BatchMessages),
        ] {
            if version > Version::new(threshold.0, threshold.1, threshold.2) {
                features.insert(feature);
            }
        }
        Self(features)
    }

    #[must_use]
    pub fn supports(&self, feature: ClientFeature) -> bool {
        self.0.contains(&feature)
    }
}

/// Presence response selected for a client version.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum PresenceMessage {
    PatchPresence {
        #[serde(rename = "room-id")]
        room_id: String,
        edits: Vec<PresenceEdit>,
    },
    RefreshPresence {
        #[serde(rename = "room-id")]
        room_id: String,
        data: Arc<BTreeMap<uuid::Uuid, crate::PeerPresence>>,
    },
}

/// Selects patch or full presence delivery. An empty edit list is suppressed.
#[must_use]
pub fn presence_message(
    features: &ClientFeatures,
    delivery: PresenceDelivery,
) -> Option<PresenceMessage> {
    match delivery.edits {
        Some(edits) if edits.is_empty() => None,
        Some(edits) if features.supports(ClientFeature::PatchPresence) => {
            Some(PresenceMessage::PatchPresence {
                room_id: delivery.room.room_id,
                edits,
            })
        }
        Some(_) | None => Some(PresenceMessage::RefreshPresence {
            room_id: delivery.room.room_id,
            data: delivery.data,
        }),
    }
}

/// Recipient-facing broadcast message.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ServerBroadcast {
    pub room_id: String,
    pub topic: Value,
    pub data: ServerBroadcastData,
}

/// Sender fragment carried by a server broadcast.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ServerBroadcastData {
    pub peer_id: uuid::Uuid,
    pub user: Option<PeerUser>,
    pub data: Value,
}

impl From<BroadcastPayload> for ServerBroadcast {
    fn from(payload: BroadcastPayload) -> Self {
        Self {
            room_id: payload.room_id,
            topic: payload.topic,
            data: ServerBroadcastData {
                peer_id: payload.data.peer_id,
                user: payload.data.user,
                data: payload.data.data,
            },
        }
    }
}

/// One websocket frame after optional broadcast batching.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum BroadcastFrame {
    Single(ServerBroadcast),
    Batch(Vec<ServerBroadcast>),
}

/// Applies the reference server's single-message and 500-item batching contract.
#[must_use]
pub fn broadcast_frames(
    features: &ClientFeatures,
    messages: Vec<ServerBroadcast>,
) -> Vec<BroadcastFrame> {
    if messages.len() <= 1 || !features.supports(ClientFeature::BatchMessages) {
        return messages.into_iter().map(BroadcastFrame::Single).collect();
    }
    messages
        .chunks(500)
        .map(|chunk| BroadcastFrame::Batch(chunk.to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::{
        PeerPresence, PresenceEditOperation, PresencePathSegment, PresenceUpdate, RoomStore,
    };

    fn features(version: &str) -> ClientFeatures {
        ClientFeatures::from_versions(&BTreeMap::from([(
            CORE_VERSION.to_owned(),
            version.to_owned(),
        )]))
    }

    #[test]
    fn feature_thresholds_are_strict_and_accept_v_prefix() {
        assert!(!features("v0.17.5").supports(ClientFeature::PatchPresence));
        assert!(features("v0.17.6").supports(ClientFeature::PatchPresence));
        assert!(!features("0.20.4").supports(ClientFeature::SkipAttrs));
        assert!(features("0.20.5").supports(ClientFeature::SkipAttrs));
        assert!(!features("0.22.75").supports(ClientFeature::BatchMessages));
        assert!(features("0.22.76").supports(ClientFeature::BatchMessages));
        assert_eq!(features("not-semver"), ClientFeatures::default());
    }

    #[test]
    fn sdk_sessions_prefer_the_most_specific_wrapper_and_normalize() {
        assert_eq!(
            sdk_session(&BTreeMap::from([
                (
                    CORE_VERSION.to_owned(),
                    "v1.0.66-alpha.1+build.7".to_owned()
                ),
                ("@instantdb/react".to_owned(), "1.0.66".to_owned()),
            ])),
            ("react", "1.0.66".to_owned())
        );
        assert_eq!(
            sdk_session(&BTreeMap::from([(
                "@instantdb/react-native".to_owned(),
                "1.0.66".to_owned(),
            )])),
            ("react-native", "1.0.66".to_owned())
        );
        assert_eq!(
            sdk_session(&BTreeMap::from([(
                "@instantdb/admin".to_owned(),
                "1.0.66".to_owned(),
            )])),
            ("admin", "1.0.66".to_owned())
        );
        assert_eq!(
            sdk_session(&BTreeMap::from([(
                "@instantdb/core".to_owned(),
                "1.0.66".to_owned()
            )])),
            ("core", "1.0.66".to_owned())
        );
        assert_eq!(
            sdk_session(&BTreeMap::from([(
                "not-semver".to_owned(),
                "1.0.66".to_owned()
            )])),
            ("other", SDK_VERSION_OTHER.to_owned())
        );
        assert_eq!(
            sdk_session(&BTreeMap::from([(
                CORE_VERSION.to_owned(),
                "not-semver".to_owned()
            )])),
            ("core", SDK_VERSION_OTHER.to_owned())
        );
    }

    #[test]
    fn untested_sdk_versions_compare_without_prerelease_or_build_metadata() {
        let (major, minor, patch) = MAX_TESTED_SDK_VERSION;
        let max_tested = format!("{major}.{minor}.{patch}");
        let older_patch = format!("{major}.{minor}.{}", patch.saturating_sub(1));
        let next_patch = format!("{major}.{minor}.{}", patch + 1);
        assert_eq!(
            normalize_sdk_version(&format!("v{max_tested}-rc.1+build")),
            Some(max_tested.clone())
        );
        assert_eq!(normalize_sdk_version(" 0.20.5 "), Some("0.20.5".to_owned()));
        assert_eq!(normalize_sdk_version("latest"), None);
        assert!(!sdk_version_is_newer(&max_tested));
        assert!(!sdk_version_is_newer(&older_patch));
        assert!(sdk_version_is_newer(&next_patch));
        assert!(sdk_version_is_newer(&format!("{}.0.0", major + 1)));
        assert!(!sdk_version_is_newer("not-semver"));
    }

    #[test]
    fn presence_uses_patch_only_for_supported_existing_peers() {
        let app = Uuid::new_v4();
        let session = Uuid::new_v4();
        let mut rooms = RoomStore::default();
        let PresenceUpdate { deliveries } = rooms
            .join(app, "room", session, "dev", None, Value::Null)
            .unwrap();
        assert!(matches!(
            presence_message(&features("0.17.6"), deliveries[0].clone()),
            Some(PresenceMessage::RefreshPresence { .. })
        ));
        let update = rooms
            .set_presence(app, "room", session, serde_json::json!({"x": 1}))
            .unwrap();
        assert!(matches!(
            presence_message(&features("0.17.6"), update.deliveries[0].clone()),
            Some(PresenceMessage::PatchPresence { .. })
        ));
        assert!(matches!(
            presence_message(&features("0.17.5"), update.deliveries[0].clone()),
            Some(PresenceMessage::RefreshPresence { .. })
        ));

        let noop = PresenceDelivery {
            session_id: session,
            room: crate::RoomKey::new(app, "room"),
            data: Arc::new(BTreeMap::<Uuid, PeerPresence>::new()),
            edits: Some(Vec::new()),
        };
        assert!(presence_message(&features("1.0.0"), noop).is_none());
    }

    #[test]
    fn max_tested_sdk_version_covers_every_negotiated_feature() {
        let tested = Version::new(
            MAX_TESTED_SDK_VERSION.0,
            MAX_TESTED_SDK_VERSION.1,
            MAX_TESTED_SDK_VERSION.2,
        );
        for threshold in [PATCH_PRESENCE_MIN, SKIP_ATTRS_MIN, BATCH_MESSAGES_MIN] {
            assert!(tested > Version::new(threshold.0, threshold.1, threshold.2));
        }
        assert!(!is_above_max_tested_sdk_version(&tested));
        assert!(!is_above_max_tested_sdk_version(&Version::new(0, 22, 76)));
        assert!(is_above_max_tested_sdk_version(&Version::new(
            MAX_TESTED_SDK_VERSION.0,
            MAX_TESTED_SDK_VERSION.1,
            MAX_TESTED_SDK_VERSION.2 + 1,
        )));
        assert!(is_above_max_tested_sdk_version(&Version::new(
            MAX_TESTED_SDK_VERSION.0 + 1,
            0,
            0,
        )));
    }

    #[test]
    fn presence_messages_use_exact_mutually_exclusive_wire_fields() {
        let peer_id = Uuid::nil();
        let peer = PeerPresence {
            peer_id,
            instance_id: "device".to_owned(),
            user: None,
            data: serde_json::json!({"online": true}),
        };
        let refresh = serde_json::to_value(PresenceMessage::RefreshPresence {
            room_id: "lobby".to_owned(),
            data: Arc::new(BTreeMap::from([(peer_id, peer)])),
        })
        .unwrap();
        assert_eq!(
            refresh,
            serde_json::json!({
                "op": "refresh-presence",
                "room-id": "lobby",
                "data": {
                    peer_id.to_string(): {
                        "peer-id": peer_id,
                        "instance-id": "device",
                        "user": null,
                        "data": {"online": true}
                    }
                }
            })
        );
        assert!(refresh.get("edits").is_none());
        assert!(refresh.get("room_id").is_none());

        let patch = serde_json::to_value(PresenceMessage::PatchPresence {
            room_id: "lobby".to_owned(),
            edits: vec![PresenceEdit {
                path: vec![
                    PresencePathSegment::Session(peer_id),
                    PresencePathSegment::Key("data".to_owned()),
                ],
                operation: PresenceEditOperation::Replace,
                value: Some(serde_json::json!({"online": false})),
            }],
        })
        .unwrap();
        assert_eq!(
            patch,
            serde_json::json!({
                "op": "patch-presence",
                "room-id": "lobby",
                "edits": [[[peer_id, "data"], "r", {"online": false}]]
            })
        );
        assert!(patch.get("data").is_none());
        assert!(patch.get("room_id").is_none());
    }

    #[test]
    fn broadcasts_batch_at_five_hundred_only_for_new_clients() {
        let messages = (0..1_001)
            .map(|index| ServerBroadcast {
                room_id: "room".into(),
                topic: Value::String("topic".into()),
                data: ServerBroadcastData {
                    peer_id: Uuid::nil(),
                    user: None,
                    data: Value::from(index),
                },
            })
            .collect::<Vec<_>>();
        assert_eq!(
            broadcast_frames(&features("0.22.75"), messages.clone()).len(),
            1_001
        );
        let frames = broadcast_frames(&features("0.22.76"), messages);
        assert_eq!(frames.len(), 3);
        assert!(matches!(&frames[0], BroadcastFrame::Batch(items) if items.len() == 500));
        assert!(matches!(&frames[1], BroadcastFrame::Batch(items) if items.len() == 500));
        assert!(matches!(&frames[2], BroadcastFrame::Batch(items) if items.len() == 1));
    }
}
