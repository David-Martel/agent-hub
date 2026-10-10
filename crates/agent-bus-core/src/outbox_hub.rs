//! Stable-ID native hub acceptance, separate from ordinary MCP tool count.
//!
//! Caller authentication is the existing shared bearer gate, not per-actor
//! authorization. The HTTP wrapper must require that gate and exact hub identity.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use thiserror::Error;

use crate::mcp_dispatch::McpToolDispatch;
use crate::models::{AckDeadline, Message, PROTOCOL_VERSION, Presence, ack_deadline_seconds};
use crate::outbox::{Operation, ReplayRequest, ReplayResponse};
use crate::redis_bus::{
    notification_reason, notification_stream_key, prepare_message, should_publish_message_event,
};
use crate::settings::Settings;

/// A replay route is bound to a named hub, never an arbitrary fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayEnvelope {
    pub hub_identity: String,
    pub request: ReplayRequest,
}

/// Hub failures contain no arguments, bearer, raw Redis error or request body.
#[derive(Debug, Error)]
pub enum ReplayHubError {
    #[error("invalid replay request")]
    Invalid,
    #[error("replay hub identity mismatch")]
    Identity,
    #[error("replay identity collision")]
    Collision,
    #[error("replay backend temporarily unavailable; request may be pending")]
    Unavailable,
    #[error("replay backend representation incompatible")]
    Representation,
}

fn text<'a>(args: &'a Map<String, Value>, key: &str, default: &'a str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or(default)
}
fn strings(args: &Map<String, Value>, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}
fn timestamp(ms: u64) -> Result<String, ReplayHubError> {
    let ms = i64::try_from(ms).map_err(|_sanitized| ReplayHubError::Invalid)?;
    DateTime::from_timestamp_millis(ms)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
        .ok_or(ReplayHubError::Invalid)
}

fn scope(settings: &Settings) -> String {
    crate::outbox::digest_hex(settings.stream_key.as_bytes())
}
fn memo_key(settings: &Settings, request: &ReplayRequest) -> String {
    // UUID is globally bound within this receiving hub, not client-controlled
    // agent names. A repeated ID with different complete payload refuses.
    format!(
        "agent_bus:outbox:{}:{}",
        scope(settings),
        request.request_id
    )
}
fn redis_failure(error: &redis::RedisError) -> ReplayHubError {
    match error.code() {
        Some("OUTBOX_ID_COLLISION") => ReplayHubError::Collision,
        Some("OUTBOX_FUTURE_REQUEST" | "OUTBOX_KEY_TYPE") => ReplayHubError::Invalid,
        _ => ReplayHubError::Unavailable,
    }
}

/// Apply or recover one immutable request using the original authority logic.
///
/// # Errors
/// Invalid schema/identity, collision or unavailable/indeterminate backend.
// Keep the four explicit effect plans and cached repair at one transaction boundary.
#[allow(clippy::too_many_lines)]
pub fn accept(
    conn: &mut redis::Connection,
    settings: &Settings,
    envelope: &ReplayEnvelope,
    has_sse_subscribers: bool,
) -> Result<ReplayResponse, ReplayHubError> {
    let request = &envelope.request;
    request
        .validate()
        .map_err(|_sanitized| ReplayHubError::Invalid)?;
    if settings.hub_identity.as_deref() != Some(envelope.hub_identity.as_str()) {
        return Err(ReplayHubError::Identity);
    }
    let fingerprint = request
        .fingerprint()
        .map_err(|_sanitized| ReplayHubError::Invalid)?;
    let args = &request.arguments;
    let memo = memo_key(settings, request);
    let unused = format!("{memo}:unused");
    let mut keys = vec![
        memo.clone(),
        settings.stream_key.clone(),
        settings.channel_key.clone(),
        unused.clone(),
        unused.clone(),
        unused.clone(),
        unused.clone(),
        unused.clone(),
        unused,
    ];
    let mut input = json!({
        "fingerprint":fingerprint, "created_ms":request.created_ms,
        "expires_ms":request.expires_ms, "origin_id":request.origin_id,
        "sequence":request.sequence, "operation":request.operation,
        "key_types":["string","stream","none","none","none","none","none","none","none"],
        "stream_maxlen":settings.stream_maxlen
    });
    match request.operation {
        Operation::Presence => {
            let agent = text(args, "agent", "");
            let status = text(args, "status", "online");
            let capabilities = strings(args, "capabilities");
            crate::validation::reject_nul_in_fields(&[(status, "status")])
                .map_err(|_sanitized| ReplayHubError::Invalid)?;
            for capability in &capabilities {
                crate::validation::reject_nul_bytes(capability, "capability")
                    .map_err(|_sanitized| ReplayHubError::Invalid)?;
            }
            let session_id = text(args, "session_id", "").to_owned();
            crate::validation::reject_nul_bytes(&session_id, "session_id")
                .map_err(|_sanitized| ReplayHubError::Invalid)?;
            let presence = Presence {
                agent: agent.to_owned(),
                status: status.to_owned(),
                protocol_version: PROTOCOL_VERSION.to_owned(),
                timestamp_utc: timestamp(request.created_ms)?,
                session_id: if session_id.is_empty() {
                    request.request_id.to_string()
                } else {
                    session_id
                },
                capabilities,
                metadata: args.get("metadata").cloned().unwrap_or_else(|| json!({})),
                ttl_seconds: (request.expires_ms - request.created_ms) / 1_000,
            };
            keys[6] = format!("{}{agent}", settings.presence_prefix);
            keys[7] = format!(
                "agent_bus:outbox_presence:{}:{}",
                scope(settings),
                crate::outbox::digest_hex(agent.as_bytes())
            );
            input["key_types"][6] = json!("string");
            input["key_types"][7] = json!("string");
            input["result_json"] = json!(
                serde_json::to_string(&presence)
                    .map_err(|_sanitized| ReplayHubError::Representation)?
            );
            input["created_timestamp"] = json!(presence.timestamp_utc);
        }
        Operation::Send | Operation::Ack => {
            let ack = request.operation == Operation::Ack;
            let from = text(args, if ack { "agent" } else { "sender" }, "");
            let to = if ack {
                "all"
            } else {
                text(args, "recipient", "")
            };
            let topic = if ack { "ack" } else { text(args, "topic", "") };
            let priority = if ack {
                "normal"
            } else {
                text(args, "priority", "normal")
            };
            crate::validation::validate_priority(priority)
                .map_err(|_sanitized| ReplayHubError::Invalid)?;
            let body = text(args, "body", if ack { "ack" } else { "" });
            let schema = if ack {
                None
            } else {
                crate::validation::resolve_message_schema(
                    request.surface.as_str(),
                    args.get("schema").and_then(Value::as_str),
                    topic,
                )
                .map_err(|_sanitized| ReplayHubError::Invalid)?
            };
            let fitted = if ack {
                body.to_owned()
            } else {
                crate::validation::auto_fit_schema(
                    body,
                    schema.map(crate::validation::MessageSchema::as_str),
                )
            };
            let tags = if ack { vec![] } else { strings(args, "tags") };
            let metadata = if ack {
                json!({"ack_for":text(args,"message_id","")})
            } else {
                args.get("metadata").cloned().unwrap_or_else(|| json!({}))
            };
            let reply = if ack {
                Some(text(args, "message_id", ""))
            } else {
                args.get("reply_to").and_then(Value::as_str)
            };
            let mut prepared = prepare_message(
                from,
                to,
                topic,
                &fitted,
                args.get("thread_id").and_then(Value::as_str),
                &tags,
                priority,
                !ack && args
                    .get("request_ack")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                reply,
                &metadata,
                settings.session_id.as_deref(),
                schema,
            );
            prepared.message.client_msg_id = Some(request.request_id.to_string());
            prepared.message.origin_hub = Some(envelope.hub_identity.clone());
            let message = &prepared.message;
            let mut fields = vec![
                "id".to_owned(),
                message.id.clone(),
                "timestamp_utc".to_owned(),
                message.timestamp_utc.clone(),
                "protocol_version".to_owned(),
                PROTOCOL_VERSION.to_owned(),
                "from".to_owned(),
                message.from.clone(),
                "to".to_owned(),
                message.to.clone(),
                "topic".to_owned(),
                message.topic.clone(),
                "body".to_owned(),
                prepared.stored_body.clone(),
                "tags".to_owned(),
                prepared.tags_json.clone(),
                "priority".to_owned(),
                message.priority.clone(),
                "request_ack".to_owned(),
                prepared.ack_str.to_owned(),
                "reply_to".to_owned(),
                message.reply_to.clone().unwrap_or_default(),
                "metadata".to_owned(),
                prepared.meta_json.clone(),
                "client_msg_id".to_owned(),
                request.request_id.to_string(),
                "origin_hub".to_owned(),
                envelope.hub_identity.clone(),
            ];
            if !prepared.thread_str.is_empty() {
                fields.extend(["thread_id".to_owned(), prepared.thread_str.clone()]);
            }
            if prepared.is_compressed {
                fields.extend(["_compressed".to_owned(), "lz4".to_owned()]);
            }
            input["message_fields"] = json!(fields);
            input["result_json"] = json!(
                serde_json::to_string(message)
                    .map_err(|_sanitized| ReplayHubError::Representation)?
            );
            // Always publish this new route: direct and legacy subscribers see
            // exactly one event, even when the initial request's reply is lost.
            input["publish"] = json!(should_publish_message_event(message, has_sse_subscribers));
            if to != "all" {
                keys[3] = notification_stream_key(to);
                input["key_types"][3] = json!("stream");
                input["notification_fields"] = json!(vec![
                    "id".to_owned(),
                    message.id.clone(),
                    "agent".to_owned(),
                    to.to_owned(),
                    "created_at".to_owned(),
                    message.timestamp_utc.clone(),
                    "reason".to_owned(),
                    notification_reason(message).to_owned(),
                    "requires_ack".to_owned(),
                    prepared.ack_str.to_owned()
                ]);
            }
            let tracked_id = if ack {
                text(args, "message_id", "")
            } else {
                message.id.as_str()
            };
            keys[4] = format!("agent_bus:pending_ack:{tracked_id}");
            keys[5] = format!("agent_bus:ack_deadline:{tracked_id}");
            input["key_types"][4] = json!("string");
            input["key_types"][5] = json!("string");
            if message.request_ack {
                let ttl = ack_deadline_seconds(priority);
                let deadline = AckDeadline {
                    message_id: message.id.clone(),
                    recipient: to.to_owned(),
                    deadline_at: (Utc::now()
                        + chrono::Duration::seconds(
                            i64::try_from(ttl).map_err(|_sanitized| ReplayHubError::Invalid)?,
                        ))
                    .format("%Y-%m-%dT%H:%M:%S%.6fZ")
                    .to_string(),
                    escalation_level: 0,
                    escalated_to: None,
                };
                input["pending_ack_json"] = json!(
                    json!({"message_id":message.id,"recipient":to,"sent_at":message.timestamp_utc})
                        .to_string()
                );
                input["ack_deadline_json"] = json!(
                    serde_json::to_string(&deadline)
                        .map_err(|_sanitized| ReplayHubError::Representation)?
                );
                input["ack_deadline_seconds"] = json!(ttl);
            }
        }
        Operation::ClaimRequest => {
            keys[8] = crate::channels::claims_key(text(args, "resource", ""));
            input["key_types"][8] = json!("hash");
            input["claim_agent"] = json!(text(args, "agent", ""));
            crate::ops::claim::parse_lease_mode(text(args, "mode", "exclusive"))
                .map_err(|_sanitized| ReplayHubError::Invalid)?;
            if let Some(value) = args.get("scope").and_then(Value::as_str) {
                crate::ops::claim::parse_resource_scope(value)
                    .map_err(|_sanitized| ReplayHubError::Invalid)?;
            }
        }
    }
    let raw: String = redis::Script::new(include_str!("outbox_replay.lua"))
        .prepare_invoke()
        .key(&keys)
        .arg(input.to_string())
        .invoke(conn)
        .map_err(|error| redis_failure(&error))?;
    if raw == r#"{"status":"claim_admission"}"# {
        // The Redis reservation is durable before invoking the ordinary lease
        // authority. Failure after that point is PENDING, not a second grant.
        let (seconds, microseconds): (u64, u64) = redis::cmd("TIME")
            .query(conn)
            .map_err(|_sanitized| ReplayHubError::Unavailable)?;
        let now = seconds.saturating_mul(1000) + microseconds / 1000;
        let remaining = (request.expires_ms.saturating_sub(now) / 1000)
            .min((request.expires_ms - request.created_ms) / 1000);
        if remaining == 0 {
            return finish_claim(
                conn,
                &memo,
                &fingerprint,
                &keys[8],
                text(args, "agent", ""),
                request.expires_ms,
                ReplayResponse::Superseded,
            );
        }
        let mut arguments = args.clone();
        arguments.insert("lease_ttl_seconds".to_owned(), json!(remaining));
        let Ok(result) = McpToolDispatch::new(settings).dispatch_tool("claim_resource", &arguments)
        else {
            return Ok(ReplayResponse::Pending);
        };
        return finish_claim(
            conn,
            &memo,
            &fingerprint,
            &keys[8],
            text(args, "agent", ""),
            request.expires_ms,
            ReplayResponse::Applied { result },
        );
    }
    let mut response: ReplayResponse =
        serde_json::from_str(&raw).map_err(|_sanitized| ReplayHubError::Representation)?;
    // A cached lost reply still forwards the same identity to PG. Existing
    // message upsert uniqueness prevents duplicate durable message rows.
    if let ReplayResponse::Applied { result } = &mut response {
        match request.operation {
            Operation::Send | Operation::Ack => {
                let message: Message = serde_json::from_value(result.clone())
                    .map_err(|_sanitized| ReplayHubError::Representation)?;
                if !crate::outbox_pg::persist(settings, crate::outbox_pg::Event::Message(&message))
                {
                    return Ok(ReplayResponse::Pending);
                }
                if request.operation == Operation::Ack {
                    *result = json!({"ack_sent":true,"ack_message_id":message.id,
                        "acked_message_id":text(args,"message_id",""),"timestamp":message.timestamp_utc});
                }
            }
            Operation::Presence => {
                let presence: Presence = serde_json::from_value(result.clone())
                    .map_err(|_sanitized| ReplayHubError::Representation)?;
                if !crate::outbox_pg::persist(
                    settings,
                    crate::outbox_pg::Event::Presence(
                        &presence,
                        request.request_id,
                        &envelope.hub_identity,
                    ),
                ) {
                    return Ok(ReplayResponse::Pending);
                }
            }
            Operation::ClaimRequest => {}
        }
    }
    Ok(response)
}

fn finish_claim(
    conn: &mut redis::Connection,
    memo: &str,
    fingerprint: &str,
    claims_key: &str,
    agent: &str,
    expires_ms: u64,
    response: ReplayResponse,
) -> Result<ReplayResponse, ReplayHubError> {
    let claim_expires_ms = if let ReplayResponse::Applied { result } = &response {
        let Some(deadline) = result
            .get("expires_at")
            .and_then(Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        else {
            return Ok(ReplayResponse::Pending);
        };
        deadline.min(expires_ms)
    } else {
        expires_ms
    };
    let raw =
        serde_json::to_string(&response).map_err(|_sanitized| ReplayHubError::Representation)?;
    let completed:i32=redis::Script::new(r"
local function same(left,right)
  if type(left)~=type(right) then return false end
  if type(left)~='table' then return left==right end
  for key,value in pairs(left) do if not same(value,right[key]) then return false end end
  for key,_ in pairs(right) do if left[key]==nil then return false end end
  return true
end
local prior=redis.call('GET',KEYS[1])
if not prior then return 0 end
local record=cjson.decode(prior)
if record.fingerprint~=ARGV[1] or record.state~='pending' then return 0 end
local time=redis.call('TIME')
local now=tonumber(time[1])*1000+math.floor(tonumber(time[2])/1000)
if tonumber(ARGV[4])<=now then return 2 end
local response=cjson.decode(ARGV[2])
if response.status=='applied' then
  local kind=redis.call('TYPE',KEYS[2]).ok
  if kind=='none' then return 2 end
  if kind~='hash' then return 0 end
  local current=redis.call('HGET',KEYS[2],ARGV[5])
  if not current then return 2 end
  local valid,claim=pcall(cjson.decode,current)
  if not valid or type(claim)~='table' then return 0 end
  if not same(claim,response.result) then return 2 end
end
redis.call('SET',KEYS[1],cjson.encode({state='applied',fingerprint=ARGV[1],response=ARGV[2],claim_expires_ms=tonumber(ARGV[4])}),'PXAT',ARGV[3])
return 1").key(memo).key(claims_key).arg(fingerprint).arg(raw).arg(expires_ms.saturating_add(86_400_000)).arg(claim_expires_ms).arg(agent)
        .invoke(conn).map_err(|_sanitized|ReplayHubError::Unavailable)?;
    // Missing reservation is not evidence of safely recoverable grant.
    // Caller keeps request pending rather than silently repeating it.
    if completed == 1 {
        Ok(response)
    } else if completed == 2 {
        Ok(ReplayResponse::Superseded)
    } else {
        Ok(ReplayResponse::Pending)
    }
}
#[cfg(test)]
mod backend_tests {
    use super::*;
    use crate::outbox::{ClientSurface, Journal, ReplayFailure, ReplayTransport};
    use std::cell::RefCell;
    use uuid::Uuid;

    fn unavailable_pg(settings: &Settings) -> String {
        let mut url = url::Url::parse(
            settings
                .database_url
                .as_deref()
                .expect("PG fixture required"),
        )
        .expect("validated disposable PG URL");
        // Local disposable PostgreSQL may trust loopback; a fresh absent database
        // refuses connection independently of its password authentication policy.
        url.set_path(&format!("/unavailable_fixture_{}", Uuid::new_v4().simple()));
        url.into()
    }
    fn fixture() -> (Settings, redis::Connection) {
        let mut settings = Settings::for_test();
        settings.redis_url = crate::test_support::backend_url(crate::test_support::REDIS_URL_VAR);
        settings.database_url = Some(crate::test_support::backend_url(
            crate::test_support::DATABASE_URL_VAR,
        ));
        settings.allow_remote = true;
        settings.auth_token = Some("fixture-only-bearer".to_owned());
        settings.hub_identity = Some("disposable-outbox".to_owned());
        let scope = Uuid::new_v4().simple().to_string();
        settings.stream_key = format!("fixture:{scope}:messages");
        settings.channel_key = format!("fixture:{scope}:events");
        settings.presence_prefix = format!("fixture:{scope}:presence:");

        let mut conn = redis::Client::open(settings.redis_url.as_str())
            .unwrap()
            .get_connection()
            .unwrap();
        conn.set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        conn.set_write_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let pong: String = redis::cmd("PING").query(&mut conn).unwrap();
        assert_eq!(pong, "PONG");
        (settings, conn)
    }
    fn request(conn: &mut redis::Connection, operation: Operation, args: Value) -> ReplayRequest {
        let (seconds, micros): (u64, u64) = redis::cmd("TIME").query(conn).unwrap();
        let now = seconds * 1000 + micros / 1000;
        ReplayRequest {
            request_id: Uuid::now_v7(),
            origin_id: Uuid::new_v4(),
            sequence: 1,
            created_ms: now,
            expires_ms: if matches!(operation, Operation::Send | Operation::Ack) {
                0
            } else {
                now + 60_000
            },
            operation,
            surface: ClientSurface::Cli,
            arguments: match args {
                Value::Object(arguments) => arguments,
                _ => panic!("fixture requires object"),
            },
        }
    }
    fn envelope(settings: &Settings, request: ReplayRequest) -> ReplayEnvelope {
        ReplayEnvelope {
            hub_identity: settings.hub_identity.clone().unwrap(),
            request,
        }
    }
    fn pg(settings: &Settings) -> postgres::Client {
        postgres::Client::connect(settings.database_url.as_ref().unwrap(), postgres::NoTls).unwrap()
    }
    fn count(conn: &mut redis::Connection, key: &str) -> u64 {
        redis::cmd("XLEN").arg(key).query(conn).unwrap()
    }

    #[test]
    #[ignore = "requires disposable Redis and PostgreSQL"]
    fn outbox_cached_claim_refuses_release_replacement_contest_and_resolution_without_regrant() {
        for action in ["release", "replace", "contest", "resolve"] {
            let (mut settings, mut conn) = fixture();
            settings.database_url = None;
            let resource = format!("fixture-{}", Uuid::new_v4());
            let env = envelope(
                &settings,
                request(
                    &mut conn,
                    Operation::ClaimRequest,
                    json!({"agent":"fixture","resource":resource,"lease_ttl_seconds":60}),
                ),
            );
            let ReplayResponse::Applied { result } =
                accept(&mut conn, &settings, &env, true).unwrap()
            else {
                panic!("initial authority not established")
            };
            assert_eq!(result["status"], "granted");
            let dispatch = McpToolDispatch::new(&settings);
            match action {
                "release" => {
                    dispatch
                        .dispatch_tool(
                            "release_claim",
                            json!({"agent":"fixture","resource":resource})
                                .as_object()
                                .unwrap(),
                        )
                        .unwrap();
                }
                "replace" => {
                    dispatch.dispatch_tool("claim_resource",json!({"agent":"fixture","resource":resource,"reason":"replacement","lease_ttl_seconds":60}).as_object().unwrap()).unwrap();
                }
                _ => {
                    dispatch
                        .dispatch_tool(
                            "claim_resource",
                            json!({"agent":"contender","resource":resource,"lease_ttl_seconds":60})
                                .as_object()
                                .unwrap(),
                        )
                        .unwrap();
                    if action == "resolve" {
                        dispatch.dispatch_tool("resolve_claim",json!({"winner":"contender","resource":resource,"reason":"fixture","resolved_by":"fixture-operator"}).as_object().unwrap()).unwrap();
                    }
                }
            }
            let before = serde_json::to_value(
                crate::channels::list_claims(&settings, Some(&resource), None).unwrap(),
            )
            .unwrap();
            assert_eq!(
                accept(&mut conn, &settings, &env, true).unwrap(),
                ReplayResponse::Superseded,
                "{action}: cached grant is stale"
            );
            assert_eq!(
                accept(&mut conn, &settings, &env, true).unwrap(),
                ReplayResponse::Superseded,
                "{action}: repeat must not regrant"
            );
            let after = serde_json::to_value(
                crate::channels::list_claims(&settings, Some(&resource), None).unwrap(),
            )
            .unwrap();
            assert_eq!(before, after, "{action}: replay changed authority");
        }
    }

    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_lost_reply_journal_restart_preserves_one_message_notification_and_pg_row() {
        struct LostReply {
            settings: Settings,
            conn: RefCell<redis::Connection>,
            first: std::cell::Cell<bool>,
        }
        impl ReplayTransport for LostReply {
            fn replay(
                &self,
                request: &ReplayRequest,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                let response = accept(
                    &mut self.conn.borrow_mut(),
                    &self.settings,
                    &envelope(&self.settings, request.clone()),
                    true,
                )
                .unwrap();
                if self.first.replace(false) {
                    Err(ReplayFailure::Transport)
                } else {
                    Ok(response)
                }
            }
        }
        let (settings, mut conn) = fixture();
        let recipient = format!("receiver-{}", Uuid::new_v4());
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("journal");
        crate::outbox_private::create_directory(&private).unwrap();
        let path = private.join("queue.jsonl");
        let mut journal = Journal::open(&path, "fixture-destination").unwrap();
        let (seconds, micros): (u64, u64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now = seconds * 1000 + micros / 1000;
        let retained=journal.enqueue(Operation::Send,ClientSurface::Cli,
            json!({"sender":"fixture","recipient":recipient,"topic":"status","body":"one","request_ack":true}).as_object().unwrap().clone(),now).unwrap();
        let transport = LostReply {
            settings: settings.clone(),
            conn: RefCell::new(
                redis::Client::open(settings.redis_url.as_str())
                    .unwrap()
                    .get_connection()
                    .unwrap(),
            ),
            first: std::cell::Cell::new(true),
        };
        let report = journal.flush(&transport, now, 16).unwrap();
        assert_eq!((report.applied, report.remaining), (0, 1));
        drop(journal);
        let mut recovered = Journal::open(&path, "fixture-destination").unwrap();
        assert_eq!(
            recovered.pending().next().unwrap().request_id,
            retained.request_id
        );
        let report = recovered.flush(&transport, now + 1, 16).unwrap();
        assert_eq!((report.applied, report.remaining), (1, 0));
        assert_eq!(count(&mut conn, &settings.stream_key), 1);
        assert_eq!(count(&mut conn, &notification_stream_key(&recipient)), 1);
        let row = pg(&settings)
            .query_one(
                &format!(
                    "select count(*) from {} where client_msg_id=$1",
                    settings.message_table
                ),
                &[&retained.request_id.to_string()],
            )
            .unwrap();
        assert_eq!(row.get::<_, i64>(0), 1);
    }

    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_concurrent_duplicate_and_payload_collision_cannot_add_second_message() {
        let (settings, mut conn) = fixture();
        let req = request(
            &mut conn,
            Operation::Send,
            json!({"sender":"fixture","recipient":"all","topic":"status","body":"concurrent"}),
        );
        let env = envelope(&settings, req);
        let workers = (0..4)
            .map(|_| {
                let settings = settings.clone();
                let env = env.clone();
                std::thread::spawn(move || {
                    let mut conn = redis::Client::open(settings.redis_url.as_str())
                        .unwrap()
                        .get_connection()
                        .unwrap();
                    accept(&mut conn, &settings, &env, true).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let responses = workers
            .into_iter()
            .map(|child| child.join().unwrap())
            .collect::<Vec<_>>();
        assert!(responses.iter().all(|response| response == &responses[0]));
        assert_eq!(count(&mut conn, &settings.stream_key), 1);
        let mut changed = env.clone();
        changed
            .request
            .arguments
            .insert("body".to_owned(), json!("different"));
        assert!(matches!(
            accept(&mut conn, &settings, &changed, true),
            Err(ReplayHubError::Collision)
        ));
        let mut wrong = env;
        wrong.hub_identity = "different-hub".to_owned();
        assert!(matches!(
            accept(&mut conn, &settings, &wrong, true),
            Err(ReplayHubError::Identity)
        ));
        assert_eq!(count(&mut conn, &settings.stream_key), 1);
    }

    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_cached_presence_pg_handoff_is_one_original_row_and_pg_failure_stays_pending() {
        let (settings, mut conn) = fixture();
        let req = request(
            &mut conn,
            Operation::Presence,
            json!({"agent":"fixture","status":"online","ttl_seconds":60,"capabilities":["mcp"]}),
        );
        let env = envelope(&settings, req);
        let first = accept(&mut conn, &settings, &env, true).unwrap();
        let second = accept(&mut conn, &settings, &env, true).unwrap();
        assert_eq!(first, second);
        let row=pg(&settings).query_one(&format!("select count(*),min(timestamp_utc),min(ttl_seconds) from {} where replay_request_id=$1 and replay_hub=$2",settings.presence_event_table),&[&env.request.request_id,&env.hub_identity]).unwrap();
        assert_eq!(row.get::<_, i64>(0), 1);
        assert_eq!(row.get::<_, i64>(2), 60);
        assert_eq!(
            row.get::<_, chrono::DateTime<Utc>>(1).timestamp_millis(),
            i64::try_from(env.request.created_ms).unwrap()
        );
        let mut unavailable = settings.clone();
        unavailable.database_url = Some(unavailable_pg(&settings));
        assert_eq!(
            accept(&mut conn, &unavailable, &env, true).unwrap(),
            ReplayResponse::Pending
        );
        assert_eq!(accept(&mut conn, &settings, &env, true).unwrap(), first);
        let total = pg(&settings)
            .query_one(
                &format!(
                    "select count(*) from {} where replay_request_id=$1",
                    settings.presence_event_table
                ),
                &[&env.request.request_id],
            )
            .unwrap();
        assert_eq!(total.get::<_, i64>(0), 1);
    }

    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_presence_expiry_and_newer_live_state_prevent_resurrection() {
        let (settings, mut conn) = fixture();
        let mut old = request(
            &mut conn,
            Operation::Presence,
            json!({"agent":"fixture","status":"offline","ttl_seconds":60}),
        );
        let mut fresh = old.clone();
        fresh.request_id = Uuid::now_v7();
        fresh.sequence = 2;
        fresh.created_ms += 1;
        fresh.expires_ms += 1;
        fresh.arguments.insert("status".to_owned(), json!("online"));
        let applied = accept(&mut conn, &settings, &envelope(&settings, fresh), true).unwrap();
        assert!(matches!(applied, ReplayResponse::Applied { .. }));
        assert_eq!(
            accept(
                &mut conn,
                &settings,
                &envelope(&settings, old.clone()),
                true
            )
            .unwrap(),
            ReplayResponse::Superseded
        );
        let live: String = redis::cmd("GET")
            .arg(format!("{}fixture", settings.presence_prefix))
            .query(&mut conn)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Presence>(&live).unwrap().status,
            "online"
        );
        old.request_id = Uuid::now_v7();
        old.created_ms -= 70_000;
        old.expires_ms -= 70_000;
        old.arguments
            .insert("agent".to_owned(), json!("expired-agent"));
        assert_eq!(
            accept(&mut conn, &settings, &envelope(&settings, old), true).unwrap(),
            ReplayResponse::Superseded
        );
        let exists: bool = redis::cmd("EXISTS")
            .arg(format!("{}expired-agent", settings.presence_prefix))
            .query(&mut conn)
            .unwrap();
        assert!(!exists);
    }
    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_redis_accept_pg_failure_retains_attempt_and_recovers_original_presence_after_expiry()
    {
        struct Recover {
            settings: Settings,
            conn: RefCell<redis::Connection>,
            unavailable: std::cell::Cell<bool>,
        }
        impl ReplayTransport for Recover {
            fn replay(
                &self,
                request: &ReplayRequest,
            ) -> std::result::Result<ReplayResponse, ReplayFailure> {
                let mut settings = self.settings.clone();
                if self.unavailable.get() {
                    settings.database_url = Some(unavailable_pg(&settings));
                }
                let result = accept(
                    &mut self.conn.borrow_mut(),
                    &settings,
                    &envelope(&settings, request.clone()),
                    true,
                )
                .unwrap();
                if self.unavailable.get() {
                    assert_eq!(result, ReplayResponse::Pending);
                    Err(ReplayFailure::Transport)
                } else {
                    Ok(result)
                }
            }
        }
        let (settings, mut conn) = fixture();
        let (seconds, micros): (u64, u64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now = seconds * 1000 + micros / 1000;
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("journal");
        crate::outbox_private::create_directory(&private).unwrap();
        let path = private.join("queue.jsonl");
        let mut journal = Journal::open(&path, "fixture").unwrap();
        let req = journal
            .enqueue(
                Operation::Presence,
                ClientSurface::Cli,
                json!({"agent":"crash-window-fixture","ttl_seconds":1})
                    .as_object()
                    .unwrap()
                    .clone(),
                now,
            )
            .unwrap();
        let transport = Recover {
            settings: settings.clone(),
            conn: RefCell::new(
                redis::Client::open(settings.redis_url.as_str())
                    .unwrap()
                    .get_connection()
                    .unwrap(),
            ),
            unavailable: std::cell::Cell::new(true),
        };
        let report = journal.flush(&transport, now, 16).unwrap();
        assert_eq!((report.applied, report.remaining), (0, 1));
        // Real Lua acceptance happened while PG authentication failed. No final
        // HTTPApplied/client settlement is inferred from a cached Redis result.
        let memo: String = redis::cmd("GET")
            .arg(memo_key(&settings, &req))
            .query(&mut conn)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&memo).unwrap()["state"],
            "applied"
        );
        drop(journal);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let exists: bool = redis::cmd("EXISTS")
            .arg(format!("{}crash-window-fixture", settings.presence_prefix))
            .query(&mut conn)
            .unwrap();
        assert!(!exists);
        let mut recovered = Journal::open(&path, "fixture").unwrap();
        transport.unavailable.set(false);
        let report = recovered.flush(&transport, now + 1100, 16).unwrap();
        assert_eq!((report.applied, report.remaining), (1, 0));
        assert!(
            !redis::cmd("EXISTS")
                .arg(format!("{}crash-window-fixture", settings.presence_prefix))
                .query::<bool>(&mut conn)
                .unwrap(),
            "cached repair must not resurrect expired presence"
        );
        let row=pg(&settings).query_one(&format!("select count(*),min(timestamp_utc),min(ttl_seconds) from {} where replay_request_id=$1",settings.presence_event_table),&[&req.request_id]).unwrap();
        assert_eq!(row.get::<_, i64>(0), 1);
        assert_eq!(row.get::<_, i64>(2), 1);
        assert_eq!(
            row.get::<_, chrono::DateTime<Utc>>(1).timestamp_millis(),
            i64::try_from(now).unwrap()
        );
    }

    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_ack_lost_reply_cannot_repeat_stream_event_or_pending_clear() {
        let (settings, mut conn) = fixture();
        let env = envelope(
            &settings,
            request(
                &mut conn,
                Operation::Send,
                json!({"sender":"fixture","recipient":"receiver","topic":"status","body":"ack me","request_ack":true}),
            ),
        );
        let ReplayResponse::Applied { result } = accept(&mut conn, &settings, &env, true).unwrap()
        else {
            panic!("send not applied");
        };
        let id = result["id"].as_str().unwrap();
        assert!(
            redis::cmd("EXISTS")
                .arg(format!("agent_bus:pending_ack:{id}"))
                .query::<bool>(&mut conn)
                .unwrap()
        );
        let ack = envelope(
            &settings,
            request(
                &mut conn,
                Operation::Ack,
                json!({"agent":"receiver","message_id":id}),
            ),
        );
        let first = accept(&mut conn, &settings, &ack, true).unwrap();
        let second = accept(&mut conn, &settings, &ack, true).unwrap();
        assert_eq!(first, second);
        assert_eq!(count(&mut conn, &settings.stream_key), 2);
        assert!(
            !redis::cmd("EXISTS")
                .arg(format!("agent_bus:pending_ack:{id}"))
                .query::<bool>(&mut conn)
                .unwrap()
        );
        assert!(
            !redis::cmd("EXISTS")
                .arg(format!("agent_bus:ack_deadline:{id}"))
                .query::<bool>(&mut conn)
                .unwrap()
        );
    }

    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_cached_claim_success_after_expiry_is_not_an_active_grant() {
        let (mut settings, mut conn) = fixture();
        settings.database_url = None;
        let mut req = request(
            &mut conn,
            Operation::ClaimRequest,
            json!({"agent":"fixture","resource":format!("fixture-{}",Uuid::new_v4()),"lease_ttl_seconds":4}),
        );
        req.expires_ms = req.created_ms + 4000;
        let env = envelope(&settings, req);
        let first = accept(&mut conn, &settings, &env, true).unwrap();
        assert!(matches!(first, ReplayResponse::Applied { .. }));
        assert_eq!(accept(&mut conn, &settings, &env, true).unwrap(), first);
        let ReplayResponse::Applied { result } = &first else {
            unreachable!()
        };
        let lease_expiry = u64::try_from(
            chrono::DateTime::parse_from_rfc3339(result["expires_at"].as_str().unwrap())
                .unwrap()
                .timestamp_millis(),
        )
        .unwrap();
        assert!(
            lease_expiry < env.request.expires_ms,
            "flooring TTL creates earlier actual lease expiry"
        );
        let (seconds, micros): (u64, u64) = redis::cmd("TIME").query(&mut conn).unwrap();
        let now = seconds * 1000 + micros / 1000;
        std::thread::sleep(std::time::Duration::from_millis(
            lease_expiry.saturating_sub(now) + 50,
        ));
        let (seconds, micros): (u64, u64) = redis::cmd("TIME").query(&mut conn).unwrap();
        assert!(
            seconds * 1000 + micros / 1000 < env.request.expires_ms,
            "actual lease expiry must be tested before request expiry"
        );
        assert_eq!(
            accept(&mut conn, &settings, &env, true).unwrap(),
            ReplayResponse::Superseded
        );
    }

    #[test]
    #[ignore = "requires owned disposable Redis and PostgreSQL"]
    fn outbox_presence_memo_has_no_day_cutoff_and_preaged_cached_history_can_repair() {
        let (settings, mut conn) = fixture();
        let fresh = envelope(
            &settings,
            request(
                &mut conn,
                Operation::Presence,
                json!({"agent":"memo-fixture","ttl_seconds":60}),
            ),
        );
        let ReplayResponse::Applied { result } =
            accept(&mut conn, &settings, &fresh, true).unwrap()
        else {
            panic!("presence not applied");
        };
        assert_eq!(
            redis::cmd("PTTL")
                .arg(memo_key(&settings, &fresh.request))
                .query::<i64>(&mut conn)
                .unwrap(),
            -1,
            "actual accepted presence cache must not expire before durable repair"
        );
        // Pre-aged external cache fixture exercises an actual server TIME more
        // than 24 hours beyond original expiry, without a fake clock or delay.
        let mut old = fresh.request.clone();
        old.request_id = Uuid::now_v7();
        old.created_ms -= 26 * 60 * 60 * 1000;
        old.expires_ms = old.created_ms + 60_000;
        let mut presence: Presence = serde_json::from_value(result).unwrap();
        presence.timestamp_utc = timestamp(old.created_ms).unwrap();
        presence.session_id = old.request_id.to_string();
        let response = ReplayResponse::Applied {
            result: serde_json::to_value(&presence).unwrap(),
        };
        let memo=json!({"state":"applied","fingerprint":old.fingerprint().unwrap(),"response":serde_json::to_string(&response).unwrap()}).to_string();
        redis::cmd("SET")
            .arg(memo_key(&settings, &old))
            .arg(memo)
            .query::<()>(&mut conn)
            .unwrap();
        assert_eq!(
            accept(
                &mut conn,
                &settings,
                &envelope(&settings, old.clone()),
                true
            )
            .unwrap(),
            response
        );
        let row = pg(&settings)
            .query_one(
                &format!(
                    "select count(*),min(timestamp_utc) from {} where replay_request_id=$1",
                    settings.presence_event_table
                ),
                &[&old.request_id],
            )
            .unwrap();
        assert_eq!(row.get::<_, i64>(0), 1);
        assert_eq!(
            row.get::<_, chrono::DateTime<Utc>>(1).timestamp_millis(),
            i64::try_from(old.created_ms).unwrap()
        );
    }
}
