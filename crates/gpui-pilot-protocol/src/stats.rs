//! Frame-timing statistics: a bounded recorder of frame and notch stamps and the report built
//! from it. Pure std, shared by the headless host and the session agent.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Stamps a recording keeps per kind before it stops collecting and marks itself truncated.
pub const MAX_STAMPS: usize = 100_000;

/// Nearest-rank percentiles of a set of durations, in milliseconds.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    pub count: u32,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

/// How the recorded host was compiled.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub opt_level: String,
    pub debug_assertions: bool,
}

impl BuildInfo {
    /// Describes a build by its `opt-level` and whether debug assertions are on.
    pub fn new(opt_level: impl Into<String>, debug_assertions: bool) -> Self {
        Self {
            opt_level: opt_level.into(),
            debug_assertions,
        }
    }
}

/// The statistics of one recording; milliseconds are rounded to 3 decimals.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameReport {
    pub refresh_mhz: u32,
    pub budget_ms: f64,
    pub over_budget_threshold_ms: f64,
    pub duration_ms: f64,
    pub frames: u32,
    pub truncated: bool,
    pub idle_ms: u64,
    pub intervals: Option<Spread>,
    pub idle_gaps: u32,
    pub counted_intervals: u32,
    pub over_budget: u32,
    pub latency: Option<Spread>,
    pub late: u32,
    pub send_lag_max_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<Spread>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildInfo>,
    pub warnings: Vec<String>,
}

/// One recording: frame stamps (with the work behind each, headless only) and notch stamps.
#[derive(Debug)]
pub struct Recorder {
    started: Instant,
    refresh_mhz: u32,
    idle_ms: u64,
    frames: Vec<(Instant, Option<Duration>)>,
    notches: Vec<(Instant, Instant)>,
    ended: bool,
    truncated: bool,
}

impl Recorder {
    /// Starts an empty recording at `started` for a display refreshing at `refresh_mhz`.
    pub fn new(started: Instant, refresh_mhz: u32, idle_ms: u64) -> Self {
        Self {
            started,
            refresh_mhz,
            idle_ms,
            frames: Vec::new(),
            notches: Vec::new(),
            ended: false,
            truncated: false,
        }
    }

    /// Stamps a drawn frame; once ended or full the stamp is dropped and the recording truncated.
    pub fn frame(&mut self, at: Instant, work: Option<Duration>) {
        if self.accepts(self.frames.len()) {
            self.frames.push((at, work));
        }
    }

    /// Stamps a wheel notch due at `scheduled` and sent at `sent`; dropped like `frame` when full.
    pub fn notch(&mut self, scheduled: Instant, sent: Instant) {
        if self.accepts(self.notches.len()) {
            self.notches.push((scheduled, sent));
        }
    }

    /// Stops collecting and marks the recording truncated.
    pub fn end(&mut self) {
        self.ended = true;
        self.truncated = true;
    }

    fn accepts(&mut self, len: usize) -> bool {
        if self.ended || len >= MAX_STAMPS {
            self.truncated = true;
            return false;
        }
        true
    }

    /// Builds the report of everything collected up to `stopped`; the recorder stays usable.
    pub fn finish(&self, stopped: Instant, build: Option<BuildInfo>) -> FrameReport {
        let budget = 1e6 / f64::from(self.refresh_mhz);
        let threshold = 1.5 * budget;
        let half_budget = Duration::from_secs_f64(budget / 2.0 / 1e3);

        // Both stamp lists are swept with binary search, so they are sorted once up front.
        let mut frames: Vec<Instant> = self.frames.iter().map(|(at, _)| *at).collect();
        frames.sort_unstable();
        let mut sents: Vec<Instant> = self.notches.iter().map(|(_, sent)| *sent).collect();
        sents.sort_unstable();

        // An interval is eligible unless it is an idle gap; with notches it counts only when
        // some notch was sent within half a budget after the frame that opened it.
        let mut intervals = Vec::new();
        let (mut idle_gaps, mut counted, mut over) = (0, 0, 0);
        for pair in frames.windows(2) {
            let gap = millis(pair[1] - pair[0]);
            if self.idle_ms > 0 && gap > self.idle_ms as f64 {
                idle_gaps += 1;
                continue;
            }
            intervals.push(gap);
            let next = sents.partition_point(|sent| *sent <= pair[0]);
            if sents.is_empty() || sents.get(next).is_some_and(|s| *s <= pair[0] + half_budget) {
                counted += 1;
                if gap > threshold {
                    over += 1;
                }
            }
        }

        // A notch's latency runs from when it was due to the first frame strictly after it was
        // sent; a notch no frame followed has none.
        let latencies: Vec<f64> = self
            .notches
            .iter()
            .filter_map(|(scheduled, sent)| {
                let drawn = frames.get(frames.partition_point(|at| at <= sent))?;
                Some(millis(drawn.saturating_duration_since(*scheduled)))
            })
            .collect();
        let late = latencies.iter().filter(|l| **l > 2.0 * budget).count() as u32;
        let send_lag_max = self
            .notches
            .iter()
            .map(|(scheduled, sent)| millis(sent.saturating_duration_since(*scheduled)))
            .reduce(f64::max);

        let works: Vec<f64> = self
            .frames
            .iter()
            .filter_map(|(_, w)| w.map(millis))
            .collect();
        let work_median = percentile(&sorted(&works), 50);

        let mut warnings = Vec::new();
        let unoptimised = build
            .as_ref()
            .and_then(|b| b.opt_level.parse::<u32>().ok())
            .is_some_and(|level| level < 2);
        if unoptimised {
            warnings.push("unoptimised_host".to_owned());
        }
        if work_median.is_some_and(|median| median > budget) {
            warnings.push("work_exceeds_budget".to_owned());
        }
        if self.truncated {
            warnings.push("truncated".to_owned());
        }

        FrameReport {
            refresh_mhz: self.refresh_mhz,
            budget_ms: round(budget),
            over_budget_threshold_ms: round(threshold),
            duration_ms: round(millis(stopped.saturating_duration_since(self.started))),
            frames: self.frames.len() as u32,
            truncated: self.truncated,
            idle_ms: self.idle_ms,
            intervals: spread(&intervals),
            idle_gaps,
            counted_intervals: counted,
            over_budget: over,
            latency: spread(&latencies),
            late,
            send_lag_max_ms: send_lag_max.map(round),
            work: spread(&works),
            build,
            warnings,
        }
    }
}

fn millis(d: Duration) -> f64 {
    d.as_nanos() as f64 / 1e6
}

fn round(ms: f64) -> f64 {
    (ms * 1000.0).round() / 1000.0
}

fn sorted(values: &[f64]) -> Vec<f64> {
    let mut values = values.to_vec();
    values.sort_unstable_by(f64::total_cmp);
    values
}

/// Nearest-rank percentile of already sorted values: `sorted[ceil(p * n / 100) - 1]`.
fn percentile(sorted: &[f64], p: usize) -> Option<f64> {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

fn spread(values: &[f64]) -> Option<Spread> {
    let values = sorted(values);
    let at = |p| percentile(&values, p).map(round);
    Some(Spread {
        count: values.len() as u32,
        median_ms: at(50)?,
        p95_ms: at(95)?,
        p99_ms: at(99)?,
        max_ms: round(*values.last()?),
    })
}
