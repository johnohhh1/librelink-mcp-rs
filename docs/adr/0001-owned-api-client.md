# ADR 0001: Replace the borrowed LibreLinkUp client with an owned one

- **Status:** implemented and live-validated 2026-11-05 (released in 0.2.0)
- **Date:** 2026-11-05
- **Milestone:** v0.2 (see [roadmap](../roadmap.md#v02-owned-api-client-behind-a-datasource-trait))

## Context

The server reads LibreLinkUp through `libre_link_up_api_client` 0.3.3, pinned exactly. A local check showed that it works against a real follower account. It also has four problems that block v0.3 polling and make error handling hard to test:

1. **It retries login after any request error.** `authenticated_request` catches every failure from `try_request`, logs in again, and retries once. That includes 429/430 throttling and other errors that a new login cannot fix. Each extra login moves the account toward an Abbott lockout, and a background poller would do this repeatedly. The server's 60-second cooldown and rejected-credential latch work only after the crate has already retried.
2. **Its base URL is fixed.** The base URL comes from `Region::base_url()` and region redirects, with no override. The client cannot be pointed at a mock server, so redirect, lockout, account-action, minimum-version, and throttling paths cannot be tested with synthetic HTTP fixtures.
3. **It keeps its own copy of the password.** `ClientConfig` holds the password as a plain public `String` for the life of the client and clones it into each login request. The server zeroizes its own copy, but cannot zeroize the crate's.
4. **It mislabels sensor fields.** The `Sensor` model names fields `a`, `w`, and `pt` with misleading comments. For example, `a` is described as age in days, but observed payloads hold epoch seconds. Its processed `read()` also replaces unparseable timestamps with `Utc::now()` and unknown trend arrows with `Flat`, which is why the server uses only `read_raw()`.

Its error type also turns HTTP failures into `InvalidResponse(String)` that contain the response body. The server parses only the status prefix out of that string so it never forwards the body, which is fragile.

## Decision

Replace the crate with an owned client, in three steps. Each step is its own reviewable change.

1. **Seam.** Define a `DataSource` trait in `src/source.rs` that returns the project's own types:
   - `Snapshot`: exactly one current reading, the graph samples, and the active sensors.
   - `GlucoseSample`: source and factory timestamp text, the mg/dL value, and the trend arrow code if present.
   - `SensorRecord`: device ID, serial number, activation epoch seconds, warmup minutes, and product type code, named for what they hold.

   The current reading comes from `connection.glucose_measurement`, the field `get_current_glucose` already used. The crate also returns `connection.glucose_item`, and history previously appended that instead; the seam drops it, so every tool sees the same current reading.

   All crate use lives in `src/borrowed_client.rs`, the one `DataSource` implementation, including region parsing and error classification. The 60-second cache, failure cooldown, and rejected-credential latch live in `src/fetch.rs` (`Fetcher`) and depend only on the trait. MCP tools receive a `Fetcher` and can run against a fake source in tests.
2. **Owned client.** Write a `reqwest` client that implements `DataSource`:
   - A configurable base URL, so `wiremock` tests can stand in for the server.
   - Region redirect handling and a bounded session refresh: at most one login per failed request, and none after 429/430, lockout, or rejected credentials.
   - Credentials read once from the OS store into zeroizing storage and not duplicated into long-lived strings.
   - Typed status handling that maps upstream errors to public codes without ever holding a response body in an error. This includes `client_version_rejected`, `request_rejected`, and `api_changed`, the candidate retirement signal; `api_retired` is reserved for a confirmed retirement.
   - Synthetic fixtures for redirect, lockout, status 2 and 4 account actions, 401, 429/430, 403/920 minimum version, and account-ID mismatch.
3. **Switch over.** Use the owned client in `serve` and `check`, delete `borrowed_client.rs`, and remove the crate from `Cargo.toml` and `Cargo.lock`.

## Consequences

- Tool output did not change in step 1, with one exception: history, stats, and trends now append `glucose_measurement` rather than `glucose_item`.
- Step 2 also added `request_rejected` (`AccountIdMismatch` / `RequiredHeaderMissing`) and `api_changed` (a successful non-JSON response). `client_version_rejected`, `request_rejected`, and `authentication_failed` latch until restart.
- The owned client sends the same request headers as the crate (`product: llu.ios` and its User-Agent), so the switch-over changed no request headers.
- After step 3 the crate is no longer a dependency.
- The project takes on maintenance of the HTTP protocol details: headers, version string, region routing, and response shapes. Abbott may change the unofficial API at any time. See [API retirement risk](../roadmap.md#api-retirement-risk). The owned client reports `api_changed` as a candidate signal; `api_retired` is reserved for a confirmed retirement. It will not try to get around v5 response encryption.
- Every upstream error path becomes testable offline with synthetic data. No real account responses are ever used as fixtures.
