#!/usr/bin/env bash
# One-time, idempotent host preparation for the agent-hub Docker-capable runner on minisforum-ms01.
# Run as root on ms01: `sudo bash setup-host.sh`. It creates the dedicated rootless account
# `ci-agenthub`, bounds everything that account runs, and starts its own Podman API socket.
# Registration is NOT done here; see register.sh and README.md.
#
# Why a dedicated account: the job gets a Docker-compatible socket. A socket from the shared
# `vigil-ci` account would let any agent-hub job control the other repositories' runner containers
# on this host. This account's socket reaches only this account's containers (owner approval
# 2026-10-29, relayed on agent-bus machine-hygiene).
set -euo pipefail

ACCOUNT=ci-agenthub
UID_GID=2002
HOME_DIR=/srv/workspaces/ci-agenthub # the 3 TiB workspace LV; / has too little room for images
SUBID_START=296608                   # first id after vigil-ci's 231072+65536; disjoint from it
SUBID_COUNT=65536
CORES=4-7     # shared with ms01-flir-streaming-camera-1; never 8-11 (vigil-spark runner 48)
CPU_QUOTA=400%
MEMORY_MAX=16G

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 77; }

if ! getent passwd "$ACCOUNT" >/dev/null; then
  if getent passwd "$UID_GID" >/dev/null || getent group "$UID_GID" >/dev/null; then
    echo "uid/gid $UID_GID is taken by another account; refusing" >&2
    exit 65
  fi
  groupadd --gid "$UID_GID" "$ACCOUNT"
  useradd --uid "$UID_GID" --gid "$UID_GID" --home-dir "$HOME_DIR" --create-home \
    --shell /usr/sbin/nologin --comment "agent-hub CI runner (rootless podman)" "$ACCOUNT"
fi
[ "$(id -u "$ACCOUNT")" = "$UID_GID" ] || { echo "$ACCOUNT exists with another uid" >&2; exit 65; }
chmod 0750 "$HOME_DIR"

subid_end=$((SUBID_START + SUBID_COUNT - 1))
for f in /etc/subuid /etc/subgid; do
  if ! grep -q "^$ACCOUNT:" "$f"; then
    # Refuse a range that overlaps any existing entry.
    while IFS=: read -r _ start count; do
      end=$((start + count - 1))
      if [ "$SUBID_START" -le "$end" ] && [ "$start" -le "$subid_end" ]; then
        echo "subordinate range $SUBID_START-$subid_end overlaps $f entry $start+$count" >&2
        exit 65
      fi
    done <"$f"
  fi
done
grep -q "^$ACCOUNT:" /etc/subuid || usermod --add-subuids "$SUBID_START-$subid_end" "$ACCOUNT"
grep -q "^$ACCOUNT:" /etc/subgid || usermod --add-subgids "$SUBID_START-$subid_end" "$ACCOUNT"

# Aggregate bound for EVERYTHING the account runs: the runner container and the sibling
# containers (postgres, redis, image builds) that jobs start through the Podman socket.
install -d -m 0755 "/etc/systemd/system/user-$UID_GID.slice.d"
cat >"/etc/systemd/system/user-$UID_GID.slice.d/90-agenthub-ci-resources.conf" <<EOF
# Written by agent-hub config/fleet/minisforum-ms01-agenthub/setup-host.sh.
[Slice]
AllowedCPUs=$CORES
CPUQuota=$CPU_QUOTA
MemoryMax=$MEMORY_MAX
TasksMax=4096
EOF
systemctl daemon-reload
loginctl enable-linger "$ACCOUNT"
# Wait for the user manager that linger starts.
for _ in $(seq 1 30); do
  systemctl is-active --quiet "user@$UID_GID.service" && break
  sleep 1
done
systemctl is-active --quiet "user@$UID_GID.service" || { echo "user@$UID_GID did not start" >&2; exit 1; }

# The Podman API service spawns the sibling containers, so its priority is what they inherit.
user_unit_dir="$HOME_DIR/.config/systemd/user"
install -d -o "$ACCOUNT" -g "$ACCOUNT" -m 0755 "$HOME_DIR/.config" "$HOME_DIR/.config/systemd" \
  "$user_unit_dir" "$user_unit_dir/podman.service.d" "$HOME_DIR/ci-scratch"
cat >"$user_unit_dir/podman.service.d/90-ci-priority.conf" <<EOF
# Written by agent-hub config/fleet/minisforum-ms01-agenthub/setup-host.sh. Containers that jobs
# start through the socket inherit this scheduling from the API service.
[Service]
Nice=19
CPUSchedulingPolicy=idle
IOSchedulingClass=idle
CPUAffinity=$CORES
EOF
chown "$ACCOUNT:$ACCOUNT" "$user_unit_dir/podman.service.d/90-ci-priority.conf"

systemctl --user -M "$ACCOUNT@" daemon-reload
systemctl --user -M "$ACCOUNT@" enable --now podman.socket
systemctl --user -M "$ACCOUNT@" is-active podman.socket
test -S "/run/user/$UID_GID/podman/podman.sock"
echo "ok: $ACCOUNT ready; socket /run/user/$UID_GID/podman/podman.sock"
