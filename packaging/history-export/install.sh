#!/usr/bin/env bash
# Install the agent-hub history export job for the current user on the hub host.
#
#   packaging/history-export/install.sh [--dry-run] [--no-enable]
#   packaging/history-export/install.sh --profile work [--rev ID] [--windows-dir DIR] [--enable] [--dry-run]
#
# Default profile: copies agent-hub-history-export.py and agent-hub-history-export-notify.sh to
# ~/.local/share/agent-hub-history-export/ and the three user units to ~/.config/systemd/user/, reloads the user manager
# and enables the daily timer. Files whose bytes already match are left alone.
# It never creates or reads the drop key; README.md says how to make one.
#
# --profile work (Work-authoritative exporter): stages the helper under
# ~/.local/share/agent-hub-history-export/<rev>/ (rev defaults to this checkout's
# git commit), points the `current` symlink there, installs the work service, timer
# and notify units, reloads the user manager and does NOT enable or start anything
# unless --enable is given. --windows-dir also copies the uncached reader to a
# Windows-local directory (for example /mnt/r/AgentHub-offsite/bin): PowerShell
# refuses a script on a WSL network share under RemoteSigned. It never creates
# the config file or any credential.
# SYSTEMCTL overrides the systemctl binary (the packaging test uses a recorder).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dry=0
enable=1
profile=asuspro13
rev=""
windir=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run) dry=1 ;;
        --no-enable) enable=0 ;;
        --enable) enable=2 ;;
        --profile)
            [ "$#" -ge 2 ] || { echo "install.sh: --profile needs a value" >&2; exit 2; }
            profile="$2"; shift ;;
        --rev)
            [ "$#" -ge 2 ] || { echo "install.sh: --rev needs a value" >&2; exit 2; }
            rev="$2"; shift ;;
        --windows-dir)
            [ "$#" -ge 2 ] || { echo "install.sh: --windows-dir needs a value" >&2; exit 2; }
            windir="$2"; shift ;;
        -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
        *) echo "install.sh: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
case "$profile" in
    asuspro13|work) ;;
    *) echo "install.sh: unknown profile: $profile" >&2; exit 2 ;;
esac
if [ "$profile" = work ]; then
    # The work profile never enables a timer on its own.
    if [ "$enable" = 2 ]; then enable=1; else enable=0; fi
    if [ -z "$rev" ]; then
        rev="$(git -C "$here" rev-parse HEAD 2>/dev/null || true)"
    fi
    case "$rev" in
        ''|*[!0-9A-Za-z._-]*)
            echo "install.sh: --profile work needs --rev (a git commit id) outside a git checkout" >&2
            exit 2 ;;
    esac
elif [ "$enable" = 2 ]; then
    enable=1
fi

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
if [ "$profile" = asuspro13 ] && [ ! -r "$key" ]; then
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

if [ "$profile" = work ]; then
    place "$here/agent-hub-history-export.py" "$share/$rev" 0644
    place "$here/windows/Get-UncachedHash.ps1" "$share/$rev/windows" 0644
    if [ -n "$windir" ]; then
        place "$here/windows/Get-UncachedHash.ps1" "$windir" 0644
    fi
    place "$here/agent-hub-history-export-notify.sh" "$share" 0644
    for unit in agent-hub-history-export-work.service agent-hub-history-export-work.timer                 agent-hub-history-export-notify@.service; do
        place "$here/systemd/$unit" "$units" 0644
    done
    if [ "$dry" = 1 ]; then
        log "would point $share/current at $rev"
    else
        ln -sfn "$rev" "$share/current"
        log "current -> $rev"
    fi
else
    place "$here/agent-hub-history-export.py" "$share" 0644
    place "$here/agent-hub-history-export-notify.sh" "$share" 0644
    for unit in agent-hub-history-export.service agent-hub-history-export.timer                 agent-hub-history-export-notify@.service; do
        place "$here/systemd/$unit" "$units" 0644
    done
fi

timer=agent-hub-history-export.timer
[ "$profile" = work ] && timer=agent-hub-history-export-work.timer
if [ "$dry" = 1 ]; then
    log "dry run: would run $systemctl_bin --user daemon-reload"
    [ "$enable" = 1 ] && log "dry run: would run $systemctl_bin --user enable --now $timer"
    exit 0
fi
"$systemctl_bin" --user daemon-reload
if [ "$enable" = 1 ]; then
    "$systemctl_bin" --user enable --now "$timer"
    log "timer enabled; next run: $systemctl_bin --user list-timers $timer"
elif [ "$profile" = work ]; then
    log "timer $timer installed but NOT enabled; enable it only after the README activation gate"
fi
