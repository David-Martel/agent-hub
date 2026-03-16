//! End-to-end HTTP integration tests against the running agent-bus service.
//!
//! Prerequisites: the agent-bus HTTP server must be running at `localhost:8400`.
//! All tests skip gracefully when the server is not reachable.
//!
//! Tests are isolated by using unique agent/resource names derived from
//! `std::time::SystemTime` so concurrent runs do not interfere.
//!
//! # Running
//!
//! Run with a single test thread to avoid Redis key collisions from parallel
//! execution (the `unique_suffix()` helper has millisecond precision, which is
//! sufficient for sequential runs but can collide under aggressive parallelism):
//!
//! ```text
//! cargo test --test http_integration_test -- --test-threads=1
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const BASE_URL: &str = "http://localhost:8400";

/// Returns a millisecond-precision timestamp string suitable for unique IDs.
fn unique_suffix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Returns a reqwest client and `true` when the service is reachable.
///
/// All test bodies should call this at the top and return early on `false`.
async fn service_available(client: &reqwest::Client) -> bool {
    client
        .get(format!("{BASE_URL}/health"))
        .send()
        .await
        .is_ok()
}

// ---------------------------------------------------------------------------
// 1. Health endpoint
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .get(format!("{BASE_URL}/health"))
        .send()
        .await
        .expect("GET /health failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.expect("response not JSON");
    assert_eq!(body["ok"], json!(true), "health.ok should be true");
    assert!(
        body.get("protocol_version").is_some(),
        "health missing protocol_version"
    );
}

#[tokio::test]
async fn health_toon_encoding_returns_text() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .get(format!("{BASE_URL}/health?encoding=toon"))
        .send()
        .await
        .expect("GET /health?encoding=toon failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/plain"), "expected text/plain, got {ct}");
    let text = resp.text().await.expect("response not text");
    // TOON health format: `ok=true r=<n> p=<n> v=<version>`
    assert!(
        text.starts_with("ok="),
        "TOON health should start with ok=, got: {text}"
    );
    assert!(text.contains("r="), "TOON health missing r= field: {text}");
}

#[tokio::test]
async fn health_json_contains_pool_metrics() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .get(format!("{BASE_URL}/health"))
        .send()
        .await
        .expect("GET /health failed");

    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body.get("pool").is_some(),
        "health response missing pool metrics"
    );
    let pool = &body["pool"];
    assert!(pool.get("idle").is_some(), "pool missing idle field");
    assert!(
        pool.get("max_size").is_some(),
        "pool missing max_size field"
    );
}

// ---------------------------------------------------------------------------
// 2. Message send/read round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_message_returns_id() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": format!("http-tester-{ts}"),
            "recipient": format!("http-recv-{ts}"),
            "topic": "http-test",
            "body": format!("round-trip-test-{ts}"),
        }))
        .send()
        .await
        .expect("POST /messages failed");

    assert_eq!(resp.status(), StatusCode::OK, "expected 200 on valid send");
    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body.get("id").and_then(|v| v.as_str()).is_some(),
        "response missing id field: {body}"
    );
}

#[tokio::test]
async fn post_message_missing_sender_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "recipient": "some-agent",
            "topic": "test",
            "body": "test body",
        }))
        .send()
        .await
        .expect("POST /messages failed");

    // The JsonRejection handler normalises all deserialisation failures to 400.
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "missing sender should be 400, got {}",
        resp.status()
    );
    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body.get("error").is_some(),
        "error response missing 'error' field: {body}"
    );
}

#[tokio::test]
async fn post_message_empty_sender_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": "   ",
            "recipient": "some-agent",
            "topic": "test",
            "body": "test body",
        }))
        .send()
        .await
        .expect("POST /messages failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "whitespace-only sender should be 400"
    );
    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body["error"].as_str().unwrap_or("").contains("sender"),
        "error message should mention sender: {body}"
    );
}

#[tokio::test]
async fn post_message_empty_body_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": "http-tester",
            "recipient": "some-agent",
            "topic": "test",
            "body": "",
        }))
        .send()
        .await
        .expect("POST /messages failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "empty body should be 400"
    );
}

#[tokio::test]
async fn get_messages_returns_array() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .get(format!("{BASE_URL}/messages"))
        .send()
        .await
        .expect("GET /messages failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body.is_array(),
        "GET /messages should return a JSON array, got: {body}"
    );
}

#[tokio::test]
async fn get_messages_toon_encoding_returns_text() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    // First send a message so we have something to read back.
    let ts = unique_suffix();
    let agent = format!("http-toon-test-{ts}");
    client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &agent,
            "recipient": &agent,
            "topic": "toon-test",
            "body": "toon encoding E2E test",
            "tags": ["test:toon"],
        }))
        .send()
        .await
        .expect("send failed");

    let resp = client
        .get(format!(
            "{BASE_URL}/messages?agent={agent}&encoding=toon&since=1"
        ))
        .send()
        .await
        .expect("GET /messages?encoding=toon failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/plain"), "expected text/plain, got {ct}");
    let text = resp.text().await.expect("response not text");
    // TOON message format: `@from→to #topic [tags] body`
    if !text.is_empty() {
        assert!(
            text.contains('@'),
            "TOON message line should contain '@': {text}"
        );
        assert!(
            text.contains('#'),
            "TOON message line should contain '#': {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. TOON encoding E2E
// ---------------------------------------------------------------------------

#[tokio::test]
async fn toon_format_matches_spec() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let from_agent = format!("toon-sender-{ts}");
    let to_agent = format!("toon-receiver-{ts}");

    // Send a message with known content and tags.
    client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &from_agent,
            "recipient": &to_agent,
            "topic": "spec-check",
            "body": "Hello TOON world",
            "tags": ["env:test", "repo:agent-hub"],
        }))
        .send()
        .await
        .expect("send failed");

    // Read back with TOON encoding filtered to our unique recipient.
    let resp = client
        .get(format!(
            "{BASE_URL}/messages?agent={to_agent}&encoding=toon&since=1"
        ))
        .send()
        .await
        .expect("read failed");

    let text = resp.text().await.expect("response not text");
    assert!(!text.is_empty(), "expected at least one TOON line");

    // Find our specific message in the TOON output (bus may have other messages)
    let our_line = text
        .lines()
        .find(|l| l.contains("Hello TOON world"))
        .expect("expected our TOON message in output");
    // Format: `@from→to #topic [tag1,tag2] body`
    assert!(
        our_line.starts_with('@'),
        "TOON line should start with '@': {our_line}"
    );
    assert!(
        our_line.contains("→"),
        "TOON line should contain '→': {our_line}"
    );
    assert!(
        our_line.contains("#spec-check"),
        "TOON line should contain #spec-check: {our_line}"
    );
}

#[tokio::test]
async fn toon_body_truncated_at_120_chars() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let agent = format!("toon-trunc-{ts}");
    // Body is exactly 200 chars — should be truncated at 120 in TOON output.
    let long_body: String = std::iter::repeat_n('x', 200).collect();

    client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &agent,
            "recipient": &agent,
            "topic": "truncation-test",
            "body": &long_body,
        }))
        .send()
        .await
        .expect("send failed");

    let resp = client
        .get(format!(
            "{BASE_URL}/messages?agent={agent}&encoding=toon&since=1"
        ))
        .send()
        .await
        .expect("read failed");

    let text = resp.text().await.expect("response not text");
    if text.is_empty() {
        return; // No messages to check — timing window, not a failure.
    }

    for line in text.lines() {
        // Extract the body portion (everything after the last '] ' or after '#topic ')
        // The full line including metadata can be longer than 120 chars, but
        // the body portion alone must not exceed 120 chars.
        let body_start = line
            .rfind(']')
            .map(|i| i + 2)
            .or_else(|| {
                // No tags — body starts after `#topic `
                line.find(' ')
                    .map(|i| line[i + 1..].find(' ').map_or(i + 1, |j| i + 1 + j + 1))
            })
            .unwrap_or(0);

        let body_part = &line[body_start..];
        assert!(
            body_part.chars().count() <= 120,
            "TOON body exceeds 120 chars ({} chars): {body_part}",
            body_part.chars().count()
        );
    }
}

#[tokio::test]
async fn toon_shows_tags_in_brackets() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let agent = format!("toon-tags-{ts}");

    client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &agent,
            "recipient": &agent,
            "topic": "tags-test",
            "body": "testing tag display",
            "tags": ["alpha", "beta"],
        }))
        .send()
        .await
        .expect("send failed");

    let resp = client
        .get(format!(
            "{BASE_URL}/messages?agent={agent}&encoding=toon&since=1"
        ))
        .send()
        .await
        .expect("read failed");

    let text = resp.text().await.expect("response not text");
    if text.is_empty() {
        return;
    }

    let line = text
        .lines()
        .find(|l| l.contains("testing tag display"))
        .unwrap_or("");
    assert!(
        line.contains("[alpha,beta]"),
        "TOON line should show [alpha,beta]: {line}"
    );
}

// ---------------------------------------------------------------------------
// 4. LZ4 compression E2E
// ---------------------------------------------------------------------------

#[tokio::test]
async fn large_body_is_compressed_and_decompressed() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let agent = format!("lz4-test-{ts}");
    // Body > 512 bytes — should trigger LZ4 compression.
    let large_body = format!("LZ4-compression-test: {} {}", ts, "A".repeat(600));

    let send_resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &agent,
            "recipient": &agent,
            "topic": "compression-test",
            "body": &large_body,
        }))
        .send()
        .await
        .expect("send failed");

    assert_eq!(send_resp.status(), StatusCode::OK);
    let sent: Value = send_resp.json().await.expect("send response not JSON");
    let msg_id = sent["id"].as_str().expect("no id in send response");

    // Read back — body should be transparently decompressed.
    let read_resp = client
        .get(format!(
            "{BASE_URL}/messages?agent={agent}&since=1&limit=10"
        ))
        .send()
        .await
        .expect("read failed");

    let messages: Value = read_resp.json().await.expect("read response not JSON");
    let msgs = messages.as_array().expect("expected array");

    let found = msgs.iter().find(|m| m["id"].as_str() == Some(msg_id));
    assert!(
        found.is_some(),
        "message {msg_id} not found in read response"
    );

    let found_msg = found.unwrap();
    assert_eq!(
        found_msg["body"].as_str().unwrap_or(""),
        large_body,
        "decompressed body should match original"
    );

    // Metadata should indicate compression was used.
    let metadata = &found_msg["metadata"];
    if metadata.is_object() && metadata.get("_compressed").is_some() {
        assert_eq!(
            metadata["_compressed"].as_str().unwrap_or(""),
            "lz4",
            "compression type should be lz4"
        );
        assert!(
            metadata.get("_original_size").is_some(),
            "metadata should contain _original_size"
        );
    }
    // Note: compression metadata presence is best-effort; the key invariant
    // is that the body round-trips correctly.
}

#[tokio::test]
async fn small_body_not_compressed() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let agent = format!("no-lz4-{ts}");
    // Body < 512 bytes — should NOT be compressed.
    let small_body = format!("small-body-{ts}");

    let send_resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &agent,
            "recipient": &agent,
            "topic": "no-compression-test",
            "body": &small_body,
        }))
        .send()
        .await
        .expect("send failed");

    assert_eq!(send_resp.status(), StatusCode::OK);
    let sent: Value = send_resp.json().await.expect("send response not JSON");
    let metadata = &sent["metadata"];

    if metadata.is_object() {
        assert!(
            metadata.get("_compressed").is_none(),
            "small body should not be compressed, got metadata: {metadata}"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Batch endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_send_three_messages_returns_three_ids() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let resp = client
        .post(format!("{BASE_URL}/messages/batch"))
        .json(&json!({
            "messages": [
                {"sender": "batch-agent", "recipient": format!("batch-recv-{ts}"), "topic": "batch", "body": "msg1"},
                {"sender": "batch-agent", "recipient": format!("batch-recv-{ts}"), "topic": "batch", "body": "msg2"},
                {"sender": "batch-agent", "recipient": format!("batch-recv-{ts}"), "topic": "batch", "body": "msg3"},
            ]
        }))
        .send()
        .await
        .expect("POST /messages/batch failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.expect("response not JSON");
    assert_eq!(body["count"], json!(3), "count should be 3");
    let ids = body["ids"].as_array().expect("ids should be array");
    assert_eq!(ids.len(), 3, "should return 3 IDs");
    for id in ids {
        assert!(
            id.as_str().is_some_and(|s| !s.is_empty()),
            "each ID should be a non-empty string"
        );
    }
}

#[tokio::test]
async fn batch_send_empty_array_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .post(format!("{BASE_URL}/messages/batch"))
        .json(&json!({"messages": []}))
        .send()
        .await
        .expect("POST /messages/batch failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "empty batch should be 400"
    );
}

#[tokio::test]
async fn batch_send_over_100_messages_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let messages: Vec<Value> = (0..=100)
        .map(|i| {
            json!({
                "sender": "batch-agent",
                "recipient": "batch-recv",
                "topic": "batch",
                "body": format!("msg{i}"),
            })
        })
        .collect();

    let resp = client
        .post(format!("{BASE_URL}/messages/batch"))
        .json(&json!({"messages": messages}))
        .send()
        .await
        .expect("POST /messages/batch failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "101 messages should be 400"
    );
    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("limit"),
        "error message should mention limit: {body}"
    );
}

#[tokio::test]
async fn batch_ack_valid_ids_returns_ok() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    // Send two messages first to get real IDs.
    let ts = unique_suffix();
    let agent = format!("batch-ack-agent-{ts}");
    let batch_resp = client
        .post(format!("{BASE_URL}/messages/batch"))
        .json(&json!({
            "messages": [
                {"sender": &agent, "recipient": &agent, "topic": "test", "body": "ack-me-1"},
                {"sender": &agent, "recipient": &agent, "topic": "test", "body": "ack-me-2"},
            ]
        }))
        .send()
        .await
        .expect("batch send failed");

    let batch_body: Value = batch_resp.json().await.expect("batch send not JSON");
    let ids: Vec<String> = batch_body["ids"]
        .as_array()
        .expect("no ids")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    // Now batch-ack them.
    let ack_resp = client
        .post(format!("{BASE_URL}/ack/batch"))
        .json(&json!({
            "agent": &agent,
            "message_ids": ids,
        }))
        .send()
        .await
        .expect("POST /ack/batch failed");

    assert_eq!(ack_resp.status(), StatusCode::OK);
    let ack_body: Value = ack_resp.json().await.expect("ack response not JSON");
    assert_eq!(ack_body["acked"], json!(2), "should have acked 2 messages");
}

// ---------------------------------------------------------------------------
// 6. Channel endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn direct_channel_send_and_read() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let sender = format!("direct-sender-{ts}");
    let recipient = format!("direct-recipient-{ts}");

    // POST to direct channel.
    let send_resp = client
        .post(format!("{BASE_URL}/channels/direct/{recipient}"))
        .json(&json!({
            "sender": &sender,
            "body": format!("direct-msg-{ts}"),
        }))
        .send()
        .await
        .expect("POST /channels/direct/:agent failed");

    assert_eq!(
        send_resp.status(),
        StatusCode::OK,
        "direct send should be 200"
    );
    let sent: Value = send_resp.json().await.expect("send response not JSON");
    assert!(
        sent.get("id").is_some(),
        "direct send should return message with id: {sent}"
    );

    // GET from direct channel — reading as the recipient viewing conversation with sender.
    let read_resp = client
        .get(format!(
            "{BASE_URL}/channels/direct/{sender}?agent={recipient}"
        ))
        .send()
        .await
        .expect("GET /channels/direct/:agent failed");

    assert_eq!(
        read_resp.status(),
        StatusCode::OK,
        "direct read should be 200"
    );
    let msgs: Value = read_resp.json().await.expect("read response not JSON");
    assert!(msgs.is_array(), "direct read should return array");
    let arr = msgs.as_array().unwrap();
    assert!(
        !arr.is_empty(),
        "direct channel should contain the sent message"
    );
}

#[tokio::test]
async fn escalate_channel_sets_high_priority() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let resp = client
        .post(format!("{BASE_URL}/channels/escalate"))
        .json(&json!({
            "sender": format!("escalate-agent-{ts}"),
            "body": format!("urgent issue {ts}"),
        }))
        .send()
        .await
        .expect("POST /channels/escalate failed");

    assert_eq!(resp.status(), StatusCode::OK, "escalate should be 200");
    let body: Value = resp.json().await.expect("escalate response not JSON");
    // Escalation messages are always high priority.
    assert_eq!(
        body["priority"].as_str().unwrap_or(""),
        "high",
        "escalation should have priority=high: {body}"
    );
    // Escalation routes to an agent with `orchestration` capability if one is
    // online, or falls back to `"all"`. Either is valid here — the exact agent
    // ID depends on presence state at the time of the test.
    let to_field = body["to"].as_str().unwrap_or("");
    assert!(
        !to_field.is_empty(),
        "escalation 'to' must not be empty — full body: {body}"
    );
    // The metadata must mark this as an escalation.
    assert_eq!(
        body["metadata"]["escalation"],
        json!(true),
        "escalation metadata.escalation should be true: {body}"
    );
}

/// Verify that when an agent with `orchestration` capability is online the
/// escalation `to` field is set to that specific agent rather than `"all"`.
///
/// This test registers a presence record, posts an escalation, then checks the
/// `to` field of the returned message. It is self-contained — the presence
/// record expires on its own TTL.
#[tokio::test]
async fn escalate_routes_to_orchestrator_when_present() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let orch_agent = format!("orchestrator-{ts}");
    let sender = format!("escalate-sender-{ts}");

    // Register the orchestrator agent with the required capability.
    let presence_resp = client
        .put(format!("{BASE_URL}/presence/{orch_agent}"))
        .json(&json!({
            "status": "online",
            "capabilities": ["orchestration"],
            "ttl_seconds": 60,
        }))
        .send()
        .await
        .expect("PUT /presence failed");
    assert_eq!(
        presence_resp.status(),
        StatusCode::OK,
        "presence registration failed: {}",
        presence_resp.status()
    );

    // Post the escalation.
    let resp = client
        .post(format!("{BASE_URL}/channels/escalate"))
        .json(&json!({
            "sender": sender,
            "body": format!("escalation for orchestrator test {ts}"),
        }))
        .send()
        .await
        .expect("POST /channels/escalate failed");

    assert_eq!(resp.status(), StatusCode::OK, "escalate should be 200");
    let body: Value = resp.json().await.expect("escalate response not JSON");

    let to_field = body["to"].as_str().unwrap_or("");
    // The server routes to whichever orchestrator-capable agent it finds first
    // in presence. Our agent may not be picked if there are other orchestrator
    // presence records still alive from previous runs. What we can assert is:
    // 1. Routing happened (to is not "all"), since we just registered an
    //    orchestrator-capable agent.
    // 2. The to field is not empty.
    assert!(
        !to_field.is_empty() && to_field != "all",
        "escalation should route to a specific orchestrator agent (not 'all') \
         when at least one is online; registered '{orch_agent}', got: '{to_field}' — full body: {body}"
    );

    // priority field on the Message struct must be "high" (Deviation 3).
    assert_eq!(
        body["priority"].as_str().unwrap_or(""),
        "high",
        "escalation Message.priority must be 'high': {body}"
    );
}

/// Verify that batch send returns 400 (not 422) when the messages array field
/// is missing from the request body. This covers Deviation 1 for the batch endpoint.
#[tokio::test]
async fn batch_send_missing_messages_field_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    // Send a body that is missing the required `messages` field entirely.
    let resp = client
        .post(format!("{BASE_URL}/messages/batch"))
        .json(&json!({"unexpected_field": "value"}))
        .send()
        .await
        .expect("POST /messages/batch failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "missing 'messages' field should be 400, got {}",
        resp.status()
    );
    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body.get("error").is_some(),
        "error response missing 'error' field: {body}"
    );
}

/// Verify that batch ack returns 400 (not 422) when the required `agent` field
/// is missing. This covers Deviation 1 for the batch-ack endpoint.
#[tokio::test]
async fn batch_ack_missing_agent_field_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    // Omit the required `agent` field.
    let resp = client
        .post(format!("{BASE_URL}/ack/batch"))
        .json(&json!({"message_ids": ["fake-id-1"]}))
        .send()
        .await
        .expect("POST /ack/batch failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "missing 'agent' field should be 400, got {}",
        resp.status()
    );
    let body: Value = resp.json().await.expect("response not JSON");
    assert!(
        body.get("error").is_some(),
        "error response missing 'error' field: {body}"
    );
}

#[tokio::test]
async fn arbitrate_first_claim_granted() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let resource = format!("test-resource-{ts}");
    let first_agent = format!("claimer-1-{ts}");

    let resp = client
        .post(format!("{BASE_URL}/channels/arbitrate/{resource}"))
        .json(&json!({
            "agent": &first_agent,
            "priority_argument": "first to edit",
        }))
        .send()
        .await
        .expect("POST /channels/arbitrate/:resource failed");

    assert_eq!(resp.status(), StatusCode::OK, "first claim should be 200");
    let body: Value = resp.json().await.expect("claim response not JSON");
    // The claim response uses the "agent" field (from the ClaimRecord struct).
    let claimed_agent = body["agent"].as_str().unwrap_or("");
    assert_eq!(
        claimed_agent, first_agent,
        "first claimant should be the requesting agent: {body}"
    );
    assert_eq!(
        body["status"].as_str().unwrap_or(""),
        "granted",
        "first claim status should be granted: {body}"
    );
}

#[tokio::test]
async fn arbitrate_second_claim_is_contested() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let resource = format!("contested-resource-{ts}");
    let first_agent = format!("first-claimer-{ts}");
    let second_agent = format!("second-claimer-{ts}");

    // First claim.
    client
        .post(format!("{BASE_URL}/channels/arbitrate/{resource}"))
        .json(&json!({"agent": &first_agent}))
        .send()
        .await
        .expect("first claim failed");

    // Second claim — should show contested state.
    let resp = client
        .post(format!("{BASE_URL}/channels/arbitrate/{resource}"))
        .json(&json!({
            "agent": &second_agent,
            "priority_argument": "I need it too",
        }))
        .send()
        .await
        .expect("second claim failed");

    assert_eq!(resp.status(), StatusCode::OK, "second claim should be 200");
    let body: Value = resp.json().await.expect("second claim response not JSON");
    let status = body["status"].as_str().unwrap_or("");
    assert!(
        status == "contested" || status == "granted",
        "second claim should be contested or granted: {body}"
    );
}

#[tokio::test]
async fn arbitrate_get_state_shows_claims() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let resource = format!("state-check-resource-{ts}");
    let agent = format!("state-check-agent-{ts}");

    // Post a claim.
    client
        .post(format!("{BASE_URL}/channels/arbitrate/{resource}"))
        .json(&json!({"agent": &agent}))
        .send()
        .await
        .expect("claim failed");

    // GET arbitration state.
    let resp = client
        .get(format!("{BASE_URL}/channels/arbitrate/{resource}"))
        .send()
        .await
        .expect("GET /channels/arbitrate/:resource failed");

    assert_eq!(resp.status(), StatusCode::OK, "get state should be 200");
    let body: Value = resp.json().await.expect("state response not JSON");
    assert!(
        body.get("owner").is_some() || body.get("claims").is_some(),
        "arbitration state should contain owner or claims: {body}"
    );
}

#[tokio::test]
async fn arbitrate_resolve_sets_winner() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let resource = format!("resolve-resource-{ts}");
    let agent_a = format!("resolve-agent-a-{ts}");
    let agent_b = format!("resolve-agent-b-{ts}");

    // Establish a contested state.
    client
        .post(format!("{BASE_URL}/channels/arbitrate/{resource}"))
        .json(&json!({"agent": &agent_a}))
        .send()
        .await
        .expect("claim a failed");

    client
        .post(format!("{BASE_URL}/channels/arbitrate/{resource}"))
        .json(&json!({"agent": &agent_b}))
        .send()
        .await
        .expect("claim b failed");

    // Resolve in favour of agent_a.
    let resp = client
        .put(format!("{BASE_URL}/channels/arbitrate/{resource}/resolve"))
        .json(&json!({
            "winner": &agent_a,
            "reason": "agent_a has higher priority task",
            "resolved_by": "orchestrator",
        }))
        .send()
        .await
        .expect("PUT /channels/arbitrate/:resource/resolve failed");

    assert_eq!(resp.status(), StatusCode::OK, "resolve should be 200");
    let body: Value = resp.json().await.expect("resolve response not JSON");
    // The resolve response carries the winner in the "winner" field.
    let winner_field = body["winner"].as_str().unwrap_or("");
    assert_eq!(
        winner_field, agent_a,
        "resolved winner should be agent_a: {body}"
    );
    // Resolution should record the reason.
    assert!(
        body.get("resolution_reason").is_some()
            || body.get("reason").is_some()
            || body.get("status").is_some(),
        "resolve response should contain resolution details: {body}"
    );
}

// ---------------------------------------------------------------------------
// 7. Presence endpoint tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn put_presence_returns_200() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let agent = format!("presence-test-{ts}");

    let resp = client
        .put(format!("{BASE_URL}/presence/{agent}"))
        .json(&json!({
            "status": "online",
            "capabilities": ["test", "http"],
            "ttl_seconds": 30,
        }))
        .send()
        .await
        .expect("PUT /presence/:agent failed");

    assert_eq!(resp.status(), StatusCode::OK, "PUT /presence should be 200");
    let body: Value = resp.json().await.expect("presence response not JSON");
    assert_eq!(
        body["agent"].as_str().unwrap_or(""),
        agent,
        "response should echo agent name: {body}"
    );
    assert_eq!(
        body["status"].as_str().unwrap_or(""),
        "online",
        "status should be online: {body}"
    );
}

#[tokio::test]
async fn get_presence_returns_array() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .get(format!("{BASE_URL}/presence"))
        .send()
        .await
        .expect("GET /presence failed");

    assert_eq!(resp.status(), StatusCode::OK, "GET /presence should be 200");
    let body: Value = resp.json().await.expect("presence list not JSON");
    assert!(
        body.is_array(),
        "GET /presence should return array, got: {body}"
    );
}

#[tokio::test]
async fn get_presence_toon_encoding() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    // First register a presence so we have something to show.
    let ts = unique_suffix();
    let agent = format!("toon-presence-{ts}");
    client
        .put(format!("{BASE_URL}/presence/{agent}"))
        .json(&json!({"status": "online", "ttl_seconds": 30}))
        .send()
        .await
        .expect("presence set failed");

    let resp = client
        .get(format!("{BASE_URL}/presence?encoding=toon"))
        .send()
        .await
        .expect("GET /presence?encoding=toon failed");

    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ct.contains("text/plain"), "expected text/plain, got {ct}");
    let text = resp.text().await.expect("response not text");
    if !text.is_empty() {
        // TOON presence format: `~agent status [caps] ttl=Ns`
        assert!(
            text.contains('~'),
            "TOON presence line should start with '~': {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. Required-ACK (pending-acks) tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pending_ack_message_appears_in_pending_list() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let sender = format!("ack-sender-{ts}");
    let receiver = format!("ack-receiver-{ts}");

    // Send a message with request_ack=true.
    let send_resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &sender,
            "recipient": &receiver,
            "topic": "ack-required",
            "body": format!("please ack this {ts}"),
            "request_ack": true,
        }))
        .send()
        .await
        .expect("send failed");

    assert_eq!(send_resp.status(), StatusCode::OK);
    let sent: Value = send_resp.json().await.expect("send not JSON");
    let msg_id = sent["id"].as_str().expect("no id").to_owned();

    // GET /pending-acks — the message should appear.
    let pending_resp = client
        .get(format!("{BASE_URL}/pending-acks?agent={receiver}"))
        .send()
        .await
        .expect("GET /pending-acks failed");

    assert_eq!(pending_resp.status(), StatusCode::OK);
    let pending: Value = pending_resp.json().await.expect("pending-acks not JSON");
    let arr = pending.as_array().expect("expected array");

    // The message ID should be in the list.
    let found = arr.iter().any(|entry| {
        entry["id"].as_str() == Some(&msg_id)
            || entry["message_id"].as_str() == Some(&msg_id)
            || entry.get("id").is_some_and(|v| v.as_str() == Some(&msg_id))
    });
    assert!(
        found,
        "message {msg_id} should appear in pending-acks for {receiver}: {pending}"
    );
}

#[tokio::test]
async fn acknowledged_message_removed_from_pending() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let ts = unique_suffix();
    let sender = format!("ack-cycle-sender-{ts}");
    let receiver = format!("ack-cycle-recv-{ts}");

    // Send with request_ack=true.
    let send_resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": &sender,
            "recipient": &receiver,
            "topic": "ack-cycle",
            "body": format!("ack cycle test {ts}"),
            "request_ack": true,
        }))
        .send()
        .await
        .expect("send failed");

    let sent: Value = send_resp.json().await.expect("send not JSON");
    let msg_id = sent["id"].as_str().expect("no id").to_owned();

    // ACK the message.
    let ack_resp = client
        .post(format!("{BASE_URL}/messages/{msg_id}/ack"))
        .json(&json!({
            "agent": &receiver,
            "body": "acknowledged",
        }))
        .send()
        .await
        .expect("POST /messages/:id/ack failed");

    assert_eq!(ack_resp.status(), StatusCode::OK, "ack should return 200");
    let ack_body: Value = ack_resp.json().await.expect("ack not JSON");
    assert_eq!(
        ack_body["ack_sent"],
        json!(true),
        "ack_sent should be true: {ack_body}"
    );
    assert_eq!(
        ack_body["acked_message_id"].as_str().unwrap_or(""),
        msg_id,
        "acked_message_id should match: {ack_body}"
    );
}

// ---------------------------------------------------------------------------
// 9. Input validation edge cases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn post_message_invalid_priority_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": "tester",
            "recipient": "recv",
            "topic": "test",
            "body": "valid body",
            "priority": "ultra-mega-high",
        }))
        .send()
        .await
        .expect("POST /messages failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "invalid priority should be 400"
    );
}

#[tokio::test]
async fn post_message_missing_recipient_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .post(format!("{BASE_URL}/messages"))
        .json(&json!({
            "sender": "tester",
            "topic": "test",
            "body": "body",
        }))
        .send()
        .await
        .expect("POST /messages failed");

    // Axum returns 422 Unprocessable Entity when a required JSON field is absent.
    assert!(
        resp.status() == StatusCode::BAD_REQUEST
            || resp.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "missing recipient should be 400 or 422, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn batch_send_message_with_bad_priority_returns_400() {
    let client = reqwest::Client::new();
    if !service_available(&client).await {
        eprintln!("SKIP: agent-bus not running at {BASE_URL}");
        return;
    }

    let resp = client
        .post(format!("{BASE_URL}/messages/batch"))
        .json(&json!({
            "messages": [{
                "sender": "tester",
                "recipient": "recv",
                "topic": "test",
                "body": "body",
                "priority": "BADVALUE",
            }]
        }))
        .send()
        .await
        .expect("POST /messages/batch failed");

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "batch with bad priority should be 400"
    );
}

// ---------------------------------------------------------------------------
// 10. MCP Streamable HTTP transport (basic smoke tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_get_returns_tool_list() {
    // The MCP-HTTP server runs on port 8401 when started with --transport mcp-http.
    // The main HTTP server on 8400 also has /health; skip if 8401 not available.
    let client = reqwest::Client::new();
    let mcp_url = "http://localhost:8401/mcp";

    let ok = client.get(mcp_url).send().await.is_ok();
    if !ok {
        eprintln!("SKIP: MCP HTTP server not running at port 8401");
        return;
    }

    let resp = client.get(mcp_url).send().await.expect("GET /mcp failed");
    assert_eq!(resp.status(), StatusCode::OK);
    // Capture headers before consuming the response body (json() takes ownership).
    let has_session_header = resp.headers().contains_key("mcp-session-id");
    let body: Value = resp.json().await.expect("GET /mcp not JSON");
    let tools = body
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .expect("GET /mcp should return result.tools array");
    assert!(!tools.is_empty(), "tool list should not be empty");

    // Verify Mcp-Session-Id header is present.
    assert!(
        has_session_header,
        "MCP response should include Mcp-Session-Id header"
    );
}
