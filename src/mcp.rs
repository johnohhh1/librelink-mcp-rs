use crate::{
    fetch_raw, load_settings, save_settings,
    time::{STALE_AFTER_MINUTES, TimeAssessment, assess_time},
};
use chrono::{DateTime, Duration, Utc};
use libre_link_up_api_client::GlucoseItem;
use rmcp::{
    ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
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
}

fn nonempty(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
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

fn history_reading(item: &GlucoseItem, now: DateTime<Utc>) -> Reading {
    let (glucose_mg_dl, value_quality) = value(item.value_in_mg_per_dl);
    Reading {
        glucose_mg_dl,
        value_quality,
        time: assess_time(Some(&item.timestamp), Some(&item.factory_timestamp), now),
        trend: trend(item.trend_arrow),
    }
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
    match serde_json::to_string(&body) {
        Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
        Err(_) => failure("Unable to encode tool result."),
    }
}

fn failure(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.to_owned())])
}

fn invalid_window(value: u32, max: u32) -> bool {
    value == 0 || value > max
}

#[tool_router(server_handler)]
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
            Err(message) => return failure(message),
        };
        let item = &raw.connection.glucose_measurement;
        let (glucose_mg_dl, value_quality) = value(item.value_in_mg_per_dl);
        success(Reading {
            glucose_mg_dl,
            value_quality,
            time: assess_time(
                Some(&item.timestamp),
                Some(&item.factory_timestamp),
                Utc::now(),
            ),
            trend: trend(Some(item.trend_arrow)),
        })
    }

    #[tool(
        description = "Return graph readings within requested hours only when each reading has an offset-aware timestamp. Otherwise show the returned graph and mark the window unverified."
    )]
    async fn get_glucose_history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> CallToolResult {
        let hours = args.hours.unwrap_or(24);
        if invalid_window(u32::from(hours), 720) {
            return failure("hours must be between 1 and 720.");
        }
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(message) => return failure(message),
        };
        let now = Utc::now();
        let (items, window_filter_applied) =
            select_graph_window(&raw.graph_data, i64::from(hours), now);
        let readings: Vec<Reading> = items
            .into_iter()
            .map(|item| history_reading(item, now))
            .collect();
        success(serde_json::json!({
            "requested_hours": hours,
            "window_verified": false,
            "window_filter_applied": window_filter_applied,
            "reason": if window_filter_applied {
                "Readings were filtered by offset-aware timestamps, but API graph coverage may be shorter than requested."
            } else {
                "The API graph may be shorter than requested and its timestamps have no trusted offset. All returned graph points are shown."
            },
            "total_readings": readings.len(),
            "readings": readings,
        }))
    }

    #[tool(
        description = "Calculate descriptive statistics for returned graph points. Filter requested days only when all timestamps have trusted offsets; coverage and true time in range remain unverified."
    )]
    async fn get_glucose_stats(&self, Parameters(args): Parameters<StatsArgs>) -> CallToolResult {
        let days = args.days.unwrap_or(7);
        if invalid_window(u32::from(days), 90) {
            return failure("days must be between 1 and 90.");
        }
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(message) => return failure(message),
        };
        let settings = match load_settings() {
            Ok(settings) => settings,
            Err(message) => return failure(message),
        };
        let (items, window_filter_applied) =
            select_graph_window(&raw.graph_data, i64::from(days) * 24, Utc::now());
        let values: Vec<f64> = items
            .iter()
            .map(|item| item.value_in_mg_per_dl)
            .filter(|value| value.is_finite() && *value > 0.0)
            .collect();
        if values.is_empty() {
            return failure("No valid historical glucose values were returned.");
        }
        let count = values.len();
        let average = values.iter().sum::<f64>() / count as f64;
        let variance = values.iter().map(|v| (v - average).powi(2)).sum::<f64>() / count as f64;
        let standard_deviation = variance.sqrt();
        let low = f64::from(settings.target_low);
        let high = f64::from(settings.target_high);
        let below = values.iter().filter(|v| **v < low).count();
        let above = values.iter().filter(|v| **v > high).count();
        let in_range = count - below - above;
        success(serde_json::json!({
            "requested_days": days,
            "window_verified": false,
            "window_filter_applied": window_filter_applied,
            "sample_count": count,
            "average_glucose_mg_dl": average,
            "standard_deviation_mg_dl": standard_deviation,
            "coefficient_of_variation_percent": standard_deviation / average * 100.0,
            "sample_percentages": {
                "below_range": below as f64 / count as f64 * 100.0,
                "in_range": in_range as f64 / count as f64 * 100.0,
                "above_range": above as f64 / count as f64 * 100.0,
            },
            "target_range_mg_dl": { "low": settings.target_low, "high": settings.target_high },
            "time_in_range_percent": null,
            "gmi_percent": null,
            "reason": "These statistics describe selected graph samples only. Full time coverage, time-weighted range, and a valid GMI window are unverified.",
        }))
    }

    #[tool(
        description = "Summarize observed graph values and trend arrows without inferring meals, diagnoses, or treatment changes."
    )]
    async fn get_glucose_trends(&self, Parameters(args): Parameters<TrendsArgs>) -> CallToolResult {
        let (period, hours) = match args.period.unwrap_or(Period::Weekly) {
            Period::Daily => ("daily", 24),
            Period::Weekly => ("weekly", 7 * 24),
            Period::Monthly => ("monthly", 30 * 24),
        };
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(message) => return failure(message),
        };
        let (items, window_filter_applied) =
            select_graph_window(&raw.graph_data, hours, Utc::now());
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
            "sample_count": values.len(),
            "observed_min_mg_dl": min,
            "observed_max_mg_dl": max,
            "trend_arrow_counts": {
                "falling": falling, "stable": stable, "rising": rising, "unknown": unknown
            },
            "dawn_phenomenon": null,
            "meal_response_average": null,
            "overnight_stability": null,
            "reason": "The graph does not establish the requested period or contain meal and sleep context.",
        }))
    }

    #[tool(
        description = "Return source-provided sensor details only; unknown type, activation date, and remaining life are omitted."
    )]
    async fn get_sensor_info(&self, Parameters(_args): Parameters<EmptyArgs>) -> CallToolResult {
        let raw = match fetch_raw().await {
            Ok(raw) => raw,
            Err(message) => return failure(message),
        };
        let sensors: Vec<_> = raw
            .active_sensors
            .iter()
            .map(|active| {
                serde_json::json!({
                    "device_id": nonempty(&active.sensor.device_id),
                    "serial_number": nonempty(&active.sensor.sn),
                    "source_reported_age_days": (active.sensor.a >= 0).then_some(active.sensor.a),
                    "device_type": null,
                    "activation_time": null,
                    "remaining_life": null,
                })
            })
            .collect();
        success(serde_json::json!({
            "sensor_count": sensors.len(),
            "active_sensors": sensors,
        }))
    }

    #[tool(
        description = "Explain how to set LibreLinkUp credentials in the local terminal. This tool accepts no passwords, tokens, or other secrets."
    )]
    fn configure_credentials(&self, Parameters(_args): Parameters<EmptyArgs>) -> String {
        "Run `cargo run -- setup` in the project directory, or `librelink-mcp-rs setup` if installed. Credentials cannot be entered through MCP.".to_owned()
    }

    #[tool(
        description = "Set local target range bounds in mg/dL for descriptive sample summaries."
    )]
    fn configure_ranges(&self, Parameters(args): Parameters<RangesArgs>) -> CallToolResult {
        if args.target_low == 0 || args.target_low >= args.target_high || args.target_high > 1000 {
            return failure("Require 0 < target_low < target_high <= 1000 mg/dL.");
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
            Err(message) => return failure(message),
        };
        success(serde_json::json!({
            "connected": true,
            "graph_points_returned": raw.graph_data.len(),
            "active_sensors_returned": raw.active_sensors.len(),
            "reading_time": assess_time(
                Some(&raw.connection.glucose_measurement.timestamp),
                Some(&raw.connection.glucose_measurement.factory_timestamp),
                Utc::now(),
            ),
            "stale_threshold_minutes": STALE_AFTER_MINUTES,
        }))
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
    fn missing_values_and_trends_are_not_invented() {
        assert!(value(0.0).0.is_none());
        assert!(value(f64::NAN).0.is_none());
        assert_eq!(trend(None), None);
        assert_eq!(trend(Some(42)), None);
        assert_eq!(trend(Some(3)), Some("stable"));
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
        let graph = [
            synthetic_item("2026-11-05T18:00:00Z"),
            synthetic_item("11/05/2026 03:30:00 PM"),
        ];
        let (selected, applied) = select_graph_window(&graph, 1, now);
        assert!(!applied);
        assert_eq!(selected.len(), graph.len());
    }
}
