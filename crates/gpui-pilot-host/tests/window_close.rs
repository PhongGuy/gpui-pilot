//! Closing the window as a window manager would: GPUI's should-close callback decides.

mod common;

use std::time::Duration;

use common::{first_frame, with_demo};
use gpui_kit::QuitMode;
use gpui_pilot_host::Pilot;
use gpui_pilot_protocol::{Outcome, Reply, Request};

fn close_window() -> Request {
    Request::CloseWindow {
        timeout_ms: 1000,
        settle_ms: 2000,
    }
}

#[test]
fn an_unguarded_window_agrees_and_finishing_removes_it() {
    let (reply, open_before, open_after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        // Removing the last window would otherwise quit the test's app.
        h.cx.update(|cx| cx.set_quit_mode(QuitMode::Explicit));
        let pilot = Pilot::new(h.platform.clone());
        let reply = pilot.handle(close_window(), &mut h.cx).await;
        let open_before = h.platform.main_window().is_some();
        pilot.finish_window_close(&mut h.cx);
        h.cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        (reply, open_before, h.platform.main_window().is_some())
    });
    assert_eq!(
        reply,
        Outcome::Ok(Reply::CloseWindow {
            closed: true,
            settled: true
        })
    );
    assert!(open_before, "the reply comes before the window goes");
    assert!(!open_after, "finishing runs GPUI's close path");
}

#[test]
fn a_guarded_window_refuses_shows_its_prompt_and_closes_on_discard() {
    let (refused, tree, asks, answered, open_after) =
        with_demo(640., 400., 1.0, |mut h| async move {
            first_frame(&h).await;
            h.cx.update(|cx| cx.set_quit_mode(QuitMode::Explicit));
            let demo = h.demo.clone();
            h.window
                .update(&mut h.cx, |_, window, cx| {
                    pilot_demo::guard_close(demo, window, cx)
                })
                .expect("the demo window is open");
            let pilot = Pilot::new(h.platform.clone());
            let refused = pilot.handle(close_window(), &mut h.cx).await;
            let tree = Request::Tree {
                within: None,
                wait_for: None,
                timeout_ms: 0,
                settle_ms: 0,
            };
            let tree = pilot.handle(tree, &mut h.cx).await;
            let asks = h.demo.read_with(&h.cx, |d, _| d.close_asks);
            let discard = Request::Dialog {
                button: Some("Discard".into()),
                timeout_ms: 0,
                settle_ms: 2000,
            };
            let answered = pilot.handle(discard, &mut h.cx).await;
            h.cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            (
                refused,
                tree,
                asks,
                answered,
                h.platform.main_window().is_some(),
            )
        });
    assert!(
        matches!(
            refused,
            Outcome::Ok(Reply::CloseWindow { closed: false, .. })
        ),
        "{refused:?}"
    );
    let Outcome::Ok(Reply::Tree {
        dialog: Some(dialog),
        ..
    }) = tree
    else {
        panic!("the tree shows the close prompt: {tree:?}");
    };
    assert_eq!(dialog.message, "Discard unsaved changes?");
    assert_eq!(dialog.buttons, ["Discard", "Cancel"]);
    assert_eq!(asks, 1);
    assert!(
        matches!(&answered, Outcome::Ok(Reply::Dialog { answered: Some(label), .. }) if label == "Discard"),
        "{answered:?}"
    );
    assert!(!open_after, "Discard closed the window");
}
