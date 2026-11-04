//! GPUI drag and drop on pilot-demo, driven by the host's drag events.

mod common;

use std::time::Duration;

use common::{Harness, bounds_of, first_frame, with_demo};
use gpui_kit::{AppContext as _, MouseButton, Pixels, Point, point, px};
use gpui_pilot_host::{dispatch_events, drag_move, drag_path, drag_press, drag_release, settle};

/// Presses at `from`, moves in ten steps with a frame between each, releases at `to`, settles.
async fn drag(h: &mut Harness, from: Point<Pixels>, to: Point<Pixels>) {
    let mut batches = vec![drag_press(from, MouseButton::Left, Default::default())];
    batches.extend(
        drag_path(from, to, 10)
            .into_iter()
            .map(|at| vec![drag_move(at, MouseButton::Left, Default::default())]),
    );
    batches.push(drag_release(to, MouseButton::Left, Default::default()));
    for events in batches {
        h.cx.update_window(h.window.into(), |_, window, cx| {
            dispatch_events(window, cx, events)
        })
        .expect("the demo window is open");
        h.cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
    }
    let window = h.pilot_window();
    settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
}

#[test]
fn dragging_the_chip_onto_the_dropzone_drops_it_once() {
    let (dropped, clicks, line) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let from = bounds_of(&mut h, "chip").center();
        let to = bounds_of(&mut h, "dropzone").center();
        drag(&mut h, from, to).await;
        h.demo
            .read_with(&h.cx, |d, cx| (d.dropped, d.clicks, d.state_line(cx)))
    });
    assert_eq!(dropped, 1);
    assert_eq!(clicks, 0);
    assert_eq!(line, "clicks=0 name=\"\" dropped=1");
}

#[test]
fn a_drag_released_off_the_dropzone_drops_and_clicks_nothing() {
    let (dropped, clicks) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let from = bounds_of(&mut h, "chip").center();
        // Onto the count button, which has no drop handler, then onto empty space.
        let count = bounds_of(&mut h, "count").center();
        drag(&mut h, from, count).await;
        drag(&mut h, from, point(px(620.), px(390.))).await;
        h.demo.read_with(&h.cx, |d, _| (d.dropped, d.clicks))
    });
    assert_eq!((dropped, clicks), (0, 0));
}
