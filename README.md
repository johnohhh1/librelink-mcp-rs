# LibreLink MCP RS 🦀

**A local, privacy-conscious Rust MCP server for LibreLinkUp sharing data.**

[![CI](https://github.com/johnohhh1/librelink-mcp-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/johnohhh1/librelink-mcp-rs/actions/workflows/ci.yml)
[![Rust 1.89+](https://img.shields.io/badge/Rust-1.89%2B-DEA584?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![MCP stdio](https://img.shields.io/badge/MCP-stdio-7053C1)](https://modelcontextprotocol.io/)
[![MIT license](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Status: early prototype](https://img.shields.io/badge/status-early%20prototype-orange)](docs/roadmap.md)

[Quick start](#quick-start) · [Eight tools](#the-eight-tools) · [Data quality](#data-quality) · [Privacy](#privacy-and-local-data) · [Roadmap](docs/roadmap.md)

This independent port preserves the **eight MCP tool names** from [Amandeep Khurana's original LibreLink MCP Server](https://github.com/amansk/librelink-mcp-server). It uses Rust, `rmcp`, and a local OS credential store. Thank you to Amandeep and the original contributors for the project and tool design. Their TypeScript code is not included here.

> [!IMPORTANT]
> Use a **LibreLinkUp follower account**. Enter credentials only in the local `setup` prompt, never in chat or an MCP tool. This project is unofficial, is not affiliated with Abbott, and does not provide medical treatment advice.

## Project snapshot

| | Current state |
| --- | --- |
| **Transport** | Local MCP over stdio |
| **Platforms** | Linux, macOS, and Windows CI builds and tests |
| **Secrets** | OS credential store; no plaintext fallback or MCP password arguments |
| **Readings** | Source timestamps retained; factory UTC interpretation labeled as inferred |
| **History** | About 12 hours from the live graph, or the requested window from the optional local collector; observed span, cadence, and labeled gaps |
| **Analytics** | Sample statistics plus time-weighted coverage, consensus bands, and time in range; GMI as an estimate only for 14+ days at 70%+ coverage |

## Quick start

You need **Rust 1.89 or newer** and an available OS credential store: macOS Keychain, Windows Credential Manager, or a Linux Secret Service session. On Debian or Ubuntu, install `libdbus-1-dev` and `pkg-config` to build the keyring backend. If `cargo` is not on your shell path after installing Rust, run `. "$HOME/.cargo/env"`.

```sh
git clone https://github.com/johnohhh1/librelink-mcp-rs.git
cd librelink-mcp-rs
cargo run -- setup
cargo run -- check
```

`setup` prompts locally for the follower account email, a hidden password, and a region. It accepts `global` (the default), `us`, `eu`, `eu2`, `ae`, `ap`, `au`, `ca`, `de`, `fr`, `jp`, `la`, `ru`, and `cn`. If the account follows multiple people, run `cargo run -- select-patient` and enter the followed person's full name before `check`. An ambiguous account is rejected instead of silently choosing the first connection.

`check` logs in, reads the followed person's graph, and prints a small connectivity report. Since v0.2 the server uses its own LibreLinkUp client instead of the `libre_link_up_api_client` crate; [ADR 0001](docs/adr/0001-owned-api-client.md) explains why. The owned client sends the same request headers as the crate did. It was checked against one live US follower account on 2026-11-05; other accounts, regions, and sensors still need validation.

`check` includes `display_minus_factory_minutes`, so you can inspect the clock relationship locally without sharing a response. The difference reflects the **sharer's** local clock, which may differ from your own timezone. To change the client version after an Abbott version requirement changes, run `cargo run -- set-api-version` locally and restart the MCP server.

### Connect an MCP client

Build the binary and point your MCP client at it:

```sh
cargo build --release
```

```json
{
  "mcpServers": {
    "librelink-rs": {
      "command": "/absolute/path/to/librelink-mcp-rs/target/release/librelink-mcp-rs",
      "args": ["serve"]
    }
  }
}
```

The server reuses its authenticated API client. Restart it after changing credentials or patient selection. Range changes take effect without a restart.

## The eight tools

| Tool | What it currently returns |
| --- | --- |
| `get_current_glucose` | Latest available reading, source time, and explicit freshness |
| `get_glucose_history` | Graph readings plus the current measurement, each labeled with its source; coverage and whether filtering applied |
| `get_glucose_stats` | Descriptive statistics over the returned graph, labeled `partial` when it covers less than the requested days |
| `get_glucose_trends` | Partial-period status, extrema, source arrows, and descriptive adjacent-point rates |
| `get_sensor_info` | Masked serial and device ID (last four characters), interpreted activation epoch, warmup minutes, and product type code |
| `configure_credentials` | Local setup instructions; accepts **no credentials** |
| `configure_ranges` | Validates and saves local target bounds in mg/dL |
| `validate_connection` | Connection and data-availability status without glucose values or secrets |

Tool replies contain MCP `structuredContent` plus a JSON text block for clients that only display text. Errors set `isError: true` and use safe codes including `invalid_input`, `insufficient_data`, `rate_limited`, `timeout`, `session_expired`, `authentication_failed`, `account_action_required`, `client_version_rejected` (Abbott requires a newer client version; run `set-api-version`), `request_rejected` (the API refused this client's request headers; please report it as a bug), `api_changed`, `local_store_unavailable` (the collector's database could not be read), and `upstream_unavailable`. `api_changed` means a successful response was not JSON: the API may have changed or been retired, but it is not proof of either. After `authentication_failed`, `client_version_rejected`, or `request_rejected`, the server makes no further LibreLinkUp requests until it is restarted, so a wrong password cannot drive the account into an Abbott lockout. The client logs in only when it has no valid session or after a 401, and never because of throttling or other errors. Rate-limit replies include `retry_after_seconds`. Upstream response bodies and tokens are never included in error messages.

## Data quality

The API can return timestamps without timezone offsets. When both clock strings parse, their gap is quarter-hour aligned and within ±14 hours, and the factory clock is not in the future, the server interprets `FactoryTimestamp` as UTC and labels `time_basis: factory_utc_inferred`. This follows the [Rust client's UTC interpretation](https://github.com/ItsXrgon/libre-link-up-api-client/blob/main/src/utils.rs) and [public paired-clock examples](https://github.com/NightscoutFoundation/xDrip/discussions/2118), but the API does not attach an offset. A missing, inconsistent, or future factory time keeps freshness `unknown`. Retrieval time does **not** turn a reading into a real-time reading.

When every graph point has a usable offset-aware or inferred UTC time, history, stats, and trends filter to the requested window. Coverage reports the first and last observations, span, estimated cadence and expected point count, actual count, and detected gaps. These estimates come from returned intervals. Because the LibreLinkUp graph covers about 12 hours, statistics and trends for a requested day or longer return `period_status: partial` with the observed span in `coverage`; they describe the returned graph, not the requested days. Statistics return `insufficient_data` only when no valid values were returned. `window_verified` remains false: the API cannot prove complete coverage. Sample percentages count each reading once and are not time in range. `get_glucose_stats` also reports `time_weighted` figures: each reading stands for the time until the next one, at most 15 minutes, so gaps are uncovered rather than bridged or counted as zero. They include `coverage_percent` of the requested window, the consensus bands (very low `<54`, low `54–<70`, target `70–180`, high `>180–250`, very high `>250` mg/dL, as percent of covered time), time in the configured target range, and a time-weighted mean, SD, and CV. `time_in_range_percent` is the time-weighted 70–180 value. `gmi_percent` is an estimate (3.31 + 0.02392 × time-weighted mean), given only for a requested window of at least 14 days with at least 70% coverage, following the [international consensus on CGM time in range](https://pmc.ncbi.nlm.nih.gov/articles/PMC6973648/); otherwise it is `null` with `gmi_unavailable_reason` `window_too_short` or `coverage_too_low`. With the live graph alone, GMI is never available. Clinical-pattern fields remain unavailable. See the [architecture and parity notes](docs/architecture.md) and [release roadmap](docs/roadmap.md).

## Keeping longer history (optional)

The LibreLinkUp graph covers only about 12 hours. The opt-in `collect` command polls about once a minute (never more often than every 60 seconds) and keeps readings in a local SQLite database for 90 days by default:

```sh
cargo run -- collect
```

It runs in the foreground until Ctrl+C, allows one collector per database, and stops with exit status 78 on errors a retry cannot fix. While a collector is running (its heartbeat is under 3 minutes old), `serve` answers every data tool from the database and makes no LibreLinkUp requests; otherwise it uses the live API. Every data reply says which in `data_source` (`local_store` or `live_api`), and stored data is only as fresh as the collector. Both commands accept `--db <path>`. See the [collector guide](docs/collector.md) for options, what is and isn't stored, and a systemd user service example.

To add history from before the collector started, `import-csv <file> --timezone <IANA zone>` loads a LibreView CSV export into the same database, offline. See [Importing a LibreView CSV export](docs/csv-import.md). When the export and collected data overlap, the collected readings win, so nothing is counted twice.

Upgrading from 0.3: stop the collector, install the new version, run `import-csv` or `collect` (either one upgrades the database), then start the collector. `serve` never upgrades or writes the database, and an upgraded database cannot be used by 0.3.0. See [Upgrading](docs/collector.md#upgrading).

## Privacy and local data

- Credentials live in the OS credential store. There is no plaintext file fallback, command-line password flag, or MCP secret input.
- `serve` does not persist glucose history. The opt-in `collect` command stores readings in a local SQLite database: values and timestamps keyed by a salted patient pseudonym, with no credentials, tokens, names, or raw patient IDs. Nothing is uploaded. See [what the collector stores](docs/collector.md#privacy).
- Successful graph reads are cached in memory for 60 seconds. Detected rate limits and account lockouts trigger a cooldown, using the server's retry time within 60 to 900 seconds; other upstream failures also pause calls for 60 seconds. Cached readings keep their original retrieval time.
- `cargo run -- clear-local-data [--db <path>]` removes this project's credential and preference entries from the OS store, and the collector's database. It refuses while a collector holds that database's lock. Stop the server first; run `setup` again to reconnect.
- The `.gitignore` excludes local configuration, data exports, databases, logs, and key files. Tests use synthetic fixtures only.

On headless Linux systems without Secret Service, `setup` fails closed. If you used the original server, its `~/.librelink-mcp/config.json` may contain a plaintext password; this port does not read, migrate, or delete that file.

## Build and contribute

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CI runs those checks on Linux, macOS, and Windows. A synthetic stdio test initializes the server, lists all eight tools, checks structured output, and confirms that a password argument is rejected without echoing it. Please keep real account responses, glucose readings, credentials, and tokens out of issues and test fixtures. Start with the [roadmap](docs/roadmap.md) and [architecture notes](docs/architecture.md).

## License

MIT — see [LICENSE](LICENSE).
