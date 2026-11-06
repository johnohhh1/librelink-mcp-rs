//! The data the tools need from LibreLinkUp, independent of any HTTP client.

use crate::fetch::FetchError;
use crate::time::StoredTime;
use chrono::{DateTime, Utc};
use std::future::Future;
use std::pin::Pin;

/// One glucose sample as the source reported it. Timestamps stay as source
/// text; interpretation happens in `time`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GlucoseSample {
    /// The sharer's local wall clock, as sent.
    pub timestamp: String,
    /// The factory clock, as sent; inferred as UTC only after checks.
    pub factory_timestamp: String,
    pub value_in_mg_per_dl: f64,
    /// Source trend arrow code, if the source sent one.
    pub trend_arrow: Option<i32>,
    /// Set only for readings read back from the local store: the UTC time
    /// and basis established when the reading was stored.
    pub stored_time: Option<StoredTime>,
}

/// An active sensor's source fields, named for what they hold.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SensorRecord {
    pub device_id: String,
    pub serial_number: String,
    /// Epoch seconds in observed payloads (source field `a`).
    pub activation_epoch_seconds: i32,
    /// Source field `w`.
    pub warmup_minutes: i32,
    /// Source field `pt`.
    pub product_type_code: i32,
}

/// One read from a data source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    /// The selected patient's source ID. Used only to key the local store;
    /// no tool output includes it.
    pub patient_id: String,
    /// The single current reading every tool uses.
    pub current: GlucoseSample,
    pub graph: Vec<GlucoseSample>,
    pub active_sensors: Vec<SensorRecord>,
}

pub(crate) type ReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Snapshot, FetchError>> + Send + 'a>>;

/// Something that can read the selected patient's current data. Errors carry
/// only safe public codes and messages, never upstream bodies.
pub(crate) trait DataSource: Send + Sync {
    fn read(&self) -> ReadFuture<'_>;
}

/// Sensor serial numbers and device IDs identify a person's hardware. Only the
/// last four characters are kept, which is enough to tell sensors apart.
/// Lowercase hex of `bytes`, as used for hashed identifiers.
pub(crate) fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn masked_identifier(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 4 {
        return Some("****".to_owned());
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    Some(format!("****{tail}"))
}

pub(crate) fn sensor_activation_time(
    epoch_seconds: i32,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    // Sensor a is an epoch-seconds field in observed LibreLinkUp payloads.
    // Reject a days-since-start value or a clock that has not happened yet.
    const FIRST_PLAUSIBLE_ACTIVATION: i64 = 1_388_534_400; // 2014-01-01 UTC
    let seconds = i64::from(epoch_seconds);
    (seconds >= FIRST_PLAUSIBLE_ACTIVATION)
        .then(|| DateTime::<Utc>::from_timestamp(seconds, 0))
        .flatten()
        .filter(|time| *time <= now)
}

/// A sensor exactly as tools show it: masked identifiers and only the
/// source fields that passed validation. The local store saves this form,
/// never the raw identifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SensorSummary {
    pub device_id_masked: Option<String>,
    pub serial_number_masked: Option<String>,
    pub activation_epoch_seconds: Option<i32>,
    pub warmup_minutes: Option<i32>,
    pub product_type_code: Option<i32>,
}

impl SensorSummary {
    pub(crate) fn from_record(record: &SensorRecord, now: DateTime<Utc>) -> Self {
        Self {
            device_id_masked: masked_identifier(&record.device_id),
            serial_number_masked: masked_identifier(&record.serial_number),
            activation_epoch_seconds: sensor_activation_time(record.activation_epoch_seconds, now)
                .map(|_| record.activation_epoch_seconds),
            warmup_minutes: (record.warmup_minutes >= 0).then_some(record.warmup_minutes),
            product_type_code: (record.product_type_code >= 0).then_some(record.product_type_code),
        }
    }
}
