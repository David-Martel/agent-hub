# Rotating the hub bearer token

The hub gates every HTTP route except `/health` behind one bearer token,
`auth_token`. Before `previous_auth_token` existed, changing it was a flag day:
every client and the hub had to switch in the same minute, and any client left
behind got `401`. `previous_auth_token` lets the hub accept the retired token
next to the new one while the clients are updated one host at a time.

This is the hub's own token. The cloud tier has a separate map and its own
procedure in [cloud-token-operations.md](cloud-token-operations.md).

## Settings

| Setting | Where | Meaning |
|---|---|---|
| `auth_token` | `AGENT_BUS_AUTH_TOKEN`, then `auth_token` in `config.json` | The token clients send. |
| `previous_auth_token` | `AGENT_BUS_PREVIOUS_AUTH_TOKEN`, then `previous_auth_token` in `config.json` | A retired token the HTTP server still accepts. Server-side only: a client never sends it. |

`Settings::validate` refuses a previous token when `auth_token` is unset (an open
server would look protected) and when it equals `auth_token` (the rotation did
not change anything). Neither error prints a token.

A request that presents the previous token is served normally. The hub logs a
warning on the first such request and on every thousandth after, so an operator
can see that a client is still on the old token without a log line per request.

## Procedure

Never print, log or put a token in a command line. Take new values from, and
store them in, the vault as described in
[cloud-token-operations.md](cloud-token-operations.md#credential-inventory-and-rotation-record-2026-11-05).

1. Generate the new token and write it to the vault note `agent-bus fleet token`
   first, keeping the old value in a hidden field of the same item.
2. On the hub host set `previous_auth_token` to the old value and `auth_token`
   to the new one, then restart the hub (a few seconds). Every client keeps
   working because it still presents the old value.
3. Update the clients one host at a time: the client `config.json` or token
   file, the MCP server environment, and any user-level
   `AGENT_BUS_AUTH_TOKEN`. After each host, check `agent-bus health`, a send and
   a read.
4. When the hub has logged no use of the previous token since the last client
   changed, remove `previous_auth_token` and restart the hub. A straggler now
   gets `401`, which is the signal to fix it.

Rollback at any point before step 4 is to swap the two values back and restart.

## Related

- [Cloud token operations](cloud-token-operations.md)
- [Per-candidate hub auth](per-candidate-hub-auth-20261004.md)
