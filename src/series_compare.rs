//! Metrics derived from a report's time series for `--compare`, and the
//! `--compare-window` that restricts them to a steady-state part of the run.
//!
//! Whole-run totals average over warm-up and ramp-down and hide short
//! instability. These metrics summarise the per-interval points instead: how
//! steady throughput was and how bad the worst interval was. Each one is a
//! summary of one report, so two reports with different interval lengths can
//! be compared. METHODOLOGY.md section 6.5 defines them.

use crate::config::parse_human_duration;
use crate::error::{ConfigError, Result, WsBlastError};
use crate::timeseries::{TimeSeries, TimeSeriesPoint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// Prefix of every metric derived from the time series.
pub(crate) const SERIES_PREFIX: &str = "series.";

/// Interval boundaries are sampled by a timer, so a point can start or end
/// slightly after its nominal boundary. Edges and lengths are compared with
/// this fraction of the interval as slack, the same fraction below which the
/// series folds a final sliver into the interval before it.
const EDGE_SLACK: f64 = 1.0 / 20.0;

/// The part of a run, in time since the run started, that the time-series
/// metrics are computed over. `None` on either side leaves it open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompareWindow {
    pub(crate) start: Option<Duration>,
    pub(crate) end: Option<Duration>,
}

impl CompareWindow {
    /// Parses `START..END`, where each side is a duration in the form
    /// `--duration` accepts (`10s`, `500ms`, `2m`) and either side may be
    /// left out: `10s..` skips the first 10 seconds and `..50s` drops
    /// everything after 50 seconds.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidOption`] when the value has no `..`,
    /// both sides are empty, a side is not a non-negative duration, or the
    /// start is not before the end.
    pub(crate) fn parse(value: &str) -> Result<Self> {
        let invalid = |reason: String| {
            WsBlastError::Config(ConfigError::InvalidOption(format!(
                "--compare-window {value:?}: {reason}"
            )))
        };
        let Some((start, end)) = value.trim().split_once("..") else {
            return Err(invalid(
                "expected START..END, for example 10s..50s, 10s.. or ..50s".into(),
            ));
        };
        let side = |text: &str, name: &str| -> Result<Option<Duration>> {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            parse_human_duration(text).map(Some).map_err(|_| {
                invalid(format!(
                    "{name} {text:?} is not a non-negative duration such as 10s or 500ms"
                ))
            })
        };
        let window = Self {
            start: side(start, "start")?,
            end: side(end, "end")?,
        };
        match (window.start, window.end) {
            (None, None) => Err(invalid(
                "give a start, an end or both, for example 10s..50s".into(),
            )),
            (start, Some(end)) if start.unwrap_or_default() >= end => {
                Err(invalid("the start must be before the end".into()))
            }
            _ => Ok(window),
        }
    }

    fn start_secs(&self) -> f64 {
        self.start.map_or(0.0, |start| start.as_secs_f64())
    }

    fn end_secs(&self) -> f64 {
        self.end.map_or(f64::INFINITY, |end| end.as_secs_f64())
    }

    /// Whether the whole interval of `point` lies inside the window.
    fn contains(&self, point: &TimeSeriesPoint, slack: f64) -> bool {
        point.start_secs >= self.start_secs() - slack
            && point.start_secs + point.duration_secs <= self.end_secs() + slack
    }
}

impl fmt::Display for CompareWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(start) = self.start {
            write!(f, "{}s", start.as_secs_f64())?;
        }
        f.write_str("..")?;
        if let Some(end) = self.end {
            write!(f, "{}s", end.as_secs_f64())?;
        }
        Ok(())
    }
}

/// The points of `series` that `window` keeps: every point without a window.
pub(crate) fn window_points<'a>(
    series: &'a TimeSeries,
    window: Option<&'a CompareWindow>,
) -> impl Iterator<Item = &'a TimeSeriesPoint> + 'a {
    let slack = series.interval_secs.max(0.0) * EDGE_SLACK;
    series
        .points
        .iter()
        .filter(move |point| window.is_none_or(|window| window.contains(point, slack)))
}

/// The time-series metrics of one report. A metric that is undefined for
/// the points in the window is `None`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SeriesSummary {
    /// Points inside the window.
    pub(crate) points: usize,
    pub(crate) throughput_mean: Option<f64>,
    pub(crate) throughput_cv: Option<f64>,
    pub(crate) p99_median_us: Option<f64>,
    pub(crate) p99_worst_us: Option<f64>,
    pub(crate) error_intervals: u64,
    pub(crate) max_interval_errors: u64,
}

impl SeriesSummary {
    pub(crate) fn of(series: &TimeSeries, window: Option<&CompareWindow>) -> Self {
        let points: Vec<&TimeSeriesPoint> = window_points(series, window).collect();

        // A full-length point covers a whole interval. The shorter last
        // point of a run or of a scenario phase is left out of the
        // throughput metrics: it often holds the ramp-down, and its rate is
        // computed over too little time to compare.
        let min_full = series.interval_secs * (1.0 - EDGE_SLACK);
        let full: Vec<(f64, f64)> = points
            .iter()
            .filter(|point| point.duration_secs >= min_full && point.duration_secs > 0.0)
            .map(|point| (point.throughput_msg_per_sec, point.duration_secs))
            .collect();
        let total_secs: f64 = full.iter().map(|(_, secs)| secs).sum();
        let throughput_mean = (total_secs > 0.0)
            .then(|| full.iter().map(|(rate, secs)| rate * secs).sum::<f64>() / total_secs);
        let throughput_cv = throughput_mean
            .filter(|mean| *mean > 0.0 && full.len() >= 2)
            .map(|mean| {
                let variance = full
                    .iter()
                    .map(|(rate, secs)| secs * (rate - mean).powi(2))
                    .sum::<f64>()
                    / total_secs;
                variance.sqrt() / mean
            });

        let mut p99s: Vec<u64> = points
            .iter()
            .filter(|point| point.latency.count > 0)
            .map(|point| point.latency.p99_us)
            .collect();
        p99s.sort_unstable();
        let p99_median_us = match p99s.len() {
            0 => None,
            n if n % 2 == 1 => Some(p99s[n / 2] as f64),
            n => Some((p99s[n / 2 - 1] as f64 + p99s[n / 2] as f64) / 2.0),
        };
        let p99_worst_us = p99s.last().map(|worst| *worst as f64);

        Self {
            points: points.len(),
            throughput_mean,
            throughput_cv,
            p99_median_us,
            p99_worst_us,
            error_intervals: points.iter().filter(|point| point.errors > 0).count() as u64,
            max_interval_errors: points.iter().map(|point| point.errors).max().unwrap_or(0),
        }
    }

    /// Adds the defined metrics to a comparison map under `series.` names.
    /// A report without points in the window adds nothing.
    pub(crate) fn insert_into(&self, map: &mut BTreeMap<String, f64>) {
        if self.points == 0 {
            return;
        }
        let mut insert = |name: &str, value: Option<f64>| {
            if let Some(value) = value {
                map.insert(format!("{SERIES_PREFIX}{name}"), value);
            }
        };
        insert("throughput_mean", self.throughput_mean);
        insert("throughput_cv", self.throughput_cv);
        insert("p99_median_us", self.p99_median_us);
        insert("p99_worst_us", self.p99_worst_us);
        insert("error_intervals", Some(self.error_intervals as f64));
        insert("max_interval_errors", Some(self.max_interval_errors as f64));
    }
}

/// Whether a higher value of the time-series metric `metric` is better.
pub(crate) fn higher_is_better(metric: &str) -> bool {
    metric == "series.throughput_mean"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeseries::IntervalLatency;

    fn point(start: f64, rate: f64, p99: u64, errors: u64) -> TimeSeriesPoint {
        TimeSeriesPoint {
            start_secs: start,
            duration_secs: 1.0,
            messages_sent: rate as u64,
            messages_recv: rate as u64,
            errors,
            throughput_msg_per_sec: rate,
            latency: IntervalLatency {
                count: u64::from(p99 > 0) * 100,
                p50_us: p99 / 2,
                p90_us: p99,
                p99_us: p99,
                max_us: p99,
            },
            ..TimeSeriesPoint::default()
        }
    }

    fn series(points: Vec<TimeSeriesPoint>) -> TimeSeries {
        TimeSeries {
            interval_secs: 1.0,
            latency_basis: "rtt".into(),
            points,
        }
    }

    fn steady(n: usize) -> Vec<TimeSeriesPoint> {
        (0..n).map(|i| point(i as f64, 1_000.0, 2_000, 0)).collect()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn window(value: &str) -> CompareWindow {
        CompareWindow::parse(value).unwrap()
    }

    #[test]
    fn steady_series_has_no_variation() {
        let summary = SeriesSummary::of(&series(steady(10)), None);
        assert_eq!(summary.points, 10);
        assert_eq!(summary.throughput_mean, Some(1_000.0));
        assert_eq!(summary.throughput_cv, Some(0.0));
        assert_eq!(summary.p99_median_us, Some(2_000.0));
        assert_eq!(summary.p99_worst_us, Some(2_000.0));
        assert_eq!(
            (summary.error_intervals, summary.max_interval_errors),
            (0, 0)
        );
    }

    #[test]
    fn sawtooth_throughput_has_the_same_mean_and_a_high_cv() {
        let points = (0..10)
            .map(|i| point(i as f64, if i % 2 == 0 { 500.0 } else { 1_500.0 }, 2_000, 0))
            .collect();
        let summary = SeriesSummary::of(&series(points), None);
        assert_eq!(summary.throughput_mean, Some(1_000.0));
        // Every point is 500 away from the mean of 1000.
        assert!(close(summary.throughput_cv.unwrap(), 0.5));
    }

    #[test]
    fn a_stall_shows_as_a_throughput_gap_and_a_worst_interval() {
        let mut points = steady(10);
        points[4] = point(4.0, 0.0, 0, 0);
        points[5] = point(5.0, 1_000.0, 2_000_000, 0);
        let summary = SeriesSummary::of(&series(points), None);
        assert!(close(summary.throughput_mean.unwrap(), 900.0));
        // Nine points at 1000 and one at 0 around a mean of 900: the standard
        // deviation is 300.
        assert!(close(summary.throughput_cv.unwrap(), 300.0 / 900.0));
        // The stalled interval has no replies, so nine intervals have a p99.
        assert_eq!(summary.p99_median_us, Some(2_000.0));
        assert_eq!(summary.p99_worst_us, Some(2_000_000.0));
    }

    #[test]
    fn an_error_burst_is_counted_by_interval() {
        let mut points = steady(10);
        points[6].errors = 40;
        points[7].errors = 3;
        let summary = SeriesSummary::of(&series(points), None);
        assert_eq!(summary.error_intervals, 2);
        assert_eq!(summary.max_interval_errors, 40);
    }

    #[test]
    fn median_of_an_even_count_is_the_mean_of_the_middle_two() {
        let points = [1_000, 4_000, 2_000, 3_000]
            .iter()
            .enumerate()
            .map(|(i, p99)| point(i as f64, 1_000.0, *p99, 0))
            .collect();
        let summary = SeriesSummary::of(&series(points), None);
        assert_eq!(summary.p99_median_us, Some(2_500.0));
        assert_eq!(summary.p99_worst_us, Some(4_000.0));
    }

    #[test]
    fn a_single_point_has_a_mean_but_no_cv() {
        let summary = SeriesSummary::of(&series(steady(1)), None);
        assert_eq!(summary.throughput_mean, Some(1_000.0));
        assert_eq!(summary.throughput_cv, None);
        assert_eq!(summary.p99_median_us, Some(2_000.0));
        let mut map = BTreeMap::new();
        summary.insert_into(&mut map);
        assert!(!map.contains_key("series.throughput_cv"));
        assert_eq!(map["series.throughput_mean"], 1_000.0);
    }

    #[test]
    fn zero_throughput_has_no_cv() {
        let points = (0..5).map(|i| point(f64::from(i), 0.0, 0, 1)).collect();
        let summary = SeriesSummary::of(&series(points), None);
        assert_eq!(summary.throughput_mean, Some(0.0));
        assert_eq!(summary.throughput_cv, None);
        assert_eq!(summary.p99_median_us, None);
        assert_eq!(summary.p99_worst_us, None);
        assert_eq!(summary.error_intervals, 5);
    }

    #[test]
    fn short_points_are_left_out_of_throughput_but_not_of_latency_or_errors() {
        let mut points = steady(4);
        points.push(TimeSeriesPoint {
            duration_secs: 0.3,
            ..point(4.0, 10.0, 9_000, 2)
        });
        let summary = SeriesSummary::of(&series(points), None);
        assert_eq!(summary.throughput_mean, Some(1_000.0));
        assert_eq!(summary.throughput_cv, Some(0.0));
        assert_eq!(summary.p99_worst_us, Some(9_000.0));
        assert_eq!(summary.error_intervals, 1);
    }

    #[test]
    fn throughput_mean_is_weighted_by_duration() {
        let points = vec![
            point(0.0, 1_000.0, 0, 0),
            TimeSeriesPoint {
                duration_secs: 1.05,
                ..point(1.0, 2_000.0, 0, 0)
            },
        ];
        let summary = SeriesSummary::of(&series(points), None);
        let mean = (1_000.0 + 2_000.0 * 1.05) / 2.05;
        assert!(close(summary.throughput_mean.unwrap(), mean));
    }

    #[test]
    fn the_window_keeps_only_points_fully_inside_it() {
        // Warm-up at 100 msg/s for 3 s, steady at 1000, ramp-down at 200.
        let mut points: Vec<TimeSeriesPoint> = (0..10)
            .map(|i| {
                let rate = match i {
                    0..=2 => 100.0,
                    9 => 200.0,
                    _ => 1_000.0,
                };
                // Boundaries are sampled a little late.
                point(f64::from(i) + 0.002, rate, 2_000, 0)
            })
            .collect();
        points[0].start_secs = 0.0;
        let series = series(points);

        let whole = SeriesSummary::of(&series, None);
        assert!(whole.throughput_mean.unwrap() < 800.0);

        let steady = SeriesSummary::of(&series, Some(&window("3s..9s")));
        assert_eq!(steady.points, 6);
        assert_eq!(steady.throughput_mean, Some(1_000.0));
        assert_eq!(steady.throughput_cv, Some(0.0));

        let skip_warmup = SeriesSummary::of(&series, Some(&window("3s..")));
        assert_eq!(skip_warmup.points, 7);
        let drop_tail = SeriesSummary::of(&series, Some(&window("..9s")));
        assert_eq!(drop_tail.points, 9);
        // 3.5 s to 9 s holds only the intervals starting at 4 to 8.
        let half = SeriesSummary::of(&series, Some(&window("3500ms..9s")));
        assert_eq!(half.points, 5);
    }

    #[test]
    fn an_empty_window_adds_no_metrics() {
        let summary = SeriesSummary::of(&series(steady(10)), Some(&window("20s..30s")));
        assert_eq!(summary, SeriesSummary::default());
        let mut map = BTreeMap::new();
        summary.insert_into(&mut map);
        assert!(map.is_empty());

        let empty = SeriesSummary::of(&TimeSeries::default(), None);
        let mut map = BTreeMap::new();
        empty.insert_into(&mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn windows_parse_either_side_open() {
        assert_eq!(
            window("10s..50s"),
            CompareWindow {
                start: Some(Duration::from_secs(10)),
                end: Some(Duration::from_secs(50)),
            }
        );
        assert_eq!(window("10s..").end, None);
        assert_eq!(window("..50s").start, None);
        assert_eq!(window(" 500ms .. 2m ").end, Some(Duration::from_secs(120)));
        assert_eq!(window("0s..1s").start, Some(Duration::ZERO));
        assert_eq!(window("10s..50s").to_string(), "10s..50s");
        assert_eq!(window("1.5s..").to_string(), "1.5s..");
        assert_eq!(window("..50s").to_string(), "..50s");
    }

    #[test]
    fn invalid_windows_are_rejected() {
        for value in [
            "", "10s", "..", "50s..10s", "10s..10s", "..0s", "-1s..5s", "1s..-5s", "abc..5s",
            "1s..5x",
        ] {
            let err = CompareWindow::parse(value).expect_err(value);
            assert!(
                matches!(err, WsBlastError::Config(ConfigError::InvalidOption(_))),
                "{value}: {err}"
            );
            assert!(err.to_string().contains("--compare-window"), "{err}");
        }
    }

    #[test]
    fn directions() {
        assert!(higher_is_better("series.throughput_mean"));
        for metric in [
            "series.throughput_cv",
            "series.p99_median_us",
            "series.p99_worst_us",
            "series.error_intervals",
            "series.max_interval_errors",
        ] {
            assert!(!higher_is_better(metric), "{metric}");
        }
    }
}
