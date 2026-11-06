# LibreLink MCP RS

An independent Rust implementation of a local MCP server for LibreLinkUp sharing data. This repository is at the **connectivity proof-of-concept** stage: it does not yet start an MCP server or implement the eight tools.

This project was inspired by [Amandeep Khurana's LibreLink MCP Server](https://github.com/amansk/librelink-mcp-server). The original project defines the tool names and initial behavior. Its TypeScript code is not included here. Thank you to Amandeep Khurana and the original contributors.

## Compatibility before use

The current proof of concept uses [`libre_link_up_api_client` 0.3.3](https://github.com/ItsXrgon/libre-link-up-api-client) to test the API connection. That crate authenticates a **LibreLinkUp follower account**, obtains the first followed patient's connection by default, and fetches a graph response. The [Node client used by the original server](https://github.com/DRFR0ST/libre-link-unofficial-api) also calls for a LibreLinkUp follower account, although the original server's README calls it a LibreLink account. A sensor owner's primary LibreLink login may not work unless the same credentials also belong to a LibreLinkUp follower account. Live parity remains **unverified** until a local test with an appropriate account succeeds. Do not enter credentials into chat or an MCP tool.

The crate supports global routing plus US, EU, EU2, AE, AP, AU, CA, DE, FR, JP, LA, RU, and CN endpoints. `setup` accepts an explicit region, defaulting to global auto-redirect. An unknown region is rejected. Multiple followed patients are not yet selectable; the crate's default is the first connection. If that is ambiguous, do not rely on the returned reading.

## Local proof of concept

Requires Rust 1.89 or newer and an available OS credential store (macOS Keychain, Windows Credential Manager, or a Linux Secret Service session). There is no plaintext fallback.

```sh
cargo run -- setup
cargo run -- check
```

`setup` prompts locally for the follower account email, hides password input, and saves a single credential record in the OS credential store. The password is never accepted as a command-line or MCP argument. `check` performs an authenticated `read_raw()` call and prints only a small, typed JSON connectivity report. API error bodies and tokens are not printed. If the source timestamp has no timezone, freshness is `unknown`; the report does not call that reading real-time. No treatment recommendations are produced.

On headless Linux systems without Secret Service, `setup` fails. That is intentional until another secure store is designed and tested. If you previously used the original server, its `~/.librelink-mcp/config.json` may contain a plaintext password; this prototype does not read or migrate that file.

## Status and next steps

- The eight original MCP tool names and the planned behavior are in [docs/architecture.md](docs/architecture.md).
- This executable currently provides only `setup` and `check`. A successful API call with a follower account is still needed to confirm live compatibility.
- `rmcp`, `tracing`, and optional SQLite are planned for the server phase. SQLite will be opt-in and will not store credentials.
- Tests use synthetic timestamps and no account secrets.

This is an unofficial project and is not affiliated with Abbott. It is for inspecting shared data, not making medical decisions.

## License

MIT; see [LICENSE](LICENSE).
