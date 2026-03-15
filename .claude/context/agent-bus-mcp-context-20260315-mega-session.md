# Agent Bus MCP — Mega Session Context (2026-03-15)

## Project
- **Branch:** main @ d2cc281 (agent-hub), dfa94da3 (stm32-merge)
- **Agent-Hub:** 13 modules, ~4800 LOC, 77 tests, Windows service at localhost:8400
- **Bus:** Redis 524 / PG 475 messages, 54 presence records, 3 repos coordinated

## This Was an Epic Session

### Agent-Hub Deliverables (35+ commits)
- Module split: 2688 LOC monolith → 13 focused modules
- Settings: config.json 3-tier, localhost enforcement, startup validation
- Storage: PG shared client pool, circuit breaker, GIN index, sync backfill
- Features: SSE streaming, journal, sync, prune, export, presence-history (13 CLI subcommands)
- Schema: auto-inference from topic, auto-fitter (wraps plain bodies), validation on CLI/MCP/HTTP
- Protocol: AGENT_COMMUNICATIONS.md with MCP tools, orchestration patterns, ownership claims, polling
- Service: nssm direct binary, auto-restart, build-deploy.ps1
- Quality: lefthook + ast-grep enforcement, .gitattributes LF normalization
- Infrastructure: Redis AOF + PG trust auth + 100K MAXLEN

### stm32-merge Deliverables (30+ commits)
- P0 fixes: FPU fpv5-sp-d16, MSC memcpy bounds, USB VID centralization (29 refs)
- IEC 62304: 8 modules tagged, DHF validation wired into lefthook + CI (blocking)
- Build: lsp_sync.cmake, toolchain consolidation, emulation+simulator CMake presets
- Cleanup: 26 git artifacts removed, CMSIS dedup identified (395MB), DEPRECATED.md (9 items, 9GB)
- Simulator: WORKING at 800x480 SDL2 + LVGL + miniaudio (48kHz/24-bit WASAPI audio)
  - Both MinGW (20.6MB) and MSVC (4.9MB) builds verified
  - Interactive mode with keyboard navigation (F1-F11)
- Renode: SAI_Timed.cs C# peripheral, DMA2_AudioStub.cs, audio_timing.robot
- Testing: 82 GoogleTest cases for audio_engine (329/329 pass), 2 bugs discovered
- CargoTools: -LlmOutput, -PublishBin flags, CPAL audio bridge (Rust FFI)

### Multi-Agent Orchestration Stats
- 40+ specialist agents dispatched across 12+ waves
- 524 Redis / 475 PG messages across 3 repos
- Cross-session coordination: finance-warehouse Claude adopted our protocol improvements in real-time
- Patterns validated: ownership claims, wave-then-quality-gate, orchestrator-mediated handoffs
- Schema adoption: auto-inference solved the zero-adoption problem

## Bugs Discovered
1. audio_engine.cpp:205 — initCodec() has inverted BSP guard (calls init when HW unavailable)
2. audio_engine.cpp:1400 — EQ stubs are unimplemented (enableEQ/isEQEnabled)
3. 48kHz vs 44.1kHz discrepancy between code and CLAUDE.md docs

## Next Session
1. Build ARM firmware ELF and run Renode smoke tests with SAI_Timed peripheral
2. Fix initCodec() inverted guard bug
3. Delete vendor/CMSIS/ (395MB unused duplicate)
4. Flatten repo: apps/stm32_tinntester/ → firmware/
5. Async PG write-through (tokio mpsc)
6. Complete simulator interactive audio (synthesizer → miniaudio bridge)
7. Run GoogleTest suite in CI
