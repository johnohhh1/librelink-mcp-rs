# Importing a LibreView CSV export

`import-csv` loads a LibreView glucose export, saved on your machine, into the collector's local database. It works offline: it uploads nothing and never contacts LibreLinkUp. Imported readings then appear in `serve`'s history, statistics, and trends from the store. The design is recorded in [ADR 0003](adr/0003-time-weighted-metrics-and-csv-import.md).

## Usage

Run `collect` at least once first, so the database knows which patient the readings belong to (see the [collector guide](collector.md)). Then:

```sh
librelink-mcp-rs import-csv export.csv --timezone America/Detroit
```

Options:

| Option | Meaning |
| --- | --- |
| `--timezone <IANA zone>` | Required. The time zone of the device that recorded the readings, for example `America/Detroit` or `Europe/Berlin`. The export's timestamps carry no offset. |
| `--date-format mdy\|dmy` | Month-first or day-first dates. Needed only when the file cannot settle it: if every date has both parts 12 or lower. |
| `--db <path>` | Import into this database instead of the default. |
| `--dry-run` | Read the file and print the counts without writing anything. Needs no collected patient and creates no files. |

The output is counts only:
- rows read;
- rows imported, or that would be imported on a dry run;
- duplicates;
- rows skipped, by reason.

It never shows values, names, or serial numbers.

Exit statuses: 0 on success, 2 for invalid options, and 1 when the import is refused or fails. Nothing is imported when it is refused.

## What it reads

- **Columns:** they are found by their English header names: `Device Timestamp`, `Record Type`, `Historic Glucose mg/dL` and `Scan Glucose mg/dL` (or the `mmol/L` versions). Other languages and semicolon-delimited files are refused with a message naming the expected columns. Values in mmol/L are converted with 18.0182 and rounded to 0.1 mg/dL.
- **Record types:** only type 0 (historic readings) and type 1 (scans) are imported. Their origins are `libreview_csv_historic` and `libreview_csv_scan`. Other record types (events such as insulin, food, or notes) are counted as `ignored_record_type`.
- **Never stored or printed:** the first line, which names the account holder, the serial number, and the insulin, food, carbohydrate, notes, strip, and ketone columns. They are not kept beyond parsing the line that holds them.
- **Not changed:** the source file is never copied or modified.
- **Format:** the file may have quoted fields, a UTF-8 byte-order mark, and CRLF or LF line endings. Rows need not be sorted.
- **Size:** files larger than 50 MiB are refused. The limit is checked before reading and again while reading.

## Timestamps

- **Formats:** timestamps such as `11-05-2026 08:30 AM` (12-hour) and `05-11-2026 20:30` (24-hour) are read in the declared time zone. They are stored with `time_basis: csv_local_declared_timezone`.
- **Date order:** it is detected from the data. A first part above 12 means day-first, and a second part above 12 means month-first. A file with both kinds is refused. A file where every date fits either order needs `--date-format`.
- **DST changes:** local times that occur twice when the clocks go back are skipped and counted as `dst_ambiguous`, and so are times skipped when they go forward (`dst_nonexistent`). They are never guessed.
- **Device clock:** one declared zone cannot correct a device clock that was set wrong, or readings taken while travelling in another zone. The import cannot detect either.

## Duplicates and retention

- **Duplicates:** each reading's key is its record type plus the original timestamp text, so a historic reading and a scan in the same minute both import. Importing the same file again reports every row as a duplicate.
- **Retention:** applies to imported rows too. The collector records its `--retention-days` in the database, and the import uses it, or 90 days if none is recorded. Older rows are skipped as `outside_retention`. To keep longer history, run `collect` with a larger `--retention-days` before importing.
- **Future rows:** rows timestamped more than 5 minutes in the future are skipped as `future_time`.

The other skip reasons are `unparseable_time` and `invalid_value`, for a missing, non-numeric, or non-positive glucose value.

## Overlap with collected readings

Collected readings take precedence. When a collected reading lies within 90 seconds of an imported row, `serve` leaves that CSV row out of every reply, including the current reading, history, statistics, trends, coverage, and counts. The rule applies when the data is read, so it works whichever was stored first, and importing never deletes or changes collected rows.

Inside the span of imported rows, a gap with no collector polls in it is labeled `no_data_in_export`: the device's own history has nothing there, for example during a sensor change. See the [collector guide](collector.md#how-serve-chooses-its-data) for all gap causes.

## Running beside a collector

An import can run while a collector is running on the same database; SQLite's WAL mode handles both writers. It writes in batches of about 1,000 rows, so it never blocks the collector for long.

Before 0.4.0's first import or collector start, stop any older collector: the import upgrades the database, and a 0.3.0 collector refuses it afterward. See [Upgrading](collector.md#upgrading).

An import holds its own lock, `<db>.import.lock`, so a second import on the same database is refused. `clear-local-data` refuses while either the collector or an import holds its lock.
