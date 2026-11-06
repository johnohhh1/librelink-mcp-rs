# Roadmap

The server connects to a LibreLinkUp follower account, exposes the eight original tool names over MCP stdio, and labels what it cannot know. The LibreLinkUp graph covers about 12 hours, so multi-day history and standardized analytics come from the optional local collector and LibreView CSV import. This document records what was done in each milestone and what remains.

Each milestone has a gate. A milestone is not done until its gate passes on Linux, macOS, and Windows CI.

## Done (v0.1)

- **Eight tools over MCP stdio.** `configure_credentials` accepts no secret arguments. Replies carry `structuredContent` and a JSON text block; errors carry safe public codes. A synthetic stdio test checks the handshake, the tool list, structured output, and secret rejection.
- **Server identity and instructions.** The server reports its name and version, and its instructions explain freshness, `factory_utc_inferred`, `partial` periods, and the no-advice boundary.
- **Time contract.** Source timestamp text, factory text, retrieval time, interpreted UTC time, time basis, and freshness are kept separate. Malformed, missing, future, conflicting, DST-ambiguous, and cached-then-aged timestamps have synthetic tests. A bad timestamp is never replaced with fetch time.
- **Factory-UTC inference with live validation.** `FactoryTimestamp` is read as UTC only when both clocks parse, their gap is quarter-hour aligned and within ±14 hours, and the factory time is not in the future. The basis is labeled `factory_utc_inferred`. One live US follower account confirmed the interpretation on 2026-11-05; see [architecture notes](architecture.md#factory-clock-validation).
- **Fetch cache and failure cooldown.** Successful reads are cached in memory for 60 seconds with their original retrieval time. Rate limits and lockouts cool down for the upstream-indicated time; other failures pause calls for 60 seconds.
- **Rejected-credential latch.** After `authentication_failed`, the server makes no further login attempts until it restarts.
- **Window filtering and coverage.** `hours`, `days`, and `period` filter graph points when every point has a usable UTC time. Coverage reports first and last observations, span, estimated cadence and expected points, gaps, and `window_verified: false`.
- **Partial statistics.** A graph shorter than the requested period returns statistics labeled `period_status: partial` with the observed span in `coverage`. `insufficient_data` means no valid values were returned. Sample percentages are not called time in range; GMI, time-weighted range, and meal or dawn fields are `null`.
- **Current reading appended to history.** The graph lags the live reading, so history, stats, and trends append the current measurement when the graph lacks it, labeled `source: current_measurement`. History points carry no per-point freshness.
- **Masked sensor IDs.** Serial numbers and device IDs are reduced to their last four characters. Activation time is shown only when the source epoch is plausible and not in the future.
- **Configurable API version.** `set-api-version` stores a validated three-part version locally.
- **D-Bus session fallback.** On Linux, a missing `DBUS_SESSION_BUS_ADDRESS` falls back to the user's own `/run/user/<uid>/bus` socket, so MCP clients started outside a desktop session can reach Secret Service.
- **Local data deletion.** `clear-local-data` removes this project's credential and preference entries.
- **CI.** Formatting, Clippy with warnings denied, and tests on Linux, macOS, and Windows, plus an MSRV job that runs the tests on Rust 1.89.

## v0.2: owned API client behind a `DataSource` trait

**Status:** done; released as 0.2.0. The crate is removed and the owned client passed a live smoke test on 2026-11-05.

The borrowed `libre_link_up_api_client` crate retries login after any request error, including throttling. Its base URL is fixed, so it cannot be tested against a mock server. It keeps its own copy of the password, and its sensor model mislabels epoch fields. v0.2 replaces it. This is a prerequisite for any polling in v0.3.

1. **Adapter seam. Done.** Record the decision in `docs/adr/0001-owned-api-client.md`. Introduce a `DataSource` trait that returns one current reading, graph items, and active sensors. Move all crate use behind one implementation. Tools and the fetch cache and cooldown depend only on the trait. A fake `DataSource` test proves a tool runs without the network.
   - The trait defines exactly **one** current reading, taken from `connection.glucose_measurement`, the field `get_current_glucose` already uses. History, stats, and trends append that same reading. A test proves the point history appends equals what `get_current_glucose` reports. This is the only intended behavior change in this step.
2. **Owned `reqwest` client. Done.** Configurable base URL, region redirect handling, bounded session refresh, and no blind login retry after 429/430. Credentials are read once and not duplicated into long-lived strings. `wiremock` tests with synthetic bodies cover redirect, lockout, status 2 and 4 account actions, 401, 429/430, 403/920 minimum version, and account-ID mismatch, plus missing fields and unknown trend codes.
   - Done as two parts: session handling, then error mapping. Status 2 maps to `authentication_failed`, status 4 to `account_action_required`, a lockout body and HTTP 429/430 to `rate_limited` (retry time clamped to 60–900 seconds), 403 with status 920 to `client_version_rejected`, `AccountIdMismatch` / `RequiredHeaderMissing` to `request_rejected`, 5xx to `upstream_unavailable`, and a successful non-JSON body to `api_changed`. `authentication_failed`, `client_version_rejected`, and `request_rejected` latch until restart.
3. **Switch over and remove the crate. Done.** `check` and `serve` use the owned client, with the same request headers the crate sent. `api_changed` is the candidate retirement signal; `api_retired` stays reserved for a confirmed retirement (see [API retirement risk](#api-retirement-risk)). Patient selection keeps its current name-based behavior.
4. **Live validation. Done 2026-11-05.** A manual smoke test against one US follower account, with no values or account details recorded. It checked login, patient selection, all six data tools, factory-UTC inference, the current reading appended to history, masked sensor IDs, and that one upstream fetch served six tool calls from the cache. No errors. Other regions and accounts remain a v1.0 item.

**Gate:** no crate dependency remains; every upstream error path has a synthetic HTTP fixture; no login is caused by throttling, server errors, or parse errors, and a 401 causes at most one re-login per read; tool output is unchanged except for the single current reading and the new error codes.

## v0.3: opt-in `collect` command and SQLite

**Status:** done; released as 0.3.0 and live-validated 2026-11-06.

The graph endpoint takes no date range, so it cannot supply 7-, 30-, or 90-day history on its own. See [Nightscout Connect's LibreLinkUp notes](https://github.com/nightscout/nightscout-connect/blob/main/README.md).

The design is fixed in [ADR 0002](adr/0002-local-store-and-collector.md). In short:

- `collect` is a separate foreground command that polls through the v0.2 client and writes to a local SQLite file (`rusqlite`, bundled, WAL). The interval has a 60-second floor enforced in code, plus 0–5 seconds of jitter. `serve` uses the store while the collector heartbeat is under 3 minutes old and the live API otherwise, and every data reply states `data_source`. While a collector runs, only one process talks to Abbott.
- No credentials, tokens, or raw patient IDs in SQLite; records are keyed by a salted SHA-256 of the patient ID, with a per-database salt. The directory is `0700` and the file `0600` on Unix. Readings deduplicate on factory timestamp, and every poll attempt is recorded so collector downtime and API gaps can be told apart.
- Retention defaults to 90 days (1–3650); `clear-local-data` also removes the database and its WAL and lock files.
- Throttling and lockouts use the v0.2 client's cooldowns. A latched error stops the collector with exit status 78 instead of retrying, and the example systemd unit does not restart on that status. A lock file allows only one collector per database.

Steps:

0. **CI fix and design record. Done.** Bump `actions/checkout`; write ADR 0002.
1. **Store module. Done.** Schema and migrations, permissions, deduplicating inserts, poll records, heartbeat, retention, and deletion, all tested with synthetic data.
2. **`collect` command. Done.** See the [collector guide](collector.md). Poll loop with floor and jitter, cooldowns, a heartbeat at least every 30 seconds, exit status 78 for latched errors, the lock file, clean shutdown, and a systemd user unit example.
3. **`serve` source selection and tool reads. Done.** `serve [--db]` uses the store while the collector heartbeat is under 3 minutes old, with no upstream requests, and the live API otherwise. Every data reply states `data_source`. Store-backed history, stats, and trends cover the requested window and label interior gaps `before_collection`, `collector_not_running`, or `no_data_from_api`; sensors are stored masked.
4. **Live validation. Done 2026-11-06.** A manual check with a temporary database and one US follower account, with no values or account details recorded. It checked that `collect` polled about 60 seconds apart and deduplicated (one backfill, then one new reading per poll); that the database, WAL, shared-memory, and lock files were all mode `0600`; that all six data tools reported `local_store`; that `serve` opened no network connections (traced) and caused no extra collector polls; and that SIGTERM stopped the collector cleanly and released the lock.

**Gate:** `get_glucose_history(hours)` returns stored readings for the requested window with source labels, real first and last observations, expected and observed samples, and gaps. A window the database does not cover returns `partial` coverage, never a claim of completeness. Statistics over a short window stay labeled `partial`; `insufficient_data` still means no valid values.

## v0.4: LibreView CSV import and defensible analytics

**Status:** done; released as 0.4.0 and live-validated 2026-11-06. The design is fixed in [ADR 0003](adr/0003-time-weighted-metrics-and-csv-import.md).

- Import a locally saved LibreView CSV export into the same database, labeled with origin `libreview_csv_historic` or `libreview_csv_scan`, and deduplicated against collected rows. Import is offline and uploads nothing.
- Time-weighted time in range, kept separate from sample percentages. Irregular samples are weighted by duration, gaps are excluded rather than counted as zero, and long gaps are not bridged.
- GMI reported as an estimate only when the window has at least 14 days and at least 70% data coverage, following the [international CGM time-in-range consensus](https://pmc.ncbi.nlm.nih.gov/articles/PMC6973648/). Otherwise `gmi_percent` stays `null` with a reason. This is a data-quality gate, not a diagnosis.
- Every summary includes sample count, coverage, data source, and the target bounds used. Meal, dawn, and overnight fields stay `null` unless validated context exists.

Steps:

0. **Design record. Done.** Write ADR 0003.
1. **Metrics module, wired into stats. Done.** Duration-weighted samples capped at 15 minutes, `coverage_percent`, the fixed consensus bands and the configured target range, time-weighted mean, SD, and CV next to the sample statistics, and GMI gated on 14 days and 70% coverage. One implementation for the live and store paths.
2. **CSV parser and `import-csv`. Done.** See [Importing a LibreView CSV export](csv-import.md). `import-csv <file> --timezone <IANA> [--date-format mdy|dmy] [--db <path>]` reads only the timestamp, record type, and glucose columns. DST-ambiguous or nonexistent times are skipped and counted. Rows attach to the collector's patient key, and the output is counts only. Tested on synthetic fixtures.
3. **Overlap, docs, and release. Done.**
   - A CSV row within ±90 seconds of a collected reading is dropped from every store read, in either import order.
   - Gaps inside the imported span with no polls are labeled `no_data_in_export`.
   - `serve` opens the database read-only and never migrates it.
   - The docs include an upgrade note.
   - `period_status: partial` means only that the observed span does not reach both ends of the window; interior gaps are reported in coverage instead.
4. **Live validation. Done 2026-11-06.** A manual check on the PM's real setup, with no values or account details recorded, and the real export kept out of the repo.
   - **Upgrade:** the procedure ran as documented: stop the collector, back up, install, `import-csv`, start the collector. The import migrated the database to schema 3, and the collector polled normally on it afterward.
   - **Import:** the counts matched the dry run and an independent check. A re-import was all duplicates.
   - **`serve`:** it opened no network connections and answered every tool from `local_store`.
   - **14-day stats against the vendor's AGP report for the same data:**
     - within 1 percentage point on every consensus band;
     - within 1 mg/dL on the mean;
     - within 0.1 on CV;
     - within 0.5 percentage point on coverage.
   - **GMI:** it differed by 0.07, traced to a 0.6 mg/dL difference in the vendor's mean. An independent recomputation matched this project's value.

**Gate:** no metric treats missing values as zero, irregular samples as equal durations, or a short window as the full requested period. Tests cover the 14-day and 70% boundaries.

## v1.0: release

**Status:** done, pending the PM's live check. Version 1.0.0 is a release candidate until it is tagged. Its steps:

1. **Stable patient selection. Done.** `select-patient` lists the account's connections in a terminal and stores the chosen patient ID in the OS credential store, never printed. With no ID stored, a single connection is used and several give `patient_selection_required`. A stored ID that is gone gives `patient_selection_changed`, which latches and never falls back to someone else. A legacy name selection is converted once.
2. **MCP surface. Done.**
   - Annotations on all eight tools, and typed replies with a published `outputSchema` that tests check against real replies on both data paths.
   - The resource `glucose://current`.
   - History paging (`limit`, `before`, `truncated`, `next_before`), with coverage over the whole window and store windows up to 90 days.
3. **Leak and panic audit. Done.**
   - A panic prints a fixed message unless `LIBRELINK_MCP_DEBUG=1`, and release builds abort on panic and strip symbols.
   - Marker tests drive the command-line error paths, the client's failure responses, the collector's logs, and the import.
   - The only network destinations are LibreLinkUp's API hosts. There is no telemetry.
4. **Supply-chain gate. Done.** `cargo deny` in CI, with `deny.toml` covering advisories, licenses, bans, and sources. Its one license exception and two duplicate skips are justified there.
5. **Release automation. Done.** Tagged releases build five targets with checksums and CHANGELOG notes. macOS and Windows also get a Claude Desktop bundle (`.mcpb`, manifest 0.3), which collects no configuration. Pull requests build everything without publishing.
6. **Documentation. Done.** [Install](install.md), [troubleshooting](troubleshooting.md), [privacy](privacy.md), and [compatibility](compatibility.md).
7. **Release candidate and live check.** Version 1.0.0. Remaining: the PM's live check on the real setup before tagging.

**Release bar (PM decision):** v1.0 is validated on the configuration in [compatibility](compatibility.md): one US follower account, a FreeStyle Libre 3, and the America/Detroit time zone, on 2026-11-05 and 2026-11-06. Other accounts, regions, sensors, hosts, and clients are documented as unvalidated rather than claimed. All eight tools have honest parameter semantics or explicit unavailable results, every gate above passes, and the public docs state the remaining API limitations.

After 1.0:
- validate other accounts, regions, and sensors through the [validation guide](compatibility.md#validating-another-configuration), including factory-UTC inference outside the US, without recording anyone's responses;
- confirm the exact trigger for `api_retired`.

## API retirement risk

Abbott's LibreLinkUp 5.x API encrypts its responses. The v4 plain-JSON endpoints this project uses may be shut off.

- **Detect and report.** Since v0.2 the owned client reports `api_changed` when a successful response is not JSON, and `client_version_rejected` when Abbott requires a newer version. Both are candidate signals, not proof. `api_retired` is reserved for a confirmed retirement: for example, a minimum-version rejection that no v4 version satisfies, or responses that stay non-JSON across versions. Its exact trigger will be pinned with synthetic fixtures once real behavior is observed.
- **No circumvention.** The project will not decrypt, emulate, or otherwise work around v5 encryption.
- **Stay useful afterward.** Data already collected in SQLite (v0.3) and LibreView CSV import (v0.4) do not depend on the live API. After retirement, history, statistics, and trends keep working from local data, while live-only tools report `api_retired` once retirement is confirmed.
