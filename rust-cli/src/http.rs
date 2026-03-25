//! Axum HTTP REST server mirroring MCP tool operations.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::{
    Json, Router,
    extract::rejection::JsonRejection,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    response::sse::{Event, Sse},
    routing::{get, post, put},
};
use chrono::Utc;
use serde::Deserialize;
use tokio::sync::{Notify, RwLock};
use tokio_stream::wrappers::ReceiverStream;

use crate::mcp::AgentBusMcpServer;
use crate::models::MAX_HISTORY_MINUTES;
use crate::ops::{
    AckMessageRequest, MessageFilters, PostMessageRequest, PresenceRequest, ReadMessagesRequest,
    knock_metadata, list_messages_live, post_ack, post_message, set_presence,
};
use crate::output::{format_health_toon, format_message_toon, format_presence_toon};
use crate::redis_bus::{
    BatchSendPayload, RedisPool, SseSubscriberCount, bus_health, bus_list_messages_from_redis,
    bus_list_presence, bus_post_message_with_notifications,
    bus_post_messages_batch_with_notifications, list_notifications, list_notifications_since_id,
};
use crate::settings::Settings;
use crate::validation::{
    auto_fit_schema, enforce_schema_for_transport, validate_message_schema, validate_priority,
};

/// Per-agent live SSE subscriber channels.
///
/// The outer `Arc<RwLock<...>>` makes the map cheap to clone into every
/// handler.  The inner `Vec` holds one sender per active SSE connection for
/// that agent.  Disconnected senders are removed lazily when a send fails.
type AgentConnections = Arc<
    RwLock<
        HashMap<String, Vec<tokio::sync::mpsc::Sender<Result<Event, std::convert::Infallible>>>>,
    >,
>;
type ControlStatusState = Arc<RwLock<ServerControlStatus>>;

/// Shared state injected into every axum handler.
///
/// `AppState` is cheap to clone — `settings` is behind an `Arc` and `redis`
/// wraps an r2d2 pool whose `inner` field is `Arc`-backed.
///
/// `agent_connections` carries the live agent-specific SSE subscriber map so
/// that `POST /messages` can push directly to connected agents without them
/// having to poll.
///
/// `sse_subscriber_count` tracks active `GET /events` (Redis pub/sub) clients.
/// When it is zero, `bus_post_message` skips the Redis `PUBLISH` entirely,
/// saving ~3 µs per message.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) settings: Arc<Settings>,
    pub(crate) redis: RedisPool,
    /// Live SSE connections keyed by agent ID.
    pub(crate) agent_connections: AgentConnections,
    /// Count of active `GET /events` (Redis pub/sub SSE) clients.
    pub(crate) sse_subscriber_count: Arc<SseSubscriberCount>,
    /// Live server maintenance / shutdown state.
    pub(crate) control_status: ControlStatusState,
    /// Cooperative graceful-shutdown trigger for HTTP service maintenance.
    pub(crate) shutdown_signal: Arc<Notify>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServerMode {
    Running,
    Maintenance,
    Stopping,
}

impl ServerMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Maintenance => "maintenance",
            Self::Stopping => "stopping",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ServerControlStatus {
    pub(crate) mode: ServerMode,
    pub(crate) write_blocked: bool,
    pub(crate) reason: Option<String>,
    pub(crate) requested_by: Option<String>,
    pub(crate) changed_at_utc: String,
    pub(crate) last_flush_at_utc: Option<String>,
    pub(crate) pid: u32,
    pub(crate) service_agent_id: String,
    pub(crate) service_name: String,
}

impl ServerControlStatus {
    fn new(service_agent_id: &str, service_name: &str) -> Self {
        Self {
            mode: ServerMode::Running,
            write_blocked: false,
            reason: None,
            requested_by: None,
            changed_at_utc: current_timestamp(),
            last_flush_at_utc: None,
            pid: std::process::id(),
            service_agent_id: service_agent_id.to_owned(),
            service_name: service_name.to_owned(),
        }
    }
}

fn current_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

async fn ensure_writes_allowed(
    state: &AppState,
    operation: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let control = state.control_status.read().await.clone();
    if let Some(response) = write_guard_response(&control, operation) {
        return Err(response);
    }
    Ok(())
}

fn write_guard_response(
    control: &ServerControlStatus,
    operation: &str,
) -> Option<(StatusCode, Json<serde_json::Value>)> {
    if !control.write_blocked {
        return None;
    }

    Some((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": format!("server is in {} mode; {operation} is temporarily unavailable", control.mode.as_str()),
            "maintenance": control,
        })),
    ))
}

/// Map an `anyhow::Error` to an HTTP 500 response with a JSON body.
#[expect(
    clippy::needless_pass_by_value,
    reason = "used as map_err(internal_error) — fn pointer requires by-value"
)]
fn internal_error(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": format!("{e:#}")})),
    )
}

/// Map a bad-input string to an HTTP 400 response with a JSON body.
fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": msg.into()})),
    )
}

/// Map an Axum [`JsonRejection`] (deserialization failure) to an HTTP 400
/// response so clients receive a consistent error code whether a field is
/// missing from the payload or present but invalid.
///
/// Axum's built-in `Json<T>` extractor returns 422 on deserialisation errors.
/// By accepting `Result<Json<T>, JsonRejection>` in handlers and mapping
/// rejections through this function we normalise all JSON parse failures to 400.
#[expect(
    clippy::needless_pass_by_value,
    reason = "used as .map_err(json_rejection_to_400) — fn pointer requires by-value"
)]
fn json_rejection_to_400(e: JsonRejection) -> (StatusCode, Json<serde_json::Value>) {
    bad_request(e.to_string())
}

// Local wrapper so serde's `default = "default_priority"` resolves in this module scope.
fn default_priority() -> String {
    crate::models::default_priority()
}

/// Optional `?encoding=` query parameter accepted by read/health/presence endpoints.
///
/// When `encoding=toon`, the response is `text/plain` TOON lines instead of JSON.
#[derive(Debug, Deserialize, Default)]
pub(crate) struct EncodingQuery {
    #[serde(default)]
    pub(crate) encoding: Option<String>,
}

impl EncodingQuery {
    fn is_toon(&self) -> bool {
        self.encoding.as_deref() == Some("toon")
    }
}

// --- GET /health -----------------------------------------------------------

pub(crate) async fn http_health_handler(
    State(state): State<AppState>,
    Query(enc): Query<EncodingQuery>,
) -> impl IntoResponse {
    let pool = state.redis.clone();
    let pool_for_health = pool.clone();
    let control = state.control_status.read().await.clone();
    let result =
        tokio::task::spawn_blocking(move || bus_health(&state.settings, Some(&pool_for_health)))
            .await
            .expect("spawn_blocking panicked");

    // Attach r2d2 pool metrics so operators can see connection reuse stats.
    let (acquired, errors) = pool.metrics();
    let pool_state = pool.pool_state();
    let mut val = serde_json::to_value(&result).unwrap_or_default();
    if let serde_json::Value::Object(ref mut map) = val {
        map.insert(
            "pool".to_owned(),
            serde_json::json!({
                "connections_acquired": acquired,
                "connection_errors": errors,
                "idle": pool_state.idle_connections,
                "max_size": pool_state.connections,
            }),
        );
        map.insert(
            "maintenance".to_owned(),
            serde_json::to_value(&control).unwrap_or_default(),
        );
    }

    if enc.is_toon() {
        axum::response::Response::builder()
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(axum::body::Body::from(format_health_toon(&result)))
            .unwrap_or_default()
    } else {
        axum::response::Response::builder()
            .header("Content-Type", "application/json")
            .body(axum::body::Body::from(
                serde_json::to_string(&val).unwrap_or_default(),
            ))
            .unwrap_or_default()
    }
}

#[derive(Debug, Deserialize)]
struct HttpControlRequest {
    action: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    requested_by: Option<String>,
    #[serde(default = "default_true")]
    flush: bool,
}

async fn flush_pg_writer() -> Result<()> {
    if let Some(writer) = crate::pg_writer()
        && !writer.flush_and_wait_async(Duration::from_secs(2)).await
    {
        anyhow::bail!("pg flush did not complete before timeout");
    }
    Ok(())
}

async fn http_control_status_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let control = state.control_status.read().await.clone();
    Ok(Json(serde_json::to_value(&control).unwrap_or_default()))
}

async fn http_control_action_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpControlRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    let action = req.action.trim().to_owned();
    if action.is_empty() {
        return Err(bad_request("action must not be empty"));
    }

    match action.as_str() {
        "pause" => {
            if req.flush {
                flush_pg_writer().await.map_err(internal_error)?;
            }
            let mut control = state.control_status.write().await;
            control.mode = ServerMode::Maintenance;
            control.write_blocked = true;
            control.reason = req.reason.filter(|reason| !reason.trim().is_empty());
            control.requested_by = req.requested_by.filter(|agent| !agent.trim().is_empty());
            control.changed_at_utc = current_timestamp();
            if req.flush {
                control.last_flush_at_utc = Some(control.changed_at_utc.clone());
            }
            let snapshot = control.clone();
            Ok(Json(serde_json::json!({
                "ok": true,
                "action": "pause",
                "maintenance": snapshot,
            })))
        }
        "resume" => {
            let mut control = state.control_status.write().await;
            control.mode = ServerMode::Running;
            control.write_blocked = false;
            control.reason = None;
            control.requested_by = req.requested_by.filter(|agent| !agent.trim().is_empty());
            control.changed_at_utc = current_timestamp();
            let snapshot = control.clone();
            Ok(Json(serde_json::json!({
                "ok": true,
                "action": "resume",
                "maintenance": snapshot,
            })))
        }
        "flush" => {
            flush_pg_writer().await.map_err(internal_error)?;
            let mut control = state.control_status.write().await;
            control.last_flush_at_utc = Some(current_timestamp());
            let snapshot = control.clone();
            Ok(Json(serde_json::json!({
                "ok": true,
                "action": "flush",
                "maintenance": snapshot,
            })))
        }
        "stop" => {
            if req.flush {
                flush_pg_writer().await.map_err(internal_error)?;
            }
            let mut control = state.control_status.write().await;
            control.mode = ServerMode::Stopping;
            control.write_blocked = true;
            control.reason = req.reason.filter(|reason| !reason.trim().is_empty());
            control.requested_by = req.requested_by.filter(|agent| !agent.trim().is_empty());
            control.changed_at_utc = current_timestamp();
            if req.flush {
                control.last_flush_at_utc = Some(control.changed_at_utc.clone());
            }
            let snapshot = control.clone();
            let shutdown_signal = Arc::clone(&state.shutdown_signal);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                shutdown_signal.notify_waiters();
            });
            Ok(Json(serde_json::json!({
                "ok": true,
                "action": "stop",
                "maintenance": snapshot,
            })))
        }
        _ => Err(bad_request("action must be pause|resume|flush|stop")),
    }
}

// --- POST /messages --------------------------------------------------------

/// Request body for POST /messages.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpSendRequest {
    pub(crate) sender: String,
    pub(crate) recipient: String,
    pub(crate) topic: String,
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default = "default_priority")]
    pub(crate) priority: String,
    #[serde(default)]
    pub(crate) request_ack: bool,
    #[serde(default)]
    pub(crate) reply_to: Option<String>,
    #[serde(default)]
    pub(crate) metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) schema: Option<String>,
}

pub(crate) async fn http_send_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpSendRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "send").await?;
    // Validate required fields (cheap — stays on the async task).
    let sender = req.sender.trim().to_owned();
    let recipient = req.recipient.trim().to_owned();
    let topic = req.topic.trim().to_owned();
    let body_text = req.body.trim().to_owned();

    if sender.is_empty() {
        return Err(bad_request("sender must not be empty"));
    }
    if recipient.is_empty() {
        return Err(bad_request("recipient must not be empty"));
    }
    if topic.is_empty() {
        return Err(bad_request("topic must not be empty"));
    }
    if body_text.is_empty() {
        return Err(bad_request("body must not be empty"));
    }
    validate_priority(&req.priority).map_err(|e| bad_request(format!("{e:#}")))?;
    let effective_schema = enforce_schema_for_transport("http", req.schema.as_deref(), &topic);
    let fitted_body = auto_fit_schema(&body_text, effective_schema);
    validate_message_schema(&fitted_body, effective_schema)
        .map_err(|e| bad_request(format!("schema validation failed: {e:#}")))?;

    let metadata = req
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let thread_id = req.thread_id;
    let tags = req.tags;
    let priority = req.priority;
    let request_ack = req.request_ack;
    let reply_to = req.reply_to;

    // Capture SSE subscriber state before `state` is moved.
    let has_sse_subscribers = state.sse_subscriber_count.any();

    let posted = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        post_message(
            &mut conn,
            &state.settings,
            &PostMessageRequest {
                sender: &sender,
                recipient: &recipient,
                topic: &topic,
                body: &fitted_body,
                thread_id: thread_id.as_deref(),
                tags: &tags,
                priority: &priority,
                request_ack,
                reply_to: reply_to.as_deref(),
                metadata: &metadata,
                has_sse_subscribers,
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&posted).unwrap_or_default()),
    ))
}

// --- GET /messages ---------------------------------------------------------

/// Query parameters for GET /messages.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpReadQuery {
    pub(crate) agent: Option<String>,
    pub(crate) from: Option<String>,
    #[serde(default)]
    pub(crate) repo: Option<String>,
    #[serde(default)]
    pub(crate) session: Option<String>,
    #[serde(default)]
    pub(crate) tag: Vec<String>,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default = "default_since_minutes")]
    pub(crate) since: u64,
    #[serde(default = "default_limit")]
    pub(crate) limit: usize,
    #[serde(default = "default_true")]
    pub(crate) broadcast: bool,
    /// Optional encoding override. `toon` returns `text/plain` TOON lines.
    #[serde(default)]
    pub(crate) encoding: Option<String>,
}

fn default_since_minutes() -> u64 {
    60
}

fn default_limit() -> usize {
    50
}

fn default_true() -> bool {
    true
}

pub(crate) async fn http_read_handler(
    State(state): State<AppState>,
    Query(params): Query<HttpReadQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let since = params.since.min(MAX_HISTORY_MINUTES);
    let limit = params.limit.clamp(1, 500);
    let agent = params.agent;
    let from = params.from;
    let repo = params.repo;
    let session = params.session;
    let tag = params.tag;
    let thread_id = params.thread_id;
    let broadcast = params.broadcast;
    let toon = params.encoding.as_deref() == Some("toon");
    let msgs = tokio::task::spawn_blocking(move || {
        // Always read from Redis for the HTTP path: Redis is the authoritative
        // source-of-truth (the PgWriter flushes async so PG may lag ~100 ms).
        // This ensures read-after-write consistency without the synchronous PG
        // write that caused POST /messages to take ~230 ms.
        let mut conn = state.redis.get_connection()?;
        let filters = MessageFilters {
            repo: repo.as_deref(),
            session: session.as_deref(),
            tags: &tag,
            thread_id: thread_id.as_deref(),
        };
        list_messages_live(
            &mut conn,
            &state.settings,
            &ReadMessagesRequest {
                agent: agent.as_deref(),
                from_agent: from.as_deref(),
                since_minutes: since,
                limit,
                include_broadcast: broadcast,
                filters,
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    if toon {
        let lines: Vec<String> = msgs.iter().map(format_message_toon).collect();
        Ok(axum::response::Response::builder()
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(axum::body::Body::from(lines.join("\n")))
            .unwrap_or_default()
            .into_response())
    } else {
        Ok(Json(serde_json::to_value(&msgs).unwrap_or_default()).into_response())
    }
}

// --- POST /messages/:id/ack ------------------------------------------------

/// Request body for POST /messages/:id/ack.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpAckRequest {
    pub(crate) agent: String,
    #[serde(default = "default_ack_body")]
    pub(crate) body: String,
}

fn default_ack_body() -> String {
    "ack".to_owned()
}

pub(crate) async fn http_ack_handler(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
    payload: Result<Json<HttpAckRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "ack").await?;
    // Validation stays on the async task (cheap).
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    let message_id = message_id.trim().to_owned();
    if message_id.is_empty() {
        return Err(bad_request("message id must not be empty"));
    }
    let ack_body = req.body;

    let has_sse_subscribers = state.sse_subscriber_count.any();
    let acked_id = message_id.clone();
    let posted = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        post_ack(
            &mut conn,
            &state.settings,
            &AckMessageRequest {
                agent: &agent,
                message_id: &message_id,
                body: &ack_body,
                has_sse_subscribers,
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;
    let response = serde_json::json!({
        "ack_sent": true,
        "ack_message_id": posted.message.id,
        "acked_message_id": acked_id,
        "timestamp": posted.message.timestamp_utc,
    });
    Ok(Json(response))
}

// --- PUT /presence/:agent --------------------------------------------------

/// Request body for PUT /presence/:agent.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpPresenceRequest {
    #[serde(default = "default_status")]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    #[serde(default = "default_ttl")]
    pub(crate) ttl_seconds: u64,
    #[serde(default)]
    pub(crate) metadata: Option<serde_json::Value>,
}

fn default_status() -> String {
    "online".to_owned()
}

fn default_ttl() -> u64 {
    180
}

pub(crate) async fn http_presence_set_handler(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    payload: Result<Json<HttpPresenceRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "presence update").await?;
    // Validation stays on the async task (cheap).
    let agent = agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }

    let ttl = req.ttl_seconds.clamp(1, 86400);
    let metadata = req
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let status = req.status;
    let session_id = req.session_id;
    let capabilities = req.capabilities;

    let presence = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        set_presence(
            &mut conn,
            &state.settings,
            &PresenceRequest {
                agent: &agent,
                status: &status,
                session_id: session_id.as_deref(),
                capabilities: &capabilities,
                ttl_seconds: ttl,
                metadata: &metadata,
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&presence).unwrap_or_default()))
}

// --- GET /presence ---------------------------------------------------------

pub(crate) async fn http_presence_list_handler(
    State(state): State<AppState>,
    Query(enc): Query<EncodingQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let results = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        bus_list_presence(&mut conn, &state.settings)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    if enc.is_toon() {
        let lines: Vec<String> = results.iter().map(format_presence_toon).collect();
        Ok(axum::response::Response::builder()
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(axum::body::Body::from(lines.join("\n")))
            .unwrap_or_default()
            .into_response())
    } else {
        Ok(Json(serde_json::to_value(&results).unwrap_or_default()).into_response())
    }
}

// --- GET /events -----------------------------------------------------------

/// Query parameters for `GET /events`.
#[derive(Debug, Deserialize)]
struct SseQuery {
    agent: Option<String>,
    #[serde(default = "default_true")]
    broadcast: bool,
}

#[derive(Debug, Deserialize)]
struct NotificationQuery {
    #[serde(default)]
    since_id: Option<String>,
    #[serde(default = "default_notification_history")]
    history: usize,
}

fn default_notification_history() -> usize {
    20
}

/// Stream Redis Pub/Sub events to the client as Server-Sent Events.
///
/// Filters by `agent` when specified; includes broadcast messages when `broadcast=true`.
/// The stream runs until the client disconnects or the Redis connection drops.
///
/// Increments the [`SseSubscriberCount`] on connect and decrements it on disconnect
/// so that `bus_post_message` can skip the Redis `PUBLISH` when no clients are present.
async fn http_sse_handler(
    State(state): State<AppState>,
    Query(params): Query<SseQuery>,
) -> Sse<ReceiverStream<Result<Event, std::convert::Infallible>>> {
    let agent_filter = params.agent;
    let include_broadcast = params.broadcast;
    let settings = (*state.settings).clone();

    // Register this SSE client so bus_post_message knows to PUBLISH.
    state.sse_subscriber_count.inc();
    let counter = Arc::clone(&state.sse_subscriber_count);

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    tokio::task::spawn_blocking(move || {
        // RAII guard: decrement the counter on any exit path — normal return,
        // client disconnect, or subscription failure.
        struct CountGuard(Arc<SseSubscriberCount>);
        impl Drop for CountGuard {
            fn drop(&mut self) {
                self.0.dec();
            }
        }
        let _count_guard = CountGuard(counter);

        let Ok(client) = redis::Client::open(settings.redis_url.as_str()) else {
            return;
        };
        let Ok(mut conn) = client.get_connection() else {
            return;
        };
        let mut pubsub = conn.as_pubsub();
        if pubsub.subscribe(&settings.channel_key).is_err() {
            return;
        }

        while let Ok(msg) = pubsub.get_message() {
            let Ok(payload): Result<String, _> = msg.get_payload() else {
                continue;
            };

            // Apply agent filter when one was provided.
            if let Some(ref filter) = agent_filter
                && let Ok(event_val) = serde_json::from_str::<serde_json::Value>(&payload)
            {
                let recipient = event_val
                    .pointer("/message/to")
                    .or_else(|| event_val.pointer("/presence/agent"))
                    .and_then(|v| v.as_str());
                let matches = recipient == Some(filter.as_str())
                    || (include_broadcast && recipient == Some("all"));
                if !matches {
                    continue;
                }
            }

            let sse_event = Event::default().data(payload);
            if tx.blocking_send(Ok(sse_event)).is_err() {
                break; // client disconnected
            }
        }
    });

    Sse::new(ReceiverStream::new(rx))
}

// --- GET /presence/history -------------------------------------------------

/// Query parameters for `GET /presence/history`.
#[derive(Debug, Deserialize)]
struct HttpPresenceHistoryQuery {
    agent: Option<String>,
    #[serde(default = "default_since_minutes")]
    since: u64,
    #[serde(default = "default_limit")]
    limit: usize,
}

async fn http_presence_history_handler(
    State(state): State<AppState>,
    Query(params): Query<HttpPresenceHistoryQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let since = params.since.min(MAX_HISTORY_MINUTES);
    let limit = params.limit.clamp(1, 500);
    let agent = params.agent;
    let settings = Arc::clone(&state.settings);

    let result = tokio::task::spawn_blocking(move || {
        crate::postgres_store::list_presence_history_postgres(
            &settings,
            agent.as_deref(),
            since,
            limit,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&result).unwrap_or_default()))
}

// --- MCP Streamable HTTP transport (POST /mcp + GET /mcp) ------------------
//
// The MCP Streamable HTTP transport (spec 2025-06-18) works as follows:
//
// 1. Client sends POST /mcp with a JSON-RPC 2.0 request body.
// 2. For single-response tools the server replies 200 with a JSON-RPC response.
// 3. For streaming or multi-event responses the server replies 200 with
//    `Content-Type: text/event-stream` (SSE) containing one or more JSON-RPC
//    response events.
// 4. Session continuity is maintained via the `Mcp-Session-Id` response header;
//    clients echo it back on subsequent requests.
//
// This implementation routes to the same `AgentBusMcpServer` tool handlers used
// by the stdio transport, ensuring behavioural parity across transports.

/// Dispatch a JSON-RPC 2.0 request to the appropriate MCP tool and return a
/// JSON-RPC 2.0 response.
///
/// Stateless per request — session ID is echoed for client convenience but not
/// used for routing in this implementation (all tools are side-effect free at
/// the dispatch level).
pub(crate) async fn handle_mcp_http(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Assign or echo the session ID.
    let session_id = headers
        .get("Mcp-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), String::from);

    let request_id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let method = request
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

    let server = AgentBusMcpServer::new((*state.settings).clone());
    let response =
        tokio::task::spawn_blocking(move || dispatch_mcp_method(&server, &method, &params))
            .await
            .unwrap_or_else(|e| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32603, "message": format!("task join error: {e}")},
                })
            });

    // Merge the id into the final response.
    let mut resp = response;
    if let serde_json::Value::Object(ref mut map) = resp {
        map.insert("jsonrpc".to_owned(), serde_json::json!("2.0"));
        map.insert("id".to_owned(), request_id);
    }

    let mut builder = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_id);

    // If the client sent `Accept: text/event-stream`, wrap in SSE framing.
    let wants_sse = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/event-stream"));

    if wants_sse {
        builder = builder.header("Content-Type", "text/event-stream");
        let body = format!(
            "data: {}\n\n",
            serde_json::to_string(&resp).unwrap_or_default()
        );
        builder
            .body(axum::body::Body::from(body))
            .unwrap_or_default()
    } else {
        builder
            .body(axum::body::Body::from(
                serde_json::to_string(&resp).unwrap_or_default(),
            ))
            .unwrap_or_default()
    }
}

/// Route a JSON-RPC method to the matching MCP tool or lifecycle handler.
///
/// Returns a partial JSON-RPC response (no `jsonrpc` or `id` fields — the
/// caller merges those in).
fn dispatch_mcp_method(
    server: &AgentBusMcpServer,
    method: &str,
    params: &serde_json::Value,
) -> serde_json::Value {
    match method {
        "initialize" => {
            // Return server capabilities in the MCP initialize response format.
            serde_json::json!({
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "agent-bus",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": "Agent Hub coordination bus (Redis + PostgreSQL). \
                        Use post_message, list_messages, set_presence, list_presence, \
                        ack_message, bus_health, list_presence_history, negotiate."
                }
            })
        }

        "tools/list" => {
            let tools: Vec<serde_json::Value> = AgentBusMcpServer::tool_list()
                .into_iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": *t.input_schema,
                    })
                })
                .collect();
            serde_json::json!({"result": {"tools": tools}})
        }

        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let args = params
                .get("arguments")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            match server.call_tool_sync(&tool_name, &args) {
                Ok(value) => {
                    serde_json::json!({"result": {"content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}]}})
                }
                Err(e) => serde_json::json!({
                    "error": {"code": -32603, "message": format!("{e:#}")}
                }),
            }
        }

        other => serde_json::json!({
            "error": {
                "code": -32601,
                "message": format!("method not found: {other}")
            }
        }),
    }
}

/// GET /mcp — capability discovery endpoint (returns server info + tool list).
///
/// Clients that only need to enumerate capabilities without sending a
/// JSON-RPC body can GET this endpoint.
pub(crate) async fn handle_mcp_sse(State(state): State<AppState>) -> impl IntoResponse {
    let tools: Vec<serde_json::Value> = AgentBusMcpServer::tool_list()
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": *t.input_schema,
            })
        })
        .collect();

    let session_id = uuid::Uuid::new_v4().to_string();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "agent-bus",
                "version": env!("CARGO_PKG_VERSION"),
                "redis_url": state.settings.redis_url.as_str()
            },
            "tools": tools
        }
    });

    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json")
        .header("Mcp-Session-Id", &session_id)
        .body(axum::body::Body::from(
            serde_json::to_string(&body).unwrap_or_default(),
        ))
        .unwrap_or_default()
}

// --- GET /events/:agent_id -------------------------------------------------

fn notification_event(notification: &crate::redis_bus::Notification) -> Event {
    let payload = serde_json::json!({
        "event": "notification",
        "notification": notification,
        "message": notification.message,
    });
    let event_json = serde_json::to_string(&payload).unwrap_or_default();
    let mut event = Event::default().event("notification").data(event_json);
    if let Some(stream_id) = notification.notification_stream_id.as_deref() {
        event = event.id(stream_id);
    }
    event
}

fn notification_from_message(
    message: crate::models::Message,
) -> Option<crate::redis_bus::Notification> {
    if message.to.is_empty() || message.to == "all" {
        return None;
    }

    let reason = if message.topic == "knock" {
        "knock"
    } else if message.request_ack {
        "direct_request_ack"
    } else {
        "direct_message"
    };

    Some(crate::redis_bus::Notification {
        id: message.id.clone(),
        agent: message.to.clone(),
        created_at: message.timestamp_utc.clone(),
        reason: reason.to_owned(),
        requires_ack: message.request_ack,
        message,
        notification_stream_id: None,
    })
}

/// Push notification events to all live SSE connections for a specific agent
/// (and for `"all"` broadcast connections).
async fn push_notifications_to_agent_connections(
    agent_connections: &AgentConnections,
    notifications: &[crate::redis_bus::Notification],
) {
    for notification in notifications {
        let targets: Vec<String> = {
            let guard = agent_connections.read().await;
            let mut t = Vec::new();
            if guard.contains_key(&notification.agent) {
                t.push(notification.agent.clone());
            }
            if notification.agent != "all" && guard.contains_key("all") {
                t.push("all".to_owned());
            }
            t
        };

        if targets.is_empty() {
            continue;
        }

        let mut guard = agent_connections.write().await;
        for target in &targets {
            let Some(senders) = guard.get_mut(target) else {
                continue;
            };
            let event = notification_event(notification);
            senders.retain(|tx| tx.try_send(Ok(event.clone())).is_ok());
            if senders.is_empty() {
                guard.remove(target);
            }
        }
    }
}

fn spawn_agent_notification_bridge(state: &AppState) {
    let settings = Arc::clone(&state.settings);
    let agent_connections = Arc::clone(&state.agent_connections);
    let runtime = tokio::runtime::Handle::current();

    tokio::task::spawn_blocking(move || {
        loop {
            let Ok(client) = redis::Client::open(settings.redis_url.as_str()) else {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            };
            let Ok(mut conn) = client.get_connection() else {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            };
            let mut pubsub = conn.as_pubsub();
            if pubsub.subscribe(&settings.channel_key).is_err() {
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }

            while let Ok(msg) = pubsub.get_message() {
                let Ok(payload): Result<String, _> = msg.get_payload() else {
                    continue;
                };
                let Ok(event_val) = serde_json::from_str::<serde_json::Value>(&payload) else {
                    continue;
                };
                if event_val.get("event").and_then(|value| value.as_str()) != Some("message") {
                    continue;
                }
                let Some(message_val) = event_val.get("message").cloned() else {
                    continue;
                };
                let Ok(message) = serde_json::from_value::<crate::models::Message>(message_val)
                else {
                    continue;
                };
                let Some(notification) = notification_from_message(message) else {
                    continue;
                };
                runtime.block_on(async {
                    push_notifications_to_agent_connections(&agent_connections, &[notification])
                        .await;
                });
            }
        }
    });
}

/// Agent-specific SSE stream: `GET /events/:agent_id`.
///
/// When an agent connects here it is registered in the shared `AgentConnections`
/// map so that any subsequent `POST /messages` addressed to that agent (or
/// broadcast `"all"`) is delivered immediately without polling.
///
/// The stream runs until the client disconnects or the underlying channel
/// buffer fills.
async fn http_sse_agent_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(params): Query<NotificationQuery>,
    headers: HeaderMap,
) -> Sse<ReceiverStream<Result<Event, std::convert::Infallible>>> {
    // Channel capacity 128: enough for burst delivery without unbounded memory.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(128);

    let since_id = params.since_id.or_else(|| {
        headers
            .get("Last-Event-ID")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    });
    let history = params.history.min(500);

    if let Ok(mut conn) = state.redis.get_connection() {
        let replay = if let Some(ref since_id) = since_id {
            list_notifications_since_id(&mut conn, &agent_id, since_id, history)
        } else {
            list_notifications(&mut conn, &agent_id, history)
        };
        if let Ok(notifications) = replay {
            for notification in notifications {
                if tx
                    .send(Ok(notification_event(&notification)))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    state
        .agent_connections
        .write()
        .await
        .entry(agent_id.clone())
        .or_default()
        .push(tx.clone());

    // Spawn a cleanup task that runs when the receiver drops (i.e. client
    // disconnects).  It removes this specific sender from the map.
    let connections = Arc::clone(&state.agent_connections);
    tokio::spawn(async move {
        // The tx is held both by the map entry above and this closure.
        // We drop our local copy here so only the map holds it; when the
        // client disconnects the ReceiverStream drops rx, the map entry's
        // sender becomes the only live copy, and `try_send` in the push
        // helper will fail + remove it.
        drop(tx);
        // Yield once to let the connection establish before cleanup runs.
        tokio::task::yield_now().await;
        // Note: actual cleanup happens lazily in push_notifications_to_agent_connections.
        let _ = &connections; // keep alive
    });

    Sse::new(ReceiverStream::new(rx))
}

async fn http_notifications_handler(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Query(params): Query<NotificationQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let history = params.history.min(500);
    let since_id = params.since_id;
    let notifications = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        if let Some(since_id) = since_id.as_deref() {
            list_notifications_since_id(&mut conn, &agent_id, since_id, history)
        } else {
            list_notifications(&mut conn, &agent_id, history)
        }
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(
        serde_json::to_value(&notifications).unwrap_or_default(),
    ))
}

// --- GET /pending-acks -----------------------------------------------------

/// Query parameters for `GET /pending-acks`.
#[derive(Debug, Deserialize)]
struct HttpPendingAcksQuery {
    agent: Option<String>,
}

/// List all messages currently awaiting acknowledgement.
///
/// Returns a JSON array of `PendingAck` records.  Entries older than 60 seconds
/// are flagged with `stale: true`.
async fn http_pending_acks_handler(
    State(state): State<AppState>,
    Query(params): Query<HttpPendingAcksQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let agent = params.agent;
    let pending = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        crate::redis_bus::list_pending_acks(&mut conn, agent.as_deref())
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&pending).unwrap_or_default()))
}

// --- Server bootstrap ------------------------------------------------------

/// Start the MCP Streamable HTTP server on the given port.
///
/// Exposes `POST /mcp` (tool dispatch) and `GET /mcp` (capability discovery)
/// alongside the existing REST routes.  This allows LLM clients that implement
/// the 2025-06-18 MCP Streamable HTTP transport spec to connect directly.
pub(crate) async fn start_mcp_http_server(settings: Settings, port: u16) -> Result<()> {
    let bind_host = settings.server_host.clone();
    let redis = RedisPool::new(&settings).context("Redis pool creation failed")?;
    let control_status = Arc::new(RwLock::new(ServerControlStatus::new(
        &settings.service_agent_id,
        &settings.service_name,
    )));
    let state = AppState {
        settings: Arc::new(settings),
        redis,
        agent_connections: Arc::new(RwLock::new(HashMap::new())),
        sse_subscriber_count: Arc::new(SseSubscriberCount::default()),
        control_status,
        shutdown_signal: Arc::new(Notify::new()),
    };
    let app = Router::new()
        .route("/mcp", post(handle_mcp_http).get(handle_mcp_sse))
        .route("/health", get(http_health_handler))
        .with_state(state);

    let addr = format!("{bind_host}:{port}");
    tracing::info!("MCP HTTP server listening on {addr}");
    eprintln!("agent-bus MCP Streamable HTTP server listening on http://{addr}/mcp");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context("failed to bind MCP HTTP server")?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("MCP HTTP server error")?;
    Ok(())
}

// --- POST /messages/batch --------------------------------------------------

/// Request body for `POST /messages/batch`.
///
/// Post up to 100 messages in a single HTTP request, saving N-1 round-trips
/// when agents send multiple findings at once.  Each entry is validated the
/// same way as a single `POST /messages` request.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpBatchSendRequest {
    pub(crate) messages: Vec<HttpSendRequest>,
}

/// Response body for `POST /messages/batch`.
#[derive(Debug, serde::Serialize)]
pub(crate) struct HttpBatchSendResponse {
    /// Created message IDs in the same order as the input array.
    pub(crate) ids: Vec<String>,
    /// Number of messages created (convenience field).
    pub(crate) count: usize,
}

pub(crate) async fn http_batch_send_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpBatchSendRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "batch send").await?;
    if req.messages.is_empty() {
        return Err(bad_request("messages array must not be empty"));
    }
    if req.messages.len() > 100 {
        return Err(bad_request("batch size limit is 100 messages"));
    }

    // Validate all messages eagerly on the async task (cheap, no I/O) and
    // collect into typed `BatchSendPayload` structs that `bus_post_messages_batch`
    // consumes directly.  This removes the unwieldy 10-tuple from the old path.
    let mut payloads: Vec<BatchSendPayload> = Vec::with_capacity(req.messages.len());
    for m in &req.messages {
        let sender = m.sender.trim().to_owned();
        let recipient = m.recipient.trim().to_owned();
        let topic = m.topic.trim().to_owned();
        let body_text = m.body.trim().to_owned();
        if sender.is_empty() {
            return Err(bad_request("sender must not be empty"));
        }
        if recipient.is_empty() {
            return Err(bad_request("recipient must not be empty"));
        }
        if topic.is_empty() {
            return Err(bad_request("topic must not be empty"));
        }
        if body_text.is_empty() {
            return Err(bad_request("body must not be empty"));
        }
        validate_priority(&m.priority).map_err(|e| bad_request(format!("{e:#}")))?;
        let effective_schema = enforce_schema_for_transport("http", m.schema.as_deref(), &topic);
        let fitted_body = auto_fit_schema(&body_text, effective_schema);
        validate_message_schema(&fitted_body, effective_schema)
            .map_err(|e| bad_request(format!("schema validation failed: {e:#}")))?;
        let metadata = m
            .metadata
            .clone()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        payloads.push(BatchSendPayload {
            from: sender,
            to: recipient,
            topic,
            body: fitted_body,
            thread_id: m.thread_id.clone(),
            tags: m.tags.clone(),
            priority: m.priority.clone(),
            request_ack: m.request_ack,
            reply_to: m.reply_to.clone(),
            metadata,
        });
    }

    // Snapshot the SSE subscriber flag before moving `state` into the blocking
    // task to avoid a cross-thread Arc dance inside spawn_blocking.
    let has_sse = state.sse_subscriber_count.any();

    let posted = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        // Single Redis pipeline round-trip for all XADD + PUBLISH + pending-ack.
        bus_post_messages_batch_with_notifications(
            &mut conn,
            &state.settings,
            payloads,
            crate::pg_writer(),
            has_sse,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    let ids: Vec<String> = posted.iter().map(|m| m.message.id.clone()).collect();
    let count = ids.len();
    Ok(Json(
        serde_json::to_value(HttpBatchSendResponse { ids, count }).unwrap_or_default(),
    ))
}

// --- POST /read/batch -------------------------------------------------------

/// Request body for `POST /read/batch`.
///
/// Returns messages for multiple agent IDs in one request, deduplicated by
/// message ID and sorted chronologically.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpBatchReadRequest {
    pub(crate) agents: Vec<String>,
    #[serde(default = "default_since_minutes")]
    pub(crate) since: u64,
    #[serde(default = "default_limit")]
    pub(crate) limit: usize,
    #[serde(default = "default_true")]
    pub(crate) broadcast: bool,
}

pub(crate) async fn http_batch_read_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpBatchReadRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    if req.agents.is_empty() {
        return Err(bad_request("agents array must not be empty"));
    }
    if req.agents.len() > 20 {
        return Err(bad_request("batch read limit is 20 agents"));
    }
    let since = req.since.min(MAX_HISTORY_MINUTES);
    let limit = req.limit.clamp(1, 500);
    let broadcast = req.broadcast;
    let agents = req.agents;

    let result = tokio::task::spawn_blocking(move || {
        // Read from Redis (not PG) for the same read-after-write consistency
        // reason as in `http_read_handler`.
        let mut conn = state.redis.get_connection()?;
        let mut seen = std::collections::HashSet::new();
        let mut all_msgs: Vec<crate::models::Message> = Vec::new();
        for agent in &agents {
            let msgs = bus_list_messages_from_redis(
                &mut conn,
                &state.settings,
                Some(agent.as_str()),
                None,
                since,
                limit,
                broadcast,
            )?;
            for msg in msgs {
                if seen.insert(msg.id.clone()) {
                    all_msgs.push(msg);
                }
            }
        }
        all_msgs.sort_by(|a, b| a.timestamp_utc.cmp(&b.timestamp_utc));
        Ok::<Vec<crate::models::Message>, anyhow::Error>(all_msgs)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&result).unwrap_or_default()))
}

// --- POST /ack/batch --------------------------------------------------------

/// Request body for `POST /ack/batch`.
///
/// Acknowledge multiple messages in one HTTP request.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpBatchAckRequest {
    pub(crate) agent: String,
    pub(crate) message_ids: Vec<String>,
    #[serde(default = "default_ack_body")]
    pub(crate) body: String,
}

pub(crate) async fn http_batch_ack_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpBatchAckRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "batch ack").await?;
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    if req.message_ids.is_empty() {
        return Err(bad_request("message_ids must not be empty"));
    }
    if req.message_ids.len() > 100 {
        return Err(bad_request("batch ack limit is 100 message IDs"));
    }
    let ids = req.message_ids;
    let ack_body = req.body;
    let has_sse_subscribers = state.sse_subscriber_count.any();

    let posted = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        let mut posted = Vec::with_capacity(ids.len());
        for message_id in &ids {
            let meta = serde_json::json!({"ack_for": message_id});
            posted.push(bus_post_message_with_notifications(
                &mut conn,
                &state.settings,
                &agent,
                "all",
                "ack",
                &ack_body,
                None,
                &[],
                "normal",
                false,
                Some(message_id.as_str()),
                &meta,
                crate::pg_writer(),
                has_sse_subscribers,
            )?);
        }
        Ok::<Vec<crate::redis_bus::PostedMessage>, anyhow::Error>(posted)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    let acked_ids: Vec<String> = posted
        .into_iter()
        .map(|posted| posted.message.reply_to.unwrap_or_default())
        .collect();

    Ok(Json(serde_json::json!({
        "acked": acked_ids.len(),
        "message_ids": acked_ids,
    })))
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Channel HTTP handlers
// ---------------------------------------------------------------------------

// --- POST /channels/direct/:agent_id  /  GET /channels/direct/:agent_id ---

/// Request body for `POST /channels/direct/:agent_id`.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpDirectSendRequest {
    pub(crate) sender: String,
    pub(crate) body: String,
    #[serde(default = "default_direct_topic")]
    pub(crate) topic: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
}

fn default_direct_topic() -> String {
    "direct".to_owned()
}

async fn http_direct_send_handler(
    State(state): State<AppState>,
    Path(recipient): Path<String>,
    payload: Result<Json<HttpDirectSendRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "direct send").await?;
    let sender = req.sender.trim().to_owned();
    let recipient = recipient.trim().to_owned();
    let body = req.body.trim().to_owned();
    if sender.is_empty() {
        return Err(bad_request("sender must not be empty"));
    }
    if recipient.is_empty() {
        return Err(bad_request("recipient must not be empty"));
    }
    if body.is_empty() {
        return Err(bad_request("body must not be empty"));
    }
    let topic = req.topic;
    let thread_id = req.thread_id;
    let tags = req.tags;

    let msg = tokio::task::spawn_blocking(move || {
        crate::channels::post_direct(
            &state.settings,
            &sender,
            &recipient,
            &topic,
            &body,
            thread_id.as_deref(),
            &tags,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&msg).unwrap_or_default()),
    ))
}

#[derive(Debug, Deserialize)]
struct HttpDirectReadQuery {
    agent: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

async fn http_direct_read_handler(
    State(state): State<AppState>,
    Path(other): Path<String>,
    Query(params): Query<HttpDirectReadQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let agent_a = params.agent;
    let limit = params.limit.clamp(1, 500);

    let msgs = tokio::task::spawn_blocking(move || {
        crate::channels::read_direct(&state.settings, &agent_a, &other, limit)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&msgs).unwrap_or_default()))
}

// --- POST /channels/groups  /  GET /channels/groups ------------------------

/// Request body for `POST /channels/groups`.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpCreateGroupRequest {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) members: Vec<String>,
    pub(crate) created_by: String,
}

async fn http_create_group_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpCreateGroupRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "group creation").await?;
    let name = req.name.trim().to_owned();
    if name.is_empty() {
        return Err(bad_request("name must not be empty"));
    }
    let members = req.members;
    let created_by = req.created_by;

    let info = tokio::task::spawn_blocking(move || {
        crate::channels::create_group(&state.settings, &name, &members, &created_by)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&info).unwrap_or_default()),
    ))
}

async fn http_list_groups_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let groups = tokio::task::spawn_blocking(move || crate::channels::list_groups(&state.settings))
        .await
        .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
        .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&groups).unwrap_or_default()))
}

// --- POST /channels/groups/:name/messages  /  GET /channels/groups/:name/messages

/// Request body for `POST /channels/groups/:name/messages`.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpGroupSendRequest {
    pub(crate) sender: String,
    pub(crate) body: String,
    #[serde(default = "default_group_topic")]
    pub(crate) topic: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
}

fn default_group_topic() -> String {
    "group".to_owned()
}

async fn http_group_send_handler(
    State(state): State<AppState>,
    Path(group_name): Path<String>,
    payload: Result<Json<HttpGroupSendRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "group send").await?;
    let sender = req.sender.trim().to_owned();
    let body = req.body.trim().to_owned();
    if sender.is_empty() {
        return Err(bad_request("sender must not be empty"));
    }
    if body.is_empty() {
        return Err(bad_request("body must not be empty"));
    }
    let topic = req.topic;
    let thread_id = req.thread_id;

    let msg = tokio::task::spawn_blocking(move || {
        crate::channels::post_to_group(
            &state.settings,
            &group_name,
            &sender,
            &topic,
            &body,
            thread_id.as_deref(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&msg).unwrap_or_default()),
    ))
}

#[derive(Debug, Deserialize)]
struct HttpGroupReadQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

async fn http_group_read_handler(
    State(state): State<AppState>,
    Path(group_name): Path<String>,
    Query(params): Query<HttpGroupReadQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let limit = params.limit.clamp(1, 500);

    let msgs = tokio::task::spawn_blocking(move || {
        crate::channels::read_group(&state.settings, &group_name, limit)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&msgs).unwrap_or_default()))
}

// --- POST /channels/escalate -----------------------------------------------

/// Request body for `POST /channels/escalate`.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpEscalateRequest {
    pub(crate) sender: String,
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
}

async fn http_escalate_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpEscalateRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "escalation").await?;
    let sender = req.sender.trim().to_owned();
    let body = req.body.trim().to_owned();
    if sender.is_empty() {
        return Err(bad_request("sender must not be empty"));
    }
    if body.is_empty() {
        return Err(bad_request("body must not be empty"));
    }
    let thread_id = req.thread_id;
    let tags = req.tags;

    let msg = tokio::task::spawn_blocking(move || {
        crate::channels::post_escalation(
            &state.settings,
            &sender,
            &body,
            thread_id.as_deref(),
            &tags,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&msg).unwrap_or_default()),
    ))
}

// --- POST /channels/arbitrate/:resource  /  GET /channels/arbitrate/:resource

/// Request body for `POST /channels/arbitrate/:resource`.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpClaimRequest {
    pub(crate) agent: String,
    #[serde(default = "default_priority_argument")]
    pub(crate) priority_argument: String,
    #[serde(default = "default_claim_mode")]
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) namespace: Option<String>,
    #[serde(default)]
    pub(crate) scope_kind: Option<String>,
    #[serde(default)]
    pub(crate) scope_path: Option<String>,
    #[serde(default)]
    pub(crate) repo_scopes: Vec<String>,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default = "default_claim_ttl_seconds")]
    pub(crate) lease_ttl_seconds: u64,
}

fn default_priority_argument() -> String {
    "first-edit required".to_owned()
}

fn default_claim_mode() -> String {
    "exclusive".to_owned()
}

fn default_claim_ttl_seconds() -> u64 {
    3600
}

async fn http_claim_handler(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    payload: Result<Json<HttpClaimRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "claim").await?;
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    let priority_argument = req.priority_argument;
    let options = crate::channels::ClaimOptions {
        mode: match req.mode.as_str() {
            "shared" => crate::channels::ResourceLeaseMode::Shared,
            "shared_namespaced" => crate::channels::ResourceLeaseMode::SharedNamespaced,
            "exclusive" => crate::channels::ResourceLeaseMode::Exclusive,
            _ => {
                return Err(bad_request(
                    "mode must be shared|shared_namespaced|exclusive",
                ));
            }
        },
        namespace: req.namespace,
        scope_kind: req.scope_kind,
        scope_path: req.scope_path,
        repo_scopes: req.repo_scopes,
        thread_id: req.thread_id,
        lease_ttl_seconds: req.lease_ttl_seconds.max(1),
    };

    let claim = tokio::task::spawn_blocking(move || {
        crate::channels::claim_resource_with_options(
            &state.settings,
            &resource,
            &agent,
            &priority_argument,
            &options,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&claim).unwrap_or_default()),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct HttpRenewClaimRequest {
    pub(crate) agent: String,
    #[serde(default = "default_claim_ttl_seconds")]
    pub(crate) lease_ttl_seconds: u64,
}

async fn http_renew_claim_handler(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    payload: Result<Json<HttpRenewClaimRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "claim renewal").await?;
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }

    let claim = tokio::task::spawn_blocking(move || {
        crate::channels::renew_claim(
            &state.settings,
            &resource,
            &agent,
            Some(req.lease_ttl_seconds.max(1)),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&claim).unwrap_or_default()))
}

#[derive(Debug, Deserialize)]
pub(crate) struct HttpReleaseClaimRequest {
    pub(crate) agent: String,
}

async fn http_release_claim_handler(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    payload: Result<Json<HttpReleaseClaimRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "claim release").await?;
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }

    let arbitration = tokio::task::spawn_blocking(move || {
        crate::channels::release_claim(&state.settings, &resource, &agent)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&arbitration).unwrap_or_default()))
}

async fn http_arbitration_state_handler(
    State(state): State<AppState>,
    Path(resource): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let state_data = tokio::task::spawn_blocking(move || {
        crate::channels::get_arbitration_state(&state.settings, &resource)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&state_data).unwrap_or_default()))
}

// --- PUT /channels/arbitrate/:resource/resolve -----------------------------

/// Request body for `PUT /channels/arbitrate/:resource/resolve`.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpResolveRequest {
    pub(crate) winner: String,
    #[serde(default = "default_resolve_reason")]
    pub(crate) reason: String,
    #[serde(default = "default_resolved_by")]
    pub(crate) resolved_by: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HttpKnockRequest {
    pub(crate) sender: String,
    pub(crate) recipient: String,
    #[serde(default = "default_knock_body")]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default = "default_knock_request_ack")]
    pub(crate) request_ack: bool,
}

fn default_knock_body() -> String {
    "check the bus".to_owned()
}

fn default_knock_request_ack() -> bool {
    true
}

async fn http_knock_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpKnockRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "knock").await?;
    let sender = req.sender.trim().to_owned();
    let recipient = req.recipient.trim().to_owned();
    if sender.is_empty() {
        return Err(bad_request("sender must not be empty"));
    }
    if recipient.is_empty() {
        return Err(bad_request("recipient must not be empty"));
    }
    if req.body.trim().is_empty() {
        return Err(bad_request("body must not be empty"));
    }

    let metadata = knock_metadata(req.request_ack);
    let posted = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        post_message(
            &mut conn,
            &state.settings,
            &PostMessageRequest {
                sender: &sender,
                recipient: &recipient,
                topic: "knock",
                body: &req.body,
                thread_id: req.thread_id.as_deref(),
                tags: &req.tags,
                priority: "urgent",
                request_ack: req.request_ack,
                reply_to: None,
                metadata: &metadata,
                has_sse_subscribers: state.sse_subscriber_count.any(),
            },
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&posted).unwrap_or_default()),
    ))
}

fn default_resolve_reason() -> String {
    "resolved by orchestrator".to_owned()
}

fn default_resolved_by() -> String {
    "orchestrator".to_owned()
}

async fn http_resolve_handler(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    payload: Result<Json<HttpResolveRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "claim resolution").await?;
    let winner = req.winner.trim().to_owned();
    if winner.is_empty() {
        return Err(bad_request("winner must not be empty"));
    }
    let reason = req.reason;
    let resolved_by = req.resolved_by;

    let arbitration = tokio::task::spawn_blocking(move || {
        crate::channels::resolve_claim(&state.settings, &resource, &winner, &reason, &resolved_by)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&arbitration).unwrap_or_default()))
}

// --- GET /channels/summary -------------------------------------------------

async fn http_channel_summary_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let summary =
        tokio::task::spawn_blocking(move || crate::channels::channel_summary(&state.settings))
            .await
            .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
            .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&summary).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// Token estimation and context compaction HTTP handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct HttpTokenCountRequest {
    text: String,
}

async fn http_token_count_handler(
    payload: Result<Json<HttpTokenCountRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    let (characters, estimated_tokens) = tokio::task::spawn_blocking(move || {
        let chars = req.text.chars().count();
        let tokens = crate::token::estimate_tokens(&req.text);
        (chars, tokens)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?;

    Ok(Json(serde_json::json!({
        "characters": characters,
        "estimated_tokens": estimated_tokens,
    })))
}

#[derive(Debug, Deserialize)]
struct HttpCompactContextRequest {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default = "default_since_minutes")]
    since_minutes: u64,
    #[serde(default = "default_max_tokens")]
    max_tokens: usize,
}

fn default_max_tokens() -> usize {
    4000
}

async fn http_compact_context_handler(
    State(state): State<AppState>,
    payload: Result<Json<HttpCompactContextRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    let since = req.since_minutes.min(MAX_HISTORY_MINUTES);
    let max_tokens = req.max_tokens;
    let agent = req.agent;
    let repo = req.repo;
    let session = req.session;
    let tags = req.tags;
    let thread_id = req.thread_id;
    let settings = Arc::clone(&state.settings);

    let compacted = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        let filters = MessageFilters {
            repo: repo.as_deref(),
            session: session.as_deref(),
            tags: &tags,
            thread_id: thread_id.as_deref(),
        };
        list_messages_live(
            &mut conn,
            &settings,
            &ReadMessagesRequest {
                agent: agent.as_deref(),
                from_agent: None,
                since_minutes: since,
                limit: 500,
                include_broadcast: true,
                filters,
            },
        )
        .map(|msgs| crate::token::compact_context(&msgs, max_tokens))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::Value::Array(compacted)))
}

// ---------------------------------------------------------------------------
// Task queue HTTP handlers
// ---------------------------------------------------------------------------

/// Request body for `POST /tasks/:agent`.
#[derive(Debug, Deserialize)]
struct HttpPushTaskRequest {
    task: String,
}

/// Query parameters for `GET /tasks/:agent`.
#[derive(Debug, Deserialize)]
struct HttpPeekTasksQuery {
    #[serde(default = "default_peek_limit")]
    limit: usize,
}

fn default_peek_limit() -> usize {
    10
}

/// `POST /tasks/:agent` — push a task to the tail of an agent's queue.
///
/// Request body: `{"task": "<payload>"}`.
/// Response: `{"agent": "<id>", "queue_length": N}`.
async fn http_push_task_handler(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    payload: Result<Json<HttpPushTaskRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    ensure_writes_allowed(&state, "task push").await?;
    let task = req.task.trim().to_owned();
    if task.is_empty() {
        return Err(bad_request("task must not be empty"));
    }
    if agent.trim().is_empty() {
        return Err(bad_request("agent must not be empty"));
    }

    let len = tokio::task::spawn_blocking(move || {
        crate::redis_bus::push_task(&state.settings, &agent, &task)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(
        serde_json::to_value(serde_json::json!({"queue_length": len})).unwrap_or_default(),
    ))
}

/// `GET /tasks/:agent` — peek at pending tasks without consuming them.
///
/// Query params: `?limit=10` (default 10; `0` returns all).
/// Response: `{"agent": "<id>", "tasks": [...], "count": N, "queue_length": N}`.
///
/// `count` is the number of tasks returned (bounded by `limit`).
/// `queue_length` is the total number of pending tasks in the queue.
async fn http_peek_tasks_handler(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(params): Query<HttpPeekTasksQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if agent.trim().is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    let limit = params.limit;
    let agent_clone = agent.clone();

    let (tasks, queue_length) = tokio::task::spawn_blocking(move || {
        let tasks = crate::redis_bus::peek_tasks(&state.settings, &agent_clone, limit)?;
        let queue_length = crate::redis_bus::task_queue_length(&state.settings, &agent_clone)?;
        Ok::<_, anyhow::Error>((tasks, queue_length))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    let count = tasks.len();
    Ok(Json(
        serde_json::to_value(serde_json::json!({
            "agent": agent,
            "tasks": tasks,
            "count": count,
            "queue_length": queue_length,
        }))
        .unwrap_or_default(),
    ))
}

/// `DELETE /tasks/:agent` — pop and consume the next task.
///
/// Response: `{"agent": "<id>", "task": "<payload>" | null}`.
async fn http_pull_task_handler(
    State(state): State<AppState>,
    Path(agent): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    ensure_writes_allowed(&state, "task pull").await?;
    if agent.trim().is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    let agent_clone = agent.clone();

    let task = tokio::task::spawn_blocking(move || {
        crate::redis_bus::pull_task(&state.settings, &agent_clone)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(
        serde_json::to_value(serde_json::json!({"agent": agent, "task": task})).unwrap_or_default(),
    ))
}

// ---------------------------------------------------------------------------
// GET /dashboard — monitoring web dashboard
// ---------------------------------------------------------------------------

/// Render a self-contained HTML monitoring dashboard with auto-refresh.
///
/// The page is fully self-contained (no external CSS/JS) and refreshes every
/// 10 seconds via a `setInterval` call that re-fetches `/presence` and
/// `/messages` from the live server.
///
/// # Errors
///
/// Returns an internal-server error when the health spawn-blocking call fails.
pub(crate) async fn http_dashboard_handler(State(state): State<AppState>) -> impl IntoResponse {
    let pool = state.redis.clone();
    let settings = Arc::clone(&state.settings);
    let health = tokio::task::spawn_blocking(move || bus_health(&settings, Some(&pool)))
        .await
        .unwrap_or_else(|_| crate::redis_bus::health_error_fallback());

    axum::response::Html(generate_dashboard_html(&health))
}

/// Generate the self-contained dashboard HTML string from a [`Health`] snapshot.
///
/// Static health values (Redis status, PG status, stream length, counts) are
/// inlined server-side.  Agent presence and recent messages are fetched by
/// client-side JavaScript every 10 seconds so the page stays live without a
/// full reload.
#[expect(
    clippy::too_many_lines,
    reason = "HTML template — splitting it would hurt readability without improving correctness"
)]
fn generate_dashboard_html(health: &crate::models::Health) -> String {
    let redis_class = if health.ok { "ok" } else { "err" };
    let redis_status = if health.ok { "Connected" } else { "Error" };
    let pg_ok = health.database_ok.unwrap_or(false);
    let pg_class = if pg_ok { "ok" } else { "err" };
    let pg_status = if pg_ok { "Connected" } else { "Unavailable" };
    let stream_length = health.stream_length.unwrap_or(0);
    let pg_count = health.pg_message_count.unwrap_or(0);
    let pg_presence = health.pg_presence_count.unwrap_or(0);
    let writes_queued = health.pg_writes_queued.unwrap_or(0);
    let writes_done = health.pg_writes_completed.unwrap_or(0);
    let write_errors = health.pg_write_errors.unwrap_or(0);

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Agent Hub Dashboard</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:#0d1117;color:#c9d1d9;padding:20px;font-size:14px}}
h1{{color:#58a6ff;margin-bottom:16px;font-size:22px;font-weight:600}}
.subtitle{{color:#8b949e;font-size:12px;margin-bottom:20px}}
.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(280px,1fr));gap:16px;margin-bottom:20px}}
.card{{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:16px}}
.card h2{{color:#f0883e;font-size:11px;text-transform:uppercase;letter-spacing:.08em;margin-bottom:12px;font-weight:600}}
.stat{{display:flex;justify-content:space-between;align-items:center;padding:4px 0;border-bottom:1px solid #21262d}}
.stat:last-child{{border-bottom:none}}
.stat-label{{color:#8b949e}}
.stat-value{{font-weight:500;font-variant-numeric:tabular-nums}}
.ok{{color:#3fb950}}
.err{{color:#f85149}}
.warn{{color:#d29922}}
.section{{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:16px;margin-bottom:16px}}
.section h2{{color:#f0883e;font-size:11px;text-transform:uppercase;letter-spacing:.08em;margin-bottom:12px;font-weight:600;display:flex;justify-content:space-between;align-items:center}}
.refresh-ts{{color:#8b949e;font-weight:400;font-size:10px;text-transform:none;letter-spacing:0}}
table{{width:100%;border-collapse:collapse;font-size:13px}}
th{{color:#58a6ff;text-align:left;padding:6px 8px;border-bottom:2px solid #30363d;font-weight:600;font-size:11px;text-transform:uppercase}}
td{{padding:5px 8px;border-bottom:1px solid #21262d;vertical-align:top;max-width:300px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
tr:last-child td{{border-bottom:none}}
tr:hover td{{background:#1c2128}}
.badge{{display:inline-block;padding:1px 7px;border-radius:12px;font-size:10px;font-weight:600;text-transform:uppercase}}
.badge-online{{background:#0d4a1f;color:#3fb950;border:1px solid #238636}}
.badge-busy{{background:#4a2c0a;color:#d29922;border:1px solid #9e6a03}}
.badge-offline{{background:#1c1c1c;color:#8b949e;border:1px solid #30363d}}
.badge-critical{{background:#490202;color:#f85149;border:1px solid #da3633}}
.badge-high{{background:#4a2c0a;color:#d29922;border:1px solid #9e6a03}}
.badge-medium{{background:#1a2f1a;color:#3fb950;border:1px solid #238636}}
.badge-low{{background:#1c2128;color:#8b949e;border:1px solid #30363d}}
.spinner{{display:inline-block;width:10px;height:10px;border:2px solid #30363d;border-top-color:#58a6ff;border-radius:50%;animation:spin .8s linear infinite;margin-right:6px}}
@keyframes spin{{to{{transform:rotate(360deg)}}}}
.empty{{color:#8b949e;text-align:center;padding:20px;font-style:italic}}
</style>
</head>
<body>
<h1>Agent Hub Dashboard</h1>
<p class="subtitle">Live view — auto-refreshes every 10 s &nbsp;|&nbsp; Data from Redis (realtime) + PostgreSQL (durable)</p>

<div class="grid">
  <div class="card">
    <h2>System Health</h2>
    <div class="stat"><span class="stat-label">Redis</span><span class="stat-value {redis_class}">{redis_status}</span></div>
    <div class="stat"><span class="stat-label">PostgreSQL</span><span class="stat-value {pg_class}">{pg_status}</span></div>
    <div class="stat"><span class="stat-label">Redis stream length</span><span class="stat-value">{stream_length}</span></div>
    <div class="stat"><span class="stat-label">PG messages</span><span class="stat-value">{pg_count}</span></div>
    <div class="stat"><span class="stat-label">PG presence events</span><span class="stat-value">{pg_presence}</span></div>
  </div>
  <div class="card">
    <h2>PG Write-Through</h2>
    <div class="stat"><span class="stat-label">Writes queued</span><span class="stat-value">{writes_queued}</span></div>
    <div class="stat"><span class="stat-label">Writes completed</span><span class="stat-value">{writes_done}</span></div>
    <div class="stat"><span class="stat-label">Write errors</span><span class="stat-value {err_class}">{write_errors}</span></div>
    <div class="stat"><span class="stat-label">Codec</span><span class="stat-value">{codec}</span></div>
  </div>
</div>

<div class="section" id="agents-section">
  <h2>Active Agents <span class="refresh-ts" id="agents-ts"></span></h2>
  <div id="agents-body"><span class="spinner"></span>Loading&hellip;</div>
</div>

<div class="section" id="messages-section">
  <h2>Recent Messages (last 60 min) <span class="refresh-ts" id="messages-ts"></span></h2>
  <div id="messages-body"><span class="spinner"></span>Loading&hellip;</div>
</div>

<script>
function ts() {{
  return new Date().toLocaleTimeString();
}}

function statusBadge(s) {{
  const cls = s === 'online' ? 'online' : s === 'busy' ? 'busy' : 'offline';
  return '<span class="badge badge-' + cls + '">' + esc(s) + '</span>';
}}

function esc(s) {{
  return String(s)
    .replace(/&/g,'&amp;')
    .replace(/</g,'&lt;')
    .replace(/>/g,'&gt;')
    .replace(/"/g,'&quot;');
}}

function fmtTime(iso) {{
  if (!iso) return '';
  return iso.substring(11, 19);
}}

async function refreshAgents() {{
  try {{
    const data = await fetch('/presence').then(r => r.json());
    const list = Array.isArray(data) ? data : [];
    let html;
    if (list.length === 0) {{
      html = '<p class="empty">No active agents</p>';
    }} else {{
      html = '<table><thead><tr><th>Agent</th><th>Status</th><th>Capabilities</th><th>Session</th></tr></thead><tbody>';
      list.forEach(p => {{
        const caps = (p.capabilities || []).map(esc).join(', ') || '—';
        const sess = esc((p.session_id || '').substring(0, 16));
        html += '<tr><td>' + esc(p.agent) + '</td><td>' + statusBadge(p.status)
              + '</td><td>' + caps + '</td><td>' + sess + '</td></tr>';
      }});
      html += '</tbody></table>';
    }}
    document.getElementById('agents-body').innerHTML = html;
    document.getElementById('agents-ts').textContent = 'updated ' + ts();
  }} catch(e) {{
    document.getElementById('agents-body').innerHTML = '<p class="empty err">Failed to load: ' + esc(String(e)) + '</p>';
  }}
}}

async function refreshMessages() {{
  try {{
    const data = await fetch('/messages?since=60&limit=20').then(r => r.json());
    const list = Array.isArray(data) ? data : [];
    let html;
    if (list.length === 0) {{
      html = '<p class="empty">No messages in last 60 minutes</p>';
    }} else {{
      html = '<table><thead><tr><th>Time</th><th>From</th><th>To</th><th>Topic</th><th>Priority</th><th>Body</th></tr></thead><tbody>';
      list.slice().reverse().forEach(m => {{
        const body = esc((m.body || '').substring(0, 100));
        const prio = m.priority || 'normal';
        const prioCls = prio === 'critical' ? 'critical' : prio === 'high' ? 'high'
                      : prio === 'low' ? 'low' : 'medium';
        html += '<tr><td>' + fmtTime(m.timestamp_utc) + '</td><td>' + esc(m.from)
              + '</td><td>' + esc(m.to) + '</td><td>' + esc(m.topic)
              + '</td><td><span class="badge badge-' + prioCls + '">' + esc(prio) + '</span></td>'
              + '<td title="' + esc(m.body || '') + '">' + body + '</td></tr>';
      }});
      html += '</tbody></table>';
    }}
    document.getElementById('messages-body').innerHTML = html;
    document.getElementById('messages-ts').textContent = 'updated ' + ts();
  }} catch(e) {{
    document.getElementById('messages-body').innerHTML = '<p class="empty err">Failed to load: ' + esc(String(e)) + '</p>';
  }}
}}

async function refresh() {{
  await Promise.all([refreshAgents(), refreshMessages()]);
}}

refresh();
setInterval(refresh, 10000);
</script>
</body>
</html>"#,
        redis_class = redis_class,
        redis_status = redis_status,
        pg_class = pg_class,
        pg_status = pg_status,
        stream_length = stream_length,
        pg_count = pg_count,
        pg_presence = pg_presence,
        writes_queued = writes_queued,
        writes_done = writes_done,
        write_errors = write_errors,
        err_class = if write_errors > 0 { "err" } else { "ok" },
        codec = health.codec.as_str(),
    )
}

// ---------------------------------------------------------------------------

pub(crate) async fn start_http_server(settings: Settings, port: u16) -> Result<()> {
    let bind_host = settings.server_host.clone();
    let redis = RedisPool::new(&settings).context("Redis client creation failed")?;
    let control_status = Arc::new(RwLock::new(ServerControlStatus::new(
        &settings.service_agent_id,
        &settings.service_name,
    )));
    let shutdown_signal = Arc::new(Notify::new());
    let state = AppState {
        settings: Arc::new(settings),
        redis,
        agent_connections: Arc::new(RwLock::new(HashMap::new())),
        sse_subscriber_count: Arc::new(SseSubscriberCount::default()),
        control_status,
        shutdown_signal: Arc::clone(&shutdown_signal),
    };
    spawn_agent_notification_bridge(&state);
    let app = Router::new()
        .route("/health", get(http_health_handler))
        .route(
            "/admin/control",
            get(http_control_status_handler).post(http_control_action_handler),
        )
        .route("/admin/service", get(http_control_status_handler))
        .route("/admin/service/control", post(http_control_action_handler))
        .route("/messages", post(http_send_handler).get(http_read_handler))
        .route("/messages/batch", post(http_batch_send_handler))
        .route("/messages/{id}/ack", post(http_ack_handler))
        .route("/knock", post(http_knock_handler))
        .route("/read/batch", post(http_batch_read_handler))
        .route("/ack/batch", post(http_batch_ack_handler))
        .route("/presence/{agent}", put(http_presence_set_handler))
        .route("/presence", get(http_presence_list_handler))
        .route("/presence/history", get(http_presence_history_handler))
        .route("/events", get(http_sse_handler))
        .route("/events/{agent_id}", get(http_sse_agent_handler))
        .route("/notifications/{agent_id}", get(http_notifications_handler))
        .route("/pending-acks", get(http_pending_acks_handler))
        // Channel routes
        .route(
            "/channels/direct/{agent_id}",
            post(http_direct_send_handler).get(http_direct_read_handler),
        )
        .route(
            "/channels/groups",
            post(http_create_group_handler).get(http_list_groups_handler),
        )
        .route(
            "/channels/groups/{name}/messages",
            post(http_group_send_handler).get(http_group_read_handler),
        )
        .route("/channels/escalate", post(http_escalate_handler))
        .route(
            "/channels/arbitrate/{resource}",
            post(http_claim_handler).get(http_arbitration_state_handler),
        )
        .route(
            "/channels/arbitrate/{resource}/resolve",
            put(http_resolve_handler),
        )
        .route(
            "/channels/arbitrate/{resource}/renew",
            post(http_renew_claim_handler),
        )
        .route(
            "/channels/arbitrate/{resource}/release",
            post(http_release_claim_handler),
        )
        .route("/channels/summary", get(http_channel_summary_handler))
        .route("/token-count", post(http_token_count_handler))
        .route("/compact-context", post(http_compact_context_handler))
        // Task queue routes
        .route(
            "/tasks/{agent}",
            post(http_push_task_handler)
                .get(http_peek_tasks_handler)
                .delete(http_pull_task_handler),
        )
        // Monitoring dashboard
        .route("/dashboard", get(http_dashboard_handler))
        .with_state(state);

    let addr = format!("{bind_host}:{port}");
    tracing::info!("HTTP server listening on {addr}");
    eprintln!("agent-bus HTTP server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context("failed to bind HTTP server")?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                () = shutdown_signal.notified() => {},
                _ = tokio::signal::ctrl_c() => {},
            }
        })
        .await
        .context("HTTP server error")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Health, PROTOCOL_VERSION};

    fn make_health(redis_ok: bool, pg_ok: bool) -> Health {
        Health {
            ok: redis_ok,
            protocol_version: PROTOCOL_VERSION.to_owned(),
            redis_url: "redis://localhost:6380/0".to_owned(),
            database_url: Some("postgresql://localhost:5300/redis_backend".to_owned()),
            database_ok: Some(pg_ok),
            database_error: None,
            storage_ready: pg_ok,
            runtime: "rust-native".to_owned(),
            codec: "serde_json+lz4".to_owned(),
            stream_length: Some(42),
            pg_message_count: Some(3805),
            pg_presence_count: Some(881),
            pg_writes_queued: Some(100),
            pg_writes_completed: Some(99),
            pg_batches: Some(20),
            pg_write_errors: Some(1),
        }
    }

    #[test]
    fn dashboard_html_contains_health_values() {
        let health = make_health(true, true);
        let html = generate_dashboard_html(&health);

        assert!(html.contains("Agent Hub Dashboard"), "title missing");
        assert!(html.contains("Connected"), "redis status missing");
        assert!(html.contains("42"), "stream_length missing");
        assert!(html.contains("3805"), "pg_message_count missing");
        assert!(html.contains("881"), "pg_presence_count missing");
        assert!(html.contains("serde_json+lz4"), "codec missing");
    }

    #[test]
    fn dashboard_html_shows_redis_error_class_when_down() {
        let health = make_health(false, false);
        let html = generate_dashboard_html(&health);

        // Both Redis and PG are down — the err CSS class must appear.
        assert!(
            html.contains("class=\"stat-value err\""),
            "err class missing"
        );
        assert!(html.contains("Error"), "Redis error status missing");
        assert!(
            html.contains("Unavailable"),
            "PG unavailable status missing"
        );
    }

    #[test]
    fn dashboard_html_is_valid_html_structure() {
        let health = make_health(true, true);
        let html = generate_dashboard_html(&health);

        assert!(html.starts_with("<!DOCTYPE html>"), "DOCTYPE missing");
        assert!(html.contains("<html"), "html tag missing");
        assert!(html.contains("</html>"), "html close tag missing");
        assert!(html.contains("<head>"), "head tag missing");
        assert!(html.contains("</head>"), "head close tag missing");
        assert!(html.contains("<body>"), "body tag missing");
        assert!(html.contains("</body>"), "body close tag missing");
        assert!(html.contains("<script>"), "script tag missing");
        assert!(html.contains("setInterval"), "auto-refresh JS missing");
    }

    #[test]
    fn dashboard_html_contains_api_fetch_calls() {
        let health = make_health(true, true);
        let html = generate_dashboard_html(&health);

        assert!(
            html.contains("fetch('/presence')"),
            "presence fetch missing"
        );
        assert!(
            html.contains("fetch('/messages?since=60&limit=20')"),
            "messages fetch missing"
        );
        assert!(
            html.contains("setInterval(refresh, 10000)"),
            "10 s interval missing"
        );
    }

    #[test]
    fn dashboard_html_has_no_external_resources() {
        let health = make_health(true, true);
        let html = generate_dashboard_html(&health);

        // Must not reference any external CDN/font/script URLs.
        assert!(!html.contains("cdn."), "unexpected CDN reference");
        assert!(
            !html.contains("googleapis.com"),
            "unexpected Google reference"
        );
        assert!(!html.contains("unpkg.com"), "unexpected unpkg reference");
        assert!(!html.contains("<link"), "unexpected external link tag");
        assert!(!html.contains("src="), "unexpected external script src");
    }

    #[test]
    fn dashboard_write_errors_show_err_class_when_nonzero() {
        // make_health has pg_write_errors = 1, so err_class resolves to "err".
        let html = generate_dashboard_html(&make_health(true, true));
        // The format! macro must substitute the placeholder — raw placeholder must not appear.
        assert!(
            !html.contains("{err_class}"),
            "unformatted placeholder leaked into output"
        );
        // With write_errors = 1 the "err" class must be present somewhere.
        assert!(
            html.contains("class=\"stat-value err\""),
            "err class absent when write_errors > 0"
        );
        // Zero write errors should produce the "ok" class instead.
        let mut healthy = make_health(true, true);
        healthy.pg_write_errors = Some(0);
        let html_zero = generate_dashboard_html(&healthy);
        assert!(
            !html_zero.contains("class=\"stat-value err\""),
            "err class should be absent when write_errors = 0"
        );
        assert!(
            html_zero.contains("class=\"stat-value ok\""),
            "ok class should be present when write_errors = 0"
        );
    }

    #[test]
    fn server_control_status_defaults_to_running() {
        let status = ServerControlStatus::new("agent-bus", "AgentHub");
        assert_eq!(status.mode, ServerMode::Running);
        assert!(!status.write_blocked);
        assert_eq!(status.service_agent_id, "agent-bus");
        assert_eq!(status.service_name, "AgentHub");
        assert!(status.reason.is_none());
    }

    #[test]
    fn write_guard_response_blocks_mutations_during_maintenance() {
        let mut status = ServerControlStatus::new("agent-bus", "AgentHub");
        status.mode = ServerMode::Maintenance;
        status.write_blocked = true;
        status.reason = Some("deploy".to_owned());

        let (code, body) = write_guard_response(&status, "send").expect("guard should trigger");
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        let body = serde_json::to_value(body.0).expect("body should serialize");
        assert_eq!(body["maintenance"]["mode"], "maintenance");
        assert_eq!(body["maintenance"]["reason"], "deploy");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("send"),
            "error should mention blocked operation"
        );
    }

    #[test]
    fn write_guard_response_allows_mutations_while_running() {
        let status = ServerControlStatus::new("agent-bus", "AgentHub");
        assert!(write_guard_response(&status, "send").is_none());
    }
}
