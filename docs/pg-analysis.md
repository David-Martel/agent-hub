# PostgreSQL Query Plan Analysis

Date: 2026-03-20
Database: `redis_backend` at `localhost:5300`
Table counts: 3,805 messages, 881 presence events

## Table and Index Sizes

| Object | Size |
|--------|------|
| `agent_bus.messages` (heap) | 1,584 kB |
| `agent_bus.messages` (total with indexes) | 3,048 kB |
| `agent_bus.presence_events` (total) | 304 kB |
| `agent_bus_messages_recipient_ts_idx` | 264 kB |
| `agent_bus_messages_sender_ts_idx` | 264 kB |
| `agent_bus_messages_topic_ts_idx` | 232 kB |
| `messages_pkey` | 168 kB |
| `agent_bus_messages_stream_id_idx` | 136 kB |
| `agent_bus_messages_reply_to_idx` | 128 kB |
| `agent_bus_messages_tags_idx` (GIN) | 104 kB |
| `agent_bus_presence_events_agent_ts_idx` | 72 kB |

## Index Usage (at time of analysis)

| Index | Scans | Tup Read | Assessment |
|-------|------:|--------:|------------|
| `messages_pkey` | 806 | 0 | High use (conflict-check on INSERT) |
| `agent_bus_messages_recipient_ts_idx` | 228 | 5,734 | Hot path — list-messages query |
| `agent_bus_messages_topic_ts_idx` | 45 | 752 | Moderate use |
| `agent_bus_messages_sender_ts_idx` | 8 | 56 | Low but needed |
| `agent_bus_messages_tags_idx` | 2 | 278 | GIN — used for tag queries |
| `agent_bus_messages_reply_to_idx` | 2 | 5,999 | Used by journal/sync |
| `agent_bus_presence_events_agent_ts_idx` | 0 | 0 | Not yet scanned |
| `agent_bus_messages_stream_id_idx` | 0 | 0 | Not yet scanned |
| `presence_events_pkey` | 0 | 0 | Expected (bigserial PK) |

## Findings

### Finding 1: GIN index on `tags` is working correctly

The `EXPLAIN ANALYZE` for the tag-based query confirms index use:

```
Bitmap Index Scan on agent_bus_messages_tags_idx
  Index Cond: (tags @> '["repo:agent-bus"]'::jsonb)
  Execution Time: 1.401 ms
```

No action needed.

### Finding 2: Composite index on `(recipient, timestamp_utc)` is optimal

The most-executed query (list messages by agent) uses a Bitmap Index Scan:

```
Bitmap Index Scan on agent_bus_messages_recipient_ts_idx
  Index Cond: ((recipient = ANY ('{claude,all}'::text[]))
               AND (timestamp_utc >= (now() - '01:00:00'::interval)))
  Execution Time: 0.342 ms
```

The planner correctly uses the existing composite index. No new index needed.

### Finding 3: `count(*)` caused seq scans (FIXED)

The health endpoint calls `count_both_postgres`, which runs:
```sql
SELECT count(*) FROM agent_bus.messages
SELECT count(*) FROM agent_bus.presence_events
```

Without a timestamp-only index, PostgreSQL chose a full Seq Scan on both tables
(993 seq scans observed, reading 3.5M tuples from `messages` and 827K from
`presence_events`).

**Fix applied (2026-03-20):** Two new indexes created and backfilled into
`ensure_postgres_storage` so they are created automatically on fresh installs:

```sql
CREATE INDEX agent_bus_messages_ts_idx
    ON agent_bus.messages (timestamp_utc DESC);

CREATE INDEX agent_bus_presence_events_ts_idx
    ON agent_bus.presence_events (timestamp_utc DESC);
```

After `VACUUM ANALYZE`, both count queries now use **Index Only Scans**:

```
-- messages
Aggregate  (cost=81.17..81.18)
  -> Index Only Scan using agent_bus_messages_ts_idx on messages
       (cost=0.28..71.66 rows=3805)

-- presence_events
Aggregate  (cost=21.19..21.20)
  -> Index Only Scan using agent_bus_presence_events_ts_idx on presence_events
       (cost=0.28..18.99 rows=881)
```

Estimated speedup: seq scan read ~3.5M + 827K tuples per health call;
index-only scan reads 3,805 + 881 index entries — approximately **1000x fewer
tuple reads** at current row counts, growing with the table.

### Finding 4: `presence_events` query plan is fine

`list_presence_history_postgres` produces a well-planned Index Scan using the
`(agent, timestamp_utc)` composite index even when no agent filter is provided
(the index condition fires on `timestamp_utc` alone):

```
Index Scan using agent_bus_presence_events_agent_ts_idx on presence_events
  Index Cond: (timestamp_utc >= (now() - '01:00:00'::interval))
  Execution Time: 0.763 ms
```

## Connection Pool Assessment

### Current configuration

`postgres_store.rs` uses a **single shared `PgClient`** behind a
`Mutex<Option<PgClient>>` with take/return semantics:

- Take the client out (mutex released during I/O)
- Execute query
- Return on success; drop on error (triggers reconnect on next call)
- Reconnect lazily on any error via `get_pg_client`

### Server-side settings (observed)

| Setting | Value |
|---------|-------|
| `max_connections` | 200 |
| `work_mem` | 32 MB |
| `shared_buffers` | 8 GB |
| Active connections to `redis_backend` | 2 |

### Assessment

With only 2 active connections and all queries completing in < 2 ms, the
single-connection pool is **adequate for current load**. The 8 GB
`shared_buffers` means all 3 MB of message table data fits in memory, which
explains why index-hit queries are so fast (no disk I/O observed in any plan).

### When to revisit

Increase to a `r2d2`-style multi-connection pool (e.g. `deadpool-postgres` or
`r2d2-postgres`) if:
- `pg_stat_activity` shows > 5 simultaneous connections
- P99 write latency in `pg_metrics` exceeds 50 ms under load
- The HTTP server processes > 100 concurrent requests

At current agent-bus traffic patterns (< 50 req/s) a single pooled connection
with retry logic is simpler and avoids connection overhead.

## Summary of Changes

| Item | Action |
|------|--------|
| GIN index on `tags` | Already correct — no change |
| Composite `(recipient, timestamp_utc)` | Already correct — no change |
| `count(*)` seq scan on `messages` | Fixed: added `agent_bus_messages_ts_idx` |
| `count(*)` seq scan on `presence_events` | Fixed: added `agent_bus_presence_events_ts_idx` |
| `ensure_postgres_storage` | Updated to create both new indexes on fresh install |
| Connection pool | Single-connection pool adequate for current load |
