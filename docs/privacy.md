# Privacy

librelink-mcp-rs runs only on your machine. It reads one LibreLinkUp follower account and has no server, telemetry, or account of its own.

## What is stored where

**OS credential store** (macOS Keychain, Windows Credential Manager, or Linux Secret Service), under the service `org.librelink-mcp-rs`:

| Entry | Contents |
| --- | --- |
| `librelinkup-account` | The follower account's email, password, and region. |
| `local-settings` | The target range, the API version, and, until it is converted, a full-name patient selection saved before 1.0. |
| `selected-patient` | The patient ID chosen with `select-patient`. |

**Local database:** only if you run `collect` or `import-csv`. It defaults to `readings.sqlite3` in the platform data directory, for example `~/.local/share/librelink-mcp-rs/` on Linux; `collect` prints the path when it starts. Its WAL, shared-memory, and lock files sit beside it. On Unix the directory is `0700` and the files are `0600`. It holds:

- **readings:** glucose values, timestamps, their time basis, trend codes, origins, and when each reading was first seen;
- **polls:** one row per poll attempt, with its outcome and readings added;
- **collector heartbeat:** its time, process ID, and version;
- **sensors:** the latest sensors, masked to their last four characters;
- **meta:** the collector's retention setting and a random per-database salt.

Readings are keyed by a salted SHA-256 of the patient ID. See [what the collector stores](collector.md#privacy).

**Memory:** `serve` caches the last LibreLinkUp read for 60 seconds. Nothing else is kept between calls.

## What is never stored

- **No plaintext copies:** credentials never go into a file, environment variable, command-line argument, MCP tool, or bundle setting.
- **No tokens on disk:** session tokens are kept in memory only.
- **Not in the database:** the email, the raw patient ID, and patient names.
- **No raw sensor identifiers:** serial numbers and device IDs are never stored unmasked.
- **Not from a CSV import:** the export's first line (which names the account holder), serial numbers, notes, insulin, food, and strip or ketone values. The source file itself is never copied or changed.
- **Never in errors or logs:** credentials, tokens, account or patient IDs, names, serial numbers, glucose values, file contents, or upstream response bodies. Tests check this with planted markers.

## Network destinations

The only network traffic is HTTPS to LibreLinkUp's regional API host for your account:
- `api.libreview.io`, or a regional `api-<region>.libreview.io`;
- `api.libreview.ru`;
- `api-cn.myfreestyle.cn`.

It comes only from `check`, `select-patient`, `collect`, and `serve` when it uses the live API.

There is no telemetry, crash reporting, or update check. `import-csv`, `clear-local-data`, and `serve` while a collector is running make no network requests. The account ID is sent only as a SHA-256 hash, as the LibreLinkUp app does.

Abbott's own privacy terms apply to the LibreLinkUp account and its data.

## What the MCP client sees

The MCP client, and the AI model behind it, receives tool replies. Those include:
- glucose values and their timestamps;
- coverage and statistics;
- masked sensor identifiers;
- the data source.

Replies never include credentials, tokens, account or patient IDs, names, or unmasked serial numbers. Whatever the client does with the replies is governed by that client and its provider. If you use a hosted model, treat your glucose readings as shared with that provider.

`configure_credentials` accepts no secrets. `configure_ranges` writes only the target range.

## Deleting your data

Stop the collector and any MCP client using the server, then run:

```sh
librelink-mcp-rs clear-local-data
```

This removes:
- all three credential-store entries;
- the default database, with its `-wal`, `-shm`, `.lock`, and `.import.lock` files.

For a database at another location, run:

```sh
librelink-mcp-rs clear-local-data --db /path/to/readings.sqlite3
```

The second form also removes the credential-store entries. It refuses while a collector or an import holds that database's lock, before deleting anything.

Copies are not tracked: delete any you made yourself. That includes database backups (for example one made before an upgrade), copies of LibreView exports, and saved MCP client conversations.

To keep less history, run `collect --retention-days <days>`. Older readings and polls are pruned at startup and then daily, and imports respect the same limit.
