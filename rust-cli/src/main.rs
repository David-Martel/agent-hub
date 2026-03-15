//! Fast standalone CLI + MCP server for agent-bus coordination.
//!
//! Direct Redis client — no Python interpreter, no GIL, instant startup.
//! Full feature parity with the Python agent-bus-mcp server, including MCP stdio mode.
//!
//! # Example
//!
//! ```bash
//! agent-bus health --encoding compact
//! agent-bus send --from-agent claude --to-agent codex --topic test --body "hello"
//! agent-bus read --agent codex --since-minutes 5 --encoding human
//! agent-bus serve --transport stdio
//! ```

// ---------------------------------------------------------------------------
// Lint suppressions at the crate level
// ---------------------------------------------------------------------------
#![expect(
    clippy::multiple_crate_versions,
    reason = "dependency diamond between rmcp and redis — not our choice"
)]

mod cli;
mod commands;
mod http;
mod mcp;
mod models;
mod output;
mod postgres_store;
mod redis_bus;
mod settings;
mod validation;

use anyhow::{Context as _, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use rmcp::serve_server;

use cli::{Cli, Cmd};
use commands::{
    PresenceArgs, ReadArgs, SendArgs, cmd_ack, cmd_health, cmd_presence, cmd_presence_list,
    cmd_read, cmd_send, cmd_watch,
};
use http::start_http_server;
use mcp::AgentBusMcpServer;
use models::STARTUP_PRESENCE_TTL;
use postgres_store::probe_postgres;
use redis_bus::{bus_post_message, bus_set_presence, connect};
use settings::Settings;

// Items re-exported into scope solely so that `use super::*` in the test
// module can access them without additional per-test imports.
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use models::{MAX_HISTORY_MINUTES, default_priority};
#[cfg(test)]
use output::minimize_value;
#[cfg(test)]
use redis_bus::{decode_stream_entry, parse_xrange_result};
#[cfg(test)]
use settings::{env_or, env_parse, redact_url};
#[cfg(test)]
use validation::{VALID_PRIORITIES, non_empty, parse_metadata_arg, validate_priority};

/// Use mimalloc for notable allocation performance gains (M-MIMALLOC-APPS).
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// ---------------------------------------------------------------------------
// Section 9 – Startup announcement helper
// ---------------------------------------------------------------------------

fn maybe_announce_startup(settings: &Settings) {
    if !settings.startup_enabled {
        return;
    }
    let Ok(mut conn) = connect(settings) else {
        return; // silently skip if Redis not up yet
    };
    let meta = serde_json::json!({"service": "agent-bus", "startup": true});
    let mut caps = vec!["mcp".to_owned(), "redis".to_owned()];
    if probe_postgres(settings).0 == Some(true) {
        caps.push("postgres".to_owned());
    }
    let _ = bus_set_presence(
        &mut conn,
        settings,
        &settings.service_agent_id,
        "online",
        None,
        &caps,
        STARTUP_PRESENCE_TTL,
        &meta,
    );
    let _ = bus_post_message(
        &mut conn,
        settings,
        &settings.service_agent_id,
        &settings.startup_recipient,
        &settings.startup_topic,
        &settings.startup_body,
        None,
        &[
            "startup".to_owned(),
            "system".to_owned(),
            "health".to_owned(),
        ],
        "normal",
        false,
        None,
        &meta,
    );
}

// ---------------------------------------------------------------------------
// Section 10 – main
// ---------------------------------------------------------------------------

#[expect(
    clippy::too_many_lines,
    reason = "main command dispatch — extracting further would obscure flow"
)]
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let settings = Settings::from_env();
    let cli = Cli::parse();

    match cli.command {
        Cmd::Health { ref encoding } => {
            cmd_health(&settings, encoding);
        }

        Cmd::Send {
            ref from_agent,
            ref to_agent,
            ref topic,
            ref body,
            ref thread_id,
            ref tag,
            ref priority,
            request_ack,
            ref reply_to,
            ref metadata,
            ref encoding,
        } => {
            cmd_send(
                &settings,
                &SendArgs {
                    from_agent,
                    to_agent,
                    topic,
                    body,
                    thread_id,
                    tags: tag,
                    priority,
                    request_ack,
                    reply_to,
                    metadata: metadata.as_deref(),
                    encoding,
                },
            )?;
        }

        Cmd::Read {
            ref agent,
            ref from_agent,
            since_minutes,
            limit,
            exclude_broadcast,
            ref encoding,
        } => {
            cmd_read(
                &settings,
                &ReadArgs {
                    agent,
                    from_agent,
                    since_minutes,
                    limit,
                    exclude_broadcast,
                    encoding,
                },
            )?;
        }

        Cmd::Watch {
            ref agent,
            history,
            exclude_broadcast,
            ref encoding,
        } => {
            cmd_watch(&settings, agent, history, exclude_broadcast, encoding)?;
        }

        Cmd::Ack {
            ref agent,
            ref message_id,
            ref body,
            ref encoding,
        } => {
            cmd_ack(&settings, agent, message_id, body, encoding)?;
        }

        Cmd::Presence {
            ref agent,
            ref status,
            ref session_id,
            ref capability,
            ttl_seconds,
            ref metadata,
            ref encoding,
        } => {
            cmd_presence(
                &settings,
                &PresenceArgs {
                    agent,
                    status,
                    session_id,
                    capabilities: capability,
                    ttl_seconds,
                    metadata: metadata.as_deref(),
                    encoding,
                },
            )?;
        }

        Cmd::PresenceList { ref encoding } => {
            cmd_presence_list(&settings, encoding)?;
        }

        Cmd::Serve {
            ref transport,
            port,
        } => {
            if transport == "http" {
                maybe_announce_startup(&settings);
                start_http_server(settings, port).await?;
            } else {
                // Default: MCP stdio transport.
                // Start the MCP server FIRST so it can respond to initialize
                // immediately, then announce startup in the background.
                let announce_settings = settings.clone();
                let server = AgentBusMcpServer::new(settings);
                let mcp_transport = (tokio::io::stdin(), tokio::io::stdout());
                let service = serve_server(server, mcp_transport)
                    .await
                    .context("MCP server init failed")?;
                // Announce after MCP handshake completes (non-blocking)
                tokio::spawn(async move {
                    maybe_announce_startup(&announce_settings);
                });
                service.waiting().await.context("MCP server loop error")?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Section 11 – Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_priority --------------------------------------------------

    #[test]
    fn validate_priority_accepts_all_valid_values() {
        for &p in VALID_PRIORITIES {
            assert!(validate_priority(p).is_ok(), "expected '{p}' to be valid");
        }
    }

    #[test]
    fn validate_priority_rejects_unknown() {
        assert!(validate_priority("critical").is_err());
        assert!(validate_priority("").is_err());
        assert!(validate_priority("NORMAL").is_err());
    }

    // -- non_empty ----------------------------------------------------------

    #[test]
    fn non_empty_returns_trimmed_value() {
        assert_eq!(non_empty("  hello  ", "field").unwrap(), "hello");
    }

    #[test]
    fn non_empty_rejects_blank() {
        assert!(non_empty("", "field").is_err());
        assert!(non_empty("   ", "field").is_err());
    }

    // -- parse_metadata_arg -------------------------------------------------

    #[test]
    fn parse_metadata_arg_none_returns_empty_object() {
        let val = parse_metadata_arg(None).unwrap();
        assert!(val.is_object());
        assert!(val.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_metadata_arg_valid_json() {
        let val = parse_metadata_arg(Some(r#"{"key":"value"}"#)).unwrap();
        assert_eq!(val["key"], "value");
    }

    #[test]
    fn parse_metadata_arg_invalid_json() {
        assert!(parse_metadata_arg(Some("not json")).is_err());
    }

    // -- default_priority ---------------------------------------------------

    #[test]
    fn default_priority_is_normal() {
        assert_eq!(default_priority(), "normal");
    }

    // -- minimize_value -----------------------------------------------------

    #[test]
    fn minimize_strips_defaults_and_shortens_keys() {
        let input = serde_json::json!({
            "timestamp_utc": "2026-01-01T00:00:00Z",
            "from": "claude",
            "to": "codex",
            "topic": "test",
            "body": "hello",
            "protocol_version": "1.0",
            "stream_id": "123-0",
            "tags": [],
            "metadata": {},
            "thread_id": null,
            "request_ack": false,
            "priority": "normal"
        });
        let minimized = minimize_value(&input);
        let obj = minimized.as_object().unwrap();

        // Stripped fields should be absent
        assert!(!obj.contains_key("protocol_version"));
        assert!(!obj.contains_key("stream_id"));
        assert!(!obj.contains_key("tags"));
        assert!(!obj.contains_key("tg"));
        assert!(!obj.contains_key("metadata"));
        assert!(!obj.contains_key("m"));
        assert!(!obj.contains_key("thread_id"));
        assert!(!obj.contains_key("tid"));
        assert!(!obj.contains_key("request_ack"));
        assert!(!obj.contains_key("ack"));
        assert!(!obj.contains_key("priority"));
        assert!(!obj.contains_key("p"));

        // Shortened keys should be present
        assert_eq!(obj["ts"], "2026-01-01T00:00:00Z");
        assert_eq!(obj["f"], "claude");
        assert_eq!(obj["t"], "codex");
        assert_eq!(obj["tp"], "test");
        assert_eq!(obj["b"], "hello");
    }

    #[test]
    fn minimize_preserves_non_default_values() {
        let input = serde_json::json!({
            "priority": "high",
            "request_ack": true,
            "tags": ["important"],
            "metadata": {"key": "val"},
            "thread_id": "abc-123"
        });
        let minimized = minimize_value(&input);
        let obj = minimized.as_object().unwrap();

        assert_eq!(obj["p"], "high");
        assert_eq!(obj["ack"], true);
        assert_eq!(obj["tg"], serde_json::json!(["important"]));
        assert_eq!(obj["m"], serde_json::json!({"key": "val"}));
        assert_eq!(obj["tid"], "abc-123");
    }

    // -- env helpers --------------------------------------------------------

    #[test]
    fn env_or_returns_default_for_missing_var() {
        let result = env_or("__AGENT_BUS_TEST_NONEXISTENT_KEY__", "fallback");
        assert_eq!(result, "fallback");
    }

    #[test]
    fn env_parse_returns_default_for_missing_var() {
        let result: u64 = env_parse("__AGENT_BUS_TEST_NONEXISTENT_KEY__", 42);
        assert_eq!(result, 42);
    }

    // -- Settings -----------------------------------------------------------

    #[test]
    fn settings_from_env_has_sane_defaults() {
        let s = Settings::from_env();
        assert!(s.redis_url.starts_with("redis://"));
        assert!(!s.stream_key.is_empty());
        assert!(!s.channel_key.is_empty());
        assert!(s.stream_maxlen > 0);
    }

    // -- decode_stream_entry ------------------------------------------------

    #[test]
    fn decode_stream_entry_handles_empty_fields() {
        let fields: HashMap<String, redis::Value> = HashMap::new();
        let msg = decode_stream_entry(&fields);
        assert!(msg.id.is_empty());
        assert!(msg.from.is_empty());
        assert_eq!(msg.priority, "normal");
        assert!(!msg.request_ack);
    }

    #[test]
    fn decode_stream_entry_reads_bulk_string_fields() {
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "id".to_owned(),
            redis::Value::BulkString(b"msg-123".to_vec()),
        );
        fields.insert(
            "from".to_owned(),
            redis::Value::BulkString(b"claude".to_vec()),
        );
        fields.insert("to".to_owned(), redis::Value::BulkString(b"codex".to_vec()));
        fields.insert(
            "topic".to_owned(),
            redis::Value::BulkString(b"test".to_vec()),
        );
        fields.insert(
            "body".to_owned(),
            redis::Value::BulkString(b"hello".to_vec()),
        );
        fields.insert(
            "priority".to_owned(),
            redis::Value::BulkString(b"high".to_vec()),
        );
        fields.insert(
            "request_ack".to_owned(),
            redis::Value::BulkString(b"true".to_vec()),
        );
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"tid-1".to_vec()),
        );
        fields.insert(
            "reply_to".to_owned(),
            redis::Value::BulkString(b"claude".to_vec()),
        );

        let msg = decode_stream_entry(&fields);
        assert_eq!(msg.id, "msg-123");
        assert_eq!(msg.from, "claude");
        assert_eq!(msg.to, "codex");
        assert_eq!(msg.topic, "test");
        assert_eq!(msg.body, "hello");
        assert_eq!(msg.priority, "high");
        assert!(msg.request_ack);
        assert_eq!(msg.thread_id, Some("tid-1".to_owned()));
        assert_eq!(msg.reply_to, Some("claude".to_owned()));
    }

    #[test]
    fn decode_stream_entry_thread_id_none_is_mapped() {
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"None".to_vec()),
        );
        let msg = decode_stream_entry(&fields);
        assert_eq!(msg.thread_id, None);
    }

    // -- parse_xrange_result ------------------------------------------------

    #[test]
    fn parse_xrange_result_handles_empty() {
        let raw: Vec<redis::Value> = vec![];
        let result = parse_xrange_result(&raw);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_xrange_result_parses_single_entry() {
        let entry = redis::Value::Array(vec![
            redis::Value::BulkString(b"1234-0".to_vec()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"id".to_vec()),
                redis::Value::BulkString(b"msg-1".to_vec()),
                redis::Value::BulkString(b"from".to_vec()),
                redis::Value::BulkString(b"claude".to_vec()),
            ]),
        ]);
        let raw = vec![entry];
        let result = parse_xrange_result(&raw);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "1234-0");
        assert!(result[0].1.contains_key("id"));
        assert!(result[0].1.contains_key("from"));
    }

    // -- MCP tool list ------------------------------------------------------

    #[test]
    fn tool_list_has_expected_count() {
        let tools = AgentBusMcpServer::tool_list();
        assert_eq!(tools.len(), 6, "expected 6 MCP tools");
    }

    #[test]
    fn tool_list_names_are_correct() {
        let tools = AgentBusMcpServer::tool_list();
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.iter().any(|n| n == "bus_health"));
        assert!(names.iter().any(|n| n == "post_message"));
        assert!(names.iter().any(|n| n == "list_messages"));
        assert!(names.iter().any(|n| n == "ack_message"));
        assert!(names.iter().any(|n| n == "set_presence"));
        assert!(names.iter().any(|n| n == "list_presence"));
    }

    // -- Health struct ------------------------------------------------------

    #[test]
    fn health_codec_field_is_accurate() {
        let s = Settings::from_env();
        let h = redis_bus::bus_health(&s);
        assert_eq!(h.codec, "serde_json");
        assert_eq!(h.runtime, "rust-native");
    }

    #[test]
    fn redact_url_hides_credentials() {
        assert_eq!(
            redact_url("postgresql://postgres:secret@localhost:5432/redis_backend"),
            "postgresql://***:***@localhost:5432/redis_backend"
        );
        assert_eq!(
            redact_url("redis://default@localhost:6380/0"),
            "redis://***@localhost:6380/0"
        );
    }

    #[test]
    fn redact_url_leaves_plain_urls_unchanged() {
        assert_eq!(
            redact_url("redis://localhost:6380/0"),
            "redis://localhost:6380/0"
        );
    }

    // -- Constants ----------------------------------------------------------

    #[test]
    fn max_history_minutes_is_one_week() {
        assert_eq!(MAX_HISTORY_MINUTES, 7 * 24 * 60);
    }
}
