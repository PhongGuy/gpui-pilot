//! `resize` changes the window's size as a compositor would, without moving the pointer.

mod common;

use std::time::Duration;

use common::{first_frame, with_demo};
use gpui_kit::accesskit::Role;
use gpui_pilot_host::Pilot;
use gpui_pilot_protocol::{ErrorCode, Outcome, Reply, Request, Segment, Target, WindowInfo};

fn resize(width: f32, height: f32) -> Request {
    Request::Resize {
        width,
        height,
        settle_ms: 2000,
    }
}

/// pilot-demo's element with GPUI's own tooltip, which follows the pointer's position.
fn tipped() -> Target {
    Target::Id {
        id: vec![Segment::Str("tipped".into())],
    }
}

#[test]
fn a_resize_changes_the_reply_the_frame_and_the_layout() {
    let (outcome, frame, viewport) = with_demo(640., 400., 2.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let outcome = pilot.handle(resize(800., 500.), &mut h.cx).await;
        let frame = h.pilot_window().last_frame().unwrap().unwrap();
        let size = (frame.image.width(), frame.image.height());
        (outcome, size, h.demo.read_with(&h.cx, |d, _| d.viewport))
    });
    assert_eq!(
        outcome,
        Outcome::Ok(Reply::Resize {
            window: WindowInfo::new(800., 500., 2.0),
            settled: true,
        })
    );
    assert_eq!(frame, (1600, 1000), "device pixels at scale 2");
    assert_eq!(viewport, (800., 500.), "the app laid out at the new size");
}

#[test]
fn a_resize_keeps_the_pointer_and_the_tooltip_it_shows() {
    let (roles, pointer, at) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        gpui_pilot_host::settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let pilot = Pilot::new(h.platform.clone());
        let hover = Request::Hover {
            target: tipped(),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let Outcome::Ok(Reply::Hover { .. }) = pilot.handle(hover, &mut h.cx).await else {
            panic!("the hover failed");
        };
        let at = common::bounds_of(&mut h, "tipped").center();
        let resized = pilot.handle(resize(700., 450.), &mut h.cx).await;
        assert!(
            matches!(resized, Outcome::Ok(Reply::Resize { .. })),
            "{resized:?}"
        );
        let pointer = h
            .window
            .update(&mut h.cx, |_, window, _| window.mouse_position())
            .expect("the demo window is open");
        let tree = common::tree_of(&h, &window).await;
        let roles: Vec<Role> = tree.nodes.iter().map(|(_, node)| node.role()).collect();
        (roles, pointer, at)
    });
    assert_eq!(pointer, at, "the resize must not move the pointer");
    assert!(
        roles.contains(&Role::Tooltip),
        "the tooltip went away on resize: {roles:?}"
    );
}

#[test]
fn a_resize_outside_64_to_8192_is_refused_and_keeps_the_size() {
    let (outcomes, viewport) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let mut outcomes = Vec::new();
        for (w, hgt) in [
            (63., 400.),
            (640., 8193.),
            (f32::NAN, 400.),
            (640., f32::INFINITY),
        ] {
            outcomes.push(pilot.handle(resize(w, hgt), &mut h.cx).await);
        }
        (outcomes, h.demo.read_with(&h.cx, |d, _| d.viewport))
    });
    for outcome in outcomes {
        let Outcome::Err(error) = outcome else {
            panic!("expected InvalidArgument: {outcome:?}");
        };
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
    assert_eq!(viewport, (640., 400.));
}

#[test]
fn a_resize_past_8192_device_pixels_is_refused_at_its_scale() {
    let (outcome, viewport, frame) = with_demo(640., 400., 2.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let outcome = pilot.handle(resize(8000., 600.), &mut h.cx).await;
        let frame = h.pilot_window().last_frame().unwrap().unwrap();
        let size = (frame.image.width(), frame.image.height());
        (outcome, h.demo.read_with(&h.cx, |d, _| d.viewport), size)
    });
    let Outcome::Err(error) = outcome else {
        panic!("expected InvalidArgument: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        error.message.contains("scale 2") && error.message.contains("8192"),
        "{}",
        error.message
    );
    assert_eq!(viewport, (640., 400.));
    assert_eq!(frame, (1280, 800));
}
