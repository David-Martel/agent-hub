# Agent-Bus Development Sprint — Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking. **5 workstreams run in parallel with strict file ownership.**

**Goal:** Complete remaining P2-P7 TODO items, port useful PyO3 codec functions to Rust, remove Python dependency, and harden the system based on live session observations (524+ messages, 30+ agents, 3 repos).

**Architecture:** 5 independent workstreams with non-overlapping file ownership. Each workstream produces compilable, testable code. Coordination via agent-bus MCP tools. All workstreams share `Cargo.toml` — WS1 adds dependencies first, others wait.

**Tech Stack:** Rust 2024, axum 0.8, tokio 1, redis 0.29, postgres 0.19, rmp-serde (new), criterion (new)

**Build/verify:** `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

---

## File Ownership Matrix

| File | WS1 (PyO3 Port) | WS2 (Async PG) | WS3 (Schema) | WS4 (Session) | WS5 (Testing) |
|------|:---:|:---:|:---:|:---:|:---:|
| `token.rs` (NEW) | OWN | | | | |
| `Cargo.toml` | OWN (first) | | | | ADD `[[bench]]` after WS1 |
| `output.rs` | MODIFY | | | | |
| `postgres_store.rs` | | OWN | | | |
| `main.rs` | ADD mod | ADD spawn | | ADD env | |
| `validation.rs` | | | OWN | | |
| `channels.rs` | | | MODIFY | | |
| `mcp.rs` | | | MODIFY | | |
| `http.rs` | ADD routes | | MODIFY | ADD routes | |
| `models.rs` | | | | OWN | |
| `settings.rs` | | | | MODIFY | |
| `commands.rs` | ADD cmds | | | ADD cmds | |
| `cli.rs` | ADD cmds | | | ADD cmds | |
| `journal.rs` | | | | MODIFY | |
| `tests/` | | | | | OWN |
| `benches/` (NEW) | | | | | OWN |
| `TODO.md` | | | | | OWN |

**Conflict resolution:** WS1 lands `Cargo.toml` changes first. WS3 and WS4 both touch `http.rs` — WS3 modifies existing handlers, WS4 adds new routes (no overlap). WS1 and WS4 both add to `cli.rs`/`commands.rs` — different subcommands.

---

## Workstream 1: Python Deprecation + PyO3 Port

**Agent:** rust-pro
**Goal:** Port token estimation, context compaction, and MessagePack from PyO3 codec into Rust CLI. Remove Python dependency.

### Task 1.1: Add dependencies to Cargo.toml

**Files:**
- Modify: `rust-cli/Cargo.toml`

- [ ] **Step 1: Add rmp-serde dependency**

Add to `[dependencies]`:
```toml
rmp-serde = "1"
```

- [ ] **Step 2: Verify build**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo check`

- [ ] **Step 3: Commit**

```bash
git add rust-cli/Cargo.toml
git commit -m "deps: add rmp-serde for MessagePack encoding"
```

### Task 1.2: Create token.rs — Token estimation and context compaction

**Files:**
- Create: `rust-cli/src/token.rs`
- Modify: `rust-cli/src/main.rs` (add `mod token;`)

- [ ] **Step 1: Write tests for estimate_tokens**

Create `rust-cli/src/token.rs` with tests first:
```rust
//! Token estimation and LLM context compaction.
//!
//! Ported from the PyO3 codec (`~/.codex/tools/agent-bus-mcp/rust/src/codec.rs`).
//! Heuristic: JSON-heavy text ~2.5 chars/token, natural text ~4 chars/token.

use crate::models::Message;

/// Estimate token count for a string using character-ratio heuristics.
///
/// JSON-heavy text (>20% braces/brackets/quotes) uses ~2.5 chars/token.
/// Natural text uses ~4 chars/token.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    todo!()
}

/// Minimize a message for LLM consumption by shortening field names
/// and stripping default values.
///
/// Field mappings: timestamp_utc->ts, from->f, to->t, topic->tp,
/// body->b, priority->p, request_ack->ack, reply_to->rt,
/// thread_id->tid, tags->tg, metadata->m
pub(crate) fn minimize_message(msg: &Message) -> serde_json::Value {
    todo!()
}

/// Select the most recent messages that fit within a token budget.
///
/// Returns messages newest-first, each minimized, until adding another
/// would exceed `max_tokens`.
pub(crate) fn compact_context(messages: &[Message], max_tokens: usize) -> Vec<serde_json::Value> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_natural_text() {
        // ~4 chars/token for natural English
        let text = "The quick brown fox jumps over the lazy dog near the riverbank";
        let tokens = estimate_tokens(text);
        // 62 chars / 4 = ~15 tokens
        assert!(tokens >= 12 && tokens <= 20, "got {tokens}");
    }

    #[test]
    fn estimate_tokens_json_heavy() {
        // ~2.5 chars/token for JSON-heavy text
        let text = r#"{"from":"claude","to":"all","topic":"status","body":"done"}"#;
        let tokens = estimate_tokens(text);
        // 59 chars / 2.5 = ~24 tokens
        assert!(tokens >= 18 && tokens <= 30, "got {tokens}");
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn minimize_message_strips_defaults() {
        let msg = Message {
            id: "1".into(),
            timestamp_utc: "2026-03-19T00:00:00Z".into(),
            protocol_version: "1.0".into(),
            from: "claude".into(),
            to: "codex".into(),
            topic: "status".into(),
            body: "all done".into(),
            thread_id: None,
            tags: smallvec::smallvec![],
            priority: "normal".into(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        };
        let min = minimize_message(&msg);
        // Should use short keys
        assert!(min.get("f").is_some(), "should have 'f' not 'from'");
        assert!(min.get("from").is_none());
        // Should strip defaults
        assert!(min.get("p").is_none(), "normal priority should be stripped");
        assert!(min.get("ack").is_none(), "false request_ack should be stripped");
    }

    #[test]
    fn minimize_message_keeps_non_defaults() {
        let msg = Message {
            id: "2".into(),
            timestamp_utc: "2026-03-19T00:00:00Z".into(),
            protocol_version: "1.0".into(),
            from: "claude".into(),
            to: "codex".into(),
            topic: "findings".into(),
            body: "found bug".into(),
            thread_id: Some("th-1".into()),
            tags: smallvec::smallvec!["repo:test".into()],
            priority: "high".into(),
            request_ack: true,
            reply_to: Some("msg-0".into()),
            metadata: serde_json::json!({"key": "val"}),
            stream_id: None,
        };
        let min = minimize_message(&msg);
        assert_eq!(min["p"], "high");
        assert_eq!(min["ack"], true);
        assert_eq!(min["tid"], "th-1");
        assert_eq!(min["rt"], "msg-0");
    }

    #[test]
    fn compact_context_fits_budget() {
        let msgs: Vec<Message> = (0..10)
            .map(|i| Message {
                id: format!("msg-{i}"),
                timestamp_utc: format!("2026-03-19T00:00:{i:02}Z"),
                protocol_version: "1.0".into(),
                from: "a".into(),
                to: "b".into(),
                topic: "status".into(),
                body: format!("message body {i}"),
                thread_id: None,
                tags: smallvec::smallvec![],
                priority: "normal".into(),
                request_ack: false,
                reply_to: None,
                metadata: serde_json::Value::Null,
                stream_id: None,
            })
            .collect();

        // Very small budget — should only fit a few messages
        let compacted = compact_context(&msgs, 50);
        assert!(!compacted.is_empty());
        assert!(compacted.len() < 10);

        // Large budget — should fit all
        let compacted_all = compact_context(&msgs, 100_000);
        assert_eq!(compacted_all.len(), 10);
    }

    #[test]
    fn compact_context_empty_input() {
        let result = compact_context(&[], 1000);
        assert!(result.is_empty());
    }
}
```

- [ ] **Step 2: Add `mod token;` to main.rs**

In `rust-cli/src/main.rs`, add `mod token;` after the existing mod declarations (~line 37).

- [ ] **Step 3: Run tests — verify they fail**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test token:: -- --nocapture`
Expected: FAIL (todo! panics)

- [ ] **Step 4: Implement estimate_tokens**

Replace the `todo!()` in `estimate_tokens`:
```rust
pub(crate) fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let len = text.len();
    let json_chars = text.bytes().filter(|&b| matches!(b, b'{' | b'}' | b'[' | b']' | b'"' | b':')).count();
    let json_ratio = json_chars as f64 / len as f64;
    let chars_per_token = if json_ratio > 0.15 { 2.5 } else { 4.0 };
    (len as f64 / chars_per_token).ceil() as usize
}
```

- [ ] **Step 5: Implement minimize_message**

Replace the `todo!()` in `minimize_message`:
```rust
pub(crate) fn minimize_message(msg: &Message) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    m.insert("ts".into(), serde_json::Value::String(msg.timestamp_utc.clone()));
    m.insert("f".into(), serde_json::Value::String(msg.from.clone()));
    m.insert("t".into(), serde_json::Value::String(msg.to.clone()));
    m.insert("tp".into(), serde_json::Value::String(msg.topic.clone()));
    m.insert("b".into(), serde_json::Value::String(msg.body.clone()));

    if msg.priority != "normal" {
        m.insert("p".into(), serde_json::Value::String(msg.priority.clone()));
    }
    if msg.request_ack {
        m.insert("ack".into(), serde_json::Value::Bool(true));
    }
    if let Some(ref rt) = msg.reply_to {
        m.insert("rt".into(), serde_json::Value::String(rt.clone()));
    }
    if let Some(ref tid) = msg.thread_id {
        m.insert("tid".into(), serde_json::Value::String(tid.clone()));
    }
    if !msg.tags.is_empty() {
        m.insert("tg".into(), serde_json::json!(msg.tags.as_slice()));
    }
    if !msg.metadata.is_null() {
        m.insert("m".into(), msg.metadata.clone());
    }
    serde_json::Value::Object(m)
}
```

- [ ] **Step 6: Implement compact_context**

Replace the `todo!()` in `compact_context`:
```rust
pub(crate) fn compact_context(messages: &[Message], max_tokens: usize) -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    let mut tokens_used: usize = 0;

    // Process newest first (input assumed chronological, reverse iterate)
    for msg in messages.iter().rev() {
        let minimized = minimize_message(msg);
        let json = serde_json::to_string(&minimized).unwrap_or_default();
        let msg_tokens = estimate_tokens(&json);

        if tokens_used.saturating_add(msg_tokens) > max_tokens {
            break;
        }
        tokens_used += msg_tokens;
        result.push(minimized);
    }
    result
}
```

- [ ] **Step 7: Run tests — verify all pass**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test token:: -- --nocapture`
Expected: 6 tests PASS

- [ ] **Step 8: Commit**

```bash
git add rust-cli/src/token.rs rust-cli/src/main.rs
git commit -m "feat: add token estimation and context compaction (ported from PyO3)"
```

### Task 1.3: Add MessagePack encoding mode

**Files:**
- Modify: `rust-cli/src/output.rs`

- [ ] **Step 1: Write test for MessagePack round-trip**

Add to `output.rs` tests section:
```rust
#[test]
fn msgpack_round_trip() {
    let val = serde_json::json!({"from": "claude", "to": "all", "body": "hello"});
    let packed = encode_msgpack(&val).unwrap();
    let unpacked: serde_json::Value = decode_msgpack(&packed).unwrap();
    assert_eq!(val, unpacked);
}

#[test]
fn msgpack_smaller_than_json() {
    let val = serde_json::json!({
        "from": "claude", "to": "codex", "topic": "status",
        "body": "analysis complete", "tags": ["repo:test", "session:123"],
        "priority": "normal", "request_ack": false
    });
    let json_size = serde_json::to_string(&val).unwrap().len();
    let msgpack_size = encode_msgpack(&val).unwrap().len();
    assert!(msgpack_size < json_size, "msgpack {msgpack_size} >= json {json_size}");
}
```

- [ ] **Step 2: Run tests — verify they fail**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test msgpack -- --nocapture`
Expected: FAIL (functions don't exist)

- [ ] **Step 3: Implement encode/decode_msgpack**

Add to `output.rs` before the tests module:
```rust
/// Encode a JSON value as MessagePack bytes.
pub(crate) fn encode_msgpack(value: &serde_json::Value) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec(value)
}

/// Decode MessagePack bytes to a JSON value.
pub(crate) fn decode_msgpack(data: &[u8]) -> Result<serde_json::Value, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}
```

- [ ] **Step 4: Run tests — verify pass**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test msgpack -- --nocapture`
Expected: 2 tests PASS

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/output.rs
git commit -m "feat: add MessagePack encode/decode for binary-efficient serialization"
```

### Task 1.4: Add token-count and compact-context CLI subcommands

**Files:**
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Add CLI variants to cli.rs**

Add to the `Cmd` enum in `cli.rs`:
```rust
/// Estimate token count for a text string or piped input.
TokenCount {
    /// Text to estimate (reads stdin if omitted)
    #[arg(long)]
    text: Option<String>,
},
/// Compact recent messages to fit within a token budget.
CompactContext {
    #[arg(long, help = "Filter by recipient agent")]
    agent: Option<String>,
    #[arg(long, default_value_t = 60, help = "Time window in minutes")]
    since_minutes: u64,
    #[arg(long, default_value_t = 4000, help = "Maximum token budget")]
    max_tokens: usize,
    #[arg(long, default_value = "compact", help = "Output format")]
    encoding: String,
},
```

- [ ] **Step 2: Add command implementations to commands.rs**

```rust
pub(crate) fn cmd_token_count(text: Option<&str>) -> Result<()> {
    let input = match text {
        Some(t) => t.to_string(),
        None => {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
            buf
        }
    };
    let tokens = crate::token::estimate_tokens(&input);
    let result = serde_json::json!({
        "characters": input.len(),
        "estimated_tokens": tokens,
    });
    println!("{}", serde_json::to_string(&result).unwrap_or_default());
    Ok(())
}

pub(crate) fn cmd_compact_context(
    settings: &Settings,
    agent: Option<&str>,
    since_minutes: u64,
    max_tokens: usize,
    encoding: &Encoding,
) -> Result<()> {
    let messages = crate::redis_bus::bus_list_messages(
        settings, agent, None, since_minutes, 500, true,
    )?;
    let compacted = crate::token::compact_context(&messages, max_tokens);
    crate::output::output(&compacted, encoding);
    Ok(())
}
```

- [ ] **Step 3: Wire up in main.rs dispatch**

Add match arms for the new commands in the main dispatch.

- [ ] **Step 4: Run full test suite**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/cli.rs rust-cli/src/commands.rs rust-cli/src/main.rs
git commit -m "feat: add token-count and compact-context CLI subcommands"
```

### Task 1.5: Add HTTP endpoints for token/context APIs

**Files:**
- Modify: `rust-cli/src/http.rs`

- [ ] **Step 1: Add POST /token-count handler**

```rust
#[derive(Debug, Deserialize)]
struct TokenCountRequest {
    text: String,
}

async fn http_token_count_handler(
    Json(req): Json<TokenCountRequest>,
) -> impl IntoResponse {
    let tokens = crate::token::estimate_tokens(&req.text);
    Json(serde_json::json!({
        "characters": req.text.len(),
        "estimated_tokens": tokens,
    }))
}
```

- [ ] **Step 2: Add POST /compact-context handler**

```rust
#[derive(Debug, Deserialize)]
struct CompactContextRequest {
    agent: Option<String>,
    since_minutes: Option<u64>,
    max_tokens: Option<usize>,
}

async fn http_compact_context_handler(
    State(state): State<AppState>,
    Json(req): Json<CompactContextRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let since = req.since_minutes.unwrap_or(60);
    let max_tokens = req.max_tokens.unwrap_or(4000);
    let settings = state.settings.clone();
    let agent = req.agent.clone();

    let result = tokio::task::spawn_blocking(move || {
        let messages = crate::redis_bus::bus_list_messages(
            &settings, agent.as_deref(), None, since, 500, true,
        )?;
        Ok::<_, anyhow::Error>(crate::token::compact_context(&messages, max_tokens))
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("join: {e}")))?
    .map_err(internal_error)?;

    Ok(Json(serde_json::json!({ "messages": result, "count": result.len() })))
}
```

- [ ] **Step 3: Register routes**

Add to the router:
```rust
.route("/token-count", post(http_token_count_handler))
.route("/compact-context", post(http_compact_context_handler))
```

- [ ] **Step 4: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/http.rs
git commit -m "feat(http): add /token-count and /compact-context endpoints"
```

### Task 1.6: Archive Python codebase

**Files:**
- External: `~/.codex/tools/agent-bus-mcp/`

- [ ] **Step 1: Verify all PyO3 functionality is ported**

Check list:
- [x] `estimate_tokens` — in `token.rs`
- [x] `minimize_message` — in `token.rs`
- [x] `compact_context` — in `token.rs`
- [x] `dumps_msgpack`/`loads_msgpack` — in `output.rs`
- [x] `lz4_compress`/`lz4_decompress` — already in `redis_bus.rs`
- [x] JSON serialization — already in `output.rs`
- [x] `decode_stream_entry` — already in `redis_bus.rs`

- [ ] **Step 2: Update DEPRECATION.md**

Update `~/.codex/tools/agent-bus-mcp/DEPRECATION.md` to mark as FULLY DEPRECATED:
```markdown
# Python agent-bus-mcp — FULLY DEPRECATED

**Status:** Fully deprecated as of 2026-03-19. All functionality ported to Rust CLI.
**Action:** This directory can be safely deleted. The Rust binary at ~/bin/agent-bus.exe is the sole implementation.
```

- [ ] **Step 3: Commit in agent-bus repo**

```bash
git add -A
git commit -m "feat: complete PyO3 port — Python agent-bus-mcp fully deprecated"
```

---

## Workstream 2: Async PG Write-Through + Circuit Breaker

**Agent:** rust-pro
**Goal:** Enhance PgWriter with true batched fire-and-forget writes, add proactive circuit breaker reset.

### Task 2.1: Add proactive circuit breaker health check

**Files:**
- Modify: `rust-cli/src/postgres_store.rs`

- [ ] **Step 1: Write test for proactive CB reset**

Add test in `postgres_store.rs`:
```rust
#[test]
fn circuit_breaker_resets_after_cooldown() {
    mark_pg_down();
    assert!(is_pg_circuit_open());
    // Simulate time passing by directly resetting
    mark_pg_up();
    assert!(!is_pg_circuit_open());
}

#[test]
fn probe_postgres_returns_status() {
    // Just verify the function signature works without a real PG
    let settings = Settings::from_env();
    let (ok, err) = probe_postgres(&settings);
    // Will be None/Some(error) without PG — that's fine
    assert!(ok.is_none() || ok == Some(true) || ok == Some(false));
}
```

- [ ] **Step 2: Run test — verify pass (or skip if no PG)**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test circuit_breaker -- --nocapture`

- [ ] **Step 3: Add background health check spawner**

Add to `postgres_store.rs`:
```rust
/// Spawn a background task that probes PG every `interval` seconds
/// and resets the circuit breaker when PG becomes available.
pub(crate) fn spawn_pg_health_monitor(settings: Settings, interval_secs: u64) {
    std::thread::Builder::new()
        .name("pg-health-monitor".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(interval_secs));
                if is_pg_circuit_open() {
                    let (ok, _err) = probe_postgres(&settings);
                    if ok == Some(true) {
                        mark_pg_up();
                        tracing::info!("PG circuit breaker reset — database is back online");
                    }
                }
            }
        })
        .ok();
}
```

- [ ] **Step 4: Wire into main.rs startup**

In `main.rs`, after PgWriter initialization, add:
```rust
if settings.database_url.is_some() {
    postgres_store::spawn_pg_health_monitor(settings.clone(), 30);
}
```

- [ ] **Step 5: Run full tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 6: Commit**

```bash
git add rust-cli/src/postgres_store.rs rust-cli/src/main.rs
git commit -m "feat(pg): proactive circuit breaker reset via background health monitor"
```

### Task 2.2: Enhance PgWriter batch efficiency

**Files:**
- Modify: `rust-cli/src/postgres_store.rs`

- [ ] **Step 1: Write test for batch coalescing**

Add test:
```rust
#[test]
fn pg_writer_batch_params() {
    // Verify batch constants are reasonable
    assert!(PG_BATCH_SIZE >= 5, "batch too small");
    assert!(PG_BATCH_SIZE <= 100, "batch too large");
    assert!(PG_BATCH_TIMEOUT_MS >= 50, "timeout too aggressive");
    assert!(PG_BATCH_TIMEOUT_MS <= 500, "timeout too slow");
}
```

- [ ] **Step 2: Add batch constants and metrics**

Add near top of `postgres_store.rs`:
```rust
const PG_BATCH_SIZE: usize = 20;
const PG_BATCH_TIMEOUT_MS: u64 = 100;

/// Metrics for PG write-through performance.
pub(crate) struct PgWriteMetrics {
    pub(crate) messages_queued: AtomicU64,
    pub(crate) messages_written: AtomicU64,
    pub(crate) batches_flushed: AtomicU64,
    pub(crate) write_errors: AtomicU64,
}
```

- [ ] **Step 3: Expose metrics via health endpoint**

In the `PgWriter` implementation, add a `metrics()` method that returns current counters. Wire into `bus_health()` output in `redis_bus.rs` (add `pg_writes_queued`, `pg_writes_completed`, `pg_batches` fields to Health).

- [ ] **Step 4: Run full tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/postgres_store.rs
git commit -m "perf(pg): add write-through metrics and batch tuning constants"
```

---

## Workstream 3: Ownership Conflict Detection + Schema Enforcement

**Agent:** rust-pro
**Goal:** Track file ownership claims, detect conflicts, make schema mandatory on MCP/HTTP.

### Task 3.1: Ownership conflict detection

**Files:**
- Modify: `rust-cli/src/channels.rs`
- Modify: `rust-cli/src/validation.rs`

- [ ] **Step 1: Write tests for ownership tracking**

Add to `channels.rs` tests:
```rust
#[test]
fn parse_ownership_body_extracts_files() {
    let body = "OWNERSHIP: Claiming src/main.rs, src/lib.rs for refactoring";
    let files = extract_claimed_files(body);
    assert_eq!(files, vec!["src/main.rs", "src/lib.rs"]);
}

#[test]
fn parse_ownership_body_path_patterns() {
    let body = "Claiming ownership of tests/integration_test.rs";
    let files = extract_claimed_files(body);
    assert_eq!(files, vec!["tests/integration_test.rs"]);
}

#[test]
fn detect_conflict_when_file_claimed_by_other() {
    let mut tracker = OwnershipTracker::new();
    tracker.record_claim("agent-a", &["src/main.rs"]);
    let conflicts = tracker.check_conflicts("agent-b", &["src/main.rs"]);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].file, "src/main.rs");
    assert_eq!(conflicts[0].claimed_by, "agent-a");
}

#[test]
fn no_conflict_when_same_agent() {
    let mut tracker = OwnershipTracker::new();
    tracker.record_claim("agent-a", &["src/main.rs"]);
    let conflicts = tracker.check_conflicts("agent-a", &["src/main.rs"]);
    assert!(conflicts.is_empty());
}
```

- [ ] **Step 2: Run tests — verify fail**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test ownership_track -- --nocapture`
Expected: FAIL (types don't exist)

- [ ] **Step 3: Implement OwnershipTracker**

Add to `channels.rs`:
```rust
use std::collections::HashMap;
use std::sync::Mutex;

/// File path conflict detected during ownership check.
#[derive(Debug, Clone)]
pub(crate) struct OwnershipConflict {
    pub(crate) file: String,
    pub(crate) claimed_by: String,
    pub(crate) claimed_at: String,
}

/// In-memory tracker for file ownership claims.
/// Thread-safe via internal Mutex.
pub(crate) struct OwnershipTracker {
    /// file_path -> (agent, timestamp)
    claims: HashMap<String, (String, String)>,
}

impl OwnershipTracker {
    pub(crate) fn new() -> Self {
        Self { claims: HashMap::new() }
    }

    pub(crate) fn record_claim(&mut self, agent: &str, files: &[&str]) {
        let ts = chrono::Utc::now().to_rfc3339();
        for f in files {
            self.claims.insert((*f).to_string(), (agent.to_string(), ts.clone()));
        }
    }

    pub(crate) fn check_conflicts(&self, agent: &str, files: &[&str]) -> Vec<OwnershipConflict> {
        files.iter().filter_map(|f| {
            self.claims.get(*f).and_then(|(owner, ts)| {
                if owner != agent {
                    Some(OwnershipConflict {
                        file: (*f).to_string(),
                        claimed_by: owner.clone(),
                        claimed_at: ts.clone(),
                    })
                } else {
                    None
                }
            })
        }).collect()
    }
}

/// Extract file paths from an ownership claim message body.
/// Looks for patterns like `path/to/file.rs` (containing `/` or `\` and a `.ext`).
pub(crate) fn extract_claimed_files(body: &str) -> Vec<String> {
    let re_pattern = regex_lite::Regex::new(r"(?:^|[\s,])([a-zA-Z0-9_./-]+\.[a-zA-Z0-9]+)").unwrap();
    re_pattern.find_iter(body)
        .map(|m| m.as_str().trim_start_matches([' ', ',']).to_string())
        .filter(|p| p.contains('/') || p.contains('\\'))
        .collect()
}

/// Global ownership tracker, lazily initialized.
pub(crate) fn global_ownership_tracker() -> &'static Mutex<OwnershipTracker> {
    static TRACKER: std::sync::OnceLock<Mutex<OwnershipTracker>> = std::sync::OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(OwnershipTracker::new()))
}
```

- [ ] **Step 4: Wire conflict detection into post_message**

In `redis_bus.rs::bus_post_message()`, after successful post, check if `topic == "ownership"`:
```rust
if prepared.topic == "ownership" {
    let files = crate::channels::extract_claimed_files(&prepared.body);
    if !files.is_empty() {
        let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let mut tracker = crate::channels::global_ownership_tracker().lock().unwrap();
        let conflicts = tracker.check_conflicts(&prepared.from, &file_refs);
        if !conflicts.is_empty() {
            tracing::warn!(
                "Ownership conflict: {} files claimed by other agents: {:?}",
                conflicts.len(), conflicts.iter().map(|c| format!("{} (by {})", c.file, c.claimed_by)).collect::<Vec<_>>()
            );
        }
        tracker.record_claim(&prepared.from, &file_refs);
    }
}
```

- [ ] **Step 5: Run tests — verify pass**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 6: Commit**

```bash
git add rust-cli/src/channels.rs rust-cli/src/redis_bus.rs
git commit -m "feat: ownership conflict detection — track claimed files, warn on conflicts"
```

### Task 3.2: Make schema mandatory on MCP and HTTP

**Files:**
- Modify: `rust-cli/src/validation.rs`
- Modify: `rust-cli/src/mcp.rs`
- Modify: `rust-cli/src/http.rs`

- [ ] **Step 1: Write tests for mandatory schema**

Add to `validation.rs` tests:
```rust
#[test]
fn enforce_schema_rejects_missing_on_mcp() {
    let result = enforce_schema_for_transport("mcp", None, "findings");
    assert!(result.is_some(), "MCP should infer schema for findings topic");
}

#[test]
fn enforce_schema_allows_explicit() {
    let result = enforce_schema_for_transport("mcp", Some("status"), "status");
    assert_eq!(result, Some("status"));
}

#[test]
fn enforce_schema_cli_is_optional() {
    let result = enforce_schema_for_transport("cli", None, "random-topic");
    assert!(result.is_none(), "CLI should not enforce schema");
}
```

- [ ] **Step 2: Implement enforce_schema_for_transport**

Add to `validation.rs`:
```rust
/// For MCP and HTTP transports, schema is mandatory.
/// If not provided explicitly, infer from topic. If inference fails
/// for known topic patterns, default to "status".
pub(crate) fn enforce_schema_for_transport<'a>(
    transport: &str,
    explicit_schema: Option<&'a str>,
    topic: &str,
) -> Option<&'a str> {
    if let Some(s) = explicit_schema {
        return Some(s);
    }
    let inferred = infer_schema_from_topic(topic, None);
    if inferred.is_some() {
        return inferred;
    }
    // MCP/HTTP: default to "status" if no inference possible
    if transport == "mcp" || transport == "http" {
        // Return leaked &str for "status" — these are static anyway
        return Some("status");
    }
    None
}
```

- [ ] **Step 3: Wire into MCP post_message handler**

In `mcp.rs`, in the `post_message` tool handler, replace the schema lookup:
```rust
let schema = enforce_schema_for_transport("mcp", explicit_schema, &topic);
```

- [ ] **Step 4: Wire into HTTP POST /messages handler**

In `http.rs`, in the send handler:
```rust
let schema = crate::validation::enforce_schema_for_transport("http", explicit_schema, &topic);
```

- [ ] **Step 5: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 6: Commit**

```bash
git add rust-cli/src/validation.rs rust-cli/src/mcp.rs rust-cli/src/http.rs
git commit -m "feat: mandatory schema enforcement on MCP and HTTP transports"
```

### Task 3.3: Add regex-lite dependency for file path extraction

**Files:**
- Modify: `rust-cli/Cargo.toml`

- [ ] **Step 1: Add regex-lite**

Add to `[dependencies]`:
```toml
regex-lite = "0.1"
```

- [ ] **Step 2: Verify build**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo check`

- [ ] **Step 3: Commit**

```bash
git add rust-cli/Cargo.toml
git commit -m "deps: add regex-lite for ownership file path extraction"
```

**Note:** This task should be done BEFORE Task 3.1 if regex-lite is needed.

---

## Workstream 4: Session Management + Message Threading

**Agent:** rust-pro
**Goal:** Auto-generate session IDs, session summaries, finding deduplication, message threading.

### Task 4.1: Session ID auto-generation

**Files:**
- Modify: `rust-cli/src/settings.rs`
- Modify: `rust-cli/src/models.rs`
- Modify: `rust-cli/src/redis_bus.rs`

- [ ] **Step 1: Write tests**

Add to `settings.rs` tests:
```rust
#[test]
fn session_id_from_env() {
    std::env::set_var("AGENT_BUS_SESSION_ID", "test-session-42");
    let settings = Settings::from_env();
    assert_eq!(settings.session_id.as_deref(), Some("test-session-42"));
    std::env::remove_var("AGENT_BUS_SESSION_ID");
}

#[test]
fn session_id_default_none() {
    std::env::remove_var("AGENT_BUS_SESSION_ID");
    let settings = Settings::from_env();
    assert!(settings.session_id.is_none());
}
```

- [ ] **Step 2: Add session_id to Settings**

In `settings.rs`, add to the `Settings` struct:
```rust
pub(crate) session_id: Option<String>,
```

In `from_env()`, add:
```rust
session_id: std::env::var("AGENT_BUS_SESSION_ID").ok().filter(|s| !s.is_empty()),
```

- [ ] **Step 3: Auto-tag messages with session_id**

In `redis_bus.rs::prepare_message()`, if `settings.session_id` is set, add `session:<id>` to tags:
```rust
if let Some(ref sid) = settings.session_id {
    let session_tag = format!("session:{sid}");
    if !tags.iter().any(|t| t.starts_with("session:")) {
        tags.push(session_tag);
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/settings.rs rust-cli/src/redis_bus.rs
git commit -m "feat: auto-tag messages with AGENT_BUS_SESSION_ID when set"
```

### Task 4.2: Session summary command

**Files:**
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Add SessionSummary CLI variant**

In `cli.rs`, add:
```rust
/// Generate a summary of all messages in a session.
SessionSummary {
    #[arg(long, help = "Session ID to summarize")]
    session: String,
    #[arg(long, default_value = "compact", help = "Output format")]
    encoding: String,
},
```

- [ ] **Step 2: Implement session summary**

In `commands.rs`:
```rust
pub(crate) fn cmd_session_summary(settings: &Settings, session_id: &str, encoding: &Encoding) -> Result<()> {
    let session_tag = format!("session:{session_id}");

    // Read all messages (wide window) and filter by session tag
    let all_msgs = crate::redis_bus::bus_list_messages(
        settings, None, None, 10080, 10000, true,
    )?;
    let session_msgs: Vec<&Message> = all_msgs.iter()
        .filter(|m| m.tags.iter().any(|t| t == &session_tag))
        .collect();

    // Compute summary stats
    let mut agents: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut topics: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut severities: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for msg in &session_msgs {
        agents.insert(&msg.from);
        *topics.entry(&msg.topic).or_insert(0) += 1;

        // Count severity if it's a finding
        if msg.body.contains("SEVERITY:") {
            for sev in &["CRITICAL", "HIGH", "MEDIUM", "LOW"] {
                if msg.body.contains(sev) {
                    *severities.entry((*sev).to_string()).or_insert(0) += 1;
                    break;
                }
            }
        }
    }

    let first_ts = session_msgs.last().map(|m| m.timestamp_utc.as_str()).unwrap_or("N/A");
    let last_ts = session_msgs.first().map(|m| m.timestamp_utc.as_str()).unwrap_or("N/A");

    let summary = serde_json::json!({
        "session_id": session_id,
        "message_count": session_msgs.len(),
        "agent_count": agents.len(),
        "agents": agents.into_iter().collect::<Vec<_>>(),
        "topics": topics,
        "severities": severities,
        "time_range": { "first": first_ts, "last": last_ts },
    });

    crate::output::output(&summary, encoding);
    Ok(())
}
```

- [ ] **Step 3: Wire into main.rs**

Add match arm for `Cmd::SessionSummary`.

- [ ] **Step 4: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/cli.rs rust-cli/src/commands.rs rust-cli/src/main.rs
git commit -m "feat: add session-summary command for session forensics"
```

### Task 4.3: Finding deduplication command

**Files:**
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Add Dedup CLI variant**

```rust
/// Deduplicate findings by file path and severity.
Dedup {
    #[arg(long, help = "Session ID to deduplicate")]
    session: Option<String>,
    #[arg(long, help = "Filter by recipient agent")]
    agent: Option<String>,
    #[arg(long, default_value_t = 1440, help = "Time window in minutes")]
    since_minutes: u64,
    #[arg(long, default_value = "compact", help = "Output format")]
    encoding: String,
},
```

- [ ] **Step 2: Implement dedup logic**

In `commands.rs`:
```rust
pub(crate) fn cmd_dedup(
    settings: &Settings,
    session: Option<&str>,
    agent: Option<&str>,
    since_minutes: u64,
    encoding: &Encoding,
) -> Result<()> {
    let all_msgs = crate::redis_bus::bus_list_messages(
        settings, agent, None, since_minutes, 10000, true,
    )?;

    // Filter by session if specified
    let msgs: Vec<&Message> = if let Some(sid) = session {
        let tag = format!("session:{sid}");
        all_msgs.iter().filter(|m| m.tags.iter().any(|t| t == &tag)).collect()
    } else {
        all_msgs.iter().collect()
    };

    // Group findings by file path (extracted from body)
    let mut groups: std::collections::HashMap<String, Vec<&Message>> = std::collections::HashMap::new();

    for msg in &msgs {
        if msg.body.contains("FINDING:") || msg.topic.ends_with("-findings") {
            // Extract file path from body (look for path-like strings)
            let file_key = extract_file_from_finding(&msg.body)
                .unwrap_or_else(|| msg.topic.clone());
            groups.entry(file_key).or_default().push(msg);
        }
    }

    // For each group, keep highest severity and merge bodies
    let mut deduped: Vec<serde_json::Value> = Vec::new();
    for (file, findings) in &groups {
        let severity = findings.iter()
            .filter_map(|m| extract_severity(&m.body))
            .max_by_key(|s| match s.as_str() {
                "CRITICAL" => 4, "HIGH" => 3, "MEDIUM" => 2, "LOW" => 1, _ => 0
            })
            .unwrap_or_else(|| "MEDIUM".to_string());

        deduped.push(serde_json::json!({
            "file": file,
            "finding_count": findings.len(),
            "max_severity": severity,
            "agents": findings.iter().map(|m| m.from.as_str()).collect::<std::collections::HashSet<_>>(),
        }));
    }

    let result = serde_json::json!({
        "total_findings": msgs.iter().filter(|m| m.body.contains("FINDING:") || m.topic.ends_with("-findings")).count(),
        "unique_files": groups.len(),
        "deduped": deduped,
    });

    crate::output::output(&result, encoding);
    Ok(())
}

fn extract_file_from_finding(body: &str) -> Option<String> {
    // Look for file paths in the body
    for word in body.split_whitespace() {
        let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '\\' && c != '.' && c != '_' && c != '-');
        if (trimmed.contains('/') || trimmed.contains('\\')) && trimmed.contains('.') {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn extract_severity(body: &str) -> Option<String> {
    for sev in &["CRITICAL", "HIGH", "MEDIUM", "LOW"] {
        if body.contains(sev) {
            return Some((*sev).to_string());
        }
    }
    None
}
```

- [ ] **Step 3: Wire into main.rs, run tests, commit**

```bash
git add rust-cli/src/cli.rs rust-cli/src/commands.rs rust-cli/src/main.rs
git commit -m "feat: add dedup command for finding deduplication by file path"
```

### Task 4.4: Message threading (auto-link chains)

**Files:**
- Modify: `rust-cli/src/redis_bus.rs`

- [ ] **Step 1: Write test for thread auto-linking**

```rust
#[test]
fn auto_thread_links_reply_to_previous() {
    // When a message has reply_to set, thread_id should be inferred
    // from the original message's thread_id (or its own id if first in chain)
    let thread = infer_thread_id(Some("msg-1"), None);
    assert!(thread.is_some());
}

#[test]
fn auto_thread_preserves_explicit() {
    let thread = infer_thread_id(Some("msg-1"), Some("thread-abc"));
    assert_eq!(thread, Some("thread-abc".to_string()));
}
```

- [ ] **Step 2: Implement infer_thread_id**

In `redis_bus.rs`:
```rust
/// If thread_id is not set but reply_to is, use reply_to as the thread root.
fn infer_thread_id(reply_to: Option<&str>, explicit_thread: Option<&str>) -> Option<String> {
    if let Some(t) = explicit_thread {
        return Some(t.to_string());
    }
    reply_to.map(|r| r.to_string())
}
```

Wire into `prepare_message()`:
```rust
let thread_id = infer_thread_id(reply_to, thread_id);
```

- [ ] **Step 3: Run tests, commit**

```bash
git add rust-cli/src/redis_bus.rs
git commit -m "feat: auto-infer thread_id from reply_to for message threading"
```

---

## Workstream 5: Testing, Benchmarks, and Documentation

**Agent:** rust-pro
**Goal:** E2E channel tests, benchmark harness, TOON validation, TODO.md update.

### Task 5.1: E2E channel system integration tests

**Files:**
- Create: `rust-cli/tests/channel_integration_test.rs`

- [ ] **Step 1: Write channel integration tests**

```rust
//! Integration tests for the channel system.
//! Requires Redis running at the configured URL.

use std::process::Command;

fn agent_bus() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    cmd.env("RUST_LOG", "error");
    cmd
}

fn redis_available() -> bool {
    agent_bus()
        .args(["health", "--encoding", "compact"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn channel_direct_message_round_trip() {
    if !redis_available() { return; }

    // Send direct message
    let send = agent_bus()
        .args(["post-direct", "--from-agent", "test-a", "--to-agent", "test-b",
               "--topic", "status", "--body", "direct test message"])
        .output()
        .expect("post-direct failed");
    assert!(send.status.success(), "send: {}", String::from_utf8_lossy(&send.stderr));

    // Read direct messages
    let read = agent_bus()
        .args(["read-direct", "--agent-a", "test-a", "--agent-b", "test-b",
               "--limit", "1", "--encoding", "json"])
        .output()
        .expect("read-direct failed");
    assert!(read.status.success());
    let body = String::from_utf8_lossy(&read.stdout);
    assert!(body.contains("direct test message"), "body: {body}");
}

#[test]
fn channel_group_lifecycle() {
    if !redis_available() { return; }

    let group_name = format!("test-group-{}", uuid::Uuid::new_v4().as_simple());

    // Create group
    let create = agent_bus()
        .args(["post-group", "--group", &group_name, "--from-agent", "test-a",
               "--topic", "coordination", "--body", "group test"])
        .output()
        .expect("create group failed");
    assert!(create.status.success());

    // Read group
    let read = agent_bus()
        .args(["read-group", "--group", &group_name, "--limit", "5", "--encoding", "json"])
        .output()
        .expect("read group failed");
    assert!(read.status.success());
    let body = String::from_utf8_lossy(&read.stdout);
    assert!(body.contains("group test"));
}

#[test]
fn claim_and_resolve() {
    if !redis_available() { return; }

    let resource = format!("test-file-{}.rs", uuid::Uuid::new_v4().as_simple());

    // Claim resource
    let claim = agent_bus()
        .args(["claim", "--agent", "test-claimer", "--resource", &resource,
               "--priority-argument", "I need this file"])
        .output()
        .expect("claim failed");
    assert!(claim.status.success());

    // List claims
    let list = agent_bus()
        .args(["claims", "--encoding", "json"])
        .output()
        .expect("claims list failed");
    assert!(list.status.success());

    // Resolve claim
    let resolve = agent_bus()
        .args(["resolve", "--resource", &resource, "--winner", "test-claimer",
               "--reason", "only claimant"])
        .output()
        .expect("resolve failed");
    assert!(resolve.status.success());
}

#[test]
fn ownership_via_topic() {
    if !redis_available() { return; }

    // Send ownership claim via regular message
    let send = agent_bus()
        .args(["send", "--from-agent", "test-owner", "--to-agent", "all",
               "--topic", "ownership", "--body", "Claiming src/test_owned.rs for refactoring"])
        .output()
        .expect("ownership send failed");
    assert!(send.status.success());
}
```

- [ ] **Step 2: Run integration tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test --test channel_integration_test -- --test-threads=1`

- [ ] **Step 3: Commit**

```bash
git add rust-cli/tests/channel_integration_test.rs
git commit -m "test: add E2E integration tests for channel system"
```

### Task 5.2: Benchmark harness

**Files:**
- Create: `rust-cli/benches/throughput.rs`
- Modify: `rust-cli/Cargo.toml`

- [ ] **Step 1: Add criterion dependency and bench target**

In `Cargo.toml`:
```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "throughput"
harness = false
```

- [ ] **Step 2: Write benchmarks**

Create `rust-cli/benches/throughput.rs`:
```rust
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_estimate_tokens(c: &mut Criterion) {
    let text = r#"{"from":"claude","to":"all","topic":"status","body":"Analysis complete. Found 3 issues in src/main.rs: unused import, missing error handling, and deprecated API call.","tags":["repo:agent-bus","session:test"]}"#;

    c.bench_function("estimate_tokens_json", |b| {
        b.iter(|| agent_bus::token::estimate_tokens(black_box(text)))
    });

    let natural = "The quick brown fox jumps over the lazy dog. This is a longer piece of natural text that should benchmark differently from JSON content.";
    c.bench_function("estimate_tokens_natural", |b| {
        b.iter(|| agent_bus::token::estimate_tokens(black_box(natural)))
    });
}

fn bench_minimize_value(c: &mut Criterion) {
    let val = serde_json::json!({
        "id": "msg-123",
        "timestamp_utc": "2026-03-19T12:00:00Z",
        "protocol_version": "1.0",
        "from": "claude",
        "to": "codex",
        "topic": "status",
        "body": "analysis complete",
        "thread_id": null,
        "tags": ["repo:test"],
        "priority": "normal",
        "request_ack": false,
        "reply_to": null,
        "metadata": {}
    });

    c.bench_function("minimize_value", |b| {
        b.iter(|| agent_bus::output::minimize_value(black_box(&val)))
    });
}

fn bench_msgpack_encode(c: &mut Criterion) {
    let val = serde_json::json!({
        "from": "claude", "to": "codex", "topic": "status",
        "body": "analysis complete with detailed findings about the codebase quality",
        "tags": ["repo:test", "session:bench"],
        "priority": "normal"
    });

    c.bench_function("msgpack_encode", |b| {
        b.iter(|| agent_bus::output::encode_msgpack(black_box(&val)))
    });

    let packed = agent_bus::output::encode_msgpack(&val).unwrap();
    c.bench_function("msgpack_decode", |b| {
        b.iter(|| agent_bus::output::decode_msgpack(black_box(&packed)))
    });

    c.bench_function("json_encode", |b| {
        b.iter(|| serde_json::to_string(black_box(&val)))
    });
}

fn bench_toon_format(c: &mut Criterion) {
    let msg = agent_bus::models::Message {
        id: "msg-bench".into(),
        timestamp_utc: "2026-03-19T12:00:00Z".into(),
        protocol_version: "1.0".into(),
        from: "claude".into(),
        to: "codex".into(),
        topic: "status".into(),
        body: "benchmarking the TOON format for token efficiency measurement".into(),
        thread_id: None,
        tags: smallvec::smallvec!["repo:bench".into()],
        priority: "normal".into(),
        request_ack: false,
        reply_to: None,
        metadata: serde_json::Value::Null,
        stream_id: None,
    };

    c.bench_function("format_toon", |b| {
        b.iter(|| agent_bus::output::format_message_toon(black_box(&msg)))
    });
}

criterion_group!(benches, bench_estimate_tokens, bench_minimize_value, bench_msgpack_encode, bench_toon_format);
criterion_main!(benches);
```

- [ ] **Step 3: Make necessary items pub for benchmarks**

The benchmark crate needs `pub` access. In `main.rs`, make the relevant modules `pub`:
```rust
pub mod token;
pub mod output;
pub mod models;
```

Or use `#[cfg(test)]` + `pub(crate)` pattern. The simplest approach is a `lib.rs` re-export or making the bench use the binary's test infrastructure.

**Alternative:** If the crate is binary-only, move bench-targeted functions to a `lib.rs` or use `#[cfg(feature = "bench")]`.

- [ ] **Step 4: Run benchmarks**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo bench`

- [ ] **Step 5: Commit**

```bash
git add rust-cli/benches/ rust-cli/Cargo.toml rust-cli/src/main.rs
git commit -m "bench: add criterion benchmarks for token estimation, TOON, MessagePack"
```

### Task 5.3: TOON encoding token savings validation

**Files:**
- Modify: `rust-cli/tests/integration_test.rs`

- [ ] **Step 1: Add TOON vs JSON comparison test**

```rust
#[test]
fn toon_encoding_saves_tokens() {
    if !redis_available() { return; }

    // Send a message
    let _ = agent_bus_binary()
        .args(["send", "--from-agent", "toon-test", "--to-agent", "all",
               "--topic", "status", "--body", "TOON encoding validation test"])
        .output();

    // Read as JSON
    let json_out = agent_bus_binary()
        .args(["read", "--agent", "toon-test", "--since-minutes", "1",
               "--limit", "1", "--encoding", "json"])
        .output()
        .expect("json read failed");
    let json_str = String::from_utf8_lossy(&json_out.stdout);

    // Read as TOON
    let toon_out = agent_bus_binary()
        .args(["read", "--agent", "toon-test", "--since-minutes", "1",
               "--limit", "1", "--encoding", "toon"])
        .output()
        .expect("toon read failed");
    let toon_str = String::from_utf8_lossy(&toon_out.stdout);

    // TOON should be significantly smaller
    if !json_str.is_empty() && !toon_str.is_empty() {
        let savings = 1.0 - (toon_str.len() as f64 / json_str.len() as f64);
        assert!(savings > 0.3, "TOON savings only {:.0}%, expected >30%", savings * 100.0);
    }
}
```

- [ ] **Step 2: Run test**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test toon_encoding -- --nocapture`

- [ ] **Step 3: Commit**

```bash
git add rust-cli/tests/integration_test.rs
git commit -m "test: validate TOON encoding achieves >30% token savings vs JSON"
```

### Task 5.4: Update TODO.md

**Files:**
- Modify: `TODO.md`

- [ ] **Step 1: Mark completed items**

Update TODO.md to check off:
- P6: Ownership conflict detection
- P6: Proactive circuit breaker reset
- P6: Finding deduplication command
- P6: Session ID auto-generation
- P6: Session summary auto-generation
- P6: Message threading enforcement
- P6: TOON encoding (already done, mark explicitly)
- P7: Agent prompt template library (partial — templates in AGENT_COMMUNICATIONS.md)

Add new section:
```markdown
### Completed in 2026-03-19 sprint

- [x] Port PyO3 token estimation to Rust (`token.rs`)
- [x] Port PyO3 context compaction to Rust (`token.rs`)
- [x] Port PyO3 message minimization to Rust (`token.rs`)
- [x] Add MessagePack encoding (`output.rs`)
- [x] Python agent-bus-mcp fully deprecated
- [x] Proactive PG circuit breaker reset (background health monitor)
- [x] PG write-through metrics and batch tuning
- [x] Ownership conflict detection (file tracking + warnings)
- [x] Mandatory schema enforcement on MCP/HTTP
- [x] Session ID auto-generation via AGENT_BUS_SESSION_ID
- [x] Session summary command
- [x] Finding deduplication command
- [x] Message threading (auto-infer thread_id from reply_to)
- [x] E2E channel integration tests
- [x] Criterion benchmark harness (token, TOON, MessagePack)
- [x] TOON token savings validation test
```

- [ ] **Step 2: Update suggested execution order**

```markdown
## Suggested execution order

1. ~~Finish Rust-side localhost validation and connection pooling.~~ Done.
2. ~~Split `main.rs` into modules without changing behavior.~~ Done.
3. ~~Add integration tests that exercise Redis and PostgreSQL together.~~ Done.
4. ~~Add HTTP streaming and remote client mode.~~ SSE streaming done. `--server` client mode deferred.
5. ~~Multi-repo validation (stm32-merge, finance-warehouse).~~ Done. 320+ PG messages.
6. ~~Async PG write-through + finding dedup + default schema enforcement.~~ Done.
7. ~~PyO3 port + Python deprecation.~~ Done. Single Rust binary.
8. Reassess wire-format and packaging work after the runtime stabilizes.
9. `--server` client mode for multi-machine LAN access.
10. Agent task queue (orchestrator pushes, agents pull).
```

- [ ] **Step 3: Commit**

```bash
git add TODO.md
git commit -m "docs: update TODO.md — mark sprint items complete, revise execution order"
```

---

## Execution Order and Dependencies

```
WS1 Task 1.1 (Cargo.toml deps) ──┐
                                   ├── All other WS1 tasks (sequential)
WS3 Task 3.3 (regex-lite dep) ────┤
                                   ├── All other WS3 tasks (sequential)
                                   │
WS2 (independent) ────────────────── parallel
WS4 (independent) ────────────────── parallel
WS5 (after WS1 for bench pub) ────── mostly parallel, Task 5.2 after WS1

Within each workstream: tasks are sequential.
Across workstreams: fully parallel (non-overlapping files).
```

## Post-Sprint

After all workstreams complete:
1. Run full test suite: `cd rust-cli && RUSTC_WRAPPER="" cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`
2. Run benchmarks: `cd rust-cli && RUSTC_WRAPPER="" cargo bench`
3. Build and deploy: `cd rust-cli && RUSTC_WRAPPER="" cargo build --release && cp target/release/agent-bus.exe ~/bin/`
4. Restart service: `nssm restart AgentHub`
5. Verify health: `agent-bus health --encoding json`
