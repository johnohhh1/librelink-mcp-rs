# Roadmap

The server connects to a LibreLinkUp follower account, exposes the eight original tool names over MCP stdio, and labels what it cannot know. It is not yet at parity: the LibreLinkUp graph covers about 12 hours, so multi-day history and standardized analytics need local data. This document records what is done and the versioned milestones that remain.

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

The borrowed `libre_link_up_api_client` crate retries login after any request error, including throttling. Its base URL is fixed, so it cannot be tested against a mock server. It keeps its own copy of the password, and its sensor model mislabels epoch fields. v0.2 replaces it. This is a prerequisite for any polling in v0.3.

1. **Adapter seam.** Record the decision in `docs/adr/0001-owned-api-client.md`. Introduce a `DataSource` trait that returns one current reading, graph items, and active sensors. Move all crate use behind one implementation. Tools and the fetch cache and cooldown depend only on the trait. A fake `DataSource` test proves a tool runs without the network.
   - The trait defines exactly **one** current reading, taken from `connection.glucose_measurement`, the field `get_current_glucose` already uses. History, stats, and trends append that same reading. A test proves the point history appends equals what `get_current_glucose` reports. This is the only intended behavior change in this step.
2. **Owned `reqwest` client.** Configurable base URL, region redirect handling, bounded session refresh, and no blind login retry after 429/430. Credentials are read once and not duplicated into long-lived strings. `wiremock` tests with synthetic bodies cover redirect, lockout, status 2 and 4 account actions, 401, 429/430, 403/920 minimum version, and account-ID mismatch, plus missing fields and unknown trend codes.
3. **Switch over and remove the crate.** Map structured status to `client_version_rejected` and `api_retired` (see [API retirement risk](#api-retirement-risk)) as distinct from `upstream_unavailable`. Patient selection keeps its current name-based behavior.

**Gate:** no crate dependency remains; every upstream error path has a synthetic HTTP fixture; one failed request causes at most one login attempt; tool output is unchanged except for the single current reading and the new error codes.

## v0.3: opt-in `collect` command and SQLite

The graph endpoint takes no date range, so it cannot supply 7-, 30-, or 90-day history on its own. See [Nightscout Connect's LibreLinkUp notes](https://github.com/nightscout/nightscout-connect/blob/main/README.md).

- A separate, explicitly started `collect` command polls through the v0.2 client and writes to a local SQLite file. The poll interval defaults to 60 seconds, and 60 seconds is a minimum enforced in code that configuration cannot lower. Each cycle adds a small random jitter of a few seconds. `serve` never polls on its own and reads history from the database when one exists.
- No credentials or tokens in SQLite. Scope records to one selected patient. Restrictive file permissions. Deduplicate overlapping graph responses by source timestamp, keep source and time basis per row, and record collection gaps explicitly.
- Retention controls; `clear-local-data` also removes the database.
- Throttling and lockout handling come from the v0.2 client; the collector backs off and never causes login storms.

**Gate:** `get_glucose_history(hours)` returns stored readings for the requested window with source labels, real first and last observations, expected and observed samples, and gaps. A window the database does not cover returns `partial` coverage, never a claim of completeness. Statistics over a short window stay labeled `partial`; `insufficient_data` still means no valid values.

## v0.4: LibreView CSV import and defensible analytics

- Import a locally saved LibreView CSV export into the same database, with `source: libreview_csv` provenance and deduplication against collected rows. Import is offline and uploads nothing.
- Time-weighted time in range, kept separate from sample percentages. Irregular samples are weighted by duration, gaps are excluded rather than counted as zero, and long gaps are not bridged.
- GMI reported as an estimate only when the window has at least 14 days and at least 70% data coverage, following the [international CGM time-in-range consensus](https://pmc.ncbi.nlm.nih.gov/articles/PMC6973648/). Otherwise `gmi_percent` stays `null` with a reason. This is a data-quality gate, not a diagnosis.
- Every summary includes sample count, coverage, data source, and the target bounds used. Meal, dawn, and overnight fields stay `null` unless validated context exists.

**Gate:** no metric treats missing values as zero, irregular samples as equal durations, or a short window as the full requested period. Tests cover the 14-day and 70% boundaries.

## v1.0: release

- Replace name-based patient selection with a local, stable-ID chooser that refuses ambiguous or changed connections without exposing patient IDs.
- MCP tool annotations and typed output schemas for all eight tools. A current-reading resource. Cap or paginate large history responses.
- Error paths, panics, traces, and debug output audited for passwords, tokens, account identifiers, and readings. Telemetry stays off.
- Dependency advisory and license gate in CI.
- Versioned release binaries, a changelog, and a Claude Desktop bundle.
- Installation, keyring troubleshooting, data deletion, privacy, and compatibility docs.
- Manual validation on multiple account, region, and sensor combinations, including factory-UTC inference outside the US, without recording anyone's responses in issues or fixtures.

**Release bar:** all eight tools have honest parameter semantics or explicit unavailable results, every gate above passes, and the public docs state the remaining API limitations.

## API retirement risk

Abbott's LibreLinkUp 5.x API encrypts its responses. The v4 plain-JSON endpoints this project uses may be shut off.

- **Detect and report.** The owned client in v0.2 will recognize retirement signals and return `api_retired` instead of a generic upstream failure. The exact signals will be pinned with synthetic fixtures once observed; candidates are a minimum-version rejection that no v4 version satisfies and responses that are no longer plain JSON.
- **No circumvention.** The project will not decrypt, emulate, or otherwise work around v5 encryption.
- **Stay useful afterward.** Data already collected in SQLite (v0.3) and LibreView CSV import (v0.4) do not depend on the live API. After retirement, history, statistics, and trends keep working from local data, while live-only tools report `api_retired`.
