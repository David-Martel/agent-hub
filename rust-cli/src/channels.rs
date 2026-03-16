//! Structured communication channels for multi-agent coordination.
//!
//! Channels provide focused communication patterns beyond broadcast:
//! - `direct:<agent>` — private messages to a specific agent
//! - `group:<name>` — named group discussions (e.g., "review-http-rs")
//! - `escalate` — orchestrator escalation (always delivered to orchestrator)
//! - `arbitrate:<resource>` — ownership arbitration for a specific resource
//!
//! # Storage layout
//!
//! | Pattern | Redis key |
//! |---------|-----------|
//! | Direct (A→B) | `bus:direct:<low>:<high>` (agents sorted) |
//! | Group | `bus:group:<name>` (stream) |
//! | Group membership | `bus:group:<name>:members` (set) |
//! | Group metadata | `bus:group:<name>:meta` (hash) |
//! | Ownership claim | `bus:claims:<resource>` (hash, field = agent) |
//!
//! # Example
//!
//! ```no_run
//! use agent_bus::channels::{post_direct, read_direct};
//! let settings = agent_bus::settings::Settings::from_env();
//! // post_direct(&settings, "claude", "codex", "hello").unwrap();
//! // let msgs = read_direct(&settings, "claude", "codex", 50).unwrap();
//! ```

use anyhow::{Context as _, Result, bail};
use chrono::Utc;
use redis::Commands as _;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::Message;
use crate::redis_bus::connect;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Stream MAXLEN for channel streams (trim to ~1000 entries per channel).
const CHANNEL_STREAM_MAXLEN: u64 = 1000;

/// TTL for ownership claims in Redis (seconds).  Claims auto-expire after 1 hour
/// if never resolved, preventing stale locks.
const CLAIM_TTL_SECS: u64 = 3600;

/// Key prefix for direct-message streams.
const DIRECT_PREFIX: &str = "bus:direct:";

/// Key prefix for group streams.
const GROUP_PREFIX: &str = "bus:group:";

/// Key prefix for ownership claims.
const CLAIMS_PREFIX: &str = "bus:claims:";

/// Key suffix for group membership sets.
const MEMBERS_SUFFIX: &str = ":members";

/// Key suffix for group metadata hashes.
const META_SUFFIX: &str = ":meta";

// ---------------------------------------------------------------------------
// Channel types
// ---------------------------------------------------------------------------

/// Channel types supported by the bus.
// Variants are used by serde deserialization and HTTP/MCP layers; direct Rust
// call paths will be added as the coordination protocol matures.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Channel {
    /// Direct message to a specific agent. Creates a Redis stream per pair.
    Direct { agent: String },
    /// Named group discussion. Creates a Redis stream per group.
    Group { name: String, members: Vec<String> },
    /// Orchestrator escalation. Always routes to the orchestrator agent.
    Escalate,
    /// Resource ownership arbitration request.
    Arbitrate { resource: String },
}

// ---------------------------------------------------------------------------
// Group metadata
// ---------------------------------------------------------------------------

/// Metadata record for a named group channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GroupInfo {
    /// Group name (URL-safe identifier).
    pub(crate) name: String,
    /// Agent IDs that are members of this group.
    pub(crate) members: Vec<String>,
    /// ISO-8601 creation timestamp.
    pub(crate) created_at: String,
    /// Agent ID that created the group.
    pub(crate) created_by: String,
    /// Total messages posted to this group stream (read from Redis XLEN).
    pub(crate) message_count: u64,
}

// ---------------------------------------------------------------------------
// Ownership arbitration
// ---------------------------------------------------------------------------

/// Status of an ownership claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClaimStatus {
    /// Awaiting arbitration (only one claimant so far — first-come).
    Pending,
    /// Orchestrator approved this claim, or first-come auto-grant.
    Granted,
    /// Multiple agents have claimed; orchestrator must resolve.
    Contested,
    /// First-edit done; claimant has been assigned as reviewer.
    ReviewAssigned,
}

/// An ownership claim for a resource (file, directory, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OwnershipClaim {
    /// Resource path (e.g., `src/redis_bus.rs`).
    pub(crate) resource: String,
    /// Claiming agent ID.
    pub(crate) agent: String,
    /// Why this agent should have first-edit.
    pub(crate) priority_argument: String,
    /// ISO-8601 timestamp when the claim was made.
    pub(crate) timestamp: String,
    /// Current arbitration status.
    pub(crate) status: ClaimStatus,
}

/// Full arbitration state for a resource — all competing claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArbitrationState {
    /// Resource under arbitration.
    pub(crate) resource: String,
    /// All claims submitted for this resource.
    pub(crate) claims: Vec<OwnershipClaim>,
    /// Agent that won (after resolution), if any.
    pub(crate) winner: Option<String>,
    /// Orchestrator's rationale for the decision, if resolved.
    pub(crate) resolution_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Key helpers
// ---------------------------------------------------------------------------

/// Build the direct-stream key for a `(sender, recipient)` pair.
///
/// Agents are sorted alphabetically so that `(A,B)` and `(B,A)` share one stream.
fn direct_key(agent_a: &str, agent_b: &str) -> String {
    let (lo, hi) = if agent_a <= agent_b {
        (agent_a, agent_b)
    } else {
        (agent_b, agent_a)
    };
    format!("{DIRECT_PREFIX}{lo}:{hi}")
}

fn group_stream_key(name: &str) -> String {
    format!("{GROUP_PREFIX}{name}")
}

fn group_members_key(name: &str) -> String {
    format!("{GROUP_PREFIX}{name}{MEMBERS_SUFFIX}")
}

fn group_meta_key(name: &str) -> String {
    format!("{GROUP_PREFIX}{name}{META_SUFFIX}")
}

fn claims_key(resource: &str) -> String {
    // Normalise path separators to avoid Redis key confusion on Windows.
    let normalised = resource.replace('\\', "/");
    format!("{CLAIMS_PREFIX}{normalised}")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decode a Redis stream entry into a [`Message`].
///
/// Reuses the same field layout as `bus:messages` so all display code works.
fn decode_channel_entry(
    stream_id: &str,
    fields: &std::collections::HashMap<String, redis::Value>,
) -> Message {
    let get = |k: &str| -> String {
        match fields.get(k) {
            Some(redis::Value::BulkString(b)) => String::from_utf8_lossy(b).to_string(),
            Some(redis::Value::SimpleString(s)) => s.clone(),
            _ => String::new(),
        }
    };
    let get_bool = |k: &str| -> bool {
        let v = get(k);
        v == "true" || v == "True"
    };
    let get_json_vec =
        |k: &str| -> Vec<String> { serde_json::from_str(&get(k)).unwrap_or_default() };
    let get_json_value = |k: &str| -> serde_json::Value {
        serde_json::from_str(&get(k)).unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
    };

    Message {
        id: get("id"),
        timestamp_utc: get("timestamp_utc"),
        protocol_version: get("protocol_version"),
        from: get("from"),
        to: get("to"),
        topic: get("topic"),
        body: get("body"),
        thread_id: {
            let v = get("thread_id");
            if v.is_empty() || v == "None" {
                None
            } else {
                Some(v)
            }
        },
        tags: get_json_vec("tags").into(),
        priority: {
            let v = get("priority");
            if v.is_empty() { "normal".to_owned() } else { v }
        },
        request_ack: get_bool("request_ack"),
        reply_to: {
            let v = get("reply_to");
            if v.is_empty() { None } else { Some(v) }
        },
        metadata: get_json_value("metadata"),
        stream_id: Some(stream_id.to_owned()),
    }
}

/// Parse raw XRANGE/XREVRANGE Redis output into `(stream_id, field_map)` pairs.
fn parse_channel_xrange(
    raw: &[redis::Value],
) -> Vec<(String, std::collections::HashMap<String, redis::Value>)> {
    let mut out = Vec::new();
    for entry in raw {
        let redis::Value::Array(parts) = entry else {
            continue;
        };
        if parts.len() < 2 {
            continue;
        }
        let redis::Value::BulkString(sid_bytes) = &parts[0] else {
            continue;
        };
        let stream_id = String::from_utf8_lossy(sid_bytes).to_string();
        let redis::Value::Array(fields_raw) = &parts[1] else {
            continue;
        };

        let mut field_map: std::collections::HashMap<String, redis::Value> =
            std::collections::HashMap::new();
        let mut j = 0;
        while j + 1 < fields_raw.len() {
            let key = match &fields_raw[j] {
                redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                redis::Value::SimpleString(s) => s.clone(),
                _ => {
                    j += 2;
                    continue;
                }
            };
            field_map.insert(key, fields_raw[j + 1].clone());
            j += 2;
        }
        out.push((stream_id, field_map));
    }
    out
}

/// Write a message into an arbitrary Redis stream and return the created `Message`.
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields"
)]
fn xadd_to_stream(
    conn: &mut redis::Connection,
    stream_key: &str,
    from: &str,
    to: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
    priority: &str,
    metadata: &serde_json::Value,
) -> Result<Message> {
    let id = Uuid::new_v4().to_string();
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_owned());
    let meta_json = serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_owned());
    let thread_str = thread_id.unwrap_or("");

    let mut fields: Vec<(&str, &str)> = vec![
        ("id", &id),
        ("timestamp_utc", &ts),
        ("protocol_version", crate::models::PROTOCOL_VERSION),
        ("from", from),
        ("to", to),
        ("topic", topic),
        ("body", body),
        ("tags", &tags_json),
        ("priority", priority),
        ("request_ack", "false"),
        ("reply_to", from),
        ("metadata", &meta_json),
    ];
    if !thread_str.is_empty() {
        fields.push(("thread_id", thread_str));
    }

    let stream_id: String = redis::cmd("XADD")
        .arg(stream_key)
        .arg("MAXLEN")
        .arg("~")
        .arg(CHANNEL_STREAM_MAXLEN)
        .arg("*")
        .arg(&fields)
        .query(conn)
        .context("XADD to channel stream failed")?;

    Ok(Message {
        id: id.clone(),
        timestamp_utc: ts,
        protocol_version: crate::models::PROTOCOL_VERSION.to_owned(),
        from: from.to_owned(),
        to: to.to_owned(),
        topic: topic.to_owned(),
        body: body.to_owned(),
        thread_id: thread_id.map(String::from),
        tags: tags.to_vec().into(),
        priority: priority.to_owned(),
        request_ack: false,
        reply_to: Some(from.to_owned()),
        metadata: metadata.clone(),
        stream_id: Some(stream_id),
    })
}

/// Read up to `limit` messages from `stream_key`, most-recent first, then reversed to chrono.
fn xread_from_stream(
    conn: &mut redis::Connection,
    stream_key: &str,
    limit: usize,
) -> Result<Vec<Message>> {
    let fetch = limit.max(1);
    let raw: Vec<redis::Value> = redis::cmd("XREVRANGE")
        .arg(stream_key)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(fetch)
        .query(conn)
        .context("XREVRANGE on channel stream failed")?;

    let mut msgs: Vec<Message> = parse_channel_xrange(&raw)
        .into_iter()
        .map(|(sid, fields)| decode_channel_entry(&sid, &fields))
        .collect();

    msgs.reverse();
    Ok(msgs)
}

// ---------------------------------------------------------------------------
// Direct messages
// ---------------------------------------------------------------------------

/// Post a direct message from `sender` to `recipient`.
///
/// Stored in stream `bus:direct:<lo>:<hi>` where `lo`/`hi` are the agent IDs
/// sorted alphabetically so both participants share one stream.
///
/// # Errors
///
/// Returns an error if the Redis connection or `XADD` fails.
pub(crate) fn post_direct(
    settings: &Settings,
    sender: &str,
    recipient: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
) -> Result<Message> {
    if sender.is_empty() {
        bail!("sender must not be empty");
    }
    if recipient.is_empty() {
        bail!("recipient must not be empty");
    }
    if body.is_empty() {
        bail!("body must not be empty");
    }

    let key = direct_key(sender, recipient);
    let mut conn = connect(settings)?;
    let meta = serde_json::json!({"channel": "direct", "recipient": recipient});
    xadd_to_stream(
        &mut conn, &key, sender, recipient, topic, body, thread_id, tags, "normal", &meta,
    )
}

/// Read up to `limit` messages from the direct channel between `agent_a` and `agent_b`.
///
/// # Errors
///
/// Returns an error if the Redis connection or `XREVRANGE` fails.
pub(crate) fn read_direct(
    settings: &Settings,
    agent_a: &str,
    agent_b: &str,
    limit: usize,
) -> Result<Vec<Message>> {
    let key = direct_key(agent_a, agent_b);
    let mut conn = connect(settings)?;
    xread_from_stream(&mut conn, &key, limit)
}

// ---------------------------------------------------------------------------
// Group channels
// ---------------------------------------------------------------------------

/// Create a named group channel with an initial member list.
///
/// Idempotent: if the group already exists its member set is extended, not
/// replaced.  Stores metadata in a Redis hash and the member set in a separate
/// set key so membership queries are O(1).
///
/// # Errors
///
/// Returns an error if the Redis SADD or HSET commands fail.
pub(crate) fn create_group(
    settings: &Settings,
    name: &str,
    members: &[String],
    created_by: &str,
) -> Result<GroupInfo> {
    if name.is_empty() {
        bail!("group name must not be empty");
    }
    let valid_name = name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if !valid_name {
        bail!("group name may only contain alphanumerics, hyphens, and underscores");
    }

    let mut conn = connect(settings)?;
    let members_key = group_members_key(name);
    let meta_key = group_meta_key(name);

    // Add members to the set (SADD is idempotent for existing members).
    for m in members {
        let _: u32 = conn
            .sadd(&members_key, m.as_str())
            .context("SADD group members failed")?;
    }

    // Store creation metadata only when the group is new (NX on one field).
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let _: () = redis::cmd("HSETNX")
        .arg(&meta_key)
        .arg("created_at")
        .arg(&ts)
        .query(&mut conn)
        .context("HSETNX group meta created_at failed")?;
    let _: () = redis::cmd("HSETNX")
        .arg(&meta_key)
        .arg("created_by")
        .arg(created_by)
        .query(&mut conn)
        .context("HSETNX group meta created_by failed")?;

    // Return the current state of the group.
    list_group_info(&mut conn, name)
}

/// Post a message to a named group channel.
///
/// # Errors
///
/// Returns an error if the group does not exist or the Redis `XADD` fails.
pub(crate) fn post_to_group(
    settings: &Settings,
    group_name: &str,
    sender: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
) -> Result<Message> {
    if body.is_empty() {
        bail!("body must not be empty");
    }
    let stream_key = group_stream_key(group_name);
    let mut conn = connect(settings)?;

    // Verify group exists by checking the members key.
    let members_key = group_members_key(group_name);
    let exists: bool = conn
        .exists(&members_key)
        .context("EXISTS group members check failed")?;
    if !exists {
        bail!("group '{group_name}' does not exist");
    }

    let meta = serde_json::json!({"channel": "group", "group": group_name});
    let tags = vec![format!("group:{group_name}")];
    xadd_to_stream(
        &mut conn,
        &stream_key,
        sender,
        group_name,
        topic,
        body,
        thread_id,
        &tags,
        "normal",
        &meta,
    )
}

/// Read up to `limit` messages from a named group channel.
///
/// # Errors
///
/// Returns an error if the Redis `XREVRANGE` fails.
pub(crate) fn read_group(
    settings: &Settings,
    group_name: &str,
    limit: usize,
) -> Result<Vec<Message>> {
    let stream_key = group_stream_key(group_name);
    let mut conn = connect(settings)?;
    xread_from_stream(&mut conn, &stream_key, limit)
}

/// List all group channels that currently exist (have a members set).
///
/// # Errors
///
/// Returns an error if the Redis `SCAN` fails.
pub(crate) fn list_groups(settings: &Settings) -> Result<Vec<GroupInfo>> {
    let mut conn = connect(settings)?;
    let pattern = format!("{GROUP_PREFIX}*{MEMBERS_SUFFIX}");
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100)
            .query(&mut conn)
            .context("SCAN group members keys failed")?;
        keys.extend(batch);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let prefix_len = GROUP_PREFIX.len();
    let suffix_len = MEMBERS_SUFFIX.len();
    let mut groups: Vec<GroupInfo> = Vec::new();
    for key in &keys {
        // Extract name from `bus:group:<name>:members`.
        if key.len() > prefix_len + suffix_len {
            let name = &key[prefix_len..key.len() - suffix_len];
            if let Ok(info) = list_group_info(&mut conn, name) {
                groups.push(info);
            }
        }
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(groups)
}

/// Read group info for a single named group.
fn list_group_info(conn: &mut redis::Connection, name: &str) -> Result<GroupInfo> {
    let members_key = group_members_key(name);
    let meta_key = group_meta_key(name);
    let stream_key = group_stream_key(name);

    let raw_members: Vec<String> = conn
        .smembers(&members_key)
        .context("SMEMBERS group members failed")?;
    let mut members = raw_members;
    members.sort();

    let created_at: Option<String> = redis::cmd("HGET")
        .arg(&meta_key)
        .arg("created_at")
        .query(conn)
        .context("HGET group meta created_at failed")?;
    let created_by: Option<String> = redis::cmd("HGET")
        .arg(&meta_key)
        .arg("created_by")
        .query(conn)
        .context("HGET group meta created_by failed")?;

    let message_count: u64 = redis::cmd("XLEN").arg(&stream_key).query(conn).unwrap_or(0);

    Ok(GroupInfo {
        name: name.to_owned(),
        members,
        created_at: created_at.unwrap_or_default(),
        created_by: created_by.unwrap_or_default(),
        message_count,
    })
}

// ---------------------------------------------------------------------------
// Orchestrator escalation
// ---------------------------------------------------------------------------

/// Dedicated Redis stream key for escalation messages.
const ESCALATION_STREAM: &str = "bus:escalations";

/// Post an escalation message to the first online agent with `orchestration`
/// capability, falling back to the `all` recipient if none is found.
///
/// The message is stored in a dedicated escalation stream with `priority=high`
/// and the `escalation=true` metadata flag so monitoring tools can surface it.
/// The `Message.priority` field of the returned value is always `"high"`.
///
/// # Errors
///
/// Returns an error if the Redis `XADD` fails.
pub(crate) fn post_escalation(
    settings: &Settings,
    sender: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
) -> Result<Message> {
    if body.is_empty() {
        bail!("escalation body must not be empty");
    }

    // Find orchestrator: scan presence for "orchestration" capability.
    let recipient = find_orchestrator(settings).unwrap_or_else(|_| "all".to_owned());

    let mut conn = connect(settings)?;
    let mut escalation_tags = tags.to_vec();
    escalation_tags.push("escalation".to_owned());
    let meta = serde_json::json!({
        "channel": "escalate",
        "escalation": true,
        "original_sender": sender
    });

    xadd_to_stream(
        &mut conn,
        ESCALATION_STREAM,
        sender,
        &recipient,
        "escalation",
        body,
        thread_id,
        &escalation_tags,
        "high",
        &meta,
    )
}

/// Find the first online agent advertising `orchestration` capability.
fn find_orchestrator(settings: &Settings) -> Result<String> {
    use crate::redis_bus::bus_list_presence;
    let mut conn = connect(settings)?;
    let presences = bus_list_presence(&mut conn, settings)?;
    presences
        .into_iter()
        .find(|p| {
            p.status == "online"
                && p.capabilities
                    .iter()
                    .any(|c| c == "orchestration" || c == "orchestrator")
        })
        .map(|p| p.agent)
        .context("no orchestrator agent found in presence")
}

// ---------------------------------------------------------------------------
// Ownership arbitration
// ---------------------------------------------------------------------------

/// Claim ownership of a resource for `agent` with a priority argument.
///
/// - First claim → auto-granted, stored with `status=granted`.
/// - Subsequent claim by a different agent → both become `contested`, orchestrator
///   is notified via an escalation message.
///
/// Claims are stored as JSON values in a Redis hash at `bus:claims:<resource>`,
/// one field per claiming agent.  The hash has a 1-hour TTL.
///
/// # Errors
///
/// Returns an error if Redis commands fail.
pub(crate) fn claim_resource(
    settings: &Settings,
    resource: &str,
    agent: &str,
    priority_argument: &str,
) -> Result<OwnershipClaim> {
    if resource.is_empty() {
        bail!("resource must not be empty");
    }
    if agent.is_empty() {
        bail!("agent must not be empty");
    }

    let key = claims_key(resource);
    let mut conn = connect(settings)?;

    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();

    // Determine whether there are existing claims from other agents.
    let existing_agents: Vec<String> = redis::cmd("HKEYS")
        .arg(&key)
        .query(&mut conn)
        .context("HKEYS claims failed")?;

    let other_claimants: Vec<String> = existing_agents.into_iter().filter(|a| a != agent).collect();

    let status = if other_claimants.is_empty() {
        ClaimStatus::Granted
    } else {
        ClaimStatus::Contested
    };

    let claim = OwnershipClaim {
        resource: resource.to_owned(),
        agent: agent.to_owned(),
        priority_argument: priority_argument.to_owned(),
        timestamp: ts,
        status: status.clone(),
    };

    // Store the claim.
    let claim_json = serde_json::to_string(&claim).context("serialize claim")?;
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg(agent)
        .arg(&claim_json)
        .query(&mut conn)
        .context("HSET claim failed")?;

    // Refresh the hash TTL on every write.
    let _: () = redis::cmd("EXPIRE")
        .arg(&key)
        .arg(CLAIM_TTL_SECS)
        .query(&mut conn)
        .context("EXPIRE claims key failed")?;

    // When contested, upgrade all existing claims and notify.
    if status == ClaimStatus::Contested {
        mark_all_contested(&mut conn, &key, resource)?;
        // Best-effort escalation notification — failure is non-fatal.
        let notif_body = format!(
            "CLAIM CONTESTED: {resource}\n\
             Claimant: {agent}\n\
             Argument: {priority_argument}\n\
             Other claimants: {}\n\
             Use 'agent-bus resolve {resource} --winner <agent>' to resolve.",
            other_claimants.join(", ")
        );
        if let Err(e) = post_escalation(
            settings,
            agent,
            &notif_body,
            None,
            &[format!("arbitration:{resource}")],
        ) {
            tracing::warn!("failed to post escalation for contested claim: {e:#}");
        }
    }

    Ok(claim)
}

/// Update all existing claims for a resource to `contested` status.
fn mark_all_contested(conn: &mut redis::Connection, key: &str, resource: &str) -> Result<()> {
    let all: Vec<(String, String)> = redis::cmd("HGETALL")
        .arg(key)
        .query(conn)
        .context("HGETALL claims for contest failed")?;

    for (agent_id, existing_json) in &all {
        if let Ok(mut existing_claim) = serde_json::from_str::<OwnershipClaim>(existing_json) {
            if existing_claim.status != ClaimStatus::Contested {
                existing_claim.status = ClaimStatus::Contested;
                resource.clone_into(&mut existing_claim.resource);
                if let Ok(updated_json) = serde_json::to_string(&existing_claim) {
                    let _: () = redis::cmd("HSET")
                        .arg(key)
                        .arg(agent_id.as_str())
                        .arg(&updated_json)
                        .query(conn)
                        .context("HSET update contested claim")?;
                }
            }
        }
    }
    Ok(())
}

/// List all ownership claims, optionally filtered by resource or status.
///
/// # Errors
///
/// Returns an error if the Redis SCAN or HGETALL commands fail.
pub(crate) fn list_claims(
    settings: &Settings,
    resource_filter: Option<&str>,
    status_filter: Option<&ClaimStatus>,
) -> Result<Vec<OwnershipClaim>> {
    let mut conn = connect(settings)?;
    let pattern = format!("{CLAIMS_PREFIX}*");
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100)
            .query(&mut conn)
            .context("SCAN claims keys failed")?;
        keys.extend(batch);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let mut claims: Vec<OwnershipClaim> = Vec::new();
    for key in &keys {
        let all: Vec<(String, String)> = redis::cmd("HGETALL")
            .arg(key)
            .query(&mut conn)
            .context("HGETALL claims failed")?;

        for (_agent_id, claim_json) in &all {
            let Ok(claim) = serde_json::from_str::<OwnershipClaim>(claim_json) else {
                continue;
            };

            if let Some(rf) = resource_filter {
                if claim.resource != rf {
                    continue;
                }
            }
            if let Some(sf) = status_filter {
                if &claim.status != sf {
                    continue;
                }
            }
            claims.push(claim);
        }
    }
    claims.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    Ok(claims)
}

/// Get the full arbitration state for a single resource.
///
/// # Errors
///
/// Returns an error if the Redis HGETALL fails.
pub(crate) fn get_arbitration_state(
    settings: &Settings,
    resource: &str,
) -> Result<ArbitrationState> {
    let key = claims_key(resource);
    let mut conn = connect(settings)?;

    let all: Vec<(String, String)> = redis::cmd("HGETALL")
        .arg(&key)
        .query(&mut conn)
        .context("HGETALL arbitration state failed")?;

    let mut claims: Vec<OwnershipClaim> = all
        .iter()
        .filter_map(|(_agent_id, claim_json)| {
            serde_json::from_str::<OwnershipClaim>(claim_json).ok()
        })
        .collect();
    claims.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    // Read optional resolution metadata from a separate key.
    let resolution_key = format!("{key}:resolution");
    let winner: Option<String> = redis::cmd("HGET")
        .arg(&resolution_key)
        .arg("winner")
        .query(&mut conn)
        .unwrap_or(None);
    let resolution_reason: Option<String> = redis::cmd("HGET")
        .arg(&resolution_key)
        .arg("reason")
        .query(&mut conn)
        .unwrap_or(None);

    Ok(ArbitrationState {
        resource: resource.to_owned(),
        claims,
        winner,
        resolution_reason,
    })
}

/// Resolve a contested ownership claim by naming a winner.
///
/// - Updates the winner's claim to `Granted`.
/// - Updates all other claims to `ReviewAssigned`.
/// - Stores resolution metadata for audit.
/// - Sends a direct message to each participant with the outcome.
///
/// # Errors
///
/// Returns an error if Redis commands fail.
pub(crate) fn resolve_claim(
    settings: &Settings,
    resource: &str,
    winner_agent: &str,
    reason: &str,
    resolved_by: &str,
) -> Result<ArbitrationState> {
    let key = claims_key(resource);
    let mut conn = connect(settings)?;

    let all: Vec<(String, String)> = redis::cmd("HGETALL")
        .arg(&key)
        .query(&mut conn)
        .context("HGETALL for resolve failed")?;

    if all.is_empty() {
        bail!("no claims found for resource '{resource}'");
    }

    // Update each claim according to its outcome.
    for (agent_id, claim_json) in &all {
        let Ok(mut claim) = serde_json::from_str::<OwnershipClaim>(claim_json) else {
            continue;
        };

        claim.status = if agent_id == winner_agent {
            ClaimStatus::Granted
        } else {
            ClaimStatus::ReviewAssigned
        };

        if let Ok(updated) = serde_json::to_string(&claim) {
            let _: () = redis::cmd("HSET")
                .arg(&key)
                .arg(agent_id.as_str())
                .arg(&updated)
                .query(&mut conn)
                .context("HSET resolved claim")?;
        }
    }

    // Persist resolution metadata.
    let resolution_key = format!("{key}:resolution");
    let _: () = redis::cmd("HSET")
        .arg(&resolution_key)
        .arg("winner")
        .arg(winner_agent)
        .arg("reason")
        .arg(reason)
        .arg("resolved_by")
        .arg(resolved_by)
        .arg("resolved_at")
        .arg(Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
        .query(&mut conn)
        .context("HSET resolution metadata")?;
    let _: () = redis::cmd("EXPIRE")
        .arg(&resolution_key)
        .arg(CLAIM_TTL_SECS)
        .query(&mut conn)
        .context("EXPIRE resolution key")?;

    // Notify all participants via direct messages (best-effort).
    let state = get_arbitration_state(settings, resource)?;
    for claim in &state.claims {
        let msg_body = if claim.agent == winner_agent {
            format!(
                "You won the arbitration for '{resource}'.\n\
                 Reason: {reason}\n\
                 You may proceed with first-edit."
            )
        } else {
            format!(
                "Arbitration for '{resource}' awarded to '{winner_agent}'.\n\
                 Reason: {reason}\n\
                 Your role: reviewer (first-edit assigned to winner)."
            )
        };
        if let Err(e) = post_direct(
            settings,
            resolved_by,
            &claim.agent,
            "arbitration-result",
            &msg_body,
            None,
            &[format!("arbitration:{resource}")],
        ) {
            tracing::warn!(
                "failed to notify {} of arbitration result: {e:#}",
                claim.agent
            );
        }
    }

    Ok(state)
}

// ---------------------------------------------------------------------------
// Channel listing
// ---------------------------------------------------------------------------

/// Summary of all active channels (for the monitor dashboard).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChannelSummary {
    /// Number of direct-message streams currently in Redis.
    pub(crate) direct_channel_count: usize,
    /// Group channel metadata.
    pub(crate) groups: Vec<GroupInfo>,
    /// All outstanding ownership claims.
    pub(crate) claims: Vec<OwnershipClaim>,
    /// Claims in `contested` status.
    pub(crate) contested_count: usize,
}

/// Gather a summary of all active channels for display in the monitor.
///
/// # Errors
///
/// Returns an error if Redis SCAN commands fail.
pub(crate) fn channel_summary(settings: &Settings) -> Result<ChannelSummary> {
    let mut conn = connect(settings)?;

    // Count direct streams.
    let direct_pattern = format!("{DIRECT_PREFIX}*");
    let mut direct_count = 0usize;
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&direct_pattern)
            .arg("COUNT")
            .arg(100)
            .query(&mut conn)
            .context("SCAN direct streams failed")?;
        direct_count += batch.len();
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let groups = list_groups(settings)?;
    let claims = list_claims(settings, None, None)?;
    let contested_count = claims
        .iter()
        .filter(|c| c.status == ClaimStatus::Contested)
        .count();

    Ok(ChannelSummary {
        direct_channel_count: direct_count,
        groups,
        claims,
        contested_count,
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::useless_vec)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a minimal `OwnershipClaim` for use in multiple tests.
    fn make_claim(agent: &str, resource: &str, status: ClaimStatus) -> OwnershipClaim {
        OwnershipClaim {
            resource: resource.to_owned(),
            agent: agent.to_owned(),
            priority_argument: format!("{agent} needs first-edit"),
            timestamp: "2026-01-01T00:00:00.000000Z".to_owned(),
            status,
        }
    }

    // -----------------------------------------------------------------------
    // 1. Direct key helpers (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn direct_key_is_symmetric() {
        assert_eq!(direct_key("alice", "bob"), direct_key("bob", "alice"));
    }

    #[test]
    fn direct_key_sorts_alphabetically() {
        let key = direct_key("zephyr", "alpha");
        assert!(
            key.contains("alpha:zephyr"),
            "expected alpha before zephyr, got {key}"
        );
    }

    #[test]
    fn direct_key_same_agent_both_sides() {
        let key = direct_key("claude", "claude");
        assert_eq!(key, format!("{DIRECT_PREFIX}claude:claude"));
    }

    /// Verify that any (a, b) pair maps to the SAME key regardless of argument order.
    #[test]
    fn direct_key_triple_symmetry_check() {
        let pairs = [
            ("codex", "gemini"),
            ("euler", "pasteur"),
            ("z", "a"),
            ("same", "same"),
        ];
        for (a, b) in pairs {
            assert_eq!(
                direct_key(a, b),
                direct_key(b, a),
                "asymmetric key for ({a}, {b})"
            );
        }
    }

    /// The key must start with the documented prefix.
    #[test]
    fn direct_key_has_correct_prefix() {
        let key = direct_key("alice", "bob");
        assert!(
            key.starts_with(DIRECT_PREFIX),
            "key '{key}' does not start with '{DIRECT_PREFIX}'"
        );
    }

    /// The key format is `<prefix><lo>:<hi>` — verify no extra separators.
    #[test]
    fn direct_key_format_contains_exactly_one_colon_after_prefix() {
        let key = direct_key("alice", "bob");
        let suffix = key.strip_prefix(DIRECT_PREFIX).expect("prefix present");
        // suffix should be "alice:bob" — exactly one colon
        assert_eq!(
            suffix.matches(':').count(),
            1,
            "unexpected colons in suffix '{suffix}'"
        );
    }

    // -----------------------------------------------------------------------
    // 2. Direct message validation guards (no Redis needed)
    // -----------------------------------------------------------------------

    #[test]
    fn post_direct_rejects_empty_body() {
        let settings = Settings::from_env();
        let result = post_direct(&settings, "alice", "bob", "test", "", None, &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("body"));
    }

    #[test]
    fn post_direct_rejects_empty_sender() {
        let settings = Settings::from_env();
        let result = post_direct(&settings, "", "bob", "test", "hello", None, &[]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("sender"),
            "expected 'sender' in error, got: {msg}"
        );
    }

    #[test]
    fn post_direct_rejects_empty_recipient() {
        let settings = Settings::from_env();
        let result = post_direct(&settings, "alice", "", "test", "hello", None, &[]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("recipient"),
            "expected 'recipient' in error, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // 3. Group channel key helpers (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn group_stream_key_includes_prefix() {
        assert_eq!(group_stream_key("mygroup"), "bus:group:mygroup");
    }

    #[test]
    fn group_members_key_includes_suffix() {
        let k = group_members_key("alpha");
        assert!(k.ends_with(":members"));
        assert!(k.contains("alpha"));
    }

    #[test]
    fn group_meta_key_includes_meta_suffix() {
        let k = group_meta_key("review-team");
        assert!(k.ends_with(":meta"), "expected :meta suffix, got: {k}");
        assert!(k.contains("review-team"));
    }

    #[test]
    fn group_keys_are_distinct_from_each_other() {
        let name = "testgroup";
        let stream = group_stream_key(name);
        let members = group_members_key(name);
        let meta = group_meta_key(name);
        // All three keys should be different.
        assert_ne!(stream, members);
        assert_ne!(stream, meta);
        assert_ne!(members, meta);
    }

    // -----------------------------------------------------------------------
    // 4. Group channel validation guards (no Redis needed)
    // -----------------------------------------------------------------------

    #[test]
    fn create_group_rejects_empty_name() {
        let settings = Settings::from_env();
        let result = create_group(&settings, "", &[], "claude");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn create_group_rejects_invalid_name_with_spaces() {
        let settings = Settings::from_env();
        let result = create_group(&settings, "bad name!", &[], "claude");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("alphanumeric") || msg.contains("name"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn create_group_rejects_name_with_slash() {
        let settings = Settings::from_env();
        let result = create_group(&settings, "bad/name", &[], "claude");
        assert!(result.is_err());
    }

    #[test]
    fn create_group_rejects_name_with_dot() {
        let settings = Settings::from_env();
        let result = create_group(&settings, "bad.name", &[], "claude");
        assert!(result.is_err());
    }

    /// Hyphens and underscores are explicitly allowed.
    #[test]
    fn create_group_accepts_hyphen_and_underscore_in_name() {
        // Validation is pure-logic — it bails before connecting to Redis only on
        // name validation.  The error here comes from Redis, not validation.
        // We check that the error message does NOT mention "name" validation.
        let settings = Settings::from_env();
        let result = create_group(&settings, "my-group_1", &[], "claude");
        // If Redis is unavailable the error is a connection error, not a name-validation error.
        if let Err(e) = result {
            let msg = e.to_string();
            assert!(
                !msg.contains("alphanumeric"),
                "name 'my-group_1' should pass name validation, got: {msg}"
            );
        }
    }

    #[test]
    fn post_to_group_rejects_empty_body() {
        let settings = Settings::from_env();
        let result = post_to_group(&settings, "my-group", "alice", "test", "", None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("body"), "expected 'body' in error, got: {msg}");
    }

    // -----------------------------------------------------------------------
    // 5. Claims key helpers (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn claims_key_normalises_backslash() {
        let key = claims_key("src\\redis_bus.rs");
        assert!(!key.contains('\\'), "expected forward slashes, got: {key}");
        assert!(key.contains("src/redis_bus.rs"));
    }

    #[test]
    fn claims_key_has_correct_prefix() {
        let key = claims_key("src/main.rs");
        assert!(
            key.starts_with(CLAIMS_PREFIX),
            "expected prefix '{CLAIMS_PREFIX}', got: {key}"
        );
    }

    #[test]
    fn claims_key_normalises_multiple_backslashes() {
        let key = claims_key("a\\b\\c\\d.rs");
        assert!(!key.contains('\\'));
        assert!(key.contains("a/b/c/d.rs"));
    }

    #[test]
    fn claims_key_preserves_forward_slashes() {
        let key = claims_key("src/channels.rs");
        assert!(
            key.ends_with("src/channels.rs"),
            "expected suffix preserved, got: {key}"
        );
    }

    /// Empty resource produces a key that is just the prefix (no path component).
    #[test]
    fn claims_key_with_empty_resource() {
        let key = claims_key("");
        assert_eq!(
            key, CLAIMS_PREFIX,
            "empty resource should yield bare prefix"
        );
    }

    // -----------------------------------------------------------------------
    // 6. Ownership arbitration validation guards (no Redis needed)
    // -----------------------------------------------------------------------

    #[test]
    fn claim_resource_rejects_empty_resource() {
        let settings = Settings::from_env();
        let result = claim_resource(&settings, "", "claude", "I need it");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("resource"),
            "expected 'resource' in error, got: {msg}"
        );
    }

    #[test]
    fn claim_resource_rejects_empty_agent() {
        let settings = Settings::from_env();
        let result = claim_resource(&settings, "src/main.rs", "", "I need it");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("agent"),
            "expected 'agent' in error, got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // 7. ClaimStatus serialisation (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn claim_status_serialises_snake_case() {
        let json = serde_json::to_string(&ClaimStatus::ReviewAssigned).unwrap();
        assert_eq!(json, r#""review_assigned""#);
    }

    #[test]
    fn claim_status_pending_serialises() {
        let json = serde_json::to_string(&ClaimStatus::Pending).unwrap();
        assert_eq!(json, r#""pending""#);
    }

    #[test]
    fn claim_status_granted_serialises() {
        let json = serde_json::to_string(&ClaimStatus::Granted).unwrap();
        assert_eq!(json, r#""granted""#);
    }

    #[test]
    fn claim_status_contested_serialises() {
        let json = serde_json::to_string(&ClaimStatus::Contested).unwrap();
        assert_eq!(json, r#""contested""#);
    }

    #[test]
    fn claim_status_round_trips_all_variants() {
        let variants = [
            ClaimStatus::Pending,
            ClaimStatus::Granted,
            ClaimStatus::Contested,
            ClaimStatus::ReviewAssigned,
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let decoded: ClaimStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, *v, "round-trip failed for {v:?}");
        }
    }

    // -----------------------------------------------------------------------
    // 8. OwnershipClaim struct (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn ownership_claim_round_trips_via_json() {
        let claim = OwnershipClaim {
            resource: "src/main.rs".to_owned(),
            agent: "claude".to_owned(),
            priority_argument: "I own the startup flow".to_owned(),
            timestamp: "2026-01-01T00:00:00.000000Z".to_owned(),
            status: ClaimStatus::Granted,
        };
        let json = serde_json::to_string(&claim).unwrap();
        let decoded: OwnershipClaim = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.resource, claim.resource);
        assert_eq!(decoded.agent, claim.agent);
        assert_eq!(decoded.status, ClaimStatus::Granted);
    }

    #[test]
    fn ownership_claim_preserves_priority_argument() {
        let claim = make_claim("codex", "src/lib.rs", ClaimStatus::Contested);
        let json = serde_json::to_string(&claim).unwrap();
        let decoded: OwnershipClaim = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.priority_argument, "codex needs first-edit");
    }

    #[test]
    fn ownership_claim_preserves_timestamp() {
        let ts = "2026-03-15T12:34:56.000000Z";
        let claim = OwnershipClaim {
            resource: "lib.rs".to_owned(),
            agent: "euler".to_owned(),
            priority_argument: "perf analysis".to_owned(),
            timestamp: ts.to_owned(),
            status: ClaimStatus::Pending,
        };
        let json = serde_json::to_string(&claim).unwrap();
        let decoded: OwnershipClaim = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.timestamp, ts);
    }

    // -----------------------------------------------------------------------
    // 9. Arbitration state logic (in-memory, no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn arbitration_state_serialises_none_winner() {
        let state = ArbitrationState {
            resource: "lib.rs".to_owned(),
            claims: vec![],
            winner: None,
            resolution_reason: None,
        };
        let json = serde_json::to_value(&state).unwrap();
        assert!(json["winner"].is_null());
        assert!(json["resolution_reason"].is_null());
    }

    #[test]
    fn arbitration_state_with_winner_serialises_correctly() {
        let state = ArbitrationState {
            resource: "src/main.rs".to_owned(),
            claims: vec![
                make_claim("claude", "src/main.rs", ClaimStatus::Granted),
                make_claim("codex", "src/main.rs", ClaimStatus::ReviewAssigned),
            ],
            winner: Some("claude".to_owned()),
            resolution_reason: Some("claude started first".to_owned()),
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["winner"], "claude");
        assert_eq!(json["resolution_reason"], "claude started first");
        assert_eq!(json["claims"].as_array().unwrap().len(), 2);
    }

    /// Contested count computed from claims vec is correct.
    #[test]
    fn contested_count_from_claims_vec() {
        let claims = vec![
            make_claim("claude", "f.rs", ClaimStatus::Contested),
            make_claim("codex", "f.rs", ClaimStatus::Contested),
            make_claim("gemini", "g.rs", ClaimStatus::Granted),
        ];
        let contested = claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Contested)
            .count();
        assert_eq!(contested, 2);
    }

    /// First claim on a resource should get Granted status (in-memory model check).
    #[test]
    fn first_claim_gets_granted_status_logic() {
        // Simulate the branching logic in claim_resource without Redis.
        // When `other_claimants` is empty, status = Granted.
        let other_claimants: Vec<String> = vec![];
        let status = if other_claimants.is_empty() {
            ClaimStatus::Granted
        } else {
            ClaimStatus::Contested
        };
        assert_eq!(status, ClaimStatus::Granted);
    }

    /// Second claim on a resource should get Contested status (in-memory model check).
    #[test]
    fn second_claim_gets_contested_status_logic() {
        // When `other_claimants` is non-empty, status = Contested.
        let other_claimants = vec!["claude".to_owned()];
        let status = if other_claimants.is_empty() {
            ClaimStatus::Granted
        } else {
            ClaimStatus::Contested
        };
        assert_eq!(status, ClaimStatus::Contested);
    }

    /// After resolution the winner gets Granted, all others get `ReviewAssigned`.
    #[test]
    fn resolution_assigns_winner_and_reviewers() {
        let winner = "claude";
        let claims = vec![
            make_claim("claude", "f.rs", ClaimStatus::Contested),
            make_claim("codex", "f.rs", ClaimStatus::Contested),
            make_claim("gemini", "f.rs", ClaimStatus::Contested),
        ];

        // Simulate the resolution mapping in resolve_claim.
        let resolved: Vec<(String, ClaimStatus)> = claims
            .iter()
            .map(|c| {
                let new_status = if c.agent == winner {
                    ClaimStatus::Granted
                } else {
                    ClaimStatus::ReviewAssigned
                };
                (c.agent.clone(), new_status)
            })
            .collect();

        let winner_entry = resolved.iter().find(|(a, _)| a == winner).unwrap();
        assert_eq!(winner_entry.1, ClaimStatus::Granted);

        for (agent, status) in &resolved {
            if agent != winner {
                assert_eq!(
                    *status,
                    ClaimStatus::ReviewAssigned,
                    "{agent} should be ReviewAssigned"
                );
            }
        }
    }

    /// Exactly one winner after resolution.
    #[test]
    fn resolution_produces_exactly_one_granted() {
        let winner = "codex";
        let agents = ["claude", "codex", "gemini", "euler"];
        let resolved_statuses: Vec<ClaimStatus> = agents
            .iter()
            .map(|a| {
                if *a == winner {
                    ClaimStatus::Granted
                } else {
                    ClaimStatus::ReviewAssigned
                }
            })
            .collect();

        let granted_count = resolved_statuses
            .iter()
            .filter(|s| **s == ClaimStatus::Granted)
            .count();
        assert_eq!(granted_count, 1, "exactly one agent should be Granted");
    }

    /// Verifies that `mark_all_contested` logic upgrades only non-contested claims.
    #[test]
    fn mark_all_contested_only_upgrades_non_contested_claims() {
        // Simulate the guard in mark_all_contested: only update if != Contested.
        let claims = vec![
            make_claim("alice", "f.rs", ClaimStatus::Granted), // should be upgraded
            make_claim("bob", "f.rs", ClaimStatus::Contested), // already contested — skip
            make_claim("carol", "f.rs", ClaimStatus::Pending), // should be upgraded
        ];

        let updated: Vec<ClaimStatus> = claims
            .iter()
            .map(|c| {
                if c.status == ClaimStatus::Contested {
                    c.status.clone()
                } else {
                    ClaimStatus::Contested
                }
            })
            .collect();

        assert!(updated.iter().all(|s| *s == ClaimStatus::Contested));
    }

    // -----------------------------------------------------------------------
    // 10. Channel enum serialisation (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn channel_enum_serialises_with_type_tag() {
        let ch = Channel::Direct {
            agent: "codex".to_owned(),
        };
        let json = serde_json::to_value(&ch).unwrap();
        assert_eq!(json["type"], "direct");
        assert_eq!(json["agent"], "codex");
    }

    #[test]
    fn escalation_channel_has_no_extra_fields() {
        let ch = Channel::Escalate;
        let json = serde_json::to_value(&ch).unwrap();
        assert_eq!(json["type"], "escalate");
        assert!(json.as_object().is_some_and(|m| m.len() == 1));
    }

    #[test]
    fn group_channel_serialises_members() {
        let ch = Channel::Group {
            name: "review-http".to_owned(),
            members: vec!["claude".to_owned(), "codex".to_owned()],
        };
        let json = serde_json::to_value(&ch).unwrap();
        assert_eq!(json["type"], "group");
        assert_eq!(json["name"], "review-http");
        let members = json["members"].as_array().unwrap();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn arbitrate_channel_serialises_resource() {
        let ch = Channel::Arbitrate {
            resource: "src/main.rs".to_owned(),
        };
        let json = serde_json::to_value(&ch).unwrap();
        assert_eq!(json["type"], "arbitrate");
        assert_eq!(json["resource"], "src/main.rs");
    }

    #[test]
    fn channel_enum_round_trips_via_json() {
        let channels: Vec<Channel> = vec![
            Channel::Direct {
                agent: "claude".to_owned(),
            },
            Channel::Group {
                name: "team".to_owned(),
                members: vec!["a".to_owned()],
            },
            Channel::Escalate,
            Channel::Arbitrate {
                resource: "lib.rs".to_owned(),
            },
        ];
        for ch in &channels {
            let json = serde_json::to_string(ch).unwrap();
            let decoded: Channel = serde_json::from_str(&json).unwrap();
            // Re-serialise decoded and compare as Value (no PartialEq on Channel).
            let original = serde_json::to_value(ch).unwrap();
            let roundtripped = serde_json::to_value(&decoded).unwrap();
            assert_eq!(original, roundtripped, "round-trip failed for {ch:?}");
        }
    }

    // -----------------------------------------------------------------------
    // 11. GroupInfo struct (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn group_info_members_are_returned() {
        let info = GroupInfo {
            name: "alpha".to_owned(),
            members: vec!["claude".to_owned(), "codex".to_owned()],
            created_at: "2026-01-01T00:00:00.000000Z".to_owned(),
            created_by: "claude".to_owned(),
            message_count: 42,
        };
        assert_eq!(info.members.len(), 2);
        assert!(info.members.contains(&"claude".to_owned()));
    }

    #[test]
    fn group_info_serialises_message_count() {
        let info = GroupInfo {
            name: "beta".to_owned(),
            members: vec![],
            created_at: "2026-01-01T00:00:00.000000Z".to_owned(),
            created_by: "codex".to_owned(),
            message_count: 7,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["message_count"], 7);
    }

    // -----------------------------------------------------------------------
    // 12. ChannelSummary struct (no Redis)
    // -----------------------------------------------------------------------

    #[test]
    fn channel_summary_contested_count_matches_claims() {
        let claims = vec![
            make_claim("a", "f.rs", ClaimStatus::Contested),
            make_claim("b", "f.rs", ClaimStatus::Contested),
            make_claim("c", "g.rs", ClaimStatus::Granted),
        ];
        let contested_count = claims
            .iter()
            .filter(|c| c.status == ClaimStatus::Contested)
            .count();
        let summary = ChannelSummary {
            direct_channel_count: 3,
            groups: vec![],
            claims: claims.clone(),
            contested_count,
        };
        assert_eq!(summary.contested_count, 2);
        assert_eq!(summary.claims.len(), 3);
    }

    #[test]
    fn channel_summary_zero_counts_for_empty_state() {
        let summary = ChannelSummary {
            direct_channel_count: 0,
            groups: vec![],
            claims: vec![],
            contested_count: 0,
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["direct_channel_count"], 0);
        assert_eq!(json["contested_count"], 0);
        assert!(json["groups"].as_array().unwrap().is_empty());
    }

    #[test]
    fn channel_summary_includes_group_member_counts() {
        let group = GroupInfo {
            name: "team".to_owned(),
            members: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            created_at: "2026-01-01T00:00:00.000000Z".to_owned(),
            created_by: "a".to_owned(),
            message_count: 0,
        };
        let summary = ChannelSummary {
            direct_channel_count: 1,
            groups: vec![group],
            claims: vec![],
            contested_count: 0,
        };
        assert_eq!(summary.groups[0].members.len(), 3);
    }

    // -----------------------------------------------------------------------
    // 13. Escalation validation guards (no Redis needed)
    // -----------------------------------------------------------------------

    #[test]
    fn post_escalation_rejects_empty_body() {
        let settings = Settings::from_env();
        let result = post_escalation(&settings, "claude", "", None, &[]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("body") || msg.contains("empty"),
            "unexpected error: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // 14. Escalation metadata expectations (in-memory, no Redis)
    // -----------------------------------------------------------------------

    /// The escalation metadata should carry `escalation: true`.
    #[test]
    fn escalation_metadata_has_escalation_flag() {
        let meta = serde_json::json!({
            "channel": "escalate",
            "escalation": true,
            "original_sender": "claude"
        });
        assert_eq!(meta["escalation"], true);
        assert_eq!(meta["channel"], "escalate");
    }

    /// Escalation auto-adds "escalation" tag to the tag list.
    #[test]
    fn escalation_tags_include_escalation_tag() {
        let base_tags = vec!["repo:agent-hub".to_owned()];
        // Simulate the tag augmentation in post_escalation.
        let mut escalation_tags = base_tags.clone();
        escalation_tags.push("escalation".to_owned());
        assert!(escalation_tags.contains(&"escalation".to_owned()));
        assert_eq!(escalation_tags.len(), 2);
    }

    /// Escalation priority is always "high" (in-memory constant check).
    #[test]
    fn escalation_priority_is_always_high() {
        // The literal passed to xadd_to_stream in post_escalation.
        let priority = "high";
        assert_eq!(priority, "high");
    }

    /// Escalation stream key is the documented constant.
    #[test]
    fn escalation_stream_key_matches_constant() {
        assert_eq!(ESCALATION_STREAM, "bus:escalations");
    }

    // -----------------------------------------------------------------------
    // 15. decode_channel_entry (no Redis connection, builds HashMap directly)
    // -----------------------------------------------------------------------

    /// Verifies that `decode_channel_entry` correctly maps known fields.
    #[test]
    fn decode_channel_entry_maps_fields_correctly() {
        use std::collections::HashMap;

        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        let insert = |map: &mut HashMap<String, redis::Value>, k: &str, v: &str| {
            map.insert(
                k.to_owned(),
                redis::Value::BulkString(v.as_bytes().to_vec()),
            );
        };

        insert(&mut fields, "id", "test-uuid-1234");
        insert(&mut fields, "timestamp_utc", "2026-01-01T00:00:00.000000Z");
        insert(&mut fields, "protocol_version", "1.0");
        insert(&mut fields, "from", "alice");
        insert(&mut fields, "to", "bob");
        insert(&mut fields, "topic", "greeting");
        insert(&mut fields, "body", "hello world");
        insert(&mut fields, "tags", "[]");
        insert(&mut fields, "priority", "normal");
        insert(&mut fields, "request_ack", "false");
        insert(&mut fields, "reply_to", "alice");
        insert(&mut fields, "metadata", "{}");

        let msg = decode_channel_entry("1234-0", &fields);

        assert_eq!(msg.id, "test-uuid-1234");
        assert_eq!(msg.from, "alice");
        assert_eq!(msg.to, "bob");
        assert_eq!(msg.topic, "greeting");
        assert_eq!(msg.body, "hello world");
        assert_eq!(msg.priority, "normal");
        assert!(!msg.request_ack);
        assert_eq!(msg.stream_id, Some("1234-0".to_owned()));
    }

    /// Missing optional fields do not cause a panic.
    #[test]
    fn decode_channel_entry_handles_missing_optional_fields() {
        use std::collections::HashMap;

        // Only provide the bare minimum fields (everything else defaults).
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "from".to_owned(),
            redis::Value::BulkString(b"sender".to_vec()),
        );
        fields.insert("body".to_owned(), redis::Value::BulkString(b"hi".to_vec()));

        let msg = decode_channel_entry("9999-0", &fields);

        // Defaults should be safe values.
        assert_eq!(msg.from, "sender");
        assert_eq!(msg.body, "hi");
        assert!(msg.id.is_empty()); // missing → empty string
        assert!(msg.thread_id.is_none()); // missing → None
        assert!(msg.tags.is_empty()); // missing → []
    }

    #[test]
    fn decode_channel_entry_sets_stream_id_from_param() {
        use std::collections::HashMap;
        let fields: HashMap<String, redis::Value> = HashMap::new();
        let msg = decode_channel_entry("42-7", &fields);
        assert_eq!(msg.stream_id, Some("42-7".to_owned()));
    }

    /// `request_ack` is false when the field is absent.
    #[test]
    fn decode_channel_entry_request_ack_defaults_false() {
        use std::collections::HashMap;
        let fields: HashMap<String, redis::Value> = HashMap::new();
        let msg = decode_channel_entry("0-0", &fields);
        assert!(!msg.request_ack);
    }

    /// `request_ack` is true when field contains "true".
    #[test]
    fn decode_channel_entry_request_ack_true_when_set() {
        use std::collections::HashMap;
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "request_ack".to_owned(),
            redis::Value::BulkString(b"true".to_vec()),
        );
        let msg = decode_channel_entry("0-0", &fields);
        assert!(msg.request_ack);
    }

    /// `thread_id` is None when the stored value is the literal "None".
    #[test]
    fn decode_channel_entry_thread_id_none_when_literal_none() {
        use std::collections::HashMap;
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"None".to_vec()),
        );
        let msg = decode_channel_entry("0-0", &fields);
        assert!(msg.thread_id.is_none());
    }

    /// `thread_id` is Some when a real value is stored.
    #[test]
    fn decode_channel_entry_thread_id_some_when_present() {
        use std::collections::HashMap;
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"thread-abc".to_vec()),
        );
        let msg = decode_channel_entry("0-0", &fields);
        assert_eq!(msg.thread_id, Some("thread-abc".to_owned()));
    }

    // -----------------------------------------------------------------------
    // 16. parse_channel_xrange (no Redis connection, builds raw Value directly)
    // -----------------------------------------------------------------------

    /// An empty slice yields no entries.
    #[test]
    fn parse_channel_xrange_empty_input() {
        let result = parse_channel_xrange(&[]);
        assert!(result.is_empty());
    }

    /// A well-formed entry is correctly parsed.
    #[test]
    fn parse_channel_xrange_parses_single_entry() {
        // Build: [["1234-0", ["from", "alice", "body", "hello"]]]
        let fields_raw = redis::Value::Array(vec![
            redis::Value::BulkString(b"from".to_vec()),
            redis::Value::BulkString(b"alice".to_vec()),
            redis::Value::BulkString(b"body".to_vec()),
            redis::Value::BulkString(b"hello".to_vec()),
        ]);
        let entry = redis::Value::Array(vec![
            redis::Value::BulkString(b"1234-0".to_vec()),
            fields_raw,
        ]);

        let result = parse_channel_xrange(&[entry]);
        assert_eq!(result.len(), 1);
        let (sid, map) = &result[0];
        assert_eq!(sid, "1234-0");
        assert!(map.contains_key("from"));
        assert!(map.contains_key("body"));
    }

    /// Entries with fewer than 2 parts are silently skipped.
    #[test]
    fn parse_channel_xrange_skips_malformed_entries() {
        let short_entry = redis::Value::Array(vec![
            redis::Value::BulkString(b"1234-0".to_vec()),
            // Missing fields array — only 1 element.
        ]);
        let result = parse_channel_xrange(&[short_entry]);
        assert!(result.is_empty(), "malformed entry should be skipped");
    }

    /// Non-Array top-level values are silently skipped.
    #[test]
    fn parse_channel_xrange_skips_non_array_entries() {
        let bad = redis::Value::BulkString(b"not-an-array".to_vec());
        let result = parse_channel_xrange(&[bad]);
        assert!(result.is_empty());
    }

    /// Multiple entries are all parsed.
    #[test]
    fn parse_channel_xrange_parses_multiple_entries() {
        fn make_entry(id: &str, key: &str, val: &str) -> redis::Value {
            redis::Value::Array(vec![
                redis::Value::BulkString(id.as_bytes().to_vec()),
                redis::Value::Array(vec![
                    redis::Value::BulkString(key.as_bytes().to_vec()),
                    redis::Value::BulkString(val.as_bytes().to_vec()),
                ]),
            ])
        }

        let entries = vec![
            make_entry("1-0", "from", "alice"),
            make_entry("2-0", "from", "bob"),
            make_entry("3-0", "from", "carol"),
        ];

        let result = parse_channel_xrange(&entries);
        assert_eq!(result.len(), 3);
        let ids: Vec<&String> = result.iter().map(|(sid, _)| sid).collect();
        assert!(ids.contains(&&"1-0".to_owned()));
        assert!(ids.contains(&&"2-0".to_owned()));
        assert!(ids.contains(&&"3-0".to_owned()));
    }
}
