use crate::{
    FetchError, fetch_raw, load_settings, save_settings,
    time::{STALE_AFTER_MINUTES, TimeAssessment, TimeBasis, assess_time, assess_time_at},
};
use chrono::{DateTime, Duration, Utc};
use libre_link_up_api_client::GlucoseItem;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub(crate) struct LibreMcp;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HistoryArgs {
    /// Requested hours, from 1 through 720. The API may return a shorter graph.
    hours: Option<u16>,
}

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

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ValueQuality {
    Observed,
    Missing,
}

#[derive(Serialize)]
struct Reading {
    glucose_mg_dl: Option<f64>,
    value_quality: ValueQuality,
    #[serde(flatten)]
    time: TimeAssessment,
    trend: Option<&'static str>,
    cache_hit: bool,
}

#[derive(Serialize)]
struct GraphGap {
    from_utc: DateTime<Utc>,
    to_utc: DateTime<Utc>,
    duration_minutes: f64,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
struct RateSummary {
    pair_count: usize,
    minimum_mg_dl_per_minute: Option<f64>,
    maximum_mg_dl_per_minute: Option<f64>,
    average_mg_dl_per_minute: Option<f64>,
    maximum_pair_gap_minutes: u8,
}

fn observed_rates(items: &[&GlucoseItem], now: DateTime<Utc>) -> RateSummary {
    let mut points: Vec<_> = items
        .iter()
        .filter_map(|item| {
            let time = assess_time(Some(&item.timestamp), Some(&item.factory_timestamp), now)
                .observed_at_utc?;
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

fn graph_coverage(items: &[GlucoseItem], hours: i64, now: DateTime<Utc>) -> GraphCoverage {
    let cutoff = now - Duration::hours(hours);
    let assessed: Vec<_> = items
        .iter()
        .map(|item| assess_time(Some(&item.timestamp), Some(&item.factory_timestamp), now))
        .collect();
    let usable = !assessed.is_empty()
        && assessed
            .iter()
            .all(|time| time.observed_at_utc.is_some_and(|observed| observed <= now));
    if !usable {
        return GraphCoverage {
            source: "librelinkup_graph",
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
    let cadence_seconds = (intervals.len() >= 2).then(|| intervals[intervals.len() / 2]);
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
                    });
                }
            }
        }
    }
    let start_gap_seconds = first.signed_duration_since(cutoff).num_seconds().max(0);
    let end_gap_seconds = now.signed_duration_since(last).num_seconds().max(0);
    let period_span_sufficient = cadence_seconds.is_some_and(|cadence| {
        first <= cutoff + Duration::seconds(cadence)
            && end_gap_seconds <= 15 * 60
            && interior_gap_count == 0
    });
    GraphCoverage {
        source: "librelinkup_graph",
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

/// Sensor serial numbers and device IDs identify a person's hardware. Only the
/// last four characters go to the model, which is enough to tell sensors apart.
fn masked_identifier(raw: &str) -> Option<String> {
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

const SERVER_INSTRUCTIONS: &str = "Unofficial, read-only access to LibreLinkUp follower data. \
Glucose values are mg/dL. Only get_current_glucose reports freshness: treat `stale` or `unknown` \
as not real-time and say so. `time_basis: factory_utc_inferred` means the UTC time is inferred \
from the API's factory clock. The LibreLinkUp graph covers about 12 hours, so stats and trends \
for a day or more are labeled `partial`; report the observed span from `coverage` rather than \
the requested period. Results are descriptive only: do not diagnose, recommend treatment, or \
infer meals. Credentials are never accepted through MCP; direct users to the local `setup` command.";

fn sensor_activation_time(epoch_seconds: i32, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    // Sensor a is an epoch-seconds field in observed LibreLinkUp payloads.
    // Reject a days-since-start value or a clock that has not happened yet.
    const FIRST_PLAUSIBLE_ACTIVATION: i64 = 1_388_534_400; // 2014-01-01 UTC
    let seconds = i64::from(epoch_seconds);
    (seconds >= FIRST_PLAUSIBLE_ACTIVATION)
        .then(|| DateTime::<Utc>::from_timestamp(seconds, 0))
        .flatten()
        .filter(|time| *time <= now)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PointSource {
    Graph,
    CurrentMeasurement,
}

/// One historical sample. Freshness is deliberately absent: it describes how
/// current the latest reading is, and every past sample would read as stale.
#[derive(Serialize)]
struct HistoryPoint {
    glucose_mg_dl: Option<f64>,
    value_quality: ValueQuality,
    observed_at_utc: Option<DateTime<Utc>>,
    time_basis: TimeBasis,
    source_timestamp: Option<String>,
    trend: Option<&'static str>,
    source: PointSource,
}

fn history_point(item: &GlucoseItem, source: PointSource, now: DateTime<Utc>) -> HistoryPoint {
    let (glucose_mg_dl, value_quality) = value(item.value_in_mg_per_dl);
    let time = assess_time(Some(&item.timestamp), Some(&item.factory_timestamp), now);
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
fn points_with_current(graph: &[GlucoseItem], current: &GlucoseItem) -> (Vec<GlucoseItem>, bool) {
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
    items: &[GlucoseItem],
    hours: i64,
    now: DateTime<Utc>,
) -> (Vec<&GlucoseItem>, bool) {
    let assessed: Vec<_> = items
        .iter()
        .map(|item| {
            (
                item,
                assess_time(Some(&item.timestamp), Some(&item.factory_timestamp), now),
            )
        })
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

fn failure(message: &str) -> CallToolResult {
    failure_code("request_failed", message)
}

fn failure_code(code: &str, message: &str) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "error_code": code,
        "message": message,
    }))
}

fn failure_fetch(error: FetchError) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "error_code": error.code,
        "message": error.message,
        "retry_after_seconds": error.retry_after_seconds,
    }))
}

/// Descriptive statistics over the returned graph. A graph shorter than the
/// requested window still yields statistics, labeled `partial`, because the
/// LibreLinkUp graph covers roughly 12 hours and no request of a day or more
/// can ever be fully covered by it.
fn summarize_stats(
    graph: &[GlucoseItem],
    days: u8,
    target_low: u16,
    target_high: u16,
    now: DateTime<Utc>,
) -> Result<serde_json::Value, CallToolResult> {
    let hours = i64::from(days) * 24;
    let coverage = graph_coverage(graph, hours, now);
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
    Ok(serde_json::json!({
        "requested_days": days,
        "period_status": period_status,
        "window_verified": false,
        "window_filter_applied": window_filter_applied,
        "coverage": coverage,
        "sample_count": count,
        "average_glucose_mg_dl": average,
        "standard_deviation_mg_dl": standard_deviation,
        "coefficient_of_variation_percent": standard_deviation / average * 100.0,
        "sample_percentages": {
            "below_range": below as f64 / count as f64 * 100.0,
            "in_range": in_range as f64 / count as f64 * 100.0,
            "above_range": above as f64 / count as f64 * 100.0,
        },
        "target_range_mg_dl": { "low": target_low, "high": target_high },
        "time_in_range_percent": null,
        "gmi_percent": null,
        "reason": if period_status == "partial" {
            "These statistics describe only the returned graph, which covers less than the requested period; see coverage for the observed span. They are not a summary of the requested days. Time-weighted range and GMI are unavailable."
        } else {
            "These statistics describe selected graph samples only. Time-weighted range and a valid GMI window are unverified."
        },
    }))
}

fn invalid_window(value: u32, max: u32) -> bool {
    value == 0 || value > max
}

#[tool_router]
impl LibreMcp {
    #[tool(
        description = "Get the latest available glucose reading with its source timestamp and freshness. An unknown or stale reading is not real-time data."
    )]
    async fn get_current_glucose(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> CallToolResult {
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let item = &raw.connection.glucose_measurement;
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
            trend: trend(Some(item.trend_arrow)),
            cache_hit: raw.cache_hit,
        })
    }

    #[tool(
        description = "Return glucose readings within the requested hours: the graph plus the current measurement, which the graph lags by several minutes. Each point carries its time and source; freshness applies only to get_current_glucose."
    )]
    async fn get_glucose_history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> CallToolResult {
        let hours = args.hours.unwrap_or(24);
        if invalid_window(u32::from(hours), 720) {
            return failure_code("invalid_input", "hours must be between 1 and 720.");
        }
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let now = Utc::now();
        let current = &raw.connection.glucose_item;
        let (points, current_appended) = points_with_current(&raw.graph_data, current);
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
        success(serde_json::json!({
            "requested_hours": hours,
            "window_verified": false,
            "window_filter_applied": window_filter_applied,
            "cache_hit": raw.cache_hit,
            "retrieved_at_utc": raw.retrieved_at_utc,
            "current_measurement_appended": current_appended,
            "coverage": coverage,
            "reason": if window_filter_applied {
                "Readings were filtered by offset-aware or inferred factory UTC timestamps, but API graph coverage may be shorter than requested."
            } else {
                "The API graph may be shorter than requested and some timestamps lack a usable time basis. All returned graph points are shown."
            },
            "total_readings": readings.len(),
            "readings": readings,
        }))
    }

    #[tool(
        description = "Calculate descriptive statistics for graph points. The LibreLinkUp graph covers about 12 hours, so longer requests return statistics labeled partial with the observed span in coverage."
    )]
    async fn get_glucose_stats(&self, Parameters(args): Parameters<StatsArgs>) -> CallToolResult {
        let days = args.days.unwrap_or(7);
        if invalid_window(u32::from(days), 90) {
            return failure_code("invalid_input", "days must be between 1 and 90.");
        }
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let settings = match load_settings() {
            Ok(settings) => settings,
            Err(message) => return failure(message),
        };
        let (points, _) = points_with_current(&raw.graph_data, &raw.connection.glucose_item);
        match summarize_stats(
            &points,
            days,
            settings.target_low,
            settings.target_high,
            Utc::now(),
        ) {
            Ok(mut body) => {
                body["cache_hit"] = serde_json::Value::Bool(raw.cache_hit);
                success(body)
            }
            Err(error) => error,
        }
    }

    #[tool(
        description = "Summarize observed graph values and adjacent-point rates. Partial periods are labeled; no meal, diagnosis, or treatment inference is made."
    )]
    async fn get_glucose_trends(&self, Parameters(args): Parameters<TrendsArgs>) -> CallToolResult {
        let (period, hours) = match args.period.unwrap_or(Period::Weekly) {
            Period::Daily => ("daily", 24),
            Period::Weekly => ("weekly", 7 * 24),
            Period::Monthly => ("monthly", 30 * 24),
        };
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let now = Utc::now();
        let (points, _) = points_with_current(&raw.graph_data, &raw.connection.glucose_item);
        let coverage = graph_coverage(&points, hours, now);
        let (items, window_filter_applied) = select_graph_window(&points, hours, now);
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
        success(serde_json::json!({
            "requested_period": period,
            "window_verified": false,
            "window_filter_applied": window_filter_applied,
            "cache_hit": raw.cache_hit,
            "coverage": coverage,
            "period_status": if coverage.period_span_sufficient { "sample_summary" } else { "partial" },
            "sample_count": values.len(),
            "observed_min_mg_dl": min,
            "observed_max_mg_dl": max,
            "trend_arrow_counts": {
                "falling": falling, "stable": stable, "rising": rising, "unknown": unknown
            },
            "adjacent_point_rates": rates,
            "dawn_phenomenon": null,
            "meal_response_average": null,
            "overnight_stability": null,
            "reason": "Rates are descriptive changes between returned graph points, not source trend arrows or treatment guidance. The graph does not contain meal and sleep context.",
        }))
    }

    #[tool(
        description = "Return source-provided sensor identifiers, activation epoch, warmup minutes, and product type code. Unknown details remain null."
    )]
    async fn get_sensor_info(&self, Parameters(_args): Parameters<EmptyArgs>) -> CallToolResult {
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        let sensors: Vec<_> = raw
            .active_sensors
            .iter()
            .map(|active| {
                let activation_time = sensor_activation_time(active.sensor.a, Utc::now());
                serde_json::json!({
                    "device_id_masked": masked_identifier(&active.sensor.device_id),
                    "serial_number_masked": masked_identifier(&active.sensor.sn),
                    "source_activation_epoch_seconds": activation_time.map(|_| active.sensor.a),
                    "activation_time": activation_time,
                    "activation_time_basis": activation_time.map(|_| "source_epoch_seconds_interpreted"),
                    "warmup_minutes": (active.sensor.w >= 0).then_some(active.sensor.w),
                    "product_type_code": (active.sensor.pt >= 0).then_some(active.sensor.pt),
                    "device_type": null,
                    "remaining_life": null,
                })
            })
            .collect();
        success(serde_json::json!({
            "sensor_count": sensors.len(),
            "active_sensors": sensors,
            "cache_hit": raw.cache_hit,
        }))
    }

    #[tool(
        description = "Explain how to set LibreLinkUp credentials in the local terminal. This tool accepts no passwords, tokens, or other secrets."
    )]
    fn configure_credentials(&self, Parameters(_args): Parameters<EmptyArgs>) -> CallToolResult {
        success(serde_json::json!({
            "credential_input_supported": false,
            "setup_command": "librelink-mcp-rs setup",
            "message": "Run `cargo run -- setup` in the project directory, or `librelink-mcp-rs setup` if installed. Credentials cannot be entered through MCP."
        }))
    }

    #[tool(
        description = "Set local target range bounds in mg/dL for descriptive sample summaries."
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
        success(serde_json::json!({
            "saved": true,
            "target_low": args.target_low,
            "target_high": args.target_high,
            "unit": "mg/dL",
        }))
    }

    #[tool(
        description = "Test LibreLinkUp access and report source-data availability without exposing credentials or glucose values."
    )]
    async fn validate_connection(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> CallToolResult {
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(error) => return failure_fetch(error),
        };
        success(serde_json::json!({
            "connected": true,
            "graph_points_returned": raw.graph_data.len(),
            "active_sensors_returned": raw.active_sensors.len(),
            "reading_time": assess_time_at(
                Some(&raw.connection.glucose_measurement.timestamp),
                Some(&raw.connection.glucose_measurement.factory_timestamp),
                raw.retrieved_at_utc,
                Utc::now(),
            ),
            "cache_hit": raw.cache_hit,
            "stale_threshold_minutes": STALE_AFTER_MINUTES,
        }))
    }
}

#[tool_handler]
impl ServerHandler for LibreMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                    .with_title("LibreLinkUp MCP (Rust)"),
            )
            .with_instructions(SERVER_INSTRUCTIONS)
    }
}

pub(crate) async fn serve() -> Result<(), &'static str> {
    let service = LibreMcp
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
        let info = LibreMcp.get_info();
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

    fn synthetic_item(timestamp: &str) -> GlucoseItem {
        GlucoseItem {
            factory_timestamp: timestamp.to_owned(),
            timestamp: timestamp.to_owned(),
            item_type: 0,
            value_in_mg_per_dl: 110.0,
            trend_arrow: Some(3),
            trend_message: None,
            measurement_color: 0,
            glucose_units: 0,
            value: 110.0,
            is_high: false,
            is_low: false,
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
        assert!(!coverage.period_span_sufficient);
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
        let mut graph: Vec<GlucoseItem> = (0..=48)
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
}
