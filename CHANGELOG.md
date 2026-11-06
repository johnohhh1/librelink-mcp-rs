# Changelog

## 0.2.0 — 2026-11-05

### Changed
- The server now uses its own LibreLinkUp client (`reqwest` with rustls) instead of the `libre_link_up_api_client` crate. See [ADR 0001](docs/adr/0001-owned-api-client.md).
  - It logs in only when there is no valid session or after a 401, with at most one 401 re-login per read. Throttling, server errors, parse errors, and rejected requests never trigger a login.
  - It follows one login region redirect, and only to a region in the known table.
  - It sends the account ID only as a SHA-256 hash, keeps the password once in zeroizing storage, and never logs or returns response bodies.
  - It sends the same request headers as the crate did.
- Tools read through a `DataSource` trait. Every tool uses one current reading, `glucoseMeasurement`; history, stats, and trends previously appended `glucoseItem`.
- Live-validated on 2026-11-05 against one US follower account.

### Added
- Error codes `client_version_rejected` (403 with status 920; a plain minimum version is included in the message), `request_rejected` (`AccountIdMismatch` / `RequiredHeaderMissing`), and `api_changed` (a successful response that is not JSON; a candidate retirement signal, not proof).
- `client_version_rejected` and `request_rejected` latch until restart, as `authentication_failed` already did, so the server stops sending requests that cannot succeed.
- Login status 4 maps to `account_action_required`. Lockouts and HTTP 429/430 map to `rate_limited`, using the server's retry time clamped to 60–900 seconds.

### Removed
- The `libre_link_up_api_client` dependency.

## 0.1.0

First prototype: the eight original tool names over MCP stdio, OS credential storage, labeled timestamps and freshness, partial statistics, a fetch cache and failure cooldown, and a rejected-credential latch.
