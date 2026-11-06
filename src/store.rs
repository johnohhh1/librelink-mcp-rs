//! Local SQLite store for collected readings. See docs/adr/0002-local-store-and-collector.md.
//!
//! The API is blocking; async callers wrap it in `spawn_blocking`. The
//! database holds values, timestamps, and a salted patient key, never
//! credentials, tokens, or raw patient IDs.

use crate::source::{GlucoseSample, SensorSummary, Snapshot};
use crate::time::assess_time;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

pub(crate) const MIN_RETENTION_DAYS: u32 = 1;
pub(crate) const MAX_RETENTION_DAYS: u32 = 3650;
pub(crate) const DEFAULT_RETENTION_DAYS: u32 = 90;
const SALT_KEY: &str = "patient_key_salt";
const SALT_BYTES: usize = 32;
const DB_FILE_NAME: &str = "readings.sqlite3";

/// Migrations in order; `PRAGMA user_version` is the number applied.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
CREATE TABLE readings (
    patient_key       TEXT    NOT NULL,
    factory_timestamp TEXT    NOT NULL,
    source_timestamp  TEXT    NOT NULL,
    observed_at_utc   INTEGER,
    time_basis        TEXT    NOT NULL,
    value_mg_dl       REAL    NOT NULL,
    trend_arrow       INTEGER,
    origin            TEXT    NOT NULL,
    first_seen_utc    INTEGER NOT NULL,
    PRIMARY KEY (patient_key, factory_timestamp)
) WITHOUT ROWID;
CREATE INDEX readings_by_time ON readings (patient_key, observed_at_utc);
CREATE TABLE polls (
    polled_at_utc  INTEGER NOT NULL,
    outcome        TEXT    NOT NULL,
    readings_added INTEGER NOT NULL
);
CREATE INDEX polls_by_time ON polls (polled_at_utc);
CREATE TABLE collector_state (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    heartbeat_utc INTEGER NOT NULL,
    pid           INTEGER NOT NULL,
    version       TEXT    NOT NULL
);
"#,
    r#"
ALTER TABLE collector_state ADD COLUMN patient_key TEXT;
CREATE TABLE sensors (
    position                 INTEGER PRIMARY KEY,
    device_id_masked         TEXT,
    serial_number_masked     TEXT,
    activation_epoch_seconds INTEGER,
    warmup_minutes           INTEGER,
    product_type_code        INTEGER
);
"#,
];

pub(crate) type StoreResult<T> = Result<T, &'static str>;

fn db_error(_: rusqlite::Error) -> &'static str {
    "The local database could not be read or written."
}

/// Where a stored reading came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Graph,
    CurrentMeasurement,
}

impl Origin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Graph => "graph",
            Self::CurrentMeasurement => "current_measurement",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "graph" => Some(Self::Graph),
            "current_measurement" => Some(Self::CurrentMeasurement),
            _ => None,
        }
    }
}

/// A reading as the tools use it, plus where it came from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StoredReading {
    pub sample: GlucoseSample,
    pub origin: Origin,
    /// When the collector first stored this reading.
    pub first_seen_utc: DateTime<Utc>,
}

/// What the collector last reported about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CollectorStatus {
    pub heartbeat: Heartbeat,
    /// The salted key of the patient the collector is writing, once it has
    /// stored a snapshot.
    pub patient_key: Option<String>,
}

/// The most recent poll attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PollRecord {
    pub at: DateTime<Utc>,
    pub outcome: String,
    pub readings_added: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Heartbeat {
    pub at: DateTime<Utc>,
    pub pid: u32,
    pub version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Pruned {
    pub readings: usize,
    pub polls: usize,
}

/// The default database location: the platform data directory.
pub(crate) fn default_db_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "librelink-mcp-rs", "librelink-mcp-rs")
        .map(|dirs| dirs.data_dir().join(DB_FILE_NAME))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// The advisory lock file that allows one collector per database.
pub(crate) fn lock_path(db: &Path) -> PathBuf {
    with_suffix(db, ".lock")
}

/// Why the collector lock could not be taken.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LockError {
    /// Another process holds it.
    Held,
    /// The lock file could not be opened or locked.
    Unavailable,
}

/// Takes the exclusive advisory lock that allows one collector per
/// database. The lock lasts as long as the returned file is open, and the
/// OS releases it if the process dies.
pub(crate) fn lock_database(db: &Path) -> Result<fs::File, LockError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(lock_path(db))
        .map_err(|_| LockError::Unavailable)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(LockError::Held),
        Err(fs::TryLockError::Error(_)) => Err(LockError::Unavailable),
    }
}

/// Whether a collector currently holds the database lock.
pub(crate) fn database_locked(db: &Path) -> StoreResult<bool> {
    if !lock_path(db).exists() {
        return Ok(false);
    }
    match lock_database(db) {
        Ok(_released_on_drop) => Ok(false),
        Err(LockError::Held) => Ok(true),
        Err(LockError::Unavailable) => Err("Unable to check the local database lock."),
    }
}

/// Removes the database, its WAL and shared-memory files, and its lock
/// file. Missing files are not an error.
pub(crate) fn delete_database_files(db: &Path) -> StoreResult<()> {
    for path in [
        db.to_path_buf(),
        with_suffix(db, "-wal"),
        with_suffix(db, "-shm"),
        lock_path(db),
    ] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err("Unable to delete a local database file."),
        }
    }
    Ok(())
}

/// Creates missing parent directories, private to the user on Unix.
/// Existing directories are left as they are.
fn create_parent(db: &Path) -> io::Result<()> {
    let Some(parent) = db.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
        return Ok(());
    };
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)
}

/// Creates the database file if needed and makes it private to the user on
/// Unix. SQLite gives its `-wal` and `-shm` files the same mode.
fn create_private_file(db: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(db)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(db, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> StoreResult<()> {
    let applied: usize = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(db_error)?
        .try_into()
        .map_err(|_| "The local database has an invalid schema version.")?;
    if applied > MIGRATIONS.len() {
        return Err("The local database was created by a newer version of librelink-mcp-rs.");
    }
    for (index, migration) in MIGRATIONS.iter().enumerate().skip(applied) {
        let transaction = conn.transaction().map_err(db_error)?;
        transaction.execute_batch(migration).map_err(db_error)?;
        transaction
            .pragma_update(
                None,
                "user_version",
                i64::try_from(index + 1).unwrap_or(i64::MAX),
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
    }
    Ok(())
}

/// Returns the database's salt, creating it on first open.
fn load_or_create_salt(conn: &Connection) -> StoreResult<[u8; SALT_BYTES]> {
    let mut fresh = [0u8; SALT_BYTES];
    getrandom::fill(&mut fresh).map_err(|_| "Unable to generate a random salt.")?;
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
        params![SALT_KEY, fresh.as_slice()],
    )
    .map_err(db_error)?;
    let stored: Vec<u8> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [SALT_KEY], |row| {
            row.get(0)
        })
        .map_err(db_error)?;
    stored
        .try_into()
        .map_err(|_| "The local database has an invalid salt.")
}

fn epoch(time: DateTime<Utc>) -> i64 {
    time.timestamp()
}

fn time_basis_text(time_basis: crate::time::TimeBasis) -> String {
    serde_json::to_value(time_basis)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid".to_owned())
}

pub(crate) struct Store {
    conn: Connection,
    salt: [u8; SALT_BYTES],
}

impl Store {
    /// Opens or creates the database at `path`, applying migrations.
    pub(crate) fn open(path: &Path) -> StoreResult<Self> {
        create_parent(path).map_err(|_| "Unable to create the local data directory.")?;
        create_private_file(path).map_err(|_| "Unable to create the local database file.")?;
        let mut conn = Connection::open(path).map_err(db_error)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_error)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_error)?;
        migrate(&mut conn)?;
        let salt = load_or_create_salt(&conn)?;
        Ok(Self { conn, salt })
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Lowercase hex SHA-256 of `salt || patient_id`.
    fn patient_key(&self, patient_id: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(patient_id.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Stores the snapshot's graph and current reading, ignoring readings
    /// already stored for this patient. Readings without a factory timestamp
    /// or a valid value are skipped. Returns the number of rows added.
    pub(crate) fn insert_snapshot(
        &mut self,
        patient_id: &str,
        snapshot: &Snapshot,
        now: DateTime<Utc>,
    ) -> StoreResult<usize> {
        let key = self.patient_key(patient_id);
        let transaction = self.conn.transaction().map_err(db_error)?;
        let mut added = 0;
        // Graph rows first, so a current reading already in the graph keeps
        // the graph origin.
        let rows = snapshot
            .graph
            .iter()
            .map(|sample| (sample, Origin::Graph))
            .chain([(&snapshot.current, Origin::CurrentMeasurement)]);
        for (sample, origin) in rows {
            added += insert_reading(&transaction, &key, sample, origin, now)?;
        }
        replace_sensors(&transaction, snapshot, now)?;
        // Tell `serve` which patient's rows are current. Heartbeat 0 keeps
        // a missing collector row stale until a real heartbeat arrives.
        transaction
            .execute(
                "INSERT INTO collector_state (id, heartbeat_utc, pid, version, patient_key)
                 VALUES (1, 0, 0, '', ?1)
                 ON CONFLICT (id) DO UPDATE SET patient_key = excluded.patient_key",
                [&key],
            )
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(added)
    }

    /// Records one poll attempt. `outcome` is `ok` or a public error code.
    pub(crate) fn record_poll(
        &self,
        outcome: &'static str,
        readings_added: usize,
        at: DateTime<Utc>,
    ) -> StoreResult<()> {
        let added = i64::try_from(readings_added).unwrap_or(i64::MAX);
        self.conn
            .execute(
                "INSERT INTO polls (polled_at_utc, outcome, readings_added) VALUES (?1, ?2, ?3)",
                params![epoch(at), outcome, added],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub(crate) fn write_heartbeat(&self, heartbeat: &Heartbeat) -> StoreResult<()> {
        self.conn
            .execute(
                "INSERT INTO collector_state (id, heartbeat_utc, pid, version) VALUES (1, ?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET heartbeat_utc = excluded.heartbeat_utc,
                     pid = excluded.pid, version = excluded.version",
                params![epoch(heartbeat.at), heartbeat.pid, heartbeat.version],
            )
            .map_err(db_error)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn read_heartbeat(&self) -> StoreResult<Option<Heartbeat>> {
        Ok(self.collector_status()?.map(|status| status.heartbeat))
    }

    pub(crate) fn collector_status(&self) -> StoreResult<Option<CollectorStatus>> {
        let row = self
            .conn
            .query_row(
                "SELECT heartbeat_utc, pid, version, patient_key FROM collector_state WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        Ok(row.and_then(|(at, pid, version, patient_key)| {
            DateTime::from_timestamp(at, 0).map(|at| CollectorStatus {
                heartbeat: Heartbeat { at, pid, version },
                patient_key,
            })
        }))
    }

    /// The newest stored reading with a usable UTC time.
    pub(crate) fn latest_reading(&self, patient_key: &str) -> StoreResult<Option<StoredReading>> {
        let mut readings = self.query_readings(
            "WHERE patient_key = ?1 AND observed_at_utc IS NOT NULL
             ORDER BY observed_at_utc DESC, factory_timestamp DESC LIMIT 1",
            params![patient_key],
        )?;
        Ok(readings.pop())
    }

    pub(crate) fn reading_count(&self, patient_key: &str) -> StoreResult<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM readings WHERE patient_key = ?1",
                [patient_key],
                |row| row.get(0),
            )
            .map_err(db_error)
    }

    pub(crate) fn last_poll(&self) -> StoreResult<Option<PollRecord>> {
        let row = self
            .conn
            .query_row(
                "SELECT polled_at_utc, outcome, readings_added FROM polls
                 ORDER BY polled_at_utc DESC, rowid DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(db_error)?;
        Ok(row.and_then(|(at, outcome, readings_added)| {
            DateTime::from_timestamp(at, 0).map(|at| PollRecord {
                at,
                outcome,
                readings_added,
            })
        }))
    }

    /// Times of all poll attempts with `from <= polled_at_utc <= to`.
    pub(crate) fn poll_times(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> StoreResult<Vec<DateTime<Utc>>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT polled_at_utc FROM polls WHERE polled_at_utc BETWEEN ?1 AND ?2
                 ORDER BY polled_at_utc",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map(params![epoch(from), epoch(to)], |row| row.get::<_, i64>(0))
            .map_err(db_error)?;
        rows.map(|row| {
            let seconds = row.map_err(db_error)?;
            DateTime::from_timestamp(seconds, 0).ok_or("The local database has an invalid time.")
        })
        .collect()
    }

    /// The latest snapshot's sensors, in source order.
    pub(crate) fn sensors(&self) -> StoreResult<Vec<SensorSummary>> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT device_id_masked, serial_number_masked, activation_epoch_seconds,
                        warmup_minutes, product_type_code
                 FROM sensors ORDER BY position",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(SensorSummary {
                    device_id_masked: row.get(0)?,
                    serial_number_masked: row.get(1)?,
                    activation_epoch_seconds: row.get(2)?,
                    warmup_minutes: row.get(3)?,
                    product_type_code: row.get(4)?,
                })
            })
            .map_err(db_error)?;
        rows.map(|row| row.map_err(db_error)).collect()
    }

    fn query_readings(
        &self,
        clause: &str,
        parameters: impl rusqlite::Params,
    ) -> StoreResult<Vec<StoredReading>> {
        let sql = format!(
            "SELECT source_timestamp, factory_timestamp, value_mg_dl, trend_arrow, origin,
                    first_seen_utc
             FROM readings {clause}"
        );
        let mut statement = self.conn.prepare(&sql).map_err(db_error)?;
        let rows = statement
            .query_map(parameters, |row| {
                Ok((
                    GlucoseSample {
                        timestamp: row.get(0)?,
                        factory_timestamp: row.get(1)?,
                        value_in_mg_per_dl: row.get(2)?,
                        trend_arrow: row.get(3)?,
                    },
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(db_error)?;
        rows.map(|row| {
            let (sample, origin, first_seen) = row.map_err(db_error)?;
            let origin =
                Origin::parse(&origin).ok_or("The local database has an unknown origin.")?;
            let first_seen_utc = DateTime::from_timestamp(first_seen, 0)
                .ok_or("The local database has an invalid time.")?;
            Ok(StoredReading {
                sample,
                origin,
                first_seen_utc,
            })
        })
        .collect()
    }

    /// Readings with `from <= observed_at_utc <= to`, oldest first. Readings
    /// without a usable UTC time cannot be placed in a window and are left
    /// out.
    #[cfg(test)]
    pub(crate) fn readings_in_window(
        &self,
        patient_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> StoreResult<Vec<StoredReading>> {
        self.readings_for_key(&self.patient_key(patient_id), from, to)
    }

    /// Readings with `from <= observed_at_utc <= to` for a salted patient
    /// key, oldest first. `serve` only ever has the key, never the raw ID.
    /// Readings without a usable UTC time cannot be placed in a window and
    /// are left out.
    pub(crate) fn readings_for_key(
        &self,
        patient_key: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> StoreResult<Vec<StoredReading>> {
        self.query_readings(
            "WHERE patient_key = ?1 AND observed_at_utc BETWEEN ?2 AND ?3
             ORDER BY observed_at_utc, factory_timestamp",
            params![patient_key, epoch(from), epoch(to)],
        )
    }

    /// Deletes readings and polls older than `retention_days`. Readings are
    /// aged by `COALESCE(observed_at_utc, first_seen_utc)`.
    pub(crate) fn prune(&self, retention_days: u32, now: DateTime<Utc>) -> StoreResult<Pruned> {
        if !(MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&retention_days) {
            return Err("Retention must be between 1 and 3650 days.");
        }
        let cutoff = epoch(now - Duration::days(i64::from(retention_days)));
        let readings = self
            .conn
            .execute(
                "DELETE FROM readings WHERE COALESCE(observed_at_utc, first_seen_utc) < ?1",
                [cutoff],
            )
            .map_err(db_error)?;
        let polls = self
            .conn
            .execute("DELETE FROM polls WHERE polled_at_utc < ?1", [cutoff])
            .map_err(db_error)?;
        Ok(Pruned { readings, polls })
    }
}

fn replace_sensors(
    transaction: &Transaction<'_>,
    snapshot: &Snapshot,
    now: DateTime<Utc>,
) -> StoreResult<()> {
    transaction
        .execute("DELETE FROM sensors", [])
        .map_err(db_error)?;
    for (position, record) in snapshot.active_sensors.iter().enumerate() {
        let sensor = SensorSummary::from_record(record, now);
        transaction
            .execute(
                "INSERT INTO sensors (position, device_id_masked, serial_number_masked,
                     activation_epoch_seconds, warmup_minutes, product_type_code)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    i64::try_from(position).unwrap_or(i64::MAX),
                    sensor.device_id_masked,
                    sensor.serial_number_masked,
                    sensor.activation_epoch_seconds,
                    sensor.warmup_minutes,
                    sensor.product_type_code,
                ],
            )
            .map_err(db_error)?;
    }
    Ok(())
}

fn insert_reading(
    transaction: &Transaction<'_>,
    key: &str,
    sample: &GlucoseSample,
    origin: Origin,
    now: DateTime<Utc>,
) -> StoreResult<usize> {
    let factory = sample.factory_timestamp.trim();
    let value = sample.value_in_mg_per_dl;
    if factory.is_empty() || !value.is_finite() || value <= 0.0 {
        return Ok(0);
    }
    let time = assess_time(Some(&sample.timestamp), Some(factory), now);
    transaction
        .execute(
            "INSERT OR IGNORE INTO readings (patient_key, factory_timestamp, source_timestamp,
                 observed_at_utc, time_basis, value_mg_dl, trend_arrow, origin, first_seen_utc)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                key,
                factory,
                sample.timestamp,
                time.observed_at_utc.map(epoch),
                time_basis_text(time.time_basis),
                value,
                sample.trend_arrow,
                origin.as_str(),
                epoch(now),
            ],
        )
        .map_err(db_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Synthetic identifiers and values only.
    const PATIENT: &str = "synthetic-patient-raw-id-7f3a";
    const OTHER_PATIENT: &str = "synthetic-patient-other-0002";

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sample(timestamp: &str, value: f64) -> GlucoseSample {
        GlucoseSample {
            timestamp: timestamp.to_owned(),
            factory_timestamp: timestamp.to_owned(),
            value_in_mg_per_dl: value,
            trend_arrow: Some(3),
        }
    }

    fn snapshot(graph: &[&str], current: &str) -> Snapshot {
        Snapshot {
            patient_id: PATIENT.to_owned(),
            current: sample(current, 120.0),
            graph: graph.iter().map(|time| sample(time, 110.0)).collect(),
            active_sensors: vec![],
        }
    }

    fn temp_db() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(DB_FILE_NAME);
        (dir, path)
    }

    fn count(store: &Store, table: &str) -> i64 {
        store
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn migrates_an_empty_database_and_keeps_its_salt() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let mode: String = store
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        for table in ["meta", "readings", "polls", "collector_state"] {
            assert_eq!(count(&store, table), i64::from(table == "meta"), "{table}");
        }
        let key = store.patient_key(PATIENT);
        drop(store);

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.patient_key(PATIENT), key);
        assert_eq!(count(&reopened, "meta"), 1);
    }

    #[test]
    fn a_database_from_a_newer_version_is_refused() {
        let (_dir, path) = temp_db();
        drop(Store::open(&path).unwrap());
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(Store::open(&path).is_err());
    }

    #[test]
    fn overlapping_snapshots_are_deduplicated() {
        let (_dir, path) = temp_db();
        let mut store = Store::open(&path).unwrap();
        let now = at("2026-11-05T20:00:00Z");
        let first = snapshot(
            &[
                "2026-11-05T19:00:00Z",
                "2026-11-05T19:15:00Z",
                "2026-11-05T19:30:00Z",
            ],
            "2026-11-05T19:40:00Z",
        );
        assert_eq!(store.insert_snapshot(PATIENT, &first, now).unwrap(), 4);

        // Overlaps three stored readings; the earlier current reading is now
        // in the graph. Invalid rows are skipped.
        let mut second = snapshot(
            &[
                "2026-11-05T19:15:00Z",
                "2026-11-05T19:30:00Z",
                "2026-11-05T19:40:00Z",
                "2026-11-05T19:45:00Z",
            ],
            "2026-11-05T19:55:00Z",
        );
        second.graph.push(sample("2026-11-05T19:50:00Z", 0.0));
        second.graph.push(sample("", 100.0));
        let later = now + Duration::minutes(1);
        assert_eq!(store.insert_snapshot(PATIENT, &second, later).unwrap(), 2);
        assert_eq!(store.insert_snapshot(PATIENT, &second, later).unwrap(), 0);
        assert_eq!(count(&store, "readings"), 6);

        let stored = store
            .readings_in_window(PATIENT, now - Duration::hours(2), now)
            .unwrap();
        let origin_of = |time: &str| {
            stored
                .iter()
                .find(|reading| reading.sample.factory_timestamp == time)
                .unwrap()
                .origin
        };
        // The first sighting wins.
        assert_eq!(
            origin_of("2026-11-05T19:40:00Z"),
            Origin::CurrentMeasurement
        );
        assert_eq!(origin_of("2026-11-05T19:45:00Z"), Origin::Graph);
    }

    #[test]
    fn the_same_patient_gets_different_keys_in_different_databases() {
        let (_first_dir, first_path) = temp_db();
        let (_second_dir, second_path) = temp_db();
        let first = Store::open(&first_path).unwrap();
        let second = Store::open(&second_path).unwrap();
        assert_ne!(first.patient_key(PATIENT), second.patient_key(PATIENT));
        assert_ne!(first.patient_key(PATIENT), first.patient_key(OTHER_PATIENT));
        let key = first.patient_key(PATIENT);
        assert_eq!(key.len(), 64);
        assert!(
            key.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        // Not the unsalted hash.
        assert_ne!(key, format!("{:x}", Sha256::digest(PATIENT.as_bytes())));
    }

    #[test]
    fn the_raw_patient_id_never_reaches_the_database_files() {
        let (_dir, path) = temp_db();
        let mut store = Store::open(&path).unwrap();
        let now = at("2026-11-05T20:00:00Z");
        let snapshot = snapshot(&["2026-11-05T19:30:00Z"], "2026-11-05T19:45:00Z");
        store.insert_snapshot(PATIENT, &snapshot, now).unwrap();
        store.record_poll("ok", 2, now).unwrap();
        store
            .write_heartbeat(&Heartbeat {
                at: now,
                pid: 4242,
                version: "0.0.0-synthetic".to_owned(),
            })
            .unwrap();
        store
            .readings_in_window(PATIENT, now - Duration::hours(1), now)
            .unwrap();

        let files_contain_id = |path: &Path| {
            [
                path.to_path_buf(),
                with_suffix(path, "-wal"),
                with_suffix(path, "-shm"),
            ]
            .iter()
            .filter_map(|file| fs::read(file).ok())
            .any(|bytes| {
                bytes
                    .windows(PATIENT.len())
                    .any(|window| window == PATIENT.as_bytes())
            })
        };
        assert!(!files_contain_id(&path), "raw id found while open");
        drop(store);
        assert!(!files_contain_id(&path), "raw id found after close");
        assert!(!fs::read(&path).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn database_directory_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("created").join("nested");
        let path = data_dir.join(DB_FILE_NAME);
        let mut store = Store::open(&path).unwrap();
        store
            .insert_snapshot(
                PATIENT,
                &snapshot(&[], "2026-11-05T19:45:00Z"),
                at("2026-11-05T20:00:00Z"),
            )
            .unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data_dir), 0o700);
        assert_eq!(mode(data_dir.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        let wal = with_suffix(&path, "-wal");
        assert!(wal.exists());
        assert_eq!(mode(&wal), 0o600);

        // An existing file with a wider mode is tightened on open.
        drop(store);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        drop(Store::open(&path).unwrap());
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn window_queries_include_both_boundaries_and_one_patient() {
        let (_dir, path) = temp_db();
        let mut store = Store::open(&path).unwrap();
        let now = at("2026-11-05T21:00:00Z");
        let mut mine = snapshot(
            &[
                "2026-11-05T19:30:00Z",
                "2026-11-05T18:59:59Z",
                "2026-11-05T19:00:00Z",
                "2026-11-05T20:00:00Z",
                "2026-11-05T20:00:01Z",
            ],
            "2026-11-05T20:30:00Z",
        );
        // No usable UTC time: stored, but outside every window.
        mine.graph.push(GlucoseSample {
            timestamp: "not a date".to_owned(),
            factory_timestamp: "synthetic-bad-factory".to_owned(),
            value_in_mg_per_dl: 100.0,
            trend_arrow: None,
        });
        store.insert_snapshot(PATIENT, &mine, now).unwrap();
        store
            .insert_snapshot(OTHER_PATIENT, &snapshot(&["2026-11-05T19:30:00Z"], ""), now)
            .unwrap();

        let window = store
            .readings_in_window(
                PATIENT,
                at("2026-11-05T19:00:00Z"),
                at("2026-11-05T20:00:00Z"),
            )
            .unwrap();
        let times: Vec<_> = window
            .iter()
            .map(|reading| reading.sample.timestamp.as_str())
            .collect();
        assert_eq!(
            times,
            [
                "2026-11-05T19:00:00Z",
                "2026-11-05T19:30:00Z",
                "2026-11-05T20:00:00Z"
            ]
        );
        assert_eq!(window[0].sample.value_in_mg_per_dl, 110.0);
        assert_eq!(window[0].sample.trend_arrow, Some(3));
        assert_eq!(count(&store, "readings"), 8);
    }

    #[test]
    fn prune_ages_rows_by_observed_time_or_first_seen() {
        let (_dir, path) = temp_db();
        let mut store = Store::open(&path).unwrap();
        let now = at("2026-11-05T20:00:00Z");
        let old = now - Duration::days(100);
        let recent = now - Duration::days(10);
        let bad_time = |factory: &str| Snapshot {
            patient_id: PATIENT.to_owned(),
            current: GlucoseSample {
                timestamp: "not a date".to_owned(),
                factory_timestamp: factory.to_owned(),
                value_in_mg_per_dl: 100.0,
                trend_arrow: None,
            },
            graph: vec![],
            active_sensors: vec![],
        };
        // Observed 100 days ago but first seen today: dropped by observed time.
        store
            .insert_snapshot(PATIENT, &snapshot(&[&old.to_rfc3339()], ""), now)
            .unwrap();
        // Observed 10 days ago: kept.
        store
            .insert_snapshot(PATIENT, &snapshot(&[&recent.to_rfc3339()], ""), recent)
            .unwrap();
        // No UTC time, first seen 100 days ago: dropped.
        store
            .insert_snapshot(PATIENT, &bad_time("synthetic-old-unknown"), old)
            .unwrap();
        // No UTC time, first seen today: kept.
        store
            .insert_snapshot(PATIENT, &bad_time("synthetic-new-unknown"), now)
            .unwrap();
        store.record_poll("ok", 1, old).unwrap();
        store.record_poll("rate_limited", 0, recent).unwrap();
        assert_eq!(count(&store, "readings"), 4);

        let pruned = store.prune(DEFAULT_RETENTION_DAYS, now).unwrap();
        assert_eq!(
            pruned,
            Pruned {
                readings: 2,
                polls: 1
            }
        );
        let kept: Vec<String> = store
            .conn
            .prepare("SELECT factory_timestamp FROM readings ORDER BY factory_timestamp")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            kept,
            [recent.to_rfc3339(), "synthetic-new-unknown".to_owned()]
        );
        assert_eq!(count(&store, "polls"), 1);

        assert!(store.prune(0, now).is_err());
        assert!(store.prune(MAX_RETENTION_DAYS + 1, now).is_err());
        assert!(store.prune(MAX_RETENTION_DAYS, now).is_ok());
    }

    #[test]
    fn heartbeat_and_polls_round_trip() {
        let (_dir, path) = temp_db();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.read_heartbeat().unwrap(), None);
        let first = Heartbeat {
            at: at("2026-11-05T20:00:00Z"),
            pid: 4242,
            version: "0.0.0-synthetic".to_owned(),
        };
        store.write_heartbeat(&first).unwrap();
        assert_eq!(store.read_heartbeat().unwrap(), Some(first));
        let second = Heartbeat {
            at: at("2026-11-05T20:00:30Z"),
            pid: 4343,
            version: "0.0.1-synthetic".to_owned(),
        };
        store.write_heartbeat(&second).unwrap();
        assert_eq!(store.read_heartbeat().unwrap(), Some(second));
        assert_eq!(count(&store, "collector_state"), 1);

        store
            .record_poll("ok", 3, at("2026-11-05T20:00:00Z"))
            .unwrap();
        store
            .record_poll("upstream_unavailable", 0, at("2026-11-05T20:01:00Z"))
            .unwrap();
        assert_eq!(count(&store, "polls"), 2);
    }

    #[test]
    fn deleting_removes_the_database_wal_shm_and_lock_files() {
        let (_dir, path) = temp_db();
        drop(Store::open(&path).unwrap());
        let companions = [
            with_suffix(&path, "-wal"),
            with_suffix(&path, "-shm"),
            lock_path(&path),
        ];
        // SQLite removes -wal and -shm on a clean close; recreate them to
        // cover a crash that left them behind.
        for file in &companions {
            fs::write(file, b"synthetic").unwrap();
        }
        delete_database_files(&path).unwrap();
        assert!(!path.exists());
        for file in &companions {
            assert!(!file.exists(), "{}", file.display());
        }
        // Nothing left to delete is not an error.
        delete_database_files(&path).unwrap();
        assert_eq!(
            lock_path(Path::new("/data/readings.sqlite3")),
            PathBuf::from("/data/readings.sqlite3.lock")
        );
    }

    fn with_sensor(mut snapshot: Snapshot) -> Snapshot {
        snapshot.active_sensors = vec![crate::source::SensorRecord {
            device_id: "synthetic-device-raw-7777".to_owned(),
            serial_number: "SYNTHSERIAL-RAW-0001".to_owned(),
            activation_epoch_seconds: 1_652_400_270,
            warmup_minutes: 60,
            product_type_code: 4,
        }];
        snapshot
    }

    #[test]
    fn migrates_a_version_1_database_to_version_2() {
        let (_dir, path) = temp_db();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO readings VALUES ('synthetic-key', 'f1', 's1', 1762372800, 'offset_known', 120.0, 3, 'graph', 1762372800)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO collector_state (id, heartbeat_utc, pid, version) VALUES (1, 1762372800, 7, '0.0.0-synthetic')",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(store.reading_count("synthetic-key").unwrap(), 1);
        let status = store.collector_status().unwrap().unwrap();
        assert_eq!(status.heartbeat.pid, 7);
        assert_eq!(status.patient_key, None);
        assert!(store.sensors().unwrap().is_empty());
    }

    #[test]
    fn sensors_are_stored_masked_and_replaced_on_each_snapshot() {
        let (_dir, path) = temp_db();
        let mut store = Store::open(&path).unwrap();
        let now = at("2026-11-05T20:00:00Z");
        let first = with_sensor(snapshot(&["2026-11-05T19:30:00Z"], "2026-11-05T19:45:00Z"));
        store.insert_snapshot(PATIENT, &first, now).unwrap();
        assert_eq!(
            store.sensors().unwrap(),
            [SensorSummary {
                device_id_masked: Some("****7777".to_owned()),
                serial_number_masked: Some("****0001".to_owned()),
                activation_epoch_seconds: Some(1_652_400_270),
                warmup_minutes: Some(60),
                product_type_code: Some(4),
            }]
        );
        let status = store.collector_status().unwrap().unwrap();
        assert_eq!(
            status.patient_key.as_deref(),
            Some(store.patient_key(PATIENT).as_str())
        );
        // A row created by an insert alone has an epoch heartbeat: stale.
        assert_eq!(status.heartbeat.at.timestamp(), 0);

        let files_contain = |needle: &str| {
            [path.clone(), with_suffix(&path, "-wal")]
                .iter()
                .filter_map(|file| fs::read(file).ok())
                .any(|bytes| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_bytes())
                })
        };
        assert!(!files_contain("SYNTHSERIAL-RAW"));
        assert!(!files_contain("synthetic-device-raw"));

        let second = snapshot(&["2026-11-05T19:50:00Z"], "2026-11-05T19:55:00Z");
        store.insert_snapshot(PATIENT, &second, now).unwrap();
        assert!(store.sensors().unwrap().is_empty());
    }

    #[test]
    fn latest_reading_last_poll_and_poll_times() {
        let (_dir, path) = temp_db();
        let mut store = Store::open(&path).unwrap();
        let now = at("2026-11-05T20:00:00Z");
        let key = store.patient_key(PATIENT);
        assert_eq!(store.latest_reading(&key).unwrap(), None);
        assert_eq!(store.last_poll().unwrap(), None);
        store
            .insert_snapshot(
                PATIENT,
                &snapshot(
                    &["2026-11-05T19:30:00Z", "2026-11-05T19:45:00Z"],
                    "2026-11-05T19:58:00Z",
                ),
                now,
            )
            .unwrap();
        let latest = store.latest_reading(&key).unwrap().unwrap();
        assert_eq!(latest.sample.timestamp, "2026-11-05T19:58:00Z");
        assert_eq!(latest.origin, Origin::CurrentMeasurement);
        assert_eq!(latest.first_seen_utc, now);
        store
            .record_poll("ok", 3, at("2026-11-05T19:59:00Z"))
            .unwrap();
        store.record_poll("rate_limited", 0, now).unwrap();
        let last = store.last_poll().unwrap().unwrap();
        assert_eq!(
            (last.outcome.as_str(), last.readings_added, last.at),
            ("rate_limited", 0, now)
        );
        assert_eq!(
            store.poll_times(at("2026-11-05T19:59:30Z"), now).unwrap(),
            [now]
        );
        assert_eq!(store.reading_count(&key).unwrap(), 3);
    }

    #[test]
    fn default_path_is_in_a_data_directory() {
        if let Some(path) = default_db_path() {
            assert!(path.ends_with(DB_FILE_NAME));
            assert!(path.to_string_lossy().contains("librelink-mcp-rs"));
        }
    }
}
