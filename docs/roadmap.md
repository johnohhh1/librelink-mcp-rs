# Roadmap to a reliable release

The current server proves connectivity and MCP interoperability. It exposes the eight original tool names but is not yet feature complete: `hours`, `days`, and `period` are accepted while the returned graph may cover a shorter, unknown window. The tool responses say so. This document defines the release gates for claiming parity.

## 1. Establish a trustworthy time and data contract

- Keep source timestamp text, fetch time, unit, trend code, sensor identifiers, and provenance separate in a typed reading model.
- Determine the relationship between `FactoryTimestamp` and `Timestamp` with synthetic fixtures and local checks across time zones and daylight-saving transitions. Do not convert an offset-free timestamp to UTC without evidence.
- Mark value state (`observed`, `missing`, `estimated`) and freshness (`recent`, `stale`, `unknown`, `missing`) independently. Never replace a bad timestamp with fetch time.
- Reject impossible values, deduplicate overlapping graph responses, and retain gaps explicitly.

**Gate:** `get_current_glucose` never calls an unknown or stale reading real-time. Tests cover malformed, missing, future, delayed, and daylight-saving timestamps.

## 2. Make historical windows real

The LibreLinkUp graph endpoint does not accept a requested date range, so it cannot on its own guarantee 7-, 30-, or 90-day history. See [Nightscout Connect's LibreLinkUp integration notes](https://github.com/nightscout/nightscout-connect/blob/main/README.md). Add an **opt-in** local SQLite collector or a verified import source. Store no credentials or tokens in SQLite. Scope records to one selected patient, set restrictive file permissions, allow deletion and retention controls, and record collection gaps. Poll conservatively and handle 429/backoff without repeated login storms.

**Gate:** `get_glucose_history(hours)` filters to the requested window when timestamps are trusted and reports actual first/last observations, expected and observed samples, gaps, and source. If a window is unavailable, return `insufficient_data` instead of pretending the graph covered it.

## 3. Build defensible analytics

- Make `days` and `period` affect the data selection, with tests at window boundaries and during sparse coverage.
- Separate percentages of returned samples from time-weighted time in range. Include sample count, coverage, data source, and the chosen target bounds with every summary.
- Report GMI as an estimate only with an eligible data window. The [international CGM time-in-range consensus](https://pmc.ncbi.nlm.nih.gov/articles/PMC6973648/) recommends 14 days and at least 70% data availability for a representative standardized report; this is a useful default quality gate, not a diagnosis.
- Remove meal and dawn labels unless corresponding context and validated methods exist. Keep descriptive patterns and avoid treatment recommendations.

**Gate:** no metric silently treats missing values as zero, irregular samples as equal durations, or a short graph as a full requested period.

## 4. Harden the API and privacy boundary

- Keep the eight tool names. `configure_credentials` must continue to reject secret arguments. Test the serialized MCP schemas and real stdio calls.
- Replace first-person selection by a local, stable-ID chooser that refuses ambiguous or changed connections. Never expose patient IDs through setup prompts or logs unless the user explicitly requests local display.
- Wrap the unofficial API client behind an adapter. Add synthetic HTTP fixtures for redirects, expired tokens, account actions, lockouts, 429s, missing fields, changed trend codes, and sensor metadata conflicts. Contribute upstream fixes when practical.
- Audit error paths, panic paths, traces, and debug output for passwords, tokens, account identifiers, and personal readings. Keep telemetry off by default.

**Gate:** no secrets in MCP inputs, stdout logs, test fixtures, or repository history; errors are actionable without echoing upstream response bodies.

## 5. Ship and maintain it across platforms

- Use typed structured MCP results and documented error codes; cap or paginate large history responses.
- Keep Linux, macOS, and Windows CI green for formatting, Clippy, tests, and a stdio handshake/tool-list smoke test.
- Add installation, keyring troubleshooting, data deletion, privacy, and compatibility docs. Publish versioned binaries and a changelog only after the release gates pass.
- Validate manually on multiple account/region/sensor combinations without collecting anyone's private responses in issues or fixtures.

**Release bar:** all eight tools have honest parameter semantics or explicit unavailable results, all privacy and data-quality gates pass, and the public docs state the remaining API limitations.
