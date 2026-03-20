//! CLI command implementations.

use anyhow::{Context as _, Result};

use crate::models::{MAX_HISTORY_MINUTES, Message, Presence};
use crate::output::{
    Encoding, format_health_toon, output, output_message, output_messages, output_presence,
};
use crate::redis_bus::{
    bus_health, bus_list_messages, bus_list_presence, bus_post_message, bus_set_presence,
    clear_pending_ack, connect, list_pending_acks,
};
use crate::settings::Settings;
use crate::validation::{
    auto_fit_schema, infer_schema_from_topic, non_empty, parse_metadata_arg,
    validate_message_schema, validate_priority,
};

// ---------------------------------------------------------------------------
// HTTP client helper — used when AGENT_BUS_SERVER_URL is set
// ---------------------------------------------------------------------------

/// Returns `true` when the caller should route through the HTTP server instead
/// of connecting to Redis directly.
#[cfg(feature = "server-mode")]
fn use_server_mode(settings: &Settings) -> bool {
    settings.server_url.is_some()
}

/// Shared HTTP client helper for server-mode commands.
///
/// Performs a `GET` request and returns the parsed JSON body.
///
/// # Errors
///
/// Returns an error if the URL is unreachable, the response is not 2xx,
/// or JSON deserialisation fails.
#[cfg(feature = "server-mode")]
fn http_get(url: &str) -> Result<serde_json::Value> {
    let resp = reqwest::blocking::get(url).with_context(|| format!("GET {url} failed"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().context("HTTP response JSON decode failed")?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {body}");
    }
    Ok(body)
}

/// Performs a `POST` request with a JSON body and returns the parsed JSON
/// response.
///
/// # Errors
///
/// Returns an error if the request fails, the server returns a non-2xx status,
/// or JSON deserialisation fails.
#[cfg(feature = "server-mode")]
fn http_post(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(url)
        .json(body)
        .send()
        .with_context(|| format!("POST {url} failed"))?;
    let status = resp.status();
    let resp_body: serde_json::Value = resp.json().context("HTTP response JSON decode failed")?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {resp_body}");
    }
    Ok(resp_body)
}

/// Performs a `PUT` request with a JSON body and returns the parsed JSON
/// response.
///
/// # Errors
///
/// Returns an error on network failure, non-2xx response, or JSON error.
#[cfg(feature = "server-mode")]
fn http_put(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .put(url)
        .json(body)
        .send()
        .with_context(|| format!("PUT {url} failed"))?;
    let status = resp.status();
    let resp_body: serde_json::Value = resp.json().context("HTTP response JSON decode failed")?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {resp_body}");
    }
    Ok(resp_body)
}

// ---------------------------------------------------------------------------
// Argument structs (avoid cloning in Cmd match)
// ---------------------------------------------------------------------------

pub(crate) struct SendArgs<'a> {
    pub(crate) from_agent: &'a str,
    pub(crate) to_agent: &'a str,
    pub(crate) topic: &'a str,
    pub(crate) body: &'a str,
    pub(crate) thread_id: &'a Option<String>,
    pub(crate) tags: &'a [String],
    pub(crate) priority: &'a str,
    pub(crate) request_ack: bool,
    pub(crate) reply_to: &'a Option<String>,
    pub(crate) metadata: Option<&'a str>,
    pub(crate) schema: Option<&'a str>,
    pub(crate) encoding: &'a Encoding,
}

pub(crate) struct ReadArgs<'a> {
    pub(crate) agent: &'a Option<String>,
    pub(crate) from_agent: &'a Option<String>,
    pub(crate) since_minutes: u64,
    pub(crate) limit: usize,
    pub(crate) exclude_broadcast: bool,
    pub(crate) encoding: &'a Encoding,
}

pub(crate) struct PresenceArgs<'a> {
    pub(crate) agent: &'a str,
    pub(crate) status: &'a str,
    pub(crate) session_id: &'a Option<String>,
    pub(crate) capabilities: &'a [String],
    pub(crate) ttl_seconds: u64,
    pub(crate) metadata: Option<&'a str>,
    pub(crate) encoding: &'a Encoding,
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

pub(crate) fn cmd_health(settings: &Settings, encoding: &Encoding) {
    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let url = format!("{}/health", settings.server_url.as_deref().unwrap_or(""));
        match http_get(&url) {
            Ok(val) => {
                output(&val, encoding);
                return;
            }
            Err(e) => {
                eprintln!("server-mode health failed: {e:#}");
                std::process::exit(1);
            }
        }
    }

    let health = bus_health(settings, None);
    if matches!(encoding, Encoding::Toon) {
        println!("{}", format_health_toon(&health));
    } else {
        output(&health, encoding);
    }
}

pub(crate) fn cmd_send(settings: &Settings, args: &SendArgs<'_>) -> Result<()> {
    validate_priority(args.priority)?;
    let from = non_empty(args.from_agent, "--from-agent")?;
    let to = non_empty(args.to_agent, "--to-agent")?;
    let topic = non_empty(args.topic, "--topic")?;
    let body = non_empty(args.body, "--body")?;
    let effective_schema = infer_schema_from_topic(topic, args.schema);
    let fitted_body = auto_fit_schema(body, effective_schema);
    validate_message_schema(&fitted_body, effective_schema)?;
    let meta = parse_metadata_arg(args.metadata)?;

    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let url = format!("{}/messages", settings.server_url.as_deref().unwrap_or(""));
        let mut payload = serde_json::json!({
            "sender": from,
            "recipient": to,
            "topic": topic,
            "body": fitted_body,
            "priority": args.priority,
            "request_ack": args.request_ack,
            "tags": args.tags,
            "metadata": meta,
        });
        if let Some(tid) = args.thread_id.as_deref() {
            payload["thread_id"] = serde_json::Value::String(tid.to_owned());
        }
        if let Some(rt) = args.reply_to.as_deref() {
            payload["reply_to"] = serde_json::Value::String(rt.to_owned());
        }
        let val = http_post(&url, &payload)?;
        output(&val, args.encoding);
        return Ok(());
    }

    let mut conn = connect(settings)?;
    let msg = bus_post_message(
        &mut conn,
        settings,
        from,
        to,
        topic,
        &fitted_body,
        args.thread_id.as_deref(),
        args.tags,
        args.priority,
        args.request_ack,
        args.reply_to.as_deref(),
        &meta,
        crate::pg_writer(),
        false, // CLI transport: no SSE subscribers
    )?;
    output(&msg, args.encoding);
    Ok(())
}

pub(crate) fn cmd_read(settings: &Settings, args: &ReadArgs<'_>) -> Result<()> {
    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        use std::fmt::Write as _;
        let base = settings.server_url.as_deref().unwrap_or("");
        let mut query = format!("since={}&limit={}", args.since_minutes, args.limit);
        if let Some(agent) = args.agent.as_deref() {
            let _ = write!(query, "&agent={agent}");
        }
        if let Some(from) = args.from_agent.as_deref() {
            let _ = write!(query, "&from={from}");
        }
        if args.exclude_broadcast {
            query.push_str("&broadcast=false");
        }
        let url = format!("{base}/messages?{query}");
        let val = http_get(&url)?;
        output(&val, args.encoding);
        return Ok(());
    }

    let msgs = bus_list_messages(
        settings,
        args.agent.as_deref(),
        args.from_agent.as_deref(),
        args.since_minutes,
        args.limit,
        !args.exclude_broadcast,
    )?;
    output_messages(&msgs, args.encoding);
    Ok(())
}

pub(crate) fn cmd_watch(
    settings: &Settings,
    agent: &str,
    history: u64,
    exclude_broadcast: bool,
    encoding: &Encoding,
) -> Result<()> {
    // Load history first (chronological order)
    if history > 0 {
        let msgs = bus_list_messages(
            settings,
            Some(agent),
            None,
            MAX_HISTORY_MINUTES,
            usize::try_from(history).unwrap_or(usize::MAX),
            !exclude_broadcast,
        )?;
        for msg in &msgs {
            output_message(msg, encoding);
        }
    }

    // Subscribe to pub/sub
    let client = redis::Client::open(settings.redis_url.as_str())
        .context("Redis client for watch failed")?;
    let mut conn = client
        .get_connection()
        .context("Redis watch connection failed")?;
    let mut pubsub = conn.as_pubsub();
    pubsub
        .subscribe(&settings.channel_key)
        .context("SUBSCRIBE failed")?;

    let include_broadcast = !exclude_broadcast;
    loop {
        let msg = pubsub.get_message().context("pub/sub receive error")?;
        let payload: String = msg.get_payload().context("pub/sub payload decode")?;
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&payload) else {
            continue;
        };
        match event.get("event").and_then(|e| e.as_str()) {
            Some("message") => {
                let Some(msg_val) = event.get("message") else {
                    continue;
                };
                let Ok(bus_msg) = serde_json::from_value::<Message>(msg_val.clone()) else {
                    continue;
                };
                let to_matches = bus_msg.to == agent;
                let broadcast_matches = include_broadcast && bus_msg.to == "all";
                if to_matches || broadcast_matches {
                    output_message(&bus_msg, encoding);
                }
            }
            Some("presence") => {
                let Some(p_val) = event.get("presence") else {
                    continue;
                };
                let Ok(presence) = serde_json::from_value::<Presence>(p_val.clone()) else {
                    continue;
                };
                output_presence(&presence, encoding);
            }
            _ => {}
        }
    }
}

pub(crate) fn cmd_ack(
    settings: &Settings,
    agent: &str,
    message_id: &str,
    body: &str,
    encoding: &Encoding,
) -> Result<()> {
    let meta = serde_json::json!({"ack_for": message_id});
    let mut conn = connect(settings)?;
    let msg = bus_post_message(
        &mut conn,
        settings,
        agent,
        "all",
        "ack",
        body,
        None,
        &[],
        "normal",
        false,
        Some(message_id), // reply_to = the message being acked
        &meta,
        crate::pg_writer(),
        false, // CLI transport: no SSE subscribers
    )?;
    // Clear the pending-ack entry now that the acknowledgement has been sent.
    // Non-fatal: the entry will expire automatically after 300 seconds anyway.
    if let Err(error) = clear_pending_ack(&mut conn, message_id) {
        tracing::warn!("failed to clear pending ack for {message_id}: {error:#}");
    }
    output(&msg, encoding);
    Ok(())
}

/// List messages awaiting acknowledgement.
///
/// Scans Redis for all `agent_bus:pending_ack:*` keys and prints the results.
/// Entries older than 60 seconds are flagged as STALE.
///
/// # Errors
///
/// Returns an error if the Redis connection or SCAN commands fail.
pub(crate) fn cmd_pending_acks(
    settings: &Settings,
    agent: Option<&str>,
    encoding: &Encoding,
) -> Result<()> {
    let mut conn = connect(settings)?;
    let pending = list_pending_acks(&mut conn, agent)?;
    if matches!(encoding, Encoding::Human) {
        if pending.is_empty() {
            println!("No pending acknowledgements.");
        } else {
            println!(
                "{:<38}  {:<12}  {:<26}  STATUS",
                "MESSAGE ID", "RECIPIENT", "SENT AT"
            );
            println!("{}", "-".repeat(90));
            for p in &pending {
                let status = if p.stale { "STALE" } else { "waiting" };
                println!(
                    "{:<38}  {:<12}  {:<26}  {}",
                    p.message_id, p.recipient, p.sent_at, status
                );
            }
            println!("\n{} pending ack(s)", pending.len());
        }
    } else {
        output(&pending, encoding);
    }
    Ok(())
}

pub(crate) fn cmd_presence(settings: &Settings, args: &PresenceArgs<'_>) -> Result<()> {
    let agent = non_empty(args.agent, "--agent")?;
    let meta = parse_metadata_arg(args.metadata)?;

    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let url = format!("{base}/presence/{agent}");
        let mut payload = serde_json::json!({
            "status": args.status,
            "ttl_seconds": args.ttl_seconds,
            "capabilities": args.capabilities,
            "metadata": meta,
        });
        if let Some(sid) = args.session_id.as_deref() {
            payload["session_id"] = serde_json::Value::String(sid.to_owned());
        }
        let val = http_put(&url, &payload)?;
        output(&val, args.encoding);
        return Ok(());
    }

    let mut conn = connect(settings)?;
    let presence = bus_set_presence(
        &mut conn,
        settings,
        agent,
        args.status,
        args.session_id.as_deref(),
        args.capabilities,
        args.ttl_seconds,
        &meta,
        crate::pg_writer(),
    )?;
    output_presence(&presence, args.encoding);
    Ok(())
}

pub(crate) fn cmd_prune(
    settings: &Settings,
    older_than_days: u64,
    encoding: &Encoding,
) -> Result<()> {
    let msgs = crate::postgres_store::prune_old_messages(settings, older_than_days)?;
    let presence = crate::postgres_store::prune_old_presence(settings, older_than_days)?;
    let result = serde_json::json!({
        "messages_deleted": msgs,
        "presence_events_deleted": presence,
        "older_than_days": older_than_days,
    });
    output(&result, encoding);
    Ok(())
}

pub(crate) fn cmd_export(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
) -> Result<()> {
    let msgs = bus_list_messages(settings, agent, from_agent, since_minutes, limit, true)?;
    for msg in &msgs {
        println!("{}", serde_json::to_string(msg).unwrap_or_default());
    }
    eprintln!("Exported {} messages", msgs.len());
    Ok(())
}

pub(crate) fn cmd_presence_history(
    settings: &Settings,
    agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    encoding: &Encoding,
) -> Result<()> {
    let events = crate::postgres_store::list_presence_history_postgres(
        settings,
        agent,
        since_minutes,
        limit,
    )?;
    if matches!(encoding, Encoding::Human) {
        for p in &events {
            output_presence(p, encoding);
        }
    } else {
        output(&events, encoding);
    }
    Ok(())
}

pub(crate) fn cmd_presence_list(settings: &Settings, encoding: &Encoding) -> Result<()> {
    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let url = format!("{}/presence", settings.server_url.as_deref().unwrap_or(""));
        let val = http_get(&url)?;
        output(&val, encoding);
        return Ok(());
    }

    let mut conn = connect(settings)?;
    let results = bus_list_presence(&mut conn, settings)?;
    if matches!(encoding, Encoding::Human) {
        for p in &results {
            output_presence(p, encoding);
        }
    } else {
        output(&results, encoding);
    }
    Ok(())
}

/// Display a real-time session monitoring dashboard.
///
/// Delegates to [`crate::monitor::monitor_session`].  Runs until interrupted.
///
/// # Errors
///
/// Returns an error if the initial Redis connection fails.
pub(crate) fn cmd_monitor(settings: &Settings, session: Option<&str>, refresh: u64) -> Result<()> {
    crate::monitor::monitor_session(settings, session, refresh)
}

/// Backfill all Redis stream messages into `PostgreSQL`.
///
/// Reads up to `limit` entries from Redis via `XRANGE` (oldest-first, no filter)
/// and inserts any that are missing from the database using `ON CONFLICT DO NOTHING`.
/// Progress is written to stderr; the final count JSON goes to stdout.
///
/// # Errors
///
/// Returns an error if Redis is unreachable or any database write fails.
pub(crate) fn cmd_sync(settings: &Settings, limit: usize, encoding: &Encoding) -> Result<()> {
    eprintln!("Reading messages from Redis stream...");
    let messages = crate::redis_bus::bus_read_all_from_redis(settings, limit)?;
    eprintln!("Found {} messages in Redis", messages.len());

    eprintln!("Syncing to PostgreSQL...");
    let (checked, inserted) = crate::postgres_store::sync_redis_to_postgres(settings, &messages)?;

    let result = serde_json::json!({
        "redis_messages": messages.len(),
        "checked": checked,
        "newly_inserted": inserted,
        "already_in_pg": checked - inserted,
    });
    output(&result, encoding);
    Ok(())
}

/// Export bus messages matching an optional tag or sender filter to an NDJSON
/// journal file on disk.
///
/// The export is idempotent: messages already present in the file (matched by
/// `id`) are skipped. Progress is reported to stderr.
///
/// # Errors
///
/// Returns an error if the query fails or the journal file cannot be written.
pub(crate) fn cmd_journal(
    settings: &Settings,
    tag: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    output: &str,
) -> Result<()> {
    let messages = if let Some(tag) = tag {
        crate::journal::query_messages_by_tag(settings, tag, since_minutes, limit)?
    } else {
        bus_list_messages(settings, None, from_agent, since_minutes, limit, true)?
    };

    let path = std::path::Path::new(output);
    let count = crate::journal::export_journal(&messages, path)?;
    eprintln!(
        "Exported {count} new messages to {output} ({} total in query)",
        messages.len()
    );
    Ok(())
}

/// Sync findings between Claude and Codex sessions.
///
/// Discovers the Codex agent ID from `~/.codex/config.toml`, reads recent bus
/// messages addressed to Codex, normalizes finding-type messages, and outputs
/// a JSON summary.  Does not post any messages.
///
/// # Errors
///
/// Returns an error if the home directory is unavailable or the Redis query fails.
pub(crate) fn cmd_codex_sync(settings: &Settings, limit: usize, encoding: &Encoding) -> Result<()> {
    let (summary, findings) = crate::codex_bridge::run_codex_sync(settings, limit)?;

    // Combine summary and findings into a single output object.
    let mut result = serde_json::to_value(&summary).unwrap_or_default();
    if let serde_json::Value::Object(ref mut map) = result {
        let findings_val: Vec<serde_json::Value> = findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "message_id": f.message_id,
                    "from": f.from_agent,
                    "topic": f.topic,
                    "severity": f.severity,
                    "timestamp": f.timestamp,
                    "tags": f.tags,
                    "body": f.body,
                })
            })
            .collect();
        map.insert(
            "findings".to_owned(),
            serde_json::Value::Array(findings_val),
        );
    }
    output(&result, encoding);
    Ok(())
}

/// Send multiple messages from a NDJSON file (one JSON object per line).
///
/// Each line must be a JSON object with `sender`, `recipient`, `topic`, and `body`
/// fields. Optional fields: `tags`, `priority`, `thread_id`, `request_ack`,
/// `reply_to`, `metadata`, `schema`.
///
/// Pass `file = "-"` to read from stdin.
///
/// # Errors
///
/// Returns an error if the file cannot be opened, any line fails JSON parsing,
/// or any message fails validation or Redis write.
pub(crate) fn cmd_batch_send(settings: &Settings, file: &str, encoding: &Encoding) -> Result<()> {
    use std::io::BufRead as _;

    #[derive(serde::Deserialize)]
    struct BatchLine {
        sender: String,
        recipient: String,
        topic: String,
        body: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default = "crate::models::default_priority")]
        priority: String,
        #[serde(default)]
        thread_id: Option<String>,
        #[serde(default)]
        request_ack: bool,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
        #[serde(default)]
        schema: Option<String>,
    }

    let lines: Vec<String> = if file == "-" {
        let stdin = std::io::stdin();
        stdin.lock().lines().collect::<std::io::Result<_>>()?
    } else {
        let f = std::fs::File::open(file)
            .with_context(|| format!("failed to open batch file: {file}"))?;
        std::io::BufReader::new(f)
            .lines()
            .collect::<std::io::Result<_>>()?
    };

    let mut conn = connect(settings)?;
    let mut ids: Vec<String> = Vec::new();

    for (line_no, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let entry: BatchLine = serde_json::from_str(line)
            .with_context(|| format!("line {}: JSON parse error", line_no + 1))?;

        validate_priority(&entry.priority)
            .with_context(|| format!("line {}: invalid priority", line_no + 1))?;
        let from = non_empty(&entry.sender, "sender")?;
        let to = non_empty(&entry.recipient, "recipient")?;
        let topic = non_empty(&entry.topic, "topic")?;
        let body = non_empty(&entry.body, "body")?;
        let effective_schema = infer_schema_from_topic(topic, entry.schema.as_deref());
        let fitted_body = auto_fit_schema(body, effective_schema);
        validate_message_schema(&fitted_body, effective_schema)
            .with_context(|| format!("line {}: schema validation failed", line_no + 1))?;
        let meta = entry
            .metadata
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

        let msg = bus_post_message(
            &mut conn,
            settings,
            from,
            to,
            topic,
            &fitted_body,
            entry.thread_id.as_deref(),
            &entry.tags,
            &entry.priority,
            entry.request_ack,
            entry.reply_to.as_deref(),
            &meta,
            crate::pg_writer(),
            false, // CLI transport: no SSE subscribers
        )
        .with_context(|| format!("line {}: Redis write failed", line_no + 1))?;
        ids.push(msg.id);
    }

    let result = serde_json::json!({
        "sent": ids.len(),
        "ids": ids,
    });
    output(&result, encoding);
    Ok(())
}

// ---------------------------------------------------------------------------
// Channel command implementations
// ---------------------------------------------------------------------------

/// Post a private direct message between two agents.
///
/// # Errors
///
/// Returns an error if the agents or body are empty, or if Redis write fails.
#[expect(clippy::too_many_arguments, reason = "maps directly to CLI positional parameters")]
pub(crate) fn cmd_post_direct(
    settings: &Settings,
    from_agent: &str,
    to_agent: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
    encoding: &Encoding,
) -> Result<()> {
    let from = non_empty(from_agent, "--from-agent")?;
    let to = non_empty(to_agent, "--to-agent")?;
    let body = non_empty(body, "--body")?;
    let msg = crate::channels::post_direct(settings, from, to, topic, body, thread_id, tags)?;
    output(&msg, encoding);
    Ok(())
}

/// Read direct messages between two agents.
///
/// # Errors
///
/// Returns an error if Redis query fails.
pub(crate) fn cmd_read_direct(
    settings: &Settings,
    agent_a: &str,
    agent_b: &str,
    limit: usize,
    encoding: &Encoding,
) -> Result<()> {
    let msgs = crate::channels::read_direct(settings, agent_a, agent_b, limit)?;
    output_messages(&msgs, encoding);
    Ok(())
}

/// Post a message to a named group channel.
///
/// # Errors
///
/// Returns an error if the group does not exist or Redis write fails.
pub(crate) fn cmd_post_group(
    settings: &Settings,
    group: &str,
    from_agent: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
    encoding: &Encoding,
) -> Result<()> {
    let group = non_empty(group, "--group")?;
    let from = non_empty(from_agent, "--from-agent")?;
    let body = non_empty(body, "--body")?;
    let msg = crate::channels::post_to_group(settings, group, from, topic, body, thread_id)?;
    output(&msg, encoding);
    Ok(())
}

/// Read messages from a named group channel.
///
/// # Errors
///
/// Returns an error if Redis query fails.
pub(crate) fn cmd_read_group(
    settings: &Settings,
    group: &str,
    limit: usize,
    encoding: &Encoding,
) -> Result<()> {
    let msgs = crate::channels::read_group(settings, group, limit)?;
    output_messages(&msgs, encoding);
    Ok(())
}

/// Claim ownership of a resource.
///
/// # Errors
///
/// Returns an error if the resource or agent are empty, or if Redis write fails.
pub(crate) fn cmd_claim(
    settings: &Settings,
    resource: &str,
    agent: &str,
    reason: &str,
    encoding: &Encoding,
) -> Result<()> {
    let claim = crate::channels::claim_resource(settings, resource, agent, reason)?;
    output(&claim, encoding);
    Ok(())
}

/// List ownership claims with optional filters.
///
/// # Errors
///
/// Returns an error if Redis scan fails.
pub(crate) fn cmd_claims(
    settings: &Settings,
    resource: Option<&str>,
    status_str: Option<&str>,
    encoding: &Encoding,
) -> Result<()> {
    use crate::channels::ClaimStatus;
    let status_filter: Option<ClaimStatus> = status_str.map(|s| match s {
        "granted" => ClaimStatus::Granted,
        "contested" => ClaimStatus::Contested,
        "review_assigned" => ClaimStatus::ReviewAssigned,
        _ => ClaimStatus::Pending, // "pending" and unrecognised values
    });
    let claims = crate::channels::list_claims(settings, resource, status_filter.as_ref())?;
    output(
        &serde_json::json!({"claims": claims, "count": claims.len()}),
        encoding,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Session management command implementations (Task 4.2 / 4.3)
// ---------------------------------------------------------------------------

/// Summarise all messages belonging to a session.
///
/// Reads all messages tagged `session:<id>` from the last 7 days (up to
/// 10 000 entries) and produces a compact JSON summary with:
/// - `session`: the session ID
/// - `message_count`: total messages in the session
/// - `agents`: sorted list of all unique senders
/// - `topics`: map of topic → count
/// - `severity_counts`: map of CRITICAL/HIGH/MEDIUM/LOW → count
/// - `time_range`: `{first, last}` ISO-8601 timestamps (null when empty)
///
/// # Errors
///
/// Returns an error if the Redis (or `PostgreSQL`) query fails.
pub(crate) fn cmd_session_summary(
    settings: &Settings,
    session: &str,
    encoding: &Encoding,
) -> Result<()> {
    use std::collections::HashMap;

    let tag_filter = format!("session:{session}");

    // Read all messages from the last 7 days; the tag filter is applied below.
    // We cannot push tag filtering into bus_list_messages without a dedicated
    // index, so we over-fetch and filter client-side.
    let all = bus_list_messages(settings, None, None, 10_080, 10_000, true)?;

    let msgs: Vec<&crate::models::Message> = all
        .iter()
        .filter(|m| m.tags.iter().any(|t| t == &tag_filter))
        .collect();

    let mut agents: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut topics: HashMap<String, u64> = HashMap::new();
    let mut severities: HashMap<&'static str, u64> = HashMap::new();
    let mut first_ts: Option<&str> = None;
    let mut last_ts: Option<&str> = None;

    for msg in &msgs {
        agents.insert(msg.from.clone());
        *topics.entry(msg.topic.clone()).or_insert(0) += 1;

        // Extract severity from body: look for SEVERITY:<level> pattern.
        for line in msg.body.lines() {
            let line_upper = line.to_uppercase();
            if let Some(rest) = line_upper.strip_prefix("SEVERITY:") {
                let level = rest.trim();
                let key: &'static str = if level.starts_with("CRITICAL") {
                    "CRITICAL"
                } else if level.starts_with("HIGH") {
                    "HIGH"
                } else if level.starts_with("MEDIUM") {
                    "MEDIUM"
                } else if level.starts_with("LOW") {
                    "LOW"
                } else {
                    continue;
                };
                *severities.entry(key).or_insert(0) += 1;
            }
        }

        // Track time range using ISO-8601 string ordering (lexicographic = chronological).
        let ts = msg.timestamp_utc.as_str();
        if first_ts.is_none_or(|f| ts < f) {
            first_ts = Some(ts);
        }
        if last_ts.is_none_or(|l| ts > l) {
            last_ts = Some(ts);
        }
    }

    let summary = serde_json::json!({
        "session": session,
        "message_count": msgs.len(),
        "agents": agents.into_iter().collect::<Vec<_>>(),
        "topics": topics,
        "severity_counts": {
            "CRITICAL": severities.get("CRITICAL").copied().unwrap_or(0),
            "HIGH": severities.get("HIGH").copied().unwrap_or(0),
            "MEDIUM": severities.get("MEDIUM").copied().unwrap_or(0),
            "LOW": severities.get("LOW").copied().unwrap_or(0),
        },
        "time_range": {
            "first": first_ts,
            "last": last_ts,
        },
    });
    output(&summary, encoding);
    Ok(())
}

/// Return a numeric rank for a severity string.
///
/// Higher rank = higher severity.  Unknown strings rank as 0 (below LOW).
fn severity_rank(s: &str) -> u8 {
    match s {
        "CRITICAL" => 4,
        "HIGH" => 3,
        "MEDIUM" => 2,
        "LOW" => 1,
        _ => 0,
    }
}

/// Deduplicate findings grouped by file path.
///
/// Reads recent messages (optionally filtered by session tag or sender agent),
/// scans each message body for path-like tokens (containing `/` or `\` and a
/// `.ext`), then groups findings by path.  For each path the output reports:
/// - `path`: the file path token
/// - `count`: how many messages mention it
/// - `max_severity`: highest severity seen (CRITICAL > HIGH > MEDIUM > LOW)
/// - `agents`: sorted unique sender IDs that mentioned this path
///
/// # Errors
///
/// Returns an error if the Redis (or `PostgreSQL`) query fails.
pub(crate) fn cmd_dedup(
    settings: &Settings,
    session: Option<&str>,
    agent: Option<&str>,
    since_minutes: u64,
    encoding: &Encoding,
) -> Result<()> {
    use std::collections::HashMap;

    let session_tag = session.map(|s| format!("session:{s}"));

    let all = bus_list_messages(settings, None, agent, since_minutes, 10_000, true)?;

    let msgs = all.iter().filter(|m| {
        session_tag
            .as_deref()
            .is_none_or(|tag| m.tags.iter().any(|t| t == tag))
    });

    // path → (count, max_severity_str, BTreeSet<agent>)
    let mut by_path: HashMap<String, (u64, &'static str, std::collections::BTreeSet<String>)> =
        HashMap::new();

    for msg in msgs {
        // Extract severity from body for this message.
        let mut msg_severity: &'static str = "LOW";
        for line in msg.body.lines() {
            let upper = line.to_uppercase();
            if let Some(rest) = upper.strip_prefix("SEVERITY:") {
                let level = rest.trim();
                let s: &'static str = if level.starts_with("CRITICAL") {
                    "CRITICAL"
                } else if level.starts_with("HIGH") {
                    "HIGH"
                } else if level.starts_with("MEDIUM") {
                    "MEDIUM"
                } else {
                    "LOW"
                };
                if severity_rank(s) > severity_rank(msg_severity) {
                    msg_severity = s;
                }
            }
        }

        // Scan body tokens for path-like strings.
        for token in msg.body.split_whitespace() {
            let has_separator = token.contains(['/', '\\']);
            // Must contain a dot after the last separator (file extension).
            let last_sep = token.rfind(['/', '\\']).unwrap_or(0);
            let after_sep = &token[last_sep..];
            if !has_separator || !after_sep.contains('.') {
                continue;
            }
            // Strip common punctuation that isn't part of the path.
            let path = token.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | ')' | '('));
            if path.is_empty() {
                continue;
            }
            let entry = by_path
                .entry(path.to_owned())
                .or_insert((0, "LOW", std::collections::BTreeSet::new()));
            entry.0 += 1;
            if severity_rank(msg_severity) > severity_rank(entry.1) {
                entry.1 = msg_severity;
            }
            entry.2.insert(msg.from.clone());
        }
    }

    // Build sorted output (by path for determinism).
    let mut rows: Vec<_> = by_path
        .into_iter()
        .map(|(path, (count, max_sev, agents))| {
            serde_json::json!({
                "path": path,
                "count": count,
                "max_severity": max_sev,
                "agents": agents.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a["path"]
            .as_str()
            .unwrap_or("")
            .cmp(b["path"].as_str().unwrap_or(""))
    });

    let result = serde_json::json!({
        "session": session,
        "since_minutes": since_minutes,
        "unique_paths": rows.len(),
        "findings": rows,
    });
    output(&result, encoding);
    Ok(())
}

/// Resolve a contested claim by naming a winner.
///
/// # Errors
///
/// Returns an error if no claims exist for the resource, or if Redis write fails.
pub(crate) fn cmd_resolve(
    settings: &Settings,
    resource: &str,
    winner: &str,
    reason: &str,
    resolved_by: &str,
    encoding: &Encoding,
) -> Result<()> {
    let state = crate::channels::resolve_claim(settings, resource, winner, reason, resolved_by)?;
    output(&state, encoding);
    Ok(())
}

pub(crate) fn cmd_token_count(text: Option<&str>, encoding: &Encoding) -> Result<()> {
    use std::io::Read as _;

    let input: String = if let Some(t) = text {
        t.to_owned()
    } else {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading stdin for token-count")?;
        buf
    };

    let chars = input.chars().count();
    let tokens = crate::token::estimate_tokens(&input);
    let result = serde_json::json!({
        "characters": chars,
        "estimated_tokens": tokens,
    });
    output(&result, encoding);
    Ok(())
}

pub(crate) fn cmd_compact_context(
    settings: &Settings,
    agent: Option<&str>,
    since_minutes: u64,
    max_tokens: usize,
    encoding: &Encoding,
) -> Result<()> {
    let msgs = bus_list_messages(
        settings,
        agent,
        None,
        since_minutes,
        500,
        true,
    )?;

    let compacted = crate::token::compact_context(&msgs, max_tokens);
    output(&compacted, encoding);
    Ok(())
}

// ---------------------------------------------------------------------------
// Task queue command implementations
// ---------------------------------------------------------------------------

/// Push a task JSON string to the tail of an agent's task queue.
///
/// Outputs `{"agent": "<id>", "queue_length": N}` on success.
///
/// # Errors
///
/// Returns an error if the Redis connection or `RPUSH` fails, or if `agent`
/// is empty.
pub(crate) fn cmd_push_task(
    settings: &Settings,
    agent: &str,
    task: &str,
    encoding: &Encoding,
) -> Result<()> {
    let agent = non_empty(agent, "--agent")?;
    let task = non_empty(task, "--task")?;
    let len = crate::redis_bus::push_task(settings, agent, task)?;
    output(
        &serde_json::json!({"agent": agent, "queue_length": len}),
        encoding,
    );
    Ok(())
}

/// Pop and return the next task from an agent's queue.
///
/// Outputs `{"agent": "<id>", "task": "<payload>"}` where `task` is `null`
/// when the queue is empty.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LPOP` fails.
pub(crate) fn cmd_pull_task(settings: &Settings, agent: &str, encoding: &Encoding) -> Result<()> {
    let agent = non_empty(agent, "--agent")?;
    let task = crate::redis_bus::pull_task(settings, agent)?;
    output(&serde_json::json!({"agent": agent, "task": task}), encoding);
    Ok(())
}

/// Peek at the pending tasks in an agent's queue without consuming them.
///
/// Outputs `{"agent": "<id>", "tasks": [...], "count": N}`.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LRANGE` fails.
pub(crate) fn cmd_peek_tasks(
    settings: &Settings,
    agent: &str,
    limit: usize,
    encoding: &Encoding,
) -> Result<()> {
    let agent = non_empty(agent, "--agent")?;
    let tasks = crate::redis_bus::peek_tasks(settings, agent, limit)?;
    let count = tasks.len();
    output(
        &serde_json::json!({"agent": agent, "tasks": tasks, "count": count}),
        encoding,
    );
    Ok(())
}
