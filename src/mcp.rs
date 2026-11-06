use crate::{
    fetch::{FetchError, Fetcher},
    load_settings,
    metrics::{self, Gmi},
    save_settings,
    source::{GlucoseSample, SensorSummary, sensor_activation_time},
    store::{CollectorStatus, Origin, Store, StoreResult, StoredReading},
    time::{
        STALE_AFTER_MINUTES, TimeAssessment, TimeBasis, assess_sample, assess_sample_at,
        assess_time_at,
    },
};
use chrono::{DateTime, Duration, Utc};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ErrorData, Implementation, ListResourcesResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
        ResourceContents, ServerCapabilities, ServerConfig,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The MCP server. Each data tool call reads from the local store while a
/// collector is running, and through `fetcher` (any `DataSource`) otherwise.
#[derive(Clone)]
pub(crate) struct LibreMcp {
    fetcher: Arc<Fetcher>,
    /// The collector's database, if any.
    db: Option<PathBuf>,
    /// Tests use fixed target bounds instead of reading the OS keyring.
    #[cfg(test)]
    fixed_ranges: Option<(u16, u16)>,
}

const LIVE_API: &str = "live_api";
const LOCAL_STORE: &str = "local_store";
/// A collector heartbeat younger than this means a collector is running.
const COLLECTOR_FRESH_SECONDS: i64 = 180;
/// Tolerated heartbeat clock skew into the future.
const COLLECTOR_FUTURE_TOLERANCE_SECONDS: i64 = 60;

fn collector_running(heartbeat_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let age = now.signed_duration_since(heartbeat_at).num_seconds();
    age < COLLECTOR_FRESH_SECONDS && age > -COLLECTOR_FUTURE_TOLERANCE_SECONDS
}

/// Where one tool call reads from.
enum DataPath {
    Live,
    Local(LocalStore),
}

/// An open store with a running collector. Queries run on the blocking pool
/// and never contact LibreLinkUp.
struct LocalStore {
    store: Arc<Mutex<Store>>,
    status: CollectorStatus,
}

impl LocalStore {
    async fn query<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Store, Option<&str>) -> StoreResult<T> + Send + 'static,
    ) -> Result<T, CallToolResult> {
        let store = Arc::clone(&self.store);
        let key = self.status.patient_key.clone();
        let result = tokio::task::spawn_blocking(move || {
            let store = store
                .lock()
                .map_err(|_| "The local database is unavailable.")?;
            operation(&store, key.as_deref())
        })
        .await
        .unwrap_or(Err("The local database task failed."));
        result.map_err(|message| failure_code("local_store_unavailable", message))
    }

    /// Readings in the window, plus the poll record needed to explain gaps.
    async fn window(
        &self,
        hours: i64,
        now: DateTime<Utc>,
    ) -> Result<(Vec<StoredReading>, PollWindow), CallToolResult> {
        let from = now - Duration::hours(hours);
        self.query(move |store, key| {
            let polls = PollWindow {
                times: store.poll_times(from, now)?,
                first_ever: store.first_poll_time()?,
                csv_span: key.map(|key| store.csv_span(key)).transpose()?.flatten(),
            };
            match key {
                Some(key) => Ok((store.readings_for_key(key, from, now)?, polls)),
                None => Ok((vec![], polls)),
            }
        })
        .await
    }
}

impl LibreMcp {
    pub(crate) fn new(fetcher: Fetcher) -> Self {
        Self {
            fetcher: Arc::new(fetcher),
            db: None,
            #[cfg(test)]
            fixed_ranges: None,
        }
    }

    pub(crate) fn with_store(mut self, db: Option<PathBuf>) -> Self {
        self.db = db;
        self
    }

    /// Chooses the data source for one call: the store while its collector
    /// heartbeat is fresh, otherwise the live API. Any store problem falls
    /// back to the live API.
    async fn data_path(&self) -> DataPath {
        let Some(db) = self.db.clone() else {
            return DataPath::Live;
        };
        let opened = tokio::task::spawn_blocking(move || {
            if !db.exists() {
                return None;
            }
            let store = Store::open_read_only(&db).ok()?;
            let status = store.collector_status().ok()??;
            Some((store, status))
        })
        .await
        .ok()
        .flatten();
        match opened {
            Some((store, status)) if collector_running(status.heartbeat.at, Utc::now()) => {
                DataPath::Local(LocalStore {
                    store: Arc::new(Mutex::new(store)),
                    status,
                })
            }
            _ => DataPath::Live,
        }
    }

    /// Local target bounds in mg/dL.
    fn target_range(&self) -> Result<(u16, u16), &'static str> {
        #[cfg(test)]
        if let Some(ranges) = self.fixed_ranges {
            return Ok(ranges);
        }
        load_settings().map(|settings| (settings.target_low, settings.target_high))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    /// Requested hours, from 1 through 2160 (90 days). The live API returns
    /// about 12 hours; longer windows need the local collector.
    hours: Option<u16>,
    /// Most readings to return, from 1 through 5000. Default 500.
    limit: Option<u32>,
    /// RFC 3339 time. Return only readings at or before it: pass the previous
    /// reply's `next_before` to get the next, older page.
    before: Option<String>,
}

const MAX_HISTORY_HOURS: u32 = 2160;
const DEFAULT_HISTORY_LIMIT: u32 = 500;
const MAX_HISTORY_LIMIT: u32 = 5000;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatsArgs {
    /// Requested days, from 1 through 90. The API may return a shorter graph.
    days: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Period {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TrendsArgs {
    period: Option<Period>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RangesArgs {
    target_low: u16,
    target_high: u16,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ValueQuality {
    Observed,
    Missing,
}

/// The `get_current_glucose` reply, also served as `glucose://current`.
#[derive(Serialize, JsonSchema)]
struct Reading {
    glucose_mg_dl: Option<f64>,
    value_quality: ValueQuality,
    #[serde(flatten)]
    time: TimeAssessment,
    trend: Option<&'static str>,
    cache_hit: bool,
    data_source: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct GraphGap {
    from_utc: DateTime<Utc>,
    to_utc: DateTime<Utc>,
    duration_minutes: f64,
    /// Local store only: why the store has no readings there.
    #[serde(skip_serializing_if = "Option::is_none")]
    cause: Option<&'static str>,
}

#[derive(Serialize, JsonSchema)]
struct GraphCoverage {
    source: &'static str,
    requested_hours: i64,
    first_observed_at_utc: Option<DateTime<Utc>>,
    last_observed_at_utc: Option<DateTime<Utc>>,
    observed_span_hours: Option<f64>,
    actual_points_in_window: Option<usize>,
    estimated_cadence_minutes: Option<f64>,
    expected_points_estimate: Option<i64>,
    start_gap_minutes: Option<f64>,
    end_gap_minutes: Option<f64>,
    interior_gap_count: usize,
    interior_gaps: Vec<GraphGap>,
    period_span_sufficient: bool,
    reason: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct RateSummary {
    pair_count: usize,
    minimum_mg_dl_per_minute: Option<f64>,
    maximum_mg_dl_per_minute: Option<f64>,
    average_mg_dl_per_minute: Option<f64>,
    maximum_pair_gap_minutes: u8,
}

fn observed_rates(items: &[&GlucoseSample], now: DateTime<Utc>) -> RateSummary {
    let mut points: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let time = assess_sample(item, now).observed_at_utc?;
            (item.value_in_mg_per_dl.is_finite() && item.value_in_mg_per_dl > 0.0 && time <= now)
                .then_some((time, item.value_in_mg_per_dl))
        })
        .collect();
    points.sort_unstable_by_key(|(time, _)| *time);
    points.dedup_by_key(|(time, _)| *time);
    let rates: Vec<f64> = points
        .windows(2)
        .filter_map(|pair| {
            let minutes = pair[1].0.signed_duration_since(pair[0].0).num_seconds() as f64 / 60.0;
            (1.0..=30.0)
                .contains(&minutes)
                .then_some((pair[1].1 - pair[0].1) / minutes)
        })
        .collect();
    RateSummary {
        pair_count: rates.len(),
        minimum_mg_dl_per_minute: rates.iter().copied().reduce(f64::min),
        maximum_mg_dl_per_minute: rates.iter().copied().reduce(f64::max),
        average_mg_dl_per_minute: (!rates.is_empty())
            .then(|| rates.iter().sum::<f64>() / rates.len() as f64),
        maximum_pair_gap_minutes: 30,
    }
}

/// How coverage is computed for one data source.
#[derive(Clone, Copy)]
struct CoverageOptions {
    source: &'static str,
    /// Use this cadence instead of estimating one from all intervals.
    cadence_seconds: Option<i64>,
}

const LIVE_COVERAGE: CoverageOptions = CoverageOptions {
    source: "librelinkup_graph",
    cadence_seconds: None,
};

fn graph_coverage(items: &[GlucoseSample], hours: i64, now: DateTime<Utc>) -> GraphCoverage {
    coverage_with(items, hours, now, LIVE_COVERAGE)
}

/// The median interval between graph-origin readings. Stored data mixes
/// 15-minute graph points with minute-by-minute current readings; the graph
/// cadence is the one that defines a gap.
fn graph_cadence_seconds(readings: &[StoredReading], now: DateTime<Utc>) -> Option<i64> {
    let mut times: Vec<_> = readings
        .iter()
        .filter(|reading| reading.origin == Origin::Graph)
        .filter_map(|reading| assess_sample(&reading.sample, now).observed_at_utc)
        .collect();
    times.sort_unstable();
    let mut intervals: Vec<i64> = times
        .windows(2)
        .map(|pair| pair[1].signed_duration_since(pair[0]).num_seconds())
        .filter(|seconds| *seconds >= 60 && *seconds <= 3600)
        .collect();
    intervals.sort_unstable();
    (intervals.len() >= 2).then(|| intervals[intervals.len() / 2])
}

fn local_coverage(readings: &[StoredReading], now: DateTime<Utc>) -> CoverageOptions {
    CoverageOptions {
        source: LOCAL_STORE,
        cadence_seconds: graph_cadence_seconds(readings, now),
    }
}

/// Poll attempts inside a window, the oldest attempt still recorded, and
/// the span of imported CSV rows.
struct PollWindow {
    times: Vec<DateTime<Utc>>,
    first_ever: Option<DateTime<Utc>>,
    /// First and last observed time of the patient's CSV rows.
    csv_span: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

/// Labels each listed interior gap, taking the first that applies:
/// 1. `no_data_from_api`: the collector polled during the gap but got no
///    readings for it.
/// 2. `no_data_in_export`: the gap lies entirely within the imported CSV
///    span, so the device's own history has nothing there (for example, a
///    sensor change).
/// 3. `before_collection`: the gap ends before the first recorded poll. The
///    store holds only the API's backfilled graph for that period, so the
///    cause is unknown.
/// 4. `collector_not_running`: the gap is after collection began and no poll
///    attempt was recorded inside it.
fn classify_gaps(coverage: &mut GraphCoverage, polls: &PollWindow) {
    for gap in &mut coverage.interior_gaps {
        let polled = polls
            .times
            .iter()
            .any(|time| *time > gap.from_utc && *time < gap.to_utc);
        let in_export = polls
            .csv_span
            .is_some_and(|(first, last)| gap.from_utc >= first && gap.to_utc <= last);
        let before_collection = polls.first_ever.is_none_or(|first| gap.to_utc < first);
        gap.cause = Some(if polled {
            "no_data_from_api"
        } else if in_export {
            "no_data_in_export"
        } else if before_collection {
            "before_collection"
        } else {
            "collector_not_running"
        });
    }
}

/// The observed span may start or end this far inside the window and still
/// count as covering it.
const SPAN_EDGE_TOLERANCE_MINUTES: i64 = 15;

fn coverage_with(
    items: &[GlucoseSample],
    hours: i64,
    now: DateTime<Utc>,
    options: CoverageOptions,
) -> GraphCoverage {
    let cutoff = now - Duration::hours(hours);
    let assessed: Vec<_> = items.iter().map(|item| assess_sample(item, now)).collect();
    let usable = !assessed.is_empty()
        && assessed
            .iter()
            .all(|time| time.observed_at_utc.is_some_and(|observed| observed <= now));
    if !usable {
        return GraphCoverage {
            source: options.source,
            requested_hours: hours,
            first_observed_at_utc: None,
            last_observed_at_utc: None,
            observed_span_hours: None,
            actual_points_in_window: None,
            estimated_cadence_minutes: None,
            expected_points_estimate: None,
            start_gap_minutes: None,
            end_gap_minutes: None,
            interior_gap_count: 0,
            interior_gaps: vec![],
            period_span_sufficient: false,
            reason: "One or more graph timestamps lack a usable UTC time basis.",
        };
    }
    let mut times: Vec<_> = assessed
        .into_iter()
        .filter_map(|time| time.observed_at_utc)
        .collect();
    times.sort_unstable();
    times.dedup();
    let first = times[0];
    let last = *times.last().unwrap();
    let actual_points_in_window = times
        .iter()
        .filter(|time| **time >= cutoff && **time <= now)
        .count();
    let mut intervals: Vec<i64> = times
        .windows(2)
        .map(|pair| pair[1].signed_duration_since(pair[0]).num_seconds())
        .filter(|seconds| *seconds >= 60 && *seconds <= 3600)
        .collect();
    intervals.sort_unstable();
    let cadence_seconds = options
        .cadence_seconds
        .or_else(|| (intervals.len() >= 2).then(|| intervals[intervals.len() / 2]));
    let mut interior_gaps = Vec::new();
    let mut interior_gap_count = 0;
    if let Some(cadence) = cadence_seconds {
        for pair in times.windows(2) {
            let seconds = pair[1].signed_duration_since(pair[0]).num_seconds();
            if pair[1] >= cutoff && seconds >= cadence * 2 {
                interior_gap_count += 1;
                if interior_gaps.len() < 20 {
                    interior_gaps.push(GraphGap {
                        from_utc: pair[0],
                        to_utc: pair[1],
                        duration_minutes: seconds as f64 / 60.0,
                        cause: None,
                    });
                }
            }
        }
    }
    let start_gap_seconds = first.signed_duration_since(cutoff).num_seconds().max(0);
    let end_gap_seconds = now.signed_duration_since(last).num_seconds().max(0);
    // `partial` means the observed span does not reach both ends of the
    // window. Interior gaps are reported in `interior_gaps` and in the
    // time-weighted coverage instead.
    let edge = Duration::minutes(SPAN_EDGE_TOLERANCE_MINUTES);
    let period_span_sufficient = first <= cutoff + edge && last >= now - edge;
    GraphCoverage {
        source: options.source,
        requested_hours: hours,
        first_observed_at_utc: Some(first),
        last_observed_at_utc: Some(last),
        observed_span_hours: Some(last.signed_duration_since(first).num_seconds() as f64 / 3600.0),
        actual_points_in_window: Some(actual_points_in_window),
        estimated_cadence_minutes: cadence_seconds.map(|seconds| seconds as f64 / 60.0),
        expected_points_estimate: cadence_seconds.map(|seconds| hours * 3600 / seconds + 1),
        start_gap_minutes: Some(start_gap_seconds as f64 / 60.0),
        end_gap_minutes: Some(end_gap_seconds as f64 / 60.0),
        interior_gap_count,
        interior_gaps,
        period_span_sufficient,
        reason: "Cadence and expected points are estimates from returned graph intervals; API completeness remains unverified.",
    }
}

const SERVER_INSTRUCTIONS: &str = "Unofficial, read-only access to LibreLinkUp follower data. \
Glucose values are mg/dL. Only get_current_glucose reports freshness: treat `stale` or `unknown` \
as not real-time and say so. `time_basis: factory_utc_inferred` means the UTC time is inferred \
from the API's factory clock. The LibreLinkUp graph covers about 12 hours, so stats and trends \
for a day or more are labeled `partial`; report the observed span from `coverage` rather than \
the requested period. Results are descriptive only: do not diagnose, recommend treatment, or \
infer meals. Credentials are never accepted through MCP; direct users to the local `setup` command. \
Every data reply has `data_source`: `live_api` (read from LibreLinkUp now) or `local_store` (read \
from the local collector's database). `local_store` data is only as fresh as the collector's last \
successful poll; check freshness and coverage before calling a reading current. \
get_glucose_stats reports sample statistics and time-weighted figures separately; \
`time_in_range_percent` is time-weighted (70-180 mg/dL). `gmi_percent` is an estimate, and null \
with `gmi_unavailable_reason` unless the window is at least 14 days with at least 70% coverage. \
get_glucose_history returns at most `limit` readings (default 500), oldest first; when `truncated` \
is true, call it again with `before` set to `next_before` for older readings. Coverage always \
describes the whole window. The resource glucose://current holds the same reply as \
get_current_glucose.";

/// The tool form of one sensor, from either data source.
#[derive(Serialize, JsonSchema)]
struct SensorReply {
    device_id_masked: Option<String>,
    serial_number_masked: Option<String>,
    source_activation_epoch_seconds: Option<i32>,
    activation_time: Option<DateTime<Utc>>,
    activation_time_basis: Option<&'static str>,
    warmup_minutes: Option<i32>,
    product_type_code: Option<i32>,
    /// Not provided by the source.
    device_type: (),
    /// Not provided by the source.
    remaining_life: (),
}

fn sensor_json(sensor: &SensorSummary, now: DateTime<Utc>) -> SensorReply {
    let epoch = sensor
        .activation_epoch_seconds
        .filter(|epoch| sensor_activation_time(*epoch, now).is_some());
    let activation_time = epoch.and_then(|epoch| sensor_activation_time(epoch, now));
    SensorReply {
        device_id_masked: sensor.device_id_masked.clone(),
        serial_number_masked: sensor.serial_number_masked.clone(),
        source_activation_epoch_seconds: epoch,
        activation_time,
        activation_time_basis: activation_time.map(|_| "source_epoch_seconds_interpreted"),
        warmup_minutes: sensor.warmup_minutes,
        product_type_code: sensor.product_type_code,
        device_type: (),
        remaining_life: (),
    }
}

#[derive(Serialize, JsonSchema)]
struct SensorsReply {
    sensor_count: usize,
    active_sensors: Vec<SensorReply>,
    cache_hit: bool,
    data_source: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct HistoryReply {
    requested_hours: u16,
    window_verified: bool,
    window_filter_applied: bool,
    cache_hit: bool,
    retrieved_at_utc: Option<DateTime<Utc>>,
    current_measurement_appended: bool,
    /// Computed over the whole window, whatever page this is.
    coverage: GraphCoverage,
    reason: &'static str,
    /// Readings in this reply.
    total_readings: usize,
    /// Readings in the whole window.
    readings_in_window: usize,
    limit: u32,
    before: Option<DateTime<Utc>>,
    /// Older readings in the window were left out of this page.
    truncated: bool,
    /// Pass as `before` for the next, older page. Null on the last page.
    next_before: Option<DateTime<Utc>>,
    /// Oldest first.
    readings: Vec<HistoryPoint>,
    data_source: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct SamplePercentages {
    below_range: f64,
    in_range: f64,
    above_range: f64,
}

#[derive(Serialize, JsonSchema)]
struct TargetRange {
    low: u16,
    high: u16,
}

#[derive(Serialize, JsonSchema)]
struct TimeWeightedReply {
    status: &'static str,
    window_start_utc: DateTime<Utc>,
    window_end_utc: DateTime<Utc>,
    max_minutes_per_reading: i64,
    covered_minutes: f64,
    coverage_percent: f64,
    readings_used: usize,
    readings_without_utc: usize,
    summary: Option<metrics::Summary>,
    consensus_reference: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct StatsReply {
    requested_days: u8,
    period_status: &'static str,
    window_verified: bool,
    window_filter_applied: bool,
    coverage: GraphCoverage,
    sample_count: usize,
    average_glucose_mg_dl: f64,
    standard_deviation_mg_dl: f64,
    coefficient_of_variation_percent: f64,
    sample_percentages: SamplePercentages,
    target_range_mg_dl: TargetRange,
    reason: &'static str,
    time_weighted: TimeWeightedReply,
    time_in_range_percent: Option<f64>,
    gmi_percent: Option<f64>,
    gmi_unavailable_reason: Option<metrics::GmiUnavailable>,
    gmi_method: &'static str,
    cache_hit: bool,
    data_source: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct TrendCounts {
    falling: usize,
    stable: usize,
    rising: usize,
    unknown: usize,
}

#[derive(Serialize, JsonSchema)]
struct TrendsReply {
    requested_period: &'static str,
    window_verified: bool,
    window_filter_applied: bool,
    coverage: GraphCoverage,
    period_status: &'static str,
    sample_count: usize,
    observed_min_mg_dl: Option<f64>,
    observed_max_mg_dl: Option<f64>,
    trend_arrow_counts: TrendCounts,
    adjacent_point_rates: RateSummary,
    /// Not inferred: the data has no meal or sleep context.
    dawn_phenomenon: (),
    /// Not inferred: the data has no meal or sleep context.
    meal_response_average: (),
    /// Not inferred: the data has no meal or sleep context.
    overnight_stability: (),
    reason: &'static str,
    cache_hit: bool,
    data_source: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct CredentialsReply {
    credential_input_supported: bool,
    setup_command: &'static str,
    message: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct RangesReply {
    saved: bool,
    target_low: u16,
    target_high: u16,
    unit: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct LiveStatus {
    connected: bool,
    graph_points_returned: usize,
    active_sensors_returned: usize,
    reading_time: TimeAssessment,
    cache_hit: bool,
    stale_threshold_minutes: i64,
    data_source: &'static str,
}

#[derive(Serialize, JsonSchema)]
struct LocalStatus {
    collector_running: bool,
    collector_heartbeat_age_seconds: i64,
    last_poll_outcome: Option<String>,
    last_poll_at_utc: Option<DateTime<Utc>>,
    stored_reading_count: i64,
    reading_time: TimeAssessment,
    upstream_contacted: bool,
    cache_hit: bool,
    stale_threshold_minutes: i64,
    data_source: &'static str,
}

/// `validate_connection` reports differently for each data source.
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum ValidateReply {
    Live(LiveStatus),
    Local(LocalStatus),
}

/// The body of every error reply (`isError: true`).
#[derive(Serialize, JsonSchema)]
struct ToolError {
    error_code: &'static str,
    message: Cow<'static, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
}

/// What a tool's `structuredContent` can be: its reply or an error.
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)] // Used only for its schema.
enum ToolOutput<T> {
    Reply(T),
    Error(ToolError),
}

/// The published output schema for a tool replying with `T`. MCP requires
/// an object root; both alternatives are objects.
fn output_schema<T: JsonSchema + 'static>() -> Arc<rmcp::model::JsonObject> {
    let mut schema = (*rmcp::handler::server::tool::schema_for_output::<ToolOutput<T>>()).clone();
    schema.insert("type".to_owned(), "object".into());
    Arc::new(schema)
}

fn value(value: f64) -> (Option<f64>, ValueQuality) {
    if value.is_finite() && value > 0.0 {
        (Some(value), ValueQuality::Observed)
    } else {
        (None, ValueQuality::Missing)
    }
}

fn trend(arrow: Option<i32>) -> Option<&'static str> {
    match arrow {
        Some(1) => Some("falling_fast"),
        Some(2) => Some("falling"),
        Some(3) => Some("stable"),
        Some(4) => Some("rising"),
        Some(5) => Some("rising_fast"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PointSource {
    Graph,
    CurrentMeasurement,
    LibreviewCsvHistoric,
    LibreviewCsvScan,
}

/// One historical sample. Freshness is deliberately absent: it describes how
/// current the latest reading is, and every past sample would read as stale.
#[derive(Serialize, JsonSchema)]
struct HistoryPoint {
    glucose_mg_dl: Option<f64>,
    value_quality: ValueQuality,
    observed_at_utc: Option<DateTime<Utc>>,
    time_basis: TimeBasis,
    source_timestamp: Option<String>,
    trend: Option<&'static str>,
    source: PointSource,
}

fn history_point(item: &GlucoseSample, source: PointSource, now: DateTime<Utc>) -> HistoryPoint {
    let (glucose_mg_dl, value_quality) = value(item.value_in_mg_per_dl);
    let time = assess_sample(item, now);
    HistoryPoint {
        glucose_mg_dl,
        value_quality,
        observed_at_utc: time.observed_at_utc,
        time_basis: time.time_basis,
        source_timestamp: time.source_timestamp,
        trend: trend(item.trend_arrow),
        source,
    }
}

/// The graph lags the live reading by several minutes, so append the current
/// measurement when the graph does not already contain it. Returns the merged
/// points and whether the current measurement was appended.
fn points_with_current(
    graph: &[GlucoseSample],
    current: &GlucoseSample,
) -> (Vec<GlucoseSample>, bool) {
    let usable = current.value_in_mg_per_dl.is_finite()
        && current.value_in_mg_per_dl > 0.0
        && !current.factory_timestamp.trim().is_empty();
    let already_present = graph
        .iter()
        .any(|item| item.factory_timestamp == current.factory_timestamp);
    let mut points = graph.to_vec();
    let appended = usable && !already_present;
    if appended {
        points.push(current.clone());
    }
    (points, appended)
}

fn select_graph_window(
    items: &[GlucoseSample],
    hours: i64,
    now: DateTime<Utc>,
) -> (Vec<&GlucoseSample>, bool) {
    let assessed: Vec<_> = items
        .iter()
        .map(|item| (item, assess_sample(item, now)))
        .collect();
    let can_filter = !assessed.is_empty()
        && assessed
            .iter()
            .all(|(_, time)| time.observed_at_utc.is_some());
    if !can_filter {
        return (items.iter().collect(), false);
    }
    let cutoff = now - Duration::hours(hours);
    (
        assessed
            .into_iter()
            .filter(|(_, time)| {
                time.observed_at_utc
                    .is_some_and(|timestamp| timestamp >= cutoff && timestamp <= now)
            })
            .map(|(item, _)| item)
            .collect(),
        true,
    )
}

fn success<T: Serialize>(body: T) -> CallToolResult {
    match serde_json::to_value(body) {
        Ok(value) => CallToolResult::structured(value),
        Err(_) => failure("Unable to encode tool result."),
    }
}

fn failure(message: &'static str) -> CallToolResult {
    failure_code("request_failed", message)
}

fn error_result(error: ToolError) -> CallToolResult {
    match serde_json::to_value(error) {
        Ok(value) => CallToolResult::structured_error(value),
        Err(_) => CallToolResult::structured_error(serde_json::json!({
            "error_code": "request_failed",
            "message": "Unable to encode tool error.",
        })),
    }
}

fn failure_code(code: &'static str, message: &'static str) -> CallToolResult {
    error_result(ToolError {
        error_code: code,
        message: Cow::Borrowed(message),
        retry_after_seconds: None,
    })
}

fn failure_fetch(error: FetchError) -> CallToolResult {
    error_result(ToolError {
        error_code: error.code,
        message: error.message,
        retry_after_seconds: error.retry_after_seconds,
    })
}

/// Descriptive statistics over the returned graph. A graph shorter than the
/// requested window still yields statistics, labeled `partial`, because the
/// LibreLinkUp graph covers roughly 12 hours and no request of a day or more
/// can ever be fully covered by it.
fn live_stats(
    graph: &[GlucoseSample],
    days: u8,
    target_low: u16,
    target_high: u16,
    now: DateTime<Utc>,
) -> Result<StatsReply, CallToolResult> {
    let coverage = graph_coverage(graph, i64::from(days) * 24, now);
    summarize_stats_with(graph, days, (target_low, target_high), now, coverage)
}

/// Tests read replies as JSON.
#[cfg(test)]
fn summarize_stats(
    graph: &[GlucoseSample],
    days: u8,
    target_low: u16,
    target_high: u16,
    now: DateTime<Utc>,
) -> Result<serde_json::Value, CallToolResult> {
    live_stats(graph, days, target_low, target_high, now)
        .map(|reply| serde_json::to_value(reply).unwrap())
}

fn summarize_stats_with(
    graph: &[GlucoseSample],
    days: u8,
    (target_low, target_high): (u16, u16),
    now: DateTime<Utc>,
    coverage: GraphCoverage,
) -> Result<StatsReply, CallToolResult> {
    let hours = i64::from(days) * 24;
    let (items, window_filter_applied) = select_graph_window(graph, hours, now);
    let values: Vec<f64> = items
        .iter()
        .map(|item| item.value_in_mg_per_dl)
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    if values.is_empty() {
        return Err(failure_code(
            "insufficient_data",
            "No valid historical glucose values were returned.",
        ));
    }
    let count = values.len();
    let average = values.iter().sum::<f64>() / count as f64;
    let variance = values.iter().map(|v| (v - average).powi(2)).sum::<f64>() / count as f64;
    let standard_deviation = variance.sqrt();
    let low = f64::from(target_low);
    let high = f64::from(target_high);
    let below = values.iter().filter(|v| **v < low).count();
    let above = values.iter().filter(|v| **v > high).count();
    let in_range = count - below - above;
    let period_status = if coverage.period_span_sufficient {
        "sample_summary"
    } else {
        "partial"
    };
    let weighted = time_weighted_stats(graph, hours, (low, high), now);
    Ok(StatsReply {
        requested_days: days,
        period_status,
        window_verified: false,
        window_filter_applied,
        coverage,
        sample_count: count,
        average_glucose_mg_dl: average,
        standard_deviation_mg_dl: standard_deviation,
        coefficient_of_variation_percent: standard_deviation / average * 100.0,
        sample_percentages: SamplePercentages {
            below_range: below as f64 / count as f64 * 100.0,
            in_range: in_range as f64 / count as f64 * 100.0,
            above_range: above as f64 / count as f64 * 100.0,
        },
        target_range_mg_dl: TargetRange {
            low: target_low,
            high: target_high,
        },
        reason: if period_status == "partial" {
            "The sample statistics describe only the returned graph, which covers less than the requested period; see coverage for the observed span. They are not a summary of the requested days. time_weighted reports how much of the requested window the readings cover."
        } else {
            "The sample statistics count each selected graph sample once. time_weighted weights each reading by the time it covers."
        },
        time_in_range_percent: weighted.time_in_range_percent,
        gmi_percent: weighted.gmi_percent,
        gmi_unavailable_reason: weighted.gmi_unavailable_reason,
        gmi_method: GMI_METHOD,
        time_weighted: weighted.reply,
        cache_hit: false,
        data_source: LIVE_API,
    })
}

struct WeightedStats {
    reply: TimeWeightedReply,
    time_in_range_percent: Option<f64>,
    gmi_percent: Option<f64>,
    gmi_unavailable_reason: Option<metrics::GmiUnavailable>,
}

const GMI_METHOD: &str = "An estimate: 3.31 + 0.02392 × time-weighted mean glucose (mg/dL), \
reported only for a requested window of at least 14 days with at least 70% of it covered. \
Method from the international consensus on CGM time in range: \
https://pmc.ncbi.nlm.nih.gov/articles/PMC6973648/";

/// Time-weighted figures over the requested window (ADR 0003), shared by the
/// live and store paths. Each reading stands for the time until the next, at
/// most 15 minutes; readings without a usable UTC time are excluded.
fn time_weighted_stats(
    graph: &[GlucoseSample],
    hours: i64,
    target: (f64, f64),
    now: DateTime<Utc>,
) -> WeightedStats {
    let samples: Vec<metrics::Sample> = graph
        .iter()
        .map(|item| metrics::Sample {
            at: assess_sample(item, now).observed_at_utc,
            mg_dl: item.value_in_mg_per_dl,
        })
        .collect();
    let weighted = metrics::time_weighted(&samples, now - Duration::hours(hours), now, target);
    let (gmi_percent, gmi_unavailable_reason) = match weighted.gmi {
        Gmi::Estimate(value) => (Some(value), None),
        Gmi::Unavailable(reason) => (None, Some(reason)),
    };
    WeightedStats {
        reply: TimeWeightedReply {
            status: if weighted.summary.is_some() {
                "ok"
            } else {
                "insufficient_data"
            },
            window_start_utc: weighted.from,
            window_end_utc: weighted.to,
            max_minutes_per_reading: metrics::MAX_SAMPLE_MINUTES,
            covered_minutes: weighted.covered_seconds as f64 / 60.0,
            coverage_percent: weighted.coverage_percent,
            readings_used: weighted.samples_used,
            readings_without_utc: weighted.samples_without_utc,
            summary: weighted.summary,
            consensus_reference: metrics::CONSENSUS_REFERENCE,
        },
        time_in_range_percent: weighted
            .summary
            .map(|summary| summary.consensus_bands_percent.target),
        gmi_percent,
        gmi_unavailable_reason,
    }
}

/// Tests read replies as JSON.
#[cfg(test)]
fn summarize_trends(
    period: &'static str,
    hours: i64,
    points: &[GlucoseSample],
    coverage: GraphCoverage,
    now: DateTime<Utc>,
) -> serde_json::Value {
    serde_json::to_value(trends_reply(period, hours, points, coverage, now)).unwrap()
}

fn trends_reply(
    period: &'static str,
    hours: i64,
    points: &[GlucoseSample],
    coverage: GraphCoverage,
    now: DateTime<Utc>,
) -> TrendsReply {
    let (items, window_filter_applied) = select_graph_window(points, hours, now);
    let rates = observed_rates(&items, now);
    let values: Vec<f64> = items
        .iter()
        .map(|item| item.value_in_mg_per_dl)
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    let min = values.iter().copied().reduce(f64::min);
    let max = values.iter().copied().reduce(f64::max);
    let mut falling = 0;
    let mut stable = 0;
    let mut rising = 0;
    let mut unknown = 0;
    for item in &items {
        match trend(item.trend_arrow) {
            Some("falling_fast" | "falling") => falling += 1,
            Some("stable") => stable += 1,
            Some("rising" | "rising_fast") => rising += 1,
            _ => unknown += 1,
        }
    }
    let period_status = if coverage.period_span_sufficient {
        "sample_summary"
    } else {
        "partial"
    };
    TrendsReply {
        requested_period: period,
        window_verified: false,
        window_filter_applied,
        coverage,
        period_status,
        sample_count: values.len(),
        observed_min_mg_dl: min,
        observed_max_mg_dl: max,
        trend_arrow_counts: TrendCounts {
            falling,
            stable,
            rising,
            unknown,
        },
        adjacent_point_rates: rates,
        dawn_phenomenon: (),
        meal_response_average: (),
        overnight_stability: (),
        reason: "Rates are descriptive changes between returned graph points, not source trend arrows or treatment guidance. The graph does not contain meal and sleep context.",
        cache_hit: false,
        data_source: LIVE_API,
    }
}

/// One page of history: the newest `limit` readings at or before `before`,
/// oldest first. Readings sharing the page's oldest second are never split
/// across pages. Readings without a UTC time (live graphs only) cannot be
/// placed, so they are paged by position and `before` does not apply.
struct Page {
    readings: Vec<HistoryPoint>,
    truncated: bool,
    next_before: Option<DateTime<Utc>>,
}

fn page(
    mut points: Vec<HistoryPoint>,
    limit: usize,
    before: Option<DateTime<Utc>>,
    placeable: bool,
) -> Page {
    if !placeable {
        let truncated = points.len() > limit;
        let start = points.len().saturating_sub(limit);
        return Page {
            readings: points.split_off(start),
            truncated,
            next_before: None,
        };
    }
    points.sort_by_key(|point| point.observed_at_utc);
    if let Some(before) = before {
        points.retain(|point| point.observed_at_utc.is_some_and(|at| at <= before));
    }
    if points.len() <= limit {
        return Page {
            readings: points,
            truncated: false,
            next_before: None,
        };
    }
    let mut start = points.len() - limit;
    let boundary = points[start].observed_at_utc;
    if points[start - 1].observed_at_utc == boundary {
        // The cut falls inside one second: leave that second for the next
        // page, unless it is the whole page.
        let after_group = points[start..]
            .iter()
            .position(|point| point.observed_at_utc != boundary)
            .map(|offset| start + offset);
        match after_group {
            Some(after) => {
                return Page {
                    readings: points.split_off(after),
                    truncated: true,
                    next_before: boundary,
                };
            }
            None => {
                start = points
                    .iter()
                    .position(|point| point.observed_at_utc == boundary)
                    .unwrap_or(start);
            }
        }
    }
    let next_before = (start > 0)
        .then(|| {
            points[start]
                .observed_at_utc
                .map(|at| at - Duration::seconds(1))
        })
        .flatten();
    Page {
        readings: points.split_off(start),
        truncated: start > 0,
        next_before,
    }
}

/// Validated history paging arguments.
struct Paging {
    limit: u32,
    before: Option<DateTime<Utc>>,
}

fn paging(args: &HistoryArgs) -> Result<Paging, CallToolResult> {
    let limit = args.limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
    if invalid_window(limit, MAX_HISTORY_LIMIT) {
        return Err(failure_code(
            "invalid_input",
            "limit must be between 1 and 5000.",
        ));
    }
    let before = match &args.before {
        None => None,
        Some(text) => Some(
            DateTime::parse_from_rfc3339(text)
                .map_err(|_| {
                    failure_code(
                        "invalid_input",
                        "before must be an RFC 3339 time, such as 2026-11-05T20:00:00Z.",
                    )
                })?
                .with_timezone(&Utc),
        ),
    };
    Ok(Paging { limit, before })
}

fn invalid_window(value: u32, max: u32) -> bool {
    value == 0 || value > max
}

// --- Local store paths. None of these contact LibreLinkUp. ---------------

fn point_source(origin: Origin) -> PointSource {
    match origin {
        Origin::Graph => PointSource::Graph,
        Origin::CurrentMeasurement => PointSource::CurrentMeasurement,
        Origin::LibreviewCsvHistoric => PointSource::LibreviewCsvHistoric,
        Origin::LibreviewCsvScan => PointSource::LibreviewCsvScan,
    }
}

fn samples(readings: &[StoredReading]) -> Vec<GlucoseSample> {
    readings
        .iter()
        .map(|reading| reading.sample.clone())
        .collect()
}

async fn local_current(local: &LocalStore) -> CallToolResult {
    let latest = match local
        .query(|store, key| key.map_or(Ok(None), |key| store.latest_reading(key)))
        .await
    {
        Ok(latest) => latest,
        Err(error) => return error,
    };
    let now = Utc::now();
    let reading = match latest {
        Some(reading) => {
            let (glucose_mg_dl, value_quality) = value(reading.sample.value_in_mg_per_dl);
            Reading {
                glucose_mg_dl,
                value_quality,
                // Retrieval time is when the collector first stored it, so a
                // lagging collector shows `stale` honestly.
                time: assess_sample_at(&reading.sample, reading.first_seen_utc, now),
                trend: trend(reading.sample.trend_arrow),
                cache_hit: false,
                data_source: LOCAL_STORE,
            }
        }
        None => Reading {
            glucose_mg_dl: None,
            value_quality: ValueQuality::Missing,
            time: assess_time_at(None, None, local.status.heartbeat.at, now),
            trend: None,
            cache_hit: false,
            data_source: LOCAL_STORE,
        },
    };
    success(reading)
}

async fn local_history(local: &LocalStore, hours: u16, paging: Paging) -> CallToolResult {
    let now = Utc::now();
    let hours_i64 = i64::from(hours);
    let (readings, polls) = match local.window(hours_i64, now).await {
        Ok(window) => window,
        Err(error) => return error,
    };
    let mut coverage = coverage_with(
        &samples(&readings),
        hours_i64,
        now,
        local_coverage(&readings, now),
    );
    classify_gaps(&mut coverage, &polls);
    let points: Vec<HistoryPoint> = readings
        .iter()
        .map(|reading| history_point(&reading.sample, point_source(reading.origin), now))
        .collect();
    let readings_in_window = points.len();
    let page = page(points, paging.limit as usize, paging.before, true);
    success(HistoryReply {
        requested_hours: hours,
        window_verified: false,
        window_filter_applied: true,
        cache_hit: false,
        retrieved_at_utc: readings.iter().map(|reading| reading.first_seen_utc).max(),
        current_measurement_appended: readings
            .iter()
            .any(|reading| reading.origin == Origin::CurrentMeasurement),
        coverage,
        reason: "Readings come from the local collector's store, filtered by their UTC timestamps. Coverage and gaps show where the store has no data.",
        total_readings: page.readings.len(),
        readings_in_window,
        limit: paging.limit,
        before: paging.before,
        truncated: page.truncated,
        next_before: page.next_before,
        readings: page.readings,
        data_source: LOCAL_STORE,
    })
}

async fn local_stats(local: &LocalStore, days: u8, range: (u16, u16)) -> CallToolResult {
    let now = Utc::now();
    let hours = i64::from(days) * 24;
    let (readings, polls) = match local.window(hours, now).await {
        Ok(window) => window,
        Err(error) => return error,
    };
    let points = samples(&readings);
    let mut coverage = coverage_with(&points, hours, now, local_coverage(&readings, now));
    classify_gaps(&mut coverage, &polls);
    match summarize_stats_with(&points, days, range, now, coverage) {
        Ok(mut body) => {
            body.reason = if body.period_status == "partial" {
                "The sample statistics describe only the stored readings, which cover less than the requested period; see coverage for the observed span and gaps. time_weighted reports how much of the requested window the readings cover."
            } else {
                "The sample statistics count each stored reading once. time_weighted weights each reading by the time it covers."
            };
            body.cache_hit = false;
            body.data_source = LOCAL_STORE;
            success(body)
        }
        Err(error) => error,
    }
}

async fn local_trends(local: &LocalStore, period: &'static str, hours: i64) -> CallToolResult {
    let now = Utc::now();
    let (readings, polls) = match local.window(hours, now).await {
        Ok(window) => window,
        Err(error) => return error,
    };
    let points = samples(&readings);
    let mut coverage = coverage_with(&points, hours, now, local_coverage(&readings, now));
    classify_gaps(&mut coverage, &polls);
    let mut body = trends_reply(period, hours, &points, coverage, now);
    body.reason = "Rates are descriptive changes between stored points, not source trend arrows or treatment guidance. The data does not contain meal and sleep context.";
    body.cache_hit = false;
    body.data_source = LOCAL_STORE;
    success(body)
}

async fn local_sensors(local: &LocalStore) -> CallToolResult {
    let sensors = match local.query(|store, _| store.sensors()).await {
        Ok(sensors) => sensors,
        Err(error) => return error,
    };
    let now = Utc::now();
    let sensors: Vec<_> = sensors
        .iter()
        .map(|sensor| sensor_json(sensor, now))
        .collect();
    success(SensorsReply {
        sensor_count: sensors.len(),
        active_sensors: sensors,
        cache_hit: false,
        data_source: LOCAL_STORE,
    })
}

async fn local_validate(local: &LocalStore) -> CallToolResult {
    let (last_poll, stored, latest) = match local
        .query(|store, key| {
            let (count, latest) = match key {
                Some(key) => (store.reading_count(key)?, store.latest_reading(key)?),
                None => (0, None),
            };
            Ok((store.last_poll()?, count, latest))
        })
        .await
    {
        Ok(result) => result,
        Err(error) => return error,
    };
    let now = Utc::now();
    let reading_time = match &latest {
        Some(reading) => assess_sample_at(&reading.sample, reading.first_seen_utc, now),
        None => assess_time_at(None, None, local.status.heartbeat.at, now),
    };
    success(ValidateReply::Local(LocalStatus {
        collector_running: true,
        collector_heartbeat_age_seconds: now
            .signed_duration_since(local.status.heartbeat.at)
            .num_seconds()
            .max(0),
        last_poll_outcome: last_poll.as_ref().map(|poll| poll.outcome.clone()),
        last_poll_at_utc: last_poll.as_ref().map(|poll| poll.at),
        stored_reading_count: stored,
        reading_time,
        upstream_contacted: false,
        cache_hit: false,
        stale_threshold_minutes: STALE_AFTER_MINUTES,
        data_source: LOCAL_STORE,
    }))
}

impl LibreMcp {
    /// The current reading, from the same source selection as every tool.
    async fn current_glucose(&self) -> CallToolResult {
        if let DataPath::Local(local) = self.data_path().await {
            return local_current(&local).await;
        }
        let raw = match self.fetcher.fetch().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let item = &raw.current;
        let (glucose_mg_dl, value_quality) = value(item.value_in_mg_per_dl);
        success(Reading {
            glucose_mg_dl,
            value_quality,
            time: assess_time_at(
                Some(&item.timestamp),
                Some(&item.factory_timestamp),
                raw.retrieved_at_utc,
                Utc::now(),
            ),
            trend: trend(item.trend_arrow),
            cache_hit: raw.cache_hit,
            data_source: LIVE_API,
        })
    }
}

#[tool_router]
impl LibreMcp {
    #[tool(
        description = "Get the latest available glucose reading with its source timestamp and freshness. An unknown or stale reading is not real-time data. Also available as the resource glucose://current.",
        output_schema = output_schema::<Reading>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_current_glucose(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> CallToolResult {
        self.current_glucose().await
    }

    #[tool(
        description = "Return glucose readings within the requested hours (up to 2160 from the local collector; about 12 from the live API), oldest first. Pages of at most `limit` readings: pass `next_before` back as `before` for older ones. Coverage always describes the whole window. Each point carries its time and source; freshness applies only to get_current_glucose.",
        output_schema = output_schema::<HistoryReply>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_glucose_history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> CallToolResult {
        let hours = args.hours.unwrap_or(24);
        if invalid_window(u32::from(hours), MAX_HISTORY_HOURS) {
            return failure_code("invalid_input", "hours must be between 1 and 2160.");
        }
        let paging = match paging(&args) {
            Ok(paging) => paging,
            Err(error) => return error,
        };
        if let DataPath::Local(local) = self.data_path().await {
            return local_history(&local, hours, paging).await;
        }
        let raw = match self.fetcher.fetch().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let now = Utc::now();
        let current = &raw.current;
        let (points, current_appended) = points_with_current(&raw.graph, current);
        let coverage = graph_coverage(&points, i64::from(hours), now);
        let (items, window_filter_applied) = select_graph_window(&points, i64::from(hours), now);
        let readings: Vec<HistoryPoint> = items
            .into_iter()
            .map(|item| {
                let source =
                    if current_appended && item.factory_timestamp == current.factory_timestamp {
                        PointSource::CurrentMeasurement
                    } else {
                        PointSource::Graph
                    };
                history_point(item, source, now)
            })
            .collect();
        let readings_in_window = readings.len();
        let page = page(
            readings,
            paging.limit as usize,
            paging.before,
            window_filter_applied,
        );
        success(HistoryReply {
            requested_hours: hours,
            window_verified: false,
            window_filter_applied,
            cache_hit: raw.cache_hit,
            retrieved_at_utc: Some(raw.retrieved_at_utc),
            current_measurement_appended: current_appended,
            coverage,
            reason: if window_filter_applied {
                "Readings were filtered by offset-aware or inferred factory UTC timestamps, but API graph coverage may be shorter than requested."
            } else {
                "The API graph may be shorter than requested and some timestamps lack a usable time basis. All returned graph points are shown, paged by position."
            },
            total_readings: page.readings.len(),
            readings_in_window,
            limit: paging.limit,
            before: paging.before,
            truncated: page.truncated,
            next_before: page.next_before,
            readings: page.readings,
            data_source: LIVE_API,
        })
    }
    #[tool(
        description = "Calculate descriptive statistics: sample statistics, plus time-weighted coverage, consensus bands, and time in range. GMI is an estimate, given only for 14 or more days with at least 70% coverage. The LibreLinkUp graph covers about 12 hours, so longer live requests are labeled partial.",
        output_schema = output_schema::<StatsReply>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_glucose_stats(&self, Parameters(args): Parameters<StatsArgs>) -> CallToolResult {
        let days = args.days.unwrap_or(7);
        if invalid_window(u32::from(days), 90) {
            return failure_code("invalid_input", "days must be between 1 and 90.");
        }
        let range = match self.target_range() {
            Ok(range) => range,
            Err(message) => return failure(message),
        };
        if let DataPath::Local(local) = self.data_path().await {
            return local_stats(&local, days, range).await;
        }
        let raw = match self.fetcher.fetch().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let (target_low, target_high) = range;
        let (points, _) = points_with_current(&raw.graph, &raw.current);
        match live_stats(&points, days, target_low, target_high, Utc::now()) {
            Ok(mut body) => {
                body.cache_hit = raw.cache_hit;
                success(body)
            }
            Err(error) => error,
        }
    }

    #[tool(
        description = "Summarize observed graph values and adjacent-point rates. Partial periods are labeled; no meal, diagnosis, or treatment inference is made.",
        output_schema = output_schema::<TrendsReply>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_glucose_trends(&self, Parameters(args): Parameters<TrendsArgs>) -> CallToolResult {
        let (period, hours) = match args.period.unwrap_or(Period::Weekly) {
            Period::Daily => ("daily", 24),
            Period::Weekly => ("weekly", 7 * 24),
            Period::Monthly => ("monthly", 30 * 24),
        };
        if let DataPath::Local(local) = self.data_path().await {
            return local_trends(&local, period, hours).await;
        }
        let raw = match self.fetcher.fetch().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let now = Utc::now();
        let (points, _) = points_with_current(&raw.graph, &raw.current);
        let coverage = graph_coverage(&points, hours, now);
        let mut body = trends_reply(period, hours, &points, coverage, now);
        body.cache_hit = raw.cache_hit;
        success(body)
    }

    #[tool(
        description = "Return masked sensor identifiers, activation epoch, warmup minutes, and product type code. Unknown details remain null.",
        output_schema = output_schema::<SensorsReply>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn get_sensor_info(&self, Parameters(_args): Parameters<EmptyArgs>) -> CallToolResult {
        if let DataPath::Local(local) = self.data_path().await {
            return local_sensors(&local).await;
        }
        let raw = match self.fetcher.fetch().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let now = Utc::now();
        let sensors: Vec<_> = raw
            .active_sensors
            .iter()
            .map(|active| sensor_json(&SensorSummary::from_record(active, now), now))
            .collect();
        success(SensorsReply {
            sensor_count: sensors.len(),
            active_sensors: sensors,
            cache_hit: raw.cache_hit,
            data_source: LIVE_API,
        })
    }

    #[tool(
        description = "Explain how to set LibreLinkUp credentials in the local terminal. This tool accepts no passwords, tokens, or other secrets.",
        output_schema = output_schema::<CredentialsReply>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    fn configure_credentials(&self, Parameters(_args): Parameters<EmptyArgs>) -> CallToolResult {
        success(CredentialsReply {
            credential_input_supported: false,
            setup_command: "librelink-mcp-rs setup",
            message: "Run `librelink-mcp-rs setup` in a local terminal (or `cargo run -- setup` in the project directory). Credentials cannot be entered through MCP.",
        })
    }

    #[tool(
        description = "Set local target range bounds in mg/dL for descriptive sample summaries.",
        output_schema = output_schema::<RangesReply>(),
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn configure_ranges(&self, Parameters(args): Parameters<RangesArgs>) -> CallToolResult {
        if args.target_low == 0 || args.target_low >= args.target_high || args.target_high > 1000 {
            return failure_code(
                "invalid_input",
                "Require 0 < target_low < target_high <= 1000 mg/dL.",
            );
        }
        let mut settings = match load_settings() {
            Ok(settings) => settings,
            Err(message) => return failure(message),
        };
        settings.target_low = args.target_low;
        settings.target_high = args.target_high;
        if let Err(message) = save_settings(&settings) {
            return failure(message);
        }
        success(RangesReply {
            saved: true,
            target_low: args.target_low,
            target_high: args.target_high,
            unit: "mg/dL",
        })
    }

    #[tool(
        description = "Test LibreLinkUp access and report source-data availability without exposing credentials or glucose values.",
        output_schema = output_schema::<ValidateReply>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn validate_connection(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> CallToolResult {
        if let DataPath::Local(local) = self.data_path().await {
            return local_validate(&local).await;
        }
        let raw = match self.fetcher.fetch().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        success(ValidateReply::Live(LiveStatus {
            connected: true,
            graph_points_returned: raw.graph.len(),
            active_sensors_returned: raw.active_sensors.len(),
            reading_time: assess_time_at(
                Some(&raw.current.timestamp),
                Some(&raw.current.factory_timestamp),
                raw.retrieved_at_utc,
                Utc::now(),
            ),
            cache_hit: raw.cache_hit,
            stale_threshold_minutes: STALE_AFTER_MINUTES,
            data_source: LIVE_API,
        }))
    }
}

/// The current reading as a resource: the same body, and the same source
/// selection, as `get_current_glucose`.
const CURRENT_RESOURCE_URI: &str = "glucose://current";

impl LibreMcp {
    async fn read_current_resource(&self) -> ReadResourceResult {
        let result = self.current_glucose().await;
        let body = result
            .structured_content
            .unwrap_or(serde_json::Value::Null)
            .to_string();
        ReadResourceResult::new(vec![
            ResourceContents::text(body, CURRENT_RESOURCE_URI).with_mime_type("application/json"),
        ])
    }
}

#[tool_handler]
impl ServerHandler for LibreMcp {
    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new(CURRENT_RESOURCE_URI, "current_glucose")
                .with_description(
                    "The latest glucose reading, the same as get_current_glucose. Check freshness before calling it current.",
                )
                .with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != CURRENT_RESOURCE_URI {
            return Err(ErrorData::resource_not_found(
                "Unknown resource; the only one is glucose://current.",
                None,
            ));
        }
        Ok(self.read_current_resource().await.into())
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                .with_title("LibreLinkUp MCP (Rust)"),
        )
        .with_instructions(SERVER_INSTRUCTIONS)
    }
}

pub(crate) async fn serve(fetcher: Fetcher, db: Option<PathBuf>) -> Result<(), &'static str> {
    let service = LibreMcp::new(fetcher)
        .with_store(db)
        .serve(stdio())
        .await
        .map_err(|_| "Unable to start MCP stdio server.")?;
    service
        .waiting()
        .await
        .map_err(|_| "MCP stdio server stopped with an error.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::masked_identifier;
    use crate::store::Heartbeat;
    use std::path::Path;

    #[test]
    fn all_original_names_are_exposed_without_a_secret_argument() {
        let tools = LibreMcp::tool_router().list_all();
        let mut names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "configure_credentials",
                "configure_ranges",
                "get_current_glucose",
                "get_glucose_history",
                "get_glucose_stats",
                "get_glucose_trends",
                "get_sensor_info",
                "validate_connection",
            ]
        );
        let configure = tools
            .iter()
            .find(|tool| tool.name == "configure_credentials")
            .unwrap();
        let schema = serde_json::to_string(&configure.input_schema).unwrap();
        assert!(!schema.contains("password"));
        assert!(!schema.contains("token"));
        assert!(schema.contains("\"additionalProperties\":false"));
    }

    #[test]
    fn server_identifies_itself_and_explains_freshness() {
        let info = fake_server(Ok(empty_snapshot())).0.get_info();
        assert_eq!(info.server_info.name, "librelink-mcp-rs");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("freshness"));
        assert!(instructions.contains("partial"));
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn sensor_identifiers_are_masked_to_four_characters() {
        assert_eq!(masked_identifier("0U28SYNTH9").as_deref(), Some("****NTH9"));
        assert_eq!(masked_identifier("ab1").as_deref(), Some("****"));
        assert_eq!(masked_identifier("  "), None);
        assert_eq!(
            masked_identifier("01a15fb1-0000-0000-0000-00000000abcd").as_deref(),
            Some("****abcd")
        );
    }

    #[test]
    fn missing_values_and_trends_are_not_invented() {
        assert!(value(0.0).0.is_none());
        assert!(value(f64::NAN).0.is_none());
        assert_eq!(trend(None), None);
        assert_eq!(trend(Some(42)), None);
        assert_eq!(trend(Some(3)), Some("stable"));
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            sensor_activation_time(1_652_400_270, now)
                .unwrap()
                .timestamp(),
            1_652_400_270
        );
        assert!(sensor_activation_time(4, now).is_none());
        assert!(sensor_activation_time(i32::MAX, now).is_none());
    }

    fn synthetic_item(timestamp: &str) -> GlucoseSample {
        GlucoseSample {
            factory_timestamp: timestamp.to_owned(),
            timestamp: timestamp.to_owned(),
            value_in_mg_per_dl: 110.0,
            trend_arrow: Some(3),
            stored_time: None,
        }
    }

    #[test]
    fn window_filters_offset_aware_graph_at_boundary() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let graph = [
            synthetic_item("2026-11-05T18:59:59Z"),
            synthetic_item("2026-11-05T19:00:00Z"),
            synthetic_item("2026-11-05T19:30:00Z"),
            synthetic_item("2026-11-05T20:00:01Z"),
        ];
        let (selected, applied) = select_graph_window(&graph, 1, now);
        assert!(applied);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].timestamp, "2026-11-05T19:00:00Z");
        assert_eq!(selected[1].timestamp, "2026-11-05T19:30:00Z");
    }

    #[test]
    fn mixed_offset_graph_is_not_partially_filtered() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut graph = [
            synthetic_item("2026-11-05T18:00:00Z"),
            synthetic_item("11/05/2026 03:30:00 PM"),
        ];
        graph[1].factory_timestamp.clear();
        let (selected, applied) = select_graph_window(&graph, 1, now);
        assert!(!applied);
        assert_eq!(selected.len(), graph.len());
    }

    #[test]
    fn inferred_factory_utc_can_filter_graph_window() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut graph = [
            synthetic_item("11/05/2026 01:50:00 PM"),
            synthetic_item("11/05/2026 02:58:00 PM"),
        ];
        graph[0].factory_timestamp = "11/05/2026 06:50:00 PM".to_owned();
        graph[1].factory_timestamp = "11/05/2026 07:58:00 PM".to_owned();
        let (selected, applied) = select_graph_window(&graph, 1, now);
        assert!(applied);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].timestamp, "11/05/2026 02:58:00 PM");
    }

    #[test]
    fn short_graph_does_not_claim_a_week_of_statistics() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let graph = [
            synthetic_item("2026-11-05T19:00:00Z"),
            synthetic_item("2026-11-05T19:15:00Z"),
            synthetic_item("2026-11-05T19:30:00Z"),
        ];
        let coverage = graph_coverage(&graph, 7 * 24, now);
        assert!(!coverage.period_span_sufficient);
        assert_eq!(coverage.actual_points_in_window, Some(3));
        assert_eq!(coverage.expected_points_estimate, Some(673));
        assert_eq!(coverage.observed_span_hours, Some(0.5));
    }

    #[test]
    fn coverage_detects_interior_gaps_without_inventing_missing_values() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let graph = [
            synthetic_item("2026-11-05T19:00:00Z"),
            synthetic_item("2026-11-05T19:15:00Z"),
            synthetic_item("2026-11-05T19:45:00Z"),
            synthetic_item("2026-11-05T20:00:00Z"),
        ];
        let coverage = graph_coverage(&graph, 1, now);
        // The span reaches both ends; the gap is reported, not hidden.
        assert!(coverage.period_span_sufficient);
        assert_eq!(coverage.interior_gap_count, 1);
        assert_eq!(coverage.actual_points_in_window, Some(4));
        assert_eq!(coverage.expected_points_estimate, Some(5));
    }

    #[test]
    fn adjacent_rates_are_descriptive_and_skip_long_gaps() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut graph = [
            synthetic_item("2026-11-05T19:00:00Z"),
            synthetic_item("2026-11-05T19:15:00Z"),
            synthetic_item("2026-11-05T19:30:00Z"),
            synthetic_item("2026-11-05T20:15:00Z"),
        ];
        graph[0].value_in_mg_per_dl = 100.0;
        graph[1].value_in_mg_per_dl = 115.0;
        graph[2].value_in_mg_per_dl = 100.0;
        let items: Vec<_> = graph.iter().collect();
        let rates = observed_rates(&items, now);
        assert_eq!(rates.pair_count, 2);
        assert_eq!(rates.minimum_mg_dl_per_minute, Some(-1.0));
        assert_eq!(rates.maximum_mg_dl_per_minute, Some(1.0));
        assert_eq!(rates.average_mg_dl_per_minute, Some(0.0));
    }

    #[test]
    fn current_measurement_closes_the_graph_lag_without_duplicating() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let graph = [
            synthetic_item("2026-11-05T19:30:00Z"),
            synthetic_item("2026-11-05T19:35:00Z"),
        ];
        let current = synthetic_item("2026-11-05T19:58:00Z");

        let (points, appended) = points_with_current(&graph, &current);
        assert!(appended);
        assert_eq!(points.len(), 3);
        let coverage = graph_coverage(&points, 1, now);
        assert_eq!(
            coverage.last_observed_at_utc.unwrap().to_rfc3339(),
            "2026-11-05T19:58:00+00:00"
        );
        assert_eq!(coverage.end_gap_minutes, Some(2.0));

        // Already in the graph: not added twice.
        let (points, appended) = points_with_current(&graph, &graph[1]);
        assert!(!appended);
        assert_eq!(points.len(), 2);

        // A missing current value is not appended.
        let mut missing = current.clone();
        missing.value_in_mg_per_dl = 0.0;
        assert!(!points_with_current(&graph, &missing).1);
    }

    #[test]
    fn history_points_omit_freshness_and_label_their_source() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let item = synthetic_item("2026-11-05T12:00:00Z");
        let point = serde_json::to_value(history_point(&item, PointSource::Graph, now)).unwrap();
        assert!(point.get("freshness").is_none());
        assert!(point.get("retrieved_at_utc").is_none());
        assert_eq!(point["source"], "graph");
        assert_eq!(point["time_basis"], "offset_known");
        assert_eq!(point["observed_at_utc"], "2026-11-05T12:00:00Z");
        let current =
            serde_json::to_value(history_point(&item, PointSource::CurrentMeasurement, now))
                .unwrap();
        assert_eq!(current["source"], "current_measurement");
    }

    #[test]
    fn short_graph_yields_partial_stats_instead_of_an_error() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // A realistic ~12 hour graph at 15 minute cadence, shorter than one day.
        let start = now - Duration::hours(12);
        let mut graph: Vec<GlucoseSample> = (0..=48)
            .map(|step| {
                let at = start + Duration::minutes(15 * step);
                synthetic_item(&at.to_rfc3339())
            })
            .collect();
        graph[0].value_in_mg_per_dl = 60.0;
        graph[1].value_in_mg_per_dl = 200.0;

        let body = summarize_stats(&graph, 1, 70, 180, now).expect("partial stats");
        assert_eq!(body["period_status"], "partial");
        assert_eq!(body["sample_count"], 49);
        assert_eq!(body["coverage"]["period_span_sufficient"], false);
        assert_eq!(body["gmi_percent"], serde_json::Value::Null);
        let in_range = body["sample_percentages"]["in_range"].as_f64().unwrap();
        assert!((in_range - 47.0 / 49.0 * 100.0).abs() < 1e-9);
    }

    #[test]
    fn stats_with_no_valid_values_is_insufficient_data() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut graph = [synthetic_item("2026-11-05T19:45:00Z")];
        graph[0].value_in_mg_per_dl = 0.0;
        let error = summarize_stats(&graph, 1, 70, 180, now).unwrap_err();
        assert_eq!(
            error.structured_content.as_ref().unwrap()["error_code"],
            "insufficient_data"
        );
    }

    #[test]
    fn stats_weight_dense_readings_by_the_time_they_cover() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let start = now - Duration::hours(13);
        let at = |minutes: i64, value: f64| {
            let mut item = synthetic_item(&(start + Duration::minutes(minutes)).to_rfc3339());
            item.value_in_mg_per_dl = value;
            item
        };
        // 12 hours every 15 minutes in range, then 1 hour every minute high.
        let mut graph: Vec<GlucoseSample> = (0..48).map(|step| at(15 * step, 120.0)).collect();
        graph.extend((720..780).map(|minute| at(minute, 220.0)));

        let body = summarize_stats(&graph, 1, 70, 180, now).unwrap();
        let sample_above = body["sample_percentages"]["above_range"].as_f64().unwrap();
        assert!((sample_above - 60.0 / 108.0 * 100.0).abs() < 1e-9);
        let weighted = &body["time_weighted"];
        assert_eq!(weighted["status"], "ok");
        assert_eq!(weighted["covered_minutes"], 780.0);
        let coverage = weighted["coverage_percent"].as_f64().unwrap();
        assert!((coverage - 13.0 / 24.0 * 100.0).abs() < 1e-9);
        let high = weighted["summary"]["consensus_bands_percent"]["high"]
            .as_f64()
            .unwrap();
        assert!((high - 100.0 / 13.0).abs() < 1e-9);
        let in_range = body["time_in_range_percent"].as_f64().unwrap();
        assert!((in_range - 1200.0 / 13.0).abs() < 1e-9);
        // A one-day window cannot carry GMI.
        assert_eq!(body["gmi_percent"], serde_json::Value::Null);
        assert_eq!(body["gmi_unavailable_reason"], "window_too_short");
        assert!(body["gmi_method"].as_str().unwrap().contains("PMC6973648"));
    }

    #[test]
    fn stats_without_utc_times_report_no_time_weighted_figures() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // Naive clocks that cannot be paired: no usable UTC time.
        let graph: Vec<GlucoseSample> = (0..4)
            .map(|step| {
                let mut item = synthetic_item(&format!("11/5/2026 7:{:02}:00 PM", 15 * step));
                item.factory_timestamp = "not a time".to_owned();
                item
            })
            .collect();
        let body = summarize_stats(&graph, 1, 70, 180, now).unwrap();
        assert_eq!(body["sample_count"], 4);
        let weighted = &body["time_weighted"];
        assert_eq!(weighted["status"], "insufficient_data");
        assert_eq!(weighted["readings_used"], 0);
        assert_eq!(weighted["readings_without_utc"], 4);
        assert_eq!(weighted["covered_minutes"], 0.0);
        assert_eq!(weighted["summary"], serde_json::Value::Null);
        assert_eq!(body["time_in_range_percent"], serde_json::Value::Null);
        assert_eq!(body["gmi_percent"], serde_json::Value::Null);
    }

    #[test]
    fn tool_result_has_structured_and_text_forms() {
        let result = success(serde_json::json!({ "sample_count": 2 }));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["sample_count"],
            2
        );
        assert_eq!(result.is_error, Some(false));
        let error = failure("Synthetic request failed.");
        assert_eq!(
            error.structured_content.as_ref().unwrap()["error_code"],
            "request_failed"
        );
        assert_eq!(error.is_error, Some(true));
        assert_eq!(error.content.len(), 1);
    }

    use crate::source::{DataSource, ReadFuture, SensorRecord, Snapshot};
    use chrono::SecondsFormat;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `DataSource` that never touches the network and counts its reads.
    struct FakeSource {
        result: Result<Snapshot, FetchError>,
        reads: Arc<AtomicUsize>,
    }

    impl DataSource for FakeSource {
        fn read(&self) -> ReadFuture<'_> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    fn fake_server(result: Result<Snapshot, FetchError>) -> (LibreMcp, Arc<AtomicUsize>) {
        let reads = Arc::new(AtomicUsize::new(0));
        let source = FakeSource {
            result,
            reads: Arc::clone(&reads),
        };
        (LibreMcp::new(Fetcher::new(Box::new(source))), reads)
    }

    fn empty_snapshot() -> Snapshot {
        Snapshot {
            patient_id: "synthetic-patient-empty".to_owned(),
            current: synthetic_item(""),
            graph: vec![],
            active_sensors: vec![],
        }
    }

    /// `minutes` before `now`, as a source timestamp. Tests read the clock
    /// once and build every timestamp from it, so a slow test cannot cross a
    /// second boundary between building data and checking it.
    fn minutes_before(now: DateTime<Utc>, minutes: i64) -> String {
        (now - Duration::minutes(minutes)).to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// Synthetic values: a lagging graph and a newer current reading.
    fn live_snapshot() -> Snapshot {
        let now = Utc::now();
        let mut current = synthetic_item(&minutes_before(now, 2));
        current.value_in_mg_per_dl = 142.0;
        current.trend_arrow = Some(4);
        Snapshot {
            patient_id: "synthetic-patient-live".to_owned(),
            current,
            graph: [30, 25, 20, 15]
                .into_iter()
                .map(|minutes| synthetic_item(&minutes_before(now, minutes)))
                .collect(),
            active_sensors: vec![SensorRecord {
                device_id: "synthetic-device-0042".to_owned(),
                serial_number: "SYNTH0001".to_owned(),
                activation_epoch_seconds: 1_652_400_270,
                warmup_minutes: 60,
                product_type_code: 4,
            }],
        }
    }

    fn body(result: &CallToolResult) -> &serde_json::Value {
        result.structured_content.as_ref().unwrap()
    }

    /// The patient ID exists only for the local store; every data tool must
    /// keep it out of both the structured and the text output.
    #[tokio::test]
    async fn no_data_tool_reveals_the_patient_id() {
        const PATIENT_ID: &str = "SYNTHETIC-PID-zq81-do-not-leak";
        let mut snapshot = live_snapshot();
        snapshot.patient_id = PATIENT_ID.to_owned();
        let (mut server, _) = fake_server(Ok(snapshot));
        server.fixed_ranges = Some((70, 180));
        let results = [
            server.get_current_glucose(Parameters(EmptyArgs {})).await,
            server
                .get_glucose_history(Parameters(HistoryArgs {
                    hours: Some(24),
                    limit: None,
                    before: None,
                }))
                .await,
            server
                .get_glucose_stats(Parameters(StatsArgs { days: Some(1) }))
                .await,
            server
                .get_glucose_trends(Parameters(TrendsArgs { period: None }))
                .await,
            server.get_sensor_info(Parameters(EmptyArgs {})).await,
            server.validate_connection(Parameters(EmptyArgs {})).await,
        ];
        for result in results {
            assert_eq!(result.is_error, Some(false));
            let serialized = serde_json::to_string(&result).unwrap();
            assert!(serialized.contains("cache_hit"));
            assert!(!serialized.contains(PATIENT_ID));
            assert!(!serialized.contains("zq81"));
        }
    }

    #[tokio::test]
    async fn tools_run_against_a_fake_source_without_the_network() {
        let (server, reads) = fake_server(Ok(live_snapshot()));
        let first = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        assert_eq!(first.is_error, Some(false));
        assert_eq!(body(&first)["glucose_mg_dl"], 142.0);
        assert_eq!(body(&first)["trend"], "rising");
        assert_eq!(body(&first)["freshness"], "recent");
        assert_eq!(body(&first)["cache_hit"], false);

        // The cache sits in front of the trait: the second call does not read.
        let sensors = server.get_sensor_info(Parameters(EmptyArgs {})).await;
        assert_eq!(body(&sensors)["sensor_count"], 1);
        assert_eq!(
            body(&sensors)["active_sensors"][0]["serial_number_masked"],
            "****0001"
        );
        assert_eq!(body(&sensors)["cache_hit"], true);
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cooldown_applies_to_any_source() {
        let throttled = FetchError {
            code: "rate_limited",
            message: "Synthetic throttle.".into(),
            retry_after_seconds: Some(60),
        };
        let (server, reads) = fake_server(Err(throttled));
        for _ in 0..2 {
            let result = server.validate_connection(Parameters(EmptyArgs {})).await;
            assert_eq!(result.is_error, Some(true));
            assert_eq!(body(&result)["error_code"], "rate_limited");
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn configuration_errors_do_not_start_a_cooldown() {
        let unconfigured = FetchError {
            code: "configuration_error",
            message: "Synthetic missing setup.".into(),
            retry_after_seconds: None,
        };
        let (server, reads) = fake_server(Err(unconfigured));
        for _ in 0..2 {
            let result = server.get_current_glucose(Parameters(EmptyArgs {})).await;
            assert_eq!(body(&result)["error_code"], "configuration_error");
        }
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn history_appends_the_reading_get_current_glucose_reports() {
        let (server, _) = fake_server(Ok(live_snapshot()));
        let current = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        let history = server
            .get_glucose_history(Parameters(HistoryArgs {
                hours: Some(1),
                limit: None,
                before: None,
            }))
            .await;
        let current = body(&current);
        let history = body(&history);
        assert_eq!(history["current_measurement_appended"], true);
        let appended: Vec<_> = history["readings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|point| point["source"] == "current_measurement")
            .collect();
        assert_eq!(appended.len(), 1);
        for field in [
            "glucose_mg_dl",
            "value_quality",
            "observed_at_utc",
            "time_basis",
            "source_timestamp",
            "trend",
        ] {
            assert_eq!(appended[0][field], current[field], "{field}");
        }
    }

    // --- Local store path -------------------------------------------------

    const STORE_PATIENT_ID: &str = "SYNTHETIC-PID-store-k27";
    const RAW_SERIAL: &str = "SYNTHSERIAL-RAW-0001";
    const RAW_DEVICE: &str = "synthetic-device-raw-7777";

    struct LocalFixture {
        _dir: tempfile::TempDir,
        db: PathBuf,
        /// The one clock reading every fixture timestamp is built from.
        now: DateTime<Utc>,
    }

    impl LocalFixture {
        /// `minutes` before the fixture's clock reading.
        fn at(&self, minutes: i64) -> DateTime<Utc> {
            self.now - Duration::minutes(minutes)
        }

        /// `minutes` before the fixture's clock reading, as a source timestamp.
        fn ago(&self, minutes: i64) -> String {
            minutes_before(self.now, minutes)
        }

        fn with_store<T>(&self, operation: impl FnOnce(&mut Store) -> T) -> T {
            operation(&mut Store::open(&self.db).unwrap())
        }
    }

    /// A store holding graph readings at `graph_minutes_ago`, an optional
    /// current reading, one sensor, and a heartbeat `heartbeat_age` seconds
    /// old (none if `None`).
    fn local_fixture(
        graph_minutes_ago: &[i64],
        current_minutes_ago: Option<i64>,
        heartbeat_age: Option<i64>,
    ) -> LocalFixture {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("readings.sqlite3");
        let fixture = LocalFixture {
            _dir: dir,
            db,
            now: Utc::now(),
        };
        let current = current_minutes_ago.map(|m| fixture.ago(m));
        let snapshot = Snapshot {
            patient_id: STORE_PATIENT_ID.to_owned(),
            current: synthetic_item(&current.unwrap_or_default()),
            graph: graph_minutes_ago
                .iter()
                .map(|m| synthetic_item(&fixture.ago(*m)))
                .collect(),
            active_sensors: vec![SensorRecord {
                device_id: RAW_DEVICE.to_owned(),
                serial_number: RAW_SERIAL.to_owned(),
                activation_epoch_seconds: 1_652_400_270,
                warmup_minutes: 60,
                product_type_code: 4,
            }],
        };
        fixture.with_store(|store| {
            store
                .insert_snapshot(STORE_PATIENT_ID, &snapshot, fixture.now)
                .unwrap();
            if let Some(age) = heartbeat_age {
                store
                    .write_heartbeat(&Heartbeat {
                        at: fixture.now - Duration::seconds(age),
                        pid: 4242,
                        version: "0.0.0-synthetic".to_owned(),
                    })
                    .unwrap();
            }
        });
        fixture
    }

    /// A server whose live source counts reads, pointed at `db`.
    fn store_server(db: &Path) -> (LibreMcp, Arc<AtomicUsize>) {
        let (server, reads) = fake_server(Ok(live_snapshot()));
        let mut server = server.with_store(Some(db.to_path_buf()));
        server.fixed_ranges = Some((70, 180));
        (server, reads)
    }

    /// Every 15 minutes from 5 minutes ago back to `hours` ago.
    fn every_15_minutes(hours: i64) -> Vec<i64> {
        (0..)
            .map(|step| 5 + 15 * step)
            .take_while(|minutes| *minutes <= hours * 60)
            .collect()
    }

    async fn all_data_tools(server: &LibreMcp) -> Vec<CallToolResult> {
        vec![
            server.get_current_glucose(Parameters(EmptyArgs {})).await,
            server
                .get_glucose_history(Parameters(HistoryArgs {
                    hours: Some(24),
                    limit: None,
                    before: None,
                }))
                .await,
            server
                .get_glucose_stats(Parameters(StatsArgs { days: Some(1) }))
                .await,
            server
                .get_glucose_trends(Parameters(TrendsArgs {
                    period: Some(Period::Daily),
                }))
                .await,
            server.get_sensor_info(Parameters(EmptyArgs {})).await,
            server.validate_connection(Parameters(EmptyArgs {})).await,
        ]
    }

    #[tokio::test]
    async fn source_selection_follows_the_collector_heartbeat() {
        let source_of = |result: &CallToolResult| body(result)["data_source"].clone();

        let fresh = local_fixture(&[20, 5], None, Some(10));
        let (server, reads) = store_server(&fresh.db);
        let reply = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        assert_eq!(source_of(&reply), "local_store");
        assert_eq!(reads.load(Ordering::SeqCst), 0);

        for age in [240, -600] {
            let stale = local_fixture(&[20, 5], None, Some(age));
            let (server, reads) = store_server(&stale.db);
            let reply = server.get_current_glucose(Parameters(EmptyArgs {})).await;
            assert_eq!(source_of(&reply), "live_api", "heartbeat age {age}");
            assert_eq!(reads.load(Ordering::SeqCst), 1);
        }

        // Readings but no heartbeat ever written.
        let silent = local_fixture(&[20, 5], None, None);
        let (server, _) = store_server(&silent.db);
        let reply = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        assert_eq!(source_of(&reply), "live_api");

        // No database file: live, and serve does not create one.
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("absent.sqlite3");
        let (server, reads) = store_server(&missing);
        let reply = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        assert_eq!(source_of(&reply), "live_api");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn the_local_store_path_never_contacts_the_api_or_leaks_identifiers() {
        let fixture = local_fixture(&every_15_minutes(30), Some(2), Some(10));
        let (server, reads) = store_server(&fixture.db);
        for result in all_data_tools(&server).await {
            assert_eq!(result.is_error, Some(false), "{result:?}");
            assert_eq!(body(&result)["data_source"], "local_store");
            let serialized = serde_json::to_string(&result).unwrap();
            for forbidden in [STORE_PATIENT_ID, "k27", RAW_SERIAL, RAW_DEVICE, "7777-"] {
                assert!(
                    !serialized.contains(forbidden),
                    "{forbidden} in {serialized}"
                );
            }
        }
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn sensors_and_status_come_from_the_store() {
        let fixture = local_fixture(&[20, 5], Some(2), Some(10));
        fixture.with_store(|store| store.record_poll("ok", 3, fixture.now).unwrap());
        let (server, reads) = store_server(&fixture.db);

        let sensors = server.get_sensor_info(Parameters(EmptyArgs {})).await;
        let sensors = body(&sensors);
        assert_eq!(sensors["sensor_count"], 1);
        let sensor = &sensors["active_sensors"][0];
        assert_eq!(sensor["serial_number_masked"], "****0001");
        assert_eq!(sensor["device_id_masked"], "****7777");
        assert_eq!(sensor["source_activation_epoch_seconds"], 1_652_400_270);
        assert_eq!(sensor["warmup_minutes"], 60);
        assert_eq!(sensor["product_type_code"], 4);

        let status = server.validate_connection(Parameters(EmptyArgs {})).await;
        let status = body(&status);
        assert_eq!(status["collector_running"], true);
        assert_eq!(status["upstream_contacted"], false);
        assert!(status["collector_heartbeat_age_seconds"].as_i64().unwrap() <= 15);
        assert_eq!(status["last_poll_outcome"], "ok");
        assert!(status["last_poll_at_utc"].is_string());
        assert_eq!(status["stored_reading_count"], 3);
        assert!(status.get("glucose_mg_dl").is_none());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_lagging_collector_reports_its_newest_reading_as_stale() {
        let fixture = local_fixture(&[60, 45, 30], None, Some(10));
        let (server, _) = store_server(&fixture.db);
        let reply = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        let reply = body(&reply);
        assert_eq!(reply["data_source"], "local_store");
        assert_eq!(reply["glucose_mg_dl"], 110.0);
        assert_eq!(reply["freshness"], "stale");
        assert_eq!(reply["cache_hit"], false);
        assert_eq!(reply["source_timestamp"], fixture.ago(30));

        // A fresh newest reading is recent.
        let fixture = local_fixture(&[30, 15], Some(2), Some(10));
        let (server, _) = store_server(&fixture.db);
        let reply = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        assert_eq!(body(&reply)["freshness"], "recent");
    }

    #[tokio::test]
    async fn history_from_the_store_spans_more_than_the_12_hour_graph() {
        let fixture = local_fixture(&every_15_minutes(30), None, Some(10));
        let (server, _) = store_server(&fixture.db);
        let reply = server
            .get_glucose_history(Parameters(HistoryArgs {
                hours: Some(24),
                limit: None,
                before: None,
            }))
            .await;
        let reply = body(&reply);
        assert_eq!(reply["data_source"], "local_store");
        assert_eq!(reply["window_filter_applied"], true);
        assert_eq!(reply["total_readings"], 96);
        assert_eq!(reply["coverage"]["source"], "local_store");
        let span = reply["coverage"]["observed_span_hours"].as_f64().unwrap();
        assert!(span > 23.0, "{span}");
        assert_eq!(reply["readings"][0]["source"], "graph");
    }

    #[tokio::test]
    async fn sufficient_store_coverage_is_a_sample_summary() {
        let fixture = local_fixture(&every_15_minutes(30), Some(2), Some(10));
        let (server, _) = store_server(&fixture.db);
        let stats = server
            .get_glucose_stats(Parameters(StatsArgs { days: Some(1) }))
            .await;
        let stats = body(&stats);
        assert_eq!(stats["data_source"], "local_store");
        assert_eq!(stats["period_status"], "sample_summary");
        assert_eq!(stats["coverage"]["period_span_sufficient"], true);
        assert_eq!(stats["coverage"]["estimated_cadence_minutes"], 15.0);
        assert_eq!(stats["gmi_percent"], serde_json::Value::Null);
        let trends = server
            .get_glucose_trends(Parameters(TrendsArgs {
                period: Some(Period::Daily),
            }))
            .await;
        assert_eq!(body(&trends)["period_status"], "sample_summary");

        // A week requested from a day and a quarter of data stays partial.
        let week = server
            .get_glucose_stats(Parameters(StatsArgs { days: Some(7) }))
            .await;
        assert_eq!(body(&week)["period_status"], "partial");
    }

    #[tokio::test]
    async fn store_stats_give_gmi_for_14_covered_days() {
        let fixture = local_fixture(&every_15_minutes(14 * 24 + 1), None, Some(10));
        let (server, reads) = store_server(&fixture.db);
        let stats = server
            .get_glucose_stats(Parameters(StatsArgs { days: Some(14) }))
            .await;
        let stats = body(&stats);
        assert_eq!(stats["data_source"], "local_store");
        let coverage = stats["time_weighted"]["coverage_percent"].as_f64().unwrap();
        assert!(coverage > 99.0, "{coverage}");
        assert_eq!(stats["time_in_range_percent"], 100.0);
        // Every synthetic reading is 110 mg/dL.
        let gmi = stats["gmi_percent"].as_f64().unwrap();
        assert!((gmi - (3.31 + 0.02392 * 110.0)).abs() < 1e-9, "{gmi}");
        assert_eq!(stats["gmi_unavailable_reason"], serde_json::Value::Null);

        // Thirteen days of the same data is too short a window.
        let stats = server
            .get_glucose_stats(Parameters(StatsArgs { days: Some(13) }))
            .await;
        let stats = body(&stats);
        assert_eq!(stats["gmi_percent"], serde_json::Value::Null);
        assert_eq!(stats["gmi_unavailable_reason"], "window_too_short");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn imported_csv_rows_are_counted_by_stats_and_history() {
        // A collector has run (it set the patient), but stored no readings.
        let fixture = local_fixture(&[], None, Some(10));
        let header = "Device,Serial Number,Device Timestamp,Record Type,\
                      Historic Glucose mg/dL,Scan Glucose mg/dL,Notes";
        let mut lines = vec![
            "Glucose Data,Generated on,synthetic,Generated by,Synthetic Person".to_owned(),
            header.to_owned(),
        ];
        // Every 15 minutes for 24 hours, in device-local time (UTC here).
        lines.extend((0..96).map(|step| {
            let at = fixture.at(5 + 15 * step);
            format!("Synthetic,SYNTH,{},0,150,,", at.format("%m-%d-%Y %I:%M %p"))
        }));
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("export.csv");
        std::fs::write(&file, lines.join("\r\n")).unwrap();
        let options = crate::import_csv::Options {
            file,
            timezone: chrono_tz::UTC,
            date_format: Some(crate::import_csv::DateOrder::Mdy),
            db: Some(fixture.db.clone()),
            dry_run: false,
        };
        let counts = crate::import_csv::import(&options, fixture.now, &mut Vec::new()).unwrap();
        assert_eq!(counts.imported, 96);

        let (server, reads) = store_server(&fixture.db);
        let stats = server
            .get_glucose_stats(Parameters(StatsArgs { days: Some(1) }))
            .await;
        let stats = body(&stats);
        assert_eq!(stats["data_source"], "local_store");
        assert_eq!(stats["sample_count"], 96);
        assert_eq!(stats["window_filter_applied"], true);
        assert_eq!(stats["time_weighted"]["readings_used"], 96);
        assert_eq!(stats["time_weighted"]["readings_without_utc"], 0);
        let coverage = stats["time_weighted"]["coverage_percent"].as_f64().unwrap();
        assert!(coverage > 98.0, "{coverage}");
        assert_eq!(stats["time_in_range_percent"], 100.0);

        let history = server
            .get_glucose_history(Parameters(HistoryArgs {
                hours: Some(24),
                limit: None,
                before: None,
            }))
            .await;
        let history = body(&history);
        assert_eq!(history["total_readings"], 96);
        let point = &history["readings"][0];
        assert_eq!(point["source"], "libreview_csv_historic");
        assert_eq!(point["time_basis"], "csv_local_declared_timezone");
        assert!(point["observed_at_utc"].is_string());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    /// A synthetic CSV historic row at `at`.
    fn csv_row(at: DateTime<Utc>, value: f64) -> crate::store::CsvReading {
        crate::store::CsvReading {
            key: format!("csv:0:{}", at.timestamp()),
            source_timestamp: at.format("%m-%d-%Y %I:%M %p").to_string(),
            observed_at_utc: at,
            value_mg_dl: value,
            origin: Origin::LibreviewCsvHistoric,
        }
    }

    fn import_rows(fixture: &LocalFixture, rows: &[crate::store::CsvReading]) {
        let now = fixture.now;
        fixture.with_store(|store| {
            let key = store
                .collector_status()
                .unwrap()
                .unwrap()
                .patient_key
                .unwrap();
            store.insert_csv_readings(&key, rows, now).unwrap();
        });
    }

    fn directory_contents(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        files.sort();
        files
    }

    #[tokio::test]
    async fn serve_reads_an_older_database_without_migrating_it() {
        let fixture = local_fixture(&[20, 5], None, Some(10));
        // The v2 and v3 schemas are identical; v3 only adds values.
        fixture.with_store(|store| {
            store
                .connection()
                .pragma_update(None, "user_version", 2)
                .unwrap()
        });
        let dir = fixture.db.parent().unwrap().to_path_buf();
        let before = directory_contents(&dir);
        let (server, reads) = store_server(&fixture.db);
        for result in all_data_tools(&server).await {
            assert_eq!(body(&result)["data_source"], "local_store", "{result:?}");
        }
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        // No migration, no writes, no new files.
        assert!(
            directory_contents(&dir) == before,
            "serve changed the database"
        );
        let version: i64 = rusqlite::Connection::open(&fixture.db)
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);

        // A database from a newer build is not read: serve uses the live API.
        fixture.with_store(|store| {
            store
                .connection()
                .pragma_update(None, "user_version", 99)
                .unwrap()
        });
        let (server, reads) = store_server(&fixture.db);
        let reply = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        assert_eq!(body(&reply)["data_source"], "live_api");
        assert_eq!(reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn csv_rows_near_collected_readings_are_hidden_in_either_import_order() {
        for csv_first in [false, true] {
            // Collected readings 20 and 10 minutes ago (110 mg/dL).
            let fixture = local_fixture(&[], None, Some(10));
            let collected = fixture.at(10);
            let csv = [
                // Within 90 s of a collected reading: hidden. One is newer
                // than the collected reading and would otherwise be current.
                csv_row(fixture.at(20) - Duration::seconds(90), 200.0),
                csv_row(collected + Duration::seconds(60), 200.0),
                // 91 s away: kept.
                csv_row(collected - Duration::seconds(91), 150.0),
                // Nothing collected near it: kept.
                csv_row(fixture.at(40), 150.0),
            ];
            let snapshot = Snapshot {
                patient_id: STORE_PATIENT_ID.to_owned(),
                current: synthetic_item(""),
                graph: vec![
                    synthetic_item(&fixture.ago(20)),
                    synthetic_item(&fixture.ago(10)),
                ],
                active_sensors: vec![],
            };
            let collect = |fixture: &LocalFixture| {
                let now = fixture.now;
                fixture.with_store(|store| {
                    store
                        .insert_snapshot(STORE_PATIENT_ID, &snapshot, now)
                        .unwrap()
                });
            };
            if csv_first {
                import_rows(&fixture, &csv);
                collect(&fixture);
            } else {
                collect(&fixture);
                import_rows(&fixture, &csv);
            }
            let (server, _) = store_server(&fixture.db);
            let order = if csv_first {
                "csv first"
            } else {
                "collected first"
            };

            let current = server.get_current_glucose(Parameters(EmptyArgs {})).await;
            assert_eq!(body(&current)["glucose_mg_dl"], 110.0, "{order}");

            let history = server
                .get_glucose_history(Parameters(HistoryArgs {
                    hours: Some(1),
                    limit: None,
                    before: None,
                }))
                .await;
            let history = body(&history);
            assert_eq!(history["total_readings"], 4, "{order}");
            let sources: Vec<_> = history["readings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|point| point["source"].as_str().unwrap().to_owned())
                .collect();
            assert_eq!(
                sources,
                [
                    "libreview_csv_historic",
                    "graph",
                    "libreview_csv_historic",
                    "graph"
                ],
                "{order}"
            );
            assert_eq!(history["coverage"]["actual_points_in_window"], 4, "{order}");

            let stats = server
                .get_glucose_stats(Parameters(StatsArgs { days: Some(1) }))
                .await;
            let stats = body(&stats);
            assert_eq!(stats["sample_count"], 4, "{order}");
            assert_eq!(stats["time_weighted"]["readings_used"], 4, "{order}");

            let trends = server
                .get_glucose_trends(Parameters(TrendsArgs {
                    period: Some(Period::Daily),
                }))
                .await;
            assert_eq!(body(&trends)["sample_count"], 4, "{order}");

            let status = server.validate_connection(Parameters(EmptyArgs {})).await;
            assert_eq!(body(&status)["stored_reading_count"], 4, "{order}");
        }
    }

    #[tokio::test]
    async fn gaps_in_imported_history_are_labeled_by_precedence() {
        // Minutes ago. Graph readings before the export, a 15-minute export
        // from 700 to 400 minutes ago, then graph readings to now.
        let early_graph = [800, 785, 770];
        let late_graph: Vec<i64> = (0..26)
            .map(|step| 5 + 15 * step)
            .filter(|m| !(200 < *m && *m < 245))
            .collect();
        let csv_minutes: Vec<i64> = (0..=20)
            .map(|step| 400 + 15 * step)
            .filter(|m| !(670 < *m && *m < 700))
            .filter(|m| !(490 < *m && *m < 550))
            .filter(|m| !(430 < *m && *m < 460))
            .collect();
        let graph: Vec<i64> = early_graph.into_iter().chain(late_graph).collect();
        let fixture = local_fixture(&graph, None, Some(10));
        let csv: Vec<_> = csv_minutes
            .iter()
            .map(|m| csv_row(fixture.at(*m), 120.0))
            .collect();
        import_rows(&fixture, &csv);
        // Collection began 650 minutes ago; one poll 520 minutes ago.
        let (first_poll, later_poll) = (fixture.at(650), fixture.at(520));
        fixture.with_store(|store| {
            store.record_poll("ok", 40, first_poll).unwrap();
            store.record_poll("ok", 0, later_poll).unwrap();
        });
        let (server, _) = store_server(&fixture.db);
        let reply = server
            .get_glucose_history(Parameters(HistoryArgs {
                hours: Some(14),
                limit: None,
                before: None,
            }))
            .await;
        let gaps: Vec<(f64, String)> = body(&reply)["coverage"]["interior_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|gap| {
                (
                    gap["duration_minutes"].as_f64().unwrap(),
                    gap["cause"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let expected = [
            // 770 to 700: starts before the export, ends before the first poll.
            (70.0, "before_collection"),
            // 700 to 670: inside the export, before the first poll.
            (30.0, "no_data_in_export"),
            // 550 to 490: inside the export, but the collector polled in it.
            (60.0, "no_data_from_api"),
            // 460 to 430: inside the export, after collection began.
            (30.0, "no_data_in_export"),
            // 245 to 200: after the export, no poll.
            (45.0, "collector_not_running"),
        ];
        let expected: Vec<(f64, String)> = expected
            .iter()
            .map(|(minutes, cause)| (*minutes, (*cause).to_owned()))
            .collect();
        assert_eq!(gaps, expected);
    }

    #[tokio::test]
    async fn interior_gaps_do_not_make_a_full_span_partial() {
        // 14 days every 15 minutes, with a 3-hour gap 3 days ago and a
        // 2-hour gap 12 hours ago.
        let minutes: Vec<i64> = every_15_minutes(14 * 24 + 1)
            .into_iter()
            .filter(|m| !(4320 < *m && *m < 4500) && !(720 < *m && *m < 840))
            .collect();
        let fixture = local_fixture(&minutes, None, Some(10));
        let (server, _) = store_server(&fixture.db);
        let stats = server
            .get_glucose_stats(Parameters(StatsArgs { days: Some(14) }))
            .await;
        let stats = body(&stats);
        assert_eq!(stats["period_status"], "sample_summary");
        assert_eq!(stats["coverage"]["interior_gap_count"], 2);
        let coverage = stats["time_weighted"]["coverage_percent"].as_f64().unwrap();
        assert!((95.0..99.0).contains(&coverage), "{coverage}");
        assert!(stats["gmi_percent"].is_number());

        let trends = server
            .get_glucose_trends(Parameters(TrendsArgs {
                period: Some(Period::Daily),
            }))
            .await;
        let trends = body(&trends);
        assert_eq!(trends["period_status"], "sample_summary");
        assert_eq!(trends["coverage"]["interior_gap_count"], 1);
    }

    #[tokio::test]
    async fn data_that_starts_late_is_partial() {
        // 12 days of readings in a 14-day window: the span starts 2 days late.
        let fixture = local_fixture(&every_15_minutes(12 * 24), None, Some(10));
        let (server, _) = store_server(&fixture.db);
        let stats = server
            .get_glucose_stats(Parameters(StatsArgs { days: Some(14) }))
            .await;
        let stats = body(&stats);
        assert_eq!(stats["period_status"], "partial");
        assert_eq!(stats["coverage"]["interior_gap_count"], 0);
        // GMI is gated on the window and coverage only: 12 of 14 days is
        // above 70%, so the estimate is given even though the span is partial.
        let coverage = stats["time_weighted"]["coverage_percent"].as_f64().unwrap();
        assert!((85.0..86.0).contains(&coverage), "{coverage}");
        assert!(stats["gmi_percent"].is_number());

        // Data that stops early is partial too.
        let minutes: Vec<i64> = every_15_minutes(24)
            .into_iter()
            .filter(|m| *m > 30)
            .collect();
        let fixture = local_fixture(&minutes, None, Some(10));
        let (server, _) = store_server(&fixture.db);
        let trends = server
            .get_glucose_trends(Parameters(TrendsArgs {
                period: Some(Period::Daily),
            }))
            .await;
        assert_eq!(body(&trends)["period_status"], "partial");
    }

    #[test]
    fn the_live_path_uses_the_same_span_rule() {
        let now = DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // A full day, with a 2-hour hole in the middle.
        let graph: Vec<GlucoseSample> = (0..=96)
            .filter(|step| !(40..48).contains(step))
            .map(|step| synthetic_item(&(now - Duration::minutes(15 * step)).to_rfc3339()))
            .collect();
        let body = summarize_stats(&graph, 1, 70, 180, now).unwrap();
        assert_eq!(body["period_status"], "sample_summary");
        assert_eq!(body["coverage"]["interior_gap_count"], 1);
        let trends = summarize_trends("daily", 24, &graph, graph_coverage(&graph, 24, now), now);
        assert_eq!(trends["period_status"], "sample_summary");
    }

    const DATA_TOOLS: [&str; 6] = [
        "get_current_glucose",
        "get_glucose_history",
        "get_glucose_stats",
        "get_glucose_trends",
        "get_sensor_info",
        "validate_connection",
    ];

    /// Checks `result`'s structured content against the tool's published
    /// output schema.
    fn assert_matches_schema(tool: &str, result: &CallToolResult) {
        let router = LibreMcp::tool_router();
        let schema = router
            .get(tool)
            .and_then(|tool| tool.output_schema.clone())
            .unwrap_or_else(|| panic!("{tool} has no output schema"));
        let schema = serde_json::Value::Object((*schema).clone());
        assert_eq!(schema["type"], "object", "{tool}");
        let validator = jsonschema::validator_for(&schema).unwrap();
        let content = body(result);
        let errors: Vec<String> = validator
            .iter_errors(content)
            .map(|error| format!("{} at {}", error, error.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{tool}: {errors:?}\n{content}");
    }

    /// The Claude Desktop bundle manifest (MCPB manifest 0.3) runs `serve`,
    /// lists exactly this server's tools, matches the crate version, and
    /// never collects configuration such as credentials.
    #[test]
    fn the_bundle_manifest_matches_the_server() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../packaging/mcpb/manifest.json")).unwrap();
        assert_eq!(manifest["manifest_version"], "0.3");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        for required in ["name", "version", "description", "author", "server"] {
            assert!(manifest.get(required).is_some(), "{required}");
        }
        assert!(manifest["author"]["name"].is_string());
        assert_eq!(manifest["server"]["type"], "binary");
        assert_eq!(
            manifest["server"]["mcp_config"]["args"],
            serde_json::json!(["serve"])
        );
        assert_eq!(
            manifest["server"]["mcp_config"]["env"],
            serde_json::json!({})
        );
        assert!(
            manifest.get("user_config").is_none(),
            "no bundle configuration"
        );
        assert!(!manifest.to_string().contains("user_config"));
        let mut listed: Vec<&str> = manifest["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        let mut served: Vec<String> = LibreMcp::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect();
        listed.sort_unstable();
        served.sort_unstable();
        assert_eq!(listed, served);
    }

    #[test]
    fn every_tool_has_annotations_and_an_output_schema() {
        let tools = LibreMcp::tool_router().list_all();
        assert_eq!(tools.len(), 8);
        for tool in &tools {
            assert!(tool.output_schema.is_some(), "{}", tool.name);
            let hints = tool.annotations.as_ref().unwrap();
            let name = tool.name.as_ref();
            match name {
                "configure_ranges" => {
                    assert_eq!(hints.read_only_hint, Some(false));
                    assert_eq!(hints.destructive_hint, Some(false));
                    assert_eq!(hints.idempotent_hint, Some(true));
                    assert_eq!(hints.open_world_hint, Some(false));
                }
                "configure_credentials" => {
                    assert_eq!(hints.read_only_hint, Some(true));
                    assert_eq!(hints.open_world_hint, Some(false));
                }
                data => {
                    assert!(DATA_TOOLS.contains(&data), "{data}");
                    assert_eq!(hints.read_only_hint, Some(true), "{data}");
                    assert_eq!(hints.idempotent_hint, Some(true), "{data}");
                    assert_eq!(hints.open_world_hint, Some(true), "{data}");
                }
            }
        }
    }

    #[tokio::test]
    async fn every_reply_matches_its_published_schema() {
        let check = |results: Vec<CallToolResult>, expect_error: bool| {
            for (tool, result) in DATA_TOOLS.iter().zip(&results) {
                assert_eq!(result.is_error, Some(expect_error), "{tool}: {result:?}");
                assert_matches_schema(tool, result);
            }
        };

        // Live path, success and error.
        let (mut live, _) = fake_server(Ok(live_snapshot()));
        live.fixed_ranges = Some((70, 180));
        check(all_data_tools(&live).await, false);
        let (mut failing, _) = fake_server(Err(FetchError {
            code: "rate_limited",
            message: Cow::Borrowed("Synthetic throttle."),
            retry_after_seconds: Some(120),
        }));
        failing.fixed_ranges = Some((70, 180));
        check(all_data_tools(&failing).await, true);

        // Store path, success and error.
        let fixture = local_fixture(&every_15_minutes(30), Some(2), Some(10));
        let (server, _) = store_server(&fixture.db);
        check(all_data_tools(&server).await, false);
        fixture.with_store(|store| {
            store
                .connection()
                .execute_batch("DROP TABLE readings; DROP TABLE polls; DROP TABLE sensors;")
                .unwrap()
        });
        let results = all_data_tools(&server).await;
        // validate_connection fails too: it counts readings.
        for (tool, result) in DATA_TOOLS.iter().zip(&results) {
            assert_eq!(
                body(result)["error_code"],
                "local_store_unavailable",
                "{tool}"
            );
        }
        check(results, true);

        // Invalid input and the local-only tools.
        let invalid = [
            (
                "get_glucose_history",
                live.get_glucose_history(Parameters(HistoryArgs {
                    hours: Some(0),
                    limit: None,
                    before: None,
                }))
                .await,
            ),
            (
                "get_glucose_history",
                live.get_glucose_history(Parameters(HistoryArgs {
                    hours: None,
                    limit: Some(5001),
                    before: None,
                }))
                .await,
            ),
            (
                "get_glucose_history",
                live.get_glucose_history(Parameters(HistoryArgs {
                    hours: None,
                    limit: None,
                    before: Some("yesterday".to_owned()),
                }))
                .await,
            ),
            (
                "get_glucose_stats",
                live.get_glucose_stats(Parameters(StatsArgs { days: Some(0) }))
                    .await,
            ),
            (
                "configure_ranges",
                live.configure_ranges(Parameters(RangesArgs {
                    target_low: 0,
                    target_high: 0,
                })),
            ),
        ];
        for (tool, result) in &invalid {
            assert_eq!(body(result)["error_code"], "invalid_input", "{tool}");
            assert_matches_schema(tool, result);
        }
        let credentials = live.configure_credentials(Parameters(EmptyArgs {}));
        assert_eq!(credentials.is_error, Some(false));
        assert_matches_schema("configure_credentials", &credentials);
        // Saving ranges needs the OS keyring; check the reply shape directly.
        let saved = success(RangesReply {
            saved: true,
            target_low: 70,
            target_high: 180,
            unit: "mg/dL",
        });
        assert_matches_schema("configure_ranges", &saved);
    }

    fn point_at(at: Option<DateTime<Utc>>) -> HistoryPoint {
        HistoryPoint {
            glucose_mg_dl: Some(110.0),
            value_quality: ValueQuality::Observed,
            observed_at_utc: at,
            time_basis: TimeBasis::OffsetKnown,
            source_timestamp: None,
            trend: None,
            source: PointSource::Graph,
        }
    }

    #[test]
    fn pages_never_split_one_second() {
        let t = |seconds: i64| DateTime::from_timestamp(1_800_000_000 + seconds, 0);
        let times = |page: &Page| -> Vec<Option<DateTime<Utc>>> {
            page.readings
                .iter()
                .map(|point| point.observed_at_utc)
                .collect()
        };
        let points = || {
            vec![
                point_at(t(0)),
                point_at(t(60)),
                point_at(t(60)),
                point_at(t(120)),
            ]
        };

        // A limit of 2 would cut the pair at 60 s: it waits for the next page.
        let first = page(points(), 2, None, true);
        assert_eq!(times(&first), [t(120)]);
        assert!(first.truncated);
        assert_eq!(first.next_before, t(60));
        let second = page(points(), 2, first.next_before, true);
        assert_eq!(times(&second), [t(60), t(60)]);
        assert_eq!(second.next_before, t(59));
        let last = page(points(), 2, second.next_before, true);
        assert_eq!(times(&last), [t(0)]);
        assert!(!last.truncated);
        assert_eq!(last.next_before, None);

        // A second larger than the limit is returned whole.
        let whole = page(points(), 1, t(60), true);
        assert_eq!(times(&whole), [t(60), t(60)]);
        assert_eq!(whole.next_before, t(59));

        // Unplaceable readings are paged by position only.
        let unplaced = page(
            vec![point_at(None), point_at(None), point_at(None)],
            2,
            t(0),
            false,
        );
        assert_eq!(unplaced.readings.len(), 2);
        assert!(unplaced.truncated);
        assert_eq!(unplaced.next_before, None);
    }

    #[tokio::test]
    async fn paging_walks_a_90_day_store_window_without_overlap_or_gaps() {
        let minutes = every_15_minutes(90 * 24);
        let fixture = local_fixture(&minutes, None, Some(10));
        let (server, _) = store_server(&fixture.db);
        let mut before: Option<String> = None;
        let mut seen: Vec<String> = Vec::new();
        let mut coverage: Option<serde_json::Value> = None;
        let mut pages = 0;
        loop {
            let reply = server
                .get_glucose_history(Parameters(HistoryArgs {
                    hours: Some(2160),
                    limit: Some(1000),
                    before: before.clone(),
                }))
                .await;
            let reply = body(&reply).clone();
            pages += 1;
            assert_eq!(reply["readings_in_window"], minutes.len());
            // Coverage describes the whole window on every page. The edge
            // gaps are measured from the real clock, so they are left out.
            let mut this = reply["coverage"].clone();
            for clock_relative in ["start_gap_minutes", "end_gap_minutes"] {
                this.as_object_mut().unwrap().remove(clock_relative);
            }
            match &coverage {
                None => coverage = Some(this),
                Some(first) => assert_eq!(&this, first),
            }
            let page: Vec<String> = reply["readings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|point| point["observed_at_utc"].as_str().unwrap().to_owned())
                .collect();
            assert!(
                page.windows(2).all(|pair| pair[0] < pair[1]),
                "oldest first"
            );
            if let Some(newest_older) = seen.first() {
                assert!(page.last().unwrap() < newest_older, "pages overlap");
            }
            seen.splice(0..0, page);
            if reply["truncated"] == false {
                assert_eq!(reply["next_before"], serde_json::Value::Null);
                break;
            }
            before = Some(reply["next_before"].as_str().unwrap().to_owned());
        }
        assert_eq!(pages, 9);
        let mut expected: Vec<String> = minutes
            .iter()
            .map(|m| {
                DateTime::parse_from_rfc3339(&fixture.ago(*m))
                    .unwrap()
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true)
            })
            .collect();
        expected.sort();
        assert_eq!(seen, expected);
    }

    #[tokio::test]
    async fn the_current_resource_is_the_current_glucose_reply() {
        let fixture = local_fixture(&[20, 5], Some(2), Some(10));
        let (server, reads) = store_server(&fixture.db);
        let tool = server.get_current_glucose(Parameters(EmptyArgs {})).await;
        let resource = server.read_current_resource().await;
        let ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } = &resource.contents[0]
        else {
            panic!("expected text contents");
        };
        assert_eq!(uri, CURRENT_RESOURCE_URI);
        assert_eq!(mime_type.as_deref(), Some("application/json"));
        let from_resource: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(&from_resource, body(&tool));
        assert_eq!(from_resource["data_source"], "local_store");
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert!(server.get_info().capabilities.resources.is_some());
    }

    #[tokio::test]
    async fn store_gaps_are_labeled_by_what_the_collector_saw() {
        let minutes: Vec<i64> = every_15_minutes(10)
            .into_iter()
            .filter(|m| !(300 < *m && *m < 420) && !(120 < *m && *m < 180))
            .collect();
        let fixture = local_fixture(&minutes, None, Some(10));
        // Collection began before both gaps. The collector polled during the
        // newer gap but got no readings, and recorded nothing in the older.
        let (first_poll, polled_at) = (fixture.at(590), fixture.at(150));
        fixture.with_store(|store| {
            store.record_poll("ok", 40, first_poll).unwrap();
            store.record_poll("ok", 0, polled_at).unwrap();
        });
        let (server, _) = store_server(&fixture.db);
        let reply = server
            .get_glucose_history(Parameters(HistoryArgs {
                hours: Some(10),
                limit: None,
                before: None,
            }))
            .await;
        let gaps = &body(&reply)["coverage"]["interior_gaps"];
        let causes: Vec<_> = gaps
            .as_array()
            .unwrap()
            .iter()
            .map(|gap| gap["cause"].as_str().unwrap())
            .collect();
        assert_eq!(causes, ["collector_not_running", "no_data_from_api"]);

        // The live path never labels gaps.
        let now = Utc::now();
        let graph = [60, 45, 5].map(|m| synthetic_item(&minutes_before(now, m)));
        let coverage = serde_json::to_value(graph_coverage(&graph, 1, now)).unwrap();
        assert!(coverage["interior_gaps"][0].get("cause").is_none());
        assert_eq!(coverage["source"], "librelinkup_graph");
    }

    #[tokio::test]
    async fn gaps_in_backfill_before_the_first_poll_have_an_unknown_cause() {
        // Readings every 15 minutes for 10 hours, with a gap in the API's
        // backfilled graph (about 7 to 5 hours ago) and a real outage after
        // collection began (about 3 to 2 hours ago).
        let minutes: Vec<i64> = every_15_minutes(10)
            .into_iter()
            .filter(|m| !(300 < *m && *m < 420) && !(120 < *m && *m < 180))
            .collect();
        let fixture = local_fixture(&minutes, None, Some(10));
        // The collector first polled 4 hours ago, backfilling the graph,
        // then stopped until 2 hours ago.
        fixture.with_store(|store| {
            store.record_poll("ok", 40, fixture.at(240)).unwrap();
            store.record_poll("ok", 4, fixture.at(110)).unwrap();
        });
        let (server, _) = store_server(&fixture.db);
        let reply = server
            .get_glucose_history(Parameters(HistoryArgs {
                hours: Some(10),
                limit: None,
                before: None,
            }))
            .await;
        let causes: Vec<_> = body(&reply)["coverage"]["interior_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|gap| gap["cause"].as_str().unwrap())
            .collect();
        assert_eq!(causes, ["before_collection", "collector_not_running"]);

        // With no polls recorded at all, every gap's cause is unknown.
        let unpolled = local_fixture(&minutes, None, Some(10));
        let (server, _) = store_server(&unpolled.db);
        let reply = server
            .get_glucose_history(Parameters(HistoryArgs {
                hours: Some(10),
                limit: None,
                before: None,
            }))
            .await;
        let causes: Vec<_> = body(&reply)["coverage"]["interior_gaps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|gap| gap["cause"].as_str().unwrap())
            .collect();
        assert_eq!(causes, ["before_collection", "before_collection"]);
    }

    #[tokio::test]
    async fn live_replies_say_live_api() {
        let (mut server, _) = fake_server(Ok(live_snapshot()));
        server.fixed_ranges = Some((70, 180));
        for result in all_data_tools(&server).await {
            assert_eq!(body(&result)["data_source"], "live_api");
        }
    }

    #[test]
    fn instructions_explain_the_data_source() {
        assert!(SERVER_INSTRUCTIONS.contains("data_source"));
        assert!(SERVER_INSTRUCTIONS.contains("local_store"));
        assert!(SERVER_INSTRUCTIONS.contains("only as fresh as the collector"));
    }
}
