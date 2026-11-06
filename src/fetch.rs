//! Caching and failure cooldown in front of any `DataSource`.

use crate::source::{DataSource, Snapshot};
use chrono::{DateTime, Utc};
use std::borrow::Cow;
use std::ops::Deref;
use std::time::Duration as StdDuration;
// Tokio's clock equals the system monotonic clock at runtime and follows a
// paused test clock, so cache and cooldown timing are testable.
use tokio::sync::Mutex;
use tokio::time::Instant;

pub(crate) const CACHE_TTL: StdDuration = StdDuration::from_secs(60);
pub(crate) const FAILURE_COOLDOWN: StdDuration = StdDuration::from_secs(60);
const READ_TIMEOUT: StdDuration = StdDuration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FetchError {
    pub code: &'static str,
    /// Fixed text, or fixed text plus a validated detail. Never upstream bodies.
    pub message: Cow<'static, str>,
    pub retry_after_seconds: Option<u64>,
}

pub(crate) struct Cooldown {
    /// `None` latches the error until the server restarts.
    pub until: Option<Instant>,
    pub error: FetchError,
}

/// Errors that cannot fix themselves while the server keeps its current
/// configuration latch until restart instead of retrying:
/// - a rejected password: each retry is another failed login that moves the
///   account toward an Abbott lockout;
/// - a rejected client version: the version is read at startup;
/// - rejected request headers: a client bug that a retry repeats.
pub(crate) fn latches_until_restart(error: &FetchError) -> bool {
    matches!(
        error.code,
        "authentication_failed"
            | "client_version_rejected"
            | "request_rejected"
            | "patient_selection_changed"
    )
}

/// Local configuration problems (no credentials yet, bad stored settings)
/// never reached the network, so they do not start a cooldown; the next call
/// retries after the user runs the local setup command.
fn starts_cooldown(error: &FetchError) -> bool {
    error.code != "configuration_error"
}

pub(crate) fn cooldown_until(error: &FetchError, now: Instant) -> Option<Instant> {
    if latches_until_restart(error) {
        return None;
    }
    let seconds = error
        .retry_after_seconds
        .unwrap_or(FAILURE_COOLDOWN.as_secs());
    Some(now + StdDuration::from_secs(seconds))
}

pub(crate) fn cooldown_active(cooldown: &Cooldown, now: Instant) -> bool {
    cooldown.until.is_none_or(|until| now < until)
}

struct CacheEntry {
    snapshot: Snapshot,
    retrieved_at_utc: DateTime<Utc>,
    expires_at: Instant,
}

struct FetchState {
    cached: Option<CacheEntry>,
    cooldown: Option<Cooldown>,
}

/// A snapshot plus when it was retrieved. A cache hit keeps the original
/// retrieval time so it cannot make an old reading look current.
#[derive(Clone, Debug)]
pub(crate) struct Fetched {
    pub snapshot: Snapshot,
    pub retrieved_at_utc: DateTime<Utc>,
    pub cache_hit: bool,
}

impl Deref for Fetched {
    type Target = Snapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}

pub(crate) struct Fetcher {
    source: Box<dyn DataSource>,
    state: Mutex<FetchState>,
}

impl Fetcher {
    pub(crate) fn new(source: Box<dyn DataSource>) -> Self {
        Self {
            source,
            state: Mutex::new(FetchState {
                cached: None,
                cooldown: None,
            }),
        }
    }

    /// Serves from the cache, refuses during a cooldown, or reads once from
    /// the source. The lock is held across the read so concurrent tool calls
    /// cannot issue parallel upstream requests.
    pub(crate) async fn fetch(&self) -> Result<Fetched, FetchError> {
        let mut state = self.state.lock().await;
        let now = Instant::now();
        if let Some(cached) = state.cached.as_ref().filter(|entry| now < entry.expires_at) {
            return Ok(Fetched {
                snapshot: cached.snapshot.clone(),
                retrieved_at_utc: cached.retrieved_at_utc,
                cache_hit: true,
            });
        }
        if let Some(cooldown) = state
            .cooldown
            .as_ref()
            .filter(|entry| cooldown_active(entry, now))
        {
            let mut error = cooldown.error.clone();
            error.retry_after_seconds = cooldown
                .until
                .map(|until| until.duration_since(now).as_secs().max(1));
            return Err(error);
        }
        let snapshot = match tokio::time::timeout(READ_TIMEOUT, self.source.read()).await {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                if starts_cooldown(&error) {
                    state.cooldown = Some(Cooldown {
                        until: cooldown_until(&error, Instant::now()),
                        error: error.clone(),
                    });
                }
                return Err(error);
            }
            Err(_) => {
                let error = FetchError {
                    code: "timeout",
                    message: Cow::Borrowed("LibreLinkUp request timed out; retry later."),
                    retry_after_seconds: Some(FAILURE_COOLDOWN.as_secs()),
                };
                state.cooldown = Some(Cooldown {
                    until: Some(Instant::now() + FAILURE_COOLDOWN),
                    error: error.clone(),
                });
                return Err(error);
            }
        };
        let retrieved_at_utc = Utc::now();
        state.cached = Some(CacheEntry {
            snapshot: snapshot.clone(),
            retrieved_at_utc,
            expires_at: Instant::now() + CACHE_TTL,
        });
        state.cooldown = None;
        Ok(Fetched {
            snapshot,
            retrieved_at_utc,
            cache_hit: false,
        })
    }
}
