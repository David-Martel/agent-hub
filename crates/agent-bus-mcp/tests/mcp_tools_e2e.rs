//! End-to-end MCP tool-dispatch integration tests.
//!
//! These drive [`agent_bus_core::mcp_dispatch::McpToolDispatch::dispatch_tool`]
//! directly — the same code path the stdio and Streamable-HTTP MCP transports
//! delegate to. No existing test exercised the live tool-dispatch path against
//! real backends, so these cover the core coordination verbs end to end:
//! `check_inbox`, channel create/post/read, claim/renew/release/resolve, and
//! `negotiate`.
//!
//! Prerequisites: a live Redis on `:6380` and (for `check_inbox` / durable
//! history) `PostgreSQL` on `:5300`, i.e. the agent-bus defaults. Every test
//! guards on [`backend_available`] and skips cleanly when Redis is unreachable,
//! mirroring the service-availability idiom in
//! `crates/agent-bus-cli/tests/http_integration_test.rs`.
//!
//! Test isolation: each test derives unique agent / resource / group names from
//! a millisecond-precision timestamp plus an atomic counter, and uses a
//! dedicated test stream/presence key prefix via environment variables so the
//! production streams are never touched.
//!
//! # Running
//!
//! ```text
//! cargo test -p agent-bus-mcp --test mcp_tools_e2e -- --test-threads=1
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_bus_core::mcp_dispatch::McpToolDispatch;
use agent_bus_core::redis_bus::connect;
use agent_bus_core::settings::Settings;
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Returns a per-process-unique suffix (timestamp ms + monotonic counter).
fn unique_suffix() -> String {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "milliseconds since epoch fit in u64 for centuries"
    )]
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ms}-{n}")
}

/// Build [`Settings`] pinned to test-isolated stream/presence keys so we never
/// pollute production streams. `PostgreSQL` is left at the default so the
/// durable history path (used by `check_inbox`) is exercised when PG is up.
fn test_settings() -> Settings {
    let mut s = Settings::from_env();
    "agent_bus:test:mcp_e2e:messages".clone_into(&mut s.stream_key);
    "agent_bus:test:mcp_e2e:events".clone_into(&mut s.channel_key);
    "agent_bus:test:mcp_e2e:presence:".clone_into(&mut s.presence_prefix);
    s
}

/// Returns `true` when Redis is reachable, so tests can skip gracefully.
fn backend_available(settings: &Settings) -> bool {
    connect(settings).is_ok()
}

/// Convenience: dispatch a tool with a JSON object argument.
fn call(
    dispatch: &McpToolDispatch<'_>,
    name: &str,
    args: &Value,
) -> agent_bus_core::error::Result<Value> {
    let obj: Map<String, Value> = args
        .as_object()
        .cloned()
        .unwrap_or_else(serde_json::Map::new);
    dispatch.dispatch_tool(name, &obj)
}

// ---------------------------------------------------------------------------
// negotiate (no backend required) — proves the dispatch wiring is live
// ---------------------------------------------------------------------------

#[test]
fn negotiate_returns_capabilities() {
    // negotiate never touches Redis, so it runs even without a backend.
    let settings = test_settings();
    let dispatch = McpToolDispatch::new(&settings);

    let val = call(&dispatch, "negotiate", &json!({})).expect("negotiate must succeed");

    assert!(
        val["protocol_version"].as_str().is_some() || val["protocol_version"].as_u64().is_some(),
        "negotiate must report a protocol_version: {val}"
    );
    let transports = val["transports"]
        .as_array()
        .expect("transports must be an array");
    assert!(
        transports.iter().any(|t| t.as_str() == Some("mcp-http")),
        "negotiate transports should include mcp-http: {val}"
    );
    let features = val["features"].as_array().expect("features array");
    assert!(
        features.iter().any(|f| f.as_str() == Some("channels")),
        "negotiate should advertise the channels feature: {val}"
    );
    assert_eq!(
        val["tools"].as_u64(),
        Some(17),
        "negotiate should report 17 tools: {val}"
    );
}

// ---------------------------------------------------------------------------
// check_inbox — cursor-based durable delivery via the dispatch path
// ---------------------------------------------------------------------------

#[test]
fn check_inbox_delivers_posted_message_then_advances_cursor() {
    let settings = test_settings();
    if !backend_available(&settings) {
        eprintln!("SKIP: Redis not reachable for check_inbox test");
        return;
    }
    let dispatch = McpToolDispatch::new(&settings);

    let suffix = unique_suffix();
    let sender = format!("mcp-inbox-sender-{suffix}");
    let recipient = format!("mcp-inbox-recv-{suffix}");
    let body = format!("inbox-body-{suffix}");

    // Reset the cursor first so this fresh agent starts from a clean slate.
    let reset = call(
        &dispatch,
        "check_inbox",
        &json!({"agent": recipient, "reset_cursor": true}),
    )
    .expect("check_inbox reset must succeed");
    assert_eq!(reset["agent"].as_str(), Some(recipient.as_str()));

    // Post a direct message to the recipient via post_message (request_ack so it
    // produces a durable per-agent notification).
    let posted = call(
        &dispatch,
        "post_message",
        &json!({
            "sender": sender,
            "recipient": recipient,
            "topic": "status",
            "body": body,
            "request_ack": true,
        }),
    )
    .expect("post_message must succeed");
    let message_id = posted["id"].as_str().expect("posted message must have id");

    // First check_inbox after the post should deliver the message.
    let first = call(&dispatch, "check_inbox", &json!({"agent": recipient}))
        .expect("check_inbox must succeed");
    let messages = first["messages"]
        .as_array()
        .expect("check_inbox messages must be an array");
    assert!(
        messages
            .iter()
            .any(|m| m["id"].as_str() == Some(message_id)),
        "check_inbox should deliver the freshly posted message {message_id}: {first}"
    );
    assert!(
        first["cursor_now"].as_str().is_some(),
        "check_inbox must report a cursor_now: {first}"
    );

    // Second check (no new messages) must return an empty delivery — the cursor
    // advanced past the only notification.
    let second = call(&dispatch, "check_inbox", &json!({"agent": recipient}))
        .expect("second check_inbox must succeed");
    let second_msgs = second["messages"]
        .as_array()
        .expect("messages array on second check");
    assert!(
        second_msgs
            .iter()
            .all(|m| m["id"].as_str() != Some(message_id)),
        "the same message must not be redelivered after the cursor advanced: {second}"
    );
}

// ---------------------------------------------------------------------------
// Channels: create_channel (group) -> post_to_channel -> read_channel
// ---------------------------------------------------------------------------

#[test]
fn channel_group_create_post_read_round_trip() {
    let settings = test_settings();
    if !backend_available(&settings) {
        eprintln!("SKIP: Redis not reachable for channel group test");
        return;
    }
    let dispatch = McpToolDispatch::new(&settings);

    let suffix = unique_suffix().replace('-', "");
    let group = format!("grp{suffix}");
    let creator = format!("grp-creator-{suffix}");
    let body = format!("group-msg-{suffix}");

    // create_channel (group) — first claim of the group name.
    let created = call(
        &dispatch,
        "create_channel",
        &json!({
            "channel_type": "group",
            "name": group,
            "members": [creator],
            "created_by": creator,
        }),
    )
    .expect("create_channel group must succeed");
    assert!(
        created.get("name").is_some() || created.get("members").is_some(),
        "create_channel should return group info: {created}"
    );

    // post_to_channel (group).
    let posted = call(
        &dispatch,
        "post_to_channel",
        &json!({
            "channel_type": "group",
            "sender": creator,
            "recipient": group,
            "topic": "group-topic",
            "body": body,
        }),
    )
    .expect("post_to_channel group must succeed");
    assert_eq!(posted["body"].as_str(), Some(body.as_str()));

    // read_channel (group).
    let read = call(
        &dispatch,
        "read_channel",
        &json!({
            "channel_type": "group",
            "group_name": group,
            "limit": 10,
        }),
    )
    .expect("read_channel group must succeed");
    let msgs = read.as_array().expect("read_channel must return an array");
    assert!(
        msgs.iter()
            .any(|m| m["body"].as_str() == Some(body.as_str())),
        "read_channel should return the posted group message: {read}"
    );
}

#[test]
fn channel_direct_post_and_read_round_trip() {
    let settings = test_settings();
    if !backend_available(&settings) {
        eprintln!("SKIP: Redis not reachable for direct channel test");
        return;
    }
    let dispatch = McpToolDispatch::new(&settings);

    let suffix = unique_suffix();
    let agent_a = format!("mcp-direct-a-{suffix}");
    let agent_b = format!("mcp-direct-b-{suffix}");
    let body = format!("direct-msg-{suffix}");

    let posted = call(
        &dispatch,
        "post_to_channel",
        &json!({
            "channel_type": "direct",
            "sender": agent_a,
            "recipient": agent_b,
            "topic": "direct-topic",
            "body": body,
        }),
    )
    .expect("post_to_channel direct must succeed");
    assert_eq!(posted["body"].as_str(), Some(body.as_str()));

    let read = call(
        &dispatch,
        "read_channel",
        &json!({
            "channel_type": "direct",
            "agent_a": agent_a,
            "agent_b": agent_b,
            "limit": 10,
        }),
    )
    .expect("read_channel direct must succeed");
    let msgs = read.as_array().expect("read_channel must return an array");
    assert!(
        msgs.iter()
            .any(|m| m["body"].as_str() == Some(body.as_str())),
        "read_channel direct should return the posted message: {read}"
    );
}

// ---------------------------------------------------------------------------
// Claims: claim_resource -> renew_claim -> release_claim
// ---------------------------------------------------------------------------

#[test]
fn claim_renew_release_round_trip() {
    let settings = test_settings();
    if !backend_available(&settings) {
        eprintln!("SKIP: Redis not reachable for claim lifecycle test");
        return;
    }
    let dispatch = McpToolDispatch::new(&settings);

    let suffix = unique_suffix();
    let resource = format!("src/mcp-lease-{suffix}.rs");
    let agent = format!("mcp-lease-agent-{suffix}");

    // claim_resource — shared_namespaced so concurrent tests never collide.
    let claim = call(
        &dispatch,
        "claim_resource",
        &json!({
            "resource": resource,
            "agent": agent,
            "reason": "mcp lease lifecycle test",
            "mode": "shared_namespaced",
            "namespace": format!("ns-{suffix}"),
            "lease_ttl_seconds": 300,
        }),
    )
    .expect("claim_resource must succeed");
    assert_eq!(
        claim["agent"].as_str(),
        Some(agent.as_str()),
        "claim should record the requesting agent: {claim}"
    );

    // renew_claim — extend the lease.
    let renewed = call(
        &dispatch,
        "renew_claim",
        &json!({
            "resource": resource,
            "agent": agent,
            "lease_ttl_seconds": 600,
        }),
    )
    .expect("renew_claim must succeed");
    assert_eq!(
        renewed["lease_ttl_seconds"].as_u64(),
        Some(600),
        "renew_claim should reflect the new TTL: {renewed}"
    );

    // release_claim — the claim list should be empty afterwards.
    let released = call(
        &dispatch,
        "release_claim",
        &json!({"resource": resource, "agent": agent}),
    )
    .expect("release_claim must succeed");
    let claim_count = released["claims"].as_array().map_or(0, std::vec::Vec::len);
    assert_eq!(
        claim_count, 0,
        "release_claim should leave no remaining claims: {released}"
    );
}

#[test]
fn claim_resolve_names_winner() {
    let settings = test_settings();
    if !backend_available(&settings) {
        eprintln!("SKIP: Redis not reachable for claim resolve test");
        return;
    }
    let dispatch = McpToolDispatch::new(&settings);

    let suffix = unique_suffix();
    let resource = format!("src/mcp-resolve-{suffix}.rs");
    let agent_a = format!("mcp-resolve-a-{suffix}");
    let agent_b = format!("mcp-resolve-b-{suffix}");

    // Two exclusive claims establish a contested state.
    call(
        &dispatch,
        "claim_resource",
        &json!({"resource": resource, "agent": agent_a}),
    )
    .expect("first claim must succeed");
    // Second claim may be contested; either way the dispatch must succeed.
    call(
        &dispatch,
        "claim_resource",
        &json!({"resource": resource, "agent": agent_b}),
    )
    .expect("second claim must succeed");

    // resolve_claim in favour of agent_a.
    let resolved = call(
        &dispatch,
        "resolve_claim",
        &json!({
            "resource": resource,
            "winner": agent_a,
            "reason": "agent_a has priority",
            "resolved_by": "mcp-test-orchestrator",
        }),
    )
    .expect("resolve_claim must succeed");

    // The resolved state should name agent_a as the winner/owner somewhere.
    let names_winner = resolved["winner"].as_str() == Some(agent_a.as_str())
        || resolved["owner"].as_str() == Some(agent_a.as_str())
        || resolved.to_string().contains(&agent_a);
    assert!(
        names_winner,
        "resolve_claim should record agent_a as winner: {resolved}"
    );

    // Clean up the lingering claim so we don't leave contested state behind.
    let _ = call(
        &dispatch,
        "release_claim",
        &json!({"resource": resource, "agent": agent_a}),
    );
    let _ = call(
        &dispatch,
        "release_claim",
        &json!({"resource": resource, "agent": agent_b}),
    );
}

// ---------------------------------------------------------------------------
// Error path: unknown tool name must surface an error through dispatch.
// ---------------------------------------------------------------------------

#[test]
fn unknown_tool_dispatch_errors() {
    let settings = test_settings();
    let dispatch = McpToolDispatch::new(&settings);
    let err =
        call(&dispatch, "definitely_not_a_tool", &json!({})).expect_err("unknown tool must error");
    assert!(
        err.to_string().contains("unknown tool"),
        "error should mention unknown tool: {err}"
    );
}
