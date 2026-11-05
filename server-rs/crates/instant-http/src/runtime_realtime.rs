use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Extension, Query, State};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{FutureExt as _, SinkExt as _, StreamExt as _, stream};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use uuid::Uuid;

use crate::{ApiError, ApiErrorType};

mod transport_metrics;
pub use transport_metrics::runtime_websocket_exit_metrics;
use transport_metrics::{WebsocketExit, WebsocketExitGuard};

const WEBSOCKET_INBOUND_LIMIT: usize = 128;
const WEBSOCKET_EXPIRY_DRAIN_LIMIT: usize = WEBSOCKET_INBOUND_LIMIT;
const WEBSOCKET_IO_TIMEOUT: Duration = Duration::from_secs(1);
const WEBSOCKET_PING_INTERVAL: Duration = Duration::from_secs(5);
const WEBSOCKET_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const SSE_PUSH_MAX_BYTES: usize = 2 * 1024 * 1024;
const WEBSOCKET_MAX_BYTES: usize = SSE_PUSH_MAX_BYTES;
const WEBSOCKET_INBOUND_MAX_BYTES: usize = SSE_PUSH_MAX_BYTES;
const WEBSOCKET_DECODED_MAX_BYTES: usize = 16 * 1024 * 1024;
const SSE_PUSH_MAX_MESSAGES: usize = 1_000;
/// IANA WebSocket close code for "service restart". Missing from
/// `close_code`, which only covers the codes a peer may receive in a normal
/// protocol error reply.
const WEBSOCKET_CLOSE_SERVICE_RESTART: u16 = 1012;
const DRAIN_RETRY_AFTER_HEADER_VALUE: &str = "1";

/// Cross-task drain coordination shared by every live transport.
///
/// Starting a drain is irreversible for the lifetime of the signal. WebSocket
/// loops observe it to send a 1012 close frame, SSE streams end cleanly, and a
/// backend that is already draining refuses new transports instead of accepting
/// a client it is about to disconnect.
#[derive(Clone, Default)]
pub struct RuntimeDrainSignal {
    state: Arc<RuntimeDrainState>,
}

#[derive(Default)]
struct RuntimeDrainState {
    draining: AtomicBool,
    notify: Notify,
}

impl RuntimeDrainSignal {
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.state.draining.load(Ordering::Acquire)
    }

    pub fn begin_drain(&self) {
        self.state.draining.store(true, Ordering::Release);
        self.state.notify.notify_waiters();
    }

    /// Resolves once a drain has started, including one that started before
    /// this future was created.
    pub async fn drained(&self) {
        if self.is_draining() {
            return;
        }
        let notified = self.state.notify.notified();
        tokio::pin!(notified);
        // Register before the second check so a drain between the check and
        // the await cannot be missed.
        notified.as_mut().enable();
        if self.is_draining() {
            return;
        }
        notified.await;
    }
}

fn draining_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, DRAIN_RETRY_AFTER_HEADER_VALUE)],
        Json(serde_json::json!({
            "type": "unavailable",
            "message": "Server is restarting; reconnect shortly.",
        })),
    )
        .into_response()
}

struct WebsocketBudget {
    wire: Arc<Semaphore>,
    decoded: Arc<Semaphore>,
}

// Conservative allocation units, not a measurement of allocator RSS. Count
// structure before deserialization so dense JSON cannot allocate first and fail later.
fn decoded_json_charge(bytes: &[u8]) -> usize {
    let mut charge = bytes.len().saturating_mul(4).saturating_add(256);
    let mut quoted = false;
    let mut escaped = false;
    for &byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if matches!(byte, b'[' | b'{' | b',' | b':') {
            charge = charge.saturating_add(256);
        }
    }
    charge
}

const SSE_PUSH_FORMAT_HEADER: &str = "x-instant-sse-push-format";
const SSE_PUSH_EXPOSE_HEADERS: &str = "access-control-expose-headers";
const SSE_PUSH_NDJSON_CONTENT_TYPE: &str = "application/x-ndjson";

#[derive(Clone, Copy)]
struct WebsocketLiveness {
    ping_interval: Duration,
    idle_timeout: Duration,
}

const PRODUCTION_WEBSOCKET_LIVENESS: WebsocketLiveness = WebsocketLiveness {
    ping_interval: WEBSOCKET_PING_INTERVAL,
    idle_timeout: WEBSOCKET_IDLE_TIMEOUT,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeTransport {
    WebSocket,
    ServerSentEvents,
}

pub struct RuntimeRealtimeSession {
    pub session_id: Uuid,
    retry_interval: Duration,
    drain: RuntimeDrainSignal,
    receiver: mpsc::Receiver<Value>,
    cleanup: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl RuntimeRealtimeSession {
    #[must_use]
    pub fn new(
        session_id: Uuid,
        receiver: mpsc::Receiver<Value>,
        cleanup: impl FnOnce() + Send + Sync + 'static,
    ) -> Self {
        Self {
            session_id,
            retry_interval: Duration::from_millis(500),
            drain: RuntimeDrainSignal::default(),
            receiver,
            cleanup: Some(Box::new(cleanup)),
        }
    }

    #[must_use]
    pub const fn with_retry_interval(mut self, retry_interval: Duration) -> Self {
        self.retry_interval = retry_interval;
        self
    }

    /// Connects the session to the backend's drain signal so its transport can
    /// shut down on its own terms instead of being dropped mid-write.
    #[must_use]
    pub fn with_drain_signal(mut self, drain: RuntimeDrainSignal) -> Self {
        self.drain = drain;
        self
    }

    #[must_use]
    pub fn drain_signal(&self) -> RuntimeDrainSignal {
        self.drain.clone()
    }

    #[must_use]
    pub const fn retry_interval(&self) -> Duration {
        self.retry_interval
    }

    pub async fn recv(&mut self) -> Option<Value> {
        self.receiver.recv().await
    }
}

impl Drop for RuntimeRealtimeSession {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeSsePushRequest {
    pub app_id: Uuid,
    pub machine_id: Uuid,
    pub session_id: Uuid,
    pub sse_token: Uuid,
    pub messages: Vec<Value>,
}

pub trait RuntimeRealtimeBackend: Send + Sync + 'static {
    fn open(
        &self,
        app_id: Option<Uuid>,
        transport: RuntimeTransport,
    ) -> impl Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send;

    /// Drain state used to refuse new transports while the process is leaving.
    /// Backends without a drain lifecycle keep accepting sessions.
    fn drain_signal(&self) -> RuntimeDrainSignal {
        RuntimeDrainSignal::default()
    }

    fn open_with_origin(
        &self,
        app_id: Option<Uuid>,
        transport: RuntimeTransport,
        _origin: Option<String>,
    ) -> impl Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send {
        self.open(app_id, transport)
    }

    fn open_with_client(
        &self,
        app_id: Option<Uuid>,
        transport: RuntimeTransport,
        origin: Option<String>,
        _client_ip: Option<IpAddr>,
    ) -> impl Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send {
        self.open_with_origin(app_id, transport, origin)
    }

    fn message(
        &self,
        session_id: Uuid,
        message: Value,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;

    fn push(
        &self,
        request: RuntimeSsePushRequest,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
}

pub fn runtime_realtime_router<B>(backend: Arc<B>) -> Router
where
    B: RuntimeRealtimeBackend,
{
    runtime_realtime_router_with_liveness(backend, PRODUCTION_WEBSOCKET_LIVENESS)
}

fn runtime_realtime_router_with_liveness<B>(backend: Arc<B>, liveness: WebsocketLiveness) -> Router
where
    B: RuntimeRealtimeBackend,
{
    Router::new()
        .route("/runtime/session", get(websocket::<B>))
        .route("/runtime/sse", get(sse_open::<B>).post(sse_push::<B>))
        .with_state((backend, liveness))
}

#[derive(Debug, Default, Deserialize)]
struct AppParams {
    app_id: Option<Uuid>,
}

async fn websocket<B>(
    State((backend, liveness)): State<(Arc<B>, WebsocketLiveness)>,
    Query(params): Query<AppParams>,
    client_ip: Option<Extension<crate::ResolvedClientIp>>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError>
where
    B: RuntimeRealtimeBackend,
{
    if backend.drain_signal().is_draining() {
        return Ok(draining_response());
    }
    let session = backend
        .open_with_client(
            params.app_id,
            RuntimeTransport::WebSocket,
            request_origin(&headers),
            client_ip.and_then(|Extension(client_ip)| client_ip.0),
        )
        .await?;
    Ok(upgrade
        .max_frame_size(WEBSOCKET_MAX_BYTES)
        .max_message_size(WEBSOCKET_MAX_BYTES)
        .on_upgrade(move |socket| websocket_loop(backend, session, socket, liveness)))
}

async fn websocket_loop<B>(
    backend: Arc<B>,
    session: RuntimeRealtimeSession,
    mut socket: WebSocket,
    liveness: WebsocketLiveness,
) where
    B: RuntimeRealtimeBackend,
{
    let session_id = session.session_id;
    let drain = session.drain_signal();
    let (message_sender, message_receiver) = mpsc::channel(WEBSOCKET_INBOUND_LIMIT);
    let byte_budget = WebsocketBudget {
        wire: Arc::new(Semaphore::new(WEBSOCKET_INBOUND_MAX_BYTES)),
        decoded: Arc::new(Semaphore::new(WEBSOCKET_DECODED_MAX_BYTES)),
    };
    let message_worker = Box::pin(websocket_message_worker(
        backend,
        session_id,
        message_receiver,
    ));
    let mut resources = WebsocketLoopResources::new(message_worker, session);
    let mut last_received_at = tokio::time::Instant::now();
    let mut unanswered_since = None;
    let first_ping_at = last_received_at + liveness.ping_interval;
    let mut ping_interval = tokio::time::interval_at(first_ping_at, liveness.ping_interval);
    let exit_guard = WebsocketExitGuard::global();
    let mut drained = std::pin::pin!(drain.drained());
    let exit_reason = loop {
        let WebsocketLoopResources {
            message_worker,
            session,
        } = &mut resources;
        let message_worker = message_worker
            .as_mut()
            .expect("message worker remains owned for the websocket loop");
        let session = session
            .as_mut()
            .expect("session remains owned for the websocket loop");
        tokio::select! {
            () = &mut drained => {
                break WebsocketExit::ServiceRestart;
            }
            () = message_worker.as_mut() => {
                break WebsocketExit::MessageWorkerEnded;
            }
            outbound = session.recv() => {
                let Some(outbound) = outbound else { break WebsocketExit::OutboundStreamEnded; };
                match tokio::time::timeout(
                    WEBSOCKET_IO_TIMEOUT,
                    socket.send(Message::Text(outbound.to_string().into())),
                ).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => break WebsocketExit::OutboundSendError,
                    Err(_) => break WebsocketExit::OutboundSendTimeout,
                }
            }
            inbound = socket.recv() => {
                let inbound = match inbound {
                    Some(Ok(inbound)) => inbound,
                    Some(Err(_)) => break WebsocketExit::InboundReceiveError,
                    None => break WebsocketExit::InboundEof,
                };
                let peer_close = matches!(&inbound, Message::Close(_));
                record_websocket_activity(
                    &inbound,
                    &mut last_received_at,
                    tokio::time::Instant::now(),
                );
                if !handle_websocket_inbound(
                    &mut socket,
                    &message_sender,
                    &byte_budget,
                    inbound,
                ).await {
                    break if peer_close { WebsocketExit::PeerClose } else { WebsocketExit::InboundHandlerEnded };
                }
            }
            _ = ping_interval.tick() => {
                let now = tokio::time::Instant::now();
                if unanswered_ping_expired(now, last_received_at, unanswered_since, liveness.idle_timeout)
                    && !buffered_activity_revives_expired_websocket(
                        &mut socket,
                        &message_sender,
                        &byte_budget,
                        &mut last_received_at,
                        liveness.idle_timeout,
                    ).await
                {
                        break WebsocketExit::IdleUnrevived;
                }
                match send_websocket_message(
                    socket.send(Message::Ping(Vec::new().into())),
                    WEBSOCKET_IO_TIMEOUT,
                ).await {
                    WebsocketSendOutcome::Sent => {}
                    WebsocketSendOutcome::Failed => break WebsocketExit::PingSendError,
                    WebsocketSendOutcome::TimedOut => break WebsocketExit::PingSendTimeout,
                }
                // Start the timeout only after a ping was actually sent. A delayed
                // timer must not turn a scheduler pause into a reconnect storm.
                if unanswered_since.is_none_or(|sent| last_received_at >= sent) {
                    unanswered_since = Some(tokio::time::Instant::now());
                }
            }
        }
    };
    finish_websocket_loop(socket, message_sender, resources, exit_guard, exit_reason).await;
}

/// Ends one WebSocket loop: records the exit reason, sends the service-restart
/// close frame when draining, then drops the owned resources in the order that
/// keeps session cleanup after message-worker cancellation.
async fn finish_websocket_loop(
    mut socket: WebSocket,
    message_sender: mpsc::Sender<(Value, Vec<OwnedSemaphorePermit>)>,
    resources: WebsocketLoopResources,
    exit_guard: WebsocketExitGuard<'static>,
    exit_reason: WebsocketExit,
) {
    exit_guard.finish(exit_reason);
    if matches!(exit_reason, WebsocketExit::ServiceRestart) {
        // Tell the client this is an operator-initiated restart so it can
        // reconnect immediately instead of treating the drop as a failure.
        let close = Message::Close(Some(CloseFrame {
            code: WEBSOCKET_CLOSE_SERVICE_RESTART,
            reason: "service restart".into(),
        }));
        let _ = tokio::time::timeout(WEBSOCKET_IO_TIMEOUT, socket.send(close)).await;
    }
    drop(message_sender);
    drop(socket);
    drop(resources);
}

async fn buffered_activity_revives_expired_websocket(
    socket: &mut WebSocket,
    message_sender: &mpsc::Sender<(Value, Vec<OwnedSemaphorePermit>)>,
    byte_budget: &WebsocketBudget,
    last_received_at: &mut tokio::time::Instant,
    idle_timeout: Duration,
) -> bool {
    // Frames already buffered by the transport are observed by the pinned
    // listener even when the timer becomes ready in the same scheduler turn.
    // Drain bounded ready work until activity revives the session or no frame
    // remains ready. The cap prevents an infinite ready Ping flood from
    // monopolizing the WebSocket task.
    let mut reconciliation = ExpiryReconciliation::new();
    while reconciliation.can_poll() {
        let Some(inbound) = take_ready(socket.recv()) else {
            return false;
        };
        let Some(Ok(inbound)) = inbound else {
            return false;
        };
        let received_at = tokio::time::Instant::now();
        let revived = reconciliation.observe(&inbound, last_received_at, received_at, idle_timeout);
        if !handle_websocket_inbound(socket, message_sender, byte_budget, inbound).await {
            return false;
        }
        if revived {
            return true;
        }
    }
    false
}

type RealtimeMessageWorker = Pin<Box<dyn Future<Output = ()> + Send>>;

struct WebsocketLoopResources {
    message_worker: Option<RealtimeMessageWorker>,
    session: Option<RuntimeRealtimeSession>,
}

impl WebsocketLoopResources {
    fn new(message_worker: RealtimeMessageWorker, session: RuntimeRealtimeSession) -> Self {
        Self {
            message_worker: Some(message_worker),
            session: Some(session),
        }
    }
}

impl Drop for WebsocketLoopResources {
    fn drop(&mut self) {
        // Dropping the directly pinned future synchronously cancels blocked
        // backend work. Session cleanup must run only after that cancellation,
        // including when the parent WebSocket task itself is aborted.
        drop(self.message_worker.take());
        drop(self.session.take());
    }
}

async fn handle_websocket_inbound(
    socket: &mut WebSocket,
    message_sender: &mpsc::Sender<(Value, Vec<OwnedSemaphorePermit>)>,
    byte_budget: &WebsocketBudget,
    inbound: Message,
) -> bool {
    let bytes = match &inbound {
        Message::Text(text) => text.as_bytes(),
        Message::Binary(bytes) => bytes.as_ref(),
        Message::Close(_) => {
            // Tungstenite has already queued the close reply on receipt. A
            // second send is rejected after closing starts and leaves that
            // reply unflushed, which browsers report as an abnormal close.
            let _close_was_flushed =
                tokio::time::timeout(WEBSOCKET_IO_TIMEOUT, socket.flush()).await;
            return false;
        }
        Message::Ping(payload) => {
            return send_websocket_message(
                socket.send(Message::Pong(payload.clone())),
                WEBSOCKET_IO_TIMEOUT,
            )
            .await
                == WebsocketSendOutcome::Sent;
        }
        Message::Pong(_) => return true,
    };
    // Charge wire bytes before decoding, including the message currently handled.
    let Ok(permit) = Arc::clone(&byte_budget.wire)
        .try_acquire_many_owned(u32::try_from(bytes.len()).expect("transport caps message size"))
    else {
        close_websocket_receive_overload(socket).await;
        return false;
    };
    let charge = decoded_json_charge(bytes);
    if charge > WEBSOCKET_DECODED_MAX_BYTES {
        close_websocket_receive_overload(socket).await;
        return false;
    }
    let Ok(decoded_permit) = Arc::clone(&byte_budget.decoded)
        .try_acquire_many_owned(u32::try_from(charge).expect("decoded budget caps message size"))
    else {
        close_websocket_receive_overload(socket).await;
        return false;
    };
    let value = serde_json::from_slice(bytes);
    let Ok(value) = value else { return true };
    match message_sender.try_send((value, vec![permit, decoded_permit])) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            close_websocket_receive_overload(socket).await;
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

async fn close_websocket_receive_overload(socket: &mut WebSocket) {
    let frame = CloseFrame {
        code: close_code::AGAIN,
        reason: "Realtime receive queue is full".into(),
    };
    let _close_was_sent = tokio::time::timeout(
        WEBSOCKET_IO_TIMEOUT,
        socket.send(Message::Close(Some(frame))),
    )
    .await;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WebsocketSendOutcome {
    Sent,
    Failed,
    TimedOut,
}

async fn send_websocket_message<F, E>(send: F, timeout: Duration) -> WebsocketSendOutcome
where
    F: Future<Output = Result<(), E>>,
{
    match tokio::time::timeout(timeout, send).await {
        Ok(Ok(())) => WebsocketSendOutcome::Sent,
        Ok(Err(_)) => WebsocketSendOutcome::Failed,
        Err(_) => WebsocketSendOutcome::TimedOut,
    }
}

fn take_ready<F>(future: F) -> Option<F::Output>
where
    F: Future,
{
    future.now_or_never()
}

const fn websocket_message_records_activity(message: &Message) -> bool {
    matches!(
        message,
        Message::Text(_) | Message::Binary(_) | Message::Pong(_)
    )
}

fn record_websocket_activity(
    message: &Message,
    last_received_at: &mut tokio::time::Instant,
    now: tokio::time::Instant,
) {
    if websocket_message_records_activity(message) {
        *last_received_at = now;
    }
}

fn reconcile_expired_websocket_frame(
    message: &Message,
    last_received_at: &mut tokio::time::Instant,
    received_at: tokio::time::Instant,
    idle_timeout: Duration,
) -> bool {
    record_websocket_activity(message, last_received_at, received_at);
    !websocket_idle_expired(received_at, *last_received_at, idle_timeout)
}

struct ExpiryReconciliation {
    remaining: usize,
}

impl ExpiryReconciliation {
    const fn new() -> Self {
        Self {
            remaining: WEBSOCKET_EXPIRY_DRAIN_LIMIT,
        }
    }

    const fn can_poll(&self) -> bool {
        self.remaining != 0
    }

    fn observe(
        &mut self,
        message: &Message,
        last_received_at: &mut tokio::time::Instant,
        received_at: tokio::time::Instant,
        idle_timeout: Duration,
    ) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        reconcile_expired_websocket_frame(message, last_received_at, received_at, idle_timeout)
    }
}

fn unanswered_ping_expired(
    now: tokio::time::Instant,
    last_received_at: tokio::time::Instant,
    unanswered_since: Option<tokio::time::Instant>,
    idle_timeout: Duration,
) -> bool {
    unanswered_since.is_some_and(|sent| {
        last_received_at < sent && websocket_idle_expired(now, sent, idle_timeout)
    })
}

fn websocket_idle_expired(
    now: tokio::time::Instant,
    last_received_at: tokio::time::Instant,
    idle_timeout: Duration,
) -> bool {
    now.duration_since(last_received_at) > idle_timeout
}

async fn websocket_message_worker<B>(
    backend: Arc<B>,
    session_id: Uuid,
    mut messages: mpsc::Receiver<(Value, Vec<OwnedSemaphorePermit>)>,
) where
    B: RuntimeRealtimeBackend,
{
    while let Some((message, _byte_permit)) = messages.recv().await {
        if backend.message(session_id, message).await.is_err() {
            break;
        }
    }
}

async fn sse_open<B>(
    State((backend, _liveness)): State<(Arc<B>, WebsocketLiveness)>,
    Query(params): Query<AppParams>,
    client_ip: Option<Extension<crate::ResolvedClientIp>>,
    headers: HeaderMap,
) -> Result<Response, ApiError>
where
    B: RuntimeRealtimeBackend,
{
    let app_id = params.app_id.ok_or_else(|| missing("[params app_id]"))?;
    if backend.drain_signal().is_draining() {
        return Ok(draining_response());
    }
    let session = backend
        .open_with_client(
            Some(app_id),
            RuntimeTransport::ServerSentEvents,
            request_origin(&headers),
            client_ip.and_then(|Extension(client_ip)| client_ip.0),
        )
        .await?;
    let retry_interval = session.retry_interval();
    let drain = session.drain_signal();
    let events = stream::unfold(
        (session, drain, true),
        move |(mut session, drain, first)| async move {
            // A restart ends the stream instead of cutting it off, so the
            // client reconnects deliberately rather than seeing a transport
            // failure.
            let value = tokio::select! {
                value = session.recv() => value?,
                () = drain.drained() => return None,
            };
            let mut event = Event::default().data(value.to_string());
            if first {
                event = event.retry(retry_interval);
            }
            Some((Ok::<Event, Infallible>(event), (session, drain, false)))
        },
    );
    Ok(Sse::new(events)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response())
}

fn request_origin(headers: &HeaderMap) -> Option<String> {
    headers
        .get("origin")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[derive(Debug, Deserialize)]
struct PushBody {
    machine_id: Uuid,
    session_id: Uuid,
    sse_token: Uuid,
    messages: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct PushCredentials {
    machine_id: Uuid,
    session_id: Uuid,
    sse_token: Uuid,
}

struct NdjsonPushParser {
    credentials: Option<PushCredentials>,
    messages: Vec<Value>,
    line: Vec<u8>,
    total_bytes: usize,
}

impl NdjsonPushParser {
    fn new() -> Self {
        Self {
            credentials: None,
            messages: Vec::new(),
            line: Vec::new(),
            total_bytes: 0,
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> Result<(), ApiError> {
        self.total_bytes = self
            .total_bytes
            .checked_add(chunk.len())
            .filter(|size| *size <= SSE_PUSH_MAX_BYTES)
            .ok_or_else(malformed_ndjson)?;

        let mut start = 0;
        while let Some(offset) = chunk[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            self.line.extend_from_slice(&chunk[start..end]);
            self.push_line()?;
            start = end + 1;
        }
        self.line.extend_from_slice(&chunk[start..]);
        if self.line.len() > SSE_PUSH_MAX_BYTES {
            return Err(malformed_ndjson());
        }
        Ok(())
    }

    fn push_line(&mut self) -> Result<(), ApiError> {
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            return Ok(());
        }
        if self.credentials.is_some() {
            if self.messages.len() == SSE_PUSH_MAX_MESSAGES {
                return Err(malformed_ndjson());
            }
            self.messages
                .push(serde_json::from_slice(&line).map_err(|_| malformed_ndjson())?);
        } else {
            self.credentials = Some(serde_json::from_slice(&line).map_err(|_| malformed_ndjson())?);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<PushBody, ApiError> {
        if !self.line.is_empty() {
            self.push_line()?;
        }
        let credentials = self.credentials.ok_or_else(malformed_ndjson)?;
        Ok(PushBody {
            machine_id: credentials.machine_id,
            session_id: credentials.session_id,
            sse_token: credentials.sse_token,
            messages: self.messages,
        })
    }
}

fn malformed_ndjson() -> ApiError {
    ApiError::new(ApiErrorType::ParamMalformed, "Malformed NDJSON body")
}

fn is_ndjson(headers: &HeaderMap) -> bool {
    headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split_once(';')
                .map_or(value, |(media_type, _)| media_type)
                .trim()
                .eq_ignore_ascii_case(SSE_PUSH_NDJSON_CONTENT_TYPE)
        })
}

async fn parse_ndjson_push(body: Body) -> Result<PushBody, ApiError> {
    let mut parser = NdjsonPushParser::new();
    let mut chunks = body.into_data_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|_| malformed_ndjson())?;
        parser.push_chunk(&chunk)?;
    }
    parser.finish()
}

async fn parse_json_push(body: Body) -> Result<PushBody, ApiError> {
    let body = axum::body::to_bytes(body, SSE_PUSH_MAX_BYTES)
        .await
        .map_err(|_| ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body"))?;
    serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(ApiErrorType::ParamMalformed, "Malformed JSON body"))
}

async fn sse_push<B>(
    State((backend, _liveness)): State<(Arc<B>, WebsocketLiveness)>,
    Query(params): Query<AppParams>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError>
where
    B: RuntimeRealtimeBackend,
{
    let app_id = params.app_id.ok_or_else(|| missing("[params app_id]"))?;
    let body = if is_ndjson(&headers) {
        parse_ndjson_push(body).await?
    } else {
        parse_json_push(body).await?
    };
    backend
        .push(RuntimeSsePushRequest {
            app_id,
            machine_id: body.machine_id,
            session_id: body.session_id,
            sse_token: body.sse_token,
            messages: body.messages,
        })
        .await?;
    Ok((
        [
            (SSE_PUSH_FORMAT_HEADER, "ndjson"),
            (SSE_PUSH_EXPOSE_HEADERS, SSE_PUSH_FORMAT_HEADER),
        ],
        Json(serde_json::json!({})),
    )
        .into_response())
}

fn missing(location: &str) -> ApiError {
    ApiError::new(
        ApiErrorType::ParamMissing,
        format!("Missing parameter: {location}"),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use axum::body::{Body, Bytes, to_bytes};
    use axum::http::{Request, StatusCode};
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::Semaphore;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as ClientMessage;
    use tower::ServiceExt;

    use super::*;

    struct FakeBackend {
        pushes: Mutex<Vec<RuntimeSsePushRequest>>,
        origins: Mutex<Vec<Option<String>>>,
        websocket_senders: Mutex<Vec<mpsc::Sender<Value>>>,
        messages: Mutex<Vec<Value>>,
        block_messages: AtomicBool,
        hold_sse_open: AtomicBool,
        release_messages_on_cleanup: Arc<AtomicBool>,
        message_started: Arc<Semaphore>,
        message_release: Arc<Semaphore>,
        cleanups: Arc<AtomicUsize>,
        lifecycle: Arc<Mutex<Vec<&'static str>>>,
        sse_senders: Mutex<Vec<mpsc::Sender<Value>>>,
        drain: RuntimeDrainSignal,
        session_drains: Mutex<Vec<RuntimeDrainSignal>>,
    }

    impl Default for FakeBackend {
        fn default() -> Self {
            Self {
                pushes: Mutex::new(Vec::new()),
                origins: Mutex::new(Vec::new()),
                websocket_senders: Mutex::new(Vec::new()),
                messages: Mutex::new(Vec::new()),
                block_messages: AtomicBool::new(false),
                hold_sse_open: AtomicBool::new(false),
                release_messages_on_cleanup: Arc::new(AtomicBool::new(true)),
                message_started: Arc::new(Semaphore::new(0)),
                message_release: Arc::new(Semaphore::new(0)),
                cleanups: Arc::new(AtomicUsize::new(0)),
                lifecycle: Arc::new(Mutex::new(Vec::new())),
                sse_senders: Mutex::new(Vec::new()),
                drain: RuntimeDrainSignal::default(),
                session_drains: Mutex::new(Vec::new()),
            }
        }
    }

    impl RuntimeRealtimeBackend for FakeBackend {
        async fn open(
            &self,
            app_id: Option<Uuid>,
            transport: RuntimeTransport,
        ) -> Result<RuntimeRealtimeSession, ApiError> {
            let session_id = Uuid::new_v4();
            let (sender, receiver) = mpsc::channel(1);
            sender
                .send(serde_json::json!({
                    "op": "sse-init",
                    "session-id": session_id,
                    "app-id": app_id,
                }))
                .await
                .unwrap();
            if transport == RuntimeTransport::WebSocket {
                self.websocket_senders.lock().unwrap().push(sender);
            } else if self.hold_sse_open.load(Ordering::Acquire) {
                self.sse_senders.lock().unwrap().push(sender);
            } else {
                drop(sender);
            }
            let cleanups = Arc::clone(&self.cleanups);
            let release = Arc::clone(&self.message_release);
            let release_messages_on_cleanup = Arc::clone(&self.release_messages_on_cleanup);
            let lifecycle = Arc::clone(&self.lifecycle);
            let session_drain = RuntimeDrainSignal::default();
            self.session_drains
                .lock()
                .unwrap()
                .push(session_drain.clone());
            Ok(RuntimeRealtimeSession::new(session_id, receiver, move || {
                lifecycle.lock().unwrap().push("session-cleanup");
                cleanups.fetch_add(1, Ordering::SeqCst);
                if release_messages_on_cleanup.load(Ordering::Acquire) {
                    release.add_permits(1);
                }
            })
            .with_retry_interval(Duration::from_millis(1_250))
            .with_drain_signal(session_drain))
        }

        fn drain_signal(&self) -> RuntimeDrainSignal {
            self.drain.clone()
        }

        fn open_with_origin(
            &self,
            app_id: Option<Uuid>,
            transport: RuntimeTransport,
            origin: Option<String>,
        ) -> impl Future<Output = Result<RuntimeRealtimeSession, ApiError>> + Send {
            self.origins.lock().unwrap().push(origin);
            self.open(app_id, transport)
        }

        async fn message(&self, _session_id: Uuid, message: Value) -> Result<(), ApiError> {
            self.messages.lock().unwrap().push(message);
            if self.block_messages.load(Ordering::Acquire) {
                let _drop_guard = MessageDropGuard(Arc::clone(&self.lifecycle));
                self.message_started.add_permits(1);
                let permit = self.message_release.acquire().await.unwrap();
                permit.forget();
            }
            Ok(())
        }

        fn push(
            &self,
            request: RuntimeSsePushRequest,
        ) -> impl Future<Output = Result<(), ApiError>> + Send {
            self.pushes.lock().unwrap().push(request);
            std::future::ready(Ok(()))
        }
    }

    struct MessageDropGuard(Arc<Mutex<Vec<&'static str>>>);

    impl Drop for MessageDropGuard {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("message-drop");
        }
    }

    #[tokio::test]
    async fn sse_open_emits_init_and_push_preserves_transport_credentials() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = runtime_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .uri(format!("/runtime/sse?app_id={app_id}"))
                    .header("origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            backend.origins.lock().unwrap().as_slice(),
            &[Some("https://app.example.com".to_owned())]
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("retry: 1250"));
        assert!(body.contains("data: {\"app-id\""));

        let session_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let response = runtime_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runtime/sse?app_id={app_id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "machine_id": machine_id,
                            "session_id": session_id,
                            "sse_token": token,
                            "messages": [{"op": "init", "app-id": app_id}],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(SSE_PUSH_FORMAT_HEADER).unwrap(),
            "ndjson"
        );
        assert_eq!(
            response.headers().get(SSE_PUSH_EXPOSE_HEADERS).unwrap(),
            SSE_PUSH_FORMAT_HEADER
        );
        let push = &backend.pushes.lock().unwrap()[0];
        assert_eq!(push.machine_id, machine_id);
        assert_eq!(push.sse_token, token);
        assert_eq!(
            push.messages,
            vec![serde_json::json!({"op": "init", "app-id": app_id})]
        );
    }

    #[tokio::test]
    async fn sse_push_accepts_ndjson_split_across_arbitrary_body_chunks_in_order() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let machine_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let token = Uuid::new_v4();
        let payload = format!(
            "{{\"machine_id\":\"{machine_id}\",\"session_id\":\"{session_id}\",\"sse_token\":\"{token}\"}}\n{{\"index\":0}}\n{{\"index\":1}}\n"
        );
        let chunks = payload
            .as_bytes()
            .chunks(7)
            .map(|chunk| Ok::<_, Infallible>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        let response = runtime_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runtime/sse?app_id={app_id}"))
                    .header("content-type", "application/x-ndjson; charset=utf-8")
                    .body(Body::from_stream(futures_util::stream::iter(chunks)))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            backend.pushes.lock().unwrap()[0],
            RuntimeSsePushRequest {
                app_id,
                machine_id,
                session_id,
                sse_token: token,
                messages: vec![
                    serde_json::json!({"index": 0}),
                    serde_json::json!({"index": 1})
                ],
            }
        );
    }

    #[tokio::test]
    async fn sse_push_rejects_malformed_ndjson_without_dispatching() {
        let backend = Arc::new(FakeBackend::default());
        let app_id = Uuid::new_v4();
        let response = runtime_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/runtime/sse?app_id={app_id}"))
                    .header("content-type", SSE_PUSH_NDJSON_CONTENT_TYPE)
                    .body(Body::from("{not-json}\n"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "param-malformed");
        assert_eq!(body["message"], "Malformed NDJSON body");
        assert!(backend.pushes.lock().unwrap().is_empty());
    }

    #[test]
    fn decoded_admission_bounds_dense_json_before_allocation() {
        let dense = format!("[{}0]", "0,".repeat(70_000));
        assert!(dense.len() < WEBSOCKET_MAX_BYTES);
        assert!(decoded_json_charge(dense.as_bytes()) > WEBSOCKET_DECODED_MAX_BYTES);
        let text = serde_json::to_vec(&"[,{}:]\\\"".repeat(1_000)).unwrap();
        assert_eq!(decoded_json_charge(&text), text.len() * 4 + 256);
        let exact_wire_limit = format!("\"{}\"", "x".repeat(WEBSOCKET_MAX_BYTES - 2));
        assert!(decoded_json_charge(exact_wire_limit.as_bytes()) < WEBSOCKET_DECODED_MAX_BYTES);
    }

    #[tokio::test]
    async fn websocket_message_worker_preserves_receive_order() {
        let backend = Arc::new(FakeBackend::default());
        let (sender, receiver) = mpsc::channel(3);
        let byte_budget = Arc::new(Semaphore::new(3));
        let worker = tokio::spawn(websocket_message_worker(
            Arc::clone(&backend),
            Uuid::new_v4(),
            receiver,
        ));
        for index in 0..3 {
            sender
                .send((
                    serde_json::json!({"index": index}),
                    vec![Arc::clone(&byte_budget).try_acquire_owned().unwrap()],
                ))
                .await
                .unwrap();
        }
        drop(sender);
        worker.await.unwrap();
        assert_eq!(
            *backend.messages.lock().unwrap(),
            vec![
                serde_json::json!({"index": 0}),
                serde_json::json!({"index": 1}),
                serde_json::json!({"index": 2}),
            ]
        );
    }

    #[tokio::test]
    async fn websocket_byte_permits_release_after_completion_and_cancellation() {
        let backend = Arc::new(FakeBackend::default());
        backend.block_messages.store(true, Ordering::Release);
        let budget = Arc::new(Semaphore::new(2));
        let (sender, receiver) = mpsc::channel(2);
        for _ in 0..2 {
            sender
                .send((
                    serde_json::json!({"op": "slow"}),
                    vec![Arc::clone(&budget).try_acquire_owned().unwrap()],
                ))
                .await
                .unwrap();
        }
        let worker = tokio::spawn(websocket_message_worker(
            Arc::clone(&backend),
            Uuid::new_v4(),
            receiver,
        ));
        tokio::time::timeout(Duration::from_secs(2), backend.message_started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(budget.available_permits(), 0);
        backend.message_release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), backend.message_started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        assert_eq!(budget.available_permits(), 1);
        // Cancellation releases both in-flight and still-queued reservations.
        sender
            .send((
                serde_json::json!({"op": "queued"}),
                vec![Arc::clone(&budget).try_acquire_owned().unwrap()],
            ))
            .await
            .unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert_eq!(budget.available_permits(), 2);
    }

    #[tokio::test]
    async fn websocket_ping_and_close_remain_responsive_during_a_blocked_handler() {
        let backend = Arc::new(FakeBackend::default());
        backend.block_messages.store(true, Ordering::Release);
        backend
            .release_messages_on_cleanup
            .store(false, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Text(_)
        ));
        socket
            .send(ClientMessage::Text(
                serde_json::json!({"op": "slow"}).to_string().into(),
            ))
            .await
            .unwrap();
        let started = backend.message_started.acquire().await.unwrap();
        started.forget();

        socket
            .send(ClientMessage::Ping(b"probe".as_slice().into()))
            .await
            .unwrap();
        let pong = tokio::time::timeout(Duration::from_millis(250), socket.next())
            .await
            .expect("blocked application handler must not block ping")
            .unwrap()
            .unwrap();
        assert_eq!(pong, ClientMessage::Pong(b"probe".as_slice().into()));

        socket.send(ClientMessage::Close(None)).await.unwrap();
        let close = tokio::time::timeout(Duration::from_millis(250), socket.next())
            .await
            .expect("blocked application handler must not block close")
            .expect("server must reply to the close handshake")
            .expect("close handshake must not reset the transport");
        assert_eq!(close, ClientMessage::Close(None));
        tokio::time::timeout(Duration::from_secs(1), async {
            while backend.cleanups.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            backend.lifecycle.lock().unwrap().as_slice(),
            &["message-drop", "session-cleanup"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn websocket_exit_counters_observe_peer_close_and_backend_stream_end() {
        fn observed(reason: &str) -> u64 {
            runtime_websocket_exit_metrics()
                .into_iter()
                .find(|(label, _)| *label == reason)
                .unwrap()
                .1
        }
        let backend = Arc::new(FakeBackend::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let before_peer_close = observed("peer-close");
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Text(_)
        ));
        socket.send(ClientMessage::Close(None)).await.unwrap();
        assert_eq!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Close(None)
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while backend.cleanups.load(Ordering::SeqCst) < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(observed("peer-close") > before_peer_close);
        drop(socket);

        let before_stream_end = observed("outbound-stream-ended");
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Text(_)
        ));
        backend.websocket_senders.lock().unwrap().clear();
        let _terminal = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while backend.cleanups.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(observed("outbound-stream-ended") > before_stream_end);
        server.abort();
    }

    #[tokio::test]
    async fn websocket_rejects_oversized_text() {
        assert_websocket_rejects_oversized(false, false).await;
    }

    #[tokio::test]
    async fn websocket_rejects_oversized_binary() {
        assert_websocket_rejects_oversized(true, false).await;
    }

    #[tokio::test]
    async fn websocket_rejects_oversized_fragmented_text() {
        assert_websocket_rejects_oversized(false, true).await;
    }

    #[tokio::test]
    async fn websocket_rejects_oversized_fragmented_binary() {
        assert_websocket_rejects_oversized(true, true).await;
    }

    async fn assert_websocket_rejects_oversized(binary: bool, fragmented: bool) {
        use tokio_tungstenite::tungstenite::protocol::frame::{
            Frame,
            coding::{Data, OpCode},
        };

        let backend = Arc::new(FakeBackend::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        let payload = format!("\"{}\"", "x".repeat(SSE_PUSH_MAX_BYTES - 1));
        let frames = if fragmented {
            let split = payload.len() / 2;
            vec![
                ClientMessage::Frame(Frame::message(
                    payload.as_bytes()[..split].to_vec(),
                    OpCode::Data(if binary { Data::Binary } else { Data::Text }),
                    false,
                )),
                ClientMessage::Frame(Frame::message(
                    payload.as_bytes()[split..].to_vec(),
                    OpCode::Data(Data::Continue),
                    true,
                )),
            ]
        } else if binary {
            vec![ClientMessage::Binary(payload.into_bytes().into())]
        } else {
            vec![ClientMessage::Text(payload.into())]
        };
        for frame in frames {
            if socket.send(frame).await.is_err() {
                break;
            }
        }
        let terminal = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
        server.abort();
        assert!(
            matches!(
                terminal,
                Ok(None | Some(Err(_) | Ok(ClientMessage::Close(_))))
            ),
            "oversized payload must terminate the transport: {terminal:?}"
        );
        assert!(backend.messages.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn websocket_accepts_size_boundary_and_releases_invalid_json_bytes() {
        let backend = Arc::new(FakeBackend::default());
        backend.block_messages.store(true, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        socket
            .send(ClientMessage::Text("!".repeat(SSE_PUSH_MAX_BYTES).into()))
            .await
            .unwrap();
        let payload = format!("\"{}\"", "x".repeat(SSE_PUSH_MAX_BYTES - 2));
        socket
            .send(ClientMessage::Binary(payload.into_bytes().into()))
            .await
            .unwrap();
        let started =
            tokio::time::timeout(Duration::from_secs(2), backend.message_started.acquire()).await;
        server.abort();
        started
            .expect("exact-limit payload must be accepted after invalid JSON")
            .unwrap()
            .forget();
        assert_eq!(backend.messages.lock().unwrap().len(), 1);
        socket.send(ClientMessage::Close(None)).await.unwrap();
    }

    #[tokio::test]
    async fn websocket_byte_budget_includes_blocked_handler_and_closes_on_burst() {
        let backend = Arc::new(FakeBackend::default());
        backend.block_messages.store(true, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        let payload = format!("\"{}\"", "x".repeat(SSE_PUSH_MAX_BYTES / 2 - 2));
        socket
            .send(ClientMessage::Text(payload.clone().into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), backend.message_started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        socket
            .send(ClientMessage::Binary(payload.into_bytes().into()))
            .await
            .unwrap();
        // Even malformed JSON must reserve bytes before decoding.
        socket.send(ClientMessage::Text("!".into())).await.unwrap();
        let close = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
        server.abort();
        let ClientMessage::Close(Some(frame)) = close
            .expect("byte overflow must close below the message-count limit")
            .unwrap()
            .unwrap()
        else {
            panic!("expected overload close");
        };
        assert_eq!(u16::from(frame.code), close_code::AGAIN);
        assert_eq!(frame.reason, "Realtime receive queue is full");
        assert_eq!(backend.messages.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn websocket_decoded_budget_rejects_dense_payload_before_backend_work() {
        let backend = Arc::new(FakeBackend::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        let payload = format!("[{}0]", "0,".repeat(70_000));
        assert!(payload.len() < WEBSOCKET_MAX_BYTES);
        socket
            .send(ClientMessage::Text(payload.into()))
            .await
            .unwrap();
        let close = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
        server.abort();
        let ClientMessage::Close(Some(frame)) = close.unwrap().unwrap().unwrap() else {
            panic!("expected decoded-budget overload close");
        };
        assert_eq!(u16::from(frame.code), close_code::AGAIN);
        assert!(backend.messages.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn websocket_decoded_budget_includes_in_flight_and_queued_values() {
        let backend = Arc::new(FakeBackend::default());
        backend.block_messages.store(true, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        socket.next().await.unwrap().unwrap();
        let payload = format!("[{}0]", "0,".repeat(25_000));
        let charge = decoded_json_charge(payload.as_bytes());
        assert!(charge * 2 <= WEBSOCKET_DECODED_MAX_BYTES);
        assert!(charge * 3 > WEBSOCKET_DECODED_MAX_BYTES);
        assert!(payload.len() * 3 < WEBSOCKET_INBOUND_MAX_BYTES);
        socket
            .send(ClientMessage::Text(payload.clone().into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), backend.message_started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        for _ in 0..2 {
            socket
                .send(ClientMessage::Binary(payload.clone().into_bytes().into()))
                .await
                .unwrap();
        }
        let close = tokio::time::timeout(Duration::from_secs(2), socket.next()).await;
        server.abort();
        let ClientMessage::Close(Some(frame)) = close.unwrap().unwrap().unwrap() else {
            panic!("expected aggregate decoded-budget overload close");
        };
        assert_eq!(u16::from(frame.code), close_code::AGAIN);
        assert_eq!(backend.messages.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn websocket_receive_buffer_is_bounded_and_closes_when_full() {
        let backend = Arc::new(FakeBackend::default());
        backend.block_messages.store(true, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Text(_)
        ));
        socket
            .send(ClientMessage::Text(
                serde_json::json!({"index": 0}).to_string().into(),
            ))
            .await
            .unwrap();
        let started = backend.message_started.acquire().await.unwrap();
        started.forget();
        for index in 1..=(WEBSOCKET_INBOUND_LIMIT + 1) {
            socket
                .send(ClientMessage::Text(
                    serde_json::json!({"index": index}).to_string().into(),
                ))
                .await
                .unwrap();
        }
        let close = tokio::time::timeout(Duration::from_secs(1), socket.next())
            .await
            .expect("queue overflow must close the websocket")
            .unwrap()
            .unwrap();
        let ClientMessage::Close(Some(frame)) = close else {
            panic!("expected close frame, got {close:?}");
        };
        assert_eq!(u16::from(frame.code), close_code::AGAIN);
        server.abort();
    }

    #[test]
    fn websocket_liveness_matches_the_pinned_activity_and_strict_timeout_boundary() {
        // Pinned oracle: instantdb/instant@4e0a1bd8
        // server/src/instant/lib/ring/websocket.clj:55-76,93-106.
        assert!(websocket_message_records_activity(&Message::Text(
            "not-json".into()
        )));
        assert!(websocket_message_records_activity(&Message::Binary(
            b"not-json".to_vec().into()
        )));
        assert!(websocket_message_records_activity(&Message::Pong(
            Vec::new().into()
        )));
        assert!(!websocket_message_records_activity(&Message::Ping(
            Vec::new().into()
        )));
        assert!(!websocket_message_records_activity(&Message::Close(None)));

        let last_received_at = tokio::time::Instant::now();
        assert!(!websocket_idle_expired(
            last_received_at + Duration::from_secs(15),
            last_received_at,
            Duration::from_secs(15),
        ));
        assert!(websocket_idle_expired(
            last_received_at + Duration::from_secs(15) + Duration::from_nanos(1),
            last_received_at,
            Duration::from_secs(15),
        ));
    }

    #[test]
    fn delayed_ping_does_not_expire_a_client_before_it_can_answer() {
        let start = tokio::time::Instant::now();
        let delayed = start + Duration::from_secs(60);
        let timeout = Duration::from_secs(15);
        assert!(!unanswered_ping_expired(delayed, start, None, timeout));
        assert!(!unanswered_ping_expired(
            delayed + timeout,
            start,
            Some(delayed),
            timeout
        ));
        assert!(unanswered_ping_expired(
            delayed + timeout + Duration::from_nanos(1),
            start,
            Some(delayed),
            timeout
        ));
        assert!(!unanswered_ping_expired(
            delayed + timeout + Duration::from_secs(1),
            delayed,
            Some(delayed),
            timeout
        ));
    }

    #[test]
    fn buffered_activity_behind_ping_wins_the_expiry_boundary_with_a_bounded_drain() {
        fn reconciles(frames: impl IntoIterator<Item = Message>) -> bool {
            let original_activity = tokio::time::Instant::now();
            let due_tick = original_activity + Duration::from_secs(20);
            let mut last_received_at = original_activity;
            let mut reconciliation = ExpiryReconciliation::new();
            for frame in frames {
                if !reconciliation.can_poll() {
                    return false;
                }
                if reconciliation.observe(
                    &frame,
                    &mut last_received_at,
                    due_tick,
                    Duration::from_secs(15),
                ) {
                    return true;
                }
            }
            false
        }

        assert!(reconciles([
            Message::Ping(Vec::new().into()),
            Message::Pong(Vec::new().into()),
        ]));
        assert!(reconciles([
            Message::Ping(Vec::new().into()),
            Message::Text("not-json-still-counts-as-activity".into()),
        ]));
        assert!(reconciles([
            Message::Ping(Vec::new().into()),
            Message::Binary(b"not-json-still-counts-as-activity".to_vec().into()),
        ]));
        assert!(!reconciles([Message::Ping(Vec::new().into())]));

        let mut capped_ping_flood = (0..WEBSOCKET_EXPIRY_DRAIN_LIMIT)
            .map(|_| Message::Ping(Vec::new().into()))
            .collect::<Vec<_>>();
        capped_ping_flood.push(Message::Pong(Vec::new().into()));
        assert!(!reconciles(capped_ping_flood));
    }

    #[tokio::test]
    async fn parent_task_abort_drops_backend_work_before_session_cleanup() {
        struct PendingMessage(Arc<Mutex<Vec<&'static str>>>);

        impl Future for PendingMessage {
            type Output = ();

            fn poll(
                self: Pin<&mut Self>,
                _context: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Self::Output> {
                std::task::Poll::Pending
            }
        }

        impl Drop for PendingMessage {
            fn drop(&mut self) {
                self.0.lock().unwrap().push("message-drop");
            }
        }

        let lifecycle = Arc::new(Mutex::new(Vec::new()));
        let session_lifecycle = Arc::clone(&lifecycle);
        let (_sender, receiver) = mpsc::channel(1);
        let session = RuntimeRealtimeSession::new(Uuid::new_v4(), receiver, move || {
            session_lifecycle.lock().unwrap().push("session-cleanup");
        });
        let worker: RealtimeMessageWorker = Box::pin(PendingMessage(Arc::clone(&lifecycle)));
        let task = tokio::spawn(async move {
            let _resources = WebsocketLoopResources::new(worker, session);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(
            lifecycle.lock().unwrap().as_slice(),
            &["message-drop", "session-cleanup"]
        );
    }

    #[tokio::test]
    async fn websocket_send_failures_and_timeouts_are_terminal_outcomes() {
        assert_eq!(
            send_websocket_message(std::future::ready(Ok::<(), ()>(())), Duration::from_secs(1))
                .await,
            WebsocketSendOutcome::Sent
        );
        assert_eq!(
            send_websocket_message(
                std::future::ready(Err::<(), ()>(())),
                Duration::from_secs(1)
            )
            .await,
            WebsocketSendOutcome::Failed
        );
        assert_eq!(
            send_websocket_message(
                std::future::pending::<Result<(), ()>>(),
                Duration::from_millis(1),
            )
            .await,
            WebsocketSendOutcome::TimedOut
        );
    }

    #[tokio::test]
    async fn silent_websocket_closes_promptly_while_the_message_handler_is_blocked() {
        // Idle expiry is measured from the first unanswered ping, with a
        // strict timeout boundary. Short durations exercise the same contract.
        let liveness = WebsocketLiveness {
            ping_interval: Duration::from_millis(10),
            idle_timeout: Duration::from_millis(25),
        };
        let backend = Arc::new(FakeBackend::default());
        backend.block_messages.store(true, Ordering::Release);
        backend
            .release_messages_on_cleanup
            .store(false, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                runtime_realtime_router_with_liveness(server_backend, liveness),
            )
            .await
            .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Text(_)
        ));
        socket
            .send(ClientMessage::Text(
                serde_json::json!({"op": "slow"}).to_string().into(),
            ))
            .await
            .unwrap();
        let started = backend.message_started.acquire().await.unwrap();
        started.forget();

        tokio::time::timeout(Duration::from_millis(300), async {
            while backend.cleanups.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("idle expiry must clean up even while application work is blocked");

        tokio::time::timeout(Duration::from_millis(300), async {
            while let Some(message) = socket.next().await {
                if message.is_err() || matches!(message, Ok(ClientMessage::Close(_))) {
                    break;
                }
            }
        })
        .await
        .expect("idle expiry must close the physical websocket promptly");
        assert_eq!(backend.cleanups.load(Ordering::Acquire), 1);
        assert_eq!(
            backend.lifecycle.lock().unwrap().as_slice(),
            &["message-drop", "session-cleanup"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn websocket_server_sends_empty_liveness_pings() {
        let liveness = WebsocketLiveness {
            ping_interval: Duration::from_millis(10),
            idle_timeout: Duration::from_millis(100),
        };
        let backend = Arc::new(FakeBackend::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                runtime_realtime_router_with_liveness(server_backend, liveness),
            )
            .await
            .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Text(_)
        ));
        let ping = tokio::time::timeout(Duration::from_millis(200), socket.next())
            .await
            .expect("server should send its first liveness ping")
            .unwrap()
            .unwrap();
        assert_eq!(ping, ClientMessage::Ping(Vec::new().into()));

        socket.send(ClientMessage::Close(None)).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn websocket_liveness_configuration_does_not_expire_sse_sessions() {
        let liveness = WebsocketLiveness {
            ping_interval: Duration::from_millis(5),
            idle_timeout: Duration::from_millis(10),
        };
        let backend = Arc::new(FakeBackend::default());
        backend.hold_sse_open.store(true, Ordering::Release);
        let app_id = Uuid::new_v4();
        let response = runtime_realtime_router_with_liveness(Arc::clone(&backend), liveness)
            .oneshot(
                Request::builder()
                    .uri(format!("/runtime/sse?app_id={app_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            backend.cleanups.load(Ordering::Acquire),
            0,
            "WebSocket liveness must not install an SSE idle deadline"
        );
        drop(response);
        assert_eq!(backend.cleanups.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn draining_backend_refuses_new_websocket_and_sse_sessions() {
        let backend = Arc::new(FakeBackend::default());
        backend.drain.begin_drain();
        let app_id = Uuid::new_v4();

        let response = runtime_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .uri(format!("/runtime/sse?app_id={app_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "1");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let error = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .expect_err("a draining backend must refuse the upgrade");
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("expected an HTTP rejection, got {error:?}");
        };
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "1");
        assert!(
            backend.origins.lock().unwrap().is_empty(),
            "a draining backend must not open a session it is about to close"
        );
        server.abort();
    }

    #[tokio::test]
    async fn drain_closes_existing_websockets_with_a_service_restart_frame() {
        let backend = Arc::new(FakeBackend::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_backend = Arc::clone(&backend);
        let server = tokio::spawn(async move {
            axum::serve(listener, runtime_realtime_router(server_backend))
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/runtime/session"))
            .await
            .unwrap();
        assert!(matches!(
            socket.next().await.unwrap().unwrap(),
            ClientMessage::Text(_)
        ));
        let drain = backend.session_drains.lock().unwrap()[0].clone();
        assert!(!drain.is_draining());

        drain.begin_drain();
        let close = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("drain must close an idle websocket")
            .expect("drain must emit a close frame")
            .expect("drain close must not reset the transport");
        match close {
            ClientMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), WEBSOCKET_CLOSE_SERVICE_RESTART);
            }
            other => panic!("expected a 1012 close frame, got {other:?}"),
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while backend.cleanups.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a drained websocket must release its session");
        server.abort();
    }

    #[tokio::test]
    async fn drain_ends_sse_streams_cleanly() {
        let backend = Arc::new(FakeBackend::default());
        backend.hold_sse_open.store(true, Ordering::Release);
        let app_id = Uuid::new_v4();
        let response = runtime_realtime_router(Arc::clone(&backend))
            .oneshot(
                Request::builder()
                    .uri(format!("/runtime/sse?app_id={app_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let first = body.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("sse-init"));

        let drain = backend.session_drains.lock().unwrap()[0].clone();
        drain.begin_drain();
        let next = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("drain must end the SSE stream");
        assert!(
            next.is_none(),
            "a drained SSE stream must end instead of being cut off"
        );
        assert_eq!(backend.cleanups.load(Ordering::Acquire), 1);
    }
}
