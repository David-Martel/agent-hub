# P3 Module Split Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `rust-cli/src/main.rs` (~2700 LOC) into 9 focused modules with zero behavior changes.

**Architecture:** Pure extraction refactor. Each numbered section in the current monolith maps to one module file. Shared types use `pub(crate)` visibility. Tests move with their code.

**Tech Stack:** Rust 2024 edition, cargo clippy pedantic, mimalloc, redis 0.29, postgres 0.19, rmcp 1, axum 0.8

**Spec:** `docs/superpowers/specs/2026-03-15-module-split-design.md`

---

## Chunk 1: Foundation modules (settings, models, validation, output)

These have no internal dependencies beyond each other, so they can be extracted first.

### Task 1: Extract `settings.rs`

**Files:**
- Create: `rust-cli/src/settings.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `settings.rs` with Settings struct and env helpers**

Move lines 52-126 from `main.rs`. Add `pub(crate)` to `Settings`, `Settings::from_env()`, `env_or()`, `env_or_optional_default()`, and `env_parse()`.

```rust
// rust-cli/src/settings.rs
//! Environment-variable configuration, read once at startup.

/// All environment-variable configuration, read at startup.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) redis_url: String,
    pub(crate) database_url: Option<String>,
    pub(crate) stream_key: String,
    pub(crate) channel_key: String,
    pub(crate) presence_prefix: String,
    pub(crate) message_table: String,
    pub(crate) presence_event_table: String,
    pub(crate) stream_maxlen: u64,
    pub(crate) service_agent_id: String,
    pub(crate) startup_enabled: bool,
    pub(crate) startup_recipient: String,
    pub(crate) startup_topic: String,
    pub(crate) startup_body: String,
    pub(crate) server_host: String,
}

impl Settings {
    pub(crate) fn from_env() -> Self {
        // ... exact existing code ...
    }
}

pub(crate) fn env_or(key: &str, default: &str) -> String { /* existing */ }
pub(crate) fn env_or_optional_default(key: &str, default: &str) -> Option<String> { /* existing */ }
pub(crate) fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T { /* existing */ }
```

Also move `redact_url()` here since it operates on settings-level URL strings.

- [ ] **Step 2: Add `mod settings;` to `main.rs`, replace removed code with `use crate::settings::*;`**

- [ ] **Step 3: Run `cargo check` to verify compilation**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo check`
Expected: compiles with no errors

- [ ] **Step 4: Run `cargo clippy`**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings`
Expected: no warnings

- [ ] **Step 5: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo test`
Expected: all existing tests pass

- [ ] **Step 6: Commit**

```bash
git add rust-cli/src/settings.rs rust-cli/src/main.rs
git commit -m "refactor: extract settings.rs from main.rs"
```

### Task 2: Extract `models.rs`

**Files:**
- Create: `rust-cli/src/models.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `models.rs` with Message, Presence, Health, and constants**

Move lines 127-203 from `main.rs`. Includes:
- `PROTOCOL_VERSION`, `MAX_HISTORY_MINUTES`, `XREVRANGE_OVERFETCH_FACTOR`, `XREVRANGE_MIN_FETCH`, `STARTUP_PRESENCE_TTL`
- `Message` struct + `default_priority()`
- `Presence` struct
- `Health` struct

All types and constants get `pub(crate)`.

- [ ] **Step 2: Add `mod models;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/models.rs rust-cli/src/main.rs
git commit -m "refactor: extract models.rs from main.rs"
```

### Task 3: Extract `validation.rs`

**Files:**
- Create: `rust-cli/src/validation.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `validation.rs`**

Move lines 1266-1297 from `main.rs`:
- `VALID_PRIORITIES` constant
- `validate_priority()`
- `non_empty()`
- `parse_metadata_arg()`

All items get `pub(crate)`.

- [ ] **Step 2: Add `mod validation;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/validation.rs rust-cli/src/main.rs
git commit -m "refactor: extract validation.rs from main.rs"
```

### Task 4: Extract `output.rs`

**Files:**
- Create: `rust-cli/src/output.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `output.rs`**

Move lines 924-1028 from `main.rs`:
- `Encoding` enum (needs `pub(crate)`)
- `output()`, `output_message()`, `output_messages()`, `output_presence()`
- `minimize_value()`

Depends on: `models::Message`, `models::Presence`

- [ ] **Step 2: Add `mod output;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/output.rs rust-cli/src/main.rs
git commit -m "refactor: extract output.rs from main.rs"
```

## Chunk 2: Storage modules (redis_bus, postgres_store)

### Task 5: Extract `postgres_store.rs`

**Files:**
- Create: `rust-cli/src/postgres_store.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `postgres_store.rs`**

Extract all PostgreSQL-related code from Section 3:
- `run_postgres_blocking()`
- `connect_postgres()`
- `storage_cache()`, `storage_cache_key()`, `storage_ready()`
- `ensure_postgres_storage()`
- `parse_timestamp_utc()`
- `persist_message_postgres()`
- `persist_presence_postgres()`
- `parse_tags()`
- `row_to_message()`
- `list_messages_postgres()`
- `probe_postgres()`

Depends on: `models::*`, `settings::Settings`

All functions get `pub(crate)`.

- [ ] **Step 2: Add `mod postgres_store;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/postgres_store.rs rust-cli/src/main.rs
git commit -m "refactor: extract postgres_store.rs from main.rs"
```

### Task 6: Extract `redis_bus.rs`

**Files:**
- Create: `rust-cli/src/redis_bus.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `redis_bus.rs`**

Extract all Redis-related code from Section 3:
- `connect()`
- `decode_stream_entry()`
- `parse_xrange_result()`
- `bus_post_message()` (includes Postgres dual-write call)
- `bus_list_messages_from_redis()`
- `bus_list_messages()` (PG-first facade with Redis fallback)
- `bus_set_presence()` (includes Postgres dual-write call)
- `bus_list_presence()`
- `bus_health()`

Depends on: `models::*`, `settings::Settings`, `postgres_store::*`

All functions get `pub(crate)`. Keep `#[expect(clippy::too_many_arguments)]` on `bus_post_message`.

- [ ] **Step 2: Add `mod redis_bus;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/redis_bus.rs rust-cli/src/main.rs
git commit -m "refactor: extract redis_bus.rs from main.rs"
```

## Chunk 3: Interface modules (cli, commands, mcp, http)

### Task 7: Extract `cli.rs`

**Files:**
- Create: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `cli.rs`**

Move lines 1030-1264 from `main.rs`:
- `Cli` struct with `#[derive(Parser)]`
- `Cmd` enum with `#[derive(Subcommand)]` and all variants

Depends on: `output::Encoding`

Both `Cli` and `Cmd` get `pub(crate)`.

- [ ] **Step 2: Add `mod cli;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/cli.rs rust-cli/src/main.rs
git commit -m "refactor: extract cli.rs from main.rs"
```

### Task 8: Extract `commands.rs`

**Files:**
- Create: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `commands.rs`**

Move lines 1299-1510 from `main.rs`:
- `SendArgs`, `ReadArgs`, `PresenceArgs` structs
- `cmd_health()`, `cmd_send()`, `cmd_read()`, `cmd_watch()`, `cmd_ack()`, `cmd_presence()`, `cmd_presence_list()`

Depends on: `settings::Settings`, `models::*`, `redis_bus::*`, `output::*`, `validation::*`

All items get `pub(crate)`. Note: `main.rs` will need to import `SendArgs`, `ReadArgs`, `PresenceArgs` and all `cmd_*` functions from this module for the match dispatch in `main()`.

- [ ] **Step 2: Add `mod commands;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/commands.rs rust-cli/src/main.rs
git commit -m "refactor: extract commands.rs from main.rs"
```

### Task 9: Extract `mcp.rs`

**Files:**
- Create: `rust-cli/src/mcp.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `mcp.rs`**

Move lines 1512-1886 from `main.rs`:
- `AgentBusMcpServer` struct and all `impl` blocks
- `ServerHandler` trait implementation
- All helper methods: `tool_list()`, `get_str()`, `get_str_or()`, `get_bool_or()`, `get_u64_or()`, `get_usize_or()`, `get_string_array()`, `get_object_or_empty()`, `handle_tool_call_inner()`, `json_to_text()`, `err_content()`, `ok_content()`

Depends on: `settings::Settings`, `models::*`, `redis_bus::*`, `validation::*`

`AgentBusMcpServer` and `AgentBusMcpServer::new()` get `pub(crate)`. Keep `#[expect(clippy::too_many_lines)]` on `handle_tool_call_inner`.

- [ ] **Step 2: Add `mod mcp;` to `main.rs`, update imports. Keep `use rmcp::serve_server;` in main.rs since it's used in the `main()` function.**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/mcp.rs rust-cli/src/main.rs
git commit -m "refactor: extract mcp.rs from main.rs"
```

### Task 10: Extract `http.rs`

**Files:**
- Create: `rust-cli/src/http.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Create `http.rs`**

Move lines 1888-2181 from `main.rs`:
- `AppState` type alias
- `internal_error()`, `bad_request()`
- All HTTP handler functions and their request/response structs
- `start_http_server()`
- Default value functions: `default_since_minutes()`, `default_limit()`, `default_true()`, `default_ack_body()`, `default_status()`, `default_ttl()`

**Serde note:** `HttpSendRequest::priority` uses `#[serde(default = "default_priority")]`. Since serde `default` requires a function in the current module scope, add a local `fn default_priority() -> String { crate::models::default_priority() }` wrapper in `http.rs`.

Depends on: `settings::Settings`, `models::*`, `redis_bus::*`, `validation::*`

`start_http_server()` gets `pub(crate)`.

- [ ] **Step 2: Add `mod http;` to `main.rs`, update imports**

- [ ] **Step 3: Run `cargo check && cargo clippy && cargo test`**

- [ ] **Step 4: Commit**

```bash
git add rust-cli/src/http.rs rust-cli/src/main.rs
git commit -m "refactor: extract http.rs from main.rs"
```

## Chunk 4: Final cleanup and verification

### Task 11: Clean up `main.rs` and distribute tests

**Files:**
- Modify: `rust-cli/src/main.rs`
- Modify: `rust-cli/src/settings.rs` (add tests)
- Modify: `rust-cli/src/models.rs` (add tests)
- Modify: `rust-cli/src/validation.rs` (add tests)
- Modify: `rust-cli/src/output.rs` (add tests)
- Modify: `rust-cli/src/redis_bus.rs` (add tests)
- Modify: `rust-cli/src/mcp.rs` (add tests)

- [ ] **Step 1: Move tests to their respective modules**

Distribute tests from Section 11 (`mod tests`) to each module's own `#[cfg(test)] mod tests`:

| Test | Target Module |
|------|---------------|
| `validate_priority_*`, `non_empty_*`, `parse_metadata_arg_*` | `validation.rs` |
| `minimize_*` | `output.rs` |
| `env_or_*`, `env_parse_*`, `settings_from_env_*`, `redact_url_*` | `settings.rs` |
| `decode_stream_entry_*`, `parse_xrange_result_*` | `redis_bus.rs` |
| `tool_list_*` | `mcp.rs` |
| `health_codec_*` | `redis_bus.rs` (tests `bus_health()`) |
| `max_history_minutes_*` | `models.rs` |
| `default_priority_is_normal` | `models.rs` |

- [ ] **Step 2: Verify `main.rs` is now ~200 LOC**

`main.rs` should contain only:
- Crate-level doc comment and lint suppressions
- `mod` declarations for all 9 modules
- `#[global_allocator]` with mimalloc
- `maybe_announce_startup()` function (Section 9)
- `main()` function (Section 10)
- `use` imports from submodules

- [ ] **Step 3: Run full verification**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all pass, zero warnings

- [ ] **Step 4: Run `cargo fmt`**

Run: `cd rust-cli && cargo fmt --all`

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/
git commit -m "refactor: distribute tests to modules, clean up main.rs"
```

### Task 12: Update TODO.md and CLAUDE.md

**Files:**
- Modify: `TODO.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Check off P3 module split item in TODO.md**

Mark the "Split `rust-cli/src/main.rs` into modules" item and its sub-items as done.

- [ ] **Step 2: Update CLAUDE.md architecture section**

Update the section table to reflect the new module file structure instead of numbered sections in a single file.

- [ ] **Step 3: Commit**

```bash
git add TODO.md CLAUDE.md
git commit -m "docs: update TODO and CLAUDE.md for module split"
```

### Task 13: Final push verification

- [ ] **Step 1: Run the full pre-push verification**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all pass

- [ ] **Step 2: Push to GitHub**

Run: `RUSTC_WRAPPER="" git push origin main`
Expected: push succeeds (lefthook pre-push passes)
