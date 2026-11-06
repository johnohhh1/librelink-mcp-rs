use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use serde::Serialize;

pub(crate) const STALE_AFTER_MINUTES: i64 = 15;
const FUTURE_TOLERANCE_MINUTES: i64 = 2;
const TIMESTAMP_CONFLICT_MINUTES: i64 = 2;
const NAIVE_API_FORMAT: &str = "%m/%d/%Y %I:%M:%S %p";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Freshness {
    Recent,
    Stale,
    Unknown,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimeBasis {
    OffsetKnown,
    FactoryUtcInferred,
    OffsetMissing,
    Conflicting,
    Invalid,
    Missing,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TimeAssessment {
    pub source_timestamp: Option<String>,
    pub factory_timestamp: Option<String>,
    pub retrieved_at_utc: DateTime<Utc>,
    pub observed_at_utc: Option<DateTime<Utc>>,
    pub time_basis: TimeBasis,
    pub freshness: Freshness,
    /// A difference between clock strings, not proof of either time zone.
    pub display_minus_factory_minutes: Option<i64>,
}

fn present(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_offset(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn parse_naive(raw: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(raw, NAIVE_API_FORMAT).ok()
}

fn freshness_at(observed: DateTime<Utc>, now: DateTime<Utc>) -> Freshness {
    let age = now.signed_duration_since(observed);
    if age < Duration::minutes(-FUTURE_TOLERANCE_MINUTES) {
        Freshness::Unknown
    } else if age > Duration::minutes(STALE_AFTER_MINUTES) {
        Freshness::Stale
    } else {
        Freshness::Recent
    }
}

fn infer_factory_utc(
    display: NaiveDateTime,
    factory: NaiveDateTime,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let gap_seconds = display.signed_duration_since(factory).num_seconds();
    // Real timezone offsets are quarter-hour aligned and within this range.
    // This is a consistency check, not a timezone assertion from the API.
    if gap_seconds % (15 * 60) != 0 || gap_seconds.abs() > 14 * 60 * 60 {
        return None;
    }
    let observed = DateTime::<Utc>::from_naive_utc_and_offset(factory, Utc);
    (observed <= now).then_some(observed)
}

pub(crate) fn assess_time(
    display_raw: Option<&str>,
    factory_raw: Option<&str>,
    now: DateTime<Utc>,
) -> TimeAssessment {
    assess_time_at(display_raw, factory_raw, now, now)
}

pub(crate) fn assess_time_at(
    display_raw: Option<&str>,
    factory_raw: Option<&str>,
    retrieved_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> TimeAssessment {
    let display = present(display_raw);
    let factory = present(factory_raw);
    let display_offset = display.and_then(parse_offset);
    let factory_offset = factory.and_then(parse_offset);
    let display_naive = display.and_then(parse_naive);
    let factory_naive = factory.and_then(parse_naive);
    let display_minus_factory_minutes = match (display_naive, factory_naive) {
        (Some(display), Some(factory)) => {
            Some(display.signed_duration_since(factory).num_minutes())
        }
        _ => None,
    };

    let (observed_at_utc, time_basis, freshness) = match (display_offset, factory_offset) {
        (Some(display), Some(factory))
            if display.signed_duration_since(factory).num_seconds().abs()
                > Duration::minutes(TIMESTAMP_CONFLICT_MINUTES).num_seconds() =>
        {
            (None, TimeBasis::Conflicting, Freshness::Unknown)
        }
        (Some(display), _) => (
            Some(display),
            TimeBasis::OffsetKnown,
            freshness_at(display, now),
        ),
        (None, Some(factory)) => (
            Some(factory),
            TimeBasis::OffsetKnown,
            freshness_at(factory, now),
        ),
        (None, None) if display.is_none() && factory.is_none() => {
            (None, TimeBasis::Missing, Freshness::Missing)
        }
        (None, None) if display_naive.is_some() && factory_naive.is_some() => {
            let inferred = infer_factory_utc(display_naive.unwrap(), factory_naive.unwrap(), now);
            match inferred {
                Some(observed) => (
                    Some(observed),
                    TimeBasis::FactoryUtcInferred,
                    freshness_at(observed, now),
                ),
                None => (None, TimeBasis::OffsetMissing, Freshness::Unknown),
            }
        }
        (None, None) if display_naive.is_some() || factory_naive.is_some() => {
            (None, TimeBasis::OffsetMissing, Freshness::Unknown)
        }
        (None, None) => (None, TimeBasis::Invalid, Freshness::Unknown),
    };

    TimeAssessment {
        source_timestamp: display.map(str::to_owned),
        factory_timestamp: factory.map(str::to_owned),
        retrieved_at_utc: retrieved_at,
        observed_at_utc,
        time_basis,
        freshness,
        display_minus_factory_minutes,
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
    fn matching_api_clocks_infer_factory_utc() {
        let time = assess_time(
            Some("11/05/2026 02:58:00 PM"),
            Some("11/05/2026 07:58:00 PM"),
            now(),
        );
        assert_eq!(time.time_basis, TimeBasis::FactoryUtcInferred);
        assert_eq!(time.freshness, Freshness::Recent);
        assert_eq!(time.display_minus_factory_minutes, Some(-300));
        assert_eq!(
            time.observed_at_utc.unwrap().to_rfc3339(),
            "2026-11-05T19:58:00+00:00"
        );
    }

    #[test]
    fn summer_offset_and_dst_fallback_remain_unambiguous_via_factory_clock() {
        let summer_now = DateTime::parse_from_rfc3339("2026-05-20T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let summer = assess_time(
            Some("05/20/2026 11:58:00 AM"),
            Some("05/20/2026 09:58:00 AM"),
            summer_now,
        );
        assert_eq!(summer.time_basis, TimeBasis::FactoryUtcInferred);
        assert_eq!(summer.display_minus_factory_minutes, Some(120));

        let fallback_now = DateTime::parse_from_rfc3339("2026-11-01T07:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for factory in ["11/01/2026 05:30:00 AM", "11/01/2026 06:30:00 AM"] {
            let time = assess_time(Some("11/01/2026 01:30:00 AM"), Some(factory), fallback_now);
            assert_eq!(time.time_basis, TimeBasis::FactoryUtcInferred);
        }
    }

    #[test]
    fn invalid_offsets_and_future_factory_clock_are_not_inferred() {
        for (display, factory) in [
            ("11/05/2026 08:17:00 PM", "11/05/2026 08:00:00 PM"),
            ("11/06/2026 11:00:00 AM", "11/05/2026 08:00:00 PM"),
            ("11/05/2026 08:15:00 PM", "11/05/2026 08:15:00 PM"),
        ] {
            let time = assess_time(Some(display), Some(factory), now());
            assert_eq!(time.time_basis, TimeBasis::OffsetMissing);
            assert_eq!(time.freshness, Freshness::Unknown);
            assert!(time.observed_at_utc.is_none());
        }
    }

    #[test]
    fn cached_retrieval_time_does_not_refresh_freshness() {
        let retrieved = now();
        let later = retrieved + Duration::minutes(30);
        let time = assess_time_at(
            Some("11/05/2026 02:58:00 PM"),
            Some("11/05/2026 07:58:00 PM"),
            retrieved,
            later,
        );
        assert_eq!(time.retrieved_at_utc, retrieved);
        assert_eq!(time.freshness, Freshness::Stale);
    }

    #[test]
    fn offset_aware_clocks_can_be_recent_or_stale() {
        let recent = assess_time(Some("2026-11-05T15:56:00-04:00"), None, now());
        assert_eq!(recent.freshness, Freshness::Recent);
        assert_eq!(recent.time_basis, TimeBasis::OffsetKnown);
        let stale = assess_time(Some("2026-11-05T19:20:00Z"), None, now());
        assert_eq!(stale.freshness, Freshness::Stale);
        let future = assess_time(Some("2026-11-05T20:10:00Z"), None, now());
        assert_eq!(future.freshness, Freshness::Unknown);
    }

    #[test]
    fn conflicting_clocks_are_not_resolved_by_guessing() {
        let time = assess_time(
            Some("2026-11-05T19:58:00Z"),
            Some("2026-11-05T19:20:00Z"),
            now(),
        );
        assert_eq!(time.time_basis, TimeBasis::Conflicting);
        assert_eq!(time.freshness, Freshness::Unknown);
    }

    #[test]
    fn missing_invalid_and_dst_ambiguous_clocks_remain_explicit() {
        assert_eq!(assess_time(None, None, now()).freshness, Freshness::Missing);
        assert_eq!(
            assess_time(Some("not a date"), None, now()).time_basis,
            TimeBasis::Invalid
        );
        // This local wall-clock time occurs twice when clocks fall back.
        assert_eq!(
            assess_time(Some("11/01/2026 01:30:00 AM"), None, now()).time_basis,
            TimeBasis::OffsetMissing
        );
    }
}
