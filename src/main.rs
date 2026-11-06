use chrono::{DateTime, Utc};
use keyring::{Entry, Error as KeyringError};
use libre_link_up_api_client::{
    ClientConfig, ConnectionIdentifier, LibreLinkUpClient, LibreLinkUpError, ReadRawResponse,
    Region,
};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::sync::{Mutex, OnceCell};
use zeroize::{Zeroize, Zeroizing};

mod mcp;
mod time;

use time::{TimeAssessment, assess_time_at};

const KEYRING_SERVICE: &str = "org.librelink-mcp-rs";
const KEYRING_USER: &str = "librelinkup-account";
const SETTINGS_USER: &str = "local-settings";
static CLIENT: OnceCell<LibreLinkUpClient> = OnceCell::const_new();
static FETCH_STATE: Mutex<FetchState> = Mutex::const_new(FetchState {
    cached: None,
    cooldown: None,
});
const CACHE_TTL: StdDuration = StdDuration::from_secs(60);
const FAILURE_COOLDOWN: StdDuration = StdDuration::from_secs(60);

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

#[derive(Clone)]
struct FetchedRaw {
    response: ReadRawResponse,
    retrieved_at_utc: DateTime<Utc>,
    cache_hit: bool,
}

impl Deref for FetchedRaw {
    type Target = ReadRawResponse;

    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

struct CacheEntry {
    response: ReadRawResponse,
    retrieved_at_utc: DateTime<Utc>,
    expires_at: Instant,
}

#[derive(Clone, Copy)]
pub(crate) struct FetchError {
    code: &'static str,
    message: &'static str,
    retry_after_seconds: Option<u64>,
}

struct Cooldown {
    /// `None` latches the error until the server restarts.
    until: Option<Instant>,
    error: FetchError,
}

/// A rejected password cannot fix itself while the server holds the same
/// credentials, and each retry is another failed login that moves the account
/// toward an Abbott lockout. Latch it until restart instead of retrying.
fn latches_until_restart(error: &FetchError) -> bool {
    error.code == "authentication_failed"
}

fn cooldown_until(error: &FetchError, now: Instant) -> Option<Instant> {
    if latches_until_restart(error) {
        return None;
    }
    let seconds = error
        .retry_after_seconds
        .unwrap_or(FAILURE_COOLDOWN.as_secs());
    Some(now + StdDuration::from_secs(seconds))
}

fn cooldown_active(cooldown: &Cooldown, now: Instant) -> bool {
    cooldown.until.is_none_or(|until| now < until)
}

struct FetchState {
    cached: Option<CacheEntry>,
    cooldown: Option<Cooldown>,
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
        message,
        retry_after_seconds,
    }
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
    delete_entry_if_present(keyring_entry()?)?;
    delete_entry_if_present(settings_entry()?)?;
    println!(
        "This server's credentials and local preferences were removed from the OS credential store."
    );
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

fn unique_match<T>(items: &[T], predicate: impl Fn(&T) -> bool) -> Option<&T> {
    let mut matches = items.iter().filter(|item| predicate(item));
    let selected = matches.next()?;
    matches.next().is_none().then_some(selected)
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

async fn fetch_raw() -> Result<FetchedRaw, FetchError> {
    let mut state = FETCH_STATE.lock().await;
    let now = Instant::now();
    if let Some(cached) = state.cached.as_ref().filter(|entry| now < entry.expires_at) {
        return Ok(FetchedRaw {
            response: cached.response.clone(),
            retrieved_at_utc: cached.retrieved_at_utc,
            cache_hit: true,
        });
    }
    if let Some(cooldown) = state
        .cooldown
        .as_ref()
        .filter(|entry| cooldown_active(entry, now))
    {
        let mut error = cooldown.error;
        error.retry_after_seconds = cooldown
            .until
            .map(|until| until.duration_since(now).as_secs().max(1));
        return Err(error);
    }
    let client = CLIENT
        .get_or_try_init(|| async { build_client() })
        .await
        .map_err(|message| FetchError {
            code: "configuration_error",
            message,
            retry_after_seconds: None,
        })?;
    // read_raw preserves the source timestamp. The client's processed read()
    // currently replaces an unparseable timestamp with Utc::now().
    let response = match tokio::time::timeout(StdDuration::from_secs(30), client.read_raw()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            let error = classify_fetch_error(error);
            state.cooldown = Some(Cooldown {
                until: cooldown_until(&error, Instant::now()),
                error,
            });
            return Err(error);
        }
        Err(_) => {
            let error = FetchError {
                code: "timeout",
                message: "LibreLinkUp request timed out; retry later.",
                retry_after_seconds: Some(FAILURE_COOLDOWN.as_secs()),
            };
            state.cooldown = Some(Cooldown {
                until: Some(Instant::now() + FAILURE_COOLDOWN),
                error,
            });
            return Err(error);
        }
    };
    let retrieved_at_utc = Utc::now();
    state.cached = Some(CacheEntry {
        response: response.clone(),
        retrieved_at_utc,
        expires_at: Instant::now() + CACHE_TTL,
    });
    state.cooldown = None;
    Ok(FetchedRaw {
        response,
        retrieved_at_utc,
        cache_hit: false,
    })
}

async fn check() -> Result<(), &'static str> {
    let raw = fetch_raw().await.map_err(|error| error.message)?;
    let measurement = &raw.connection.glucose_measurement;
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
        graph_points_returned: raw.graph_data.len(),
        active_sensors_returned: raw.active_sensors.len(),
        cache_hit: raw.cache_hit,
    };
    let output = serde_json::to_string_pretty(&report).map_err(|_| "Unable to format result.")?;
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
    let result = match args.as_slice() {
        [_, command] if command == "setup" => setup(),
        [_, command] if command == "check" => check().await,
        [_, command] if command == "select-patient" => select_patient(),
        [_, command] if command == "set-api-version" => set_api_version(),
        [_, command] if command == "clear-local-data" => clear_local_data(),
        [_, command] if command == "serve" => mcp::serve().await,
        _ => Err(
            "Usage: librelink-mcp-rs <setup|select-patient|set-api-version|check|clear-local-data|serve>",
        ),
    };
    if let Err(message) = result {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(target_os = "linux")]
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

    #[test]
    fn patient_selection_requires_one_match() {
        let names = ["Alex", "Sam", "Sam"];
        assert_eq!(unique_match(&names, |name| *name == "Alex"), Some(&"Alex"));
        assert_eq!(unique_match(&names, |name| *name == "Sam"), None);
        assert_eq!(unique_match(&names, |name| *name == "Pat"), None);
        assert_eq!(unique_match(&names, |_| true), None);
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
}
