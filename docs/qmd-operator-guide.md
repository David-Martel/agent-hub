# qmd Operator Guide

`qmd` is a local full-text + vector search tool for indexed document
collections. This guide covers indexing the agent-bus repo docs, choosing the
right query mode, and troubleshooting timeouts.

## What qmd indexes

`qmd` indexes files into named *collections*. For agent-bus docs the
recommended collection is `repo-readmes`, which covers `README.md` and
`CLAUDE.md` files from repos in the workspace. If you add agent-bus-specific
docs to a `litho-docs` collection via `deepwiki-rs`, those are searchable
there.

Files larger than ~195 KB are skipped during the embedding phase (they receive
BM25 index entries but no vector). Regenerated summary JSON, large markdown
exports, and binary files will silently drop out of vector results.

## Indexing a collection

```bash
# Index (BM25 + embed) the repo-readmes collection
qmd embed -c repo-readmes

# Check what is indexed
qmd status
```

Run `qmd --help` to confirm exact flag syntax for your installed version.

## Query modes

| Mode | Command | Latency (CLI) | Latency (MCP) | Best for |
|------|---------|---------------|---------------|----------|
| BM25 full-text | `qmd search "query" -c <collection>` | ~0.8 s | ~0.8 s | Keyword lookups, known file names, exact terms |
| Vector | `qmd vsearch "query" -c <collection>` | 12–45 s | ~0.9 s | Conceptual / semantic similarity |
| Hybrid (BM25 + vector + rerank) | `qmd query "query" -c <collection>` | 15–50 s | ~2–3 s | Complex multi-hop questions |

### Choosing a mode

- Use **BM25** (`search`) for the vast majority of lookups: "where is
  mcp_dispatch defined", "what port does Redis use", "list the integration
  tests". Fast and reliable.
- Use **vector** (`vsearch`) when the phrasing of the query diverges from the
  phrasing in the docs ("how does the bus handle reconnect" vs "circuit
  breaker").
- Use **hybrid** (`query`) only for genuinely ambiguous or multi-hop questions
  where BM25 and vector alone both return poor results.

### CLI cold-start warning

When you run `qmd vsearch` or `qmd query` from the terminal, the CLI loads the
embedding model (embeddinggemma-300M, ~314 MB) and reranking model on every
invocation. This accounts for nearly all of the 12–50 s latency.

**If you are running many vector or hybrid queries in an agent workflow, start
the `qmd` MCP server instead.** It keeps the models resident so latency drops
to under 1 s per query. BM25 queries are equally fast in CLI and MCP modes.

```bash
# Start qmd as an MCP server (keeps models resident)
qmd mcp
```

Then register it as a stdio MCP tool in your agent harness. See `mcp.json` in
the workspace root for an example registration.

## Narrowing queries

Always pass `-c <collection>` to restrict the search scope. Without it, `qmd`
may search all indexed collections and return noise.

```bash
# Good — scoped to repo docs
qmd search "crate layout" -c repo-readmes -n 5

# Good — scoped to C4 architecture docs
qmd search "bootstrap" -c litho-docs -n 5

# Avoid — searches all collections, slower and noisier
qmd search "bootstrap"
```

Use `-n <count>` to control the result window. Default is typically 10; 3–5 is
usually enough for agent lookups. Verify with `qmd search --help`.

## Troubleshooting timeouts

### BM25 search times out or hangs

BM25 queries should complete in under 2 s. If they hang:

1. Check whether the `qmd` index is stale or locked (`qmd status`).
2. Re-index the collection: `qmd embed -c <collection>`.
3. Verify disk is not full (the index is a SQLite file; verify with
   `qmd status` or check `~/.local/share/qmd/` — verify path with `qmd --help`).

### Vector/hybrid queries time out

- In CLI mode, 12–50 s latency is normal due to model cold-start. Do not set
  short timeouts for CLI vector queries.
- If vector queries exceed 60 s or hang, the embedding model may have failed
  to load (GPU OOM or missing CUDA). Check for error output from `qmd vsearch`.
- Switch to BM25 (`search`) for the same query — if it returns results quickly,
  the index is healthy and the issue is model loading.
- Start the MCP server to avoid per-invocation model load overhead.

### Vector results are empty

- Run `qmd embed -c <collection>` to ensure embeddings are generated.
- Files larger than ~195 KB are skipped during embedding. If a file grew past
  that limit after initial indexing, it will have no vector but will still
  match BM25.
- BM25 results being present while vector results are absent confirms a missing
  embedding (not a missing index entry).

### Results are stale after doc edits

Re-run `qmd embed -c <collection>` after editing indexed files. The cache is
not automatically invalidated.

## Common agent-bus queries

```bash
# Find the four-crate layout summary
qmd search "crate map workspace layout" -c repo-readmes -n 3

# Find MCP tool names
qmd search "MCP tool names mcp_dispatch" -c repo-readmes -n 5

# Find integration test locations
qmd search "integration tests channel_integration_test" -c repo-readmes -n 3

# Find build commands
qmd search "cargo test workspace build commands" -c repo-readmes -n 5

# Conceptual: find docs about the HTTP/MCP bridge
qmd vsearch "how does the HTTP server handle MCP tool calls" -c repo-readmes -n 5
```

## Further reference

- `qmd --help` — all subcommands and flags
- `C:\codedev\CLAUDE.md` — workspace-level `qmd` documentation including
  collection inventory, performance tables, and agent search routing decision
  tree
- `docs/current-status-2026-06-13.md` — post-split status; use BM25 search
  against `repo-readmes` to navigate it quickly
