# Local collector

The LibreLinkUp graph covers about 12 hours. To keep a longer history, run the optional `collect` command. It polls LibreLinkUp about once a minute and saves readings in a local SQLite database. The design is recorded in [ADR 0002](adr/0002-local-store-and-collector.md).

`collect` is off unless you start it. Until v0.3 step 3, `serve` does not read the database yet.

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
- **Heartbeat:** written to the database at least every 30 seconds, including while waiting out a cooldown. In step 3, `serve` will use it to tell whether a collector is running.
- **One collector per database:** an advisory lock on `<db>.lock` is held while running. A second `collect` on the same database exits with "another collector is already running for this database". The OS releases the lock when the process exits, so a crash cannot leave a stale lock.
- **Retention:** expired readings and poll records are pruned at startup and then once a day.
- **Stopping:** Ctrl+C, or SIGTERM on Unix, finishes any write in progress and exits with status 0.
- **Logs:** one line per poll on stderr, with the outcome and the number of readings added, or the error code and the wait in seconds.

Exit statuses:

| Status | Meaning |
| --- | --- |
| 0 | Stopped by Ctrl+C or SIGTERM. |
| 1 | Could not open or lock the database, another collector is running, or a database write failed. |
| 2 | Invalid command-line options. |
| 78 | A problem a retry cannot fix: `authentication_failed` (run `setup`), `client_version_rejected` (run `set-api-version`), or `request_rejected` (please report a bug). The message says which. Fix the cause, then start `collect` again. |

## Privacy

What the database stores:

- Glucose readings: the source timestamp text, the factory timestamp, the interpreted UTC time and its basis, the mg/dL value, the trend arrow code, whether the reading came from the graph or the current measurement, and when it was first seen.
- One row per poll attempt: its time, the outcome (`ok` or an error code), and how many readings it added. This lets later steps tell "collector not running" apart from "LibreLinkUp had no data".
- The collector heartbeat: time, process ID, and version.
- A random per-database salt.

What it never stores:

- Credentials, tokens, or the email address. These stay in the OS credential store.
- The raw patient ID or patient names. Readings are keyed by a SHA-256 of the per-database salt plus the patient ID. That key is a pseudonym for local deduplication, not anonymization, and it cannot be matched across databases.

The database never leaves your machine; nothing is uploaded. On Unix, the data directory is created with mode `0700` and the database with `0600`.

To delete everything, stop the collector and run:

```sh
librelink-mcp-rs clear-local-data
```

That removes the credentials, the local preferences, and the default database with its `-wal`, `-shm`, and `.lock` files. It refuses while a collector is running. A database at a custom `--db` path must be deleted by hand.

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
