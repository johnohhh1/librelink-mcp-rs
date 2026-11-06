//! The data the tools need from LibreLinkUp, independent of any HTTP client.

use crate::fetch::FetchError;
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
