//! The only module that uses `libre_link_up_api_client`. It is replaced by an
//! owned client in v0.2; see docs/adr/0001-owned-api-client.md.

use crate::fetch::FetchError;
use crate::llu_client::unique_match;
use crate::source::{DataSource, GlucoseSample, ReadFuture, SensorRecord, Snapshot};
use crate::{load_credentials, load_settings, valid_api_version};
use libre_link_up_api_client::models::{GlucoseMeasurement, Sensor};
use libre_link_up_api_client::{
    ClientConfig, ConnectionIdentifier, GlucoseItem, LibreLinkUpClient, LibreLinkUpError,
    ReadRawResponse, Region,
};
use std::sync::Arc;
use tokio::sync::OnceCell;

fn parse_region(input: &str) -> Option<Region> {
    match input.trim().to_ascii_lowercase().as_str() {
        "global" | "" => Some(Region::Global),
        "ae" => Some(Region::AE),
        "ap" => Some(Region::AP),
        "au" => Some(Region::AU),
        "ca" => Some(Region::CA),
        "de" => Some(Region::DE),
        "eu" => Some(Region::EU),
        "eu2" => Some(Region::EU2),
        "fr" => Some(Region::FR),
        "jp" => Some(Region::JP),
        "us" => Some(Region::US),
        "la" => Some(Region::LA),
        "ru" => Some(Region::RU),
        "cn" => Some(Region::CN),
        _ => None,
    }
}

/// The stored spelling of a user-entered region, or `None` if unknown.
pub(crate) fn canonical_region(input: &str) -> Option<String> {
    parse_region(input).map(|region| region.as_str().to_owned())
}

fn http_status_from_invalid(message: &str) -> Option<u16> {
    let suffix = message
        .split_once("failed - HTTP ")
        .map(|(_, suffix)| suffix)?;
    let digits = suffix.get(..3)?;
    (digits.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(suffix.as_bytes().get(3), Some(b' ' | b':')))
    .then(|| digits.parse().ok())
    .flatten()
}

fn classify_fetch_error(error: LibreLinkUpError) -> FetchError {
    let (code, message, retry_after_seconds) = match error {
        LibreLinkUpError::AccountLocked(seconds) => (
            "rate_limited",
            "LibreLinkUp account is temporarily locked; retry later.",
            Some(u64::try_from(seconds).unwrap_or(60).clamp(60, 900)),
        ),
        LibreLinkUpError::InvalidResponse(body)
            if matches!(http_status_from_invalid(&body), Some(429 | 430)) =>
        {
            (
                "rate_limited",
                "LibreLinkUp is rate limiting requests; retry later.",
                Some(60),
            )
        }
        LibreLinkUpError::InvalidResponse(body) if http_status_from_invalid(&body) == Some(401) => {
            (
                "session_expired",
                "LibreLinkUp rejected the current session; retry later.",
                Some(60),
            )
        }
        LibreLinkUpError::Http(error) if error.is_timeout() => (
            "timeout",
            "LibreLinkUp request timed out; retry later.",
            Some(60),
        ),
        LibreLinkUpError::BadCredentials => (
            "authentication_failed",
            "LibreLinkUp rejected the account credentials. No further logins will be attempted; run `setup` locally, then restart the server.",
            None,
        ),
        LibreLinkUpError::AdditionalActionRequired(_) => (
            "account_action_required",
            "LibreLinkUp requires an action in its app before API access.",
            Some(60),
        ),
        LibreLinkUpError::NoConnections => (
            "no_connection",
            "Account authenticated but follows no patients in LibreLinkUp.",
            Some(60),
        ),
        LibreLinkUpError::ConnectionNotFound(_) | LibreLinkUpError::ConnectionFunctionFailed => (
            "patient_selection_required",
            "No unique followed person was selected. Run `select-patient` locally.",
            Some(60),
        ),
        _ => (
            "upstream_unavailable",
            "LibreLinkUp request failed; retry later.",
            Some(60),
        ),
    };
    FetchError {
        code,
        message: message.into(),
        retry_after_seconds,
    }
}

fn client_error(error: LibreLinkUpError) -> &'static str {
    match error {
        LibreLinkUpError::BadCredentials => "LibreLinkUp rejected the account credentials.",
        LibreLinkUpError::AccountLocked(_) => "LibreLinkUp account is temporarily locked.",
        LibreLinkUpError::AdditionalActionRequired(_) => {
            "LibreLinkUp requires an action in its app before API access."
        }
        LibreLinkUpError::NoConnections => {
            "Account authenticated but follows no patients in LibreLinkUp."
        }
        LibreLinkUpError::ConnectionNotFound(_) => "The selected patient was not found.",
        LibreLinkUpError::ConnectionFunctionFailed => {
            "No unique followed person was selected. Run `select-patient` locally."
        }
        _ => "LibreLinkUp request failed; details were withheld to protect account data.",
    }
}

fn build_client() -> Result<LibreLinkUpClient, &'static str> {
    let credentials = load_credentials()?;
    let settings = load_settings()?;
    let region = parse_region(&credentials.region).ok_or("Stored region is invalid.")?;
    if !valid_api_version(&settings.api_version) {
        return Err("Stored API version is invalid. Run `set-api-version` locally.");
    }
    let api_version = settings.api_version.clone();
    let selected_name = settings.patient_name.map(|name| name.to_lowercase());
    let identifier = ConnectionIdentifier::ByFunction(Arc::new(move |connections| {
        unique_match(connections, |connection| {
            selected_name.as_ref().is_none_or(|name| {
                format!("{} {}", connection.first_name, connection.last_name).to_lowercase()
                    == *name
            })
        })
        .map(|selected| selected.patient_id.clone())
    }));
    let config = ClientConfig::new(credentials.email.clone(), credentials.password.clone())
        .with_connection_identifier(identifier)
        .with_api_version(api_version)
        .with_region(region);
    LibreLinkUpClient::new(config).map_err(client_error)
}

fn sample_from_item(item: &GlucoseItem) -> GlucoseSample {
    GlucoseSample {
        timestamp: item.timestamp.clone(),
        factory_timestamp: item.factory_timestamp.clone(),
        value_in_mg_per_dl: item.value_in_mg_per_dl,
        trend_arrow: item.trend_arrow,
    }
}

fn sample_from_measurement(measurement: &GlucoseMeasurement) -> GlucoseSample {
    GlucoseSample {
        timestamp: measurement.timestamp.clone(),
        factory_timestamp: measurement.factory_timestamp.clone(),
        value_in_mg_per_dl: measurement.value_in_mg_per_dl,
        trend_arrow: Some(measurement.trend_arrow),
    }
}

fn sensor_record(sensor: &Sensor) -> SensorRecord {
    SensorRecord {
        device_id: sensor.device_id.clone(),
        serial_number: sensor.sn.clone(),
        activation_epoch_seconds: sensor.a,
        warmup_minutes: sensor.w,
        product_type_code: sensor.pt,
    }
}

/// The current reading is `glucose_measurement`. The crate also exposes
/// `glucose_item`; it is deliberately unused so every tool sees one reading.
fn snapshot_from_raw(raw: &ReadRawResponse) -> Snapshot {
    Snapshot {
        current: sample_from_measurement(&raw.connection.glucose_measurement),
        graph: raw.graph_data.iter().map(sample_from_item).collect(),
        active_sensors: raw
            .active_sensors
            .iter()
            .map(|active| sensor_record(&active.sensor))
            .collect(),
    }
}

/// `DataSource` backed by the borrowed crate. The client is built on first
/// use, so a missing credential is reported per call rather than at startup.
pub(crate) struct BorrowedClientSource {
    client: OnceCell<LibreLinkUpClient>,
}

impl BorrowedClientSource {
    pub(crate) fn new() -> Self {
        Self {
            client: OnceCell::new(),
        }
    }
}

impl DataSource for BorrowedClientSource {
    fn read(&self) -> ReadFuture<'_> {
        Box::pin(async move {
            let client = self
                .client
                .get_or_try_init(|| async { build_client() })
                .await
                .map_err(|message| FetchError {
                    code: "configuration_error",
                    message: message.into(),
                    retry_after_seconds: None,
                })?;
            // read_raw preserves the source timestamp. The client's processed
            // read() replaces an unparseable timestamp with Utc::now().
            let raw = client.read_raw().await.map_err(classify_fetch_error)?;
            Ok(snapshot_from_raw(&raw))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use crate::fetch::{Cooldown, cooldown_active, cooldown_until, latches_until_restart};
    use std::time::{Duration as StdDuration, Instant};

    #[test]
    fn accepts_only_known_regions() {
        assert_eq!(parse_region("EU2"), Some(Region::EU2));
        assert_eq!(parse_region(""), Some(Region::Global));
        assert_eq!(parse_region("unknown"), None);
        assert!(valid_api_version("4.16.0"));
        assert!(!valid_api_version("4.16.0\r\npassword:secret"));
        assert!(!valid_api_version("latest"));
        let old_settings: Settings =
            serde_json::from_str(r#"{"target_low":70,"target_high":180,"patient_name":null}"#)
                .unwrap();
        assert_eq!(old_settings.api_version, "4.16.0");
    }

    #[test]
    fn parses_only_status_from_client_error_prefix() {
        assert_eq!(
            http_status_from_invalid(
                "request to '/llu/connections' failed - HTTP 429 Too Many Requests: private body"
            ),
            Some(429)
        );
        assert_eq!(
            http_status_from_invalid("Login failed - HTTP 430: private body"),
            Some(430)
        );
        assert_eq!(http_status_from_invalid("private body says HTTP 429"), None);
        assert_eq!(
            http_status_from_invalid("Login failed - HTTP xxx: body"),
            None
        );
    }

    #[test]
    fn lockouts_and_throttling_map_to_safe_errors() {
        let locked = classify_fetch_error(LibreLinkUpError::AccountLocked(125));
        assert_eq!(locked.code, "rate_limited");
        assert_eq!(locked.retry_after_seconds, Some(125));
        let throttled = classify_fetch_error(LibreLinkUpError::InvalidResponse(
            "request to '/llu/connections' failed - HTTP 429: synthetic body".to_owned(),
        ));
        assert_eq!(throttled.code, "rate_limited");
        assert!(!throttled.message.contains("synthetic body"));
    }

    #[test]
    fn rejected_credentials_latch_until_restart() {
        let now = Instant::now();
        let rejected = classify_fetch_error(LibreLinkUpError::BadCredentials);
        assert_eq!(rejected.code, "authentication_failed");
        assert_eq!(rejected.retry_after_seconds, None);
        assert!(latches_until_restart(&rejected));
        let latched = Cooldown {
            until: cooldown_until(&rejected, now),
            error: rejected,
        };
        assert!(latched.until.is_none());
        assert!(cooldown_active(
            &latched,
            now + StdDuration::from_secs(86_400)
        ));

        let throttled = classify_fetch_error(LibreLinkUpError::AccountLocked(120));
        let temporary = Cooldown {
            until: cooldown_until(&throttled, now),
            error: throttled,
        };
        assert!(cooldown_active(
            &temporary,
            now + StdDuration::from_secs(119)
        ));
        assert!(!cooldown_active(
            &temporary,
            now + StdDuration::from_secs(121)
        ));
    }

    #[test]
    fn current_reading_comes_from_glucose_measurement() {
        let measurement = GlucoseMeasurement {
            factory_timestamp: "11/05/2026 07:58:00 PM".to_owned(),
            timestamp: "11/05/2026 02:58:00 PM".to_owned(),
            item_type: 1,
            value_in_mg_per_dl: 123.0,
            trend_arrow: 4,
            trend_message: None,
            measurement_color: 1,
            glucose_units: 1,
            value: 123.0,
            is_high: false,
            is_low: false,
        };
        let sample = sample_from_measurement(&measurement);
        assert_eq!(sample.timestamp, measurement.timestamp);
        assert_eq!(sample.factory_timestamp, measurement.factory_timestamp);
        assert_eq!(sample.value_in_mg_per_dl, 123.0);
        assert_eq!(sample.trend_arrow, Some(4));
    }

    #[test]
    fn sensor_fields_are_mapped_to_named_fields() {
        let sensor = Sensor {
            device_id: "synthetic-device".to_owned(),
            sn: "SYNTH0001".to_owned(),
            a: 1_652_400_270,
            w: 60,
            pt: 4,
        };
        let record = sensor_record(&sensor);
        assert_eq!(record.device_id, "synthetic-device");
        assert_eq!(record.serial_number, "SYNTH0001");
        assert_eq!(record.activation_epoch_seconds, 1_652_400_270);
        assert_eq!(record.warmup_minutes, 60);
        assert_eq!(record.product_type_code, 4);
    }
}
