use chrono::{DateTime, Utc};
use instant_wal::{
    FeedbackState, Isn, Lsn, ReplicationStreamError, TopicError, Wal2JsonReplicationDecoder,
    WalCacheEviction, WalRecord, WalTransformError, transform_wal_transaction,
};
use thiserror::Error;

use crate::{DispatchOutcome, InvalidationCoordinator, RealtimeStore};

/// Result of applying one PostgreSQL replication `CopyData` payload.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReplicationPipelineOutcome {
    /// Typed application record available to durable history/webhook sinks.
    pub record: Option<WalRecord>,
    /// Realtime invalidation work produced by a committed application transaction.
    pub dispatch: Option<DispatchOutcome>,
    /// Immediate raw-row cache work, emitted before application transaction assembly.
    pub cache_eviction: Option<WalCacheEviction>,
    /// Process-global ISN observed when any primary WAL transaction commits,
    /// including transactions filtered out of application history.
    pub max_seen_isn: Option<Isn>,
    /// Durable boundary advanced by an applied or intentionally filtered commit.
    pub durable_lsn: Option<Lsn>,
    /// Immediate standby-status reply requested by a primary keepalive.
    pub feedback: Option<[u8; 34]>,
}

#[derive(Debug, Error)]
pub enum ReplicationPipelineError {
    #[error(transparent)]
    Stream(#[from] ReplicationStreamError),
    #[error(transparent)]
    Transform(#[from] WalTransformError),
    #[error(transparent)]
    Topic(#[from] TopicError),
}

/// Stateful bridge from wal2json replication frames to realtime invalidation.
///
/// The bridge deliberately owns the acknowledgement boundary: a commit is marked
/// flushed/applied only after its record has been transformed and dispatched. A
/// caller must terminate and recreate the pipeline from its durable checkpoint after
/// any returned error rather than continuing the damaged stream.
#[derive(Debug)]
pub struct WalInvalidationPipeline {
    slot_num: i32,
    decoder: Wal2JsonReplicationDecoder,
    coordinator: InvalidationCoordinator,
}

impl WalInvalidationPipeline {
    #[must_use]
    pub fn new(slot_num: i32, previous_isn: Option<Isn>, confirmed_lsn: Lsn) -> Self {
        Self {
            slot_num,
            decoder: Wal2JsonReplicationDecoder::new(slot_num, previous_isn, confirmed_lsn),
            coordinator: InvalidationCoordinator::default(),
        }
    }

    #[must_use]
    pub const fn feedback_state(&self) -> FeedbackState {
        self.decoder.feedback_state()
    }

    #[must_use]
    pub const fn latest_isn(&self) -> Option<Isn> {
        self.coordinator.latest_isn()
    }

    /// Resets frame assembly after a lost transport while retaining the last durable
    /// LSN and committed ISN continuity.
    pub fn restart_after_disconnect(&mut self) {
        self.decoder.restart_after_disconnect();
    }

    /// Applies one server `CopyData` payload without acknowledging application commits.
    ///
    /// Commits filtered by [`transform_wal_transaction`] are acknowledged immediately:
    /// they contain no application invalidation work. For returned application records,
    /// callers must finish durable downstream work and call [`Self::acknowledge`].
    /// Malformed frames, records, or topics never advance flushed/applied positions.
    ///
    /// # Errors
    ///
    /// Returns [`ReplicationPipelineError`] when framing, transaction assembly,
    /// transformation, or topic extraction fails.
    pub fn push(
        &mut self,
        store: &mut RealtimeStore,
        payload: &[u8],
        reply_at: DateTime<Utc>,
    ) -> Result<ReplicationPipelineOutcome, ReplicationPipelineError> {
        let decoded = self.decoder.push(payload, reply_at)?;
        let cache_eviction = decoded.cache_eviction;
        let filtered_commit_lsn = decoded.filtered_commit_lsn;
        let max_seen_isn = decoded
            .transaction
            .as_ref()
            .map(|transaction| transaction.isn)
            .or_else(|| filtered_commit_lsn.map(|lsn| Isn::new(self.slot_num, lsn)));
        let Some(raw) = decoded.transaction else {
            return Ok(ReplicationPipelineOutcome {
                cache_eviction,
                max_seen_isn,
                durable_lsn: filtered_commit_lsn,
                feedback: decoded.feedback,
                ..ReplicationPipelineOutcome::default()
            });
        };

        let durable_lsn = raw.next_lsn;
        let record = transform_wal_transaction(raw)?;
        let dispatch = record
            .as_ref()
            .map(|record| self.coordinator.dispatch(store, record))
            .transpose()?;
        Ok(ReplicationPipelineOutcome {
            record,
            dispatch,
            cache_eviction,
            max_seen_isn,
            durable_lsn: Some(durable_lsn),
            feedback: decoded.feedback,
        })
    }

    /// Advances the slot's durable feedback boundary after downstream persistence.
    pub fn acknowledge(&mut self, durable_lsn: Lsn) {
        self.decoder.acknowledge(durable_lsn);
    }

    /// Builds periodic standby feedback from the current receive/durable positions.
    #[must_use]
    pub fn periodic_feedback(&self, sent_at: DateTime<Utc>) -> [u8; 34] {
        self.decoder.periodic_feedback(sent_at)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::Session;

    fn xlog(wal_start: u64, wal_end: u64, value: &serde_json::Value) -> Vec<u8> {
        let mut frame = vec![b'w'];
        frame.extend_from_slice(&wal_start.to_be_bytes());
        frame.extend_from_slice(&wal_end.to_be_bytes());
        frame.extend_from_slice(&0_i64.to_be_bytes());
        frame.extend_from_slice(&serde_json::to_vec(value).unwrap());
        frame
    }

    fn push_transaction(
        pipeline: &mut WalInvalidationPipeline,
        store: &mut RealtimeStore,
        app_id: Uuid,
        attr_id: Option<Uuid>,
    ) -> Result<ReplicationPipelineOutcome, ReplicationPipelineError> {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        for (index, value) in [
            json!({"action": "B"}),
            json!({
                "action": "I",
                "table": "transactions",
                "columns": [
                    {"name": "id", "value": 42},
                    {"name": "app_id", "value": app_id}
                ]
            }),
            json!({
                "action": "I",
                "table": "idents",
                "columns": attr_id.map_or_else(Vec::new, |attr_id| vec![
                    json!({"name": "attr_id", "value": attr_id})
                ])
            }),
        ]
        .iter()
        .enumerate()
        {
            let offset = u64::try_from(index).unwrap();
            pipeline.push(store, &xlog(10 + offset, 20 + offset, value), now)?;
        }
        pipeline.push(
            store,
            &xlog(
                13,
                23,
                &json!({"action": "C", "lsn": "0/63", "nextlsn": "0/64"}),
            ),
            now,
        )
    }

    #[test]
    fn committed_frame_dispatches_before_advancing_durable_feedback() {
        let app_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let attr_id = Uuid::new_v4();
        let mut store = RealtimeStore::default();
        store.upsert_session(Session {
            app_id: Some(app_id),
            ..Session::new(session_id)
        });
        let mut pipeline = WalInvalidationPipeline::new(2, None, Lsn::from_u64(5));

        let outcome = push_transaction(&mut pipeline, &mut store, app_id, Some(attr_id)).unwrap();
        let dispatch = outcome.dispatch.unwrap();
        assert_eq!(
            dispatch.refresh_sessions,
            [session_id].into_iter().collect()
        );
        assert!(dispatch.schema_refresh);
        assert_eq!(dispatch.topic_count, 1);
        assert_eq!(outcome.durable_lsn, Some(Lsn::from_u64(100)));
        assert_eq!(outcome.max_seen_isn, Some(Isn::new(2, Lsn::from_u64(100))));
        assert!(outcome.record.is_some());
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(5));
        pipeline.acknowledge(outcome.durable_lsn.unwrap());
        // Feedback cannot claim the transaction's `nextlsn` until the
        // replication connection has actually received bytes through that LSN.
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(23));
        assert_eq!(pipeline.feedback_state().applied(), Lsn::from_u64(23));
        assert_eq!(store.processed_tx_id(app_id), Some(42));
        assert_eq!(pipeline.latest_isn(), Some(Isn::new(2, Lsn::from_u64(100))));
    }

    #[test]
    fn filtered_raw_cache_row_exposes_its_commit_boundary_and_survives_restart() {
        let app_id = Uuid::new_v4();
        let mut pipeline = WalInvalidationPipeline::new(1, None, Lsn::default());
        let mut store = RealtimeStore::default();
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        pipeline
            .push(&mut store, &xlog(1, 1, &json!({"action": "B"})), now)
            .unwrap();
        let outcome = pipeline
            .push(
                &mut store,
                &xlog(
                    2,
                    2,
                    &json!({
                        "action": "U",
                        "table": "rules",
                        "identity": [{"name": "app_id", "value": app_id}]
                    }),
                ),
                now,
            )
            .unwrap();

        assert_eq!(
            outcome.cache_eviction,
            Some(WalCacheEviction::Rules {
                app_id: Some(app_id)
            })
        );
        assert!(outcome.record.is_none());
        assert!(outcome.durable_lsn.is_none());

        let close = pipeline
            .push(
                &mut store,
                &xlog(
                    3,
                    3,
                    &json!({"action": "C", "lsn": "0/2", "nextlsn": "0/3"}),
                ),
                now,
            )
            .unwrap();
        assert!(close.record.is_none());
        assert!(close.cache_eviction.is_none());
        assert_eq!(close.durable_lsn, Some(Lsn::from_u64(3)));
        assert_eq!(close.max_seen_isn, Some(Isn::new(1, Lsn::from_u64(3))));
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::default());

        pipeline.acknowledge(close.durable_lsn.unwrap());
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(3));
        pipeline.restart_after_disconnect();
        assert_eq!(pipeline.feedback_state().written(), Lsn::from_u64(3));
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(3));
    }

    #[test]
    fn topic_failure_does_not_advance_durable_feedback() {
        let app_id = Uuid::new_v4();
        let mut store = RealtimeStore::default();
        let mut pipeline = WalInvalidationPipeline::new(2, None, Lsn::from_u64(5));

        let result = push_transaction(&mut pipeline, &mut store, app_id, None);
        assert!(
            matches!(
                result,
                Err(ReplicationPipelineError::Topic(TopicError::InvalidColumn(
                    "attr_id"
                )))
            ),
            "{result:?}"
        );
        assert_eq!(pipeline.feedback_state().written(), Lsn::from_u64(23));
        assert_eq!(pipeline.feedback_state().flushed(), Lsn::from_u64(5));
        assert_eq!(pipeline.feedback_state().applied(), Lsn::from_u64(5));
        assert_eq!(store.processed_tx_id(app_id), None);
    }
}
