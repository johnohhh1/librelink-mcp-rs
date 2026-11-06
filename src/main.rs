use chrono::{DateTime, Duration, Utc};
use keyring::{Entry, Error as KeyringError};
use libre_link_up_api_client::{ClientConfig, LibreLinkUpClient, LibreLinkUpError, Region};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::time::Duration as StdDuration;
use zeroize::{Zeroize, Zeroizing};

const KEYRING_SERVICE: &str = "org.librelink-mcp-rs";
const KEYRING_USER: &str = "librelinkup-account";
const STALE_AFTER_MINUTES: i64 = 15;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    email: String,
    password: String,
    region: String,
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Freshness {
    Recent,
    Stale,
    Unknown,
    Missing,
}

#[derive(Serialize)]
struct ReadingSnapshot {
    glucose_mg_dl: f64,
    source_timestamp: Option<String>,
    freshness: Freshness,
}

#[derive(Serialize)]
struct ConnectivityReport {
    status: &'static str,
    source: &'static str,
    reading: ReadingSnapshot,
    graph_points_returned: usize,
    active_sensors_returned: usize,
}

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

fn classify_freshness(timestamp: Option<&str>, now: DateTime<Utc>) -> Freshness {
    let Some(timestamp) = timestamp.filter(|value| !value.trim().is_empty()) else {
        return Freshness::Missing;
    };

    // The observed LibreLinkUp clock string has no timezone. It cannot be used
    // to assert that a reading is recent or stale without more account context.
    let Ok(parsed) = DateTime::parse_from_rfc3339(timestamp) else {
        return Freshness::Unknown;
    };
    let age = now.signed_duration_since(parsed.with_timezone(&Utc));
    if age < Duration::minutes(-2) {
        Freshness::Unknown
    } else if age > Duration::minutes(STALE_AFTER_MINUTES) {
        Freshness::Stale
    } else {
        Freshness::Recent
    }
}

fn keyring_entry() -> Result<Entry, &'static str> {
    Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|_| "OS credential store is unavailable; no credentials were saved.")
}

fn prompt(label: &str) -> Result<String, &'static str> {
    print!("{label}");
    io::stdout()
        .flush()
        .map_err(|_| "Unable to write prompt.")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|_| "Unable to read input.")?;
    Ok(answer.trim().to_owned())
}

fn setup() -> Result<(), &'static str> {
    let email = prompt("LibreLinkUp follower account email: ")?;
    if email.is_empty() {
        return Err("Email is required.");
    }
    let password = rpassword::prompt_password("LibreLinkUp password: ")
        .map_err(|_| "Unable to read password from the terminal.")?;
    if password.is_empty() {
        return Err("Password is required.");
    }
    let region_input = prompt("Region [global/us/eu/...; default global]: ")?;
    let region = parse_region(&region_input).ok_or("Unknown region.")?;

    let credentials = Credentials {
        email,
        password,
        region: region.as_str().to_owned(),
    };
    let serialized = Zeroizing::new(
        serde_json::to_string(&credentials).map_err(|_| "Unable to prepare credentials.")?,
    );
    keyring_entry()?
        .set_password(&serialized)
        .map_err(|_| "OS credential store rejected the credentials; no file fallback was used.")?;
    println!("Credentials saved in the OS credential store.");
    Ok(())
}

fn load_credentials() -> Result<Credentials, &'static str> {
    let stored = match keyring_entry()?.get_password() {
        Ok(value) => Zeroizing::new(value),
        Err(KeyringError::NoEntry) => return Err("No credentials found. Run `setup` locally."),
        Err(_) => return Err("Unable to read the OS credential store."),
    };
    serde_json::from_str(&stored).map_err(|_| "Stored credentials have an unsupported format.")
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
        _ => "LibreLinkUp request failed; details were withheld to protect account data.",
    }
}

async fn check() -> Result<(), &'static str> {
    let credentials = load_credentials()?;
    let region = parse_region(&credentials.region).ok_or("Stored region is invalid.")?;
    let config = ClientConfig::new(credentials.email.clone(), credentials.password.clone())
        .with_region(region);
    let client = LibreLinkUpClient::new(config).map_err(client_error)?;

    // read_raw preserves the source timestamp. The client's processed read()
    // currently replaces an unparseable timestamp with Utc::now().
    let raw = tokio::time::timeout(StdDuration::from_secs(30), client.read_raw())
        .await
        .map_err(|_| "LibreLinkUp request timed out.")?
        .map_err(client_error)?;
    let measurement = &raw.connection.glucose_measurement;
    let timestamp =
        (!measurement.timestamp.trim().is_empty()).then(|| measurement.timestamp.clone());
    let report = ConnectivityReport {
        status: "connected",
        source: "librelinkup_sharing_api",
        reading: ReadingSnapshot {
            glucose_mg_dl: measurement.value_in_mg_per_dl,
            freshness: classify_freshness(timestamp.as_deref(), Utc::now()),
            source_timestamp: timestamp,
        },
        graph_points_returned: raw.graph_data.len(),
        active_sensors_returned: raw.active_sensors.len(),
    };
    let output = serde_json::to_string_pretty(&report).map_err(|_| "Unable to format result.")?;
    println!("{output}");
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.as_slice() {
        [_, command] if command == "setup" => setup(),
        [_, command] if command == "check" => check().await,
        _ => Err("Usage: librelink-mcp-rs <setup|check>"),
    };
    if let Err(message) = result {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-05T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn accepts_only_known_regions() {
        assert_eq!(parse_region("EU2"), Some(Region::EU2));
        assert_eq!(parse_region(""), Some(Region::Global));
        assert_eq!(parse_region("unknown"), None);
    }

    #[test]
    fn synthetic_timestamps_have_explicit_quality() {
        assert_eq!(classify_freshness(None, now()), Freshness::Missing);
        assert_eq!(
            classify_freshness(Some("11/05/2026 03:58:00 PM"), now()),
            Freshness::Unknown
        );
        assert_eq!(
            classify_freshness(Some("2026-11-05T19:56:00Z"), now()),
            Freshness::Recent
        );
        assert_eq!(
            classify_freshness(Some("2026-11-05T19:20:00Z"), now()),
            Freshness::Stale
        );
        assert_eq!(
            classify_freshness(Some("2026-11-05T20:10:00Z"), now()),
            Freshness::Unknown
        );
    }
}
