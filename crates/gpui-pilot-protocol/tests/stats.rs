//! Frame statistics on hand-built stamp sequences.

use std::time::{Duration, Instant};

use gpui_pilot_protocol::stats::MAX_STAMPS;
use gpui_pilot_protocol::{BuildInfo, FrameReport, Recorder, Spread};
use serde_json::json;

fn us(t0: Instant, micros: u64) -> Instant {
    t0 + Duration::from_micros(micros)
}

fn ms(t0: Instant, millis: f64) -> Instant {
    us(t0, (millis * 1000.0) as u64)
}

fn spread(value: &Option<Spread>) -> (u32, f64, f64, f64, f64) {
    let s = value.as_ref().expect("a spread");
    (s.count, s.median_ms, s.p95_ms, s.p99_ms, s.max_ms)
}

/// Pure animation at 60 Hz: one interval over budget, one idle gap.
fn animation(idle_ms: u64) -> FrameReport {
    let t0 = Instant::now();
    let mut recorder = Recorder::new(t0, 60_000, idle_ms);
    for micros in [0, 16_667, 33_333, 70_000, 300_000] {
        recorder.frame(us(t0, micros), None);
    }
    recorder.finish(ms(t0, 400.0), None)
}

#[test]
fn an_animation_counts_every_non_idle_interval() {
    let report = animation(100);
    assert_eq!((report.refresh_mhz, report.budget_ms), (60_000, 16.667));
    assert_eq!(report.over_budget_threshold_ms, 25.0);
    assert_eq!(
        (report.duration_ms, report.frames, report.idle_ms),
        (400.0, 5, 100)
    );
    assert_eq!(
        spread(&report.intervals),
        (3, 16.667, 36.667, 36.667, 36.667)
    );
    assert_eq!(report.idle_gaps, 1);
    assert_eq!((report.counted_intervals, report.over_budget), (3, 1));
    assert_eq!((report.latency.clone(), report.late), (None, 0));
    assert_eq!(report.send_lag_max_ms, None);
    assert_eq!((report.work.clone(), report.build.clone()), (None, None));
    assert!(!report.truncated);
    assert!(report.warnings.is_empty());
}

#[test]
fn idle_ms_zero_treats_no_interval_as_idle() {
    let report = animation(0);
    assert_eq!(report.idle_gaps, 0);
    assert_eq!(spread(&report.intervals), (4, 16.667, 230.0, 230.0, 230.0));
    assert_eq!((report.counted_intervals, report.over_budget), (4, 2));
}

#[test]
fn with_notches_only_intervals_with_input_in_time_count() {
    let t0 = Instant::now();
    let mut recorder = Recorder::new(t0, 144_000, 100);
    for millis in [0.0, 7.0, 14.0, 30.0, 42.0] {
        recorder.frame(ms(t0, millis), None);
    }
    for (scheduled, sent) in [(1.0, 1.0), (16.0, 18.0), (31.0, 31.5), (50.0, 50.0)] {
        recorder.notch(ms(t0, scheduled), ms(t0, sent));
    }
    let report = recorder.finish(ms(t0, 60.0), None);
    assert_eq!(
        (report.budget_ms, report.over_budget_threshold_ms),
        (6.944, 10.417)
    );
    assert_eq!(spread(&report.intervals), (4, 7.0, 16.0, 16.0, 16.0));
    // 0->7 (notch at 1) and 30->42 (notch at 31.5) are counted; 14->30 is long but its notch
    // came 4 ms after the frame, past half a budget.
    assert_eq!((report.counted_intervals, report.over_budget), (2, 1));
    // Latencies 6, 14 and 11 ms; the notch at 50 ms drew no frame and has none.
    assert_eq!(spread(&report.latency), (3, 11.0, 14.0, 14.0, 14.0));
    assert_eq!(report.late, 1);
    assert_eq!(report.send_lag_max_ms, Some(2.0));
}

#[test]
fn work_and_build_raise_their_warnings() {
    let t0 = Instant::now();
    let mut recorder = Recorder::new(t0, 144_000, 100);
    for (millis, work) in [(0.0, 5), (7.0, 8), (14.0, 9)] {
        recorder.frame(ms(t0, millis), Some(Duration::from_millis(work)));
    }
    let report = recorder.finish(ms(t0, 20.0), Some(BuildInfo::new("0", true)));
    assert_eq!(spread(&report.work), (3, 8.0, 9.0, 9.0, 9.0));
    assert_eq!(report.warnings, ["unoptimised_host", "work_exceeds_budget"]);
    for level in ["3", "2", "s"] {
        let report = recorder.finish(ms(t0, 20.0), Some(BuildInfo::new(level, true)));
        assert_eq!(report.warnings, ["work_exceeds_budget"], "{level}");
    }
    let mut quick = Recorder::new(t0, 144_000, 100);
    quick.frame(t0, Some(Duration::from_millis(1)));
    let report = quick.finish(ms(t0, 5.0), Some(BuildInfo::new("3", true)));
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn fewer_than_two_frames_is_not_an_error() {
    let t0 = Instant::now();
    let mut recorder = Recorder::new(t0, 144_000, 100);
    let empty = recorder.finish(ms(t0, 250.0), None);
    assert_eq!((empty.frames, empty.duration_ms), (0, 250.0));
    assert_eq!(
        (empty.intervals.clone(), empty.latency.clone()),
        (None, None)
    );
    assert_eq!(
        (empty.counted_intervals, empty.over_budget, empty.idle_gaps),
        (0, 0, 0)
    );
    recorder.frame(ms(t0, 3.0), None);
    let one = recorder.finish(ms(t0, 250.0), None);
    assert_eq!((one.frames, one.intervals), (1, None));
}

#[test]
fn a_full_buffer_or_an_end_truncates() {
    let t0 = Instant::now();
    let mut frames = Recorder::new(t0, 60_000, 100);
    for i in 0..MAX_STAMPS as u64 + 3 {
        frames.frame(us(t0, i), None);
    }
    let report = frames.finish(ms(t0, 200.0), None);
    assert_eq!(report.frames as usize, MAX_STAMPS);
    assert!(report.truncated);
    assert_eq!(report.warnings, ["truncated"]);

    let mut notches = Recorder::new(t0, 60_000, 100);
    for i in 0..MAX_STAMPS as u64 + 1 {
        notches.notch(us(t0, i), us(t0, i));
    }
    assert!(notches.finish(ms(t0, 200.0), None).truncated);

    let mut ended = Recorder::new(t0, 60_000, 100);
    ended.frame(t0, None);
    ended.end();
    ended.frame(ms(t0, 16.0), None);
    let report = ended.finish(ms(t0, 20.0), None);
    assert_eq!((report.frames, report.truncated), (1, true));
}

#[test]
fn the_report_keeps_nulls_and_omits_absent_work_and_build() {
    let empty = Recorder::new(Instant::now(), 144_000, 100);
    let report = empty.finish(Instant::now(), None);
    let value = serde_json::to_value(&report).unwrap();
    let object = value.as_object().unwrap();
    for key in ["intervals", "latency", "send_lag_max_ms"] {
        assert!(
            object.contains_key(key) && value[key].is_null(),
            "{key}: {value}"
        );
    }
    assert!(
        !object.contains_key("work") && !object.contains_key("build"),
        "{value}"
    );
    assert_eq!(value["warnings"], json!([]));
    assert_eq!(
        serde_json::from_value::<FrameReport>(value).unwrap(),
        report
    );

    let animated = animation(100);
    let value = serde_json::to_value(&animated).unwrap();
    assert_eq!(
        value["intervals"],
        json!({"count": 3, "median_ms": 16.667, "p95_ms": 36.667, "p99_ms": 36.667, "max_ms": 36.667})
    );
}
