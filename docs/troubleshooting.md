# Troubleshooting

Every error has a public `error_code` and a fixed message. Neither ever includes credentials, account details, or response bodies, so both are safe to quote in an issue. Never include readings, names, or IDs in a report.

## The credential store

`setup`, `check`, `select-patient`, `collect`, and `serve` need the OS credential store. Without it they fail closed: there is no plaintext fallback.

- **"Unable to read the OS credential store", on Linux:** a Secret Service provider must be running and **unlocked** in the same user session, for example GNOME Keyring or KWallet. Unlock it by logging in to the desktop session, or with your keyring tool. Headless machines need a Secret Service provider started and unlocked for the user; otherwise use another machine for this server.
- **The server cannot find D-Bus:** MCP clients and services started outside a desktop session often have no `DBUS_SESSION_BUS_ADDRESS`. On Linux the server then falls back to your own session bus at `/run/user/<uid>/bus`, if that socket exists and belongs to you. If it still fails, start the client from your desktop session, or set `DBUS_SESSION_BUS_ADDRESS` in the client's or service's environment.
- **systemd user services:** the unit runs in your user manager, which can reach the session bus once you are logged in. The keyring must be unlocked before the collector starts.
- **macOS and Windows:** the server uses the login Keychain or Credential Manager. If access is denied, re-run `setup` from a normal terminal.

## Collector exit status 78

`collect` exits 78 when retrying cannot help. The message names the cause:

| Code | Fix |
| --- | --- |
| `authentication_failed` | The password or account was rejected. Run `setup`, then start `collect` again. |
| `client_version_rejected` | Abbott requires a newer client version. Run `set-api-version` (the message suggests one when Abbott names it), then restart. |
| `patient_selection_changed` | The person chosen with `select-patient` is no longer shared with this account. Run `select-patient`, then restart. |
| `request_rejected` | LibreLinkUp refused this client's request headers. This is a bug; please report it, without account details. |

The example systemd unit does not restart on status 78, so a wrong password never turns into repeated logins.

## Rate limits, cooldowns, and latches

- **`rate_limited`:** LibreLinkUp is throttling the account, or locked it after failed logins. The reply has `retry_after_seconds` (60 to 900). No request is made until it passes, and throttling never causes a new login.
- **Other upstream failures** (`upstream_unavailable`, `session_expired`, `api_changed`) pause calls for 60 seconds.
- **Latches:** after `authentication_failed`, `client_version_rejected`, `request_rejected`, or `patient_selection_changed`, `serve` makes no further LibreLinkUp requests until it is restarted. Fix the cause, then restart the MCP client or server.
- **`patient_selection_required`:** the account follows more than one person and none is selected. Run `select-patient`.
- **`no_connection`:** the account follows no one.
- **`configuration_error`:** credentials or settings are missing or invalid. Run `setup` or `set-api-version`. This does not start a cooldown.
- **`api_changed`:** a successful response was not JSON. The API may have changed or been retired; it is not proof of either. See [API retirement risk](roadmap.md#api-retirement-risk).

## `client_version_rejected` and `set-api-version`

Abbott sometimes requires a newer LibreLinkUp app version. `set-api-version` stores a three-part version (for example `4.16.0`) locally:

```sh
librelink-mcp-rs set-api-version
```

Then restart `serve` and `collect`. If Abbott names a minimum version, the error message includes it.

## `local_store` or `live_api`

Every data reply says where it came from, in `data_source`:

- **`local_store`:** a collector's heartbeat is under 3 minutes old, so `serve` read the local database and made no LibreLinkUp request.
- **`live_api`:** no collector is running, or its database is missing, unreadable, or from a newer version, so `serve` asked LibreLinkUp directly. History is then about 12 hours.

To use the store, run `collect` against the same database. Both commands use the default path unless given `--db`.

`local_store_unavailable` means a collector is running but its database could not be read during that call. Check the disk, and that `serve` and `collect` use the same `--db`.

## Stale or unknown data

- **`freshness: stale`:** the newest reading is more than 15 minutes old. From the store, the collector may be behind, for example during a rate-limit cooldown. From the live API, the sensor or the sharer's phone may not be uploading.
- **`freshness: unknown`:** the reading's time cannot be established, or is in the future. Treat it as not real-time.
- **`period_status: partial`:** the data does not reach both ends of the requested window. Report the observed span from `coverage`, not the requested period.
- **Gaps:** gaps are listed in `coverage.interior_gaps`, each with a `cause` when the data comes from the store.

## Panics

A panic prints a fixed message only. To see the details on your own machine, run the command again with `LIBRELINK_MCP_DEBUG=1` set, and review the output before sharing it: it may contain local data.
