//! Golden JSON fixtures for the Cloudflare Worker contract test
//! (`cloud/agentbus/test/contract.test.ts`, agent-hub#79).
//!
//! Each fixture is produced by serializing a real Rust wire type
//! (`serde_json::to_value`) so the Worker can be asserted byte-for-byte
//! (field-for-field) compatible with what the on-site hub actually emits —
//! including which optional fields are *omitted* (`skip_serializing_if`)
//! versus emitted as `null` (plain `Option<T>`).
//!
//! Regenerate with `REGEN_FIXTURES=1 cargo test -p agent-bus-core --test cloud_fixtures`.
//! This test performs zero I/O against Redis/Postgres — it only exercises
//! `serde` — so it is safe to run anywhere, including CI, per agent-hub#77's
//! test-isolation requirement (never point tests at the live default backend
//! endpoints).

use agent_bus_core::channels::{
    ArbitrationState, ClaimStatus, OwnershipClaim, RerouteSuggestion, ResourceLeaseMode,
    ResourceScope,
};
use agent_bus_core::models::{Message, Presence};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../cloud/agentbus/test/fixtures")
}

/// Compare `value` against the committed fixture at `name`.json, or (re)write
/// it when `REGEN_FIXTURES=1` is set in the environment.
fn assert_fixture(name: &str, value: &serde_json::Value) {
    let path = fixtures_dir().join(format!("{name}.json"));
    let pretty = serde_json::to_string_pretty(value).expect("serialize fixture") + "\n";

    if std::env::var("REGEN_FIXTURES").as_deref() == Ok("1") {
        std::fs::write(&path, &pretty)
            .unwrap_or_else(|e| panic!("failed to write fixture {}: {e}", path.display()));
        return;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "failed to read fixture {} ({e}); run with REGEN_FIXTURES=1 to create it",
            path.display()
        )
    });
    let existing_value: serde_json::Value = serde_json::from_str(&existing)
        .unwrap_or_else(|e| panic!("fixture {name}.json is not valid JSON: {e}"));
    assert_eq!(
        value, &existing_value,
        "fixture {name}.json is stale — re-run with REGEN_FIXTURES=1 and review the diff"
    );
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

#[test]
fn message_full_fixture() {
    let msg = Message {
        id: "018f4c2e-6b7a-7c3d-8e2f-1a2b3c4d5e6f".to_owned(),
        timestamp_utc: "2026-09-27T12:00:00.000000Z".to_owned(),
        protocol_version: "1.0".to_owned(),
        from: "claude".to_owned(),
        to: "codex".to_owned(),
        topic: "status".to_owned(),
        body: "STATUS: cloud tier fixture".to_owned(),
        thread_id: Some("thread-79".to_owned()),
        tags: smallvec::smallvec!["repo:agent-hub".to_owned(), "priority:high".to_owned()],
        priority: "high".to_owned(),
        request_ack: true,
        reply_to: Some("018f4c2e-0000-7000-8000-000000000000".to_owned()),
        metadata: serde_json::json!({"origin": "fixture", "n": 1}),
        stream_id: Some("1690000000000-0".to_owned()),
    };
    assert_fixture("message_full", &serde_json::to_value(&msg).unwrap());
}

#[test]
fn message_minimal_fixture() {
    // Mirrors what a bare-minimum JSON payload deserializes to: `tags`
    // defaults to an empty SmallVec (still serialized as `[]`, never
    // omitted), `metadata` defaults to `Value::Null` (also never omitted —
    // there is no skip_serializing_if on it), while `thread_id`, `reply_to`
    // and `stream_id` are genuinely absent from the output.
    let json = serde_json::json!({
        "id": "018f4c2e-6b7a-7c3d-8e2f-000000000001",
        "timestamp_utc": "2026-09-27T12:00:00.000000Z",
        "protocol_version": "1.0",
        "from": "claude",
        "to": "all",
        "topic": "knock",
        "body": "check the bus"
    });
    let msg: Message = serde_json::from_value(json).expect("minimal message deserializes");
    assert_fixture("message_minimal", &serde_json::to_value(&msg).unwrap());
}

// ---------------------------------------------------------------------------
// Presence
// ---------------------------------------------------------------------------

#[test]
fn presence_full_fixture() {
    let presence = Presence {
        agent: "claude".to_owned(),
        status: "online".to_owned(),
        protocol_version: "1.0".to_owned(),
        timestamp_utc: "2026-09-27T12:00:00.000000Z".to_owned(),
        session_id: "session-abc123".to_owned(),
        capabilities: vec!["mcp".to_owned(), "http".to_owned()],
        metadata: serde_json::json!({"host": "asuspro13", "repo": "agent-hub"}),
        ttl_seconds: 300,
    };
    assert_fixture("presence_full", &serde_json::to_value(&presence).unwrap());
}

// ---------------------------------------------------------------------------
// OwnershipClaim / ArbitrationState
// ---------------------------------------------------------------------------

#[test]
fn claim_granted_minimal_fixture() {
    let claim = OwnershipClaim {
        resource: "src/http.rs".to_owned(),
        agent: "claude".to_owned(),
        priority_argument: "first-edit required".to_owned(),
        timestamp: "2026-09-27T12:00:00.000000Z".to_owned(),
        status: ClaimStatus::Granted,
        mode: ResourceLeaseMode::Exclusive,
        namespace: None,
        scope_kind: None,
        scope_path: None,
        repo_scopes: vec![],
        thread_id: None,
        lease_ttl_seconds: 3600,
        expires_at: Some("2026-09-27T13:00:00.000000Z".to_owned()),
        scope: ResourceScope::Repo,
        reroute_suggestion: None,
    };
    assert_fixture(
        "claim_granted_minimal",
        &serde_json::to_value(&claim).unwrap(),
    );
}

#[test]
fn claim_contested_full_fixture() {
    let claim = OwnershipClaim {
        resource: "cargo-target".to_owned(),
        agent: "codex".to_owned(),
        priority_argument: "running clippy".to_owned(),
        timestamp: "2026-09-27T12:00:01.000000Z".to_owned(),
        status: ClaimStatus::Contested,
        mode: ResourceLeaseMode::Exclusive,
        namespace: Some("ns-1".to_owned()),
        scope_kind: Some("path".to_owned()),
        scope_path: Some("/home/damartel/dev/repos/agent-hub".to_owned()),
        repo_scopes: vec!["agent-hub".to_owned()],
        thread_id: Some("thread-79".to_owned()),
        lease_ttl_seconds: 1800,
        expires_at: Some("2026-09-27T12:30:01.000000Z".to_owned()),
        scope: ResourceScope::Machine,
        reroute_suggestion: Some(RerouteSuggestion {
            original_resource: "cargo-target".to_owned(),
            suggested_resource: "cargo-target:codex".to_owned(),
            isolation_hint: "use --target-dir T:\\RustCache\\cargo-target-codex".to_owned(),
            reason: "resource contested by claude".to_owned(),
        }),
    };
    assert_fixture(
        "claim_contested_full",
        &serde_json::to_value(&claim).unwrap(),
    );
}

#[test]
fn arbitration_state_unresolved_fixture() {
    let state = ArbitrationState {
        resource: "cargo-target".to_owned(),
        claims: vec![],
        winner: None,
        resolution_reason: None,
    };
    assert_fixture(
        "arbitration_state_unresolved",
        &serde_json::to_value(&state).unwrap(),
    );
}

#[test]
fn arbitration_state_resolved_fixture() {
    let state = ArbitrationState {
        resource: "cargo-target".to_owned(),
        claims: vec![],
        winner: Some("claude".to_owned()),
        resolution_reason: Some("first-come, higher priority argument".to_owned()),
    };
    assert_fixture(
        "arbitration_state_resolved",
        &serde_json::to_value(&state).unwrap(),
    );
}
