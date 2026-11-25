//! `frames start` with `a11y`: GPUI's accessibility stays on for the whole headless recording.

mod common;

use std::time::Duration;

use common::{Harness, first_frame, with_demo};
use gpui_kit::QuitMode;
use gpui_pilot_host::{Pilot, settle};
use gpui_pilot_protocol::{
    ErrorCode, FrameReport, MouseButton, Outcome, Reply, Request, Segment, Target, WaitUntil,
};

/// Switches the demo to the board with `args` and waits for its first frame; settles it too
/// unless it auto-pans, which never settles.
async fn open_board(h: &mut Harness, args: &[&str]) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let pans = args.iter().any(|a| a.starts_with("--board-autopan"));
    h.demo.update(&mut h.cx, |demo, cx| {
        demo.configure(&args);
        cx.notify();
    });
    first_frame(h).await;
    if !pans {
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
    }
}

fn start(a11y: bool) -> Request {
    Request::FramesStart { idle_ms: 100, a11y }
}

async fn stop(h: &mut Harness, pilot: &Pilot) -> FrameReport {
    match pilot.handle(Request::FramesStop, &mut h.cx).await {
        Outcome::Ok(Reply::Frames(report)) => report,
        other => panic!("expected a frames reply: {other:?}"),
    }
}

#[allow(dead_code)]
async fn record(h: &mut Harness, pilot: &Pilot, a11y: bool, during: Request) -> FrameReport {
    let started = pilot.handle(start(a11y), &mut h.cx).await;
    assert!(
        matches!(started, Outcome::Ok(Reply::FramesStarted { .. })),
        "{started:?}"
    );
    let done = pilot.handle(during, &mut h.cx).await;
    assert!(matches!(done, Outcome::Ok(_)), "{done:?}");
    stop(h, pilot).await
}

fn wait_ms(ms: u64) -> Request {
    Request::Wait {
        until: WaitUntil::Elapsed(ms),
        timeout_ms: ms,
    }
}

fn code(outcome: Outcome) -> ErrorCode {
    match outcome {
        Outcome::Err(error) => error.code,
        other => panic!("expected an error: {other:?}"),
    }
}

#[test]
fn a_recording_with_a11y_keeps_it_on_across_an_id_click_and_drops_it_at_stop() {
    let (after_start, selected, during, report, after_stop) =
        with_demo(640., 400., 1.0, |mut h| async move {
            open_board(&mut h, &["--board"]).await;
            let window = h.pilot_window();
            let pilot = Pilot::new(h.platform.clone());
            let started = pilot.handle(start(true), &mut h.cx).await;
            assert_eq!(
                started,
                Outcome::Ok(Reply::FramesStarted {
                    refresh_mhz: 60_000
                })
            );
            let after_start = (window.is_a11y_active(), window.a11y_tree().is_some());
            let click = Request::Click {
                target: Target::Id {
                    id: vec![Segment::Str("shape-2".into())],
                },
                button: MouseButton::Left,
                count: 1,
                modifiers: Vec::new(),
                timeout_ms: 2000,
                settle_ms: 2000,
            };
            let clicked = pilot.handle(click, &mut h.cx).await;
            assert!(
                matches!(clicked, Outcome::Ok(Reply::Click { .. })),
                "{clicked:?}"
            );
            let selected = h.demo.read_with(&h.cx, |d, _| {
                d.shape("shape-2").expect("a board shape").selected
            });
            let gpui_on = h
                .window
                .update(&mut h.cx, |_, window, _| window.is_a11y_active())
                .expect("the demo window is open");
            let during = (
                window.is_a11y_active(),
                window.a11y_tree().is_some(),
                gpui_on,
            );
            let report = stop(&mut h, &pilot).await;
            let after_stop = (window.is_a11y_active(), window.a11y_tree().is_some());
            (after_start, selected, during, report, after_stop)
        });
    assert_eq!(after_start, (true, true), "the priming frame built a tree");
    assert!(selected, "the id click reached shape-2");
    assert_eq!(
        during,
        (true, true, true),
        "the click's scope left accessibility on"
    );
    assert!(report.a11y, "{report:?}");
    assert!(report.frames >= 1, "the click drew frames: {report:?}");
    assert_eq!(after_stop, (false, false));
}

#[test]
fn a_recording_without_a11y_leaves_it_off() {
    let (during, report) = with_demo(640., 400., 1.0, |mut h| async move {
        open_board(&mut h, &["--board"]).await;
        let window = h.pilot_window();
        let pilot = Pilot::new(h.platform.clone());
        pilot.handle(start(false), &mut h.cx).await;
        pilot.handle(wait_ms(100), &mut h.cx).await;
        let during = (window.is_a11y_active(), window.a11y_tree().is_some());
        (during, stop(&mut h, &pilot).await)
    });
    assert_eq!(during, (false, false));
    assert!(!report.a11y);
}

#[test]
fn a_second_start_leaves_the_running_recordings_a11y_as_it_was() {
    let states = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        let pilot = Pilot::new(h.platform.clone());
        pilot.handle(start(false), &mut h.cx).await;
        let refused_on = code(pilot.handle(start(true), &mut h.cx).await);
        let off = window.is_a11y_active();
        stop(&mut h, &pilot).await;
        pilot.handle(start(true), &mut h.cx).await;
        let refused_off = code(pilot.handle(start(false), &mut h.cx).await);
        let on = window.is_a11y_active();
        let report = stop(&mut h, &pilot).await;
        (refused_on, off, refused_off, on, report.a11y)
    });
    assert_eq!(
        states,
        (
            ErrorCode::InvalidArgument,
            false,
            ErrorCode::InvalidArgument,
            true,
            true
        )
    );
}

#[test]
fn closing_the_window_mid_recording_leaves_a11y_off() {
    let (closed, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        // Removing the last window would otherwise quit the test's app.
        h.cx.update(|cx| cx.set_quit_mode(QuitMode::Explicit));
        let window = h.pilot_window();
        let pilot = Pilot::new(h.platform.clone());
        pilot.handle(start(true), &mut h.cx).await;
        let close = Request::CloseWindow {
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let closed = pilot.handle(close, &mut h.cx).await;
        pilot.finish_window_close(&mut h.cx);
        h.cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        (
            closed,
            (window.is_a11y_active(), window.a11y_tree().is_some()),
        )
    });
    assert_eq!(
        closed,
        Outcome::Ok(Reply::CloseWindow {
            closed: true,
            settled: true
        })
    );
    assert_eq!(after, (false, false));
}
