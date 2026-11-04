//! Frame recording on the headless window at 60 Hz, a rate a dev-build frame fits.

mod common;

use std::time::Duration;

use common::{Harness, first_frame, load, with_demo};
use gpui_pilot_host::{Pilot, settle};
use gpui_pilot_protocol::{
    BuildInfo, ErrorCode, FrameReport, Outcome, Reply, Request, Target, WaitUntil, Wheel,
};

async fn show_list(h: &mut Harness, delay_ms: u64) {
    first_frame(h).await;
    h.demo.update(&mut h.cx, |demo, cx| {
        demo.list = true;
        demo.render_delay = (delay_ms > 0).then(|| Duration::from_millis(delay_ms));
        cx.notify();
    });
    let window = h.pilot_window();
    let shown = settle(&h.platform, &window, &h.cx, Duration::from_secs(5)).await;
    assert!(shown.settled, "the list settles");
}

fn notches(n: i32, notch_ms: u64) -> Request {
    Request::Scroll {
        target: Target::Point { x: 320., y: 200. },
        dx: 0.,
        dy: 0.,
        modifiers: Vec::new(),
        wheel: Some(Wheel::new(0, n, notch_ms)),
        timeout_ms: 1000,
        settle_ms: 0,
    }
}

async fn record(h: &mut Harness, pilot: &Pilot, during: Request) -> FrameReport {
    let started = pilot
        .handle(Request::FramesStart { idle_ms: 100 }, &mut h.cx)
        .await;
    assert_eq!(
        started,
        Outcome::Ok(Reply::FramesStarted {
            refresh_mhz: 60_000
        })
    );
    let done = pilot.handle(during, &mut h.cx).await;
    assert!(matches!(done, Outcome::Ok(_)), "{done:?}");
    match pilot.handle(Request::FramesStop, &mut h.cx).await {
        Outcome::Ok(Reply::Frames(report)) => report,
        other => panic!("expected a frames reply: {other:?}"),
    }
}

fn code(outcome: Outcome) -> ErrorCode {
    match outcome {
        Outcome::Err(error) => error.code,
        other => panic!("expected an error: {other:?}"),
    }
}

#[test]
fn an_idle_window_records_no_frames_and_reports_the_build() {
    let report = with_demo(640., 400., 1.0, |mut h| async move {
        show_list(&mut h, 0).await;
        let pilot = Pilot::new(h.platform.clone());
        let idle = Request::Wait {
            until: WaitUntil::Elapsed(300),
            timeout_ms: 300,
        };
        record(&mut h, &pilot, idle).await
    });
    assert_eq!((report.frames, report.intervals.clone()), (0, None));
    assert!(report.duration_ms >= 300.0, "{}", report.duration_ms);
    assert_eq!(report.build, Some(BuildInfo::new("0", true)));
    assert_eq!(report.warnings, ["unoptimised_host"]);
}

#[test]
fn start_and_stop_out_of_order_are_invalid_arguments() {
    let codes = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let early = code(pilot.handle(Request::FramesStop, &mut h.cx).await);
        pilot
            .handle(Request::FramesStart { idle_ms: 100 }, &mut h.cx)
            .await;
        let twice = code(
            pilot
                .handle(Request::FramesStart { idle_ms: 100 }, &mut h.cx)
                .await,
        );
        (early, twice)
    });
    assert_eq!(
        codes,
        (ErrorCode::InvalidArgument, ErrorCode::InvalidArgument)
    );
}

#[test]
fn notches_at_60_hz_draw_one_frame_per_vblank() {
    let report = with_demo(640., 400., 1.0, |mut h| async move {
        show_list(&mut h, 0).await;
        let pilot = Pilot::new(h.platform.clone());
        record(&mut h, &pilot, notches(120, 17)).await
    });
    eprintln!("{report:?}");
    let load = load::report();
    assert!(report.frames >= 100, "{} frames; {load}", report.frames);
    let intervals = report.intervals.as_ref().expect("intervals");
    assert!(
        (intervals.median_ms - 16.667).abs() <= 1.0,
        "median {} ms; {load}",
        intervals.median_ms
    );
    assert!(
        report.counted_intervals >= 50,
        "{} counted intervals; {load}",
        report.counted_intervals
    );
    assert!(
        report.over_budget * 50 <= report.counted_intervals,
        "{} of {} over budget; {load}",
        report.over_budget,
        report.counted_intervals
    );
    assert!(report.latency.as_ref().expect("latency").count >= 100);
    assert_eq!(report.work.as_ref().expect("work").count, report.frames);
}

#[test]
fn a_slow_render_puts_every_counted_interval_over_budget() {
    let report = with_demo(640., 400., 1.0, |mut h| async move {
        show_list(&mut h, 30).await;
        let pilot = Pilot::new(h.platform.clone());
        record(&mut h, &pilot, notches(30, 17)).await
    });
    eprintln!("{report:?}");
    assert!(report.counted_intervals > 0);
    assert_eq!(report.over_budget, report.counted_intervals);
    assert!(report.late * 2 >= 30, "late {}", report.late);
    assert!(
        report.warnings.contains(&"work_exceeds_budget".to_owned()),
        "{:?}",
        report.warnings
    );
}

/// Shows the list, then records a notch scroll that waits on nothing, so every frame it draws
/// goes unwatched; hands back the readback count before the recording and the frame report.
async fn scroll_unwatched(h: &mut Harness) -> (u64, FrameReport) {
    show_list(h, 0).await;
    let before = h.pilot_window().readbacks();
    let pilot = Pilot::new(h.platform.clone());
    let report = record(h, &pilot, notches(30, 17)).await;
    (before, report)
}

#[test]
fn a_recorded_scroll_that_waits_on_nothing_reads_no_pixels_back() {
    let (before, report, after) = with_demo(640., 400., 1.0, |mut h| async move {
        let (before, report) = scroll_unwatched(&mut h).await;
        (before, report, h.pilot_window().readbacks())
    });
    assert!(report.frames >= 20, "{}", report.frames);
    assert_eq!(after, before, "{} frames drawn", report.frames);
}

#[test]
fn a_screenshot_after_unwatched_draws_shows_the_latest_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("list.png");
    let (shot, latest, scrolled_from) = with_demo(640., 400., 1.0, move |mut h| async move {
        show_list(&mut h, 0).await;
        let scrolled_from = h
            .pilot_window()
            .last_frame()
            .unwrap()
            .expect("a frame")
            .image;
        let pilot = Pilot::new(h.platform.clone());
        record(&mut h, &pilot, notches(30, 17)).await;
        // Let the last notch's frame present before comparing against the window's scene.
        h.cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        let shot = Request::Screenshot {
            target: None,
            path: path.clone(),
        };
        let outcome = pilot.handle(shot, &mut h.cx).await;
        assert!(matches!(outcome, Outcome::Ok(_)), "{outcome:?}");
        let latest = h
            .window
            .update(&mut h.cx, |_, window, _| window.render_to_image())
            .expect("the demo window is open")
            .expect("a render");
        (
            image::open(&path).unwrap().to_rgba8(),
            latest,
            scrolled_from,
        )
    });
    assert!(
        shot == latest,
        "the screenshot is the window's latest scene"
    );
    assert!(shot != *scrolled_from, "the scroll moved the list");
}

#[test]
fn settle_after_unwatched_draws_still_settles() {
    let settled = with_demo(640., 400., 1.0, |mut h| async move {
        scroll_unwatched(&mut h).await;
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_secs(5)).await
    });
    assert!(settled.settled, "{}", settled.ticks);
}

/// Measures `work` while the spinner draws every frame on the full demo view. Run by hand;
/// the numbers are in docs/research/2026-10-17-overlay-hits-and-key-up.md.
#[test]
#[ignore = "a measurement, run by hand"]
fn spinner_frame_work() {
    let report = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        h.demo.update(&mut h.cx, |demo, cx| {
            demo.spinning = true;
            cx.notify();
        });
        let pilot = Pilot::new(h.platform.clone());
        let wait = Request::Wait {
            until: WaitUntil::Elapsed(3000),
            timeout_ms: 3000,
        };
        record(&mut h, &pilot, wait).await
    });
    let work = report.work.as_ref().expect("work");
    eprintln!(
        "spinner frames={} work median={} p95={}",
        report.frames, work.median_ms, work.p95_ms
    );
    assert!(report.frames >= 150, "{}", report.frames);
}
