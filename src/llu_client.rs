//! Owned LibreLinkUp client. See docs/adr/0001-owned-api-client.md.
//!
//! Login happens only when there is no token, the token has expired, or a
//! request returns HTTP 401, and at most one 401 re-login per `read()`.
//! Throttling, server errors, parse errors, and rejected requests never
//! trigger a login.
//!
//! Errors carry fixed messages. Response bodies are read only to classify
//! the failure and are never logged or returned.

// Not wired into `serve` or `check` until v0.2 step 3, which removes this.
#![allow(dead_code)]

use crate::fetch::FetchError;
use crate::source::{DataSource, GlucoseSample, ReadFuture, SensorRecord, Snapshot};
use chrono::Utc;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_TYPE, HeaderMap,
    HeaderValue, RETRY_AFTER, USER_AGENT,
};
use reqwest::{Client, Response, StatusCode, redirect};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use tokio::sync::Mutex;
use zeroize::Zeroizing;

const LOGIN_PATH: &str = "/llu/auth/login";
const CONNECTIONS_PATH: &str = "/llu/connections";
// Headers match the borrowed crate exactly so the v0.2 switch-over is a pure
// swap. Changing the product header is a separate, later experiment.
const PRODUCT: &str = "llu.ios";
const USER_AGENT_STRING: &str = "Mozilla/5.0 (iPhone; CPU OS 17_4.1 like Mac OS X) AppleWebKit/536.26 (KHTML, like Gecko) Version/17.4.1 Mobile/10A5355d Safari/8536.25";

/// Production base URL for a stored region, matching the borrowed client.
pub(crate) fn region_base_url(region: &str) -> Option<&'static str> {
    Some(match region {
        "global" => "https://api.libreview.io",
        "ae" => "https://api-ae.libreview.io",
        "ap" => "https://api-ap.libreview.io",
        "au" => "https://api-au.libreview.io",
        "ca" => "https://api-ca.libreview.io",
        "de" => "https://api-de.libreview.io",
        "eu" => "https://api-eu.libreview.io",
        "eu2" => "https://api-eu2.libreview.io",
        "fr" => "https://api-fr.libreview.io",
        "jp" => "https://api-jp.libreview.io",
        "us" => "https://api-us.libreview.io",
        "la" => "https://api-la.libreview.io",
        "ru" => "https://api.libreview.ru",
        "cn" => "https://api-cn.myfreestyle.cn",
        _ => return None,
    })
}

/// First gate for a login redirect region: short lowercase alphanumerics.
/// The region must also be in `region_base_url`'s table.
fn valid_redirect_region(region: &str) -> bool {
    (2..=4).contains(&region.len())
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

/// The patient ID goes into a URL path.
fn valid_patient_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

pub(crate) fn unique_match<T>(items: &[T], predicate: impl Fn(&T) -> bool) -> Option<&T> {
    let mut matches = items.iter().filter(|item| predicate(item));
    let selected = matches.next()?;
    matches.next().is_none().then_some(selected)
}

/// Everything the client needs. No `Clone` or `Debug`: the password exists
/// once, in zeroizing storage.
pub(crate) struct LluConfig {
    pub email: String,
    pub password: Zeroizing<String>,
    pub api_version: String,
    pub patient_name: Option<String>,
    pub base_url: String,
}

struct Token {
    value: Zeroizing<String>,
    /// Epoch seconds from `authTicket.expires`; `None` if not sent, in which
    /// case the token is used until a 401.
    expires_at: Option<i64>,
}

struct Session {
    base_url: String,
    token: Option<Token>,
    account_id_hash: Option<String>,
    patient_id: Option<String>,
}

impl Session {
    fn needs_login(&self, now: i64) -> bool {
        self.token
            .as_ref()
            .is_none_or(|token| token.expires_at.is_some_and(|expires| now >= expires))
    }
}

/// Maps a resolved production base URL to the URL actually used.
/// Identity in production; tests substitute a mock server.
type UrlRewrite = Box<dyn Fn(&'static str) -> String + Send + Sync>;

pub(crate) struct LluClient {
    http: Client,
    email: String,
    password: Zeroizing<String>,
    patient_name: Option<String>,
    redirect_rewrite: UrlRewrite,
    session: Mutex<Session>,
}

const DEFAULT_RETRY_SECONDS: u64 = 60;
const MAX_RETRY_SECONDS: u64 = 900;
/// Graph responses are tens of kilobytes; anything far larger is refused.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

const fn error(code: &'static str, message: &'static str) -> FetchError {
    FetchError {
        code,
        message: Cow::Borrowed(message),
        retry_after_seconds: Some(DEFAULT_RETRY_SECONDS),
    }
}

/// An error that retrying will not fix: no retry is suggested to the caller.
const fn final_error(code: &'static str, message: &'static str) -> FetchError {
    FetchError {
        code,
        message: Cow::Borrowed(message),
        retry_after_seconds: None,
    }
}

const UNAVAILABLE: FetchError = error(
    "upstream_unavailable",
    "LibreLinkUp request failed; retry later.",
);
const SESSION_EXPIRED: FetchError = error(
    "session_expired",
    "LibreLinkUp rejected the current session; retry later.",
);
const BAD_CREDENTIALS: FetchError = final_error(
    "authentication_failed",
    "LibreLinkUp rejected the account credentials. No further logins will be attempted; run `setup` locally, then restart the server.",
);
const ACTION_REQUIRED: FetchError = error(
    "account_action_required",
    "LibreLinkUp requires an action in its app before API access.",
);
const REQUEST_REJECTED: FetchError = final_error(
    "request_rejected",
    "LibreLinkUp rejected this client's request headers. This is a bug in librelink-mcp-rs: please report it, without account details. No further requests will be made until the server is restarted.",
);
/// A candidate retirement signal, not proof that the API was retired.
const API_CHANGED: FetchError = error(
    "api_changed",
    "LibreLinkUp returned a successful response that is not JSON. The API may have changed or been retired.",
);
const NO_CONNECTION: FetchError = error(
    "no_connection",
    "Account authenticated but follows no patients in LibreLinkUp.",
);
const SELECTION_REQUIRED: FetchError = error(
    "patient_selection_required",
    "No unique followed person was selected. Run `select-patient` locally.",
);

fn rate_limited(seconds: Option<u64>) -> FetchError {
    FetchError {
        code: "rate_limited",
        message: Cow::Borrowed("LibreLinkUp is rate limiting requests; retry later."),
        retry_after_seconds: Some(
            seconds
                .unwrap_or(DEFAULT_RETRY_SECONDS)
                .clamp(DEFAULT_RETRY_SECONDS, MAX_RETRY_SECONDS),
        ),
    }
}

/// `minimumVersion` reaches the message only if it is plain digits and dots.
fn plain_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 20
        && text.bytes().any(|byte| byte.is_ascii_digit())
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

fn version_rejected(minimum: Option<&str>) -> FetchError {
    let message = match minimum.filter(|version| plain_version(version)) {
        Some(version) => Cow::Owned(format!(
            "LibreLinkUp requires a newer client version (minimum {version}). Run `set-api-version` locally, then restart the server; no further requests will be made until then."
        )),
        None => Cow::Borrowed(
            "LibreLinkUp requires a newer client version. Run `set-api-version` locally, then restart the server; no further requests will be made until then.",
        ),
    };
    FetchError {
        code: "client_version_rejected",
        message,
        retry_after_seconds: None,
    }
}

/// A response read in full, within `MAX_BODY_BYTES`.
struct Reply {
    status: StatusCode,
    retry_after_seconds: Option<u64>,
    body: Vec<u8>,
}

async fn receive(mut response: Response) -> Result<Reply, FetchError> {
    let status = response.status();
    // Only the delay-seconds form; an HTTP date falls back to the default.
    let retry_after_seconds = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok());
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| UNAVAILABLE)? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(UNAVAILABLE);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Reply {
        status,
        retry_after_seconds,
        body,
    })
}

/// LibreLinkUp names header problems in a `message` field. These mean the
/// request itself is wrong, so neither a retry nor a new login can help.
fn rejects_request(body: &[u8]) -> bool {
    const REJECTIONS: [&str; 2] = ["AccountIdMismatch", "RequiredHeaderMissing"];
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    [
        value.get("message"),
        value.get("error").and_then(|error| error.get("message")),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .any(|message| REJECTIONS.contains(&message))
}

/// Maps a non-success reply that is not a rejection or a retriable 401.
fn failure(reply: &Reply) -> FetchError {
    match reply.status.as_u16() {
        401 => SESSION_EXPIRED,
        429 | 430 => rate_limited(reply.retry_after_seconds),
        403 => match serde_json::from_slice::<Value>(&reply.body) {
            Ok(body) if body.get("status").and_then(Value::as_i64) == Some(920) => {
                version_rejected(
                    body.get("data")
                        .and_then(|data| data.get("minimumVersion"))
                        .and_then(Value::as_str),
                )
            }
            _ => UNAVAILABLE,
        },
        _ => UNAVAILABLE,
    }
}

#[derive(Serialize)]
struct LoginBody<'a> {
    email: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct LoginEnvelope {
    status: i32,
    #[serde(default)]
    data: Option<LoginData>,
}

#[derive(Deserialize)]
struct LoginData {
    #[serde(default)]
    redirect: bool,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    user: Option<UserRef>,
    #[serde(default, rename = "authTicket")]
    auth_ticket: Option<AuthTicket>,
    /// Lockout seconds, sent as `data.data.lockout` or `data.lockout`.
    #[serde(default)]
    lockout: Option<i64>,
    #[serde(default)]
    data: Option<LockoutInfo>,
}

impl LoginData {
    fn lockout_seconds(&self) -> Option<u64> {
        self.data
            .as_ref()
            .and_then(|info| info.lockout)
            .or(self.lockout)
            .and_then(|seconds| u64::try_from(seconds).ok())
    }
}

#[derive(Deserialize)]
struct LockoutInfo {
    #[serde(default)]
    lockout: Option<i64>,
}

#[derive(Deserialize)]
struct UserRef {
    id: String,
}

#[derive(Deserialize)]
struct AuthTicket {
    token: String,
    #[serde(default)]
    expires: Option<i64>,
}

#[derive(Deserialize)]
struct ConnectionsEnvelope {
    #[serde(default)]
    data: Vec<ConnectionSummary>,
}

#[derive(Deserialize)]
struct ConnectionSummary {
    #[serde(rename = "patientId")]
    patient_id: String,
    #[serde(default, rename = "firstName")]
    first_name: String,
    #[serde(default, rename = "lastName")]
    last_name: String,
}

#[derive(Deserialize)]
struct GraphEnvelope {
    data: GraphData,
}

#[derive(Deserialize)]
struct GraphData {
    connection: GraphConnection,
    #[serde(default, rename = "activeSensors")]
    active_sensors: Vec<ActiveSensorWire>,
    #[serde(default, rename = "graphData")]
    graph_data: Vec<GlucoseWire>,
}

#[derive(Deserialize)]
struct GraphConnection {
    #[serde(default, rename = "glucoseMeasurement")]
    glucose_measurement: GlucoseWire,
}

/// Missing fields become empty or zero, which the tools report as missing;
/// nothing is invented.
#[derive(Default, Deserialize)]
struct GlucoseWire {
    #[serde(default, rename = "Timestamp")]
    timestamp: String,
    #[serde(default, rename = "FactoryTimestamp")]
    factory_timestamp: String,
    #[serde(default, rename = "ValueInMgPerDl")]
    value_in_mg_per_dl: f64,
    #[serde(default, rename = "TrendArrow")]
    trend_arrow: Option<i32>,
}

impl From<GlucoseWire> for GlucoseSample {
    fn from(wire: GlucoseWire) -> Self {
        Self {
            timestamp: wire.timestamp,
            factory_timestamp: wire.factory_timestamp,
            value_in_mg_per_dl: wire.value_in_mg_per_dl,
            trend_arrow: wire.trend_arrow,
        }
    }
}

#[derive(Deserialize)]
struct ActiveSensorWire {
    sensor: SensorWire,
}

const fn unknown_code() -> i32 {
    -1
}

#[derive(Deserialize)]
struct SensorWire {
    #[serde(default, rename = "deviceId")]
    device_id: String,
    #[serde(default)]
    sn: String,
    #[serde(default)]
    a: i32,
    #[serde(default = "unknown_code")]
    w: i32,
    #[serde(default = "unknown_code")]
    pt: i32,
}

impl From<SensorWire> for SensorRecord {
    fn from(wire: SensorWire) -> Self {
        Self {
            device_id: wire.device_id,
            serial_number: wire.sn,
            activation_epoch_seconds: wire.a,
            warmup_minutes: wire.w,
            product_type_code: wire.pt,
        }
    }
}

/// A body that is not JSON at all is `api_changed`; JSON of the wrong shape
/// is `upstream_unavailable`.
fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, FetchError> {
    serde_json::from_slice(body).map_err(|error| {
        if error.is_data() {
            UNAVAILABLE
        } else {
            API_CHANGED
        }
    })
}

fn account_id_hash(user_id: &str) -> String {
    format!("{:x}", Sha256::digest(user_id.as_bytes()))
}

impl LluClient {
    pub(crate) fn new(config: LluConfig) -> Result<Self, &'static str> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_STRING));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(CONNECTION, HeaderValue::from_static("Keep-Alive"));
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US"));
        headers.insert("product", HeaderValue::from_static(PRODUCT));
        headers.insert(
            "version",
            HeaderValue::from_str(&config.api_version).map_err(|_| "Invalid API version.")?,
        );
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json;charset=UTF-8"),
        );
        let http = Client::builder()
            .default_headers(headers)
            // Sends `accept-encoding: gzip` and decodes it, as the crate does.
            .gzip(true)
            // Region changes come only from the login body, never HTTP 3xx.
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|_| "Unable to start the HTTP client.")?;
        Ok(Self {
            http,
            email: config.email,
            password: config.password,
            patient_name: config.patient_name.map(|name| name.to_lowercase()),
            redirect_rewrite: Box::new(str::to_owned),
            session: Mutex::new(Session {
                base_url: config.base_url,
                token: None,
                account_id_hash: None,
                patient_id: None,
            }),
        })
    }

    /// Tests point a resolved redirect URL at a second mock server.
    #[cfg(test)]
    fn with_redirect_rewrite(
        mut self,
        rewrite: impl Fn(&'static str) -> String + Send + Sync + 'static,
    ) -> Self {
        self.redirect_rewrite = Box::new(rewrite);
        self
    }

    /// Logs in, following at most one region redirect.
    async fn login(&self, session: &mut Session) -> Result<(), FetchError> {
        session.token = None;
        let body = Zeroizing::new(
            serde_json::to_vec(&LoginBody {
                email: &self.email,
                password: &self.password,
            })
            .map_err(|_| UNAVAILABLE)?,
        );
        let mut base_url = session.base_url.clone();
        for attempt in 0..2 {
            let response = self
                .http
                .post(format!("{base_url}{LOGIN_PATH}"))
                .body(body.to_vec())
                .send()
                .await
                .map_err(|_| UNAVAILABLE)?;
            let reply = receive(response).await?;
            if rejects_request(&reply.body) {
                return Err(REQUEST_REJECTED);
            }
            if !reply.status.is_success() {
                return Err(failure(&reply));
            }
            let envelope: LoginEnvelope = parse(&reply.body)?;
            match envelope.status {
                0 => {}
                2 => return Err(BAD_CREDENTIALS),
                4 => return Err(ACTION_REQUIRED),
                429 => {
                    let seconds = envelope.data.as_ref().and_then(LoginData::lockout_seconds);
                    return Err(rate_limited(seconds));
                }
                _ => return Err(UNAVAILABLE),
            }
            let data = envelope.data.ok_or(UNAVAILABLE)?;
            if data.redirect {
                let region = data.region.as_deref().unwrap_or_default();
                if attempt > 0 || !valid_redirect_region(region) {
                    return Err(UNAVAILABLE);
                }
                let resolved = region_base_url(region).ok_or(UNAVAILABLE)?;
                base_url = (self.redirect_rewrite)(resolved);
                continue;
            }
            let (Some(user), Some(ticket)) = (data.user, data.auth_ticket) else {
                return Err(UNAVAILABLE);
            };
            let token = Zeroizing::new(ticket.token);
            if token.is_empty() || user.id.is_empty() {
                return Err(UNAVAILABLE);
            }
            session.base_url = base_url;
            session.account_id_hash = Some(account_id_hash(&user.id));
            session.token = Some(Token {
                value: token,
                expires_at: ticket.expires.filter(|expires| *expires > 0),
            });
            return Ok(());
        }
        Err(UNAVAILABLE)
    }

    /// One authenticated GET. A plain 401 triggers one re-login per `read()`,
    /// tracked by `relogged`; every other failure is returned without one.
    async fn get(
        &self,
        session: &mut Session,
        path: &str,
        relogged: &mut bool,
    ) -> Result<Vec<u8>, FetchError> {
        loop {
            let (Some(token), Some(account_id)) =
                (session.token.as_ref(), session.account_id_hash.as_ref())
            else {
                return Err(UNAVAILABLE);
            };
            let bearer = Zeroizing::new(format!("Bearer {}", token.value.as_str()));
            let mut authorization = HeaderValue::from_str(&bearer).map_err(|_| UNAVAILABLE)?;
            authorization.set_sensitive(true);
            let response = self
                .http
                .get(format!("{}{path}", session.base_url))
                .header(AUTHORIZATION, authorization)
                .header("account-id", account_id.as_str())
                .send()
                .await
                .map_err(|_| UNAVAILABLE)?;
            let reply = receive(response).await?;
            // Checked before 401: a rejected header is not a stale session.
            if rejects_request(&reply.body) {
                return Err(REQUEST_REJECTED);
            }
            if reply.status == StatusCode::UNAUTHORIZED {
                session.token = None;
                if *relogged {
                    return Err(SESSION_EXPIRED);
                }
                *relogged = true;
                self.login(session).await?;
                continue;
            }
            if !reply.status.is_success() {
                return Err(failure(&reply));
            }
            return Ok(reply.body);
        }
    }

    async fn patient_id(
        &self,
        session: &mut Session,
        relogged: &mut bool,
    ) -> Result<String, FetchError> {
        if let Some(id) = &session.patient_id {
            return Ok(id.clone());
        }
        let body = self.get(session, CONNECTIONS_PATH, relogged).await?;
        let connections: ConnectionsEnvelope = parse(&body)?;
        if connections.data.is_empty() {
            return Err(NO_CONNECTION);
        }
        let selected = unique_match(&connections.data, |connection| {
            self.patient_name.as_ref().is_none_or(|name| {
                format!("{} {}", connection.first_name, connection.last_name).to_lowercase()
                    == *name
            })
        })
        .ok_or(SELECTION_REQUIRED)?;
        if !valid_patient_id(&selected.patient_id) {
            return Err(UNAVAILABLE);
        }
        session.patient_id = Some(selected.patient_id.clone());
        Ok(selected.patient_id.clone())
    }

    async fn read_snapshot(&self) -> Result<Snapshot, FetchError> {
        let mut session = self.session.lock().await;
        if session.needs_login(Utc::now().timestamp()) {
            self.login(&mut session).await?;
        }
        let mut relogged = false;
        let patient_id = self.patient_id(&mut session, &mut relogged).await?;
        let path = format!("{CONNECTIONS_PATH}/{patient_id}/graph");
        let body = self.get(&mut session, &path, &mut relogged).await?;
        let graph: GraphEnvelope = parse(&body)?;
        Ok(Snapshot {
            current: graph.data.connection.glucose_measurement.into(),
            graph: graph.data.graph_data.into_iter().map(Into::into).collect(),
            active_sensors: graph
                .data
                .active_sensors
                .into_iter()
                .map(|active| active.sensor.into())
                .collect(),
        })
    }
}

impl DataSource for LluClient {
    fn read(&self) -> ReadFuture<'_> {
        Box::pin(self.read_snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // All values are synthetic.
    const USER_ID: &str = "synthetic-user-0001";
    /// `printf 'synthetic-user-0001' | sha256sum`, computed independently.
    const USER_ID_SHA256: &str = "7f988ea024f585ed713d5c9643770393e4350878457cea3401d0c38b263e5b0c";
    const TOKEN: &str = "synthetic-token";
    const PATIENT_ID: &str = "synthetic-patient-0001";
    const EMAIL: &str = "follower@example.invalid";
    const PASSWORD: &str = "synthetic-password";
    const GRAPH_PATH: &str = "/llu/connections/synthetic-patient-0001/graph";

    fn config(base_url: &str) -> LluConfig {
        LluConfig {
            email: EMAIL.to_owned(),
            password: Zeroizing::new(PASSWORD.to_owned()),
            api_version: "4.16.0".to_owned(),
            patient_name: None,
            base_url: base_url.to_owned(),
        }
    }

    fn client(server: &MockServer) -> LluClient {
        LluClient::new(config(&server.uri())).unwrap()
    }

    fn login_ok(expires: i64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "status": 0,
            "data": {
                "user": { "id": USER_ID, "country": "XX", "unusedField": true },
                "authTicket": { "token": TOKEN, "expires": expires, "duration": 3_600_000 },
            },
        }))
    }

    fn redirect_to(region: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({ "status": 0, "data": { "redirect": true, "region": region } }))
    }

    fn in_one_hour() -> i64 {
        Utc::now().timestamp() + 3600
    }

    fn connections(people: &[(&str, &str, &str)]) -> ResponseTemplate {
        let data: Vec<Value> = people
            .iter()
            .map(|(id, first, last)| {
                json!({ "patientId": id, "firstName": first, "lastName": last, "status": 2 })
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({ "status": 0, "data": data }))
    }

    fn graph_ok() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "status": 0,
            "data": {
                "connection": {
                    "glucoseMeasurement": {
                        "Timestamp": "11/05/2026 02:58:00 PM",
                        "FactoryTimestamp": "11/05/2026 07:58:00 PM",
                        "ValueInMgPerDl": 123,
                        "TrendArrow": 4,
                        "MeasurementColor": 1,
                    },
                    // A different reading the client must not use as current.
                    "glucoseItem": {
                        "Timestamp": "11/05/2026 02:57:00 PM",
                        "FactoryTimestamp": "11/05/2026 07:57:00 PM",
                        "ValueInMgPerDl": 999,
                    },
                    "unusedConnectionField": {},
                },
                "activeSensors": [{
                    "sensor": { "deviceId": "synthetic-device-0042", "sn": "SYNTH0001", "a": 1_652_400_270, "w": 60, "pt": 4 },
                    "device": { "unused": true },
                }],
                "graphData": [
                    {
                        "Timestamp": "11/05/2026 02:43:00 PM",
                        "FactoryTimestamp": "11/05/2026 07:43:00 PM",
                        "ValueInMgPerDl": 118,
                        "TrendArrow": null,
                    },
                    { "FactoryTimestamp": "11/05/2026 07:28:00 PM" },
                ],
            },
            "ticket": { "token": "unused" },
        }))
    }

    async fn mount(server: &MockServer, verb: &str, route: &str, response: ResponseTemplate) {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(response)
            .mount(server)
            .await;
    }

    /// Mounts a full working session: login, one connection, and a graph.
    async fn mount_session(server: &MockServer, expires: i64) {
        mount(server, "POST", LOGIN_PATH, login_ok(expires)).await;
        mount(
            server,
            "GET",
            CONNECTIONS_PATH,
            connections(&[(PATIENT_ID, "Sam", "Synthetic")]),
        )
        .await;
    }

    async fn hits(server: &MockServer, verb: &str, route: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.method.as_str() == verb && request.url.path() == route)
            .count()
    }

    async fn logins(server: &MockServer) -> usize {
        hits(server, "POST", LOGIN_PATH).await
    }

    #[tokio::test]
    async fn happy_path_maps_the_graph_into_a_snapshot() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(LOGIN_PATH))
            .and(body_json(json!({ "email": EMAIL, "password": PASSWORD })))
            .respond_with(login_ok(in_one_hour()))
            .mount(&server)
            .await;
        mount(
            &server,
            "GET",
            CONNECTIONS_PATH,
            connections(&[(PATIENT_ID, "Sam", "Synthetic")]),
        )
        .await;
        mount(&server, "GET", GRAPH_PATH, graph_ok()).await;

        let snapshot = client(&server).read().await.unwrap();
        assert_eq!(
            snapshot.current,
            GlucoseSample {
                timestamp: "11/05/2026 02:58:00 PM".to_owned(),
                factory_timestamp: "11/05/2026 07:58:00 PM".to_owned(),
                value_in_mg_per_dl: 123.0,
                trend_arrow: Some(4),
            }
        );
        assert_eq!(snapshot.graph.len(), 2);
        assert_eq!(snapshot.graph[0].value_in_mg_per_dl, 118.0);
        assert_eq!(snapshot.graph[0].trend_arrow, None);
        // Missing fields stay missing rather than being invented.
        assert_eq!(snapshot.graph[1].timestamp, "");
        assert_eq!(snapshot.graph[1].value_in_mg_per_dl, 0.0);
        assert_eq!(
            snapshot.active_sensors,
            [SensorRecord {
                device_id: "synthetic-device-0042".to_owned(),
                serial_number: "SYNTH0001".to_owned(),
                activation_epoch_seconds: 1_652_400_270,
                warmup_minutes: 60,
                product_type_code: 4,
            }]
        );
        assert_eq!(logins(&server).await, 1);
    }

    #[tokio::test]
    async fn requests_carry_hashed_account_id_and_version_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(LOGIN_PATH))
            .and(header("product", "llu.ios"))
            .and(header("version", "4.16.0"))
            .and(header("content-type", "application/json;charset=UTF-8"))
            .respond_with(login_ok(in_one_hour()))
            .mount(&server)
            .await;
        for (route, response) in [
            (
                CONNECTIONS_PATH,
                connections(&[(PATIENT_ID, "Sam", "Synthetic")]),
            ),
            (GRAPH_PATH, graph_ok()),
        ] {
            Mock::given(method("GET"))
                .and(path(route))
                .and(header("product", "llu.ios"))
                .and(header("version", "4.16.0"))
                .and(header("content-type", "application/json;charset=UTF-8"))
                .and(header("authorization", "Bearer synthetic-token"))
                .and(header("account-id", USER_ID_SHA256))
                .respond_with(response)
                .mount(&server)
                .await;
        }

        client(&server).read().await.unwrap();
        for request in server.received_requests().await.unwrap() {
            for (_, value) in request.headers.iter() {
                assert!(
                    !value.to_str().unwrap_or_default().contains(USER_ID),
                    "raw user id sent in a header"
                );
            }
        }
    }

    #[tokio::test]
    async fn region_redirect_is_followed_once() {
        let first = MockServer::start().await;
        let regional = MockServer::start().await;
        mount(&first, "POST", LOGIN_PATH, redirect_to("eu")).await;
        mount_session(&regional, in_one_hour()).await;
        mount(&regional, "GET", GRAPH_PATH, graph_ok()).await;
        let regional_uri = regional.uri();
        let client = client(&first).with_redirect_rewrite(move |resolved| {
            assert_eq!(resolved, "https://api-eu.libreview.io");
            regional_uri.clone()
        });

        client.read().await.unwrap();
        client.read().await.unwrap();
        assert_eq!(logins(&first).await, 1);
        assert_eq!(logins(&regional).await, 1);
        assert_eq!(hits(&first, "GET", GRAPH_PATH).await, 0);
        assert_eq!(hits(&regional, "GET", GRAPH_PATH).await, 2);
    }

    #[tokio::test]
    async fn a_second_redirect_is_refused() {
        let first = MockServer::start().await;
        let regional = MockServer::start().await;
        mount(&first, "POST", LOGIN_PATH, redirect_to("eu")).await;
        mount(&regional, "POST", LOGIN_PATH, redirect_to("us")).await;
        let regional_uri = regional.uri();
        let client = client(&first).with_redirect_rewrite(move |_| regional_uri.clone());

        let error = client.read().await.unwrap_err();
        assert_eq!(error.code, "upstream_unavailable");
        assert_eq!(logins(&first).await, 1);
        assert_eq!(logins(&regional).await, 1);
    }

    #[tokio::test]
    async fn a_redirect_to_an_invalid_region_is_refused() {
        let server = MockServer::start().await;
        mount(&server, "POST", LOGIN_PATH, redirect_to("evil.example/")).await;
        let resolved = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&resolved);
        let client = client(&server).with_redirect_rewrite(move |_| {
            flag.store(true, Ordering::SeqCst);
            String::new()
        });

        assert_eq!(
            client.read().await.unwrap_err().code,
            "upstream_unavailable"
        );
        assert!(!resolved.load(Ordering::SeqCst));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(valid_redirect_region("eu2"));
        assert!(!valid_redirect_region("EU"));
        assert!(!valid_redirect_region("e"));
        assert!(!valid_redirect_region("abcde"));
    }

    #[tokio::test]
    async fn requests_send_the_borrowed_crate_product_and_user_agent() {
        let server = MockServer::start().await;
        let expected = [
            ("product", "llu.ios"),
            ("accept", "application/json"),
            ("accept-encoding", "gzip"),
            ("cache-control", "no-cache"),
            ("accept-language", "en-US"),
        ];
        let mut login = Mock::given(method("POST")).and(path(LOGIN_PATH));
        for (name, value) in expected {
            login = login.and(header(name, value));
        }
        login
            .respond_with(login_ok(in_one_hour()))
            .mount(&server)
            .await;
        for (route, response) in [
            (
                CONNECTIONS_PATH,
                connections(&[(PATIENT_ID, "Sam", "Synthetic")]),
            ),
            (GRAPH_PATH, graph_ok()),
        ] {
            let mut mock = Mock::given(method("GET")).and(path(route));
            for (name, value) in expected {
                mock = mock.and(header(name, value));
            }
            mock.respond_with(response).mount(&server).await;
        }

        client(&server).read().await.unwrap();
        // wiremock's header matcher splits values on commas, and the
        // User-Agent contains one, so compare it on the recorded requests.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        for request in requests {
            assert_eq!(request.headers["user-agent"], USER_AGENT_STRING);
        }
    }

    #[tokio::test]
    async fn redirect_resolves_through_the_region_table() {
        let first = MockServer::start().await;
        let regional = MockServer::start().await;
        mount(&first, "POST", LOGIN_PATH, redirect_to("ru")).await;
        mount_session(&regional, in_one_hour()).await;
        mount(&regional, "GET", GRAPH_PATH, graph_ok()).await;
        let regional_uri = regional.uri();
        let client = client(&first).with_redirect_rewrite(move |resolved| {
            assert_eq!(resolved, "https://api.libreview.ru");
            regional_uri.clone()
        });

        client.read().await.unwrap();
        assert_eq!(logins(&first).await, 1);
        assert_eq!(logins(&regional).await, 1);
        assert_eq!(region_base_url("cn"), Some("https://api-cn.myfreestyle.cn"));
    }

    #[tokio::test]
    async fn a_well_formed_region_outside_the_table_is_refused() {
        let server = MockServer::start().await;
        mount(&server, "POST", LOGIN_PATH, redirect_to("zz9")).await;
        let rewritten = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&rewritten);
        let client = client(&server).with_redirect_rewrite(move |_| {
            flag.store(true, Ordering::SeqCst);
            String::new()
        });

        assert!(valid_redirect_region("zz9"));
        assert_eq!(
            client.read().await.unwrap_err().code,
            "upstream_unavailable"
        );
        assert!(!rewritten.load(Ordering::SeqCst));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn token_is_reused_across_reads() {
        let server = MockServer::start().await;
        mount_session(&server, in_one_hour()).await;
        mount(&server, "GET", GRAPH_PATH, graph_ok()).await;
        let client = client(&server);

        client.read().await.unwrap();
        client.read().await.unwrap();
        assert_eq!(logins(&server).await, 1);
        assert_eq!(hits(&server, "GET", CONNECTIONS_PATH).await, 1);
        assert_eq!(hits(&server, "GET", GRAPH_PATH).await, 2);
    }

    #[tokio::test]
    async fn an_expired_token_logs_in_again() {
        let server = MockServer::start().await;
        mount_session(&server, Utc::now().timestamp() - 10).await;
        mount(&server, "GET", GRAPH_PATH, graph_ok()).await;
        let client = client(&server);

        client.read().await.unwrap();
        client.read().await.unwrap();
        assert_eq!(logins(&server).await, 2);
    }

    #[tokio::test]
    async fn one_401_relogs_in_once_and_succeeds() {
        let server = MockServer::start().await;
        mount_session(&server, in_one_hour()).await;
        Mock::given(method("GET"))
            .and(path(GRAPH_PATH))
            .respond_with(ResponseTemplate::new(401))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        mount(&server, "GET", GRAPH_PATH, graph_ok()).await;

        client(&server).read().await.unwrap();
        assert_eq!(logins(&server).await, 2);
        assert_eq!(hits(&server, "GET", GRAPH_PATH).await, 2);
    }

    #[tokio::test]
    async fn repeated_401_is_session_expired_after_two_logins() {
        let server = MockServer::start().await;
        mount_session(&server, in_one_hour()).await;
        mount(&server, "GET", GRAPH_PATH, ResponseTemplate::new(401)).await;

        let error = client(&server).read().await.unwrap_err();
        assert_eq!(error.code, "session_expired");
        assert_eq!(logins(&server).await, 2);
    }

    /// The borrowed crate logs in again after any error, including throttling.
    #[tokio::test]
    async fn throttling_never_triggers_a_login() {
        let server = MockServer::start().await;
        mount_session(&server, in_one_hour()).await;
        mount(&server, "GET", GRAPH_PATH, ResponseTemplate::new(429)).await;
        let client = client(&server);

        for _ in 0..2 {
            assert_eq!(client.read().await.unwrap_err().code, "rate_limited");
        }
        assert_eq!(logins(&server).await, 1);
    }

    #[tokio::test]
    async fn server_errors_and_bad_bodies_never_trigger_a_login() {
        for response in [
            ResponseTemplate::new(500),
            ResponseTemplate::new(430),
            ResponseTemplate::new(200).set_body_string("not json"),
        ] {
            let server = MockServer::start().await;
            mount_session(&server, in_one_hour()).await;
            mount(&server, "GET", GRAPH_PATH, response).await;

            assert!(client(&server).read().await.is_err());
            assert_eq!(logins(&server).await, 1);
        }
    }

    #[tokio::test]
    async fn patient_selection_requires_a_unique_name_match() {
        let server = MockServer::start().await;
        mount(&server, "POST", LOGIN_PATH, login_ok(in_one_hour())).await;
        mount(
            &server,
            "GET",
            CONNECTIONS_PATH,
            connections(&[
                ("synthetic-patient-0001", "Sam", "Synthetic"),
                ("synthetic-patient-0002", "Pat", "Synthetic"),
            ]),
        )
        .await;
        mount(
            &server,
            "GET",
            "/llu/connections/synthetic-patient-0002/graph",
            graph_ok(),
        )
        .await;

        let error = client(&server).read().await.unwrap_err();
        assert_eq!(error.code, "patient_selection_required");

        let mut named = config(&server.uri());
        named.patient_name = Some("PAT synthetic".to_owned());
        LluClient::new(named).unwrap().read().await.unwrap();
    }

    /// Runs one read where login answers with `response`.
    async fn login_failure(response: ResponseTemplate) -> (FetchError, usize) {
        let server = MockServer::start().await;
        mount(&server, "POST", LOGIN_PATH, response).await;
        let error = client(&server).read().await.unwrap_err();
        (error, logins(&server).await)
    }

    /// Runs one read with a working login where the graph answers with
    /// `response`.
    async fn graph_failure(response: ResponseTemplate) -> (FetchError, usize) {
        let server = MockServer::start().await;
        mount_session(&server, in_one_hour()).await;
        mount(&server, "GET", GRAPH_PATH, response).await;
        let error = client(&server).read().await.unwrap_err();
        (error, logins(&server).await)
    }

    fn json_reply(status: u16, body: Value) -> ResponseTemplate {
        ResponseTemplate::new(status).set_body_json(body)
    }

    #[tokio::test]
    async fn login_status_2_is_authentication_failed() {
        let (error, logins) = login_failure(json_reply(200, json!({ "status": 2 }))).await;
        assert_eq!(error.code, "authentication_failed");
        assert_eq!(error.retry_after_seconds, None);
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn login_status_4_is_account_action_required() {
        let body = json!({
            "status": 4,
            "data": { "step": { "type": "verifyEmail", "componentName": "synthetic" } },
        });
        let (error, logins) = login_failure(json_reply(200, body)).await;
        assert_eq!(error.code, "account_action_required");
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn lockout_body_is_rate_limited_with_its_retry_time() {
        for body in [
            json!({ "status": 429, "data": { "code": 60, "data": { "failures": 3, "interval": 60, "lockout": 300 }, "message": "synthetic" } }),
            json!({ "status": 429, "data": { "lockout": 300 } }),
        ] {
            let (error, logins) = login_failure(json_reply(200, body)).await;
            assert_eq!(error.code, "rate_limited");
            assert_eq!(error.retry_after_seconds, Some(300));
            assert_eq!(logins, 1);
        }
        // No lockout time: default cooldown.
        let (error, logins) =
            login_failure(json_reply(200, json!({ "status": 429, "data": {} }))).await;
        assert_eq!(error.retry_after_seconds, Some(60));
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn http_throttling_uses_numeric_retry_after_or_the_default() {
        for (response, expected) in [
            (
                ResponseTemplate::new(429).insert_header("retry-after", "120"),
                120,
            ),
            (
                ResponseTemplate::new(430).insert_header("retry-after", "240"),
                240,
            ),
            (ResponseTemplate::new(430), 60),
            (
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"),
                60,
            ),
            (
                ResponseTemplate::new(429).insert_header("retry-after", "86400"),
                900,
            ),
        ] {
            let (error, logins) = graph_failure(response).await;
            assert_eq!(error.code, "rate_limited");
            assert_eq!(error.retry_after_seconds, Some(expected));
            assert_eq!(logins, 1);
        }
        // Throttling at login is mapped the same way.
        let (error, logins) =
            login_failure(ResponseTemplate::new(429).insert_header("retry-after", "120")).await;
        assert_eq!(error.code, "rate_limited");
        assert_eq!(error.retry_after_seconds, Some(120));
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn status_920_is_client_version_rejected() {
        let rejected = |minimum: &str| {
            json_reply(
                403,
                json!({ "status": 920, "data": { "minimumVersion": minimum } }),
            )
        };
        let (error, logins) = login_failure(rejected("4.17.0")).await;
        assert_eq!(error.code, "client_version_rejected");
        assert_eq!(error.retry_after_seconds, None);
        assert!(error.message.contains("minimum 4.17.0"));
        assert_eq!(logins, 1);

        for unsafe_version in [
            "4.17.0; see https://example.invalid",
            "",
            "...",
            "4.17.0-beta",
        ] {
            let (error, logins) = login_failure(rejected(unsafe_version)).await;
            assert_eq!(error.code, "client_version_rejected");
            assert!(!error.message.contains("minimum"), "{unsafe_version}");
            assert!(!error.message.contains("example"));
            assert_eq!(logins, 1);
        }

        let (error, logins) = graph_failure(rejected("4.17.0")).await;
        assert_eq!(error.code, "client_version_rejected");
        assert_eq!(logins, 1);

        // A 403 without status 920 is not a version problem.
        let (error, logins) = graph_failure(json_reply(403, json!({ "status": 1 }))).await;
        assert_eq!(error.code, "upstream_unavailable");
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn header_rejections_are_request_rejected_without_relogin() {
        for (status, body) in [
            (401, json!({ "message": "AccountIdMismatch" })),
            (400, json!({ "message": "RequiredHeaderMissing" })),
            (
                401,
                json!({ "status": 911, "error": { "message": "AccountIdMismatch" } }),
            ),
        ] {
            let (error, logins) = graph_failure(json_reply(status, body)).await;
            assert_eq!(error.code, "request_rejected");
            assert_eq!(error.retry_after_seconds, None);
            assert_eq!(logins, 1);
        }
        let (error, logins) = login_failure(json_reply(
            400,
            json!({ "message": "RequiredHeaderMissing" }),
        ))
        .await;
        assert_eq!(error.code, "request_rejected");
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn server_errors_are_upstream_unavailable() {
        for status in [500, 502, 503] {
            let (error, logins) = graph_failure(ResponseTemplate::new(status)).await;
            assert_eq!(error.code, "upstream_unavailable");
            assert_eq!(error.retry_after_seconds, Some(60));
            assert_eq!(logins, 1);
        }
        let (error, logins) = login_failure(ResponseTemplate::new(503)).await;
        assert_eq!(error.code, "upstream_unavailable");
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn successful_non_json_bodies_are_api_changed() {
        for body in ["<html>synthetic</html>", "", "\u{1}\u{2}binary"] {
            let (error, logins) =
                graph_failure(ResponseTemplate::new(200).set_body_string(body)).await;
            assert_eq!(error.code, "api_changed");
            assert_eq!(logins, 1);
        }
        let (error, logins) =
            login_failure(ResponseTemplate::new(200).set_body_string("synthetic text")).await;
        assert_eq!(error.code, "api_changed");
        assert_eq!(logins, 1);

        // Valid JSON of the wrong shape is not a retirement signal.
        let (error, logins) = graph_failure(json_reply(200, json!({ "status": 0 }))).await;
        assert_eq!(error.code, "upstream_unavailable");
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused() {
        let (error, logins) =
            graph_failure(
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_BODY_BYTES + 1]),
            )
            .await;
        assert_eq!(error.code, "upstream_unavailable");
        assert_eq!(logins, 1);
    }

    #[tokio::test]
    async fn error_messages_never_contain_response_bodies() {
        let marker = "synthetic-private-marker";
        for response in [
            json_reply(500, json!({ "message": marker })),
            json_reply(
                403,
                json!({ "status": 920, "data": { "minimumVersion": marker } }),
            ),
            ResponseTemplate::new(200).set_body_string(marker),
        ] {
            let (error, _) = graph_failure(response).await;
            assert!(!error.message.contains(marker));
        }
    }

    #[test]
    fn unfixable_rejections_latch_until_restart() {
        use crate::fetch::{Cooldown, cooldown_active, cooldown_until, latches_until_restart};
        use std::time::{Duration as StdDuration, Instant};
        let now = Instant::now();
        for error in [
            BAD_CREDENTIALS,
            version_rejected(Some("4.17.0")),
            REQUEST_REJECTED,
        ] {
            assert!(latches_until_restart(&error), "{}", error.code);
            assert!(error.message.contains("restart"), "{}", error.code);
            let latched = Cooldown {
                until: cooldown_until(&error, now),
                error,
            };
            assert!(latched.until.is_none());
            assert!(cooldown_active(
                &latched,
                now + StdDuration::from_secs(86_400)
            ));
        }
        assert!(version_rejected(None).message.contains("set-api-version"));
        assert!(REQUEST_REJECTED.message.contains("report"));
        // Temporary failures still expire.
        assert!(!latches_until_restart(&rate_limited(Some(120))));
        assert!(!latches_until_restart(&UNAVAILABLE));
    }

    #[tokio::test]
    async fn fetcher_makes_no_further_requests_after_a_latched_rejection() {
        use crate::fetch::Fetcher;
        for (status, body) in [
            (
                403,
                json!({ "status": 920, "data": { "minimumVersion": "4.17.0" } }),
            ),
            (400, json!({ "message": "RequiredHeaderMissing" })),
        ] {
            let server = MockServer::start().await;
            mount(&server, "POST", LOGIN_PATH, json_reply(status, body)).await;
            let fetcher = Fetcher::new(Box::new(client(&server)));
            let first = fetcher.fetch().await.unwrap_err();
            let second = fetcher.fetch().await.unwrap_err();
            assert_eq!(first.code, second.code);
            assert_eq!(second.retry_after_seconds, None);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
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
    fn production_base_urls_match_regions() {
        assert_eq!(region_base_url("global"), Some("https://api.libreview.io"));
        assert_eq!(region_base_url("eu2"), Some("https://api-eu2.libreview.io"));
        assert_eq!(region_base_url("unknown"), None);
        assert!(valid_patient_id(PATIENT_ID));
        assert!(!valid_patient_id("../escape"));
    }
}
