//! The `collect` command: polls LibreLinkUp into the local store. See
//! docs/adr/0002-local-store-and-collector.md and docs/collector.md.
//!
//! Logs go to stderr, one line per poll. They never include glucose values,
//! reading timestamps, patient names or IDs, tokens, or paths other than the
//! database location printed once at startup.

use crate::fetch::{Fetcher, latches_until_restart};
use crate::store::{
    DEFAULT_RETENTION_DAYS, Heartbeat, LockError, MAX_RETENTION_DAYS, MIN_RETENTION_DAYS, Store,
    StoreResult, default_db_path, lock_database,
};
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{Instant, sleep};

/// The minimum time between polls. Deliberately has no override.
pub(crate) const POLL_FLOOR: Duration = Duration::from_secs(60);
/// Random jitter added to every wait, from 0 to this many milliseconds.
const MAX_JITTER_MILLIS: u64 = 5_000;
/// The longest the heartbeat goes unwritten while waiting.
const HEARTBEAT_SLICE: Duration = Duration::from_secs(30);
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Exit status for errors that need the user to act (`EX_CONFIG`). The
/// example systemd unit does not restart on it.
pub(crate) const EXIT_LATCHED: i32 = 78;
const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;

const USAGE: &str = "Usage: librelink-mcp-rs collect [--db <path>] [--retention-days <1-3650>]";

/// Wall-clock time for stored records. Production uses the system clock;
/// tests derive it from tokio's paused clock.
#[derive(Clone)]
pub(crate) struct Clock {
    origin: Option<(Instant, DateTime<Utc>)>,
}

impl Clock {
    pub(crate) fn system() -> Self {
        Self { origin: None }
    }

    #[cfg(test)]
    fn following_tokio_from(start: DateTime<Utc>) -> Self {
        Self {
            origin: Some((Instant::now(), start)),
        }
    }

    fn now(&self) -> DateTime<Utc> {
        match self.origin {
            None => Utc::now(),
            Some((instant, start)) => {
                start
                    + chrono::Duration::from_std(instant.elapsed()).unwrap_or(chrono::Duration::MAX)
            }
        }
    }
}

/// Where log lines go: stderr in production, a buffer in tests.
pub(crate) trait LogSink: Send {
    fn line(&mut self, line: &str);
}

struct Stderr;

impl LogSink for Stderr {
    fn line(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// How the loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Exit {
    Shutdown,
    Latched,
    StoreFailed,
}

impl Exit {
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::Shutdown => 0,
            Self::Latched => EXIT_LATCHED,
            Self::StoreFailed => EXIT_FAILURE,
        }
    }
}

fn jitter() -> Duration {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return Duration::ZERO;
    }
    Duration::from_millis(u64::from_le_bytes(bytes) % (MAX_JITTER_MILLIS + 1))
}

/// Resolves once shutdown is requested. A dropped sender never resolves.
async fn shutdown_requested(receiver: &mut watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

type SharedStore = Arc<Mutex<Store>>;

pub(crate) struct Collector {
    pub fetcher: Fetcher,
    pub store: SharedStore,
    pub clock: Clock,
    pub shutdown: watch::Receiver<bool>,
    pub log: Box<dyn LogSink>,
    pub retention_days: u32,
}

impl Collector {
    fn stopping(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// Runs a store operation on the blocking pool. The operation always
    /// finishes, even if shutdown is requested meanwhile.
    async fn store_op<T: Send + 'static>(
        &mut self,
        operation: impl FnOnce(&mut Store) -> StoreResult<T> + Send + 'static,
    ) -> Result<T, Exit> {
        let store = Arc::clone(&self.store);
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = store
                .lock()
                .map_err(|_| "The local database is unavailable.")?;
            operation(&mut guard)
        })
        .await
        .unwrap_or(Err("The local database task failed."));
        result.map_err(|message| {
            self.log.line(&format!("collect: {message}"));
            Exit::StoreFailed
        })
    }

    async fn heartbeat(&mut self) -> Result<(), Exit> {
        let heartbeat = Heartbeat {
            at: self.clock.now(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        self.store_op(move |store| store.write_heartbeat(&heartbeat))
            .await
    }

    /// Records the retention in the database, so `import-csv` applies it.
    async fn record_retention(&mut self) -> Result<(), Exit> {
        let days = self.retention_days;
        self.store_op(move |store| store.set_retention_days(days))
            .await
    }

    async fn prune(&mut self) -> Result<(), Exit> {
        let (days, now) = (self.retention_days, self.clock.now());
        self.store_op(move |store| store.prune(days, now).map(|_| ()))
            .await
    }

    /// One poll. Returns how long to wait before the next one.
    async fn poll(&mut self) -> Result<Duration, Exit> {
        match self.fetcher.fetch().await {
            Ok(fetched) => {
                let now = self.clock.now();
                let snapshot = fetched.snapshot;
                let added = self
                    .store_op(move |store| {
                        let added = store.insert_snapshot(&snapshot.patient_id, &snapshot, now)?;
                        store.record_poll("ok", added, now)?;
                        Ok(added)
                    })
                    .await?;
                self.log
                    .line(&format!("collect: poll ok, readings_added={added}"));
                Ok(POLL_FLOOR + jitter())
            }
            Err(error) => {
                let (code, now) = (error.code, self.clock.now());
                self.store_op(move |store| store.record_poll(code, 0, now))
                    .await?;
                if latches_until_restart(&error) {
                    self.log.line(&format!(
                        "collect: poll failed, error={code}. {}",
                        error.message
                    ));
                    self.log
                        .line("collect: stopping; fix the cause, then restart.");
                    return Err(Exit::Latched);
                }
                let requested = Duration::from_secs(error.retry_after_seconds.unwrap_or(0));
                let wait = POLL_FLOOR.max(requested) + jitter();
                self.log.line(&format!(
                    "collect: poll failed, error={code}, wait_seconds={}",
                    wait.as_secs()
                ));
                Ok(wait)
            }
        }
    }

    /// Waits in slices of at most `HEARTBEAT_SLICE`, writing the heartbeat
    /// after each. Returns false if shutdown was requested.
    async fn wait(&mut self, total: Duration) -> Result<bool, Exit> {
        let deadline = Instant::now() + total;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(true);
            }
            let slice = (deadline - now).min(HEARTBEAT_SLICE);
            let mut shutdown = self.shutdown.clone();
            tokio::select! {
                () = sleep(slice) => {}
                () = shutdown_requested(&mut shutdown) => {}
            }
            if self.stopping() {
                return Ok(false);
            }
            self.heartbeat().await?;
        }
    }

    pub(crate) async fn run(mut self) -> Exit {
        match self.run_inner().await {
            Ok(()) => {
                self.log.line("collect: stopped.");
                Exit::Shutdown
            }
            Err(exit) => exit,
        }
    }

    async fn run_inner(&mut self) -> Result<(), Exit> {
        self.record_retention().await?;
        self.prune().await?;
        let mut last_prune = Instant::now();
        loop {
            if self.stopping() {
                return Ok(());
            }
            self.heartbeat().await?;
            let wait = self.poll().await?;
            if self.stopping() || !self.wait(wait).await? {
                return Ok(());
            }
            if last_prune.elapsed() >= PRUNE_EVERY {
                self.prune().await?;
                last_prune = Instant::now();
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Options {
    pub db: Option<PathBuf>,
    pub retention_days: u32,
}

pub(crate) fn parse_args(args: &[String]) -> Result<Options, &'static str> {
    let mut options = Options {
        db: None,
        retention_days: DEFAULT_RETENTION_DAYS,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => {
                let path = args.next().filter(|path| !path.is_empty()).ok_or(USAGE)?;
                options.db = Some(PathBuf::from(path));
            }
            "--retention-days" => {
                let days: u32 = args
                    .next()
                    .and_then(|days| days.parse().ok())
                    .ok_or(USAGE)?;
                if !(MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&days) {
                    return Err("--retention-days must be between 1 and 3650.");
                }
                options.retention_days = days;
            }
            _ => return Err(USAGE),
        }
    }
    Ok(options)
}

/// Resolves on Ctrl+C, or SIGTERM on Unix.
async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Runs `collect` with the given arguments and returns the exit status.
pub(crate) async fn run_cli(args: &[String], fetcher: Fetcher) -> i32 {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_USAGE;
        }
    };
    let Some(db) = options.db.or_else(default_db_path) else {
        eprintln!("No data directory is available; pass --db <path>.");
        return EXIT_FAILURE;
    };
    eprintln!("collect: using database at {}", db.display());
    let store = match Store::open(&db) {
        Ok(store) => store,
        Err(message) => {
            eprintln!("collect: {message}");
            return EXIT_FAILURE;
        }
    };
    let _lock = match lock_database(&db) {
        Ok(lock) => lock,
        Err(LockError::Held) => {
            eprintln!("collect: another collector is already running for this database.");
            return EXIT_FAILURE;
        }
        Err(LockError::Unavailable) => {
            eprintln!("collect: unable to lock the database.");
            return EXIT_FAILURE;
        }
    };
    let (stop, shutdown) = watch::channel(false);
    tokio::spawn(async move {
        termination_signal().await;
        let _ = stop.send(true);
    });
    Collector {
        fetcher,
        store: Arc::new(Mutex::new(store)),
        clock: Clock::system(),
        shutdown,
        log: Box::new(Stderr),
        retention_days: options.retention_days,
    }
    .run()
    .await
    .code()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FetchError;
    use crate::source::{DataSource, GlucoseSample, ReadFuture, Snapshot};
    use std::borrow::Cow;
    use std::collections::VecDeque;
    use tempfile::TempDir;

    // Synthetic data only. The values and ID are distinctive so the log
    // check below cannot pass by accident.
    const PATIENT_ID: &str = "SYNTHETIC-PID-collect-x91";
    const PATIENT_NAME: &str = "Pat Synthetic";
    const VALUES: [f64; 3] = [287.5, 213.25, 141.75];

    fn start() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn synthetic_snapshot() -> Snapshot {
        let sample = |minutes_before: i64, value: f64| {
            let time = (start() - chrono::Duration::minutes(minutes_before)).to_rfc3339();
            GlucoseSample {
                timestamp: time.clone(),
                factory_timestamp: time,
                value_in_mg_per_dl: value,
                trend_arrow: Some(3),
                stored_time: None,
            }
        };
        Snapshot {
            patient_id: PATIENT_ID.to_owned(),
            current: sample(2, VALUES[0]),
            graph: vec![sample(20, VALUES[1]), sample(35, VALUES[2])],
            active_sensors: vec![],
        }
    }

    fn failure(code: &'static str, retry_after_seconds: Option<u64>) -> FetchError {
        FetchError {
            code,
            message: Cow::Borrowed("Synthetic failure message."),
            retry_after_seconds,
        }
    }

    /// Answers with scripted results, then repeats `fallback`, and records
    /// when it was read.
    struct ScriptedSource {
        script: Mutex<VecDeque<Result<Snapshot, FetchError>>>,
        fallback: Result<Snapshot, FetchError>,
        reads: Arc<Mutex<Vec<Instant>>>,
    }

    impl DataSource for ScriptedSource {
        fn read(&self) -> ReadFuture<'_> {
            self.reads.lock().unwrap().push(Instant::now());
            let next = self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.fallback.clone());
            Box::pin(async move { next })
        }
    }

    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<String>>>);

    impl LogSink for Lines {
        fn line(&mut self, line: &str) {
            self.0.lock().unwrap().push(line.to_owned());
        }
    }

    struct Harness {
        _dir: TempDir,
        db: PathBuf,
        store: SharedStore,
        reads: Arc<Mutex<Vec<Instant>>>,
        logs: Lines,
        stop: watch::Sender<bool>,
        clock: Clock,
    }

    impl Harness {
        fn reads(&self) -> Vec<Instant> {
            self.reads.lock().unwrap().clone()
        }

        fn polls(&self) -> Vec<(String, i64)> {
            let store = self.store.lock().unwrap();
            store
                .connection()
                .prepare("SELECT outcome, readings_added FROM polls ORDER BY rowid")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }

        fn heartbeat_age(&self) -> chrono::Duration {
            let heartbeat = self
                .store
                .lock()
                .unwrap()
                .read_heartbeat()
                .unwrap()
                .unwrap();
            self.clock.now() - heartbeat.at
        }
    }

    fn collector(
        script: Vec<Result<Snapshot, FetchError>>,
        fallback: Result<Snapshot, FetchError>,
    ) -> (Collector, Harness) {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("readings.sqlite3");
        let store = Arc::new(Mutex::new(Store::open(&db).unwrap()));
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = ScriptedSource {
            script: Mutex::new(script.into()),
            fallback,
            reads: Arc::clone(&reads),
        };
        let (stop, shutdown) = watch::channel(false);
        let logs = Lines::default();
        let clock = Clock::following_tokio_from(start());
        let collector = Collector {
            fetcher: Fetcher::new(Box::new(source)),
            store: Arc::clone(&store),
            clock: clock.clone(),
            shutdown,
            log: Box::new(logs.clone()),
            retention_days: DEFAULT_RETENTION_DAYS,
        };
        let harness = Harness {
            _dir: dir,
            db,
            store,
            reads,
            logs,
            stop,
            clock,
        };
        (collector, harness)
    }

    #[tokio::test(start_paused = true)]
    async fn polls_are_at_least_60_seconds_apart_with_bounded_jitter() {
        let (collector, harness) = collector(vec![], Ok(synthetic_snapshot()));
        let task = tokio::spawn(collector.run());
        sleep(Duration::from_secs(12 * 66)).await;
        harness.stop.send(true).unwrap();
        assert_eq!(task.await.unwrap(), Exit::Shutdown);

        let reads = harness.reads();
        assert!(reads.len() >= 12, "{} polls", reads.len());
        for pair in reads.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(gap >= POLL_FLOOR, "{gap:?}");
            assert!(
                gap <= POLL_FLOOR + Duration::from_millis(MAX_JITTER_MILLIS),
                "{gap:?}"
            );
        }
        // Not every gap is exactly the floor: jitter is applied.
        assert!(reads.windows(2).any(|pair| pair[1] - pair[0] > POLL_FLOOR));
    }

    #[test]
    fn jitter_stays_within_five_seconds() {
        for _ in 0..1000 {
            assert!(jitter() <= Duration::from_millis(MAX_JITTER_MILLIS));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_cooldown_keeps_the_heartbeat_fresh_and_polls_once() {
        let (collector, harness) = collector(
            vec![Err(failure("rate_limited", Some(900)))],
            Ok(synthetic_snapshot()),
        );
        let task = tokio::spawn(collector.run());
        // Let the first poll happen, then sample the heartbeat every second.
        sleep(Duration::from_millis(1)).await;
        for second in 1..=899 {
            sleep(Duration::from_secs(1)).await;
            assert_eq!(harness.reads().len(), 1, "extra poll at {second} s");
            let age = harness.heartbeat_age();
            assert!(
                age <= chrono::Duration::seconds(31),
                "heartbeat {age} old at {second} s"
            );
        }
        sleep(Duration::from_secs(10)).await;
        harness.stop.send(true).unwrap();
        assert_eq!(task.await.unwrap(), Exit::Shutdown);

        let reads = harness.reads();
        assert_eq!(reads.len(), 2);
        let gap = reads[1] - reads[0];
        assert!(gap >= Duration::from_secs(900), "{gap:?}");
        assert!(gap <= Duration::from_secs(905), "{gap:?}");
        assert_eq!(
            harness.polls(),
            [("rate_limited".to_owned(), 0), ("ok".to_owned(), 3)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn latched_errors_exit_78_after_one_poll() {
        for code in [
            "authentication_failed",
            "client_version_rejected",
            "request_rejected",
        ] {
            let (collector, harness) = collector(vec![], Err(failure(code, None)));
            let exit = collector.run().await;
            assert_eq!(exit, Exit::Latched);
            assert_eq!(exit.code(), 78);
            assert_eq!(harness.reads().len(), 1, "{code}");
            assert_eq!(harness.polls(), [(code.to_owned(), 0)]);
            let logs = harness.logs.0.lock().unwrap().join("\n");
            assert!(logs.contains("Synthetic failure message."), "{code}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_recorded_with_no_readings_added() {
        let (collector, harness) = collector(
            vec![
                Err(failure("upstream_unavailable", Some(60))),
                Err(failure("api_changed", Some(60))),
                Err(failure("configuration_error", None)),
            ],
            Ok(synthetic_snapshot()),
        );
        let task = tokio::spawn(collector.run());
        sleep(Duration::from_secs(4 * 66)).await;
        harness.stop.send(true).unwrap();
        assert_eq!(task.await.unwrap(), Exit::Shutdown);
        let polls = harness.polls();
        assert_eq!(
            polls[..4],
            [
                ("upstream_unavailable".to_owned(), 0),
                ("api_changed".to_owned(), 0),
                ("configuration_error".to_owned(), 0),
                ("ok".to_owned(), 3),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_a_wait_exits_0_without_another_poll() {
        let (collector, harness) = collector(vec![], Ok(synthetic_snapshot()));
        let task = tokio::spawn(collector.run());
        sleep(Duration::from_secs(45)).await;
        harness.stop.send(true).unwrap();
        let exit = task.await.unwrap();
        assert_eq!(exit, Exit::Shutdown);
        assert_eq!(exit.code(), 0);
        assert_eq!(harness.reads().len(), 1);
        assert_eq!(harness.polls(), [("ok".to_owned(), 3)]);
    }

    #[tokio::test(start_paused = true)]
    async fn logs_never_contain_values_ids_names_or_reading_times() {
        let (collector, harness) = collector(
            vec![
                Ok(synthetic_snapshot()),
                Err(failure("rate_limited", Some(120))),
            ],
            Err(failure("authentication_failed", None)),
        );
        assert_eq!(collector.run().await, Exit::Latched);
        let logs = harness.logs.0.lock().unwrap().join("\n");
        assert!(logs.contains("readings_added=3"));
        assert!(logs.contains("error=rate_limited, wait_seconds=12"));
        for forbidden in [
            "287",
            "213",
            "141",
            PATIENT_ID,
            "x91",
            PATIENT_NAME,
            "2026-11-05",
            "19:",
        ] {
            assert!(
                !logs.contains(forbidden),
                "log contains {forbidden:?}:\n{logs}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn startup_prunes_expired_rows() {
        let (collector, harness) = collector(vec![], Ok(synthetic_snapshot()));
        let old = start() - chrono::Duration::days(DEFAULT_RETENTION_DAYS as i64 + 10);
        {
            let mut store = harness.store.lock().unwrap();
            let mut expired = synthetic_snapshot();
            expired.graph.clear();
            expired.current.timestamp = old.to_rfc3339();
            expired.current.factory_timestamp = old.to_rfc3339();
            assert_eq!(store.insert_snapshot(PATIENT_ID, &expired, old).unwrap(), 1);
            store.record_poll("ok", 1, old).unwrap();
        }
        let task = tokio::spawn(collector.run());
        sleep(Duration::from_secs(1)).await;
        harness.stop.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(harness.polls(), [("ok".to_owned(), 3)]);
        // The retention is recorded for import-csv.
        let store = harness.store.lock().unwrap();
        assert_eq!(
            store.retention_days().unwrap(),
            Some(DEFAULT_RETENTION_DAYS)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_collector_is_refused_while_the_lock_is_held() {
        let (_collector, harness) = collector(vec![], Ok(synthetic_snapshot()));
        let held = lock_database(&harness.db).unwrap();
        assert_eq!(lock_database(&harness.db).unwrap_err(), LockError::Held);
        assert!(crate::store::database_locked(&harness.db).unwrap());

        // The command itself refuses before polling.
        let reads = Arc::new(Mutex::new(Vec::new()));
        let source = ScriptedSource {
            script: Mutex::new(VecDeque::new()),
            fallback: Ok(synthetic_snapshot()),
            reads: Arc::clone(&reads),
        };
        let args = ["--db".to_owned(), harness.db.display().to_string()];
        let code = run_cli(&args, Fetcher::new(Box::new(source))).await;
        assert_eq!(code, EXIT_FAILURE);
        assert!(reads.lock().unwrap().is_empty());

        drop(held);
        assert!(!crate::store::database_locked(&harness.db).unwrap());
        assert!(lock_database(&harness.db).is_ok());
    }

    #[test]
    fn arguments_are_parsed_strictly() {
        let parse = |args: &[&str]| {
            parse_args(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(
            parse(&[]).unwrap(),
            Options {
                db: None,
                retention_days: 90
            }
        );
        assert_eq!(
            parse(&["--db", "/synthetic/x.db", "--retention-days", "3650"]).unwrap(),
            Options {
                db: Some(PathBuf::from("/synthetic/x.db")),
                retention_days: 3650
            }
        );
        for bad in [
            &["--retention-days", "0"][..],
            &["--retention-days", "3651"],
            &["--retention-days", "ten"],
            &["--db"],
            &["--interval", "10"],
            &["--force"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
