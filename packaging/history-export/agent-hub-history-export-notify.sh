#!/bin/sh
# Post an agent-bus finding for a failed agent-hub unit.
#
#   agent-hub-history-export-notify.sh <unit> <host>
#
# agent-hub-history-export-notify@.service runs this through OnFailure=, with
# the failed unit as <unit>. A <unit> containing "positive-control" is a test
# of this alert path, not a failure. It posts a POSITIVE-CONTROL finding at
# SEVERITY info on thread agent-hub-alert-selftest, so no lane on the shared
# machine-hygiene thread mistakes it for an incident. README.md, "Run", has the
# command. AGENT_BUS overrides the agent-bus binary (the test uses a recorder).
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <unit> <host>" >&2
    exit 2
fi
unit=$1
host=$2

case $unit in
    *positive-control*)
        thread=agent-hub-alert-selftest
        body="FINDING: POSITIVE-CONTROL of the agent-hub alert path on $host ($unit). Not an incident: no unit failed, nothing to do.
SEVERITY: info"
        ;;
    *)
        thread=machine-hygiene
        body="FINDING: systemd unit $unit failed on $host; agent-hub history was not exported or not verified on the drop. Read: journalctl --user -u $unit -n 80
SEVERITY: high"
        ;;
esac

exec "${AGENT_BUS:-agent-bus}" send --from-agent agent-hub-history-export --to-agent all \
    --topic agent-hub-findings --schema finding --thread-id "$thread" \
    --tag repo:agent-hub --tag "$thread" --body "$body" --encoding compact
