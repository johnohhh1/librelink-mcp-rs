//! Time-weighted glucose metrics (ADR 0003).
//!
//! Each sample stands for the time until the next one, capped at 15 minutes;
//! the last runs to the window end, under the same cap. Time beyond the cap is
//! uncovered and never attributed to a reading. Samples without a usable UTC
//! time are excluded. Pure functions; callers supply the clock.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

/// The longest time one sample may stand for.
pub(crate) const MAX_SAMPLE_MINUTES: i64 = 15;
/// GMI needs a requested window of at least this many days...
pub(crate) const GMI_MIN_DAYS: i64 = 14;
/// ...and at least this share of it covered.
pub(crate) const GMI_MIN_COVERAGE_PERCENT: i64 = 70;
/// International consensus on CGM time in range, which defines the bands and GMI.
pub(crate) const CONSENSUS_REFERENCE: &str = "https://pmc.ncbi.nlm.nih.gov/articles/PMC6973648/";

/// One reading: its UTC time if known, and its value in mg/dL.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub at: Option<DateTime<Utc>>,
    pub mg_dl: f64,
}

/// The consensus bands, with exact edges so every value is in exactly one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Band {
    /// v < 54
    VeryLow,
    /// 54 <= v < 70
    Low,
    /// 70 <= v <= 180
    Target,
    /// 180 < v <= 250
    High,
    /// v > 250
    VeryHigh,
}

pub(crate) fn band(mg_dl: f64) -> Band {
    if mg_dl < 54.0 {
        Band::VeryLow
    } else if mg_dl < 70.0 {
        Band::Low
    } else if mg_dl <= 180.0 {
        Band::Target
    } else if mg_dl <= 250.0 {
        Band::High
    } else {
        Band::VeryHigh
    }
}

/// Percent of covered time in each consensus band.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub(crate) struct BandPercentages {
    pub very_low: f64,
    pub low: f64,
    pub target: f64,
    pub high: f64,
    pub very_high: f64,
}

/// Figures that need covered time. Percentages are of covered time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, schemars::JsonSchema)]
pub(crate) struct Summary {
    pub average_glucose_mg_dl: f64,
    /// Duration-weighted population SD.
    pub standard_deviation_mg_dl: f64,
    pub coefficient_of_variation_percent: f64,
    pub consensus_bands_percent: BandPercentages,
    /// Time within the caller's configured target range, inclusive.
    pub target_range_percent: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GmiUnavailable {
    WindowTooShort,
    CoverageTooLow,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Gmi {
    Estimate(f64),
    Unavailable(GmiUnavailable),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TimeWeighted {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub covered_seconds: i64,
    pub window_seconds: i64,
    pub coverage_percent: f64,
    /// Samples inside the window with a UTC time and a valid value.
    pub samples_used: usize,
    /// Samples excluded because they have no usable UTC time.
    pub samples_without_utc: usize,
    /// `None` when no time is covered: insufficient data, nothing invented.
    pub summary: Option<Summary>,
    pub gmi: Gmi,
}

fn usable(mg_dl: f64) -> bool {
    mg_dl.is_finite() && mg_dl > 0.0
}

/// `(sample, seconds it stands for)`, in time order, for samples in `[from, to]`.
fn durations(samples: &[Sample], from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<(Sample, i64)> {
    let mut timed: Vec<(DateTime<Utc>, Sample)> = samples
        .iter()
        .filter(|sample| usable(sample.mg_dl))
        .filter_map(|sample| sample.at.map(|at| (at, *sample)))
        .filter(|(at, _)| *at >= from && *at <= to)
        .collect();
    timed.sort_by_key(|(at, _)| *at);
    let cap = Duration::minutes(MAX_SAMPLE_MINUTES);
    (0..timed.len())
        .map(|index| {
            let (at, sample) = timed[index];
            let until = timed.get(index + 1).map_or(to, |(next, _)| *next);
            (sample, (until - at).min(cap).num_seconds())
        })
        .collect()
}

fn percent(part: f64, whole: f64) -> f64 {
    part / whole * 100.0
}

/// Time-weighted metrics over `[from, to]`. `target` is the configured range,
/// inclusive at both ends.
pub(crate) fn time_weighted(
    samples: &[Sample],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    target: (f64, f64),
) -> TimeWeighted {
    let timed = durations(samples, from, to);
    let window_seconds = (to - from).num_seconds().max(0);
    let covered_seconds: i64 = timed.iter().map(|(_, seconds)| seconds).sum();
    let coverage_percent = if window_seconds > 0 {
        percent(covered_seconds as f64, window_seconds as f64)
    } else {
        0.0
    };
    let summary = (covered_seconds > 0).then(|| summarize(&timed, covered_seconds, target));
    let gmi = if window_seconds < Duration::days(GMI_MIN_DAYS).num_seconds() {
        Gmi::Unavailable(GmiUnavailable::WindowTooShort)
    } else if covered_seconds * 100 < window_seconds * GMI_MIN_COVERAGE_PERCENT {
        Gmi::Unavailable(GmiUnavailable::CoverageTooLow)
    } else {
        // Coverage of at least 70% implies covered time, hence a summary.
        Gmi::Estimate(gmi_percent(
            summary.map_or(f64::NAN, |summary| summary.average_glucose_mg_dl),
        ))
    };
    TimeWeighted {
        from,
        to,
        covered_seconds,
        window_seconds,
        coverage_percent,
        samples_used: timed.len(),
        samples_without_utc: samples.iter().filter(|sample| sample.at.is_none()).count(),
        summary,
        gmi,
    }
}

fn summarize(timed: &[(Sample, i64)], covered_seconds: i64, (low, high): (f64, f64)) -> Summary {
    let covered = covered_seconds as f64;
    let weighted = |value: &dyn Fn(&Sample) -> f64| -> f64 {
        timed
            .iter()
            .map(|(sample, seconds)| value(sample) * *seconds as f64)
            .sum::<f64>()
            / covered
    };
    let mean = weighted(&|sample| sample.mg_dl);
    let variance = weighted(&|sample| (sample.mg_dl - mean).powi(2));
    let standard_deviation = variance.sqrt();
    let share = |keep: &dyn Fn(f64) -> bool| -> f64 {
        let seconds: i64 = timed
            .iter()
            .filter(|(sample, _)| keep(sample.mg_dl))
            .map(|(_, seconds)| seconds)
            .sum();
        percent(seconds as f64, covered)
    };
    let in_band = |wanted: Band| move |mg_dl: f64| band(mg_dl) == wanted;
    Summary {
        average_glucose_mg_dl: mean,
        standard_deviation_mg_dl: standard_deviation,
        coefficient_of_variation_percent: percent(standard_deviation, mean),
        consensus_bands_percent: BandPercentages {
            very_low: share(&in_band(Band::VeryLow)),
            low: share(&in_band(Band::Low)),
            target: share(&in_band(Band::Target)),
            high: share(&in_band(Band::High)),
            very_high: share(&in_band(Band::VeryHigh)),
        },
        target_range_percent: share(&|mg_dl| mg_dl >= low && mg_dl <= high),
    }
}

/// GMI (%) = 3.31 + 0.02392 × mean glucose in mg/dL.
pub(crate) fn gmi_percent(mean_mg_dl: f64) -> f64 {
    3.31 + 0.02392 * mean_mg_dl
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: (f64, f64) = (70.0, 180.0);

    fn start() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn at_minute(minute: i64, mg_dl: f64) -> Sample {
        Sample {
            at: Some(start() + Duration::minutes(minute)),
            mg_dl,
        }
    }

    /// A constant series every 15 minutes covering exactly the last
    /// `covered_seconds` of `[start, start + window_seconds]`.
    fn covering_the_last(window_seconds: i64, covered_seconds: i64, mg_dl: f64) -> TimeWeighted {
        let to = start() + Duration::seconds(window_seconds);
        let first = to - Duration::seconds(covered_seconds);
        let samples: Vec<Sample> = (0..)
            .map(|step| first + Duration::minutes(MAX_SAMPLE_MINUTES * step))
            .take_while(|at| *at < to)
            .map(|at| Sample {
                at: Some(at),
                mg_dl,
            })
            .collect();
        time_weighted(&samples, start(), to, TARGET)
    }

    fn close(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn dense_samples_do_not_outweigh_the_time_they_cover() {
        // 12 hours every 15 minutes in range, then 1 hour every minute high.
        let mut samples: Vec<Sample> = (0..48).map(|step| at_minute(15 * step, 120.0)).collect();
        samples.extend((720..780).map(|minute| at_minute(minute, 220.0)));
        let to = start() + Duration::hours(13);
        let metrics = time_weighted(&samples, start(), to, TARGET);

        assert_eq!(metrics.covered_seconds, 13 * 3600);
        assert!(close(metrics.coverage_percent, 100.0));
        let bands = metrics.summary.unwrap().consensus_bands_percent;
        assert!(close(bands.high, 100.0 / 13.0), "{bands:?}");
        assert!(close(bands.target, 1200.0 / 13.0), "{bands:?}");

        let sample_high = 60.0 / samples.len() as f64 * 100.0;
        assert!(sample_high > 55.0, "{sample_high}");
        assert!(sample_high > 7.0 * bands.high);
    }

    #[test]
    fn a_long_gap_counts_only_the_15_minute_cap() {
        // Every 15 minutes for the first hour, nothing for 2 hours, then
        // every 15 minutes for the last hour of a 4-hour window.
        let samples: Vec<Sample> = [0, 15, 30, 45, 60, 180, 195, 210, 225]
            .into_iter()
            .map(|minute| at_minute(minute, 100.0))
            .collect();
        let to = start() + Duration::hours(4);
        let metrics = time_weighted(&samples, start(), to, TARGET);
        // 60 + 15 (the sample before the gap) + 60 minutes.
        assert_eq!(metrics.covered_seconds, 135 * 60);
        assert!(close(metrics.coverage_percent, 56.25));
    }

    #[test]
    fn the_last_sample_runs_to_the_window_end_under_the_cap() {
        let to = start() + Duration::minutes(60);
        let near_end = time_weighted(&[at_minute(55, 100.0)], start(), to, TARGET);
        assert_eq!(near_end.covered_seconds, 5 * 60);
        let far_from_end = time_weighted(&[at_minute(10, 100.0)], start(), to, TARGET);
        assert_eq!(far_from_end.covered_seconds, 15 * 60);
    }

    #[test]
    fn band_edges_are_exact() {
        let cases = [
            (53.9, Band::VeryLow),
            (54.0, Band::Low),
            (69.5, Band::Low),
            (70.0, Band::Target),
            (180.0, Band::Target),
            (180.4, Band::High),
            (250.0, Band::High),
            (250.1, Band::VeryHigh),
        ];
        for (value, expected) in cases {
            assert_eq!(band(value), expected, "{value}");
        }

        // Eight samples of equal duration: each is an eighth of covered time.
        let samples: Vec<Sample> = cases
            .iter()
            .enumerate()
            .map(|(index, (value, _))| at_minute(15 * index as i64, *value))
            .collect();
        let to = start() + Duration::minutes(15 * 8);
        let summary = time_weighted(&samples, start(), to, (54.0, 180.0))
            .summary
            .unwrap();
        let bands = summary.consensus_bands_percent;
        assert!(close(bands.very_low, 12.5));
        assert!(close(bands.low, 25.0));
        assert!(close(bands.target, 25.0));
        assert!(close(bands.high, 25.0));
        assert!(close(bands.very_high, 12.5));
        // The configured range is inclusive: 54, 69.5, 70, and 180.
        assert!(close(summary.target_range_percent, 50.0));
    }

    #[test]
    fn mean_sd_and_cv_are_weighted_by_duration() {
        // 100 for 45 minutes, then 200 for 15 minutes.
        let samples = [
            at_minute(0, 100.0),
            at_minute(15, 100.0),
            at_minute(30, 100.0),
            at_minute(45, 200.0),
        ];
        let to = start() + Duration::minutes(60);
        let summary = time_weighted(&samples, start(), to, TARGET)
            .summary
            .unwrap();
        assert!(close(summary.average_glucose_mg_dl, 125.0));
        // Population SD: sqrt(0.75 × 25² + 0.25 × 75²) = sqrt(1875).
        assert!(close(summary.standard_deviation_mg_dl, 1875f64.sqrt()));
        assert!(close(
            summary.coefficient_of_variation_percent,
            1875f64.sqrt() / 125.0 * 100.0
        ));
    }

    #[test]
    fn gmi_needs_14_days_and_70_percent_coverage() {
        const DAY: i64 = 24 * 3600;
        // 13.9 days, fully covered.
        let short = 139 * DAY / 10;
        let metrics = covering_the_last(short, short, 154.0);
        assert!(close(metrics.coverage_percent, 100.0));
        assert_eq!(
            metrics.gmi,
            Gmi::Unavailable(GmiUnavailable::WindowTooShort)
        );

        // 14 days at 69.9%.
        let window = 14 * DAY;
        let metrics = covering_the_last(window, window * 699 / 1000, 154.0);
        assert!(metrics.coverage_percent < 70.0 && metrics.coverage_percent > 69.8);
        assert_eq!(
            metrics.gmi,
            Gmi::Unavailable(GmiUnavailable::CoverageTooLow)
        );

        // 14 days at exactly 70%.
        let metrics = covering_the_last(window, window * 7 / 10, 154.0);
        assert_eq!(metrics.covered_seconds * 10, window * 7);
        assert_eq!(metrics.gmi, Gmi::Estimate(3.31 + 0.02392 * 154.0));
        match metrics.gmi {
            Gmi::Estimate(value) => assert!(close(value, 6.99368)),
            Gmi::Unavailable(reason) => panic!("{reason:?}"),
        }
    }

    #[test]
    fn no_usable_samples_is_insufficient_data() {
        let to = start() + Duration::days(14);
        let empty = time_weighted(&[], start(), to, TARGET);
        assert_eq!(empty.summary, None);
        assert_eq!(empty.covered_seconds, 0);
        assert_eq!(empty.coverage_percent, 0.0);
        assert_eq!(empty.gmi, Gmi::Unavailable(GmiUnavailable::CoverageTooLow));

        let no_utc: Vec<Sample> = (0..96)
            .map(|_| Sample {
                at: None,
                mg_dl: 120.0,
            })
            .collect();
        let metrics = time_weighted(&no_utc, start(), to, TARGET);
        assert_eq!(metrics.summary, None);
        assert_eq!(metrics.samples_used, 0);
        assert_eq!(metrics.samples_without_utc, 96);
        assert_eq!(metrics.covered_seconds, 0);
    }

    #[test]
    fn samples_outside_the_window_or_invalid_are_not_counted() {
        let to = start() + Duration::minutes(60);
        let samples = [
            at_minute(-5, 300.0),
            at_minute(0, 100.0),
            at_minute(15, f64::NAN),
            at_minute(30, 0.0),
            at_minute(75, 300.0),
        ];
        let metrics = time_weighted(&samples, start(), to, TARGET);
        assert_eq!(metrics.samples_used, 1);
        assert_eq!(metrics.covered_seconds, 15 * 60);
        assert!(close(metrics.summary.unwrap().average_glucose_mg_dl, 100.0));
    }
}
