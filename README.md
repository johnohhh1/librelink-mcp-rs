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
| **History** | Graph points, observed span, estimated cadence, and gaps; API completeness remains unverified |
| **Analytics** | Statistics over a graph shorter than requested are labeled `partial`; GMI and time-weighted time in range unavailable |

## Quick start

You need **Rust 1.89 or newer** and an available OS credential store: macOS Keychain, Windows Credential Manager, or a Linux Secret Service session. On Debian or Ubuntu, install `libdbus-1-dev` and `pkg-config` to build the keyring backend. If `cargo` is not on your shell path after installing Rust, run `. "$HOME/.cargo/env"`.

```sh
git clone https://github.com/johnohhh1/librelink-mcp-rs.git
cd librelink-mcp-rs
cargo run -- setup
cargo run -- check
```

`setup` prompts locally for the follower account email, a hidden password, and a region. It accepts `global` (the default), `us`, `eu`, `eu2`, `ae`, `ap`, `au`, `ca`, `de`, `fr`, `jp`, `la`, `ru`, and `cn`. If the account follows multiple people, run `cargo run -- select-patient` and enter the followed person's full name before `check`. An ambiguous account is rejected instead of silently choosing the first connection.

`check` makes an authenticated `read_raw()` request and prints a small connectivity report. A local user check confirmed that the pinned Rust client connects and returns values matching their phone; other accounts, regions, and sensors still need validation. The [Rust client API and compatibility findings](docs/architecture.md) explain the dependency choice.

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

Tool replies contain MCP `structuredContent` plus a JSON text block for clients that only display text. Errors set `isError: true` and use safe codes including `invalid_input`, `insufficient_data`, `rate_limited`, `timeout`, `session_expired`, `authentication_failed`, `account_action_required`, and `upstream_unavailable`. The owned API client being added in v0.2 also reports `client_version_rejected` when Abbott requires a newer client version (run `set-api-version`), `request_rejected` when the API refuses this client's request headers, and `api_changed` when a successful response is not JSON. `api_changed` may mean the API changed or was retired, but it is not proof of either. After `authentication_failed`, the server makes no further login attempts until it is restarted, so a wrong password cannot drive the account into an Abbott lockout. Rate-limit replies include `retry_after_seconds`. Upstream response bodies and tokens are never included in error messages.

## Data quality

The API can return timestamps without timezone offsets. When both clock strings parse, their gap is quarter-hour aligned and within ±14 hours, and the factory clock is not in the future, the server interprets `FactoryTimestamp` as UTC and labels `time_basis: factory_utc_inferred`. This follows the [Rust client's UTC interpretation](https://github.com/ItsXrgon/libre-link-up-api-client/blob/main/src/utils.rs) and [public paired-clock examples](https://github.com/NightscoutFoundation/xDrip/discussions/2118), but the API does not attach an offset. A missing, inconsistent, or future factory time keeps freshness `unknown`. Retrieval time does **not** turn a reading into a real-time reading.

When every graph point has a usable offset-aware or inferred UTC time, history, stats, and trends filter to the requested window. Coverage reports the first and last observations, span, estimated cadence and expected point count, actual count, and detected gaps. These estimates come from returned intervals. Because the LibreLinkUp graph covers about 12 hours, statistics and trends for a requested day or longer return `period_status: partial` with the observed span in `coverage`; they describe the returned graph, not the requested days. Statistics return `insufficient_data` only when no valid values were returned. `window_verified` remains false: the API cannot prove complete coverage. Sample percentages are not time-weighted time in range; GMI and clinical-pattern fields remain unavailable. See the [architecture and parity notes](docs/architecture.md) and [release roadmap](docs/roadmap.md).

## Privacy and local data

- Credentials live in the OS credential store. There is no plaintext file fallback, command-line password flag, or MCP secret input.
- The server does not persist glucose history. SQLite is planned only as an opt-in feature and will never hold credentials.
- Successful graph reads are cached in memory for 60 seconds. Detected rate limits and account lockouts trigger a cooldown; other upstream failures also pause calls for 60 seconds. Cached readings keep their original retrieval time. The upstream client can still retry within the first failed request, pending a replacement API adapter.
- `cargo run -- clear-local-data` removes this project's credential and preference entries from the OS store. Stop the server first; run `setup` again to reconnect.
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
