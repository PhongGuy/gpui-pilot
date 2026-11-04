//! GPUI's `Window::prompt` on the pilot platform: held as a dialog, never drawn, answered by index.

mod common;

use std::time::Duration;

use common::{click_id, first_frame, with_demo};
use gpui_kit::PromptLevel;
use gpui_pilot_host::{Pilot, settle};
use gpui_pilot_protocol::{
    DialogLevel, ErrorCode, MouseButton, Outcome, Reply, Request, Segment, Target,
};

#[test]
fn a_prompt_is_held_by_the_window_and_answered_by_index() {
    let (seen, drawn, refused, answer, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        click_id(&mut h, "ask").await;
        let window = h.pilot_window();
        let seen = window.pending_dialog();
        let drawn = h
            .window
            .update(&mut h.cx, |_, window, _| window.has_active_prompt())
            .expect("the demo window is open");
        let refused = !window.answer_dialog(2);
        assert!(window.answer_dialog(0));
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let answer = h.demo.read_with(&h.cx, |d, _| d.answer);
        (seen, drawn, refused, answer, window.pending_dialog())
    });
    let seen = seen.expect("the demo's prompt is pending");
    assert_eq!(seen.level, PromptLevel::Warning);
    assert_eq!(seen.message, "Delete the chip?");
    assert_eq!(seen.detail.as_deref(), Some("This cannot be undone."));
    assert_eq!(seen.buttons, ["Delete", "Cancel"]);
    assert!(!drawn, "GPUI's fallback prompt is never built");
    assert!(refused, "an out-of-range answer is refused");
    assert_eq!(answer, Some(0));
    assert_eq!(after, None);
}

#[test]
fn a_second_prompt_replaces_the_first_and_cancels_it() {
    let (first_cancelled, pending) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let (first, _second) = h
            .window
            .update(&mut h.cx, |_, window, cx| {
                let first = window.prompt(PromptLevel::Info, "one", None, &["OK"], cx);
                let second = window.prompt(PromptLevel::Critical, "two", None, &["Yes", "No"], cx);
                (first, second)
            })
            .expect("the demo window is open");
        let first_cancelled = first.await.is_err();
        let pending = h.pilot_window().pending_dialog();
        (first_cancelled, pending)
    });
    assert!(
        first_cancelled,
        "the replaced prompt's receiver is cancelled"
    );
    let pending = pending.expect("the second prompt waits");
    assert_eq!(pending.level, PromptLevel::Critical);
    assert_eq!(
        (pending.message.as_str(), pending.buttons.len()),
        ("two", 2)
    );
}

fn dialog(button: Option<&str>, timeout_ms: u64, settle_ms: u64) -> Request {
    Request::Dialog {
        button: button.map(str::to_owned),
        timeout_ms,
        settle_ms,
    }
}

#[test]
fn the_tree_reports_a_pending_dialog_and_dialog_answers_it_by_label() {
    let (tree, read, answered, answer, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        click_id(&mut h, "ask").await;
        let tree = Request::Tree {
            within: None,
            wait_for: None,
            timeout_ms: 0,
            settle_ms: 0,
        };
        let tree = pilot.handle(tree, &mut h.cx).await;
        let read = pilot.handle(dialog(None, 0, 0), &mut h.cx).await;
        let answered = pilot
            .handle(dialog(Some("Cancel"), 0, 2000), &mut h.cx)
            .await;
        let answer = h.demo.read_with(&h.cx, |d, _| d.answer);
        let after = pilot.handle(dialog(None, 0, 0), &mut h.cx).await;
        (tree, read, answered, answer, after)
    });
    let Outcome::Ok(Reply::Tree {
        dialog: Some(shown),
        ..
    }) = tree
    else {
        panic!("the tree shows the dialog: {tree:?}");
    };
    assert_eq!(shown.level, DialogLevel::Warning);
    assert_eq!(shown.message, "Delete the chip?");
    assert_eq!(shown.detail.as_deref(), Some("This cannot be undone."));
    assert_eq!(shown.buttons, ["Delete", "Cancel"]);
    assert!(
        matches!(&read, Outcome::Ok(Reply::Dialog { dialog: Some(d), answered: None, .. }) if *d == shown),
        "{read:?}"
    );
    let Outcome::Ok(Reply::Dialog {
        answered: Some(label),
        settled,
        ..
    }) = answered
    else {
        panic!("unexpected: {answered:?}");
    };
    assert_eq!(label, "Cancel");
    assert!(settled);
    assert_eq!(answer, Some(1));
    assert!(
        matches!(
            after,
            Outcome::Ok(Reply::Dialog {
                dialog: None,
                answered: None,
                ..
            })
        ),
        "{after:?}"
    );
}

#[test]
fn input_is_occluded_while_a_dialog_waits_and_bad_answers_fail() {
    let (none, blocked, unknown, clicks) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let none = pilot
            .handle(dialog(Some("Delete"), 100, 0), &mut h.cx)
            .await;
        click_id(&mut h, "ask").await;
        let click = Request::Click {
            target: Target::Id {
                id: vec![Segment::Str("count".into())],
            },
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 0,
            settle_ms: 0,
        };
        let blocked = pilot.handle(click, &mut h.cx).await;
        let unknown = pilot.handle(dialog(Some("Maybe"), 0, 0), &mut h.cx).await;
        let clicks = h.demo.read_with(&h.cx, |d, _| d.clicks);
        (none, blocked, unknown, clicks)
    });
    let code_and_message = |outcome: Outcome| match outcome {
        Outcome::Err(error) => (error.code, error.message),
        other => panic!("expected an error: {other:?}"),
    };
    let (code, message) = code_and_message(none);
    assert_eq!(code, ErrorCode::Timeout);
    assert!(message.starts_with("after 100 ms: "), "{message}");
    let (code, message) = code_and_message(blocked);
    assert_eq!(code, ErrorCode::Occluded);
    assert!(message.contains("Delete the chip?"), "{message}");
    let (code, message) = code_and_message(unknown);
    assert_eq!(code, ErrorCode::ElementNotFound);
    assert!(
        message.contains("Delete") && message.contains("Cancel"),
        "{message}"
    );
    assert_eq!(clicks, 0, "the blocked click never reached the app");
}

#[test]
fn a_drag_is_occluded_while_a_dialog_waits_and_drops_nothing() {
    let (outcome, dropped) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        click_id(&mut h, "ask").await;
        let id = |name: &str| Target::Id {
            id: vec![Segment::Str(name.into())],
        };
        let drag = Request::Drag {
            from: id("chip"),
            to: id("dropzone"),
            steps: 10,
            button: gpui_pilot_protocol::MouseButton::Left,
            modifiers: Vec::new(),
            timeout_ms: 0,
            settle_ms: 0,
        };
        let outcome = pilot.handle(drag, &mut h.cx).await;
        (outcome, h.demo.read_with(&h.cx, |d, _| d.dropped))
    });
    let Outcome::Err(error) = outcome else {
        panic!("expected Occluded: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::Occluded);
    assert_eq!(dropped, 0);
}
