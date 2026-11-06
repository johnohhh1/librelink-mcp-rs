# Architecture and parity plan

## Findings from the two upstream projects

The original [TypeScript server](https://github.com/amansk/librelink-mcp-server) exposes exactly eight tools. Its `src/config.ts` writes the account password into `~/.librelink-mcp/config.json`. Its `src/librelink-client.ts` synthesizes sensor type, serial number, and activation time. Its `src/glucose-analytics.ts` infers dawn and meal phenomena without supporting context; missing overnight data can yield a zero variability score. Its credential tool accepts a password in MCP arguments. Those behaviors must not be ported.

The [Rust API client](https://github.com/ItsXrgon/libre-link-up-api-client) has a published 0.3.3 API with `ClientConfig::new`, `LibreLinkUpClient::new`, `read_raw`, and `read`. Its README still shows dependency version 0.2.1. The 0.3.3 source handles login redirects and region mapping, but its error says credentials must be for **LibreLinkUp**, not a primary **LibreLink** account. The [original server's Node API dependency](https://github.com/DRFR0ST/libre-link-unofficial-api) also requires LibreLinkUp credentials, despite the original server README's wording. The original server stores a region preference but does not pass it to its Node client constructor. `read_raw` requires a followed patient and defaults to the first connection. The processed `read` maps unparseable timestamps to `Utc::now()` and missing or unknown trend arrows to `Flat`; the server uses raw measurements. Its `authenticated_request` retries after any request error, including throttling. Its `sensor.a` model comment says “age days” although observed values are epoch seconds. The crate's complete raw models also require many fields, so a successful login does not yet prove it can decode every account's graph response.

A local user check confirmed that the pinned crate can connect to a LibreLinkUp follower account and return values matching the phone app. That does not verify every account, region, or source timestamp interpretation. No credentials are needed in this repository or chat.

### Factory clock validation

On 2026-11-05 the factory-UTC inference was checked against a live US follower account. The sharer's phone reported time zone America/Detroit (EST, UTC−5). `check` returned `display_minus_factory_minutes: -300`, the display timestamp matched the phone's local wall clock, and `FactoryTimestamp` read as UTC matched retrieval time to within a minute. Together with public paired-clock captures from a UTC+2 (summer) account, this supports `Timestamp` as the sharer's local wall clock and `FactoryTimestamp` as UTC. No reading values or account details were recorded. Other regions and sensors should still be checked.

## Components

1. **Local configuration CLI:** `setup` stores credentials in the OS credential store. Non-secret preferences such as target range and selected patient can later use a validated, atomic local config file. The MCP interface never receives credentials.
2. **API adapter:** tools read only through the `DataSource` trait (`src/source.rs`), which returns one current reading (from `connection.glucoseMeasurement`), graph samples, and active sensors as project-owned types. `Fetcher` (`src/fetch.rs`) adds the 60-second cache, failure cooldown, and rejected-credential latch in front of any source. Two implementations exist. `borrowed_client.rs` wraps the pinned crate and is the one in use. `llu_client.rs` is the owned `reqwest` client (rustls TLS) with a configurable base URL. It logs in only when there is no token, the token has expired, or a request returns 401, at most once per read for a 401. It follows one login region redirect, sends the account ID only as a SHA-256 hash, and keeps the password in zeroizing storage. It is tested against `wiremock` and replaces the crate in v0.2 step 3; see [ADR 0001](adr/0001-owned-api-client.md). Both preserve source timestamps and trend unknowns, and map errors to safe public codes without raw server response bodies.
3. **Data quality:** represent `observed`, `estimated`, `missing`, and `stale` separately. Paired offset-free clocks may support a labeled factory-UTC inference; inconsistent or missing clocks keep unknown freshness. Caching preserves source and retrieval times separately, so a cache hit cannot make an old reading current.
4. **Analytics:** use descriptive summaries only. Reject unsupported windows, expose observed coverage and sample count, and return unavailable fields when data are insufficient. No meal or dawn diagnosis without the needed context. GMI, if offered, is an estimate with its input window and limitations.
5. **MCP transport:** `rmcp` on stdio with `tokio`. Tool handlers return typed, serialized results and concise errors. `tracing` goes to stderr with secret and personal-data redaction. SQLite is optional for explicitly enabled local historical storage, never for passwords or tokens.

## Original tool names

| Tool | Planned safe behavior |
| --- | --- |
| `get_current_glucose` | Return latest *available* reading, source time, retrieval time, and freshness; never imply a stale value is live. |
| `get_glucose_history` | Return observed readings within requested hours and actual coverage; do not pretend the API supplied the whole requested period. |
| `get_glucose_stats` | Return descriptive statistics with sample count and coverage. Mark GMI estimated or unavailable when the window is inadequate. |
| `get_glucose_trends` | Return descriptive changes and unavailable fields with reasons; no inferred meals, diagnoses, or treatment suggestions. |
| `get_sensor_info` | Return source identifiers, an interpreted activation epoch, warmup minutes, and product type code. Unknown device type and lifetime remain missing. |
| `configure_credentials` | Preserve the name but accept **no secret arguments**. Report local `setup` instructions and credential-store status only. |
| `configure_ranges` | Validate ordered numeric bounds and save non-secret range preferences. |
| `validate_connection` | Perform a safe API read and return connection/data status without credential or raw response leakage. |

## Current parity status

The first `rmcp` stdio server exposes all eight original tool names. `configure_credentials` has no secret input. Successful tool replies carry structured JSON and a matching text block; errors carry safe public codes. `get_current_glucose`, `get_sensor_info`, and `validate_connection` use the graph response. `configure_ranges` persists bounds locally, and `set-api-version` changes the validated local API version. The time model keeps source text, factory text, retrieval time, interpreted UTC time, and freshness separate. Factory UTC is explicitly inferred only after paired-clock consistency checks; missing, inconsistent, and future clocks keep unknown freshness. Graph responses are cached in memory for 60 seconds. Detected throttling and other upstream failures pause further calls; the crate can still retry during the first failed request. History reports observed coverage and estimated cadence and gaps. Statistics over a graph shorter than the requested period are returned with `period_status: partial` and coverage; `insufficient_data` means no valid values. A rejected password latches `authentication_failed` until restart, with no further login attempts. Trends report descriptive adjacent-point rates, and partial periods are labeled. Time-weighted range, GMI, and clinical-pattern fields remain unavailable. A local opt-in collector and an owned API adapter are still required for full parity.

## Test gates

- Synthetic fixtures for stale, missing, timezone-free, ambiguous daylight-saving, conflicting, and future timestamps; unknown trend arrows; mixed-offset and boundary-crossing graph windows; short graphs that yield `partial` statistics; and graphs with no valid values that yield `insufficient_data`. Sparse history, missing sensor metadata, and invalid-range cases remain release gates.
- MCP tool-list parity and stdio handshake tests for all eight names, structured output, and a schema and call test proving `configure_credentials` rejects password input without echoing it.
- API adapter tests with synthetic responses and sanitized error mapping. A local manual account check is separate and never recorded as a fixture.
- CI on Linux, macOS, and Windows: `cargo fmt --check`, clippy with warnings denied, and tests.
