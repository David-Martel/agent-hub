# Agent-Bus PostgreSQL & Redis Optimization Guide

> Date: 2026-04-04
> PG: 18.3 (Windows x64) at `localhost:5300`, database `redis_backend`
> Redis: 8.4.0 (MSYS/Windows) at `localhost:6380`
> Machine: Intel Core Ultra 7 155H, 64 GB RAM, WD_BLACK SN850X NVMe

## Current State (as of 2026-04-04)

| Metric | PostgreSQL | Redis |
|--------|-----------|-------|
| Data size | 14 MB (5,808 msgs + 2,089 presence) | 6.21 MB / 496 keys |
| Connections | 1 active (single-conn Rust client) | ~1 |
| Write pattern | Batched: 10 msgs or 100 ms flush | XADD per message |
| Read pattern | `WHERE timestamp_utc >= now() - interval` + agent/sender/topic | XRANGE / XREAD on streams |
| Growth rate | ~264 msgs/day, ~95 presence/day | ~22 keys/day (many from tests) |

## Rust Code Changes Needed

### 1. UUIDv7 for Message IDs (Priority: Medium)

**What:** Switch from `Uuid::new_v4()` (random) to `Uuid::now_v7()` (timestamp-ordered).

**Why:** UUIDv7 embeds a millisecond timestamp in the first 48 bits, making the `messages_pkey` B-tree index sequential instead of random. This eliminates random page splits and enables PG18's `uuid_extract_timestamp()` for time-range queries on the PK alone.

**Where:** Find where `Message.id` is first populated — likely in the MCP handler or CLI command layer that constructs the `Message` struct. The `postgres_store.rs` receives the ID already set.

```toml
# Cargo.toml — add v7 feature:
uuid = { version = "1", features = ["v4", "v7"] }
```

```rust
// Before (wherever Message.id is first assigned):
id: Uuid::new_v4().to_string(),

// After:
id: Uuid::now_v7().to_string(),
```

**PG side (already applied):**
```sql
ALTER TABLE agent_bus.messages ALTER COLUMN id SET DEFAULT uuidv7();
```

Existing UUIDv4 rows coexist fine — the column type is unchanged.

### 2. Stream Trimming on XADD (Priority: High)

**What:** Add `MAXLEN ~ 1000` (or `MINID ~`) to every `XADD` call so streams don't grow unbounded.

**Why:** Currently streams are never trimmed. 193 stale test keys from integration tests prove the problem. The `~` approximate flag makes trimming O(1) amortized — Redis trims at listpack node boundaries rather than counting exact entries.

**Where:** Every call site that issues `XADD` in the Rust codebase. Search for `XADD` across `agent-bus-core/src/` and `agent-bus-http/src/`.

```rust
// Before:
cmd("XADD").arg(stream_key).arg("*").arg("field").arg(value)

// After (count-based):
cmd("XADD").arg(stream_key).arg("MAXLEN").arg("~").arg("1000").arg("*").arg("field").arg(value)

// Or (time-based, keeps last 1 hour — better for a coordination bus):
let min_id = format!("{}-0", now_ms - 3_600_000);
cmd("XADD").arg(stream_key).arg("MINID").arg("~").arg(&min_id).arg("*").arg("field").arg(value)
```

`MAXLEN ~ 1000` is simpler (no timestamp computation). `MINID ~` is semantically better for a coordination bus where message age matters more than count.

### 3. TTL on Redis Keys (Priority: High)

**What:** Issue `EXPIRE` after creating any Redis key, with TTLs based on key purpose.

**Why:** All 496 keys currently have `expires=0` (no TTL). Stale keys from crashed agents, finished sessions, and test runs accumulate forever.

**Where:** Every call site that creates a key (XADD for streams, SET for cursors, SADD for group members). Add `EXPIRE` immediately after.

| Key pattern | TTL | Rationale |
|-------------|-----|-----------|
| `bus:direct:*` | 7 days (604800s) | Direct channels are session-scoped |
| `agent_bus:notify:*` | 3 days (259200s) | Notification streams are per-session |
| `bus:cursor:*` | 7 days (604800s) | Read cursors are session-scoped |
| `bus:group:*:members` | 30 days (2592000s) | Group channels may persist |
| `bus:tasks:*` | 3 days (259200s) | Task queues are transient |
| `bus:direct:orchestrator:resolve-*` | 1 hour (3600s) | Integration test artifacts |
| `bus:direct:test-*` | 1 hour (3600s) | Test artifacts |

```rust
// After every XADD or key creation:
cmd("EXPIRE").arg(stream_key).arg(ttl_seconds)
```

### 4. Redis 8 Stream Commands (Priority: Low — Future)

These are available in the current Redis 8.4.0 build and reduce round-trips:

| Current pattern | Redis 8 replacement | Savings |
|----------------|---------------------|---------|
| `XACK` + `XDEL` (2 commands) | `XACKDEL` (1 command, atomic) | 1 round-trip |
| `XAUTOCLAIM` + `XREADGROUP` | `XREADGROUP ... CLAIM <idle>` | 1 round-trip |
| `HSET` + `EXPIRE` on presence | `HSETEX key EX <ttl> FIELDS ...` | 1 round-trip |

These are optimizations for after the higher-priority items are done.

---

## PostgreSQL Configuration Changes

All PG config changes go through `ALTER SYSTEM` (writes to `postgresql.auto.conf`).
See `C:\Program Files\PostgreSQL\18\data\POSTGRESQL-TUNING.md` for the specific settings.

**Runtime changes (no restart — apply via `psql` then `SELECT pg_reload_conf()`):**
- WAL compression: `pglz` → `lz4`
- JIT: `on` → `off`
- Async I/O: `io_workers` 3 → 4 (`io_combine_limit` already at max 16 on Windows)
- Checkpoints: `checkpoint_timeout` 5min → 15min
- Monitoring: `track_io_timing`, `track_wal_io_timing`, `log_min_duration_statement`
- Autovacuum thresholds per-table

**Restart-required changes (bundle into one restart):**
- `shared_preload_libraries = 'pg_stat_statements'`
- `wal_level` = `minimal`, `max_wal_senders` = 0
- `shared_buffers` 8GB → 4GB, `max_connections` 200 → 30

**Schema changes (psql, no restart):**
- Drop 3 unused indexes after monitoring confirms zero scans
- Set `DEFAULT uuidv7()` on messages.id

## Redis Configuration Changes

All Redis config changes go through `CONFIG SET` + `CONFIG REWRITE`.
See `C:\Program Files\Redis\REDIS-TUNING.md` for the specific settings.

**Runtime changes:**
- `hz` 10 → 20
- `lazyfree-lazy-user-del` yes
- `auto-aof-rewrite-min-size` 64mb → 8mb
- `auto-aof-rewrite-percentage` 100 → 50

**Immediate cleanup:**
- UNLINK 193+ stale test keys matching `bus:direct:orchestrator:resolve-*`, `bus:direct:test-*`

---

## Monitoring Queries

### PostgreSQL — After enabling pg_stat_statements

```sql
-- Top queries by total time:
SELECT LEFT(query, 80) AS query, calls, mean_exec_time::numeric(10,2) AS avg_ms,
  total_exec_time::numeric(10,1) AS total_ms, rows
FROM pg_stat_statements
ORDER BY total_exec_time DESC LIMIT 15;

-- Confirm index usage (wait 48h after enabling):
SELECT indexrelname, idx_scan, idx_tup_read
FROM pg_stat_user_indexes
WHERE schemaname = 'agent_bus'
ORDER BY idx_scan;

-- Autovacuum health:
SELECT relname, n_live_tup, n_dead_tup,
  last_analyze::timestamp(0), last_autoanalyze::timestamp(0),
  total_vacuum_time, total_analyze_time
FROM pg_stat_user_tables WHERE schemaname = 'agent_bus';

-- Cache hit rate:
SELECT datname,
  ROUND(blks_hit::numeric / NULLIF(blks_read + blks_hit, 0) * 100, 1) AS hit_pct
FROM pg_stat_database WHERE datname = 'redis_backend';
```

### Redis — Ongoing health checks

```bash
# Memory and fragmentation:
redis-cli -p 6380 INFO memory | grep -E "used_memory_human|mem_fragmentation"

# Key count and distribution:
redis-cli -p 6380 DBSIZE
redis-cli -p 6380 INFO keyspace

# Slow queries (>10ms):
redis-cli -p 6380 SLOWLOG GET 10

# Stream lengths for key channels:
for key in $(redis-cli -p 6380 --scan --pattern "bus:direct:*" | head -10); do
  echo "$key: $(redis-cli -p 6380 XLEN "$key")"
done
```

---

## Items Explicitly Not Recommended

| Item | Reason |
|------|--------|
| Table partitioning | Premature at 5,808 rows. Would break `ON CONFLICT (id)`. Revisit at 100K+ rows. |
| Redis active defrag | Windows MSYS build uses libc, not jemalloc. Active defrag is a no-op. |
| Redis io-threads > 1 | Windows Redis uses Winsock event loop, not epoll. Multi-threading is unsupported. |
| Redis Functions (FCALL) | Adds complexity for marginal savings. Current single-connection model doesn't bottleneck on round-trips. |
| Redis HSETEX for presence | Would require restructuring the presence model from streams to hashes. Low ROI at 95 events/day. |
| Switching maxmemory-policy | `allkeys-lru` is correct. 6 MB / 2 GB means eviction never triggers. |
| pg_trgm / btree_gin | Install only after pg_stat_statements confirms body text search or combined tag+scalar queries are actually used. |
