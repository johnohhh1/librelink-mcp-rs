# LibreLink MCP RS

An independent Rust implementation of a local MCP server for LibreLinkUp sharing data. The first stdio MCP server exposes the eight original tool names. Several analytics fields remain unavailable until source time coverage can be established; see [architecture and parity status](docs/architecture.md) and the [release roadmap](docs/roadmap.md).

This project was inspired by [Amandeep Khurana's LibreLink MCP Server](https://github.com/amansk/librelink-mcp-server). The original project defines the tool names and initial behavior. Its TypeScript code is not included here. Thank you to Amandeep Khurana and the original contributors.

## Compatibility before use

This server uses [`libre_link_up_api_client` 0.3.3](https://github.com/ItsXrgon/libre-link-up-api-client). That crate authenticates a **LibreLinkUp follower account** and fetches a graph response. The [Node client used by the original server](https://github.com/DRFR0ST/libre-link-unofficial-api) also calls for a LibreLinkUp follower account, although the original server's README calls it a LibreLink account. A sensor owner's primary LibreLink login may not work unless the same credentials also belong to a LibreLinkUp follower account. A local user check confirmed this crate can connect and return values matching their phone. Other accounts and regions remain untested. Do not enter credentials into chat or an MCP tool.

The crate supports global routing plus US, EU, EU2, AE, AP, AU, CA, DE, FR, JP, LA, RU, and CN endpoints. `setup` accepts an explicit region, defaulting to global auto-redirect. An unknown region is rejected. If the account follows multiple people, run `select-patient` locally with the followed person's full name. Without a selection, data tools refuse an ambiguous account instead of choosing the first connection.

## Local setup and connectivity check

Requires Rust 1.89 or newer and an available OS credential store (macOS Keychain, Windows Credential Manager, or a Linux Secret Service session). Building on Debian or Ubuntu also needs `libdbus-1-dev` and `pkg-config` for the keyring backend. There is no plaintext fallback.
If `cargo` is not on your shell path after installing Rust, run `. "$HOME/.cargo/env"` first.

```sh
cargo run -- setup
cargo run -- select-patient  # only if following multiple people
cargo run -- check
```

`setup` prompts locally for the follower account email, hides password input, and saves a single credential record in the OS credential store. The password is never accepted as a command-line or MCP argument. `check` performs an authenticated `read_raw()` call and prints only a small, typed JSON connectivity report. API error bodies and tokens are not printed. The report keeps source and factory timestamp text, retrieval time, and an interpreted UTC observation time separate. If source timestamps have no timezone, `observed_at_utc` is absent and freshness is `unknown`; the report does not call that reading real-time. No treatment recommendations are produced.

On headless Linux systems without Secret Service, `setup` fails. That is intentional until another secure store is designed and tested. If you previously used the original server, its `~/.librelink-mcp/config.json` may contain a plaintext password; this prototype does not read or migrate that file.

## MCP server

Run `cargo run -- serve` for a local stdio server. MCP clients must launch the binary directly; for example, after `cargo build --release`:

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

The server exposes the eight original tool names. `configure_credentials` accepts no arguments and directs you to the local setup command. `configure_ranges` persists non-secret bounds in the OS credential store. The reading tools report source timestamps and avoid fabricated sensor details. An offset-aware graph is filtered to requested hours, days, or trend period; a graph with any offset-free or conflicting timestamp remains unfiltered. Every summary states whether filtering applied. `window_verified` remains false because the graph endpoint cannot prove complete coverage of the requested interval. Statistics describe observed samples only; true time in range and GMI remain unavailable. `get_glucose_trends` reports observed values and trend-arrow counts without meal or dawn inference. These are deliberate limits, not full behavioral parity.

The server reuses its authenticated API client across tool calls. Restart it after changing credentials or patient selection with the local CLI. Range changes take effect without a restart.

Tool replies include both MCP `structuredContent` and a JSON text block for clients that only display text. Errors use `isError: true` and an `error_code`: `invalid_input` for rejected bounds, `insufficient_data` when a requested statistic has no valid samples, or `request_failed` for a local store or LibreLinkUp request failure. Error messages do not include upstream response bodies.

`rmcp` and `tokio` handle stdio. SQLite remains optional future work and will not store credentials. Tests use synthetic data and no account secrets.

CI checks formatting, Clippy, and tests on Linux, macOS, and Windows. The synthetic stdio test initializes the server, lists the eight tools, checks structured output, and verifies that a password argument is rejected without echoing its value. Only Linux has been compiled locally so far.

This is an unofficial project and is not affiliated with Abbott. It is for inspecting shared data, not making medical decisions.

## License

MIT; see [LICENSE](LICENSE).
