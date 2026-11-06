# Changelog

## 1.0.0 — 2026-11-06

The configuration validated for this release is listed in [compatibility](https://github.com/johnohhh1/librelink-mcp-rs/blob/v1.0.0/docs/compatibility.md). Other accounts, regions, sensors, hosts, and clients are unvalidated.

### Added
- `select-patient` lists the people the account follows in an interactive terminal and stores the chosen patient ID in the OS credential store. The ID is never printed or written to the settings file.
- MCP tool annotations on all eight tools. The data tools are read-only, idempotent, and open-world; `configure_ranges` is not read-only.
- An `outputSchema` for every tool, with typed replies that match it. Error replies match it too.
- The resource `glucose://current`, with the same reply and source selection as `get_current_glucose`.
- History paging in `get_glucose_history`:
  - `limit` (default 500, at most 5000) and `before` (RFC 3339);
  - replies have `truncated`, `next_before`, and `readings_in_window`;
  - readings in the same second are never split across pages, and coverage always describes the whole window.
- Release archives for x86_64 and ARM64 Linux, Apple silicon and Intel macOS, and x86_64 Windows, each with README, LICENSE, and CHANGELOG, plus `SHA256SUMS`.
- A Claude Desktop bundle (`.mcpb`) for macOS and Windows. It runs `serve` and collects no configuration; credentials still come only from `setup`.
- Documentation: [install](https://github.com/johnohhh1/librelink-mcp-rs/blob/v1.0.0/docs/install.md), [troubleshooting](https://github.com/johnohhh1/librelink-mcp-rs/blob/v1.0.0/docs/troubleshooting.md), [privacy](https://github.com/johnohhh1/librelink-mcp-rs/blob/v1.0.0/docs/privacy.md), and [compatibility](https://github.com/johnohhh1/librelink-mcp-rs/blob/v1.0.0/docs/compatibility.md), including a validation guide.
- `cargo deny` in CI for advisories, licenses, bans, and sources.

### Changed
- Patient selection uses the stored patient ID:
  - With none stored, a single connection is used, and several give `patient_selection_required`.
  - A stored ID that is no longer among the connections gives the new code `patient_selection_changed`, and nothing is read. It latches until restart, so `collect` exits with status 78; it never switches to someone else.
  - A full-name selection saved by an earlier version is converted to the ID the first time it matches exactly one connection.
- `clear-local-data` also removes the stored patient ID.
- `get_glucose_history` accepts up to 2160 hours (90 days) from the local store.
- Error replies leave out `retry_after_seconds` when there is no retry time, instead of sending `null`.
- A panic prints a fixed message with no details unless `LIBRELINK_MCP_DEBUG=1` is set. Release builds abort on panic and strip symbols.

## 0.4.0 — 2026-11-06

### Added
- `import-csv <file> --timezone <IANA zone> [--date-format mdy|dmy] [--db <path>] [--dry-run]` imports a locally saved LibreView CSV export into the collector's database, offline. It reads only the device timestamp, record type, and historic and scan glucose columns. It stores record types 0 and 1 as `libreview_csv_historic` and `libreview_csv_scan`, with `time_basis: csv_local_declared_timezone`. DST-ambiguous and nonexistent local times, rows outside the collector's retention, future rows, and bad values are skipped and counted. Output is counts only. See [Importing a LibreView CSV export](docs/csv-import.md).
- Time-weighted statistics in `get_glucose_stats`, next to the unchanged sample statistics, in a new `time_weighted` object:
  - each reading counts for the time until the next one, at most 15 minutes;
  - `coverage_percent` of the requested window;
  - the consensus bands (`<54`, `54–<70`, `70–180`, `>180–250`, `>250` mg/dL) and time in the configured target range, as percent of covered time;
  - a time-weighted mean, SD, and CV.
- The gap cause `no_data_in_export`, for a gap inside the imported span with no collector polls.

### Changed
- `time_in_range_percent` is now the time-weighted share of covered time in 70–180 mg/dL. It was always `null` before.
- `gmi_percent` is now an estimate (3.31 + 0.02392 × time-weighted mean) when the requested window is at least 14 days and at least 70% covered. Otherwise it stays `null`, with `gmi_unavailable_reason` `window_too_short` or `coverage_too_low`. With the live graph alone it is never available.
- A CSV row within 90 seconds of a collected reading is left out of every store-backed reply.
- `period_status: partial` now means only that the observed span does not cover the requested window: the first reading is more than 15 minutes after the window start, or the last is more than 15 minutes before its end. Interior gaps no longer make a period partial; they stay in `coverage.interior_gaps` and in `coverage_percent`. This applies to stats and trends on both data paths.
- Gap causes are decided in this order: `no_data_from_api`, `no_data_in_export`, `before_collection`, `collector_not_running`. Before, a gap before the first poll was `before_collection` even when the collector had polled inside it.
- `serve` opens the database read-only and never creates, migrates, or writes it. A database from a newer version makes `serve` use the live API.
- `collect` records its retention in the database, and `clear-local-data` refuses while an import holds `<db>.import.lock`.

### Upgrading
- The database schema moves to version 3 the first time 0.4.0's `collect` or `import-csv` opens it. **There is no downgrade path:** 0.3.0 refuses an upgraded database. Stop the collector, install 0.4.0, run `import-csv` or `collect`, then start the collector. See [Upgrading](docs/collector.md#upgrading).

## 0.3.0 — 2026-11-06

### Added
- An opt-in local store: a SQLite database (bundled, WAL mode) holding readings, poll attempts, the collector heartbeat, and the latest sensors in masked form. It holds no credentials, tokens, names, or raw patient or sensor IDs; patients are keyed by a salted SHA-256 with a per-database salt. On Unix the data directory is `0700` and the files are `0600`. See [ADR 0002](docs/adr/0002-local-store-and-collector.md) and the [collector guide](docs/collector.md).
- `collect [--db <path>] [--retention-days <1-3650>]`. It polls at least 60 seconds apart with 0–5 seconds of jitter and honors the server's retry times. It writes a heartbeat at least every 30 seconds, including during cooldowns, and keeps 90 days by default. A lock file allows one collector per database. Latched errors exit with status 78, and Ctrl+C or SIGTERM stops it cleanly. The guide includes a systemd user unit example.
- `serve [--db <path>]` reads every data tool from the store while the collector heartbeat is under 3 minutes old, making no LibreLinkUp requests, and from the live API otherwise.
- A `data_source` field (`local_store` or `live_api`) on every data tool reply.
- From the store, history, stats, and trends cover the requested window up to each tool's maximum, and `period_status` is `sample_summary` when coverage is sufficient. Interior gaps carry a `cause`: `before_collection`, `collector_not_running`, or `no_data_from_api`.
- `clear-local-data --db <path>`. `clear-local-data` also deletes the database with its WAL, shared-memory, and lock files, and refuses while a collector holds the lock.
- Error code `local_store_unavailable`, for when the collector's database cannot be read during a call.

### Changed
- Server instructions explain `data_source`, and that stored data is only as fresh as the collector.
- Live-validated on 2026-11-06 with a temporary database and one US follower account.

## 0.2.0 — 2026-11-05

### Changed
- The server now uses its own LibreLinkUp client (`reqwest` with rustls) instead of the `libre_link_up_api_client` crate. See [ADR 0001](docs/adr/0001-owned-api-client.md).
  - It logs in only when there is no valid session or after a 401, with at most one 401 re-login per read. Throttling, server errors, parse errors, and rejected requests never trigger a login.
  - It follows one login region redirect, and only to a region in the known table.
  - It sends the account ID only as a SHA-256 hash, keeps the password once in zeroizing storage, and never logs or returns response bodies.
  - It sends the same request headers as the crate did.
- Tools read through a `DataSource` trait. Every tool uses one current reading, `glucoseMeasurement`; history, stats, and trends previously appended `glucoseItem`.
- Live-validated on 2026-11-05 against one US follower account.

### Added
- Error codes `client_version_rejected` (403 with status 920; a plain minimum version is included in the message), `request_rejected` (`AccountIdMismatch` / `RequiredHeaderMissing`), and `api_changed` (a successful response that is not JSON; a candidate retirement signal, not proof).
- `client_version_rejected` and `request_rejected` latch until restart, as `authentication_failed` already did, so the server stops sending requests that cannot succeed.
- Login status 4 maps to `account_action_required`. Lockouts and HTTP 429/430 map to `rate_limited`, using the server's retry time clamped to 60–900 seconds.

### Removed
- The `libre_link_up_api_client` dependency.

## 0.1.0

First prototype: the eight original tool names over MCP stdio, OS credential storage, labeled timestamps and freshness, partial statistics, a fetch cache and failure cooldown, and a rejected-credential latch.
