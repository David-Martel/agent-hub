//! Axum HTTP REST server mirroring MCP tool operations.

use std::collections::HashMap;
use std::sync::Arc;

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
use serde::Deserialize;
use tokio::sync::RwLock;
use tokio_stream::wrappers::ReceiverStream;

use crate::mcp::AgentBusMcpServer;
use crate::models::MAX_HISTORY_MINUTES;
use crate::output::{format_health_toon, format_message_toon, format_presence_toon};
use crate::redis_bus::{
    RedisPool, bus_health, bus_list_messages, bus_list_presence, bus_post_message, bus_set_presence,
};
use crate::settings::Settings;
use crate::validation::{
    auto_fit_schema, infer_schema_from_topic, validate_message_schema, validate_priority,
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

/// Shared state injected into every axum handler.
///
/// `AppState` is cheap to clone — `settings` is behind an `Arc` and `redis`
/// wraps an r2d2 pool whose `inner` field is `Arc`-backed.
///
/// `agent_connections` carries the live agent-specific SSE subscriber map so
/// that `POST /messages` can push directly to connected agents without them
/// having to poll.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) settings: Arc<Settings>,
    pub(crate) redis: RedisPool,
    /// Live SSE connections keyed by agent ID.
    pub(crate) agent_connections: AgentConnections,
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
    let result = tokio::task::spawn_blocking(move || bus_health(&state.settings))
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
    let effective_schema = infer_schema_from_topic(&topic, req.schema.as_deref());
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

    // Retain a handle to the connections map before `state` is moved into the
    // blocking task.
    let agent_connections = Arc::clone(&state.agent_connections);

    let msg = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        bus_post_message(
            &mut conn,
            &state.settings,
            &sender,
            &recipient,
            &topic,
            &fitted_body,
            thread_id.as_deref(),
            &tags,
            &priority,
            request_ack,
            reply_to.as_deref(),
            &metadata,
            crate::pg_writer(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    // Push the message directly to any agent-specific SSE connections.
    // Best-effort: a failure here never affects the HTTP response.
    push_to_agent_connections(&agent_connections, &msg).await;

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
    let broadcast = params.broadcast;
    let toon = params.encoding.as_deref() == Some("toon");

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

    let acked_id = message_id.clone();
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
            crate::pg_writer(),
        )
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    let response = serde_json::json!({
        "ack_sent": true,
        "ack_message_id": msg.id,
        "acked_message_id": acked_id,
        "timestamp": msg.timestamp_utc,
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
            crate::pg_writer(),
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

/// Push a message to all live SSE connections for a specific agent (and for
/// `"all"` broadcast connections).
///
/// Stale channels whose receiver has dropped are removed in the same write
/// lock pass, keeping the map clean without a separate GC task.
async fn push_to_agent_connections(
    agent_connections: &AgentConnections,
    msg: &crate::models::Message,
) {
    let event_json =
        serde_json::to_string(&serde_json::json!({"event": "message", "message": msg}))
            .unwrap_or_default();

    // Collect the agents that should receive this event.
    let targets: Vec<String> = {
        let guard = agent_connections.read().await;
        let mut t = Vec::new();
        if guard.contains_key(&msg.to) {
            t.push(msg.to.clone());
        }
        // Always push to "all" subscribers.
        if msg.to != "all" && guard.contains_key("all") {
            t.push("all".to_owned());
        }
        t
    };

    if targets.is_empty() {
        return;
    }

    let mut guard = agent_connections.write().await;
    for target in &targets {
        let Some(senders) = guard.get_mut(target) else {
            continue;
        };
        let event = Event::default().data(event_json.clone());
        senders.retain(|tx| tx.try_send(Ok(event.clone())).is_ok());
        if senders.is_empty() {
            // Remove the entry entirely when all subscribers disconnected.
            guard.remove(target);
        }
    }
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
) -> Sse<ReceiverStream<Result<Event, std::convert::Infallible>>> {
    // Channel capacity 128: enough for burst delivery without unbounded memory.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(128);

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
        // Note: actual cleanup happens lazily in push_to_agent_connections.
        let _ = &connections; // keep alive
    });

    Sse::new(ReceiverStream::new(rx))
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
    let state = AppState {
        settings: Arc::new(settings),
        redis,
        agent_connections: Arc::new(RwLock::new(HashMap::new())),
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
    if req.messages.is_empty() {
        return Err(bad_request("messages array must not be empty"));
    }
    if req.messages.len() > 100 {
        return Err(bad_request("batch size limit is 100 messages"));
    }

    // Validate all messages eagerly before any Redis writes.
    #[allow(clippy::type_complexity)]
    let mut validated: Vec<(
        String,
        String,
        String,
        String,
        Option<String>,
        Vec<String>,
        String,
        bool,
        Option<String>,
        serde_json::Value,
    )> = Vec::new();
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
        let effective_schema = infer_schema_from_topic(&topic, m.schema.as_deref());
        let fitted_body = auto_fit_schema(&body_text, effective_schema);
        validate_message_schema(&fitted_body, effective_schema)
            .map_err(|e| bad_request(format!("schema validation failed: {e:#}")))?;
        let metadata = m
            .metadata
            .clone()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        validated.push((
            sender,
            recipient,
            topic,
            fitted_body,
            m.thread_id.clone(),
            m.tags.clone(),
            m.priority.clone(),
            m.request_ack,
            m.reply_to.clone(),
            metadata,
        ));
    }

    let ids = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        let mut ids = Vec::with_capacity(validated.len());
        for (
            sender,
            recipient,
            topic,
            body,
            thread_id,
            tags,
            priority,
            request_ack,
            reply_to,
            metadata,
        ) in validated
        {
            let msg = bus_post_message(
                &mut conn,
                &state.settings,
                &sender,
                &recipient,
                &topic,
                &body,
                thread_id.as_deref(),
                &tags,
                &priority,
                request_ack,
                reply_to.as_deref(),
                &metadata,
                crate::pg_writer(),
            )?;
            ids.push(msg.id);
        }
        Ok::<Vec<String>, anyhow::Error>(ids)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

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
        let mut seen = std::collections::HashSet::new();
        let mut all_msgs: Vec<crate::models::Message> = Vec::new();
        for agent in &agents {
            let msgs = bus_list_messages(
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

    let acked_ids = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        let mut acked = Vec::with_capacity(ids.len());
        for message_id in &ids {
            let meta = serde_json::json!({"ack_for": message_id});
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
                Some(message_id.as_str()),
                &meta,
                crate::pg_writer(),
            )?;
            acked.push(message_id.clone());
        }
        Ok::<Vec<String>, anyhow::Error>(acked)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

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
}

fn default_priority_argument() -> String {
    "first-edit required".to_owned()
}

async fn http_claim_handler(
    State(state): State<AppState>,
    Path(resource): Path<String>,
    payload: Result<Json<HttpClaimRequest>, JsonRejection>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let Json(req) = payload.map_err(json_rejection_to_400)?;
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    let priority_argument = req.priority_argument;

    let claim = tokio::task::spawn_blocking(move || {
        crate::channels::claim_resource(&state.settings, &resource, &agent, &priority_argument)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join: {e}")))?
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&claim).unwrap_or_default()),
    ))
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

pub(crate) async fn start_http_server(settings: Settings, port: u16) -> Result<()> {
    let bind_host = settings.server_host.clone();
    let redis = RedisPool::new(&settings).context("Redis client creation failed")?;
    let state = AppState {
        settings: Arc::new(settings),
        redis,
        agent_connections: Arc::new(RwLock::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/health", get(http_health_handler))
        .route("/messages", post(http_send_handler).get(http_read_handler))
        .route("/messages/batch", post(http_batch_send_handler))
        .route("/messages/{id}/ack", post(http_ack_handler))
        .route("/read/batch", post(http_batch_read_handler))
        .route("/ack/batch", post(http_batch_ack_handler))
        .route("/presence/{agent}", put(http_presence_set_handler))
        .route("/presence", get(http_presence_list_handler))
        .route("/presence/history", get(http_presence_history_handler))
        .route("/events", get(http_sse_handler))
        .route("/events/{agent_id}", get(http_sse_agent_handler))
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
        .route("/channels/summary", get(http_channel_summary_handler))
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
