# Working on librelink-mcp-rs

## Scope and safety

- This repository is an independent Rust implementation. The sibling `../librelink-mcp-server` checkout, if present, is read-only reference material for behavior and tool names. Do not copy its TypeScript build files, dependencies, or credential files here.
- Preserve the eight original MCP tool names. `configure_credentials` must never accept a password or token through MCP; it returns local setup instructions only. Bundle manifests must not declare `user_config`.
- Never put credentials, tokens, account responses, patient or account IDs, names, serial numbers, real glucose data, or real LibreView exports into source, tests, logs, issues, or commits. Use synthetic fixtures only, and never run tests or tools against a live account, a real keyring entry, or a real database.
- Do not generate clinical diagnoses, treatment advice, or claims that readings are real-time when freshness is unknown or stale.
- Keep credential setup local and in the OS credential store. If it is unavailable, fail closed.

## Data honesty

- Keep source timestamps. Never replace a malformed or missing timestamp with the current time, and never invent sensor identity, activation time, or freshness.
- Label what is inferred, estimated, partial, or missing: `time_basis`, `freshness`, `period_status`, `coverage`, gap `cause`, `data_source`.
- Statistics: sample figures and time-weighted figures stay separate. GMI is reported only behind the 14-day and 70% coverage gate ([ADR 0003](docs/adr/0003-time-weighted-metrics-and-csv-import.md)).

## Code conventions

- **Tool replies:** every reply is a typed struct deriving `Serialize` and `JsonSchema`. Each tool publishes `output_schema` and carries annotations. Errors use `ToolError` with a public `error_code` and a fixed message. The schema test validates real replies on both data paths, so extend it when you add a tool or reply.
- **Error messages:** fixed text, or numbers and validated values only. Never include upstream bodies, file contents, inputs, or identifiers. Extend the marker tests (`tests/cli_no_leaks.rs`, the client's leak test, the collector's log test, the import privacy test) when you add an output path.
- **Network:** network access goes only through the owned client (`src/llu_client.rs`) to the region table's LibreLinkUp hosts. No telemetry and no other destinations. Throttling, server errors, and parse errors never cause a login.
- **Database writes:** `serve` opens the database read-only and never migrates it. Only `collect` and `import-csv` write or migrate. Schema changes need a new migration and a `user_version` bump.
- **Time in tests:** read the clock once per test (for example `fixture.now`) and build every timestamp from it.
- **Behavior changes:** every one needs a synthetic test. Check it with a mutation: break the code, see the test fail, restore.

## Checks

Every commit must pass:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo +1.89.0 test --locked   # MSRV
cargo deny --locked check
```

- **`--locked`:** use it except when adding a dependency; then commit the updated `Cargo.lock`.
- **New dependencies:** every one must build on Rust 1.89 and pass `deny.toml`. Justify any exception in that file.
- **Toolchain limits:** record them plainly.

## Commits and releases

- Commits are authored by the maintainer. Do not add co-author trailers or tool-generated attribution lines to commits, pull requests, release notes, or docs.
- Work on a branch, and let the maintainer review and push.
- Releases come from a `v*` tag that matches `Cargo.toml` and `packaging/mcpb/manifest.json`. The tag needs a matching `## <version>` section in `CHANGELOG.md`; the release workflow uses it as the notes.
