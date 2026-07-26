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

1. Deploys the three binaries as **real files** to `~/.local/bin/` (atomic
   copy-then-rename; any binary being replaced is first backed up into a
   single timestamped directory under
   `~/.local/bin/.agent-hub-installer-backups/<UTC timestamp>/`).
2. (Re)creates `~/.local/bin/{agent-bus,agent-bus-http,agent-bus-mcp}.exe`
   as absolute symlinks into `target/release/` — this matches the existing
   on-disk convention for Windows-parity tooling on this fleet that expects
   a `.exe` suffix.
3. Verifies `agent-bus --version` and `agent-bus health`.

Re-running is idempotent: binaries are compared by sha256 before copying,
and unchanged binaries/symlinks are skipped with a log line rather than
rewritten.

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
`agent-bus-http` and `agent-bus-mcp` block in serve mode on launch and do not
support `--version`, so never invoke them for a version/health check.

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
| [`../install.sh`](../install.sh) | Main POSIX installer (build, deploy, symlink, verify, optional relay) |
| [`systemd/agent-bus-http.service.in`](systemd/agent-bus-http.service.in) | Dedicated authoritative HTTP hub unit |
| [`systemd/agent-bus-relay.service.in`](systemd/agent-bus-relay.service.in) | Template for the optional federation relay unit |
| [`tests/install-http-service-test.sh`](tests/install-http-service-test.sh) | HTTP unit rendering, idempotency, permission, and no-auto-start regression test |
