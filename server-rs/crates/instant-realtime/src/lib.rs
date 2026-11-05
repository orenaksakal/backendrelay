//! Realtime session, subscription, invalidation, and room state for `InstantDB`.

mod cache;
mod epoch;
mod invalidator;
mod protocol;
mod replication;
mod room;
mod store;
mod subscription_index;
mod topic;

pub use cache::{
    PublishPermit, PublishToken, QueryCache, QueryCacheBudget, QueryCacheCleanup, QueryCacheError,
    QueryCacheMetrics, QueryCacheOptions, QueryCachePublisher, ResultCacheBudget,
    ResultCacheBudgetSnapshot,
};
pub use epoch::{
    AppInvalidationEpochs, DATA_INVALIDATION_LOG_LIMIT, DataInvalidationLog, EpochPermit,
    EpochPublisher, EpochSnapshot, InvalidationEpoch, logged_window_affects,
};
pub use invalidator::{ContinuityGap, DispatchOutcome, InvalidationCoordinator};
pub use protocol::{
    BroadcastFrame, ClientFeature, ClientFeatures, MAX_TESTED_SDK_VERSION, PresenceMessage,
    SDK_LABELS, SDK_VERSION_OTHER, ServerBroadcast, ServerBroadcastData, broadcast_frames,
    is_above_max_tested_sdk_version, normalize_sdk_version, presence_message, sdk_session,
    sdk_version_is_newer,
};
pub use replication::{
    ReplicationPipelineError, ReplicationPipelineOutcome, WalInvalidationPipeline,
};
pub use room::{
    BroadcastData, BroadcastPayload, BroadcastPlan, PeerPresence, PeerUser, PresenceDelivery,
    PresenceEdit, PresenceEditOperation, PresencePathSegment, PresenceUpdate, RoomError, RoomKey,
    RoomStore,
};
pub use store::{
    InvalidationOutcome, QueryCompletion, QueryRecord, RealtimeError, RealtimeStore, RefreshPlan,
    ReturnType, Session, SubscriptionContext,
};
pub use subscription_index::SessionTopicIndex;
pub use topic::{
    Comparator, ComparatorKind, QueryTopic, QueryTopicPart, QueryValue, topics_intersect,
};
