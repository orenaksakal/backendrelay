use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use thiserror::Error;
use uuid::Uuid;

/// Cluster-wide identity of an ephemeral room.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RoomKey {
    pub app_id: Uuid,
    pub room_id: String,
}

impl RoomKey {
    #[must_use]
    pub fn new(app_id: Uuid, room_id: impl Into<String>) -> Self {
        Self {
            app_id,
            room_id: room_id.into(),
        }
    }
}

/// Presence data stored for one peer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct PeerPresence {
    pub peer_id: Uuid,
    pub instance_id: String,
    pub user: Option<PeerUser>,
    pub data: Value,
}

/// Public user fragment included in room messages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PeerUser {
    pub id: Uuid,
}

/// One path segment in an EditScript-compatible presence patch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PresencePathSegment {
    Session(Uuid),
    Key(String),
    Index(usize),
}

/// `EditScript` operation names used by current Instant clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PresenceEditOperation {
    #[serde(rename = "+")]
    Add,
    #[serde(rename = "-")]
    Remove,
    #[serde(rename = "r")]
    Replace,
}

/// An EditScript-compatible tuple: `[path, operation]` or `[path, operation, value]`.
#[derive(Clone, Debug, PartialEq)]
pub struct PresenceEdit {
    pub path: Vec<PresencePathSegment>,
    pub operation: PresenceEditOperation,
    pub value: Option<Value>,
}

impl Serialize for PresenceEdit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(usize::from(self.value.is_some()) + 2))?;
        sequence.serialize_element(&self.path)?;
        sequence.serialize_element(&self.operation)?;
        if let Some(value) = &self.value {
            sequence.serialize_element(value)?;
        }
        sequence.end()
    }
}

/// Presence update to deliver to one local client.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PresenceDelivery {
    pub session_id: Uuid,
    pub room: RoomKey,
    pub data: Arc<BTreeMap<Uuid, PeerPresence>>,
    /// `None` forces a full refresh for a newly joined client. Existing clients
    /// receive a patch, including an empty patch for a semantic no-op.
    pub edits: Option<Vec<PresenceEdit>>,
}

/// Result of applying one atomic room-map mutation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PresenceUpdate {
    pub deliveries: Vec<PresenceDelivery>,
}

/// Recipient split and payload for a client broadcast.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BroadcastPlan {
    pub local_session_ids: Vec<Uuid>,
    pub remote_session_ids: Vec<Uuid>,
    pub sender_ack: BroadcastPayload,
    pub recipient_payload: BroadcastPayload,
}

/// Data shared by broadcast acknowledgement and recipient messages.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BroadcastPayload {
    pub room_id: String,
    pub topic: Value,
    pub data: BroadcastData,
}

/// Sender identity and arbitrary client data carried in a broadcast.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct BroadcastData {
    pub peer_id: Uuid,
    pub user: Option<PeerUser>,
    pub data: Value,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RoomError {
    #[error("room id must be a non-empty string")]
    InvalidRoomId,
    #[error("session {session_id} has not entered room {room_id}")]
    NotInRoom { session_id: Uuid, room_id: String },
}

/// Local projection of cluster room state and local room subscriptions.
///
/// The mutation methods mirror the commutative Hazelcast entry processors used by
/// the JVM server. A production transport can replicate the same actions while
/// retaining this deterministic delivery logic.
#[derive(Debug, Default)]
pub struct RoomStore {
    // Deliveries retain immutable snapshots. Local mutations copy only when an
    // older snapshot is still held by a slow reader or an outbound queue.
    rooms: BTreeMap<RoomKey, Arc<BTreeMap<Uuid, PeerPresence>>>,
    room_revisions: BTreeMap<RoomKey, i64>,
    local_sessions_by_room: BTreeMap<RoomKey, BTreeSet<Uuid>>,
    rooms_by_local_session: BTreeMap<Uuid, BTreeSet<RoomKey>>,
}

impl RoomStore {
    /// Registers local delivery interest without mutating the cluster snapshot.
    /// Distributed coordinators use this before applying their authoritative
    /// post-join snapshot.
    pub fn register_local_session(&mut self, room: RoomKey, session_id: Uuid) {
        self.local_sessions_by_room
            .entry(room.clone())
            .or_default()
            .insert(session_id);
        self.rooms_by_local_session
            .entry(session_id)
            .or_default()
            .insert(room);
    }

    /// Removes local delivery interest without changing the cluster snapshot.
    pub fn unregister_local_session(&mut self, room: &RoomKey, session_id: Uuid) {
        if let Some(sessions) = self.local_sessions_by_room.get_mut(room) {
            sessions.remove(&session_id);
            if sessions.is_empty() {
                self.local_sessions_by_room.remove(room);
            }
        }
        if let Some(rooms) = self.rooms_by_local_session.get_mut(&session_id) {
            rooms.remove(room);
            if rooms.is_empty() {
                self.rooms_by_local_session.remove(&session_id);
            }
        }
    }

    #[must_use]
    pub fn local_rooms_for_session(&self, session_id: Uuid) -> Vec<RoomKey> {
        self.rooms_by_local_session
            .get(&session_id)
            .into_iter()
            .flat_map(BTreeSet::iter)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn local_session_ids(&self, room: &RoomKey) -> Vec<Uuid> {
        self.local_sessions_by_room
            .get(room)
            .into_iter()
            .flat_map(BTreeSet::iter)
            .copied()
            .collect()
    }

    /// Joins a room. Repeated joins update peer metadata but preserve existing data.
    ///
    /// # Errors
    ///
    /// Room identifiers are opaque strings; the empty string is valid.
    pub fn join(
        &mut self,
        app_id: Uuid,
        room_id: &str,
        session_id: Uuid,
        instance_id: impl Into<String>,
        user_id: Option<Uuid>,
        initial_data: Value,
    ) -> Result<PresenceUpdate, RoomError> {
        let key = valid_key(app_id, room_id)?;
        self.local_sessions_by_room
            .entry(key.clone())
            .or_default()
            .insert(session_id);
        self.rooms_by_local_session
            .entry(session_id)
            .or_default()
            .insert(key.clone());

        let had_room = self.rooms.contains_key(&key);
        let peers = Arc::make_mut(self.rooms.entry(key.clone()).or_default());
        let old_data = peers
            .get(&session_id)
            .map(|peer| peer.data.clone())
            .filter(clojure_truthy);
        let before = peers.insert(
            session_id,
            PeerPresence {
                peer_id: session_id,
                instance_id: instance_id.into(),
                user: user_id.map(|id| PeerUser { id }),
                data: old_data.unwrap_or_else(|| default_presence_data(initial_data)),
            },
        );
        Ok(self.peer_presence_update(&key, session_id, before.as_ref(), had_room))
    }

    /// Replaces a peer's presence data without deep-merging it.
    ///
    /// # Errors
    ///
    /// Returns [`RoomError::NotInRoom`] unless the session has locally joined.
    pub fn set_presence(
        &mut self,
        app_id: Uuid,
        room_id: &str,
        session_id: Uuid,
        data: Value,
    ) -> Result<PresenceUpdate, RoomError> {
        let key = valid_key(app_id, room_id)?;
        self.ensure_local_member(&key, session_id)?;
        let had_room = self.rooms.contains_key(&key);
        let before = self
            .rooms
            .get(&key)
            .and_then(|peers| peers.get(&session_id))
            .cloned();
        if before.as_ref().is_some_and(|peer| peer.data != data)
            && let Some(peer) = self
                .rooms
                .get_mut(&key)
                .and_then(|peers| Arc::make_mut(peers).get_mut(&session_id))
        {
            peer.data = data;
        }
        Ok(self.peer_presence_update(&key, session_id, before.as_ref(), had_room))
    }

    /// Leaves a room. The operation is idempotent, matching the reference handler.
    ///
    /// # Errors
    ///
    /// Room identifiers are opaque strings; the empty string is valid.
    pub fn leave(
        &mut self,
        app_id: Uuid,
        room_id: &str,
        session_id: Uuid,
    ) -> Result<PresenceUpdate, RoomError> {
        let key = valid_key(app_id, room_id)?;
        if let Some(sessions) = self.local_sessions_by_room.get_mut(&key) {
            sessions.remove(&session_id);
            if sessions.is_empty() {
                self.local_sessions_by_room.remove(&key);
            }
        }
        if let Some(rooms) = self.rooms_by_local_session.get_mut(&session_id) {
            rooms.remove(&key);
            if rooms.is_empty() {
                self.rooms_by_local_session.remove(&session_id);
            }
        }
        let had_room = self.rooms.contains_key(&key);
        let mut before = None;
        if let Some(peers) = self.rooms.get_mut(&key) {
            if peers.contains_key(&session_id) {
                before = Arc::make_mut(peers).remove(&session_id);
            }
            if peers.is_empty() {
                self.rooms.remove(&key);
            }
        }
        Ok(self.peer_presence_update(&key, session_id, before.as_ref(), had_room))
    }

    /// Removes a disconnected session from every room it follows.
    pub fn leave_all(&mut self, session_id: Uuid) -> Vec<PresenceUpdate> {
        let rooms = self
            .rooms_by_local_session
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        rooms
            .into_iter()
            .filter_map(|room| self.leave(room.app_id, &room.room_id, session_id).ok())
            .collect()
    }

    #[must_use]
    pub fn in_room(&self, app_id: Uuid, room_id: &str, session_id: Uuid) -> bool {
        self.local_sessions_by_room
            .get(&RoomKey::new(app_id, room_id))
            .is_some_and(|sessions| sessions.contains(&session_id))
    }

    #[must_use]
    pub fn room_data(&self, app_id: Uuid, room_id: &str) -> Option<&BTreeMap<Uuid, PeerPresence>> {
        self.rooms
            .get(&RoomKey::new(app_id, room_id))
            .map(AsRef::as_ref)
    }

    /// Builds recipient routing without mutating presence state.
    ///
    /// # Errors
    ///
    /// Returns [`RoomError::NotInRoom`] unless the sender is a local member.
    pub fn broadcast(
        &self,
        app_id: Uuid,
        room_id: &str,
        sender_id: Uuid,
        user_id: Option<Uuid>,
        topic: Value,
        data: Value,
    ) -> Result<BroadcastPlan, RoomError> {
        let key = valid_key(app_id, room_id)?;
        self.ensure_local_member(&key, sender_id)?;
        let local = self.local_sessions_by_room.get(&key);
        let mut local_session_ids = Vec::new();
        let mut remote_session_ids = Vec::new();
        for session_id in self
            .rooms
            .get(&key)
            .into_iter()
            .flat_map(|peers| peers.keys())
            .copied()
            .filter(|id| *id != sender_id)
        {
            if local.is_some_and(|sessions| sessions.contains(&session_id)) {
                local_session_ids.push(session_id);
            } else {
                remote_session_ids.push(session_id);
            }
        }
        let payload = BroadcastPayload {
            room_id: room_id.to_owned(),
            topic,
            data: BroadcastData {
                peer_id: sender_id,
                user: user_id.map(|id| PeerUser { id }),
                data,
            },
        };
        Ok(BroadcastPlan {
            local_session_ids,
            remote_session_ids,
            sender_ack: payload.clone(),
            recipient_payload: payload,
        })
    }

    /// Applies a cluster room snapshot and computes updates for local subscribers.
    pub fn apply_remote_snapshot(
        &mut self,
        room: &RoomKey,
        peers: BTreeMap<Uuid, PeerPresence>,
    ) -> PresenceUpdate {
        let before = self.rooms.insert(room.clone(), Arc::new(peers));
        if self.rooms.get(room).is_some_and(|peers| peers.is_empty()) {
            self.rooms.remove(room);
        }
        self.presence_update(room, before.as_deref())
    }

    /// Applies an authoritative cluster snapshot unless a newer revision has
    /// already been observed for the room.
    pub fn apply_versioned_remote_snapshot(
        &mut self,
        room: &RoomKey,
        revision: i64,
        peers: BTreeMap<Uuid, PeerPresence>,
    ) -> PresenceUpdate {
        if self
            .room_revisions
            .get(room)
            .is_some_and(|current| revision <= *current)
        {
            return PresenceUpdate::default();
        }
        self.room_revisions.insert(room.clone(), revision);
        self.apply_remote_snapshot(room, peers)
    }

    fn ensure_local_member(&self, key: &RoomKey, session_id: Uuid) -> Result<(), RoomError> {
        if self
            .local_sessions_by_room
            .get(key)
            .is_some_and(|sessions| sessions.contains(&session_id))
        {
            Ok(())
        } else {
            Err(RoomError::NotInRoom {
                session_id,
                room_id: key.room_id.clone(),
            })
        }
    }

    fn presence_update(
        &self,
        key: &RoomKey,
        before: Option<&BTreeMap<Uuid, PeerPresence>>,
    ) -> PresenceUpdate {
        // Remote room state still advances, but this machine has nobody to
        // notify. Avoid diffing/serializing every peer for an empty recipient set.
        if !self.local_sessions_by_room.contains_key(key) {
            return PresenceUpdate::default();
        }
        let after = self.rooms.get(key).cloned().unwrap_or_default();
        let edits = before.map(|before| presence_diff(before, &after));
        self.presence_deliveries(key, &after, edits.as_deref(), |session_id| {
            before.is_some_and(|old| !old.contains_key(&session_id))
        })
    }

    fn peer_presence_update(
        &self,
        key: &RoomKey,
        session_id: Uuid,
        before: Option<&PeerPresence>,
        had_room: bool,
    ) -> PresenceUpdate {
        if !self.local_sessions_by_room.contains_key(key) {
            return PresenceUpdate::default();
        }
        let after = self.rooms.get(key).cloned().unwrap_or_default();
        // A local mutation can affect only this peer. Do not compare every
        // other peer's potentially large JSON just to construct one patch.
        let edits = had_room.then(|| {
            let mut edits = Vec::new();
            diff_peer(session_id, before, after.get(&session_id), &mut edits);
            edits
        });
        self.presence_deliveries(key, &after, edits.as_deref(), |id| {
            id == session_id && before.is_none()
        })
    }

    fn presence_deliveries(
        &self,
        key: &RoomKey,
        after: &Arc<BTreeMap<Uuid, PeerPresence>>,
        edits: Option<&[PresenceEdit]>,
        full_refresh: impl Fn(Uuid) -> bool,
    ) -> PresenceUpdate {
        PresenceUpdate {
            deliveries: self
                .local_sessions_by_room
                .get(key)
                .into_iter()
                .flat_map(BTreeSet::iter)
                .filter(|session_id| after.contains_key(session_id))
                .map(|session_id| PresenceDelivery {
                    session_id: *session_id,
                    room: key.clone(),
                    data: Arc::clone(after),
                    edits: if full_refresh(*session_id) {
                        None
                    } else {
                        edits.map(<[PresenceEdit]>::to_vec)
                    },
                })
                .collect(),
        }
    }
}

// Keep the fallible shape so RoomStore's public Result-returning API remains
// source-compatible even though every string is currently a valid room id.
#[allow(clippy::unnecessary_wraps)]
fn valid_key(app_id: Uuid, room_id: &str) -> Result<RoomKey, RoomError> {
    Ok(RoomKey::new(app_id, room_id))
}

fn default_presence_data(data: Value) -> Value {
    if clojure_truthy(&data) {
        data
    } else {
        Value::Object(Map::new())
    }
}

fn clojure_truthy(data: &Value) -> bool {
    !data.is_null() && data.as_bool() != Some(false)
}

fn presence_diff(
    before: &BTreeMap<Uuid, PeerPresence>,
    after: &BTreeMap<Uuid, PeerPresence>,
) -> Vec<PresenceEdit> {
    let mut edits = Vec::new();
    for session_id in before.keys().filter(|id| !after.contains_key(id)) {
        edits.push(PresenceEdit {
            path: vec![PresencePathSegment::Session(*session_id)],
            operation: PresenceEditOperation::Remove,
            value: None,
        });
    }
    for (session_id, peer) in after {
        diff_peer(*session_id, before.get(session_id), Some(peer), &mut edits);
    }
    edits
}

fn diff_peer(
    session_id: Uuid,
    before: Option<&PeerPresence>,
    after: Option<&PeerPresence>,
    edits: &mut Vec<PresenceEdit>,
) {
    match (before, after) {
        (Some(_), None) => edits.push(PresenceEdit {
            path: vec![PresencePathSegment::Session(session_id)],
            operation: PresenceEditOperation::Remove,
            value: None,
        }),
        (None, Some(peer)) => edits.push(PresenceEdit {
            path: vec![PresencePathSegment::Session(session_id)],
            operation: PresenceEditOperation::Add,
            value: Some(serde_json::to_value(peer).expect("presence values serialize")),
        }),
        (Some(old_peer), Some(peer)) if old_peer != peer => {
            let mut path = vec![
                PresencePathSegment::Session(session_id),
                PresencePathSegment::Key("data".to_owned()),
            ];
            diff_value(&old_peer.data, &peer.data, &mut path, edits);
        }
        _ => {}
    }
}

fn diff_value(
    before: &Value,
    after: &Value,
    path: &mut Vec<PresencePathSegment>,
    edits: &mut Vec<PresenceEdit>,
) {
    if before == after {
        return;
    }
    if let (Value::Object(before), Value::Object(after)) = (before, after) {
        if before.is_empty() && !after.is_empty() {
            edits.push(PresenceEdit {
                path: path.clone(),
                operation: PresenceEditOperation::Replace,
                value: Some(Value::Object(after.clone())),
            });
            return;
        }
        for key in before.keys().filter(|key| !after.contains_key(*key)) {
            path.push(PresencePathSegment::Key(key.clone()));
            edits.push(PresenceEdit {
                path: path.clone(),
                operation: PresenceEditOperation::Remove,
                value: None,
            });
            path.pop();
        }
        for (key, value) in before {
            if let Some(new_value) = after.get(key) {
                path.push(PresencePathSegment::Key(key.clone()));
                diff_value(value, new_value, path, edits);
                path.pop();
            }
        }
        for (key, value) in after.iter().filter(|(key, _)| !before.contains_key(*key)) {
            path.push(PresencePathSegment::Key(key.clone()));
            edits.push(PresenceEdit {
                path: path.clone(),
                operation: PresenceEditOperation::Add,
                value: Some(value.clone()),
            });
            path.pop();
        }
        return;
    }
    edits.push(PresenceEdit {
        path: path.clone(),
        operation: PresenceEditOperation::Replace,
        value: Some(after.clone()),
    });
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn remote_state_without_local_subscribers_is_retained_for_a_future_join() {
        let app = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let local = Uuid::new_v4();
        let room = RoomKey::new(app, "room");
        let mut rooms = RoomStore::default();
        let peer = PeerPresence {
            peer_id: remote,
            instance_id: "remote".into(),
            user: None,
            data: json!({"online": true}),
        };
        assert!(
            rooms
                .apply_versioned_remote_snapshot(
                    &room,
                    10,
                    BTreeMap::from([(remote, peer.clone())])
                )
                .deliveries
                .is_empty()
        );
        assert_eq!(rooms.room_data(app, "room").unwrap()[&remote], peer);
        let joined = rooms
            .join(app, "room", local, "local", None, json!({}))
            .unwrap();
        assert_eq!(joined.deliveries.len(), 1);
        assert!(joined.deliveries[0].edits.is_none());
        assert_eq!(joined.deliveries[0].data.len(), 2);
        assert_eq!(joined.deliveries[0].data[&remote], peer);
    }

    #[test]
    fn retained_presence_snapshots_are_immutable_and_noops_reuse_the_snapshot() {
        let app = Uuid::new_v4();
        let session = Uuid::new_v4();
        let other = Uuid::new_v4();
        let mut rooms = RoomStore::default();
        let joined = rooms
            .join(app, "room", session, "node", None, json!({"x": 1}))
            .unwrap();
        let original = Arc::clone(&joined.deliveries[0].data);
        let noop = rooms
            .set_presence(app, "room", session, json!({"x": 1}))
            .unwrap();
        assert!(Arc::ptr_eq(&original, &noop.deliveries[0].data));
        assert_eq!(noop.deliveries[0].edits, Some(vec![]));

        let changed = rooms
            .set_presence(app, "room", session, json!({"x": 2}))
            .unwrap();
        assert_eq!(original[&session].data, json!({"x": 1}));
        assert_eq!(changed.deliveries[0].data[&session].data, json!({"x": 2}));
        assert!(!Arc::ptr_eq(&original, &changed.deliveries[0].data));
        rooms
            .join(app, "room", other, "node", None, json!({}))
            .unwrap();
        assert!(!changed.deliveries[0].data.contains_key(&other));
        let before_leave = rooms.rooms[&RoomKey::new(app, "room")].clone();
        rooms.leave(app, "room", session).unwrap();
        assert!(before_leave.contains_key(&session));
        assert!(!rooms.room_data(app, "room").unwrap().contains_key(&session));
    }

    #[test]
    fn local_peer_patches_match_full_snapshot_diffs() {
        let app = Uuid::new_v4();
        let room = RoomKey::new(app, "room");
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut local = RoomStore::default();
        let mut remote = RoomStore::default();
        for (step, data) in [
            Value::Null,
            json!({}),
            json!({"nested": {"x": 1}, "a": [1, 2]}),
            json!({"nested": {"y": true}}),
            json!(false),
            json!([3, 4]),
        ]
        .into_iter()
        .enumerate()
        {
            let update = if step == 0 {
                remote.register_local_session(room.clone(), first);
                local.join(app, "room", first, "node", None, data).unwrap()
            } else {
                local.set_presence(app, "room", first, data).unwrap()
            };
            let expected =
                remote.apply_remote_snapshot(&room, local.room_data(app, "room").unwrap().clone());
            assert_eq!(update, expected);
        }
        remote.register_local_session(room.clone(), second);
        let update = local
            .join(app, "room", second, "node", None, json!({"online": true}))
            .unwrap();
        assert_eq!(
            update,
            remote.apply_remote_snapshot(&room, local.room_data(app, "room").unwrap().clone())
        );
        let update = local
            .join(
                app,
                "room",
                first,
                "other-node",
                Some(Uuid::new_v4()),
                json!({}),
            )
            .unwrap();
        assert_eq!(
            update,
            remote.apply_remote_snapshot(&room, local.room_data(app, "room").unwrap().clone())
        );
        remote.unregister_local_session(&room, first);
        let update = local.leave(app, "room", first).unwrap();
        assert_eq!(
            update,
            remote.apply_remote_snapshot(&room, local.room_data(app, "room").unwrap().clone())
        );
        let update = local.leave(app, "room", first).unwrap();
        assert_eq!(
            update,
            remote.apply_remote_snapshot(&room, local.room_data(app, "room").unwrap().clone())
        );
    }

    #[test]
    fn ten_thousand_peers_share_one_snapshot_and_keep_patches_and_tenants_isolated() {
        let app = Uuid::new_v4();
        let room = RoomKey::new(app, "room");
        let mut rooms = RoomStore::default();
        let peers = (0..10_000)
            .map(|index| {
                let id = Uuid::from_u128(index + 1);
                rooms.register_local_session(room.clone(), id);
                (
                    id,
                    PeerPresence {
                        peer_id: id,
                        instance_id: "node".into(),
                        user: None,
                        data: json!({"v": 0}),
                    },
                )
            })
            .collect();
        let initial = rooms.apply_versioned_remote_snapshot(&room, 1, peers);
        assert_eq!(initial.deliveries.len(), 10_000);
        let snapshot = Arc::clone(&initial.deliveries[0].data);
        assert!(
            initial
                .deliveries
                .iter()
                .all(|delivery| Arc::ptr_eq(&snapshot, &delivery.data))
        );
        let changed = rooms
            .set_presence(app, "room", Uuid::from_u128(1), json!({"v": 1}))
            .unwrap();
        assert_eq!(changed.deliveries.len(), 10_000);
        let after = &changed.deliveries[0].data;
        assert!(changed.deliveries.iter().all(|delivery| {
            Arc::ptr_eq(after, &delivery.data)
                && delivery
                    .edits
                    .as_ref()
                    .is_some_and(|edits| edits.len() == 1)
        }));
        assert_eq!(snapshot[&Uuid::from_u128(1)].data, json!({"v": 0}));
        assert!(rooms.room_data(Uuid::new_v4(), "room").is_none());
        assert!(
            rooms
                .apply_versioned_remote_snapshot(&room, 1, BTreeMap::new())
                .deliveries
                .is_empty()
        );
        assert_eq!(
            rooms.room_data(app, "room").unwrap()[&Uuid::from_u128(1)].data,
            json!({"v": 1})
        );
    }

    #[test]
    fn joining_defaults_data_and_rejoining_preserves_presence() {
        let app = Uuid::new_v4();
        let session = Uuid::new_v4();
        let mut rooms = RoomStore::default();
        let update = rooms
            .join(app, "lobby", session, "dev", None, Value::Null)
            .unwrap();
        assert_eq!(update.deliveries.len(), 1);
        assert_eq!(update.deliveries[0].edits, None);
        assert_eq!(
            rooms.room_data(app, "lobby").unwrap()[&session].data,
            json!({})
        );
        rooms
            .set_presence(app, "lobby", session, json!({"cursor": 10}))
            .unwrap();
        rooms
            .join(
                app,
                "lobby",
                session,
                "new-node",
                None,
                json!({"ignored": true}),
            )
            .unwrap();
        let peer = &rooms.room_data(app, "lobby").unwrap()[&session];
        assert_eq!(peer.data, json!({"cursor": 10}));
        assert_eq!(peer.instance_id, "new-node");
    }

    #[test]
    fn rejoining_replaces_falsey_presence_using_clojure_or_semantics() {
        let app = Uuid::new_v4();
        let session = Uuid::new_v4();
        let mut rooms = RoomStore::default();
        rooms
            .join(app, "lobby", session, "first", None, json!({"v": 1}))
            .unwrap();

        rooms
            .set_presence(app, "lobby", session, json!(false))
            .unwrap();
        rooms
            .join(app, "lobby", session, "second", None, json!({"v": 2}))
            .unwrap();
        assert_eq!(
            rooms.room_data(app, "lobby").unwrap()[&session].data,
            json!({"v": 2})
        );

        rooms
            .set_presence(app, "lobby", session, Value::Null)
            .unwrap();
        rooms
            .join(app, "lobby", session, "third", None, json!(false))
            .unwrap();
        assert_eq!(
            rooms.room_data(app, "lobby").unwrap()[&session].data,
            json!({})
        );
    }

    #[test]
    fn presence_patches_match_reference_examples() {
        let app = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut rooms = RoomStore::default();
        rooms
            .join(app, "room", first, "dev", None, Value::Null)
            .unwrap();
        let update = rooms
            .set_presence(
                app,
                "room",
                first,
                json!({"a":"a", "b":"b", "c":"c", "d":"d", "e":"e"}),
            )
            .unwrap();
        assert_eq!(update.deliveries[0].edits.as_ref().unwrap().len(), 1);
        assert_eq!(
            update.deliveries[0].edits.as_ref().unwrap()[0].operation,
            PresenceEditOperation::Replace
        );

        let join = rooms
            .join(app, "room", second, "dev", None, Value::Null)
            .unwrap();
        assert!(Arc::ptr_eq(
            &join.deliveries[0].data,
            &join.deliveries[1].data
        ));
        let existing = join
            .deliveries
            .iter()
            .find(|delivery| delivery.session_id == first)
            .unwrap();
        assert_eq!(
            existing.edits.as_ref().unwrap()[0].operation,
            PresenceEditOperation::Add
        );
        assert!(
            join.deliveries
                .iter()
                .find(|delivery| delivery.session_id == second)
                .unwrap()
                .edits
                .is_none()
        );

        let update = rooms
            .set_presence(
                app,
                "room",
                first,
                json!({"a":"a", "b":"b", "c":"c", "e":"E", "f":"F"}),
            )
            .unwrap();
        let edits = update.deliveries[0].edits.as_ref().unwrap();
        assert_eq!(
            edits.iter().map(|edit| edit.operation).collect::<Vec<_>>(),
            vec![
                PresenceEditOperation::Remove,
                PresenceEditOperation::Replace,
                PresenceEditOperation::Add
            ]
        );
        assert_eq!(
            edits[0].path.last(),
            Some(&PresencePathSegment::Key("d".into()))
        );
        assert_eq!(
            edits[1].path.last(),
            Some(&PresencePathSegment::Key("e".into()))
        );
        assert_eq!(
            edits[2].path.last(),
            Some(&PresencePathSegment::Key("f".into()))
        );
    }

    #[test]
    fn leaving_notifies_remaining_peers_and_cleans_indexes() {
        let app = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut rooms = RoomStore::default();
        for session in [first, second] {
            rooms
                .join(app, "room", session, "dev", None, Value::Null)
                .unwrap();
        }
        let update = rooms.leave(app, "room", second).unwrap();
        assert_eq!(update.deliveries.len(), 1);
        let edit = &update.deliveries[0].edits.as_ref().unwrap()[0];
        assert_eq!(edit.path, vec![PresencePathSegment::Session(second)]);
        assert_eq!(edit.operation, PresenceEditOperation::Remove);
        assert!(!rooms.in_room(app, "room", second));
        rooms.leave_all(first);
        assert!(rooms.room_data(app, "room").is_none());
    }

    #[test]
    fn broadcasts_split_local_and_remote_recipients_without_mutation() {
        let app = Uuid::new_v4();
        let sender = Uuid::new_v4();
        let local = Uuid::new_v4();
        let remote = Uuid::new_v4();
        let user = Uuid::new_v4();
        let room = RoomKey::new(app, "room");
        let mut rooms = RoomStore::default();
        rooms
            .join(app, "room", sender, "node-a", Some(user), Value::Null)
            .unwrap();
        rooms
            .join(app, "room", local, "node-a", None, Value::Null)
            .unwrap();
        let mut snapshot = rooms.room_data(app, "room").unwrap().clone();
        snapshot.insert(
            remote,
            PeerPresence {
                peer_id: remote,
                instance_id: "node-b".into(),
                user: None,
                data: json!({}),
            },
        );
        rooms.apply_remote_snapshot(&room, snapshot);
        let before = rooms.room_data(app, "room").unwrap().clone();
        let plan = rooms
            .broadcast(
                app,
                "room",
                sender,
                Some(user),
                json!("cursor"),
                json!({"x": 1}),
            )
            .unwrap();
        assert_eq!(plan.local_session_ids, vec![local]);
        assert_eq!(plan.remote_session_ids, vec![remote]);
        assert_eq!(plan.sender_ack.data.peer_id, sender);
        assert_eq!(rooms.room_data(app, "room").unwrap(), &before);
    }

    #[test]
    fn older_cluster_snapshot_cannot_roll_back_a_newer_room_revision() {
        let app = Uuid::new_v4();
        let session = Uuid::new_v4();
        let room = RoomKey::new(app, "room");
        let peer = |value| {
            BTreeMap::from([(
                session,
                PeerPresence {
                    peer_id: session,
                    instance_id: "node".to_owned(),
                    user: None,
                    data: json!({"version": value}),
                },
            )])
        };
        let mut rooms = RoomStore::default();
        rooms.apply_versioned_remote_snapshot(&room, 2, peer(2));
        let stale = rooms.apply_versioned_remote_snapshot(&room, 1, peer(1));
        assert!(stale.deliveries.is_empty());
        assert_eq!(
            rooms.room_data(app, "room").unwrap()[&session].data,
            json!({"version": 2})
        );
    }

    #[test]
    fn presence_edit_serializes_as_wire_tuple() {
        let session = Uuid::nil();
        let edit = PresenceEdit {
            path: vec![
                PresencePathSegment::Session(session),
                PresencePathSegment::Key("data".into()),
            ],
            operation: PresenceEditOperation::Replace,
            value: Some(json!({"x": 1})),
        };
        assert_eq!(
            serde_json::to_value(edit).unwrap(),
            json!([[session, "data"], "r", {"x": 1}])
        );
    }
}
