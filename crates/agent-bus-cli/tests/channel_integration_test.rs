//! End-to-end channel integration tests.
//!
//! Exercises direct messages, group lifecycles, resource claim/resolve, and
//! ownership-topic send via the compiled `agent-bus` binary.  Every backend test
//! is `#[ignore]`d and runs only with `-- --ignored`, against the disposable
//! backends named by `AGENT_BUS_TEST_*` (see
//! `crates/agent-bus-core/tests/support/backend_env.rs`). An unset variable or an
//! unreachable backend FAILS the test; nothing here skips or defaults to the
//! live bus.
//!
//! Each test generates a unique suffix via `uuid::Uuid::new_v4().as_simple()`
//! so parallel runs never share Redis keys.
//!
//! # Running
//!
//! ```text
//! cargo test --test channel_integration_test -- --ignored
//! ```

use std::process::Command;

#[path = "../../agent-bus-core/tests/support/backend_env.rs"]
mod backend_env;

use backend_env::{DATABASE_URL_VAR, REDIS_URL_VAR, SERVER_URL_VAR, backend_url};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn agent_bus_binary() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    // Use the test-isolated stream/presence keys so we don't pollute production data.
    cmd.env_remove("AGENT_BUS_SERVER_URL");
    // Keep the developer's ~/.config/agent-bus/config.json (which may name a
    // server_url or token for the real hub) out of the child's settings.
    cmd.env(
        "AGENT_BUS_CONFIG",
        std::env::temp_dir().join(format!("agent-bus-test-config-{}.json", std::process::id())),
    );
    cmd.env("AGENT_BUS_REDIS_URL", backend_url(REDIS_URL_VAR));
    cmd.env("AGENT_BUS_DATABASE_URL", backend_url(DATABASE_URL_VAR));
    cmd.env("AGENT_BUS_STREAM_KEY", "agent_bus:test:messages");
    cmd.env("AGENT_BUS_CHANNEL", "agent_bus:test:events");
    cmd.env("AGENT_BUS_PRESENCE_PREFIX", "agent_bus:test:presence:");
    cmd
}

/// Fail (not skip) when the configured backend cannot serve `health`.
fn require_backend() {
    let output = agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run agent-bus health");
    assert!(
        output.status.success(),
        "backend unreachable via {REDIS_URL_VAR}: agent-bus health exited {} -- stdout: {} stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Generate a UUID-based unique suffix suitable for resource/group names.
///
/// Uses the simple (hyphen-free) format so it is also a valid group name
/// (alphanumerics only, no special characters beyond what the validator allows).
fn unique_id() -> String {
    uuid::Uuid::new_v4().as_simple().to_string()
}

fn http_client() -> reqwest::blocking::Client {
    let mut headers = HeaderMap::new();
    if let Ok(token) = std::env::var("AGENT_BUS_AUTH_TOKEN")
        && !token.is_empty()
    {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .expect("AGENT_BUS_AUTH_TOKEN should be a valid HTTP header value");
        headers.insert(AUTHORIZATION, value);
    }

    reqwest::blocking::Client::builder()
        .default_headers(headers)
        .build()
        .expect("failed to build HTTP test client")
}

// ---------------------------------------------------------------------------
// Test 1: direct message round-trip
// ---------------------------------------------------------------------------

/// Post a direct message from `agent-a` to `agent-b`, read it back, and verify
/// the body is present in the output.
#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn channel_direct_message_round_trip() {
    require_backend();

    let suffix = unique_id();
    let agent_a = format!("test-a-{suffix}");
    let agent_b = format!("test-b-{suffix}");
    let body = format!("direct-body-{suffix}");

    // Post a direct message from agent-a to agent-b.
    let send = agent_bus_binary()
        .args([
            "post-direct",
            "--from-agent",
            &agent_a,
            "--to-agent",
            &agent_b,
            "--body",
            &body,
            "--encoding",
            "compact",
        ])
        .output()
        .expect("post-direct failed to run");

    assert!(
        send.status.success(),
        "post-direct exited non-zero: {}",
        String::from_utf8_lossy(&send.stderr)
    );

    // Read direct messages between the two agents.
    let read = agent_bus_binary()
        .args([
            "read-direct",
            "--agent-a",
            &agent_a,
            "--agent-b",
            &agent_b,
            "--limit",
            "10",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("read-direct failed to run");

    assert!(
        read.status.success(),
        "read-direct exited non-zero: {}",
        String::from_utf8_lossy(&read.stderr)
    );

    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(
        stdout.contains(&body),
        "expected body '{body}' in read-direct output:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: group channel lifecycle
// ---------------------------------------------------------------------------

/// Create a named group, post a message to it, read it back, and verify the
/// body is present.  The group name is UUID-derived so it is unique per run.
#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn channel_group_lifecycle() {
    require_backend();

    // Group names must be alphanumerics + hyphens + underscores.
    // Uuid::as_simple() gives 32 hex chars — prefix with "grp" so it is clearly
    // a group name and stays under any Redis key length concern.
    let suffix = unique_id();
    let group = format!("grp{suffix}");
    let sender = format!("test-sender-{suffix}");
    let body = format!("group-body-{suffix}");

    // Create the group first (required before post-group). The CLI has no
    // group-create verb, so this goes through the HTTP server; a failure there
    // fails the test rather than silently skipping the body assertion.
    let http_base = backend_url(SERVER_URL_VAR);
    let create_resp = http_client()
        .post(format!("{http_base}/channels/groups"))
        .json(&serde_json::json!({
            "name": group,
            "created_by": sender,
            "members": [sender]
        }))
        .send()
        .unwrap_or_else(|e| panic!("group create via {SERVER_URL_VAR}={http_base} failed: {e}"));
    assert!(
        create_resp.status().is_success(),
        "group create returned {}",
        create_resp.status()
    );

    // Post a message to the group (succeeds only when group exists).
    let post = agent_bus_binary()
        .args([
            "post-group",
            "--group",
            &group,
            "--from-agent",
            &sender,
            "--body",
            &body,
            "--encoding",
            "compact",
        ])
        .output()
        .expect("post-group failed to run");

    assert!(
        post.status.success(),
        "post-group exited non-zero: {}",
        String::from_utf8_lossy(&post.stderr)
    );

    // Read messages from the group.
    let read = agent_bus_binary()
        .args([
            "read-group",
            "--group",
            &group,
            "--limit",
            "10",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("read-group failed to run");

    assert!(
        read.status.success(),
        "read-group exited non-zero: {}",
        String::from_utf8_lossy(&read.stderr)
    );

    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(
        stdout.contains(&body),
        "expected body '{body}' in read-group output:\n{stdout}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: claim and resolve
// ---------------------------------------------------------------------------

/// Claim a unique resource, list claims and verify it appears, then resolve it
/// and verify the winner.
#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn claim_and_resolve() {
    require_backend();

    let suffix = unique_id();
    // Use a path-like resource name that is valid on both Unix and Windows.
    let resource = format!("src/test-file-{suffix}.rs");
    let agent = format!("test-claimer-{suffix}");

    // Claim the resource.
    let claim = agent_bus_binary()
        .args([
            "claim",
            &resource,
            "--agent",
            &agent,
            "--reason",
            "integration test claim",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("claim failed to run");

    assert!(
        claim.status.success(),
        "claim exited non-zero: {}",
        String::from_utf8_lossy(&claim.stderr)
    );

    let claim_stdout = String::from_utf8_lossy(&claim.stdout);
    assert!(
        claim_stdout.contains(&agent),
        "claim output missing agent '{agent}':\n{claim_stdout}"
    );

    // List claims and verify the resource appears.
    let list = agent_bus_binary()
        .args(["claims", "--resource", &resource, "--encoding", "compact"])
        .output()
        .expect("claims failed to run");

    assert!(
        list.status.success(),
        "claims exited non-zero: {}",
        String::from_utf8_lossy(&list.stderr)
    );

    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains(&agent),
        "claims list missing agent '{agent}':\n{list_stdout}"
    );

    // Resolve the claim — same agent wins (uncontested).
    let resolve = agent_bus_binary()
        .args([
            "resolve",
            &resource,
            "--winner",
            &agent,
            "--reason",
            "integration test resolution",
            "--resolved-by",
            "test-orchestrator",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("resolve failed to run");

    assert!(
        resolve.status.success(),
        "resolve exited non-zero: {}",
        String::from_utf8_lossy(&resolve.stderr)
    );

    let resolve_stdout = String::from_utf8_lossy(&resolve.stdout);
    assert!(
        resolve_stdout.contains(&agent),
        "resolve output missing winner '{agent}':\n{resolve_stdout}"
    );
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn renew_and_release_claim_round_trip() {
    require_backend();

    let suffix = unique_id();
    let resource = format!("src/test-lease-{suffix}.rs");
    let agent = format!("test-lease-{suffix}");

    let claim = agent_bus_binary()
        .args([
            "claim",
            &resource,
            "--agent",
            &agent,
            "--mode",
            "shared_namespaced",
            "--namespace",
            "cli-test",
            "--lease-ttl-seconds",
            "300",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("claim failed to run");
    assert!(
        claim.status.success(),
        "claim failed: {}",
        String::from_utf8_lossy(&claim.stderr)
    );

    let renew = agent_bus_binary()
        .args([
            "renew-claim",
            &resource,
            "--agent",
            &agent,
            "--lease-ttl-seconds",
            "600",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("renew-claim failed to run");
    assert!(
        renew.status.success(),
        "renew-claim failed: {}",
        String::from_utf8_lossy(&renew.stderr)
    );
    let renew_stdout = String::from_utf8_lossy(&renew.stdout);
    assert!(
        renew_stdout.contains("\"lease_ttl_seconds\":600")
            || renew_stdout.contains("\"lease_ttl_seconds\": 600")
    );

    let release = agent_bus_binary()
        .args([
            "release-claim",
            &resource,
            "--agent",
            &agent,
            "--encoding",
            "compact",
        ])
        .output()
        .expect("release-claim failed to run");
    assert!(
        release.status.success(),
        "release-claim failed: {}",
        String::from_utf8_lossy(&release.stderr)
    );
    let release_stdout = String::from_utf8_lossy(&release.stdout);
    assert!(release_stdout.contains("\"claims\":[]") || release_stdout.contains("\"claims\": []"));
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn knock_command_posts_message() {
    require_backend();

    let suffix = unique_id();
    let sender = format!("knock-sender-{suffix}");
    let recipient = format!("knock-recipient-{suffix}");
    let body = format!("knock-body-{suffix}");

    let knock = agent_bus_binary()
        .args([
            "knock",
            "--from-agent",
            &sender,
            "--to-agent",
            &recipient,
            "--body",
            &body,
            "--encoding",
            "compact",
        ])
        .output()
        .expect("knock failed to run");
    assert!(
        knock.status.success(),
        "knock failed: {}",
        String::from_utf8_lossy(&knock.stderr)
    );

    let read = agent_bus_binary()
        .args([
            "read",
            "--agent",
            &recipient,
            "--since-minutes",
            "10",
            "--limit",
            "20",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("read failed to run");
    assert!(
        read.status.success(),
        "read failed: {}",
        String::from_utf8_lossy(&read.stderr)
    );
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(stdout.contains("\"topic\":\"knock\"") || stdout.contains("\"topic\": \"knock\""));
    assert!(stdout.contains(&body));
}

// ---------------------------------------------------------------------------
// Test 4: ownership-topic send
// ---------------------------------------------------------------------------

/// Send a message with `topic=ownership` and verify the bus accepts it without
/// error.  This exercises the schema auto-inference path (`status` schema) on
/// the ownership topic.
#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn ownership_via_topic() {
    require_backend();

    let suffix = unique_id();
    let from_agent = format!("test-owner-{suffix}");
    let body = format!("CLAIMING: src/lib-{suffix}.rs for refactor");

    let send = agent_bus_binary()
        .args([
            "send",
            "--from-agent",
            &from_agent,
            "--to-agent",
            "all",
            "--topic",
            "ownership",
            "--body",
            &body,
            "--encoding",
            "compact",
        ])
        .output()
        .expect("send with topic=ownership failed to run");

    assert!(
        send.status.success(),
        "send (topic=ownership) exited non-zero: stderr={}",
        String::from_utf8_lossy(&send.stderr)
    );

    // The stdout should contain a well-formed message JSON.
    let stdout = String::from_utf8_lossy(&send.stdout);
    assert!(
        stdout.contains("ownership"),
        "send output missing topic 'ownership':\n{stdout}"
    );
}
