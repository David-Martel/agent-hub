//! Axum HTTP REST server mirroring MCP tool operations.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;

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
    let h = bus_health(&state.settings);
    Json(serde_json::to_value(&h).unwrap_or_default())
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
    // Validate required fields.
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

    let mut conn = state.redis.get_connection().map_err(internal_error)?;
    let msg = bus_post_message(
        &mut conn,
        &state.settings,
        &sender,
        &recipient,
        &topic,
        &body_text,
        req.thread_id.as_deref(),
        &req.tags,
        &req.priority,
        req.request_ack,
        req.reply_to.as_deref(),
        &metadata,
    )
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

    let msgs = bus_list_messages(
        &state.settings,
        params.agent.as_deref(),
        params.from.as_deref(),
        since,
        limit,
        params.broadcast,
    )
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
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    let message_id = message_id.trim().to_owned();
    if message_id.is_empty() {
        return Err(bad_request("message id must not be empty"));
    }

    let meta = serde_json::json!({"ack_for": &message_id});
    let mut conn = state.redis.get_connection().map_err(internal_error)?;
    let msg = bus_post_message(
        &mut conn,
        &state.settings,
        &agent,
        "all",
        "ack",
        &req.body,
        None,
        &[],
        "normal",
        false,
        Some(&message_id),
        &meta,
    )
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
    let agent = agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }

    let ttl = req.ttl_seconds.clamp(1, 86400);
    let metadata = req
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let mut conn = state.redis.get_connection().map_err(internal_error)?;
    let presence = bus_set_presence(
        &mut conn,
        &state.settings,
        &agent,
        &req.status,
        req.session_id.as_deref(),
        &req.capabilities,
        ttl,
        &metadata,
    )
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&presence).unwrap_or_default()))
}

// --- GET /presence ---------------------------------------------------------

pub(crate) async fn http_presence_list_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let mut conn = state.redis.get_connection().map_err(internal_error)?;
    let results = bus_list_presence(&mut conn, &state.settings).map_err(internal_error)?;
    Ok(Json(serde_json::to_value(&results).unwrap_or_default()))
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
