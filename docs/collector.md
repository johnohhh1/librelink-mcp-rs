# Local collector

The LibreLinkUp graph covers about 12 hours. To keep a longer history, run the optional `collect` command. It polls LibreLinkUp about once a minute and saves readings in a local SQLite database. The design is recorded in [ADR 0002](adr/0002-local-store-and-collector.md).

`collect` is off unless you start it. While it runs, `serve` answers from the database instead of contacting LibreLinkUp (see [How serve chooses its data](#how-serve-chooses-its-data)).

## Usage

Run `setup` (and `select-patient` if needed) first, then:

```sh
librelink-mcp-rs collect
```

Options:

| Option | Meaning |
| --- | --- |
| `--db <path>` | Use this database file instead of the default location. |
| `--retention-days <1-3650>` | Keep readings and poll records this many days. The default is 90. |

The default database is `readings.sqlite3` in the platform data directory. On Linux that is `~/.local/share/librelink-mcp-rs/`; the path is printed once at startup.

How it behaves:

- **Polling:** at most once every 60 seconds, plus 0–5 seconds of random jitter. The 60-second floor cannot be lowered. When LibreLinkUp asks for a longer wait, for example during a rate limit, the collector waits that long instead.
- **Heartbeat:** written to the database at least every 30 seconds, including while waiting out a cooldown. `serve` uses it to tell whether a collector is running.
- **One collector per database:** an advisory lock on `<db>.lock` is held while running. A second `collect` on the same database exits with "another collector is already running for this database". The OS releases the lock when the process exits, so a crash cannot leave a stale lock.
- **Retention:** expired readings and poll records are pruned at startup and then once a day. The retention is recorded in the database at startup, and [`import-csv`](csv-import.md) applies it to imported rows.
- **Stopping:** Ctrl+C, or SIGTERM on Unix, finishes any write in progress and exits with status 0.
- **Logs:** one line per poll on stderr, with the outcome and the number of readings added, or the error code and the wait in seconds.

Exit statuses:

| Status | Meaning |
| --- | --- |
| 0 | Stopped by Ctrl+C or SIGTERM. |
| 1 | Could not open or lock the database, another collector is running, or a database write failed. |
| 2 | Invalid command-line options. |
| 78 | A problem a retry cannot fix: `authentication_failed` (run `setup`), `client_version_rejected` (run `set-api-version`), `patient_selection_changed` (run `select-patient`), or `request_rejected` (please report a bug). The message says which. Fix the cause, then start `collect` again. |

## How serve chooses its data

`serve` takes the same `--db <path>` option, with the same default. On every data tool call:

- If the database exists and its collector heartbeat is less than 3 minutes old, the reply comes from the database (`"data_source": "local_store"`). `serve` makes **no** request to LibreLinkUp, so only the collector talks to Abbott.
- Otherwise the reply comes from LibreLinkUp, as without a collector (`"data_source": "live_api"`).

Every data reply includes `data_source`. From the store:

- `get_current_glucose` reports the newest stored reading. Its `retrieved_at_utc` is when the collector stored it, so freshness turns `stale` if the collector falls behind, for example during a long rate-limit cooldown.
- `get_glucose_history`, `get_glucose_stats`, and `get_glucose_trends` use the requested window, up to each tool's maximum. When the stored readings span the whole window (the first within 15 minutes of its start and the last within 15 minutes of its end), `period_status` is `sample_summary` instead of `partial`. Interior gaps do not make a period partial; they are listed in `coverage.interior_gaps` and reflected in the time-weighted `coverage_percent`.
- Each interior gap in `coverage` has a `cause`, the first of these that applies:
  1. `no_data_from_api`: the collector polled during the gap but got no readings for that span.
  2. `no_data_in_export`: the gap lies entirely within the span of [imported CSV](csv-import.md) rows, so the device's own history has nothing there, for example during a sensor change.
  3. `before_collection`: the gap ends before the first recorded poll. For that period the database holds only the graph LibreLinkUp returned when collection began, so the cause is unknown. Once retention prunes the oldest poll records, older gaps are labeled this way too.
  4. `collector_not_running`: after collection began, no poll attempt was recorded in the gap.
- An imported CSV row within 90 seconds of a collected reading is left out of every reply, so overlapping data is never counted twice.

  The gap threshold uses the graph's cadence (normally 15 minutes), so minute-by-minute current readings do not hide or create gaps.
- `get_sensor_info` reads the latest snapshot's sensors, which the database holds only in masked form.
- `validate_connection` reports the collector heartbeat age, the last poll outcome and time, and the number of stored readings.

`serve` opens the database read-only: it never creates, migrates, or writes to it, so it is safe to run a newer `serve` beside an older collector. If it cannot read the database, or the database is from a newer version, it uses the live API. Only `collect` and `import-csv` upgrade the database.

## Upgrading

`collect` and `import-csv` upgrade the database schema when they open it. The schema changes in 0.4.0, and a 0.3.0 collector refuses an upgraded database at startup. To upgrade:

1. Stop the collector.
2. Install the new version.
3. Run `import-csv` if you have an export to import, or start `collect`. Either one upgrades the database.
4. Start the collector, if it is not already running.

There is no downgrade: an older version refuses an upgraded database with "created by a newer version". Keep the old version's binary only if you also keep a copy of the database from before the upgrade.

## Privacy

What the database stores:

- Glucose readings, collected or [imported from a LibreView CSV](csv-import.md): the source timestamp text, the reading's deduplication key (the factory clock text for readings from LibreLinkUp), the interpreted UTC time and its basis, the mg/dL value, the trend arrow code, whether the reading came from the graph, the current measurement, or a CSV historic or scan row, and when it was first seen.
- One row per poll attempt: its time, the outcome (`ok` or an error code), and how many readings it added. This lets later steps tell "collector not running" apart from "LibreLinkUp had no data".
- The collector heartbeat: time, process ID, version, and the salted key of the patient being collected.
- The latest snapshot's sensors, in the form tools show them: the last four characters of the serial number and device ID, the activation time, warmup minutes, and product type code.
- A random per-database salt, and the collector's retention in days.

What it never stores:

- Credentials, tokens, or the email address. These stay in the OS credential store.
- Raw sensor serial numbers or device IDs.
- The raw patient ID or patient names. Readings are keyed by a SHA-256 of the per-database salt plus the patient ID. That key is a pseudonym for local deduplication, not anonymization, and it cannot be matched across databases.

The database never leaves your machine; nothing is uploaded. On Unix, the data directory is created with mode `0700` and the database with `0600`.

To delete everything, stop the collector and run:

```sh
librelink-mcp-rs clear-local-data
```

That removes the credentials, the local preferences, and the database with its `-wal`, `-shm`, `.lock`, and `.import.lock` files. Pass `--db <path>` to delete a database at a custom location. It refuses while a collector or a CSV import holds that database's lock.

## Running it as a systemd user service

Save as `~/.config/systemd/user/librelink-collect.service`, adjusting the binary path:

```ini
[Unit]
Description=LibreLinkUp local collector (librelink-mcp-rs)
After=network-online.target
StartLimitBurst=3
StartLimitIntervalSec=3600

[Service]
ExecStart=%h/.cargo/bin/librelink-mcp-rs collect
Restart=on-failure
RestartSec=300
# Exit status 78 means a retry cannot help, for example rejected
# credentials. Do not restart into another failed login.
RestartPreventExitStatus=78

[Install]
WantedBy=default.target
```

Then:

```sh
systemctl --user daemon-reload
systemctl --user enable --now librelink-collect.service
journalctl --user -u librelink-collect.service -f
```

The credential store must be reachable from the service. On Linux this is the Secret Service (for example GNOME Keyring) in your session, and it must be unlocked. With these settings, other failures are retried at most three times an hour, five minutes apart.
