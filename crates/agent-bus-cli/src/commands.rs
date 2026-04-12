//! CLI command implementations.

use anyhow::{Context as _, Result};

use crate::models::{MAX_HISTORY_MINUTES, Message, Presence};
pub(crate) use crate::ops::MessageFilters;
use crate::ops::admin::{
    health as ops_health, list_pending_acks as ops_list_pending_acks,
    list_presence as ops_list_presence,
};
use crate::ops::channel::{
    PostDirectRequest, PostGroupRequest, ReadDirectRequest, ReadGroupRequest,
    post_direct as ops_post_direct, post_group as ops_post_group, read_direct as ops_read_direct,
    read_group as ops_read_group,
};
use crate::ops::claim::{
    ClaimResourceRequest, ListClaimsRequest, ReleaseClaimRequest, RenewClaimRequest,
    ResolveClaimRequest, claim_resource as ops_claim_resource, list_claims as ops_list_claims,
    parse_resource_scope, release_claim as ops_release_claim, renew_claim as ops_renew_claim,
    resolve_claim as ops_resolve_claim,
};
use crate::ops::inbox::{
    CompactContextRequest, CompactThreadRequest, SummarizeSessionRequest, SummarizeThreadRequest,
    compact_context as compact_context_op, compact_thread as compact_thread_op,
    summarize_session as ops_summarize_session, summarize_thread as ops_summarize_thread,
};
use crate::ops::task::{
    PushTaskCardRequest, peek_task_cards as ops_peek_task_cards,
    pull_task_card as ops_pull_task_card, push_task_card as ops_push_task_card,
};
use crate::ops::{
    AckMessageRequest, PostMessageRequest, PresenceRequest, ReadMessagesRequest,
    ValidatedBatchItem, ValidatedSendRequest, knock_metadata, list_messages_history, post_ack,
    post_message, set_presence, validated_batch_send, validated_post_message,
};
use crate::output::{
    Encoding, format_health_toon, output, output_message, output_messages, output_presence,
};
use crate::redis_bus::{bus_list_messages, connect};
use crate::server_mode::{
    http_get, http_post, http_put, post_service_action, query_windows_service_state,
    resolved_service_base_url, sc_action, service_status_payload, use_server_mode, wait_for_health,
    wait_for_windows_service_state,
};
use crate::settings::Settings;
#[cfg(feature = "server-mode")]
use crate::validation::{
    auto_fit_schema, infer_schema_from_topic, validate_message_schema, validate_priority,
};
use crate::validation::{non_empty, parse_metadata_arg};

#[cfg(test)]
use crate::ops::{extra_filter_fetch_limit, message_matches_filters};

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
    pub(crate) repo: &'a Option<String>,
    pub(crate) session: &'a Option<String>,
    pub(crate) tags: &'a [String],
    pub(crate) thread_id: &'a Option<String>,
    pub(crate) since_minutes: u64,
    pub(crate) limit: usize,
    pub(crate) exclude_broadcast: bool,
    pub(crate) excerpt: Option<usize>,
    pub(crate) encoding: &'a Encoding,
}

#[cfg(feature = "server-mode")]
fn build_server_resource_url(base: &str, resource: &str, action: Option<&str>) -> Result<String> {
    let mut url = reqwest::Url::parse(base).context("invalid server URL")?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|()| anyhow::anyhow!("server URL does not support path segments"))?;
        segments.extend(["channels", "arbitrate", resource]);
        if let Some(action) = action {
            segments.push(action);
        }
    }
    Ok(url.to_string())
}

fn list_filtered_messages(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
    filters: &MessageFilters<'_>,
) -> Result<Vec<Message>> {
    Ok(list_messages_history(
        settings,
        &ReadMessagesRequest {
            agent,
            from_agent,
            since_minutes,
            limit,
            include_broadcast,
            filters: MessageFilters {
                repo: filters.repo,
                session: filters.session,
                tags: filters.tags,
                thread_id: filters.thread_id,
            },
        },
    )?)
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

pub(crate) struct CompactContextArgs<'a> {
    pub(crate) agent: Option<&'a str>,
    pub(crate) repo: &'a Option<String>,
    pub(crate) session: &'a Option<String>,
    pub(crate) tags: &'a [String],
    pub(crate) thread_id: &'a Option<String>,
    pub(crate) since_minutes: u64,
    pub(crate) max_tokens: usize,
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

    let health = ops_health(settings, None);
    if matches!(encoding, Encoding::Toon) {
        println!("{}", format_health_toon(&health));
    } else {
        output(&health, encoding);
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "service lifecycle command handles all maintenance actions in one place"
)]
pub(crate) fn cmd_service(
    settings: &Settings,
    action: &str,
    reason: Option<&str>,
    base_url: Option<&str>,
    service_name_override: Option<&str>,
    timeout_seconds: u64,
    encoding: &Encoding,
) -> Result<()> {
    let action = action.trim().to_lowercase();
    let base_url = resolved_service_base_url(settings, base_url);
    let service_name = service_name_override
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&settings.service_name);

    match action.as_str() {
        "status" => {
            #[cfg(feature = "server-mode")]
            let admin_status = http_get(&format!("{base_url}/admin/service")).ok();
            #[cfg(not(feature = "server-mode"))]
            let admin_status = None;

            let windows_service_state = query_windows_service_state(service_name)?;
            output(
                &service_status_payload(
                    &base_url,
                    service_name,
                    windows_service_state.as_deref(),
                    admin_status.as_ref(),
                ),
                encoding,
            );
            Ok(())
        }
        "pause" | "resume" | "flush" => {
            let result = post_service_action(&base_url, &action, reason)?;
            output(&result, encoding);
            Ok(())
        }
        "stop" => {
            if query_windows_service_state(service_name)?.is_some() {
                let _ = post_service_action(&base_url, "pause", reason);
                let _ = post_service_action(&base_url, "flush", reason);
                sc_action(service_name, "stop")?;
                let state =
                    wait_for_windows_service_state(service_name, "STOPPED", timeout_seconds)?;
                output(
                    &serde_json::json!({
                        "ok": true,
                        "action": "stop",
                        "service_name": service_name,
                        "windows_service_state": state,
                    }),
                    encoding,
                );
            } else {
                let result = post_service_action(&base_url, "stop", reason)?;
                output(&result, encoding);
            }
            Ok(())
        }
        "start" => {
            sc_action(service_name, "start")?;
            let state = wait_for_windows_service_state(service_name, "RUNNING", timeout_seconds)?;
            let health = wait_for_health(&base_url, timeout_seconds)?;
            output(
                &serde_json::json!({
                    "ok": true,
                    "action": "start",
                    "service_name": service_name,
                    "windows_service_state": state,
                    "health": health,
                }),
                encoding,
            );
            Ok(())
        }
        "restart" => {
            let _ = post_service_action(&base_url, "pause", reason);
            let _ = post_service_action(&base_url, "flush", reason);
            if query_windows_service_state(service_name)?.is_some() {
                if let Some(state) = query_windows_service_state(service_name)?
                    && !state.eq_ignore_ascii_case("STOPPED")
                {
                    sc_action(service_name, "stop")?;
                    let _ =
                        wait_for_windows_service_state(service_name, "STOPPED", timeout_seconds)?;
                }
                sc_action(service_name, "start")?;
                let state =
                    wait_for_windows_service_state(service_name, "RUNNING", timeout_seconds)?;
                let health = wait_for_health(&base_url, timeout_seconds)?;
                output(
                    &serde_json::json!({
                        "ok": true,
                        "action": "restart",
                        "service_name": service_name,
                        "windows_service_state": state,
                        "health": health,
                    }),
                    encoding,
                );
            } else {
                let result = post_service_action(&base_url, "stop", reason)?;
                output(
                    &serde_json::json!({
                        "ok": true,
                        "action": "restart",
                        "service_name": service_name,
                        "note": "graceful stop requested for a non-service HTTP server; restart must be performed by the caller",
                        "result": result,
                    }),
                    encoding,
                );
            }
            Ok(())
        }
        _ => anyhow::bail!(
            "invalid service action '{action}'; expected status|pause|resume|flush|stop|start|restart"
        ),
    }
}

pub(crate) fn cmd_send(settings: &Settings, args: &SendArgs<'_>) -> Result<()> {
    let meta = parse_metadata_arg(args.metadata)?;

    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        // Server-mode HTTP fallback: validate locally, then forward via HTTP.
        validate_priority(args.priority)?;
        let from = non_empty(args.from_agent, "--from-agent")?;
        let to = non_empty(args.to_agent, "--to-agent")?;
        let topic = non_empty(args.topic, "--topic")?;
        let body = non_empty(args.body, "--body")?;
        let effective_schema = infer_schema_from_topic(topic, args.schema);
        let fitted_body = auto_fit_schema(body, effective_schema);
        validate_message_schema(&fitted_body, effective_schema)?;

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
    let msg = validated_post_message(
        &mut conn,
        settings,
        &ValidatedSendRequest {
            sender: args.from_agent,
            recipient: args.to_agent,
            topic: args.topic,
            body: args.body,
            priority: args.priority,
            schema: args.schema,
            tags: args.tags,
            thread_id: args.thread_id.as_deref(),
            reply_to: args.reply_to.as_deref(),
            request_ack: args.request_ack,
            metadata: &meta,
            transport: "cli",
            has_sse_subscribers: false,
        },
    )?;
    output(&msg, args.encoding);
    Ok(())
}

pub(crate) fn cmd_read(settings: &Settings, args: &ReadArgs<'_>) -> Result<()> {
    let filters = MessageFilters {
        repo: args.repo.as_deref(),
        session: args.session.as_deref(),
        tags: args.tags,
        thread_id: args.thread_id.as_deref(),
    };

    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let mut url = reqwest::Url::parse(&format!("{base}/messages"))
            .context("invalid server URL for message read")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("since", &args.since_minutes.to_string());
            query.append_pair("limit", &args.limit.to_string());
            if let Some(agent) = args.agent.as_deref() {
                query.append_pair("agent", agent);
            }
            if let Some(from) = args.from_agent.as_deref() {
                query.append_pair("from", from);
            }
            if let Some(repo) = args.repo.as_deref() {
                query.append_pair("repo", repo);
            }
            if let Some(session) = args.session.as_deref() {
                query.append_pair("session", session);
            }
            for tag in args.tags {
                query.append_pair("tag", tag);
            }
            if let Some(thread_id) = args.thread_id.as_deref() {
                query.append_pair("thread_id", thread_id);
            }
            if args.exclude_broadcast {
                query.append_pair("broadcast", "false");
            }
        }
        let val = http_get(url.as_str())?;
        output(&val, args.encoding);
        return Ok(());
    }

    let mut msgs = list_filtered_messages(
        settings,
        args.agent.as_deref(),
        args.from_agent.as_deref(),
        args.since_minutes,
        args.limit,
        !args.exclude_broadcast,
        &filters,
    )?;
    if let Some(max_chars) = args.excerpt {
        for msg in &mut msgs {
            msg.body = crate::output::excerpt_body(&msg.body, max_chars);
        }
    }
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
    let mut conn = connect(settings)?;
    let ack = post_ack(
        &mut conn,
        settings,
        &AckMessageRequest {
            agent,
            message_id,
            body,
            has_sse_subscribers: false,
        },
    )?;
    output(&ack.message, encoding);
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
    let pending = ops_list_pending_acks(&mut conn, agent)?;
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
    let presence = set_presence(
        &mut conn,
        settings,
        &PresenceRequest {
            agent,
            status: args.status,
            session_id: args.session_id.as_deref(),
            capabilities: args.capabilities,
            ttl_seconds: args.ttl_seconds,
            metadata: &meta,
        },
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

#[expect(
    clippy::too_many_arguments,
    clippy::ref_option,
    reason = "keeps CLI dispatch stable while filters are threaded through existing command shape"
)]
pub(crate) fn cmd_export(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    repo: &Option<String>,
    session: &Option<String>,
    tags: &[String],
    thread_id: &Option<String>,
    since_minutes: u64,
    limit: usize,
) -> Result<()> {
    let filters = MessageFilters {
        repo: repo.as_deref(),
        session: session.as_deref(),
        tags,
        thread_id: thread_id.as_deref(),
    };
    let msgs = list_filtered_messages(
        settings,
        agent,
        from_agent,
        since_minutes,
        limit,
        true,
        &filters,
    )?;
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
    let results = ops_list_presence(&mut conn, settings)?;
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
#[expect(
    clippy::too_many_arguments,
    clippy::ref_option,
    reason = "keeps CLI dispatch stable while filters are threaded through existing command shape"
)]
pub(crate) fn cmd_journal(
    settings: &Settings,
    repo: &Option<String>,
    session: &Option<String>,
    tags: &[String],
    thread_id: &Option<String>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    output: &str,
) -> Result<()> {
    let filters = MessageFilters {
        repo: repo.as_deref(),
        session: session.as_deref(),
        tags,
        thread_id: thread_id.as_deref(),
    };
    let messages = list_filtered_messages(
        settings,
        None,
        from_agent,
        since_minutes,
        limit,
        true,
        &filters,
    )?;

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

    // Parse all lines into batch items, skipping blanks and comments.
    let mut items: Vec<ValidatedBatchItem> = Vec::new();
    for (line_no, line) in lines.iter().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let entry: BatchLine = serde_json::from_str(line)
            .with_context(|| format!("line {}: JSON parse error", line_no + 1))?;
        items.push(ValidatedBatchItem {
            sender: entry.sender,
            recipient: entry.recipient,
            topic: entry.topic,
            body: entry.body,
            priority: entry.priority,
            schema: entry.schema,
            tags: entry.tags,
            thread_id: entry.thread_id,
            reply_to: entry.reply_to,
            request_ack: entry.request_ack,
            metadata: entry
                .metadata
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
        });
    }

    let mut conn = connect(settings)?;
    let posted = validated_batch_send(&mut conn, settings, &items, "cli", false)?;

    let ids: Vec<String> = posted.iter().map(|m| m.message.id.clone()).collect();
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
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to CLI positional parameters"
)]
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
    let msg = ops_post_direct(
        settings,
        &PostDirectRequest {
            from_agent: from,
            to_agent: to,
            topic,
            body,
            thread_id,
            tags,
        },
    )?;
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
    let msgs = ops_read_direct(
        settings,
        &ReadDirectRequest {
            agent_a,
            agent_b,
            limit,
        },
    )?;
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
    let msg = ops_post_group(
        settings,
        &PostGroupRequest {
            group,
            from_agent: from,
            topic,
            body,
            thread_id,
        },
    )?;
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
    let msgs = ops_read_group(settings, &ReadGroupRequest { group, limit })?;
    output_messages(&msgs, encoding);
    Ok(())
}

/// Claim ownership of a resource.
///
/// # Errors
///
/// Returns an error if the resource or agent are empty, or if Redis write fails.
#[expect(
    clippy::too_many_arguments,
    reason = "CLI surface maps directly to claim flags"
)]
pub(crate) fn cmd_claim(
    settings: &Settings,
    resource: &str,
    agent: &str,
    reason: &str,
    mode: &str,
    namespace: Option<&str>,
    scope_kind: Option<&str>,
    scope_path: Option<&str>,
    repo_scopes: &[String],
    thread_id: Option<&str>,
    lease_ttl_seconds: u64,
    scope: Option<&str>,
    encoding: &Encoding,
) -> Result<()> {
    let parsed_scope = scope.map(parse_resource_scope).transpose()?;

    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let url = build_server_resource_url(base, resource, None)?;
        let val = http_post(
            &url,
            &serde_json::json!({
                "agent": agent,
                "priority_argument": reason,
                "mode": mode,
                "namespace": namespace,
                "scope_kind": scope_kind,
                "scope_path": scope_path,
                "repo_scopes": repo_scopes,
                "thread_id": thread_id,
                "lease_ttl_seconds": lease_ttl_seconds.max(1),
                "scope": scope,
            }),
        )?;
        output(&val, encoding);
        return Ok(());
    }

    let claim = ops_claim_resource(
        settings,
        &ClaimResourceRequest {
            resource,
            agent,
            reason,
            mode,
            namespace,
            scope_kind,
            scope_path,
            repo_scopes,
            thread_id,
            lease_ttl_seconds,
            scope: parsed_scope,
        },
    )?;
    output(&claim, encoding);
    Ok(())
}

pub(crate) fn cmd_renew_claim(
    settings: &Settings,
    resource: &str,
    agent: &str,
    lease_ttl_seconds: u64,
    encoding: &Encoding,
) -> Result<()> {
    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let url = build_server_resource_url(base, resource, Some("renew"))?;
        let val = http_post(
            &url,
            &serde_json::json!({
                "agent": agent,
                "lease_ttl_seconds": lease_ttl_seconds.max(1),
            }),
        )?;
        output(&val, encoding);
        return Ok(());
    }

    let claim = ops_renew_claim(
        settings,
        &RenewClaimRequest {
            resource,
            agent,
            lease_ttl_seconds,
        },
    )?;
    output(&claim, encoding);
    Ok(())
}

pub(crate) fn cmd_release_claim(
    settings: &Settings,
    resource: &str,
    agent: &str,
    encoding: &Encoding,
) -> Result<()> {
    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let url = build_server_resource_url(base, resource, Some("release"))?;
        let val = http_post(&url, &serde_json::json!({ "agent": agent }))?;
        output(&val, encoding);
        return Ok(());
    }

    let state = ops_release_claim(settings, &ReleaseClaimRequest { resource, agent })?;
    output(&state, encoding);
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
    let claims = ops_list_claims(
        settings,
        &ListClaimsRequest {
            resource,
            status: status_str,
        },
    )?;
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
    let summary = ops_summarize_session(
        settings,
        &SummarizeSessionRequest {
            agent: None,
            repo: None,
            session: Some(session),
            since_minutes: 10_080,
            limit: 10_000,
        },
    )?;
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

    let filters = MessageFilters {
        repo: None,
        session,
        tags: &[],
        thread_id: None,
    };
    let msgs =
        list_filtered_messages(settings, None, agent, since_minutes, 10_000, true, &filters)?;

    // path → (count, max_severity_str, BTreeSet<agent>)
    let mut by_path: HashMap<String, (u64, &'static str, std::collections::BTreeSet<String>)> =
        HashMap::new();

    for msg in &msgs {
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
            let entry = by_path.entry(path.to_owned()).or_insert((
                0,
                "LOW",
                std::collections::BTreeSet::new(),
            ));
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
    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let url = build_server_resource_url(base, resource, Some("resolve"))?;
        let val = http_put(
            &url,
            &serde_json::json!({
                "winner": winner,
                "reason": reason,
                "resolved_by": resolved_by,
            }),
        )?;
        output(&val, encoding);
        return Ok(());
    }

    let state = ops_resolve_claim(
        settings,
        &ResolveClaimRequest {
            resource,
            winner,
            reason,
            resolved_by,
        },
    )?;
    output(&state, encoding);
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "CLI surface maps directly to knock flags"
)]
pub(crate) fn cmd_knock(
    settings: &Settings,
    from_agent: &str,
    to_agent: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
    request_ack: bool,
    encoding: &Encoding,
) -> Result<()> {
    let body = non_empty(body, "--body")?;
    let from = non_empty(from_agent, "--from-agent")?;
    let to = non_empty(to_agent, "--to-agent")?;
    let metadata = knock_metadata(request_ack);

    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let url = format!("{base}/knock");
        let val = http_post(
            &url,
            &serde_json::json!({
                "sender": from,
                "recipient": to,
                "body": body,
                "thread_id": thread_id,
                "tags": tags,
                "request_ack": request_ack,
            }),
        )?;
        output(&val, encoding);
        return Ok(());
    }

    let mut conn = connect(settings)?;
    let msg = post_message(
        &mut conn,
        settings,
        &PostMessageRequest {
            sender: from,
            recipient: to,
            topic: "knock",
            body,
            thread_id,
            tags,
            priority: "urgent",
            request_ack,
            reply_to: None,
            metadata: &metadata,
            has_sse_subscribers: true,
        },
    )?;
    output(&msg, encoding);
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
    args: &CompactContextArgs<'_>,
) -> Result<()> {
    #[cfg(feature = "server-mode")]
    if use_server_mode(settings) {
        let base = settings.server_url.as_deref().unwrap_or("");
        let mut payload = serde_json::json!({
            "since_minutes": args.since_minutes,
            "max_tokens": args.max_tokens,
            "tags": args.tags,
        });
        if let Some(agent) = args.agent {
            payload["agent"] = serde_json::Value::String(agent.to_owned());
        }
        if let Some(repo) = args.repo.as_deref() {
            payload["repo"] = serde_json::Value::String(repo.to_owned());
        }
        if let Some(session) = args.session.as_deref() {
            payload["session"] = serde_json::Value::String(session.to_owned());
        }
        if let Some(thread_id) = args.thread_id.as_deref() {
            payload["thread_id"] = serde_json::Value::String(thread_id.to_owned());
        }
        let url = format!("{base}/compact-context");
        let val = http_post(&url, &payload)?;
        output(&val, args.encoding);
        return Ok(());
    }

    // Compact recent context is a latency-sensitive, token-oriented read path.
    // Pull directly from Redis with metadata filters so the output reflects the
    // current live stream without incurring PostgreSQL fallback chatter.
    let mut conn = connect(settings)?;
    let result = compact_context_op(
        &mut conn,
        settings,
        &CompactContextRequest {
            agent: args.agent,
            filters: MessageFilters {
                repo: args.repo.as_deref(),
                session: args.session.as_deref(),
                tags: args.tags,
                thread_id: args.thread_id.as_deref(),
            },
            since_minutes: args.since_minutes,
            max_tokens: args.max_tokens,
        },
    )?;
    output(&result.messages, args.encoding);
    Ok(())
}

// ---------------------------------------------------------------------------
// Thread summary / compact command implementations
// ---------------------------------------------------------------------------

/// Summarize all messages within a single thread.
///
/// Retrieves messages tagged with the given `thread_id` from durable history
/// and aggregates them into a [`SessionSummary`].
///
/// # Errors
///
/// Returns an error if the Redis (or `PostgreSQL`) query fails.
pub(crate) fn cmd_summarize_thread(
    settings: &Settings,
    thread_id: &str,
    since_minutes: u64,
    limit: usize,
    encoding: &Encoding,
) -> Result<()> {
    let summary = ops_summarize_thread(
        settings,
        &SummarizeThreadRequest {
            thread_id,
            since_minutes,
            limit,
        },
    )?;
    output(&summary, encoding);
    Ok(())
}

/// Compact messages within a single thread to fit a token budget.
///
/// Fetches recent messages for the given `thread_id` from the live Redis
/// stream, then trims them to the token budget.
///
/// # Errors
///
/// Returns an error if the Redis connection or stream read fails.
pub(crate) fn cmd_compact_thread(
    settings: &Settings,
    thread_id: &str,
    token_budget: usize,
    since_minutes: u64,
    limit: usize,
    encoding: &Encoding,
) -> Result<()> {
    let mut conn = connect(settings)?;
    let result = compact_thread_op(
        &mut conn,
        settings,
        &CompactThreadRequest {
            thread_id,
            token_budget,
            since_minutes,
            limit,
        },
    )?;
    output(&result.messages, encoding);
    Ok(())
}

// ---------------------------------------------------------------------------
// Task queue command implementations
// ---------------------------------------------------------------------------

/// Push a structured [`TaskCard`] to the tail of an agent's task queue.
///
/// Outputs the created `TaskCard` JSON on success.
///
/// # Errors
///
/// Returns an error if the Redis connection or `RPUSH` fails, or if `agent`
/// or `task` is empty, or if `priority` is invalid.
#[expect(clippy::too_many_arguments)]
pub(crate) fn cmd_push_task(
    settings: &Settings,
    agent: &str,
    task: &str,
    encoding: &Encoding,
    repo: Option<&str>,
    priority: &str,
    tags: &[String],
    depends_on: &[String],
    reply_to: Option<&str>,
    created_by: &str,
) -> Result<()> {
    let agent = non_empty(agent, "--agent")?;
    let body = non_empty(task, "--task")?;
    let card = ops_push_task_card(
        settings,
        &PushTaskCardRequest {
            agent,
            body,
            created_by,
            priority,
            repo,
            paths: &[],
            depends_on,
            reply_to,
            tags,
        },
    )?;
    output(&serde_json::to_value(&card)?, encoding);
    Ok(())
}

/// Pop and return the next task from an agent's queue as a [`TaskCard`].
///
/// Outputs the `TaskCard` JSON, or `{"agent": "<id>", "task": null}` when the
/// queue is empty. Legacy plain-string entries are wrapped in a minimal card.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LPOP` fails.
pub(crate) fn cmd_pull_task(settings: &Settings, agent: &str, encoding: &Encoding) -> Result<()> {
    let agent = non_empty(agent, "--agent")?;
    let card = ops_pull_task_card(settings, agent)?;
    match card {
        Some(c) => output(&serde_json::to_value(&c)?, encoding),
        None => output(&serde_json::json!({"agent": agent, "task": null}), encoding),
    }
    Ok(())
}

/// Peek at the pending tasks in an agent's queue without consuming them.
///
/// Outputs `{"agent": "<id>", "tasks": [...], "count": N}` where each task is
/// a full [`TaskCard`] JSON object. Legacy plain-string entries are wrapped in
/// minimal cards.
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
    let cards = ops_peek_task_cards(settings, agent, limit)?;
    let count = cards.len();
    output(
        &serde_json::json!({"agent": agent, "tasks": cards, "count": count}),
        encoding,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Subscriptions
// ---------------------------------------------------------------------------

/// Create a subscription for an agent.
///
/// # Errors
///
/// Returns an error if `agent` is empty, `priority_min` is invalid, or if
/// the Redis connection fails.
#[expect(clippy::too_many_arguments)]
pub(crate) fn cmd_subscribe(
    settings: &Settings,
    agent: &str,
    repos: &[String],
    sessions: &[String],
    threads: &[String],
    tags: &[String],
    topics: &[String],
    priority_min: Option<&str>,
    resources: &[String],
    ttl: Option<u64>,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::subscription::{SubscribeRequest, subscribe as ops_subscribe};

    let scopes = agent_bus_core::models::SubscriptionScopes {
        repos: repos.to_vec(),
        sessions: sessions.to_vec(),
        threads: threads.to_vec(),
        tags: tags.to_vec(),
        topics: topics.to_vec(),
        priority_min: priority_min.map(str::to_owned),
        resources: resources.to_vec(),
    };

    let sub = ops_subscribe(
        settings,
        &SubscribeRequest {
            agent,
            scopes: &scopes,
            ttl_seconds: ttl,
        },
    )?;

    output(&serde_json::to_value(&sub)?, encoding);
    Ok(())
}

/// Delete a subscription by ID.
///
/// # Errors
///
/// Returns an error if `agent` or `id` is empty, or if the Redis
/// connection fails.
pub(crate) fn cmd_unsubscribe(
    settings: &Settings,
    agent: &str,
    id: &str,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::subscription::unsubscribe as ops_unsubscribe;

    let deleted = ops_unsubscribe(settings, agent, id)?;
    output(
        &serde_json::json!({"agent": agent, "id": id, "deleted": deleted}),
        encoding,
    );
    Ok(())
}

/// List all active subscriptions for an agent.
///
/// # Errors
///
/// Returns an error if `agent` is empty or the Redis connection fails.
pub(crate) fn cmd_subscriptions(
    settings: &Settings,
    agent: &str,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::subscription::list_subscriptions as ops_list_subscriptions;

    let subs = ops_list_subscriptions(settings, agent)?;
    let count = subs.len();
    output(
        &serde_json::json!({"agent": agent, "subscriptions": subs, "count": count}),
        encoding,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

pub(crate) fn cmd_inventory(
    settings: &Settings,
    repo: Option<&str>,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::inventory::{list_active_repos_and_sessions, repo_inventory};

    let mut conn = connect(settings).context("inventory: Redis connection failed")?;

    if let Some(repo_name) = repo {
        let non_empty_repo = non_empty(repo_name, "--repo")?;
        let result = repo_inventory(&mut conn, settings, non_empty_repo)?;
        output(&result, encoding);
    } else {
        let result = list_active_repos_and_sessions(&mut conn, settings)?;
        output(&result, encoding);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Thread commands (Part A)
// ---------------------------------------------------------------------------

/// Create a new conversation thread.
///
/// # Errors
///
/// Returns an error if `created_by` is empty, or the Redis connection fails.
pub(crate) fn cmd_thread_create(
    settings: &Settings,
    thread_id: Option<&str>,
    created_by: &str,
    repo: Option<&str>,
    topic: Option<&str>,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::thread::{CreateThreadRequest, create_thread as ops_create_thread};

    let thread = ops_create_thread(
        settings,
        &CreateThreadRequest {
            thread_id,
            created_by,
            repo,
            topic,
        },
    )?;

    output(&serde_json::to_value(&thread)?, encoding);
    Ok(())
}

/// Join a conversation thread.
///
/// # Errors
///
/// Returns an error if `thread_id` or `agent` is empty, the thread does not
/// exist, or the Redis connection fails.
pub(crate) fn cmd_thread_join(
    settings: &Settings,
    thread_id: &str,
    agent: &str,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::thread::{ThreadMemberRequest, join_thread as ops_join_thread};

    let thread = ops_join_thread(settings, &ThreadMemberRequest { thread_id, agent })?;

    output(&serde_json::to_value(&thread)?, encoding);
    Ok(())
}

/// Leave a conversation thread.
///
/// # Errors
///
/// Returns an error if `thread_id` or `agent` is empty, the thread does not
/// exist, or the Redis connection fails.
pub(crate) fn cmd_thread_leave(
    settings: &Settings,
    thread_id: &str,
    agent: &str,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::thread::{ThreadMemberRequest, leave_thread as ops_leave_thread};

    let thread = ops_leave_thread(settings, &ThreadMemberRequest { thread_id, agent })?;

    output(&serde_json::to_value(&thread)?, encoding);
    Ok(())
}

/// List all threads.
///
/// # Errors
///
/// Returns an error if the Redis connection fails.
pub(crate) fn cmd_thread_list(settings: &Settings, encoding: &Encoding) -> Result<()> {
    use crate::ops::thread::list_threads as ops_list_threads;

    let threads = ops_list_threads(settings)?;
    let count = threads.len();
    output(
        &serde_json::json!({"threads": threads, "count": count}),
        encoding,
    );
    Ok(())
}

/// Close a conversation thread.
///
/// # Errors
///
/// Returns an error if `thread_id` is empty, the thread does not exist,
/// or the Redis connection fails.
pub(crate) fn cmd_thread_close(
    settings: &Settings,
    thread_id: &str,
    encoding: &Encoding,
) -> Result<()> {
    use crate::ops::thread::close_thread as ops_close_thread;

    let thread = ops_close_thread(settings, thread_id)?;
    output(&serde_json::to_value(&thread)?, encoding);
    Ok(())
}

// ---------------------------------------------------------------------------
// Ack deadline commands (Part B)
// ---------------------------------------------------------------------------

/// List overdue ack deadlines.
///
/// # Errors
///
/// Returns an error if the Redis connection fails.
pub(crate) fn cmd_overdue_acks(settings: &Settings, encoding: &Encoding) -> Result<()> {
    use crate::ops::ack_deadline::check_overdue_acks as ops_check_overdue_acks;

    let overdue = ops_check_overdue_acks(settings)?;
    let count = overdue.len();
    output(
        &serde_json::json!({"overdue_acks": overdue, "count": count}),
        encoding,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message() -> Message {
        Message {
            id: "msg-1".to_owned(),
            timestamp_utc: "2026-03-22T00:00:00Z".to_owned(),
            protocol_version: crate::models::PROTOCOL_VERSION.to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: "status".to_owned(),
            body: "hello".to_owned(),
            thread_id: Some("thread-123".to_owned()),
            tags: smallvec::smallvec![
                "repo:agent-bus".to_owned(),
                "session:sprint-42".to_owned(),
                "kind:finding".to_owned(),
            ],
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        }
    }

    #[test]
    fn message_matches_repo_session_tag_and_thread_filters() {
        let msg = make_message();
        let repo = Some("agent-bus");
        let session = Some("sprint-42");
        let tags = vec!["kind:finding".to_owned()];
        let thread_id = Some("thread-123");
        let filters = MessageFilters {
            repo,
            session,
            tags: &tags,
            thread_id,
        };

        assert!(message_matches_filters(&msg, &filters));
    }

    #[test]
    fn message_matches_filters_rejects_missing_tag() {
        let msg = make_message();
        let repo = Some("agent-bus");
        let session = Some("sprint-42");
        let tags = vec!["kind:other".to_owned()];
        let thread_id = Some("thread-123");
        let filters = MessageFilters {
            repo,
            session,
            tags: &tags,
            thread_id,
        };

        assert!(!message_matches_filters(&msg, &filters));
    }

    #[test]
    fn extra_filter_fetch_limit_expands_only_for_message_filters() {
        let repo = Some("agent-bus");
        let session = None;
        let tags: Vec<String> = Vec::new();
        let thread_id = None;
        let filters = MessageFilters {
            repo,
            session,
            tags: &tags,
            thread_id,
        };

        let expanded = extra_filter_fetch_limit(10, &filters);
        assert!(expanded >= 10);
        assert!(expanded >= crate::models::XREVRANGE_MIN_FETCH);
    }

    #[cfg(feature = "server-mode")]
    #[test]
    fn build_server_resource_url_encodes_embedded_path_separators() {
        let url = build_server_resource_url(
            "http://localhost:8400/api/v1",
            "repo/path with spaces/file.rs",
            Some("resolve"),
        )
        .expect("resource URL should build");

        assert_eq!(
            url,
            "http://localhost:8400/api/v1/channels/arbitrate/repo%2Fpath%20with%20spaces%2Ffile.rs/resolve"
        );
    }

    #[test]
    fn severity_rank_orders_known_levels_and_unknowns() {
        assert_eq!(severity_rank("CRITICAL"), 4);
        assert_eq!(severity_rank("HIGH"), 3);
        assert_eq!(severity_rank("MEDIUM"), 2);
        assert_eq!(severity_rank("LOW"), 1);
        assert_eq!(severity_rank("unexpected"), 0);
    }

    fn test_settings() -> Settings {
        Settings::from_env()
    }

    #[test]
    fn cmd_service_rejects_unknown_action_before_side_effects() {
        let err = cmd_service(
            &test_settings(),
            "bogus",
            None,
            Some("http://localhost:8400"),
            None,
            1,
            &Encoding::Json,
        )
        .expect_err("invalid service action must fail");

        assert!(err.to_string().contains("invalid service action"));
    }

    #[test]
    fn cmd_push_task_rejects_blank_agent_before_queue_access() {
        let err = cmd_push_task(
            &test_settings(),
            "   ",
            "do the thing",
            &Encoding::Json,
            None,
            "normal",
            &[],
            &[],
            None,
            "codex",
        )
        .expect_err("blank agent must fail");

        assert!(err.to_string().contains("--agent"));
    }

    #[test]
    fn cmd_push_task_rejects_blank_task_before_queue_access() {
        let err = cmd_push_task(
            &test_settings(),
            "codex",
            "   ",
            &Encoding::Json,
            None,
            "normal",
            &[],
            &[],
            None,
            "codex",
        )
        .expect_err("blank task must fail");

        assert!(err.to_string().contains("--task"));
    }

    #[test]
    fn cmd_pull_task_rejects_blank_agent_before_queue_access() {
        let err = cmd_pull_task(&test_settings(), "   ", &Encoding::Json)
            .expect_err("blank agent must fail");

        assert!(err.to_string().contains("--agent"));
    }

    #[test]
    fn cmd_peek_tasks_rejects_blank_agent_before_queue_access() {
        let err = cmd_peek_tasks(&test_settings(), "   ", 5, &Encoding::Json)
            .expect_err("blank agent must fail");

        assert!(err.to_string().contains("--agent"));
    }

    #[test]
    fn cmd_knock_rejects_blank_body_before_bus_access() {
        let err = cmd_knock(
            &test_settings(),
            "codex",
            "claude",
            "   ",
            None,
            &[],
            false,
            &Encoding::Json,
        )
        .expect_err("blank knock body must fail");

        assert!(err.to_string().contains("--body"));
    }

    #[test]
    fn cmd_presence_rejects_blank_agent_before_presence_write() {
        let session_id = Some("session-123".to_owned());
        let err = cmd_presence(
            &test_settings(),
            &PresenceArgs {
                agent: "   ",
                status: "online",
                session_id: &session_id,
                capabilities: &[],
                ttl_seconds: 60,
                metadata: None,
                encoding: &Encoding::Json,
            },
        )
        .expect_err("blank agent must fail");

        assert!(err.to_string().contains("--agent"));
    }

    #[test]
    fn cmd_claim_rejects_invalid_scope_before_bus_access() {
        let err = cmd_claim(
            &test_settings(),
            "src/main.rs",
            "codex",
            "editing",
            "exclusive",
            None,
            None,
            None,
            &[],
            None,
            60,
            Some("planet"),
            &Encoding::Json,
        )
        .expect_err("invalid scope must fail");

        assert!(err.to_string().contains("invalid scope"));
    }

    #[test]
    fn cmd_thread_create_rejects_blank_created_by_before_bus_access() {
        let err = cmd_thread_create(
            &test_settings(),
            Some("thread-1"),
            "   ",
            Some("agent-bus"),
            Some("review"),
            &Encoding::Json,
        )
        .expect_err("blank created_by must fail");

        assert!(err.to_string().contains("created_by"));
    }

    #[test]
    fn cmd_thread_join_rejects_blank_thread_id_before_bus_access() {
        let err = cmd_thread_join(&test_settings(), "   ", "codex", &Encoding::Json)
            .expect_err("blank thread_id must fail");

        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn cmd_thread_join_rejects_blank_agent_before_bus_access() {
        let err = cmd_thread_join(&test_settings(), "thread-1", "   ", &Encoding::Json)
            .expect_err("blank agent must fail");

        assert!(err.to_string().contains("agent"));
    }

    #[test]
    fn cmd_thread_leave_rejects_blank_thread_id_before_bus_access() {
        let err = cmd_thread_leave(&test_settings(), "   ", "codex", &Encoding::Json)
            .expect_err("blank thread_id must fail");

        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn cmd_thread_leave_rejects_blank_agent_before_bus_access() {
        let err = cmd_thread_leave(&test_settings(), "thread-1", "   ", &Encoding::Json)
            .expect_err("blank agent must fail");

        assert!(err.to_string().contains("agent"));
    }

    #[test]
    fn cmd_thread_close_rejects_blank_thread_id_before_bus_access() {
        let err = cmd_thread_close(&test_settings(), "   ", &Encoding::Json)
            .expect_err("blank thread_id must fail");

        assert!(err.to_string().contains("thread_id"));
    }
}
