# MCP Schema, Read-Filter, and Deploy-Drift Defects

Filed 2026-08-15 by `claude` from black-box use of the running bus on `dtm-p1gen7`, then
root-caused against this repo at `039b947`. Narrower than [`TODO.md`](./TODO.md) (roadmap) and
[`agents.TODO.md`](./agents.TODO.md) (structural refactor): this file tracks **correctness and
operability defects in the MCP tool surface**, plus the deployed-binary drift that made them hard
to attribute.

Every item below cites the file and line that grounds it. Items are ordered by severity.

---

## Context: how these were found

A caller (me) invoked `list_messages` with `topic="retrospective"`, got a well-formed response
containing messages of six *other* topics, and reasonably concluded "the topic filter is broken."
That conclusion was **wrong** — and the way it was wrong is the actual defect. There is no `topic`
filter to break. The argument was silently discarded, and the tool returned an unfiltered result
that was indistinguishable from a filtered one.

I then published that incorrect diagnosis to three stores and broadcast it to the fleet before
checking the source. **That is the cost this file exists to prevent**: a silently-ignored argument
does not produce a bug report against the schema, it produces confidently wrong downstream work.

---

## P0 — Unknown arguments are silently ignored on every MCP tool

**Grounding:** [`crates/agent-bus-core/src/mcp_dispatch.rs:69-80`](./crates/agent-bus-core/src/mcp_dispatch.rs)

```rust
pub fn schema_for(props: Value, required: &[&str]) -> Value {
    let mut schema = serde_json::Map::new();
    schema.insert("type".to_owned(), serde_json::json!("object"));
    schema.insert("properties".to_owned(), props);
    if !required.is_empty() { /* ... */ }
    Value::Object(schema)
}
```

`additionalProperties` is never set. Per JSON Schema, absent means **`true`** — extra keys are
valid. All 17 tools are built through this one helper (`TOOL_COUNT = 17`,
[`mcp_dispatch.rs:15`](./crates/agent-bus-core/src/mcp_dispatch.rs)), so **all 17 accept arbitrary
unknown arguments and discard them without diagnostic.**

The dispatch side compounds it: handlers pull named keys via `get_str(args, "…")` and never
enumerate what was actually passed, so an unrecognised key cannot be detected even in principle
([`mcp_dispatch.rs:575-603`](./crates/agent-bus-core/src/mcp_dispatch.rs)).

**Why this is P0 and not cosmetic.** The failure mode is a *confident wrong answer*, not an error.
A caller filtering on a key that does not exist receives a plausible, well-formed, unfiltered
result set and has no way to tell. This is the same class as the `rag-redis` `health_check`
returning `Ok(true)` unconditionally — reachability mistaken for correctness — which this project's
own `bus_health` was deliberately designed to avoid by reporting falsifiable counts.

- [ ] Set `"additionalProperties": false` in `schema_for`.
- [ ] Return `InvalidParams` naming the offending key(s), rather than dropping them.
- [ ] Add a test asserting every tool in `tool_definitions()` declares
      `additionalProperties: false` — mirroring the existing
      `all_tool_schemas_declare_type_object` test
      ([`mcp_dispatch.rs:1043+`](./crates/agent-bus-core/src/mcp_dispatch.rs)).
- [ ] Decide the same question for the HTTP surface: does `POST /messages` reject unknown body
      keys, or does `serde` silently drop them? (`#[serde(deny_unknown_fields)]` is the analogue.)

---

## P1 — `topic` is a write and subscription dimension but has no read filter on any surface

**Grounding:**
- MCP `list_messages` schema declares `agent`, `sender`, `repo`, `session`, `tag`, `thread_id`,
  `since_minutes`, `limit`, `include_broadcast` — **no `topic`**
  ([`mcp_dispatch.rs:105-120`](./crates/agent-bus-core/src/mcp_dispatch.rs)).
- `MessageFilters` carries only `repo`, `session`, `tags`, `thread_id`
  ([`mcp_dispatch.rs:585-590`](./crates/agent-bus-core/src/mcp_dispatch.rs)).
- CLI: `--topic` is a **send** argument (`cli.rs:132`, `563`, `648`, `685`) and a **subscribe**
  scope (`cli.rs:1095`, `1101`) — it is not a `read` filter.
- HTTP: `topic` appears on write paths and in the dashboard renderer, not as a list query param
  ([`crates/agent-bus-http/src/http.rs`](./crates/agent-bus-http/src/http.rs)).

So `topic` is first-class for **writing** (`send`, `post_message`, `post_direct`, `post_group`) and
first-class for **subscribing** — [`TODO.md`](./TODO.md) P0 explicitly lists topic among the
subscription scopes, marked complete — but is absent from every **read** path. That asymmetry is
the gap, and it is arguably a genuine oversight rather than a deliberate design choice, given
`topic` carries schema-validation semantics (`*-findings` → `finding`, `status`/`ownership`/
`coordination` → `status`).

Note the tool *description* is honest — it says "filtered by recipient, sender, repo, session, tag,
or thread_id" and correctly omits topic ([`mcp_dispatch.rs:403`](./crates/agent-bus-core/src/mcp_dispatch.rs)).
The failure was that passing `topic` anyway produced no complaint. Fixing P0 alone would have
surfaced this correctly.

- [ ] Add `topic` (and optionally `priority`) to `MessageFilters` and to the CLI `read`, HTTP list,
      and MCP `list_messages` surfaces, so read scopes match the already-shipped subscription scopes.
- [ ] If topic-filtering is intentionally excluded, say so in the tool description and reject the
      argument explicitly — silence is the thing to eliminate either way.

---

## P1 — Deployed binaries are stale, and drift unevenly against each other

Measured on `dtm-p1gen7`, 2026-08-15:

| Artifact | Built from | Date | Drift vs HEAD |
|---|---|---|---|
| `~/bin/agent-bus.exe` | `75ec8f8` | 2026-07-26 | 3 commits behind |
| `~/bin/agent-bus-http.exe` | (same build run) | 2026-07-26 | 3 commits behind |
| **`~/bin/agent-bus-mcp.exe`** | **unknown** | **2026-06-26** | **~7 weeks behind** |
| repo `HEAD` | `039b947` | 2026-08-15 | — |

`agent-bus --version` self-reports `agent-bus 0.5.0 (75ec8f8303242b8fbfdf0426b17b390f624ff62e
2026-07-26)`. `75ec8f8` **is** an ancestor of HEAD (verified with `git merge-base --is-ancestor`),
so this is straightforward staleness, not a divergent build.

Two distinct problems:

1. **`agent-bus-mcp.exe` is a month older than the other two**, despite
   [`~/.claude/CLAUDE.md`](file:///C:/Users/david/.claude/CLAUDE.md) stating "Rebuild/deploy all
   three together." The deploy is not atomic in practice. An operator debugging MCP behaviour is
   therefore testing a *different* codebase from the CLI they verify against.
2. **`agent-bus-mcp.exe` does not report a version.** Only the CLI embeds
   commit + date, so there is no way to determine what the running MCP server was built from
   without filesystem mtimes — which is how this had to be established.

Fortunately the defects above are **not** artifacts of the drift: `git log -S` confirms the
`list_messages` schema has never contained `topic`, and only a dependency bump (`2e6c7de`) touched
`mcp_dispatch.rs` between `75ec8f8` and HEAD. So the source-based root cause does describe the
running binary. **That had to be checked, not assumed** — the drift was wide enough that it might
not have.

- [ ] Embed the same `version (sha date)` string in `agent-bus-http` and `agent-bus-mcp`, and
      expose it via MCP (a `bus_health` field is the natural home — it already reports `runtime`
      and `protocol_version`).
- [ ] Make deploy atomic: fail the deploy script if the three binaries' source SHAs disagree.
- [ ] Add a startup warning when the running binary's SHA is not the deployed-manifest SHA.
- [ ] Redeploy current `main` to `~/bin` on dtm-p1gen7 once the branch merges.

---

## P2 — Client-facing parameter naming is inconsistent between CLI and MCP

The CLI uses `--from-agent` / `--to-agent`; the MCP tool requires `sender` / `recipient`
([`mcp_dispatch.rs:101`](./crates/agent-bus-core/src/mcp_dispatch.rs),
[`ops/message.rs:20-24`](./crates/agent-bus-core/src/ops/message.rs)). Calling `post_message` with
`from_agent` fails with `sender is required` — a correct rejection (these *are* required fields, so
P0's gap does not apply), but the error names the expected field without noting the alias the
caller likely meant.

Internally the code already bridges the two vocabularies: `list_messages` maps the incoming
`sender` argument onto `ReadMessagesRequest.from_agent`
([`mcp_dispatch.rs:577`, `595`](./crates/agent-bus-core/src/mcp_dispatch.rs)). So both names exist
in the codebase, with the mapping done ad hoc at one call site.

- [ ] Accept `from_agent`/`to_agent` as explicit aliases on the MCP surface, **or** document the
      divergence prominently in [`MCP_CONFIGURATION.md`](./MCP_CONFIGURATION.md) and
      [`AGENT_COMMUNICATIONS.md`](./AGENT_COMMUNICATIONS.md).
- [ ] When a required field is missing, check whether a known alias was supplied and say so
      ("`sender` is required — did you mean the CLI's `--from-agent`?").

---

## P2 — Documented compression behaviour does not match observed behaviour

Three messages posted 2026-08-15 through the running bus:

| Body size | Schema | Result |
|---|---|---|
| ~1,240 chars | none (`memory-reconciliation`) | `metadata: {}` — **uncompressed** |
| ~1,900 chars | none (`memory-reconciliation`) | `metadata: {}` — **uncompressed** |
| 582 bytes | `status` | `_compressed: "lz4"`, `_original_size: 582` |

Compression correlates with **schema presence**, not body size — the smallest message was the only
one compressed. Whatever the intended rule is, it is not "compress bodies over N bytes," and the
observable behaviour should be stated somewhere a client author will find it.

- [ ] Determine the actual predicate and document it in [`POSTGRES-REDIS.md`](./POSTGRES-REDIS.md)
      or [`IMPLEMENTATION_NOTES.md`](./IMPLEMENTATION_NOTES.md).
- [ ] If schema-gating is unintentional, decide the rule deliberately and add a test pinning it.

---

## Cross-cutting principle worth adopting repo-wide

This repo already got the health-check design right, deliberately: `bus_health` returns
falsifiable counts (`stream_length`, `pg_message_count`, `pg_dropped_writes`,
`rdb_last_bgsave_status`) rather than a bare boolean, precisely so "reachable" cannot be mistaken
for "working."

**Apply the same standard to inputs.** A filter that cannot report "I did not apply that" is the
input-side equivalent of a health check that cannot return "healthy but empty." Both fail by
returning something plausible. The fix in both cases is to make the failure *observable*, not to
make the happy path prettier.

---

## Verification checklist for whoever picks this up

- [ ] `Invoke-CargoWrapper check --llm-output`
- [ ] `Invoke-CargoWrapper clippy --all-targets --all-features -- -D warnings` (workspace lints
      already set `pedantic`/`cargo` to warn — treat warnings as failures)
- [ ] New tests for: `additionalProperties: false` on all 17 tools; unknown-arg rejection;
      topic read-filter round-trip
- [ ] Re-verify against a **freshly deployed** binary, not `~/bin`, until the drift item is closed
- [ ] Confirm the fix with a real MCP tool call — a green handshake proves nothing

---

*Filed against `039b947` on branch `fix/codex-mcp-config-preservation`. That branch has an
uncommitted edit to `crates/agent-bus-mcp/src/mcp.rs`, which I did not touch; this file is
additive. Bus claim held on this path by `claude`.*
