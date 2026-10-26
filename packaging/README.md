# Packaging

Linux/macOS/WSL installer and systemd packaging for the agent-hub workspace.
For Windows service installation, use `scripts/install-agent-hub-service.ps1`
and `scripts/build-deploy.ps1` instead — this directory covers the POSIX side.

## Install

From the repo root:

```bash
./install.sh --dry-run          # preview: build plan, deploy plan, symlinks, verification
./install.sh                    # build + deploy + verify
./install.sh --skip-build       # deploy whatever is already in target/release/
```

This builds `agent-bus`, `agent-bus-http`, and `agent-bus-mcp`, then:

1. Deploys the three binaries as **real files** to `~/.local/bin/` using
   same-directory staging, source-hash validation and atomic rename.
   No backup copies are created; existing backup files and directories are
   left untouched. This is not a rollback guarantee.
2. (Re)creates `~/.local/bin/{agent-bus,agent-bus-http,agent-bus-mcp}.exe`
   as relative symlinks to the installed sibling binaries. Rebuilding or
   removing `target/release/` cannot silently change an installed alias.
3. Verifies `agent-bus --version` and `agent-bus health`.

Re-running is idempotent: binaries are compared by sha256 before copying,
and unchanged binaries/symlinks are skipped with a log line rather than
rewritten.

Replacement preserves the destination's owner, group, mode, ACLs and extended
attributes rather than importing build-tree security metadata. Linux requires
GNU `cp` built with ACL/xattr support and explicit attribute preservation; macOS
uses native `cp -p`. Both branches compare the staged owner/group/mode with the
destination. GNU `cp` can return success after an unprivileged `chown` fails, so
its exit status alone does not establish preservation of ownership. A different
owner or group that the installer cannot retain causes refusal before publication.
Preservation or staging failure leaves that destination unchanged and removes
its temporary file. New executables use mode `755`. Payload timestamps and inode
identity can change, and installation is atomic per binary, not across all three.
The POSIX installer refuses native Git Bash; it does not claim NTFS ACL handling.

The owned Linux fixture in
[`tests/install-http-service-test.sh`](tests/install-http-service-test.sh)
checks alias independence, security metadata retention, failed-copy containment,
unsupported platforms, dry runs and service activation boundaries:

```bash
bash packaging/tests/install-http-service-test.sh
```

ACL/xattr controls need `uv` with an existing Python and `libacl.so.1`, or the
`acl`/`attr` command-line tools; missing controls report `SKIP`. This fixture
mocks service managers and never starts a live hub. Its Linux result does not
qualify macOS metadata behavior; a macOS destination denying payload writes is
refused without publishing the replacement.

## Adding the authoritative HTTP hub service

Install and enable a dedicated `systemd --user` hub unit with:

```bash
./install.sh --with-http-service --dry-run
./install.sh --with-http-service
```

This renders
[`systemd/agent-bus-http.service.in`](systemd/agent-bus-http.service.in) to
`~/.config/systemd/user/agent-bus-http.service`. The unit:

- runs `~/.local/bin/agent-bus-http --port 8400` directly;
- pins Redis and PostgreSQL to loopback defaults;
- loads the normal `~/.config/agent-bus/config.json`;
- optionally loads `~/.config/agent-bus/hub.env`; and
- is enabled, but is never automatically started or restarted by the installer.

The unit template never embeds a bearer token. The preferred remote-hub setup
keeps bind/auth overrides in the optional `hub.env`:

```bash
AGENT_BUS_SERVER_HOST=0.0.0.0
AGENT_BUS_ALLOW_REMOTE=true
AGENT_BUS_AUTH_TOKEN=<from-secret-store>
```

The same settings can instead be supplied as `server_host`, `allow_remote`, and
`auth_token` in `config.json`; environment variables take precedence.

Keep both configuration sources owner-only (`chmod 600`). The installer refuses
group/world-accessible files when `--with-http-service` is requested. Review and
activate the unit explicitly:

```bash
systemctl --user cat agent-bus-http.service
systemctl --user start agent-bus-http.service
# For a running hub after a binary/unit update:
systemctl --user restart agent-bus-http.service
```

## Backend hardening (root, opt-in)

The hub's Redis and PostgreSQL are **system** services (`redis-server.service` and
`postgresql@16-main.service` on asuspro13, Redis on port 6380 and PostgreSQL on 5300). This
hardening therefore needs root, unlike the user-unit HTTP hub above. It is never installed
unless asked for, and it never starts, stops or restarts Redis or PostgreSQL.

```bash
packaging/backend-hardening.sh --dry-run   # review every file and root command
packaging/backend-hardening.sh             # install (sudo)
packaging/backend-hardening.sh --help      # options
```

The standalone script needs no build. `./install.sh --with-backend-hardening` runs the same
script after the normal build, deploy and verify steps. `--skip-build` there still (re)deploys
whatever is in `target/release/` to `~/.local/bin`, and fails if nothing is built, so use the
standalone script when only the hardening is wanted.

**Why.** On 2026-10-26 at 22:32Z a power cut left 1155 bad bytes at the tail of the hub's AOF
INCR file. `redis-server` refused to load it, the packaged unit (`Restart=always`, 100 ms
apart, 5 starts in 10 s) hit its start limit within 2 s, and the fleet bus stayed down until
someone ran `redis-check-aof --fix` by hand. `aof-load-truncated yes` does not cover this: it
handles a file that ends early, not a partly written final command. PostgreSQL recovered
cleanly from the same cut (fsync, full_page_writes and synchronous_commit are on), but there
was no copy of its databases at all.

**What it installs.**

| Target | Effect |
|---|---|
| `/usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight` | [Start check](libexec/agent-bus-redis-aof-preflight). A valid AOF passes (0.38 s for 113 MB, measured). A bad tail of at most `--redis-max-fix-bytes` (1 MiB) is repaired with `redis-check-aof --fix`, after the AOF directory is copied to `<dir>.preflight-backup.1` (three generations kept, each about the size of the AOF). A larger region, an unreadable report or a file still invalid after the fix refuses the start, and the journal says how to recover. |
| `/etc/systemd/system/redis-server.service.d/agent-bus-aof-preflight.conf` | [Drop-in](systemd/redis-server.service.d/agent-bus-aof-preflight.conf.in): `ExecStartPre=` the check, `RestartSec=5`, 10 starts in 300 s. That is about 50 s of retries before systemd gives up loudly; the installer refuses a burst and delay that cannot fit the interval. Applies at Redis's next start. |
| `/etc/sysctl.d/60-agent-bus-redis-overcommit.conf` | [`vm.overcommit_memory = 1`](sysctl/60-agent-bus-redis-overcommit.conf.in), applied live with `sysctl -p` and read back. Redis warns at every start that its background-save fork can fail without it. |
| `/usr/local/libexec/agent-bus/agent-bus-postgres-dump` | [Dump script](libexec/agent-bus-postgres-dump): `pg_dump -Fc` per database into `<dir>/<db>/`, kept only if `pg_restore --list` reads it, newest 14 kept. A failed database is counted and the others still run; any failure exits 1. |
| `/var/backups/agent-bus-postgres/` | Dump directory, owned by `postgres`, mode 0700. |
| `/etc/systemd/system/agent-bus-postgres-dump.{service,timer}` | [Oneshot](systemd/agent-bus-postgres-dump.service.in) as `postgres` over the local socket (peer authentication, no stored password), sandboxed to write only the dump directory, and a [daily timer](systemd/agent-bus-postgres-dump.timer.in) (03:30, up to 15 min jitter, `Persistent=true`). Only the timer is enabled and started. |

Measured on asuspro13 on 2026-10-26: `redis_backend` dumps to 24 MB in about 4 s and `workq` to
16 KB, from a real sandboxed run against the live cluster.

**Settings.** Every name, path, port and database list is an option of
[`backend-hardening.sh`](backend-hardening.sh) (`--help` lists them); `install.sh` forwards
`--backend-<option> <value>` as `--<option> <value>`, for example `--backend-pg-port 5432` or
`--backend-only redis`. Defaults match asuspro13. The installer validates everything before it
renders or runs anything, and refuses:

- a dump directory under `/tmp` or `/var/tmp`: the unit's `PrivateTmp` hides those, and
  systemd fails it with status 226 (measured);
- a database name outside `[A-Za-z0-9_]`, and paths with spaces, quotes or `%`;
- an AOF manifest that does not exist, unless `--allow-missing-aof` is given. The preflight
  treats a missing manifest as a clean start, so a wrong `--redis-aof-dir` would otherwise
  protect nothing. `/etc/redis/redis.conf` is root-only; the AOF directory is Redis's `dir`
  plus `appenddirname`, and the manifest is `appendfilename` plus `.manifest`.

The dry run prints every target with its rendered content and every root command, and never
calls sudo. It is a preview: its `install` lines name a staging directory that is removed on
exit, so run the script without `--dry-run` rather than pasting them. After installing, check the drop-in and run one dump:

```bash
systemctl show redis-server.service -p DropInPaths,ExecStartPre,RestartUSec
sudo systemctl start agent-bus-postgres-dump.service
journalctl -u agent-bus-postgres-dump.service -n 20
```

**Restore** a database from a dump into a scratch database first, never over the live one:
`sudo -u postgres createdb -p 5300 restore_check && sudo -u postgres pg_restore -p 5300 -d restore_check <dump>`.

**Relation to other backups and the cloud tier.** `agent-bus backup` writes an NDJSON export
of recent messages to `AGENT_BUS_BACKUP_DIR`. The dump is a complete copy of each PostgreSQL
database, including presence history and anything else in it. Neither leaves the host. The
Cloudflare tier ([`cloud/agentbus`](../cloud/agentbus/README.md), [cloud sync](../docs/cloud-sync.md))
is a replica of `internal` messages and presence, not a backup target: it never receives
`sensitivity=no-offsite` messages, and R2 is denied on the current token and deferred. A dump
holds every message, so shipping it off-site would bypass that filter. If off-site copies are
wanted later, the fitting place is the deferred R2 snapshot binding in
[`wrangler.toml`](../cloud/agentbus/wrangler.toml), with its own sensitivity decision.

Tests: [`tests/backend-hardening-test.sh`](tests/backend-hardening-test.sh) drives both scripts
with fake `redis-check-aof`, `pg_dump` and `pg_restore` that replay the formats measured from
Redis 7.0.15 and PostgreSQL 16, and the installer with a privilege recorder that runs nothing.
CI runs it in the required Unit Tests job. Background: [POSTGRES-REDIS.md](../POSTGRES-REDIS.md).

## Update

Re-run the installer after pulling new commits:

```bash
git pull
./install.sh
```

Use `--force` to redeploy even when sha256 hashes already match (rarely
needed — mainly useful if you suspect a corrupted deploy).

## Client vs. server binary caveat

This box is a **client** of the agent-bus hub (currently hosted on
`asuspro13`). All three binaries are still built and deployed here because:

- `agent-bus` (CLI) is used directly for `send`/`read`/`health`/etc. against
  the remote hub.
- `agent-bus-mcp` and `agent-bus-http` are server binaries. They are deployed
  so this machine *can* run a local hub or MCP stdio server, but neither
  starts automatically — nothing here launches an HTTP listener except a
  deliberately-installed relay or an MCP client spawning `agent-bus-mcp`
  itself.

Only `agent-bus --version` is used for install-time version verification.
The standalone HTTP binary supports `--version`, but `--help` starts its server;
MCP ignores flags and starts stdio serving. Do not invoke those binaries during
installer verification. A version response does not qualify server runtime health.

## Adding the relay service (optional)

A `systemd --user` service can bridge this machine's local hub with the
central hub. It is optional and NOT required for normal client usage
(`send`/`read`/`health` via the CLI work fine without it).

```bash
./install.sh --with-relay-service
```

This renders [`packaging/systemd/agent-bus-relay.service.in`](systemd/agent-bus-relay.service.in)
into `~/.config/systemd/user/agent-bus-relay.service`, substituting:

- `@MACHINE@` — this machine's short hostname
- `@BIN@` — path to the `agent-bus-relay.sh` wrapper script (autodetected at
  `~/.config/agent-bus-relay/agent-bus-relay.sh`; override with `--relay-bin`)
- `@CONFIG@` — path to `~/.config/agent-bus/config.json`

The unit is installed and enabled (`systemctl --user enable`), and user
linger is enabled if not already (so the service can run without an active
login session) — but the service is **not started**. Review the rendered
unit and the relay script's `--hub`/`--local` arguments before starting it:

```bash
systemctl --user cat agent-bus-relay.service
systemctl --user start agent-bus-relay.service
journalctl --user -u agent-bus-relay.service -f
```

The relay wrapper script itself (`agent-bus-relay.sh`) is machine-local
operational tooling and is not currently packaged by this repo — the
installer only manages the systemd unit that invokes it. If the script is
missing, `install.sh --with-relay-service` still writes the unit but warns
that it will fail to start until the script exists at the resolved `@BIN@`
path.

## Files

| File | Purpose |
|------|---------|
| [`../install.sh`](../install.sh) | Main POSIX installer (build, deploy, symlink, verify, optional HTTP/relay units and backend hardening) |
| [`systemd/agent-bus-http.service.in`](systemd/agent-bus-http.service.in) | Dedicated authoritative HTTP hub unit |
| [`systemd/agent-bus-relay.service.in`](systemd/agent-bus-relay.service.in) | Template for the optional federation relay unit |
| [`tests/install-http-service-test.sh`](tests/install-http-service-test.sh) | HTTP unit rendering, idempotency, permission, and no-auto-start regression test |
| [`backend-hardening.sh`](backend-hardening.sh) | Root-owned Redis/PostgreSQL hardening installer (`install.sh --with-backend-hardening`) |
| [`libexec/agent-bus-redis-aof-preflight`](libexec/agent-bus-redis-aof-preflight) | Redis `ExecStartPre` AOF check and bounded repair |
| [`libexec/agent-bus-postgres-dump`](libexec/agent-bus-postgres-dump) | Verified, rotated `pg_dump -Fc` per database |
| [`systemd/redis-server.service.d/agent-bus-aof-preflight.conf.in`](systemd/redis-server.service.d/agent-bus-aof-preflight.conf.in) | Redis drop-in: preflight, restart delay and start limit |
| [`systemd/agent-bus-postgres-dump.service.in`](systemd/agent-bus-postgres-dump.service.in), [`.timer.in`](systemd/agent-bus-postgres-dump.timer.in) | Sandboxed dump oneshot and its daily timer |
| [`sysctl/60-agent-bus-redis-overcommit.conf.in`](sysctl/60-agent-bus-redis-overcommit.conf.in) | `vm.overcommit_memory` for Redis's background fork |
| [`tests/backend-hardening-test.sh`](tests/backend-hardening-test.sh) | Preflight, dump and installer fixture (fakes only; run in CI Unit Tests) |
