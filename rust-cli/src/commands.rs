//! CLI command implementations.

use anyhow::{Context as _, Result};

use crate::models::{MAX_HISTORY_MINUTES, Message, Presence};
use crate::output::{Encoding, output, output_message, output_messages, output_presence};
use crate::redis_bus::{
    bus_health, bus_list_messages, bus_list_presence, bus_post_message, bus_set_presence, connect,
};
use crate::settings::Settings;
use crate::validation::{non_empty, parse_metadata_arg, validate_priority};

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
    let health = bus_health(settings);
    output(&health, encoding);
}

pub(crate) fn cmd_send(settings: &Settings, args: &SendArgs<'_>) -> Result<()> {
    validate_priority(args.priority)?;
    let from = non_empty(args.from_agent, "--from-agent")?;
    let to = non_empty(args.to_agent, "--to-agent")?;
    let topic = non_empty(args.topic, "--topic")?;
    let body = non_empty(args.body, "--body")?;
    let meta = parse_metadata_arg(args.metadata)?;

    let mut conn = connect(settings)?;
    let msg = bus_post_message(
        &mut conn,
        settings,
        from,
        to,
        topic,
        body,
        args.thread_id.as_deref(),
        args.tags,
        args.priority,
        args.request_ack,
        args.reply_to.as_deref(),
        &meta,
    )?;
    output(&msg, args.encoding);
    Ok(())
}

pub(crate) fn cmd_read(settings: &Settings, args: &ReadArgs<'_>) -> Result<()> {
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
    )?;
    output(&msg, encoding);
    Ok(())
}

pub(crate) fn cmd_presence(settings: &Settings, args: &PresenceArgs<'_>) -> Result<()> {
    let agent = non_empty(args.agent, "--agent")?;
    let meta = parse_metadata_arg(args.metadata)?;
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
