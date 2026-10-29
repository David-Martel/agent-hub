#!/usr/bin/env bash
# Install the agent-hub history export job for the current user on the hub host.
#
#   packaging/history-export/install.sh [--dry-run] [--no-enable]
#
# Copies agent-hub-history-export.py and agent-hub-history-export-notify.sh to
# ~/.local/share/agent-hub-history-export/ and the three user units to ~/.config/systemd/user/, reloads the user manager
# and enables the daily timer. Files whose bytes already match are left alone.
# It never creates or reads the drop key; README.md says how to make one.
# SYSTEMCTL overrides the systemctl binary (the packaging test uses a recorder).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dry=0
enable=1
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry=1 ;;
        --no-enable) enable=0 ;;
        -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
        *) echo "install.sh: unknown argument: $arg" >&2; exit 2 ;;
    esac
done

systemctl_bin="${SYSTEMCTL:-systemctl}"
share="$HOME/.local/share/agent-hub-history-export"
units="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
envfile="$HOME/.config/bash/env.bash"
key="$HOME/.ssh/agent-hub-drop_ed25519"

log() { printf 'install.sh: %s\n' "$*"; }

# The units source env.bash for the toolchain PATH. Without it every run would
# fail at start, so refuse here instead of installing a timer that cannot work.
if [ ! -r "$envfile" ]; then
    echo "install.sh: $envfile is missing; the units source it for PATH (uv, agent-bus, psql)" >&2
    exit 1
fi
if [ ! -r "$key" ]; then
    log "warning: drop key $key is missing; runs will fail until it exists (README.md, Install)"
fi

place() {  # place <source> <dest-dir> <mode>
    local src="$1" dir="$2" mode="$3" dest
    dest="$dir/$(basename "$src")"
    if [ -f "$dest" ] && cmp -s "$src" "$dest"; then
        log "unchanged $dest"
        return 0
    fi
    if [ "$dry" = 1 ]; then
        log "would install $dest"
        return 0
    fi
    mkdir -p "$dir"
    install -m "$mode" "$src" "$dest"
    log "installed $dest"
}

place "$here/agent-hub-history-export.py" "$share" 0644
place "$here/agent-hub-history-export-notify.sh" "$share" 0644
for unit in agent-hub-history-export.service agent-hub-history-export.timer \
            agent-hub-history-export-notify@.service; do
    place "$here/systemd/$unit" "$units" 0644
done

if [ "$dry" = 1 ]; then
    log "dry run: would run $systemctl_bin --user daemon-reload"
    [ "$enable" = 1 ] && log "dry run: would run $systemctl_bin --user enable --now agent-hub-history-export.timer"
    exit 0
fi
"$systemctl_bin" --user daemon-reload
if [ "$enable" = 1 ]; then
    "$systemctl_bin" --user enable --now agent-hub-history-export.timer
    log "timer enabled; next run: $systemctl_bin --user list-timers agent-hub-history-export.timer"
fi
