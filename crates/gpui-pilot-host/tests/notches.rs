//! Wheel notches on the headless window: one line event per notch, sent on deadlines.

mod common;

use std::time::{Duration, Instant};

use common::{Harness, first_frame, with_demo};
use gpui_pilot_host::{Pilot, settle};
use gpui_pilot_protocol::{ErrorCode, Modifier, Outcome, Reply, Request, Segment, Target, Wheel};

fn notches(
    target: Target,
    y: i32,
    notch_ms: u64,
    modifiers: Vec<Modifier>,
    settle_ms: u64,
) -> Request {
    Request::Scroll {
        target,
        dx: 0.,
        dy: 0.,
        modifiers,
        wheel: Some(Wheel::new(0, y, notch_ms)),
        timeout_ms: 1000,
        settle_ms,
    }
}

fn middle() -> Target {
    Target::Point { x: 320., y: 200. }
}

async fn show_list(h: &mut Harness) {
    first_frame(h).await;
    h.demo.update(&mut h.cx, |demo, cx| {
        demo.list = true;
        cx.notify();
    });
    let window = h.pilot_window();
    let shown = settle(&h.platform, &window, &h.cx, Duration::from_secs(5)).await;
    assert!(shown.settled, "the list settles");
}

fn list_offset(h: &Harness) -> f32 {
    h.demo.read_with(&h.cx, |demo, _| demo.list_offset())
}

#[test]
fn each_notch_scrolls_the_list_by_the_same_step() {
    let (one, three) = with_demo(640., 400., 1.0, |mut h| async move {
        show_list(&mut h).await;
        let pilot = Pilot::new(h.platform.clone());
        let reply = pilot
            .handle(notches(middle(), 1, 16, Vec::new(), 2000), &mut h.cx)
            .await;
        assert_eq!(reply, Outcome::Ok(Reply::Scroll { settled: true }));
        let one = list_offset(&h);
        pilot
            .handle(notches(middle(), 2, 16, Vec::new(), 2000), &mut h.cx)
            .await;
        (one, list_offset(&h))
    });
    assert!(one < 0.0, "a notch down scrolls the list down: {one}");
    assert!(
        (three - 3.0 * one).abs() < 0.5,
        "one notch {one}, three {three}"
    );
}

#[test]
fn every_notch_is_its_own_wheel_event_and_modifiers_are_released() {
    // The demo counts one zoom per wheel event with control held.
    let (zoom, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let name = Target::Id {
            id: vec![Segment::Str("name".into())],
        };
        let reply = pilot
            .handle(notches(name, 3, 16, vec![Modifier::Ctrl], 2000), &mut h.cx)
            .await;
        assert!(
            matches!(reply, Outcome::Ok(Reply::Scroll { .. })),
            "{reply:?}"
        );
        let after = h
            .window
            .update(&mut h.cx, |_, window, _| window.modifiers())
            .expect("the demo window is open");
        (h.demo.read_with(&h.cx, |demo, _| demo.zoom), after)
    });
    assert_eq!(zoom, 3);
    assert_eq!(after, gpui_kit::Modifiers::default());
}

#[test]
fn notches_go_out_on_their_deadlines() {
    let elapsed = with_demo(640., 400., 1.0, |mut h| async move {
        show_list(&mut h).await;
        let pilot = Pilot::new(h.platform.clone());
        let started = Instant::now();
        let reply = pilot
            .handle(notches(middle(), 5, 50, Vec::new(), 0), &mut h.cx)
            .await;
        assert!(
            matches!(reply, Outcome::Ok(Reply::Scroll { .. })),
            "{reply:?}"
        );
        started.elapsed()
    });
    // Notch i is due at first + i * 50 ms, so five take at least 200 ms.
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1000), "{elapsed:?}");
}

#[test]
fn a_notch_count_past_the_limit_is_refused_before_any_event() {
    let (codes, zoom) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let mut codes = Vec::new();
        for (x, y) in [(0, 201), (-201, 0), (0, i32::MIN)] {
            let request = Request::Scroll {
                target: middle(),
                dx: 0.,
                dy: 0.,
                modifiers: vec![Modifier::Ctrl],
                wheel: Some(Wheel::new(x, y, 16)),
                timeout_ms: 1000,
                settle_ms: 0,
            };
            match pilot.handle(request, &mut h.cx).await {
                Outcome::Err(error) => codes.push(error.code),
                other => panic!("({x}, {y}) is refused: {other:?}"),
            }
        }
        (codes, h.demo.read_with(&h.cx, |demo, _| demo.zoom))
    });
    assert_eq!(codes, [ErrorCode::InvalidArgument; 3]);
    assert_eq!(zoom, 0, "no wheel event was sent");
}

#[test]
fn a_notch_period_past_the_limit_is_refused_before_any_event() {
    let (codes, zoom) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let mut codes = Vec::new();
        for notch_ms in [0, 1001, u64::MAX] {
            let request = Request::Scroll {
                target: middle(),
                dx: 0.,
                dy: 0.,
                modifiers: vec![Modifier::Ctrl],
                wheel: Some(Wheel::new(0, 3, notch_ms)),
                timeout_ms: 1000,
                settle_ms: 0,
            };
            match pilot.handle(request, &mut h.cx).await {
                Outcome::Err(error) => codes.push(error.code),
                other => panic!("notch_ms {notch_ms} is refused: {other:?}"),
            }
        }
        (codes, h.demo.read_with(&h.cx, |demo, _| demo.zoom))
    });
    assert_eq!(codes, [ErrorCode::InvalidArgument; 3]);
    assert_eq!(zoom, 0, "no wheel event was sent");
}

#[test]
fn the_list_counts_each_line_wheel_event_in_its_state_line() {
    let (line, listed) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        // The windowed binary passes `--list` alone, as a flag.
        h.demo.update(&mut h.cx, |demo, cx| {
            demo.configure(&["--list".to_owned()]);
            cx.notify();
        });
        let listed = h.demo.read_with(&h.cx, |demo, _| demo.list);
        let window = h.pilot_window();
        let shown = settle(&h.platform, &window, &h.cx, Duration::from_secs(5)).await;
        assert!(shown.settled, "the list settles");
        let pilot = Pilot::new(h.platform.clone());
        let reply = pilot
            .handle(notches(middle(), 3, 16, Vec::new(), 2000), &mut h.cx)
            .await;
        assert!(
            matches!(reply, Outcome::Ok(Reply::Scroll { .. })),
            "{reply:?}"
        );
        (
            h.demo.read_with(&h.cx, |demo, cx| demo.state_line(cx)),
            listed,
        )
    });
    assert!(listed, "`--list` switches the list on");
    assert!(line.ends_with(" wheels=3 lines=-9"), "{line}");
}
