use futures_util::FutureExt;
use instant_wal::{FeedbackState, LogicalReplicationOptions, Lsn};
use pg_walstream::connection::PgReplicationConnection;
use pg_walstream::{CancellationToken, ReplicationError, ReplicationSlotOptions, SlotType};

/// A newly-created permanent logical slot whose exported snapshot remains valid
/// while this value is alive and before replication starts on its connection.
pub struct ExportedSnapshotSlot {
    _connection: PgReplicationConnection,
    pub consistent_point: Lsn,
    pub snapshot_name: String,
}

impl std::fmt::Debug for ExportedSnapshotSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExportedSnapshotSlot")
            .field("consistent_point", &self.consistent_point)
            .field("snapshot_name", &self.snapshot_name)
            .finish_non_exhaustive()
    }
}

/// Creates the permanent wal2json aggregator slot with `EXPORT_SNAPSHOT`.
/// Returns `None` when another process already created the slot. The returned
/// guard must remain alive until a regular repeatable-read transaction imports
/// `snapshot_name`.
///
/// # Errors
///
/// Returns a replication transport/protocol error or rejects a malformed slot
/// response from PostgreSQL.
pub fn create_exported_snapshot_slot(
    connection_string: &str,
    slot_name: &str,
) -> Result<Option<ExportedSnapshotSlot>, ReplicationError> {
    let connection_string = replication_connection_string(connection_string);
    let mut connection = PgReplicationConnection::connect(&connection_string)?;
    let options = ReplicationSlotOptions {
        snapshot: Some("export".to_owned()),
        ..ReplicationSlotOptions::default()
    };
    let result = match connection.create_replication_slot_with_options(
        slot_name,
        SlotType::Logical,
        Some("wal2json"),
        &options,
    ) {
        Ok(result) => result,
        Err(error) if error.to_string().contains("already exists") => return Ok(None),
        Err(error) => return Err(error),
    };
    let consistent_point = result
        .get_value(0, 1)
        .ok_or_else(|| ReplicationError::protocol("slot response omitted consistent point"))?;
    let consistent_point = consistent_point.parse().map_err(|_| {
        ReplicationError::protocol(format!(
            "slot response returned invalid consistent point `{consistent_point}`"
        ))
    })?;
    let snapshot_name = result
        .get_value(0, 2)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ReplicationError::protocol("slot response omitted exported snapshot"))?;
    Ok(Some(ExportedSnapshotSlot {
        _connection: connection,
        consistent_point,
        snapshot_name,
    }))
}

/// Live PostgreSQL `CopyBoth` connection configured for `InstantDB`'s wal2json stream.
pub struct Wal2JsonReplicationConnection {
    connection: PgReplicationConnection,
}

impl std::fmt::Debug for Wal2JsonReplicationConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Wal2JsonReplicationConnection")
            .finish_non_exhaustive()
    }
}

impl Wal2JsonReplicationConnection {
    /// Connects, authenticates in replication mode, and starts the configured slot.
    ///
    /// The connection string uses libpq-style URI parameters. TLS is provided by the
    /// transport's pure-Rust rustls backend. Connection establishment is synchronous;
    /// process wiring should create it on a blocking worker when startup latency must
    /// not occupy an async executor thread.
    ///
    /// # Errors
    ///
    /// Returns a transport or server error if connection, authentication, or
    /// `START_REPLICATION` fails.
    pub fn connect(
        connection_string: &str,
        options: &LogicalReplicationOptions,
    ) -> Result<Self, ReplicationError> {
        let connection_string = replication_connection_string(connection_string);
        let mut connection = PgReplicationConnection::connect(&connection_string)?;
        let plugin_options = wal2json_plugin_options(options.tables());
        let borrowed = plugin_options
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        connection.start_replication(
            options.slot_name(),
            options.start_lsn().as_u64(),
            &borrowed,
        )?;
        Ok(Self { connection })
    }

    /// Receives the next raw server `CopyData` payload.
    ///
    /// # Errors
    ///
    /// Returns a transport/server error or cancellation.
    pub async fn recv(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, ReplicationError> {
        self.connection
            .get_copy_data_async(cancellation)
            .await
            .map(|payload| payload.to_vec())
    }

    /// Receives an immediately available frame without waiting for more bytes.
    ///
    /// This narrowly relies on the pinned native `pg_walstream` 0.8.1 driver:
    /// inline partial bytes and complete frames live in the connection's
    /// `read_buf`/`pending`; its only read await is cancellation-safe `read_buf`.
    /// The threaded driver's pending frames and batch receiver also live in the
    /// connection, and `mpsc::Receiver::recv` is cancellation-safe. Neither path
    /// removes a frame before returning `Ready`. Re-audit this method before
    /// changing the transport version or backend; it is not a generic promise
    /// about dropping the upstream receive future or cancelling its token.
    ///
    /// # Errors
    ///
    /// Returns a ready transport/server error or cancellation.
    pub fn recv_ready(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Option<Result<Vec<u8>, ReplicationError>> {
        self.recv(cancellation).now_or_never()
    }

    /// Sends the decoder's monotonic written/flushed/applied positions.
    ///
    /// # Errors
    ///
    /// Returns a transport error if the feedback frame cannot be sent.
    pub async fn send_feedback(
        &mut self,
        state: FeedbackState,
        reply_requested: bool,
    ) -> Result<(), ReplicationError> {
        self.connection
            .send_standby_status_update(
                state.written().as_u64(),
                state.flushed().as_u64(),
                state.applied().as_u64(),
                reply_requested,
            )
            .await
    }
}

fn replication_connection_string(connection_string: &str) -> String {
    if connection_string.split_once('?').is_some_and(|(_, query)| {
        query
            .split('&')
            .any(|part| part.starts_with("replication="))
    }) {
        return connection_string.to_owned();
    }
    let separator = if connection_string.contains('?') {
        '&'
    } else {
        '?'
    };
    format!("{connection_string}{separator}replication=database")
}

fn wal2json_plugin_options(tables: &[String]) -> Vec<(&'static str, String)> {
    vec![
        ("include-lsn", "true".to_owned()),
        ("format-version", "2".to_owned()),
        ("add-tables", tables.join(",")),
    ]
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;

    use super::*;
    use instant_wal::SlotPurpose;

    #[test]
    fn plugin_options_preserve_reference_wal2json_contract() {
        assert_eq!(
            wal2json_plugin_options(
                &SlotPurpose::Aggregator
                    .tables()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            ),
            vec![
                ("include-lsn", "true".to_owned()),
                ("format-version", "2".to_owned()),
                ("add-tables", "public.triples".to_owned()),
            ]
        );
        let invalidator = wal2json_plugin_options(
            &SlotPurpose::Invalidator
                .tables()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
        );
        assert!(invalidator[2].1.starts_with("public.idents,public.triples"));
        assert!(invalidator[2].1.ends_with("public.webhooks"));
    }

    #[test]
    fn replication_connections_add_the_required_startup_parameter_once() {
        assert_eq!(
            replication_connection_string("postgresql://localhost/instant"),
            "postgresql://localhost/instant?replication=database"
        );
        assert_eq!(
            replication_connection_string("postgresql://localhost/instant?sslmode=require"),
            "postgresql://localhost/instant?sslmode=require&replication=database"
        );
        assert_eq!(
            replication_connection_string(
                "postgresql://localhost/instant?sslmode=require&replication=database"
            ),
            "postgresql://localhost/instant?sslmode=require&replication=database"
        );
    }

    async fn mock_start_replication(socket: &mut TcpStream) {
        let startup_len = socket.read_u32().await.unwrap();
        let mut startup = vec![0; startup_len as usize - 4];
        socket.read_exact(&mut startup).await.unwrap();
        let mut response = server_message(b'R', &0_u32.to_be_bytes());
        response.extend(server_message(b'S', b"server_version\x0016.1\x00"));
        response.extend(server_message(b'K', &[0; 8]));
        response.extend(server_message(b'Z', b"I"));
        socket.write_all(&response).await.unwrap();
        assert_eq!(socket.read_u8().await.unwrap(), b'Q');
        let query_len = socket.read_u32().await.unwrap();
        let mut query = vec![0; query_len as usize - 4];
        socket.read_exact(&mut query).await.unwrap();
        assert!(
            String::from_utf8(query)
                .unwrap()
                .starts_with("START_REPLICATION")
        );
        socket
            .write_all(&server_message(b'W', &[0; 3]))
            .await
            .unwrap();
    }

    fn server_message(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![tag];
        frame.extend(u32::try_from(payload.len() + 4).unwrap().to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    async fn ready_receive_preserves_split_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (advance, mut steps) = mpsc::channel::<()>(1);
        let (sent, mut sent_steps) = mpsc::channel::<()>(1);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            mock_start_replication(&mut socket).await;
            let frame = server_message(b'd', b"split WAL frame");
            for split in [1, 4, 5, 7, frame.len() - 1] {
                steps.recv().await.unwrap();
                socket.write_all(&frame[..split]).await.unwrap();
                sent.send(()).await.unwrap();
                steps.recv().await.unwrap();
                let mut remainder = frame[split..].to_vec();
                remainder.extend(server_message(b'd', b"next WAL frame"));
                socket.write_all(&remainder).await.unwrap();
            }
            steps.recv().await.unwrap();
        });
        let mut connection = tokio::task::spawn_blocking(move || {
            Wal2JsonReplicationConnection::connect(
                &format!("postgresql://test@{address}/test?sslmode=disable"),
                &LogicalReplicationOptions::new(
                    "ready_read_test",
                    Lsn::default(),
                    SlotPurpose::Invalidator,
                )
                .unwrap(),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let cancellation = CancellationToken::new();
        for _ in 0..5 {
            // Polling before and repeatedly after a partial wire frame must not
            // consume its header/body or restart the threaded push receiver.
            assert!(connection.recv_ready(&cancellation).is_none());
            advance.send(()).await.unwrap();
            sent_steps.recv().await.unwrap();
            for _ in 0..16 {
                assert!(connection.recv_ready(&cancellation).is_none());
                tokio::task::yield_now().await;
            }
            advance.send(()).await.unwrap();
            let first =
                tokio::time::timeout(Duration::from_secs(5), connection.recv(&cancellation))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(first, b"split WAL frame");
            let second = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(frame) = connection.recv_ready(&cancellation) {
                        break frame.unwrap();
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(second, b"next WAL frame");
        }
        advance.send(()).await.unwrap();
        server.await.unwrap();
        cancellation.cancel();
    }

    #[tokio::test]
    async fn ready_receive_preserves_split_frames_on_threaded_driver() {
        ready_receive_preserves_split_frames().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ready_receive_preserves_split_frames_on_inline_driver() {
        ready_receive_preserves_split_frames().await;
    }
}
