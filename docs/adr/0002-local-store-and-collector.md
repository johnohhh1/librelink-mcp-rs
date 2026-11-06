# ADR 0002: Local store and collector

- **Status:** accepted
- **Date:** 2026-11-05
- **Milestone:** v0.3 (see [roadmap](../roadmap.md#v03-opt-in-collect-command-and-sqlite))

## Context

The LibreLinkUp graph endpoint takes no date range and covers about 12 hours, so 7-, 30-, and 90-day history cannot come from the live API. v0.2 replaced the borrowed client with an owned one that never logs in because of throttling or other errors ([ADR 0001](0001-owned-api-client.md)), which makes conservative polling safe. v0.3 adds an opt-in local store that a separate collector fills, and lets `serve` read from it.

## Decision

### Process

- `collect` is a separate, long-running command that the user starts. It runs in the foreground. Step 2 documents an example systemd user unit.
- `serve` never polls on its own. Its only upstream access remains today's live fallback.

### Source selection

- `serve` reads from the store when the collector heartbeat is fresh (less than 3 minutes old), and from the live API otherwise.
- Every data tool reply includes `data_source: "local_store" | "live_api"`.
- While a collector runs, only one process talks to Abbott.

### Storage

- `rusqlite` with the `bundled` feature, in WAL mode. Blocking calls are wrapped in `spawn_blocking`.
- The schema version is kept in `PRAGMA user_version`, with embedded migrations.

### Location

- The data directory comes from the `directories` crate's `ProjectDirs`. A `--db <path>` option overrides it.
- On Unix, the directory is created `0700` and the file `0600`.
- `clear-local-data` deletes the database and its `-wal` and `-shm` files.

### Tables

```sql
readings(
  patient_key       TEXT,     -- SHA-256 hex of the patient ID
  factory_timestamp TEXT,
  source_timestamp  TEXT,
  observed_at_utc   INTEGER NULL,
  time_basis        TEXT,
  value_mg_dl       REAL,
  trend_arrow       INTEGER NULL,
  origin            TEXT,
  first_seen_utc    INTEGER,
  PRIMARY KEY (patient_key, factory_timestamp)
)  -- rows are written with INSERT OR IGNORE, so overlapping graphs deduplicate

polls(
  polled_at_utc  INTEGER,
  outcome        TEXT,
  readings_added INTEGER
)  -- one row per attempt, so "collector off" and "API had no data"
   -- can be told apart when computing gaps

collector_state(
  heartbeat_utc,
  pid,
  version
)  -- a single row
```

`patient_key` is the SHA-256 hex of the patient ID. The raw ID is never stored.

### Retention

- The default is 90 days, configurable with `--retention-days` from 1 to 3650.
- Readings and polls are pruned once a day.

### Poll loop

- The interval has a 60-second floor, enforced in code and not configurable lower, plus 0–5 seconds of random jitter.
- Errors use the `Fetcher`'s cooldown and `retry_after`.
- A latched error (`authentication_failed`, `client_version_rejected`, `request_rejected`) makes `collect` exit with a non-zero status and the error's message.
- The systemd unit example uses `Restart=on-failure`, `RestartSec=300`, and a start limit so it cannot loop.

### Single instance

`collect` refuses to start when the heartbeat is less than 2 minutes old, with a clear message. `--force` overrides this.

### Shutdown

On Ctrl+C or SIGTERM, `collect` finishes the current write and then exits with status 0.

### Privacy

- The database holds no credentials, tokens, or raw patient IDs, only values and timestamps.
- It stays local; nothing is uploaded.

### Plan

1. **Store module:** schema, migrations, permissions, inserts with deduplication, polls, heartbeat, retention, and deletion. Tested with synthetic data only.
2. **`collect` command:** poll loop, jitter, cooldown, latched-error exit, the single-instance check, shutdown, and the systemd user unit example.
3. **`serve` source selection and tool reads:** the `data_source` field and store-backed history, stats, and trends, followed by a live check by the PM.

## Consequences

- Multi-day history becomes possible, but only for periods when a collector was running. Coverage and gaps must still be reported honestly, using `polls` to tell collector downtime from API gaps.
- `serve` gains a second data path and must label every reply with its source.
- New dependencies: `rusqlite` (bundled SQLite, which adds C compilation to the build) and `directories`. Their MSRV must stay within Rust 1.89.
- A database file now holds personal glucose values on the user's machine. Its permissions, deletion, and retention are part of the privacy boundary.

## Open questions

These do not change the decisions above. They are points to settle during implementation.

1. **Heartbeat during long cooldowns.** A rate-limit cooldown can last up to 900 seconds. If the heartbeat is written only after a poll, it goes stale during a long cooldown, `serve` switches to the live API, and two processes talk to Abbott while it is throttling. Writing the heartbeat on every loop iteration, including while waiting out a cooldown, would avoid this.
2. **systemd restarts after a latched error.** With `Restart=on-failure`, an exit caused by `authentication_failed` is restarted after 300 seconds and attempts another login, up to the start limit. That is several failed logins toward an Abbott lockout. A distinct exit status for latched errors, together with `RestartPreventExitStatus=`, would stop restarts in that case only.
3. **Single-instance race.** A heartbeat check cannot stop two collectors that start within the same moment. An advisory lock on a file beside the database would close that gap.
4. **Pruning rows without a UTC time.** `observed_at_utc` can be null. Retention needs a rule for those rows; `first_seen_utc` is one candidate.
5. **`patient_key` is linkable.** An unsalted SHA-256 of a patient ID is stable across installations. That is acceptable for local deduplication, but it should not be described as anonymization.
