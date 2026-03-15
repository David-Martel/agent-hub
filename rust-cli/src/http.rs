//! Axum HTTP REST server mirroring MCP tool operations.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    response::sse::{Event, Sse},
    routing::{get, post, put},
};
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;

use crate::models::MAX_HISTORY_MINUTES;
use crate::redis_bus::{
    RedisPool, bus_health, bus_list_messages, bus_list_presence, bus_post_message, bus_set_presence,
};
use crate::settings::Settings;
use crate::validation::validate_priority;

/// Shared state injected into every axum handler.
///
/// `AppState` is cheap to clone — `settings` is behind an `Arc` and `redis`
/// wraps a single `redis::Client` (which is `Clone + Send + Sync`).
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) settings: Arc<Settings>,
    pub(crate) redis: RedisPool,
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

// Local wrapper so serde's `default = "default_priority"` resolves in this module scope.
fn default_priority() -> String {
    crate::models::default_priority()
}

// --- GET /health -----------------------------------------------------------

pub(crate) async fn http_health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || bus_health(&state.settings))
        .await
        .expect("spawn_blocking panicked");
    Json(serde_json::to_value(&result).unwrap_or_default())
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
}

pub(crate) async fn http_send_handler(
    State(state): State<AppState>,
    Json(req): Json<HttpSendRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
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

    let metadata = req
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let thread_id = req.thread_id;
    let tags = req.tags;
    let priority = req.priority;
    let request_ack = req.request_ack;
    let reply_to = req.reply_to;

    let msg = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        bus_post_message(
            &mut conn,
            &state.settings,
            &sender,
            &recipient,
            &topic,
            &body_text,
            thread_id.as_deref(),
            &tags,
            &priority,
            request_ack,
            reply_to.as_deref(),
            &metadata,
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

// --- GET /messages ---------------------------------------------------------

/// Query parameters for GET /messages.
#[derive(Debug, Deserialize)]
pub(crate) struct HttpReadQuery {
    pub(crate) agent: Option<String>,
    pub(crate) from: Option<String>,
    #[serde(default = "default_since_minutes")]
    pub(crate) since: u64,
    #[serde(default = "default_limit")]
    pub(crate) limit: usize,
    #[serde(default = "default_true")]
    pub(crate) broadcast: bool,
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
    let broadcast = params.broadcast;

    let msgs = tokio::task::spawn_blocking(move || {
        bus_list_messages(
            &state.settings,
            agent.as_deref(),
            from.as_deref(),
            since,
            limit,
            broadcast,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&msgs).unwrap_or_default()))
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
    Json(req): Json<HttpAckRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
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

    let msg = tokio::task::spawn_blocking(move || {
        let meta = serde_json::json!({"ack_for": &message_id});
        let mut conn = state.redis.get_connection()?;
        bus_post_message(
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
            Some(&message_id),
            &meta,
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&msg).unwrap_or_default()))
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
    Json(req): Json<HttpPresenceRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
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
        bus_set_presence(
            &mut conn,
            &state.settings,
            &agent,
            &status,
            session_id.as_deref(),
            &capabilities,
            ttl,
            &metadata,
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
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let results = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        bus_list_presence(&mut conn, &state.settings)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&results).unwrap_or_default()))
}

// --- GET /events -----------------------------------------------------------

/// Query parameters for `GET /events`.
#[derive(Debug, Deserialize)]
struct SseQuery {
    agent: Option<String>,
    #[serde(default = "default_true")]
    broadcast: bool,
}

/// Stream Redis Pub/Sub events to the client as Server-Sent Events.
///
/// Filters by `agent` when specified; includes broadcast messages when `broadcast=true`.
/// The stream runs until the client disconnects or the Redis connection drops.
async fn http_sse_handler(
    State(state): State<AppState>,
    Query(params): Query<SseQuery>,
) -> Sse<ReceiverStream<Result<Event, std::convert::Infallible>>> {
    let agent_filter = params.agent;
    let include_broadcast = params.broadcast;
    let settings = (*state.settings).clone();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);

    tokio::task::spawn_blocking(move || {
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
            if let Some(ref filter) = agent_filter {
                if let Ok(event_val) = serde_json::from_str::<serde_json::Value>(&payload) {
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

// --- Server bootstrap ------------------------------------------------------

pub(crate) async fn start_http_server(settings: Settings, port: u16) -> Result<()> {
    let bind_host = settings.server_host.clone();
    let redis = RedisPool::new(&settings).context("Redis client creation failed")?;
    let state = AppState {
        settings: Arc::new(settings),
        redis,
    };
    let app = Router::new()
        .route("/health", get(http_health_handler))
        .route("/messages", post(http_send_handler).get(http_read_handler))
        .route("/messages/{id}/ack", post(http_ack_handler))
        .route("/presence/{agent}", put(http_presence_set_handler))
        .route("/presence", get(http_presence_list_handler))
        .route("/presence/history", get(http_presence_history_handler))
        .route("/events", get(http_sse_handler))
        .with_state(state);

    let addr = format!("{bind_host}:{port}");
    tracing::info!("HTTP server listening on {addr}");
    eprintln!("agent-bus HTTP server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context("failed to bind HTTP server")?;
    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;
    Ok(())
}
