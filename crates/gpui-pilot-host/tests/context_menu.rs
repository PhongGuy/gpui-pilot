//! A kit context menu opened over a focused element: the menu takes focus mid-prepaint, so
//! accessibility must be inactive on that frame or gpui-pre's debug `set_focus` assert fires.

mod common;

use std::time::Duration;

use common::{Harness, bounds_of, first_frame, with_demo};
use gpui_pilot_host::{Pilot, observe, segment_of, settle};
use gpui_pilot_protocol::{MouseButton, Outcome, Reply, Request, Segment, Target};

fn click(target: Target, button: MouseButton) -> Request {
    Request::Click {
        target,
        button,
        count: 1,
        modifiers: Vec::new(),
        timeout_ms: 1000,
        settle_ms: 1000,
    }
}

fn at(point: gpui_kit::Point<gpui_kit::Pixels>) -> Target {
    Target::Point {
        x: f32::from(point.x),
        y: f32::from(point.y),
    }
}

fn in_menu(path: &[Segment]) -> bool {
    path.contains(&Segment::Str("popup-menu".into()))
}

/// Switches the demo to `--context-menu` and settles with the target focused.
async fn show_fixture(h: &mut Harness) {
    first_frame(h).await;
    h.demo.update(&mut h.cx, |demo, cx| {
        demo.configure(&["--context-menu".to_string()]);
        cx.notify();
    });
    let window = h.pilot_window();
    assert!(
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2))
            .await
            .settled
    );
}

/// Right-clicks the focused target by id, as an agent would, and asserts the reply.
async fn open_menu(h: &mut Harness, pilot: &Pilot) {
    let target = Target::IdSuffix {
        id_suffix: "ctx-target".into(),
    };
    let opened = pilot
        .handle(click(target, MouseButton::Right), &mut h.cx)
        .await;
    assert!(
        matches!(opened, Outcome::Ok(Reply::Click { .. })),
        "{opened:?}"
    );
}

/// Whether the observed snapshots hold the open menu's `popup-menu` element.
fn menu_listed(h: &mut Harness) -> bool {
    h.window
        .update(&mut h.cx, |_, window, _| {
            observe(window).iter().any(|o| {
                o.path
                    .iter()
                    .map(segment_of)
                    .any(|s| s == Segment::Str("popup-menu".into()))
            })
        })
        .expect("the demo window is open")
}

#[test]
fn a_right_click_opens_the_context_menu_without_a_panic() {
    let (listed, open) = with_demo(640., 400., 1.0, |mut h| async move {
        let pilot = Pilot::new(h.platform.clone());
        show_fixture(&mut h).await;
        open_menu(&mut h, &pilot).await;
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let listed = menu_listed(&mut h);
        (listed, h.platform.main_window().is_some())
    });
    assert!(listed, "the open menu is observed");
    assert!(open, "the window is still open");
}

#[test]
fn a_point_hover_and_click_inside_the_open_menu_work() {
    let (hover, clicked, clicks) = with_demo(640., 400., 1.0, |mut h| async move {
        let pilot = Pilot::new(h.platform.clone());
        show_fixture(&mut h).await;
        open_menu(&mut h, &pilot).await;
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let item = bounds_of(&mut h, "popup-menu").center();
        let hover = Request::Hover {
            target: at(item),
            timeout_ms: 0,
            settle_ms: 500,
        };
        let hover = pilot.handle(hover, &mut h.cx).await;
        let clicked = pilot
            .handle(click(at(item), MouseButton::Left), &mut h.cx)
            .await;
        let clicks = h.demo.read_with(&h.cx, |demo, _| demo.ctx_clicks);
        (hover, clicked, clicks)
    });
    let Outcome::Ok(Reply::Hover { hit, .. }) = hover else {
        panic!("{hover:?}");
    };
    assert!(in_menu(&hit.path), "{:?}", hit.path);
    let Outcome::Ok(Reply::Click { hit, .. }) = clicked else {
        panic!("{clicked:?}");
    };
    assert!(in_menu(&hit.path), "{:?}", hit.path);
    assert_eq!(clicks, 1, "the click reached the menu's only item");
}

/// Whether the pilot's bookkeeping or GPUI's own flag says accessibility is active.
fn a11y_active(h: &mut Harness) -> bool {
    let pilot = h.pilot_window().is_a11y_active();
    let gpui = h
        .window
        .update(&mut h.cx, |_, window, _| window.is_a11y_active())
        .expect("the demo window is open");
    pilot || gpui
}

#[test]
fn accessibility_is_inactive_outside_an_act() {
    let (settled, clicked) = with_demo(640., 400., 1.0, |mut h| async move {
        let pilot = Pilot::new(h.platform.clone());
        show_fixture(&mut h).await;
        let settled = a11y_active(&mut h);
        let target = Target::IdSuffix {
            id_suffix: "ctx-target".into(),
        };
        let reply = pilot
            .handle(click(target, MouseButton::Left), &mut h.cx)
            .await;
        assert!(
            matches!(reply, Outcome::Ok(Reply::Click { .. })),
            "{reply:?}"
        );
        (settled, a11y_active(&mut h))
    });
    assert!(!settled, "inactive after settle");
    assert!(!clicked, "inactive after a click");
}

#[test]
fn an_act_right_after_an_unsettled_right_click_draws_the_menu_inactive() {
    let listed = with_demo(640., 400., 1.0, |mut h| async move {
        let pilot = Pilot::new(h.platform.clone());
        show_fixture(&mut h).await;
        let target = || Target::IdSuffix {
            id_suffix: "ctx-target".into(),
        };
        // No settle, so the menu's opening frame is still pending when the hover acts.
        let right = Request::Click {
            target: target(),
            button: MouseButton::Right,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 0,
        };
        let opened = pilot.handle(right, &mut h.cx).await;
        assert!(
            matches!(opened, Outcome::Ok(Reply::Click { .. })),
            "{opened:?}"
        );
        let hover = Request::Hover {
            target: target(),
            timeout_ms: 0,
            settle_ms: 0,
        };
        let hovered = pilot.handle(hover, &mut h.cx).await;
        assert!(
            matches!(hovered, Outcome::Ok(Reply::Hover { .. })),
            "{hovered:?}"
        );
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        menu_listed(&mut h)
    });
    assert!(listed, "the open menu is observed");
}
