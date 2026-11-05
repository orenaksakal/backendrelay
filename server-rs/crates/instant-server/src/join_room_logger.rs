use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};
use tokio_postgres::Client;
use tracing::{Instrument, error, info_span};
use uuid::Uuid;

use crate::postgres_pool::PostgresClientSource;
use crate::{INSTANT_CONFIG_APP_ID, PostgresPool};

const JOIN_ROOM_LOGGING_TOGGLE: &str = "join-room-logging-enabled?";

/// Best-effort analytics boundary invoked after a successful Presence room join.
pub trait JoinRoomLogger: Send + Sync + 'static {
    fn log_join(&self, app_id: Uuid);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NoopJoinRoomLogger;

impl JoinRoomLogger for NoopJoinRoomLogger {
    fn log_join(&self, _app_id: Uuid) {}
}

/// Rust-native replacement for the JVM `join-room-logger` grouped queue.
///
/// Enqueueing never waits for PostgreSQL. One background worker combines all
/// currently queued joins by app and applies each count through an atomic live
/// toggle check plus upsert.
pub struct PostgresJoinRoomLogger {
    sender: mpsc::UnboundedSender<LoggerCommand>,
}

enum LoggerCommand {
    Join(Uuid),
    Flush(oneshot::Sender<()>),
}

impl PostgresJoinRoomLogger {
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self::spawn(PostgresClientSource::fixed(client), INSTANT_CONFIG_APP_ID)
    }

    #[must_use]
    pub fn from_pool(pool: PostgresPool) -> Self {
        Self::spawn(PostgresClientSource::pooled(pool), INSTANT_CONFIG_APP_ID)
    }

    #[must_use]
    pub fn with_config_app_id(client: Client, config_app_id: Uuid) -> Self {
        Self::spawn(PostgresClientSource::fixed(client), config_app_id)
    }

    fn spawn(client: PostgresClientSource, config_app_id: Uuid) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                let mut counts = HashMap::new();
                let mut flushes = Vec::new();
                accumulate_command(command, &mut counts, &mut flushes);
                while let Ok(command) = receiver.try_recv() {
                    accumulate_command(command, &mut counts, &mut flushes);
                }
                for (app_id, join_count) in counts {
                    let span = info_span!(
                        "join-room-logger/process",
                        %app_id,
                        join_count
                    );
                    async {
                        if let Err(message) =
                            record_join_count(&client, config_app_id, app_id, join_count).await
                        {
                            error!(
                                name = "join-room-logger/process-error",
                                %app_id,
                                error = %message,
                                "could not persist join-room usage"
                            );
                        }
                    }
                    .instrument(span)
                    .await;
                }
                for flush in flushes {
                    let _receiver_was_dropped = flush.send(());
                }
            }
        });
        Self { sender }
    }

    /// Waits until all events queued before this call have been processed.
    pub async fn flush(&self) {
        let (sender, receiver) = oneshot::channel();
        if self.sender.send(LoggerCommand::Flush(sender)).is_ok() {
            let _worker_has_stopped = receiver.await;
        }
    }
}

impl JoinRoomLogger for PostgresJoinRoomLogger {
    fn log_join(&self, app_id: Uuid) {
        let _worker_has_stopped = self.sender.send(LoggerCommand::Join(app_id));
    }
}

fn accumulate_command(
    command: LoggerCommand,
    counts: &mut HashMap<Uuid, i64>,
    flushes: &mut Vec<oneshot::Sender<()>>,
) {
    match command {
        LoggerCommand::Join(app_id) => {
            counts
                .entry(app_id)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }
        LoggerCommand::Flush(sender) => flushes.push(sender),
    }
}

async fn record_join_count(
    client: &PostgresClientSource,
    config_app_id: Uuid,
    app_id: Uuid,
    join_count: i64,
) -> Result<(), String> {
    let client = client.acquire().await.map_err(|error| error.to_string())?;
    client
        .execute(
            "INSERT INTO join_room_logs (app_id, join_count)
             SELECT $2, $3
             WHERE EXISTS (
               SELECT 1
               FROM attrs setting_attr
               JOIN triples toggle_setting
                 ON toggle_setting.app_id = $1 AND toggle_setting.attr_id = setting_attr.id
               JOIN attrs toggled_attr
                 ON toggled_attr.app_id = $1 AND toggled_attr.etype = 'toggles'
                AND toggled_attr.label = 'toggled'
               JOIN triples toggle_value
                 ON toggle_value.app_id = $1
                AND toggle_value.entity_id = toggle_setting.entity_id
                AND toggle_value.attr_id = toggled_attr.id
               WHERE setting_attr.app_id = $1 AND setting_attr.etype = 'toggles'
                 AND setting_attr.label = 'setting'
                 AND toggle_setting.value = to_jsonb($4::text)
                 AND toggle_value.value <> 'null'::jsonb
                 AND toggle_value.value <> 'false'::jsonb
             )
             ON CONFLICT (app_id) DO UPDATE
             SET join_count = join_room_logs.join_count + EXCLUDED.join_count",
            &[
                &config_app_id,
                &app_id,
                &join_count,
                &JOIN_ROOM_LOGGING_TOGGLE,
            ],
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_joins_combine_by_app_without_losing_flush_barriers() {
        let app_id = Uuid::new_v4();
        let other_app_id = Uuid::new_v4();
        let mut counts = HashMap::new();
        let mut flushes = Vec::new();
        accumulate_command(LoggerCommand::Join(app_id), &mut counts, &mut flushes);
        accumulate_command(LoggerCommand::Join(app_id), &mut counts, &mut flushes);
        accumulate_command(LoggerCommand::Join(other_app_id), &mut counts, &mut flushes);
        let (flush, _receiver) = oneshot::channel();
        accumulate_command(LoggerCommand::Flush(flush), &mut counts, &mut flushes);

        assert_eq!(counts, HashMap::from([(app_id, 2), (other_app_id, 1)]));
        assert_eq!(flushes.len(), 1);
    }
}
