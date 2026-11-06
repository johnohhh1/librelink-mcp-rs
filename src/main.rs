use chrono::Utc;
use keyring::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::io::{self, Write};
use zeroize::{Zeroize, Zeroizing};

mod collect;
mod fetch;
mod llu_client;
mod mcp;
mod region;
mod source;
mod store;
mod time;

use fetch::Fetcher;
use llu_client::LiveSource;
use time::{TimeAssessment, assess_time_at};

const KEYRING_SERVICE: &str = "org.librelink-mcp-rs";
const KEYRING_USER: &str = "librelinkup-account";
const SETTINGS_USER: &str = "local-settings";

/// The production data path: the owned LibreLinkUp client behind the cache,
/// cooldown, and latches.
fn live_fetcher() -> Fetcher {
    Fetcher::new(Box::new(LiveSource::new()))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    email: String,
    password: String,
    region: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    target_low: u16,
    target_high: u16,
    patient_name: Option<String>,
    #[serde(default = "default_api_version")]
    api_version: String,
}

fn default_api_version() -> String {
    "4.16.0".to_owned()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            target_low: 70,
            target_high: 180,
            patient_name: None,
            api_version: default_api_version(),
        }
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

#[derive(Serialize)]
struct ReadingSnapshot {
    glucose_mg_dl: Option<f64>,
    value_quality: &'static str,
    #[serde(flatten)]
    time: TimeAssessment,
}

#[derive(Serialize)]
struct ConnectivityReport {
    status: &'static str,
    source: &'static str,
    reading: ReadingSnapshot,
    graph_points_returned: usize,
    active_sensors_returned: usize,
    cache_hit: bool,
}

fn valid_api_version(version: &str) -> bool {
    let parts: Vec<_> = version.split('.').collect();
    parts.len() == 3
        && version.len() <= 20
        && parts.iter().all(|part| {
            !part.is_empty() && part.len() <= 5 && part.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn keyring_entry() -> Result<Entry, &'static str> {
    Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|_| "OS credential store is unavailable; no credentials were saved.")
}

fn settings_entry() -> Result<Entry, &'static str> {
    Entry::new(KEYRING_SERVICE, SETTINGS_USER).map_err(|_| "OS credential store is unavailable.")
}

fn load_settings() -> Result<Settings, &'static str> {
    match settings_entry()?.get_password() {
        Ok(value) => serde_json::from_str(&value).map_err(|_| "Stored settings are invalid."),
        Err(KeyringError::NoEntry) => Ok(Settings::default()),
        Err(_) => Err("Unable to read settings from the OS credential store."),
    }
}

fn save_settings(settings: &Settings) -> Result<(), &'static str> {
    let encoded = serde_json::to_string(settings).map_err(|_| "Unable to encode settings.")?;
    settings_entry()?
        .set_password(&encoded)
        .map_err(|_| "Unable to save settings in the OS credential store.")
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
    let region = region::canonical_region(&region_input)
        .ok_or("Unknown region.")?
        .to_owned();

    let credentials = Credentials {
        email,
        password,
        region,
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

fn select_patient() -> Result<(), &'static str> {
    let name = prompt("Followed person's full name (blank to clear selection): ")?;
    let mut settings = load_settings()?;
    settings.patient_name = (!name.is_empty()).then_some(name);
    save_settings(&settings)?;
    println!("Patient selection saved locally. Run `check` to verify it.");
    Ok(())
}

fn set_api_version() -> Result<(), &'static str> {
    let input = prompt("LibreLinkUp API version (for example 4.16.0): ")?;
    if !valid_api_version(&input) {
        return Err("Version must have three numeric components, such as 4.16.0.");
    }
    let mut settings = load_settings()?;
    settings.api_version = input;
    save_settings(&settings)?;
    println!("API version saved locally. Restart the MCP server to use it.");
    Ok(())
}

fn delete_entry_if_present(entry: Entry) -> Result<(), &'static str> {
    match entry.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(_) => Err("Unable to delete an item from the OS credential store."),
    }
}

fn clear_local_data() -> Result<(), &'static str> {
    let db = store::default_db_path();
    // Check before deleting anything, so a refusal leaves everything intact.
    if let Some(db) = &db
        && store::database_locked(db)?
    {
        return Err(
            "A collector is running for the local database. Stop it, then run clear-local-data again.",
        );
    }
    delete_entry_if_present(keyring_entry()?)?;
    delete_entry_if_present(settings_entry()?)?;
    if let Some(db) = &db {
        store::delete_database_files(db)?;
    }
    println!("This server's credentials, local preferences, and local database were removed.");
    Ok(())
}

fn load_credentials() -> Result<Credentials, &'static str> {
    let stored = match keyring_entry()?.get_password() {
        Ok(value) => Zeroizing::new(value),
        Err(KeyringError::NoEntry) => return Err("No credentials found. Run `setup` locally."),
        Err(_) => {
            return Err(
                "Unable to read the OS credential store. On Linux, a Secret Service keyring (such as GNOME Keyring or KWallet) must be running and unlocked in the user's session.",
            );
        }
    };
    serde_json::from_str(&stored).map_err(|_| "Stored credentials have an unsupported format.")
}

async fn check() -> Result<(), Cow<'static, str>> {
    let raw = live_fetcher()
        .fetch()
        .await
        .map_err(|error| error.message)?;
    let measurement = &raw.current;
    let time = assess_time_at(
        Some(&measurement.timestamp),
        Some(&measurement.factory_timestamp),
        raw.retrieved_at_utc,
        Utc::now(),
    );
    let report = ConnectivityReport {
        status: "connected",
        source: "librelinkup_sharing_api",
        reading: ReadingSnapshot {
            glucose_mg_dl: (measurement.value_in_mg_per_dl.is_finite()
                && measurement.value_in_mg_per_dl > 0.0)
                .then_some(measurement.value_in_mg_per_dl),
            value_quality: if measurement.value_in_mg_per_dl.is_finite()
                && measurement.value_in_mg_per_dl > 0.0
            {
                "observed"
            } else {
                "missing"
            },
            time,
        },
        graph_points_returned: raw.graph.len(),
        active_sensors_returned: raw.active_sensors.len(),
        cache_hit: raw.cache_hit,
    };
    let output =
        serde_json::to_string_pretty(&report).map_err(|_| Cow::from("Unable to format result."))?;
    println!("{output}");
    Ok(())
}

/// On Linux the keyring talks to Secret Service over the D-Bus session bus.
/// MCP clients started outside a desktop session often lack
/// DBUS_SESSION_BUS_ADDRESS even though the user's bus socket exists. Returns
/// the address to use, or None when it is already set or no usable socket exists.
#[cfg(target_os = "linux")]
fn session_bus_fallback(
    existing: Option<std::ffi::OsString>,
    uid: u32,
    socket_is_owned_by_uid: impl Fn(&str) -> bool,
) -> Option<String> {
    if existing.is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let socket = format!("/run/user/{uid}/bus");
    socket_is_owned_by_uid(&socket).then(|| format!("unix:path={socket}"))
}

#[cfg(target_os = "linux")]
fn ensure_session_bus() {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(process) = std::fs::metadata("/proc/self") else {
        return;
    };
    let uid = process.uid();
    let address = session_bus_fallback(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS"),
        uid,
        |socket| {
            std::fs::metadata(socket)
                .is_ok_and(|meta| meta.file_type().is_socket() && meta.uid() == uid)
        },
    );
    if let Some(address) = address {
        // SAFETY: called first thing in main, before the Tokio runtime or any
        // other thread exists, so nothing can read the environment concurrently.
        unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", address) };
    }
}

#[cfg(not(target_os = "linux"))]
fn ensure_session_bus() {}

fn main() {
    ensure_session_bus();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            eprintln!("Unable to start the async runtime.");
            std::process::exit(1);
        }
    };
    runtime.block_on(run());
}

async fn run() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("collect") {
        let code = collect::run_cli(&args[2..], live_fetcher()).await;
        std::process::exit(code);
    }
    let result: Result<(), Cow<'static, str>> = match args.as_slice() {
        [_, command] if command == "setup" => setup().map_err(Cow::from),
        [_, command] if command == "check" => check().await,
        [_, command] if command == "select-patient" => select_patient().map_err(Cow::from),
        [_, command] if command == "set-api-version" => set_api_version().map_err(Cow::from),
        [_, command] if command == "clear-local-data" => clear_local_data().map_err(Cow::from),
        [_, command] if command == "serve" => mcp::serve(live_fetcher()).await.map_err(Cow::from),
        _ => Err(Cow::from(
            "Usage: librelink-mcp-rs <setup|select-patient|set-api-version|check|collect|clear-local-data|serve>",
        )),
    };
    if let Err(message) = result {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn session_bus_fallback_only_fills_a_missing_address() {
        let owned = |_: &str| true;
        assert_eq!(
            session_bus_fallback(None, 1000, owned).as_deref(),
            Some("unix:path=/run/user/1000/bus")
        );
        assert_eq!(
            session_bus_fallback(Some("".into()), 1000, owned).as_deref(),
            Some("unix:path=/run/user/1000/bus")
        );
        assert_eq!(
            session_bus_fallback(Some("unix:path=/custom".into()), 1000, owned),
            None
        );
        assert_eq!(session_bus_fallback(None, 1000, |_| false), None);
    }
}
