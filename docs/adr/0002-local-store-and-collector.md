# ADR 0002: Local store and collector

- **Status:** implemented and live-validated 2026-11-06 (released in 0.3.0)
- **Date:** 2026-11-05 (implementation details settled the same day)
- **Milestone:** v0.3 (see [roadmap](../roadmap.md#v03-opt-in-collect-command-and-sqlite))

## Context

The LibreLinkUp graph endpoint takes no date range and covers about 12 hours, so 7-, 30-, and 90-day history cannot come from the live API. v0.2 replaced the borrowed client with an owned one that never logs in because of throttling or other errors ([ADR 0001](0001-owned-api-client.md)), which makes conservative polling safe. v0.3 adds an opt-in local store that a separate collector fills, and lets `serve` read from it.

## Decision

### Process

- `collect` is a separate, long-running command that the user starts. It runs in the foreground. Step 2 documents an example systemd user unit.
- `serve` never polls on its own. Its only upstream access remains today's live fallback.

### Source selection

- `serve` reads from the store when the collector heartbeat is fresh (less than 3 minutes old), and from the live API otherwise.
- The collector writes the heartbeat on every loop iteration and at least every 30 seconds, including while it waits out a cooldown. So `serve` keeps using the store while a collector is alive, even during a long rate-limit cooldown, and never starts a second conversation with Abbott at that moment. Stored readings simply age into `stale` freshness, which is honest.
- The heartbeat is used only for source selection, not to enforce a single collector.
- Every data tool reply includes `data_source: "local_store" | "live_api"`.
- While a collector runs, only one process talks to Abbott.

### Storage

- `rusqlite` with the `bundled` feature, in WAL mode. Blocking calls are wrapped in `spawn_blocking`.
- The schema version is kept in `PRAGMA user_version`, with embedded migrations.

### Location

- The data directory comes from the `directories` crate's `ProjectDirs`. A `--db <path>` option overrides it.
- On Unix, the directory is created `0700` and the file `0600`.
- `clear-local-data` deletes the database and its `-wal`, `-shm`, and `.lock` files.

### Tables

```sql
readings(
  patient_key       TEXT,     -- salted SHA-256 hex of the patient ID
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

meta(
  key   TEXT PRIMARY KEY,
  value BLOB
)  -- holds the per-database salt
```

`patient_key` is the lowercase hex SHA-256 of `salt || patient_id`. The salt is 32 random bytes generated when the database is created and stored in `meta`. Keys therefore cannot be linked across databases. The raw ID is never stored. CSV import (v0.4) reuses the same database salt.

### Retention

- The default is 90 days, configurable with `--retention-days` from 1 to 3650.
- Readings and polls are pruned once a day. Readings are pruned by `COALESCE(observed_at_utc, first_seen_utc)`, so rows without a usable UTC time still age out.

### Poll loop

- The interval has a 60-second floor, enforced in code and not configurable lower, plus 0–5 seconds of random jitter.
- Errors use the `Fetcher`'s cooldown and `retry_after`.
- A latched error (`authentication_failed`, `client_version_rejected`, `request_rejected`) makes `collect` print the error's message and exit with status 78 (`EX_CONFIG`).
- The systemd unit example uses `RestartPreventExitStatus=78`, so a latched error is never restarted into another failed login. Other failures use `Restart=on-failure`, `RestartSec=300`, `StartLimitBurst=3`, and `StartLimitIntervalSec=3600`.

### Single instance

`collect` takes an exclusive advisory lock on `<db>.lock` with `std::fs::File::try_lock`, which is stable since Rust 1.89, the project's MSRV. If another process holds the lock, `collect` refuses to start with a clear message. There is no `--force`: the OS releases the lock when the holding process exits or dies, so a stale lock cannot happen. The `fs4` crate is the fallback if the standard API turns out to be unavailable on a supported platform.

### Shutdown

On Ctrl+C or SIGTERM, `collect` finishes the current write and then exits with status 0.

### Privacy

- The database holds no credentials, tokens, or raw patient IDs, only values, timestamps, and a salted patient key. The salted key is a pseudonym for local deduplication, not anonymization.
- It stays local; nothing is uploaded.

### Plan

1. **Store module:** schema, migrations, permissions, inserts with deduplication, polls, heartbeat, retention, and deletion. Tested with synthetic data only.
2. **`collect` command:** poll loop, jitter, cooldown, heartbeat, latched-error exit status 78, the lock file, shutdown, and the systemd user unit example.
3. **`serve` source selection and tool reads:** the `data_source` field and store-backed history, stats, and trends, followed by a live check by the PM.

## Consequences

- Multi-day history becomes possible, but only for periods when a collector was running. Coverage and gaps must still be reported honestly, using `polls` to tell collector downtime from API gaps.
- `serve` gains a second data path and must label every reply with its source.
- New dependencies: `rusqlite` (bundled SQLite, which adds C compilation to the build) and `directories`. Their MSRV must stay within Rust 1.89.
- A database file now holds personal glucose values on the user's machine. Its permissions, deletion, and retention are part of the privacy boundary.
