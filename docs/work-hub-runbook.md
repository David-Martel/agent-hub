# Agent hub on dtm-work

dtm-work has been the authoritative hub since the cutover from asuspro13 at 2026-11-04T07:29Z. This records what runs
there, because the setup was done by hand. Check live state before relying on it. Secrets are not in this file.

## Service

- Windows service `AgentHub` through NSSM: LocalSystem, automatic start, restarts on exit.
- Binary `C:\ProgramData\AgentHub\bin\agent-bus-http.exe --port 8400`. Logs in `C:\ProgramData\AgentHub\logs`.
- Config `C:\ProgramData\AgentHub\config.json`, readable by administrators only. It holds the hub's auth token.
- Identity `dtm-work`, URL `http://100.64.0.2:8400`. Firewall rule `AgentHub 8400 tailnet` allows `100.64.0.0/10` only.
- Stores: PostgreSQL 18 on `127.0.0.1:5432` (not the fleet's usual 5300), database `redis_backend`; Redis on
  `127.0.0.1:6380`. The database from before the move is kept as `redis_backend_pre_move_20261104`.
- Cloud sync is off until a hub token for dtm-work is minted.

## Clients

All authoritative `server_urls` entries must name one hub, or `agent-bus` refuses the config. To move a hub, replace the
entry at cutover, and test the candidate file on one host first with `AGENT_BUS_CONFIG=<candidate> agent-bus health`,
keeping stderr visible.

## Backups

- [`packaging/hub-backup/hub-backup.ps1`](../packaging/hub-backup/hub-backup.ps1) runs daily as task
  `AgentHub-daily-backup` (SYSTEM) and writes to `R:\AgentHub-backups`. The deployed copy is
  `C:\ProgramData\AgentHub\hub-backup.ps1`.
- **Don't store anything on dtm-work's F: drive.** On 2026-11-04 it returned different bytes from those written, while a
  hash taken at write time passed. Both failed runs from that day are kept there as evidence.
- Restore test, last run 2026-11-04 on run `20261104-091609`: restore the dump into a throwaway database and load the
  snapshot into a temporary Redis on another port, then compare counts and a content digest with live.
- Off-site copies come from [`packaging/history-export`](../packaging/history-export/README.md). See its README for its
  state and owner steps.
- asuspro13's frozen stores are archived at `R:\AgentHub-backups\asus-frozen-20261104`: 95,681 messages, restore-tested.
  Role definitions weren't captured, because `pg_dumpall` failed on authentication; hub data doesn't depend on them.

## Known difference between stores

PostgreSQL and the Redis stream don't hold identical message sets. On 2026-11-04 there were 93 messages only in
PostgreSQL and 33 only in Redis. The cause of the Redis-only messages isn't established. Don't discard either store as a
duplicate of the other.
