# P1 Runtime Hardening + P2 Performance Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden the agent-bus runtime with input validation, retry logic, and enriched health data; then improve performance with connection pooling and reuse.

**Architecture:** P1 adds validation at the Settings boundary (fail-fast on bad config), retry/backoff in postgres_store, and richer health telemetry. P2 replaces one-connection-per-call patterns with shared Redis/PG clients. Changes are additive — existing behavior preserved for valid inputs.

**Tech Stack:** Rust 2024, redis 0.29, postgres 0.19, anyhow, tokio, axum 0.8

**Build/verify command:** `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

---

## File Map

| File | Changes |
|------|---------|
| `rust-cli/src/settings.rs` | Add `validate()` method enforcing localhost, stream/table name checks |
| `rust-cli/src/models.rs` | Add optional `stream_length` and `pg_message_count` fields to `Health` |
| `rust-cli/src/redis_bus.rs` | Enrich `bus_health()` with stream XLEN; add connection reuse in HTTP mode |
| `rust-cli/src/postgres_store.rs` | Add retry/backoff on persist failures; add `count_messages_postgres()`; connection reuse |
| `rust-cli/src/main.rs` | Call `settings.validate()?` after `from_env()` |
| `rust-cli/src/validation.rs` | Add `validate_identifier()` for stream/table names |
| `rust-cli/src/http.rs` | Pass shared connection state instead of reconnecting per request |
| `rust-cli/Cargo.toml` | No new dependencies needed (tokio retry is hand-rolled, 3 attempts) |

---

## Chunk 1: P1 Runtime Hardening

### Task 1: Enforce localhost in Settings

**Files:**
- Modify: `rust-cli/src/settings.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Add localhost validation tests to settings.rs**

Add to the existing `#[cfg(test)] mod tests` in `settings.rs`:

```rust
#[test]
fn validate_rejects_non_localhost_redis() {
    let mut s = Settings::from_env();
    s.redis_url = "redis://remote-host:6380/0".to_owned();
    assert!(s.validate().is_err());
}

#[test]
fn validate_rejects_non_localhost_database() {
    let mut s = Settings::from_env();
    s.database_url = Some("postgresql://postgres@remote:5432/db".to_owned());
    assert!(s.validate().is_err());
}

#[test]
fn validate_rejects_non_localhost_server_host() {
    let mut s = Settings::from_env();
    s.server_host = "0.0.0.0".to_owned();
    assert!(s.validate().is_err());
}

#[test]
fn validate_accepts_localhost_variants() {
    let mut s = Settings::from_env();
    s.redis_url = "redis://localhost:6380/0".to_owned();
    s.database_url = Some("postgresql://postgres@localhost:5432/db".to_owned());
    s.server_host = "localhost".to_owned();
    assert!(s.validate().is_ok());
}

#[test]
fn validate_accepts_127_0_0_1() {
    let mut s = Settings::from_env();
    s.redis_url = "redis://127.0.0.1:6380/0".to_owned();
    s.server_host = "127.0.0.1".to_owned();
    assert!(s.validate().is_ok());
}

#[test]
fn validate_accepts_none_database() {
    let mut s = Settings::from_env();
    s.database_url = None;
    assert!(s.validate().is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test settings::tests -- --nocapture 2>&1`
Expected: compilation error — `validate` method not found

- [ ] **Step 3: Implement Settings::validate()**

Add to `settings.rs`:

```rust
use anyhow::{Result, anyhow, bail};

impl Settings {
    /// Validate that all URLs point to localhost — this bus is local-only.
    pub(crate) fn validate(&self) -> Result<()> {
        validate_localhost_url(&self.redis_url, "AGENT_BUS_REDIS_URL")?;
        if let Some(ref db_url) = self.database_url {
            validate_localhost_url(db_url, "AGENT_BUS_DATABASE_URL")?;
        }
        if !is_localhost(&self.server_host) {
            bail!(
                "AGENT_BUS_SERVER_HOST must be localhost or 127.0.0.1, got '{}'",
                self.server_host
            );
        }
        Ok(())
    }
}

fn is_localhost(host: &str) -> bool {
    let h = host.trim().to_lowercase();
    h == "localhost" || h == "127.0.0.1" || h == "::1"
}

fn validate_localhost_url(url: &str, env_var: &str) -> Result<()> {
    // Extract host from URL: scheme://[user@]host[:port][/path]
    let Some(scheme_end) = url.find("://") else {
        return Ok(()); // not a URL, skip
    };
    let after_scheme = &url[scheme_end + 3..];
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or("");
    if !is_localhost(host) {
        bail!("{env_var} must use localhost, got host '{host}' in '{url}'");
    }
    Ok(())
}
```

- [ ] **Step 4: Call validate() in main.rs**

In `main.rs`, after `let settings = Settings::from_env();` add:

```rust
settings.validate()?;
```

- [ ] **Step 5: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add rust-cli/src/settings.rs rust-cli/src/main.rs
git commit -m "feat(settings): enforce localhost for all URLs and server host"
```

### Task 2: Validate stream/table names at startup

**Files:**
- Modify: `rust-cli/src/settings.rs`

- [ ] **Step 1: Add stream/table name validation tests**

```rust
#[test]
fn validate_rejects_empty_stream_key() {
    let mut s = Settings::from_env();
    s.stream_key = String::new();
    assert!(s.validate().is_err());
}

#[test]
fn validate_rejects_stream_key_with_spaces() {
    let mut s = Settings::from_env();
    s.stream_key = "bad stream key".to_owned();
    assert!(s.validate().is_err());
}

#[test]
fn validate_rejects_empty_table_name() {
    let mut s = Settings::from_env();
    s.message_table = String::new();
    assert!(s.validate().is_err());
}

#[test]
fn validate_accepts_dotted_table_name() {
    let s = Settings::from_env();
    // default is "agent_bus.messages" which contains a dot — should pass
    assert!(s.validate().is_ok());
}
```

- [ ] **Step 2: Add name validation to Settings::validate()**

Add to the `validate()` method:

```rust
validate_identifier(&self.stream_key, "AGENT_BUS_STREAM_KEY")?;
validate_identifier(&self.channel_key, "AGENT_BUS_CHANNEL")?;
validate_identifier(&self.presence_prefix, "AGENT_BUS_PRESENCE_PREFIX")?;
validate_identifier(&self.message_table, "AGENT_BUS_MESSAGE_TABLE")?;
validate_identifier(&self.presence_event_table, "AGENT_BUS_PRESENCE_EVENT_TABLE")?;
```

Add helper:

```rust
fn validate_identifier(value: &str, env_var: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{env_var} must not be empty");
    }
    if value.contains(' ') || value.contains('\n') || value.contains('\t') {
        bail!("{env_var} must not contain whitespace, got '{value}'");
    }
    Ok(())
}
```

- [ ] **Step 3: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/settings.rs
git commit -m "feat(settings): validate stream keys and table names at startup"
```

### Task 3: Enrich health with stream length and PG row counts

**Files:**
- Modify: `rust-cli/src/models.rs`
- Modify: `rust-cli/src/redis_bus.rs`
- Modify: `rust-cli/src/postgres_store.rs`

- [ ] **Step 1: Add optional telemetry fields to Health struct**

In `models.rs`, add to `Health`:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub(crate) stream_length: Option<u64>,
#[serde(skip_serializing_if = "Option::is_none")]
pub(crate) pg_message_count: Option<i64>,
#[serde(skip_serializing_if = "Option::is_none")]
pub(crate) pg_presence_count: Option<i64>,
```

- [ ] **Step 2: Add count_messages_postgres() and count_presence_postgres()**

In `postgres_store.rs`:

```rust
pub(crate) fn count_messages_postgres(settings: &Settings) -> Option<i64> {
    run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok(None);
        };
        let row = client.query_one(
            &format!("select count(*) as cnt from {}", settings.message_table),
            &[],
        )?;
        Ok(Some(row.get::<_, i64>("cnt")))
    })
    .ok()
    .flatten()
}

pub(crate) fn count_presence_postgres(settings: &Settings) -> Option<i64> {
    run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok(None);
        };
        let row = client.query_one(
            &format!("select count(*) as cnt from {}", settings.presence_event_table),
            &[],
        )?;
        Ok(Some(row.get::<_, i64>("cnt")))
    })
    .ok()
    .flatten()
}
```

- [ ] **Step 3: Enrich bus_health() in redis_bus.rs**

Update `bus_health()` to query XLEN and PG counts:

```rust
// After the Redis PING check, add:
let stream_length: Option<u64> = connect(settings)
    .ok()
    .and_then(|mut c| {
        redis::cmd("XLEN")
            .arg(&settings.stream_key)
            .query::<u64>(&mut c)
            .ok()
    });
```

Set the new fields in the Health struct construction:

```rust
stream_length,
pg_message_count: count_messages_postgres(settings),
pg_presence_count: count_presence_postgres(settings),
```

- [ ] **Step 4: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test`
Expected: all pass (existing health test still works; new fields are Option so serialization unchanged for compact)

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/models.rs rust-cli/src/redis_bus.rs rust-cli/src/postgres_store.rs
git commit -m "feat(health): add stream length and PG row counts to health output"
```

### Task 4: Add bounded retry for PostgreSQL persistence

**Files:**
- Modify: `rust-cli/src/postgres_store.rs`

- [ ] **Step 1: Add retry wrapper**

Add a retry helper in `postgres_store.rs`:

```rust
const PG_MAX_RETRIES: u32 = 3;
const PG_RETRY_BASE_MS: u64 = 50;

fn with_pg_retry<T>(operation: impl Fn() -> Result<T>) -> Result<T> {
    let mut last_error = None;
    for attempt in 0..PG_MAX_RETRIES {
        match operation() {
            Ok(result) => return Ok(result),
            Err(e) => {
                tracing::warn!(
                    "PostgreSQL operation failed (attempt {}/{}): {e:#}",
                    attempt + 1,
                    PG_MAX_RETRIES
                );
                last_error = Some(e);
                if attempt + 1 < PG_MAX_RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(
                        PG_RETRY_BASE_MS * (1 << attempt),
                    ));
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("PostgreSQL operation failed after retries")))
}
```

- [ ] **Step 2: Wrap persist_message_postgres with retry**

Change `persist_message_postgres` to use the retry wrapper around the inner `run_postgres_blocking` call. The outer function catches and logs the final error (preserving existing behavior where callers use `if let Err`).

- [ ] **Step 3: Wrap persist_presence_postgres with retry**

Same pattern.

- [ ] **Step 4: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/postgres_store.rs
git commit -m "feat(postgres): add bounded retry with exponential backoff for persistence"
```

## Chunk 2: P2 Performance — Connection Reuse

### Task 5: Add Redis connection reuse for HTTP mode

**Files:**
- Modify: `rust-cli/src/redis_bus.rs`
- Modify: `rust-cli/src/http.rs`

- [ ] **Step 1: Add a shared Redis connection pool type**

In `redis_bus.rs`, add a simple reusable client wrapper:

```rust
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// Shared Redis client that can be cloned across handlers.
/// Uses `redis::Client` which handles reconnection internally.
#[derive(Clone)]
pub(crate) struct RedisPool {
    client: redis::Client,
}

impl RedisPool {
    pub(crate) fn new(settings: &Settings) -> Result<Self> {
        let client = redis::Client::open(settings.redis_url.as_str())
            .context("Redis client creation failed")?;
        Ok(Self { client })
    }

    pub(crate) fn get_connection(&self) -> Result<redis::Connection> {
        self.client.get_connection().context("Redis connection failed")
    }
}
```

- [ ] **Step 2: Update HTTP AppState to include RedisPool**

In `http.rs`, change `AppState` from `Arc<Settings>` to:

```rust
pub(crate) struct AppState {
    pub(crate) settings: Arc<Settings>,
    pub(crate) redis: RedisPool,
}
```

Update `start_http_server()` to create `RedisPool` once and pass it in the state. Update all handlers to use `state.redis.get_connection()` instead of `connect(&settings)`.

- [ ] **Step 3: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/redis_bus.rs rust-cli/src/http.rs
git commit -m "perf(http): reuse Redis client across HTTP handlers"
```

### Task 6: Add PostgreSQL connection reuse

**Files:**
- Modify: `rust-cli/src/postgres_store.rs`

- [ ] **Step 1: Add a lazy PG client cache**

Replace the per-call `connect_postgres` pattern with a thread-local or `OnceLock`-based cached client:

```rust
use std::cell::RefCell;

thread_local! {
    static PG_CLIENT: RefCell<Option<PgClient>> = const { RefCell::new(None) };
}

pub(crate) fn get_or_connect_postgres(settings: &Settings) -> Result<Option<PgClient>> {
    let Some(database_url) = settings.database_url.as_deref() else {
        return Ok(None);
    };
    // For thread-local reuse, we try to take the cached client first
    // If it fails a health check, reconnect
    PG_CLIENT.with(|cell| {
        let mut cached = cell.borrow_mut();
        if let Some(ref mut client) = *cached {
            // Quick health check
            if client.simple_query("").is_ok() {
                // Return by taking ownership temporarily
                return Ok(cached.take());
            }
            // Connection stale, drop and reconnect
            cached.take();
        }
        let client = PgClient::connect(database_url, NoTls)
            .context("PostgreSQL connection failed")?;
        Ok(Some(client))
    })
}

pub(crate) fn return_postgres_client(client: PgClient) {
    PG_CLIENT.with(|cell| {
        cell.borrow_mut().replace(client);
    });
}
```

- [ ] **Step 2: Update persist functions to use get_or_connect + return pattern**

Update `persist_message_postgres` and `persist_presence_postgres` to use `get_or_connect_postgres` and call `return_postgres_client` on success.

- [ ] **Step 3: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/postgres_store.rs
git commit -m "perf(postgres): add thread-local connection reuse"
```

## Chunk 3: Integration test harness + cleanup

### Task 7: Add integration test harness

**Files:**
- Create: `rust-cli/tests/integration_test.rs`

- [ ] **Step 1: Create integration test file**

Create `rust-cli/tests/integration_test.rs` with tests that exercise live Redis (skipped if Redis is not available):

```rust
//! Integration tests requiring a running Redis instance.
//!
//! These tests are skipped if Redis is not reachable at the default URL.
//! Run with: RUSTC_WRAPPER="" cargo test --test integration_test

use std::process::Command;

fn agent_bus_binary() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    // Override to ensure tests don't interfere with production
    cmd.env("AGENT_BUS_STREAM_KEY", "agent_bus:test:messages");
    cmd.env("AGENT_BUS_CHANNEL", "agent_bus:test:events");
    cmd.env("AGENT_BUS_PRESENCE_PREFIX", "agent_bus:test:presence:");
    cmd
}

fn redis_available() -> bool {
    agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn health_returns_ok_when_redis_available() {
    if !redis_available() {
        eprintln!("SKIP: Redis not available");
        return;
    }
    let output = agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run agent-bus health");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""ok":true"#));
}

#[test]
fn send_and_read_round_trip() {
    if !redis_available() {
        eprintln!("SKIP: Redis not available");
        return;
    }
    // Send a test message
    let send = agent_bus_binary()
        .args([
            "send",
            "--from-agent", "test-sender",
            "--to-agent", "test-receiver",
            "--topic", "integration-test",
            "--body", "hello-from-integration-test",
            "--encoding", "compact",
        ])
        .output()
        .expect("send failed");
    assert!(send.status.success(), "send failed: {}", String::from_utf8_lossy(&send.stderr));

    // Read it back
    let read = agent_bus_binary()
        .args([
            "read",
            "--agent", "test-receiver",
            "--since-minutes", "1",
            "--encoding", "compact",
        ])
        .output()
        .expect("read failed");
    assert!(read.status.success());
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(stdout.contains("hello-from-integration-test"));
}

#[test]
fn presence_set_and_list() {
    if !redis_available() {
        eprintln!("SKIP: Redis not available");
        return;
    }
    let set = agent_bus_binary()
        .args([
            "presence",
            "--agent", "test-agent-integ",
            "--status", "online",
            "--ttl-seconds", "10",
            "--encoding", "compact",
        ])
        .output()
        .expect("presence set failed");
    assert!(set.status.success());

    let list = agent_bus_binary()
        .args(["presence-list", "--encoding", "compact"])
        .output()
        .expect("presence-list failed");
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("test-agent-integ"));
}

#[test]
fn invalid_settings_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bus"))
        .env("AGENT_BUS_REDIS_URL", "redis://remote-host:6380/0")
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run");
    assert!(!output.status.success(), "should reject non-localhost Redis URL");
}
```

- [ ] **Step 2: Run integration tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test --test integration_test -- --nocapture`
Expected: pass if Redis is running, skip gracefully if not

- [ ] **Step 3: Commit**

```bash
git add rust-cli/tests/integration_test.rs
git commit -m "test: add integration test harness for live Redis round-trips"
```

### Task 8: Update TODO.md

**Files:**
- Modify: `TODO.md`

- [ ] **Step 1: Mark completed P1/P2/P3 items**

Update TODO.md:
- P1: Check off localhost enforcement, startup validation, health enrichment, retry/backoff
- P1: Leave log rotation unchecked (requires nssm/Windows service config, not Rust code)
- P2: Check off connection reuse items (Redis + PG)
- P2: Leave benchmarks and index review unchecked (need production volume)
- P3: Check off integration test harness

- [ ] **Step 2: Commit**

```bash
git add TODO.md
git commit -m "docs: mark P1/P2/P3 completed items in TODO.md"
```

### Task 9: Final verification and push

- [ ] **Step 1: Full verification**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all pass

- [ ] **Step 2: Push**

Run: `RUSTC_WRAPPER="" git push origin main`
