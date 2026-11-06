//! `import-csv`: load a locally saved LibreView CSV export into the local
//! store (ADR 0003). Offline: nothing is uploaded and LibreLinkUp is never
//! contacted.
//!
//! Only the device timestamp, record type, and historic and scan glucose
//! columns are read. The metadata line (which names the account holder),
//! the serial number, insulin, food, notes, strip, and ketone columns are
//! never kept beyond parsing the record that holds them, and never stored
//! or printed. Output is counts only.

use crate::store::{
    self, CsvReading, DEFAULT_RETENTION_DAYS, LockError, Origin, Store, default_db_path,
};
use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Files larger than this are refused, by size and while reading.
pub(crate) const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Rows per write transaction, so the collector's writes are never blocked
/// for long.
const BATCH_ROWS: usize = 1000;
/// Rows timestamped further ahead of now than this are skipped.
const FUTURE_TOLERANCE_MINUTES: i64 = 5;
const MMOL_TO_MG_DL: f64 = 18.0182;

const EXIT_OK: i32 = 0;
const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;

const USAGE: &str = "Usage: librelink-mcp-rs import-csv <file> --timezone <IANA zone> \
[--date-format mdy|dmy] [--db <path>] [--dry-run]";

const TIMESTAMP_COLUMN: &str = "Device Timestamp";
const RECORD_TYPE_COLUMN: &str = "Record Type";

/// The order of the day and month in device timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DateOrder {
    Mdy,
    Dmy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unit {
    MgDl,
    MmolL,
}

#[derive(Debug)]
pub(crate) struct Options {
    pub file: PathBuf,
    pub timezone: Tz,
    pub date_format: Option<DateOrder>,
    pub db: Option<PathBuf>,
    pub dry_run: bool,
}

pub(crate) fn parse_args(args: &[String]) -> Result<Options, &'static str> {
    let mut file = None;
    let mut timezone = None;
    let mut date_format = None;
    let mut db = None;
    let mut dry_run = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--timezone" => {
                let name = args.next().ok_or(USAGE)?;
                timezone = Some(name.parse::<Tz>().map_err(
                    |_| "--timezone must be an IANA time zone name, such as America/Detroit.",
                )?);
            }
            "--date-format" => {
                date_format = Some(match args.next().map(String::as_str) {
                    Some("mdy") => DateOrder::Mdy,
                    Some("dmy") => DateOrder::Dmy,
                    _ => return Err("--date-format must be mdy or dmy."),
                });
            }
            "--db" => {
                let path = args.next().filter(|path| !path.is_empty()).ok_or(USAGE)?;
                db = Some(PathBuf::from(path));
            }
            "--dry-run" => dry_run = true,
            flag if flag.starts_with("--") => return Err(USAGE),
            path if file.is_none() && !path.is_empty() => file = Some(PathBuf::from(path)),
            _ => return Err(USAGE),
        }
    }
    Ok(Options {
        file: file.ok_or(USAGE)?,
        timezone: timezone
            .ok_or("--timezone is required: the export's timestamps have no offset.")?,
        date_format,
        db,
        dry_run,
    })
}

/// Counts reported after an import. Values are never reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Data rows after the header.
    pub rows_read: usize,
    pub imported: usize,
    pub duplicate: usize,
    pub outside_retention: usize,
    pub dst_ambiguous: usize,
    pub dst_nonexistent: usize,
    pub unparseable_time: usize,
    pub future_time: usize,
    pub invalid_value: usize,
    pub ignored_record_type: usize,
}

/// Why a file could not be imported at all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileError {
    TooLarge,
    Unreadable,
    /// The CSV could not be parsed at this line.
    Malformed(u64),
    MissingColumns,
    MixedUnits,
    AmbiguousDateOrder,
    ConflictingDateOrder,
}

impl FileError {
    fn message(&self) -> String {
        match self {
            Self::TooLarge => "The file is larger than 50 MiB; refusing to import it.".to_owned(),
            Self::Unreadable => "The file could not be read.".to_owned(),
            Self::Malformed(line) => format!("The file could not be parsed as CSV at line {line}."),
            Self::MissingColumns => format!(
                "The file does not have the expected LibreView columns: \"{TIMESTAMP_COLUMN}\", \
                 \"{RECORD_TYPE_COLUMN}\", \"Historic Glucose mg/dL\" (or mmol/L), and \
                 \"Scan Glucose mg/dL\" (or mmol/L). Only English LibreView exports are supported."
            ),
            Self::MixedUnits => {
                "The historic and scan glucose columns use different units.".to_owned()
            }
            Self::AmbiguousDateOrder => "The date order cannot be told from the data: every date \
                 could be month-first or day-first. Pass --date-format mdy or --date-format dmy."
                .to_owned(),
            Self::ConflictingDateOrder => "The file has both month-first and day-first dates; \
                 refusing to guess."
                .to_owned(),
        }
    }
}

/// A row of record type 0 or 1, with only the fields the import uses.
#[derive(Debug, PartialEq, Eq)]
struct RawRow {
    timestamp: String,
    record_type: u8,
    value: String,
}

#[derive(Debug)]
struct ParsedFile {
    unit: Unit,
    rows: Vec<RawRow>,
    rows_read: usize,
    ignored_record_type: usize,
}

/// Fails once more than `limit` bytes have been read.
struct Limited<R> {
    inner: R,
    limit: u64,
    read: u64,
    exceeded: Arc<AtomicBool>,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.read += count as u64;
        if self.read > self.limit {
            self.exceeded.store(true, Ordering::SeqCst);
            return Err(io::Error::other("the file exceeds the size limit"));
        }
        Ok(count)
    }
}

fn field(record: &csv::ByteRecord, index: usize) -> &str {
    record
        .get(index)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .map_or("", str::trim)
}

struct Columns {
    timestamp: usize,
    record_type: usize,
    historic: usize,
    scan: usize,
    unit: Unit,
}

fn glucose_column(names: &[String], kind: &str) -> Option<(usize, Unit)> {
    [("mg/dL", Unit::MgDl), ("mmol/L", Unit::MmolL)]
        .into_iter()
        .find_map(|(suffix, unit)| {
            let wanted = format!("{kind} Glucose {suffix}");
            names
                .iter()
                .position(|name| name == &wanted)
                .map(|index| (index, unit))
        })
}

fn header_columns(record: &csv::ByteRecord) -> Result<Option<Columns>, FileError> {
    let names: Vec<String> = (0..record.len())
        .map(|index| {
            field(record, index)
                .trim_start_matches('\u{feff}')
                .to_owned()
        })
        .collect();
    let position = |wanted: &str| names.iter().position(|name| name == wanted);
    let Some(timestamp) = position(TIMESTAMP_COLUMN) else {
        return Ok(None);
    };
    let record_type = position(RECORD_TYPE_COLUMN).ok_or(FileError::MissingColumns)?;
    let (historic, historic_unit) =
        glucose_column(&names, "Historic").ok_or(FileError::MissingColumns)?;
    let (scan, scan_unit) = glucose_column(&names, "Scan").ok_or(FileError::MissingColumns)?;
    if historic_unit != scan_unit {
        return Err(FileError::MixedUnits);
    }
    Ok(Some(Columns {
        timestamp,
        record_type,
        historic,
        scan,
        unit: historic_unit,
    }))
}

/// Reads the rows the import uses. One record buffer is reused, so the
/// metadata line and every unused column are overwritten by the next record.
fn read_rows<R: Read>(reader: R, limit: u64) -> Result<ParsedFile, FileError> {
    let exceeded = Arc::new(AtomicBool::new(false));
    let limited = Limited {
        inner: reader,
        limit,
        read: 0,
        exceeded: Arc::clone(&exceeded),
    };
    let mut csv = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(limited);
    let mut record = csv::ByteRecord::new();
    let read_error = |error: csv::Error, exceeded: &AtomicBool| {
        if exceeded.load(Ordering::SeqCst) {
            FileError::TooLarge
        } else {
            match error.position() {
                Some(position) => FileError::Malformed(position.line()),
                None => FileError::Unreadable,
            }
        }
    };
    // The header is on line 2 of a LibreView export, after the metadata
    // line; allow it on line 1 too.
    let mut columns = None;
    for _ in 0..2 {
        if !csv
            .read_byte_record(&mut record)
            .map_err(|error| read_error(error, &exceeded))?
        {
            break;
        }
        if let Some(found) = header_columns(&record)? {
            columns = Some(found);
            break;
        }
    }
    let columns = columns.ok_or(FileError::MissingColumns)?;
    let mut parsed = ParsedFile {
        unit: columns.unit,
        rows: Vec::new(),
        rows_read: 0,
        ignored_record_type: 0,
    };
    while csv
        .read_byte_record(&mut record)
        .map_err(|error| read_error(error, &exceeded))?
    {
        parsed.rows_read += 1;
        let value_column = match field(&record, columns.record_type) {
            "0" => columns.historic,
            "1" => columns.scan,
            _ => {
                parsed.ignored_record_type += 1;
                continue;
            }
        };
        parsed.rows.push(RawRow {
            timestamp: field(&record, columns.timestamp).to_owned(),
            record_type: if value_column == columns.historic {
                0
            } else {
                1
            },
            value: field(&record, value_column).to_owned(),
        });
    }
    Ok(parsed)
}

/// A device timestamp split into numbers, before the date order is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LocalStamp {
    first: u32,
    second: u32,
    year: i32,
    hour: u32,
    minute: u32,
    seconds: u32,
}

/// Splits `MM-DD-YYYY hh:mm AM/PM` or `DD-MM-YYYY HH:MM` (also with `/` or
/// `.` between date parts, and optional seconds).
fn split_stamp(text: &str) -> Option<LocalStamp> {
    let mut parts = text.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let meridiem = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let numbers = |text: &str, separators: &[char]| -> Option<Vec<u32>> {
        text.split(separators)
            .map(|part| {
                (!part.is_empty() && part.len() <= 4 && part.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| part.parse().ok())
                    .flatten()
            })
            .collect()
    };
    let year_text = date.rsplit(['-', '/', '.']).next()?;
    let [first, second, year] = <[u32; 3]>::try_from(numbers(date, &['-', '/', '.'])?).ok()?;
    let (hour, minute, seconds) = match numbers(time, &[':'])?.as_slice() {
        [hour, minute] => (*hour, *minute, 0),
        [hour, minute, seconds] => (*hour, *minute, *seconds),
        _ => return None,
    };
    if year_text.len() != 4 || minute > 59 || seconds > 59 {
        return None;
    }
    let hour = match meridiem.map(str::to_ascii_uppercase).as_deref() {
        None if hour <= 23 => hour,
        Some("AM") if (1..=12).contains(&hour) => hour % 12,
        Some("PM") if (1..=12).contains(&hour) => hour % 12 + 12,
        _ => return None,
    };
    Some(LocalStamp {
        first,
        second,
        year: i32::try_from(year).ok()?,
        hour,
        minute,
        seconds,
    })
}

fn detect_order(rows: &[RawRow], given: Option<DateOrder>) -> Result<DateOrder, FileError> {
    if let Some(order) = given {
        return Ok(order);
    }
    let stamps: Vec<LocalStamp> = rows
        .iter()
        .filter_map(|row| split_stamp(&row.timestamp))
        .collect();
    let day_first = stamps.iter().any(|stamp| stamp.first > 12);
    let month_first = stamps.iter().any(|stamp| stamp.second > 12);
    match (month_first, day_first) {
        (true, true) => Err(FileError::ConflictingDateOrder),
        (true, false) => Ok(DateOrder::Mdy),
        (false, true) => Ok(DateOrder::Dmy),
        // No rows to place: the order does not matter.
        (false, false) if stamps.is_empty() => Ok(DateOrder::Mdy),
        (false, false) => Err(FileError::AmbiguousDateOrder),
    }
}

fn naive_time(stamp: LocalStamp, order: DateOrder) -> Option<NaiveDateTime> {
    let (month, day) = match order {
        DateOrder::Mdy => (stamp.first, stamp.second),
        DateOrder::Dmy => (stamp.second, stamp.first),
    };
    NaiveDate::from_ymd_opt(stamp.year, month, day)?.and_hms_opt(
        stamp.hour,
        stamp.minute,
        stamp.seconds,
    )
}

fn mg_dl(text: &str, unit: Unit) -> Option<f64> {
    let value: f64 = text.parse().ok()?;
    let value = match unit {
        Unit::MgDl => value,
        Unit::MmolL => (value * MMOL_TO_MG_DL * 10.0).round() / 10.0,
    };
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Turns parsed rows into readings to store, counting every row it skips.
/// Rows repeated within the file are counted as duplicates.
fn convert(
    parsed: &ParsedFile,
    timezone: Tz,
    order: DateOrder,
    retention_days: u32,
    now: DateTime<Utc>,
) -> (Vec<CsvReading>, Counts) {
    let mut counts = Counts {
        rows_read: parsed.rows_read,
        ignored_record_type: parsed.ignored_record_type,
        ..Counts::default()
    };
    let cutoff = now - Duration::days(i64::from(retention_days));
    let latest = now + Duration::minutes(FUTURE_TOLERANCE_MINUTES);
    let mut seen = HashSet::new();
    let mut readings = Vec::new();
    for row in &parsed.rows {
        let Some(local) = split_stamp(&row.timestamp).and_then(|stamp| naive_time(stamp, order))
        else {
            counts.unparseable_time += 1;
            continue;
        };
        let observed = match timezone.from_local_datetime(&local) {
            LocalResult::Single(time) => time.with_timezone(&Utc),
            LocalResult::Ambiguous(_, _) => {
                counts.dst_ambiguous += 1;
                continue;
            }
            LocalResult::None => {
                counts.dst_nonexistent += 1;
                continue;
            }
        };
        if observed > latest {
            counts.future_time += 1;
            continue;
        }
        if observed < cutoff {
            counts.outside_retention += 1;
            continue;
        }
        let Some(value) = mg_dl(&row.value, parsed.unit) else {
            counts.invalid_value += 1;
            continue;
        };
        let key = format!("csv:{}:{}", row.record_type, row.timestamp);
        if !seen.insert(key.clone()) {
            counts.duplicate += 1;
            continue;
        }
        readings.push(CsvReading {
            key,
            source_timestamp: row.timestamp.clone(),
            observed_at_utc: observed,
            value_mg_dl: value,
            origin: if row.record_type == 0 {
                Origin::LibreviewCsvHistoric
            } else {
                Origin::LibreviewCsvScan
            },
        });
    }
    (readings, counts)
}

/// Reads `file`, refusing anything over the size limit by metadata and,
/// whatever the metadata says, while reading.
fn read_file(file: &std::path::Path) -> Result<ParsedFile, FileError> {
    let handle = File::open(file).map_err(|_| FileError::Unreadable)?;
    let metadata = handle.metadata().map_err(|_| FileError::Unreadable)?;
    if metadata.is_file() && metadata.len() > MAX_FILE_BYTES {
        return Err(FileError::TooLarge);
    }
    read_rows(handle, MAX_FILE_BYTES)
}

/// The import itself. Writes the counts to `out` and returns them.
pub(crate) fn import(
    options: &Options,
    now: DateTime<Utc>,
    out: &mut dyn Write,
) -> Result<Counts, String> {
    let db = options
        .db
        .clone()
        .or_else(default_db_path)
        .ok_or("No data directory is available; pass --db <path>.")?;
    if options.dry_run {
        let retention = store::read_retention_days(&db)?.unwrap_or(DEFAULT_RETENTION_DAYS);
        let parsed = read_file(&options.file).map_err(|error| error.message())?;
        let order = detect_order(&parsed.rows, options.date_format).map_err(|e| e.message())?;
        let (readings, mut counts) = convert(&parsed, options.timezone, order, retention, now);
        counts.imported = readings.len();
        report(out, &counts, retention, true)?;
        return Ok(counts);
    }

    let collect_first = "run collect at least once first, so the import knows which patient \
                         the readings belong to.";
    if !db.exists() {
        return Err(format!("No local database exists yet; {collect_first}"));
    }
    let _lock = match store::lock_import(&db) {
        Ok(lock) => lock,
        Err(LockError::Held) => {
            return Err("Another import is already running for this database.".to_owned());
        }
        Err(LockError::Unavailable) => return Err("Unable to take the import lock.".to_owned()),
    };
    let mut store = Store::open(&db)?;
    let patient_key = store
        .collector_status()?
        .and_then(|status| status.patient_key)
        .ok_or_else(|| format!("The local database has no collected patient; {collect_first}"))?;
    let retention = store.retention_days()?.unwrap_or(DEFAULT_RETENTION_DAYS);
    let parsed = read_file(&options.file).map_err(|error| error.message())?;
    let order = detect_order(&parsed.rows, options.date_format).map_err(|e| e.message())?;
    let (readings, mut counts) = convert(&parsed, options.timezone, order, retention, now);
    for batch in readings.chunks(BATCH_ROWS) {
        let added = store.insert_csv_readings(&patient_key, batch, now)?;
        counts.imported += added;
        counts.duplicate += batch.len() - added;
    }
    report(out, &counts, retention, false)?;
    Ok(counts)
}

fn report(
    out: &mut dyn Write,
    counts: &Counts,
    retention: u32,
    dry_run: bool,
) -> Result<(), String> {
    let mut text = String::new();
    if dry_run {
        text.push_str(
            "Dry run: nothing was written. Duplicates are counted within the file only.\n",
        );
    }
    let imported = if dry_run { "would import" } else { "imported" };
    text.push_str(&format!(
        "rows read: {}\n{imported}: {}\nduplicate: {}\nskipped:\n  \
         outside_retention: {}\n  dst_ambiguous: {}\n  dst_nonexistent: {}\n  \
         unparseable_time: {}\n  future_time: {}\n  invalid_value: {}\n  \
         ignored_record_type: {}\n",
        counts.rows_read,
        counts.imported,
        counts.duplicate,
        counts.outside_retention,
        counts.dst_ambiguous,
        counts.dst_nonexistent,
        counts.unparseable_time,
        counts.future_time,
        counts.invalid_value,
        counts.ignored_record_type,
    ));
    if counts.outside_retention > 0 {
        text.push_str(&format!(
            "Rows older than the database's retention ({retention} days) were skipped. To keep \
             longer history, run collect with a larger --retention-days.\n"
        ));
    }
    out.write_all(text.as_bytes())
        .map_err(|_| "Unable to write the import summary.".to_owned())
}

/// Runs `import-csv` with the given arguments and returns the exit status.
pub(crate) fn run_cli(args: &[String]) -> i32 {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_USAGE;
        }
    };
    match import(&options, Utc::now(), &mut io::stdout()) {
        Ok(_) => EXIT_OK,
        Err(message) => {
            eprintln!("import-csv: {message}");
            EXIT_FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{GlucoseSample, Snapshot};
    use std::io::Cursor;
    use std::path::Path;

    // Synthetic markers. None may ever reach the database or the output.
    const NAME: &str = "Synthetic Q. Testperson";
    const SERIAL: &str = "SYNTHSER-ZZ-0042";
    const MARKERS: [&str; 6] = [
        "MARKER-NOTES-7Q",
        "MARKER-INSULIN-7Q",
        "MARKER-CARBS-7Q",
        "MARKER-FOOD-7Q",
        "MARKER-STRIP-7Q",
        "MARKER-KETONE-7Q",
    ];
    const PATIENT_ID: &str = "SYNTHETIC-PID-csv-q7";

    const HEADER: [&str; 21] = [
        "Device",
        "Serial Number",
        "Device Timestamp",
        "Record Type",
        "Historic Glucose mg/dL",
        "Scan Glucose mg/dL",
        "Non-numeric Rapid-Acting Insulin",
        "Rapid-Acting Insulin (units)",
        "Non-numeric Food",
        "Carbohydrates (grams)",
        "Carbohydrates (servings)",
        "Non-numeric Long-Acting Insulin",
        "Long-Acting Insulin Value (units)",
        "Notes",
        "Strip Glucose mg/dL",
        "Ketone mmol/L",
        "Meal Insulin (units)",
        "Correction Insulin (units)",
        "User Change Insulin (units)",
        "Blood Ketone mmol/L",
        "Scan Ketone mmol/L",
    ];

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn detroit() -> Tz {
        "America/Detroit".parse().unwrap()
    }

    /// One data row with every unused column filled with a marker.
    fn row(timestamp: &str, record_type: &str, historic: &str, scan: &str) -> String {
        let [notes, insulin, carbs, food, strip, ketone] = MARKERS;
        [
            "FreeStyle Synthetic",
            SERIAL,
            timestamp,
            record_type,
            historic,
            scan,
            insulin,
            insulin,
            food,
            carbs,
            carbs,
            insulin,
            insulin,
            notes,
            strip,
            ketone,
            insulin,
            insulin,
            insulin,
            ketone,
            ketone,
        ]
        .join(",")
    }

    fn export_with(header: &[&str], rows: &[String], newline: &str) -> String {
        let mut lines = vec![
            format!("Glucose Data,Generated on,11-06-2026 11:00 AM UTC,Generated by,{NAME}"),
            header.join(","),
        ];
        lines.extend(rows.iter().cloned());
        lines.join(newline) + newline
    }

    fn export(rows: &[String]) -> String {
        export_with(&HEADER, rows, "\r\n")
    }

    struct Setup {
        dir: tempfile::TempDir,
        db: PathBuf,
    }

    impl Setup {
        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }

        /// Most fixtures use dates that fit either order, so they name it.
        fn options(&self, file: &Path) -> Options {
            Options {
                file: file.to_path_buf(),
                timezone: detroit(),
                date_format: Some(DateOrder::Mdy),
                db: Some(self.db.clone()),
                dry_run: false,
            }
        }
    }

    /// A database the collector has written once, so it has a patient key.
    fn collected() -> Setup {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("readings.sqlite3");
        let snapshot = Snapshot {
            patient_id: PATIENT_ID.to_owned(),
            current: GlucoseSample {
                timestamp: String::new(),
                factory_timestamp: String::new(),
                value_in_mg_per_dl: 0.0,
                trend_arrow: None,
                stored_time: None,
            },
            graph: vec![],
            active_sensors: vec![],
        };
        Store::open(&db)
            .unwrap()
            .insert_snapshot(PATIENT_ID, &snapshot, now())
            .unwrap();
        Setup { dir, db }
    }

    fn run(options: &Options) -> Result<(Counts, String), String> {
        let mut out = Vec::new();
        let counts = import(options, now(), &mut out)?;
        Ok((counts, String::from_utf8(out).unwrap()))
    }

    fn database_bytes(db: &Path) -> Vec<u8> {
        ["", "-wal", "-shm"]
            .iter()
            .flat_map(|suffix| {
                let mut path = db.as_os_str().to_owned();
                path.push(suffix);
                std::fs::read(PathBuf::from(path)).unwrap_or_default()
            })
            .collect()
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }

    #[test]
    fn imports_only_glucose_rows_and_never_keeps_other_columns() {
        let setup = collected();
        let rows = [
            row("11-15-2026 08:00 AM", "0", "120", ""),
            row("11-15-2026 08:05 AM", "0", "122", ""),
            row("11-15-2026 08:07 AM", "1", "", "125"),
            row("11-15-2026 08:07 AM", "4", "", ""),
            row("11-15-2026 08:07 AM", "5", "", ""),
            row("11-15-2026 08:08 AM", "6", "", ""),
            // Unsorted, as real exports can be.
            row("11-15-2026 07:55 AM", "0", "118", ""),
            row("not a time", "0", "118", ""),
            row("11-15-2026 08:10 AM", "0", "", ""),
        ];
        let file = setup.write("export.csv", &export(&rows));
        // A real export spans enough days to settle the date order itself.
        let mut options = setup.options(&file);
        options.date_format = None;
        let (counts, output) = run(&options).unwrap();
        assert_eq!(
            counts,
            Counts {
                rows_read: 9,
                imported: 4,
                ignored_record_type: 3,
                unparseable_time: 1,
                invalid_value: 1,
                ..Counts::default()
            }
        );
        assert!(output.contains("imported: 4"), "{output}");

        let stored = database_bytes(&setup.db);
        for forbidden in MARKERS.iter().chain(&[NAME, SERIAL, "FreeStyle Synthetic"]) {
            assert!(!contains(&stored, forbidden), "{forbidden} in the database");
            assert!(!output.contains(forbidden), "{forbidden} in the output");
        }
        // The rows are stored with their origin, basis, and UTC time.
        let store = Store::open(&setup.db).unwrap();
        let rows: Vec<(String, String, String, i64)> = store
            .connection()
            .prepare(
                "SELECT factory_timestamp, origin, time_basis, observed_at_utc FROM readings
                 ORDER BY observed_at_utc, factory_timestamp",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].0, "csv:0:11-15-2026 07:55 AM");
        assert_eq!(rows[0].1, "libreview_csv_historic");
        assert_eq!(rows[0].2, "csv_local_declared_timezone");
        // 07:55 EST is 12:55 UTC.
        assert_eq!(
            rows[0].3,
            DateTime::parse_from_rfc3339("2026-11-15T12:55:00Z")
                .unwrap()
                .timestamp()
        );
        assert_eq!(rows[3].1, "libreview_csv_scan");
    }

    #[test]
    fn dst_ambiguous_and_nonexistent_local_times_are_skipped_and_counted() {
        let setup = collected();
        let rows = [
            row("11-01-2026 12:45 AM", "0", "110", ""),
            // The fall-back hour appears twice in Detroit.
            row("11-01-2026 01:30 AM", "0", "111", ""),
            row("11-01-2026 01:30 AM", "0", "112", ""),
            row("11-01-2026 02:15 AM", "0", "113", ""),
            // Spring forward: 02:30 never happened in Detroit.
            row("03-08-2026 02:30 AM", "0", "114", ""),
        ];
        let file = setup.write("dst.csv", &export(&rows));
        let (counts, _) = run(&setup.options(&file)).unwrap();
        assert_eq!(counts.imported, 2);
        assert_eq!(counts.dst_ambiguous, 2);
        assert_eq!(counts.dst_nonexistent, 1);
    }

    #[test]
    fn same_minute_historic_and_scan_rows_both_import_and_reimport_is_all_duplicates() {
        let setup = collected();
        let rows = [
            row("11-05-2026 08:00 AM", "0", "120", ""),
            row("11-05-2026 08:00 AM", "1", "", "121"),
            row("11-05-2026 08:15 AM", "0", "123", ""),
        ];
        let file = setup.write("pair.csv", &export(&rows));
        let (first, _) = run(&setup.options(&file)).unwrap();
        assert_eq!(first.imported, 3);
        assert_eq!(first.duplicate, 0);
        let (again, output) = run(&setup.options(&file)).unwrap();
        assert_eq!(again.imported, 0);
        assert_eq!(again.duplicate, 3);
        assert!(output.contains("duplicate: 3"), "{output}");
    }

    fn parsed(rows: &[String]) -> ParsedFile {
        read_rows(Cursor::new(export(rows)), MAX_FILE_BYTES).unwrap()
    }

    #[test]
    fn date_order_is_detected_from_the_data() {
        let mdy = parsed(&[
            row("10-13-2026 01:30 PM", "0", "120", ""),
            row("10-02-2026 01:30 PM", "0", "120", ""),
        ]);
        assert_eq!(detect_order(&mdy.rows, None), Ok(DateOrder::Mdy));
        let (readings, _) = convert(&mdy, detroit(), DateOrder::Mdy, 90, now());
        // 13:30 EDT on 13 October is 17:30 UTC.
        assert_eq!(
            readings[0].observed_at_utc,
            DateTime::parse_from_rfc3339("2026-10-13T17:30:00Z").unwrap()
        );

        // Day-first, with a 24-hour clock.
        let dmy = parsed(&[
            row("13-10-2026 13:30", "0", "120", ""),
            row("02-10-2026 07:05", "0", "120", ""),
        ]);
        assert_eq!(detect_order(&dmy.rows, None), Ok(DateOrder::Dmy));
        let (readings, counts) = convert(&dmy, detroit(), DateOrder::Dmy, 90, now());
        assert_eq!(counts.unparseable_time, 0);
        assert_eq!(
            readings[0].observed_at_utc,
            DateTime::parse_from_rfc3339("2026-10-13T17:30:00Z").unwrap()
        );
        assert_eq!(
            readings[1].observed_at_utc,
            DateTime::parse_from_rfc3339("2026-10-02T11:05:00Z").unwrap()
        );

        let conflicting = parsed(&[
            row("10-13-2026 01:30 PM", "0", "120", ""),
            row("13-10-2026 01:30 PM", "0", "120", ""),
        ]);
        assert_eq!(
            detect_order(&conflicting.rows, None),
            Err(FileError::ConflictingDateOrder)
        );
    }

    #[test]
    fn an_ambiguous_date_order_is_refused_without_date_format() {
        let setup = collected();
        let rows = [
            row("10-11-2026 08:00 AM", "0", "120", ""),
            row("11-10-2026 08:00 AM", "0", "121", ""),
        ];
        let file = setup.write("ambiguous.csv", &export(&rows));
        let mut options = setup.options(&file);
        options.date_format = None;
        let error = run(&options).unwrap_err();
        assert!(error.contains("--date-format"), "{error}");
        let stored: i64 = Store::open(&setup.db)
            .unwrap()
            .connection()
            .query_row("SELECT COUNT(*) FROM readings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored, 0, "nothing is written on refusal");

        options.date_format = Some(DateOrder::Dmy);
        let (counts, _) = run(&options).unwrap();
        assert_eq!(counts.imported, 2);
    }

    #[test]
    fn mmol_per_litre_columns_are_converted() {
        let mut header = HEADER;
        header[4] = "Historic Glucose mmol/L";
        header[5] = "Scan Glucose mmol/L";
        let file = export_with(
            &header,
            &[
                row("11-05-2026 08:00 AM", "0", "5.5", ""),
                row("11-05-2026 08:01 AM", "1", "", "10.0"),
            ],
            "\r\n",
        );
        let parsed = read_rows(Cursor::new(file), MAX_FILE_BYTES).unwrap();
        let (readings, _) = convert(&parsed, detroit(), DateOrder::Mdy, 90, now());
        // 5.5 × 18.0182 = 99.1001; 10.0 × 18.0182 = 180.182.
        assert_eq!(readings[0].value_mg_dl, 99.1);
        assert_eq!(readings[1].value_mg_dl, 180.2);

        let mut mixed = HEADER;
        mixed[4] = "Historic Glucose mmol/L";
        let file = export_with(&mixed, &[], "\r\n");
        assert_eq!(
            read_rows(Cursor::new(file), MAX_FILE_BYTES).unwrap_err(),
            FileError::MixedUnits
        );
    }

    #[test]
    fn a_bom_lf_line_endings_and_quoted_fields_are_accepted() {
        let quoted = row("11-05-2026 08:00 AM", "0", "120", "")
            .replace("MARKER-NOTES-7Q", "\"MARKER-NOTES-7Q, with a comma\"");
        let file = format!("\u{feff}{}", export_with(&HEADER, &[quoted], "\n"));
        let parsed = read_rows(Cursor::new(file), MAX_FILE_BYTES).unwrap();
        let (readings, counts) = convert(&parsed, detroit(), DateOrder::Mdy, 90, now());
        assert_eq!(counts.rows_read, 1);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value_mg_dl, 120.0);

        let not_libreview = "a,b,c\r\n1,2,3\r\n";
        assert_eq!(
            read_rows(Cursor::new(not_libreview), MAX_FILE_BYTES).unwrap_err(),
            FileError::MissingColumns
        );
    }

    #[test]
    fn rows_older_than_the_recorded_retention_are_skipped() {
        let setup = collected();
        Store::open(&setup.db)
            .unwrap()
            .set_retention_days(30)
            .unwrap();
        let rows = [
            row("09-27-2026 08:00 AM", "0", "120", ""),
            row("10-27-2026 08:00 AM", "0", "121", ""),
            row("12-06-2026 08:00 AM", "0", "122", ""),
        ];
        let file = setup.write("old.csv", &export(&rows));
        let (counts, output) = run(&setup.options(&file)).unwrap();
        assert_eq!(counts.outside_retention, 1);
        assert_eq!(counts.future_time, 1);
        assert_eq!(counts.imported, 1);
        assert!(output.contains("(30 days)"), "{output}");
        assert!(output.contains("--retention-days"), "{output}");
    }

    #[test]
    fn imports_run_beside_a_collector_but_not_beside_another_import() {
        let setup = collected();
        let file = setup.write(
            "one.csv",
            &export(&[row("11-05-2026 08:00 AM", "0", "120", "")]),
        );
        let collector = store::lock_database(&setup.db).unwrap();
        let (counts, _) = run(&setup.options(&file)).unwrap();
        assert_eq!(counts.imported, 1);
        drop(collector);

        let import_lock = store::lock_import(&setup.db).unwrap();
        let error = run(&setup.options(&file)).unwrap_err();
        assert!(error.contains("Another import"), "{error}");
        drop(import_lock);
    }

    #[test]
    fn an_import_needs_a_collected_patient() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("readings.sqlite3");
        let file = dir.path().join("one.csv");
        std::fs::write(&file, export(&[row("11-05-2026 08:00 AM", "0", "120", "")])).unwrap();
        let options = Options {
            file,
            timezone: detroit(),
            date_format: Some(DateOrder::Mdy),
            db: Some(db.clone()),
            dry_run: false,
        };
        let error = run(&options).unwrap_err();
        assert!(error.contains("run collect at least once first"), "{error}");
        assert!(!db.exists(), "the import does not create a database");

        drop(Store::open(&db).unwrap());
        let error = run(&options).unwrap_err();
        assert!(error.contains("run collect at least once first"), "{error}");
    }

    #[test]
    fn a_dry_run_writes_nothing_and_needs_no_collected_patient() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("readings.sqlite3");
        let file = dir.path().join("pair.csv");
        let rows = [
            row("11-05-2026 08:00 AM", "0", "120", ""),
            row("11-05-2026 08:00 AM", "0", "120", ""),
            row("11-05-2026 08:00 AM", "1", "", "121"),
        ];
        std::fs::write(&file, export(&rows)).unwrap();
        let mut options = Options {
            file,
            timezone: detroit(),
            date_format: Some(DateOrder::Mdy),
            db: Some(db.clone()),
            dry_run: true,
        };

        // No database at all: none is created.
        let (counts, output) = run(&options).unwrap();
        assert_eq!(counts.imported, 2);
        assert_eq!(counts.duplicate, 1);
        assert!(output.contains("Dry run: nothing was written."), "{output}");
        assert!(output.contains("would import: 2"), "{output}");
        assert!(!db.exists());

        // A database with no patient: its bytes are unchanged.
        drop(Store::open(&db).unwrap());
        let files = |dir: &Path| -> Vec<(String, Vec<u8>)> {
            let mut list: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_none_or(|ext| ext != "csv"))
                .map(|path| {
                    (
                        path.file_name().unwrap().to_string_lossy().into_owned(),
                        std::fs::read(&path).unwrap(),
                    )
                })
                .collect();
            list.sort();
            list
        };
        let before = files(dir.path());
        options.dry_run = true;
        run(&options).unwrap();
        let after = files(dir.path());
        let names = |list: &[(String, Vec<u8>)]| -> Vec<String> {
            list.iter().map(|(name, _)| name.clone()).collect()
        };
        assert_eq!(names(&after), names(&before));
        assert!(after == before, "a database file changed");
        assert!(!store::import_lock_path(&db).exists());
    }

    #[test]
    fn a_dry_run_applies_the_recorded_retention_with_or_without_a_running_collector() {
        let dir = tempfile::TempDir::new().unwrap();
        // A path that needs percent-encoding in a SQLite URI.
        let db = dir.path().join("data dir #1").join("readings.sqlite3");
        Store::open(&db).unwrap().set_retention_days(30).unwrap();
        let file = dir.path().join("old.csv");
        let rows = [
            row("09-27-2026 08:00 AM", "0", "120", ""),
            row("10-27-2026 08:00 AM", "0", "121", ""),
        ];
        std::fs::write(&file, export(&rows)).unwrap();
        let options = Options {
            file,
            timezone: detroit(),
            date_format: Some(DateOrder::Mdy),
            db: Some(db.clone()),
            dry_run: true,
        };
        // Collector stopped: no WAL beside the database.
        assert!(!dir.path().join("data dir #1/readings.sqlite3-wal").exists());
        let (counts, _) = run(&options).unwrap();
        assert_eq!((counts.outside_retention, counts.imported), (1, 1));
        assert!(!dir.path().join("data dir #1/readings.sqlite3-wal").exists());

        // Collector running: the database is open, with its WAL.
        let open = Store::open(&db).unwrap();
        open.set_retention_days(60).unwrap();
        let (counts, output) = run(&options).unwrap();
        assert_eq!((counts.outside_retention, counts.imported), (0, 2));
        assert!(!output.contains("retention ("), "{output}");
        drop(open);
    }

    /// Endless rows of an ignored record type.
    struct EndlessRows {
        line: Vec<u8>,
        offset: usize,
        header_left: Vec<u8>,
    }

    impl Read for EndlessRows {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.header_left.is_empty() {
                let count = buf.len().min(self.header_left.len());
                buf[..count].copy_from_slice(&self.header_left[..count]);
                self.header_left.drain(..count);
                return Ok(count);
            }
            let mut written = 0;
            while written < buf.len() {
                buf[written] = self.line[self.offset];
                self.offset = (self.offset + 1) % self.line.len();
                written += 1;
            }
            Ok(written)
        }
    }

    #[test]
    fn the_size_limit_is_enforced_while_reading() {
        // The limit is exact at 50 MiB.
        let mut sink = io::sink();
        let exactly = Limited {
            inner: io::repeat(b'x').take(MAX_FILE_BYTES),
            limit: MAX_FILE_BYTES,
            read: 0,
            exceeded: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(
            io::copy(&mut { exactly }, &mut sink).unwrap(),
            MAX_FILE_BYTES
        );
        let one_over = Limited {
            inner: io::repeat(b'x').take(MAX_FILE_BYTES + 1),
            limit: MAX_FILE_BYTES,
            read: 0,
            exceeded: Arc::new(AtomicBool::new(false)),
        };
        assert!(io::copy(&mut { one_over }, &mut sink).is_err());

        // The parser reports it as too large, whatever the metadata says.
        let stream = EndlessRows {
            line: (row("11-05-2026 08:00 AM", "6", "", "") + "\r\n").into_bytes(),
            offset: 0,
            header_left: export(&[]).into_bytes(),
        };
        assert_eq!(
            read_rows(stream, 1024 * 1024).unwrap_err(),
            FileError::TooLarge
        );
    }

    #[test]
    fn a_file_over_50_mib_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("large.csv");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(file);
        assert_eq!(read_file(&path).unwrap_err(), FileError::TooLarge);
        // Read past the metadata check, the stream itself is refused.
        assert_eq!(
            read_rows(File::open(&path).unwrap(), MAX_FILE_BYTES).unwrap_err(),
            FileError::TooLarge
        );
    }

    #[test]
    fn arguments_require_a_file_and_a_valid_time_zone() {
        let args = |list: &[&str]| -> Vec<String> { list.iter().map(|a| a.to_string()).collect() };
        let options = parse_args(&args(&[
            "export.csv",
            "--timezone",
            "America/Detroit",
            "--date-format",
            "dmy",
            "--db",
            "x.sqlite3",
            "--dry-run",
        ]))
        .unwrap();
        assert_eq!(options.timezone, detroit());
        assert_eq!(options.date_format, Some(DateOrder::Dmy));
        assert!(options.dry_run);
        assert!(parse_args(&args(&["export.csv"])).is_err());
        assert!(parse_args(&args(&["export.csv", "--timezone", "Mars/Olympus"])).is_err());
        assert!(parse_args(&args(&["--timezone", "UTC"])).is_err());
        assert!(parse_args(&args(&["a.csv", "b.csv", "--timezone", "UTC"])).is_err());
        assert!(
            parse_args(&args(&[
                "a.csv",
                "--timezone",
                "UTC",
                "--date-format",
                "ymd"
            ]))
            .is_err()
        );
    }
}
