//! CLI/HTTP parity integration tests.
//!
//! Verifies that the CLI binary and the HTTP server agree on three flows that
//! were previously only covered on a single surface:
//!
//! 1. `read-direct` over **real direct-channel traffic** — a message posted via
//!    the CLI `post-direct` command is read back identically via both the CLI
//!    `read-direct` command and the HTTP `GET /channels/direct/:agent` route.
//!    Direct-channel streams (`bus:direct:<lo>:<hi>`) are global Redis keys
//!    independent of the configurable main stream, so a CLI subprocess and the
//!    running HTTP server share them.
//! 2. `compact-context` — main-stream messages scoped by a unique `thread_id`
//!    and repo tag are compacted identically via the CLI `compact-context`
//!    command and the HTTP `POST /compact-context` route.
//! 3. thread-summary — main-stream messages tagged with a unique `thread_id`
//!    are aggregated by the CLI `summarize-thread` command.
//!
//! Prerequisites: the agent-bus HTTP server must be running at `localhost:8400`
//! with the default Redis (`:6380`). All tests skip gracefully when the server
//! is not reachable. The CLI subprocesses are run with **default settings** (no
//! `AGENT_BUS_STREAM_KEY` override) so they target the same Redis keys as the
//! running server.
//!
//! Isolation: every test derives unique agent / thread / repo names from a
//! millisecond timestamp plus an atomic counter.
//!
//! # Running
//!
//! ```text
//! cargo test -p agent-bus --test cli_http_parity_test -- --test-threads=1
//! ```

use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};

const BASE_URL: &str = "http://localhost:8400";

static COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// Returns `true` when the HTTP server is reachable.
fn http_available(client: &reqwest::blocking::Client) -> bool {
    client.get(format!("{BASE_URL}/health")).send().is_ok()
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

/// The CLI binary, with **default** settings so its Redis keys match the
/// running HTTP server.
fn agent_bus_binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_agent-bus"))
}

// ---------------------------------------------------------------------------
// 1. read-direct parity over real direct-channel traffic
// ---------------------------------------------------------------------------

#[test]
fn read_direct_parity_cli_and_http() {
    let client = http_client();
    if !http_available(&client) {
        eprintln!("SKIP: agent-bus HTTP not running at {BASE_URL}");
        return;
    }

    let suffix = unique_suffix();
    let agent_a = format!("parity-a-{suffix}");
    let agent_b = format!("parity-b-{suffix}");
    let body = format!("direct-parity-body-{suffix}");

    // Post a REAL direct-channel message via the CLI `post-direct` command.
    let post = agent_bus_binary()
        .args([
            "post-direct",
            "--from-agent",
            &agent_a,
            "--to-agent",
            &agent_b,
            "--body",
            &body,
            "--encoding",
            "json",
        ])
        .output()
        .expect("post-direct failed to run");
    assert!(
        post.status.success(),
        "post-direct exited non-zero: {}",
        String::from_utf8_lossy(&post.stderr)
    );

    // (a) Read back via the CLI `read-direct` command.
    let cli_read = agent_bus_binary()
        .args([
            "read-direct",
            "--agent-a",
            &agent_a,
            "--agent-b",
            &agent_b,
            "--limit",
            "10",
            "--encoding",
            "json",
        ])
        .output()
        .expect("read-direct failed to run");
    assert!(
        cli_read.status.success(),
        "read-direct exited non-zero: {}",
        String::from_utf8_lossy(&cli_read.stderr)
    );
    let cli_stdout = String::from_utf8_lossy(&cli_read.stdout);
    assert!(
        cli_stdout.contains(&body),
        "CLI read-direct missing body '{body}':\n{cli_stdout}"
    );

    // (b) Read back via the HTTP direct-channel route. Reading as agent_b
    //     viewing the conversation with agent_a.
    let http_resp = client
        .get(format!(
            "{BASE_URL}/channels/direct/{agent_a}?agent={agent_b}"
        ))
        .send()
        .expect("GET /channels/direct failed");
    assert!(
        http_resp.status().is_success(),
        "HTTP direct read returned {}",
        http_resp.status()
    );
    let http_msgs: Value = http_resp.json().expect("HTTP direct read not JSON");
    let arr = http_msgs
        .as_array()
        .expect("HTTP direct read should return an array");
    let http_has_body = arr
        .iter()
        .any(|m| m["body"].as_str() == Some(body.as_str()));
    assert!(
        http_has_body,
        "HTTP direct read missing body '{body}': {http_msgs}"
    );

    // Parity: both surfaces must observe the same direct-channel message.
    assert!(
        cli_stdout.contains(&body) && http_has_body,
        "CLI and HTTP must agree on the direct-channel message"
    );
}

// ---------------------------------------------------------------------------
// 2. compact-context parity (CLI vs HTTP) over main-stream messages
// ---------------------------------------------------------------------------

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "linear seed -> HTTP-compact -> CLI-compact -> parity-assert flow reads clearer inline than split across helpers"
)]
fn compact_context_parity_cli_and_http() {
    let client = http_client();
    if !http_available(&client) {
        eprintln!("SKIP: agent-bus HTTP not running at {BASE_URL}");
        return;
    }

    let suffix = unique_suffix();
    let recipient = format!("compact-parity-{suffix}");
    let repo = format!("repo-parity-{suffix}");
    let thread_id = format!("thread-parity-{suffix}");
    let keep_body = format!("keep-compact-{suffix}");
    let drop_body = format!("drop-compact-{suffix}");

    // Post a matching message and a non-matching message to the MAIN stream via
    // the HTTP API (so both CLI and HTTP read the same backing stream).
    for (body, repo_tag, tid) in [
        (keep_body.as_str(), repo.as_str(), thread_id.as_str()),
        (drop_body.as_str(), "other-repo", "other-thread"),
    ] {
        let resp = client
            .post(format!("{BASE_URL}/messages"))
            .json(&json!({
                "sender": format!("compact-sender-{suffix}"),
                "recipient": recipient,
                "topic": "planning",
                "body": body,
                "thread_id": tid,
                "tags": [format!("repo:{repo_tag}"), "planning"],
            }))
            .send()
            .expect("POST /messages failed");
        assert!(resp.status().is_success(), "seed message POST failed");
    }

    // (a) compact-context via HTTP.
    let http_resp = client
        .post(format!("{BASE_URL}/compact-context"))
        .json(&json!({
            "agent": recipient,
            "repo": repo,
            "tags": ["planning"],
            "thread_id": thread_id,
            "since_minutes": 10,
            "max_tokens": 4000,
        }))
        .send()
        .expect("POST /compact-context failed");
    assert!(
        http_resp.status().is_success(),
        "HTTP compact-context failed"
    );
    let http_items: Value = http_resp.json().expect("compact-context not JSON");
    let http_arr = http_items
        .as_array()
        .expect("compact-context must return an array");
    let http_has_keep = http_arr
        .iter()
        .any(|m| m["b"].as_str() == Some(keep_body.as_str()));
    let http_has_drop = http_arr
        .iter()
        .any(|m| m["b"].as_str() == Some(drop_body.as_str()));
    assert!(
        http_has_keep,
        "HTTP compact-context should include the matching thread message: {http_items}"
    );
    assert!(
        !http_has_drop,
        "HTTP compact-context should exclude the non-matching message: {http_items}"
    );

    // (b) compact-context via CLI — same filters, JSON output.
    let cli = agent_bus_binary()
        .args([
            "compact-context",
            "--agent",
            &recipient,
            "--repo",
            &repo,
            "--tag",
            "planning",
            "--thread-id",
            &thread_id,
            "--since-minutes",
            "10",
            "--max-tokens",
            "4000",
            "--encoding",
            "json",
        ])
        .output()
        .expect("compact-context CLI failed to run");
    assert!(
        cli.status.success(),
        "CLI compact-context exited non-zero: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_stdout = String::from_utf8_lossy(&cli.stdout);
    assert!(
        cli_stdout.contains(&keep_body),
        "CLI compact-context should include the matching message:\n{cli_stdout}"
    );
    assert!(
        !cli_stdout.contains(&drop_body),
        "CLI compact-context should exclude the non-matching message:\n{cli_stdout}"
    );

    // Parity: both surfaces agree on inclusion/exclusion.
    assert!(
        http_has_keep && cli_stdout.contains(&keep_body),
        "CLI and HTTP must agree that the matching message is included"
    );
    assert!(
        !http_has_drop && !cli_stdout.contains(&drop_body),
        "CLI and HTTP must agree that the non-matching message is excluded"
    );
}

// ---------------------------------------------------------------------------
// 3. thread-summary flow (CLI) over main-stream thread traffic
// ---------------------------------------------------------------------------

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "linear seed -> durable-summary poll -> aggregate-assert flow reads clearer inline than split across helpers"
)]
fn summarize_thread_aggregates_thread_traffic() {
    let client = http_client();
    if !http_available(&client) {
        eprintln!("SKIP: agent-bus HTTP not running at {BASE_URL}");
        return;
    }

    let suffix = unique_suffix();
    let thread_id = format!("summary-thread-{suffix}");
    let sender_one = format!("sum-agent-one-{suffix}");
    let sender_two = format!("sum-agent-two-{suffix}");

    // Post two findings from two different senders into the same thread, plus a
    // message in an unrelated thread that must NOT be aggregated.
    let seeds = [
        (
            sender_one.as_str(),
            "findings",
            format!("FINDING: issue A\nSEVERITY: HIGH {suffix}"),
            thread_id.as_str(),
        ),
        (
            sender_two.as_str(),
            "findings",
            format!("FINDING: issue B\nSEVERITY: CRITICAL {suffix}"),
            thread_id.as_str(),
        ),
        (
            sender_one.as_str(),
            "status",
            format!("unrelated {suffix}"),
            "other-thread-xyz",
        ),
    ];
    for (sender, topic, body, tid) in seeds {
        let resp = client
            .post(format!("{BASE_URL}/messages"))
            .json(&json!({
                "sender": sender,
                "recipient": "all",
                "topic": topic,
                "body": body,
                "thread_id": tid,
                "tags": ["planning"],
            }))
            .send()
            .expect("seed POST /messages failed");
        assert!(resp.status().is_success(), "seed message POST failed");
    }

    // summarize-thread reads durable history, so wait briefly for the HTTP
    // service's background PostgreSQL writer to flush the seeded messages.
    let deadline = Instant::now() + Duration::from_secs(3);
    let summary: Value = loop {
        let cli = agent_bus_binary()
            .args([
                "summarize-thread",
                "--thread-id",
                &thread_id,
                "--since-minutes",
                "10",
                "--encoding",
                "json",
            ])
            .output()
            .expect("summarize-thread CLI failed to run");
        assert!(
            cli.status.success(),
            "summarize-thread exited non-zero: {}",
            String::from_utf8_lossy(&cli.stderr)
        );
        let cli_stdout = String::from_utf8_lossy(&cli.stdout);
        let summary: Value = serde_json::from_str(cli_stdout.trim())
            .unwrap_or_else(|e| panic!("summarize-thread output not JSON ({e}):\n{cli_stdout}"));
        let agents = summary["active_agents"]
            .as_array()
            .expect("summary must list active_agents");
        let agent_names: Vec<&str> = agents.iter().filter_map(Value::as_str).collect();
        if agent_names.contains(&sender_one.as_str()) && agent_names.contains(&sender_two.as_str())
        {
            break summary;
        }
        assert!(
            Instant::now() < deadline,
            "thread summary should include both in-thread senders, got {agent_names:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };

    // The two thread senders must both appear and the message count must be at
    // least 2 (only the two in-thread messages, not the unrelated one).
    let agents = summary["active_agents"]
        .as_array()
        .expect("summary must list active_agents");
    let agent_names: Vec<&str> = agents.iter().filter_map(Value::as_str).collect();
    assert!(
        agent_names.contains(&sender_one.as_str()) && agent_names.contains(&sender_two.as_str()),
        "thread summary should include both in-thread senders, got {agent_names:?}"
    );
    let message_count = summary["message_count"].as_u64().unwrap_or(0);
    assert!(
        message_count >= 2,
        "thread summary should aggregate at least the two in-thread messages, got {message_count}"
    );
    // The unrelated thread's sender-only "status" message must not pull in its
    // distinct content: open_threads should reference only our thread_id.
    let open_threads = summary["open_threads"]
        .as_array()
        .expect("summary must list open_threads");
    assert!(
        open_threads
            .iter()
            .any(|t| t.as_str() == Some(thread_id.as_str())),
        "open_threads should include our thread_id: {summary}"
    );
    assert!(
        open_threads
            .iter()
            .all(|t| t.as_str() != Some("other-thread-xyz")),
        "open_threads must not include the unrelated thread: {summary}"
    );
}
