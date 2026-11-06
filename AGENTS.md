# Working on the Rust port

- This repository is an independent Rust implementation. The sibling `../librelink-mcp-server` checkout is read-only reference material for behavior and tool names. Do not copy its TypeScript build files, dependencies, or credential files here.
- Preserve the eight original MCP tool names. `configure_credentials` must never accept a password or token through MCP; it should return local setup instructions or a credential-store status only.
- Do not put credentials, bearer tokens, account responses, personal glucose data, or real fixtures into source, tests, logs, issues, or commits. Use synthetic data.
- Use typed models and retain source timestamps. Mark missing, stale, and estimated values explicitly. Never replace a malformed timestamp with the current time or invent sensor identity or activation time.
- Do not generate clinical diagnoses, treatment advice, or claims that readings are real-time when freshness is unknown or stale.
- Validate the LibreLinkUp follower account model and region routing before treating the Rust API client as a permanent dependency. Its README and published crate API have differed.
- Keep credential setup local and use the OS credential store. If it is unavailable, fail closed.
- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` when a Rust toolchain and registry access are available. Record toolchain limitations plainly.
