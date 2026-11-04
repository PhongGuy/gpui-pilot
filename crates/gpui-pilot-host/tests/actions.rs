//! Pointer actions wait for their target and report the element they hit.

mod common;

use std::time::{Duration, Instant};

use common::{first_frame, with_demo};
use gpui_pilot_host::Pilot;
use gpui_pilot_protocol::{
    ErrorCode, Modifier, MouseButton, Outcome, Reply, Request, Segment, Target, WaitUntil,
};

fn id(text: &str) -> Target {
    Target::Id {
        id: vec![Segment::Str(text.into())],
    }
}

fn click(target: Target, timeout_ms: u64) -> Request {
    Request::Click {
        target,
        button: MouseButton::Left,
        count: 1,
        modifiers: Vec::new(),
        timeout_ms,
        settle_ms: 2000,
    }
}

fn ends_with(path: &[Segment], text: &str) -> bool {
    path.last() == Some(&Segment::Str(text.into()))
}

#[test]
fn a_covered_target_is_occluded_and_names_the_cover() {
    let (outcome, clicks) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let outcome = pilot.handle(click(id("covered"), 200), &mut h.cx).await;
        (outcome, h.demo.read_with(&h.cx, |d, _| d.covered_clicks))
    });
    let Outcome::Err(error) = outcome else {
        panic!("expected Occluded: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::Occluded);
    assert!(ends_with(&error.hit.expect("the cover").path, "cover"));
    assert_eq!(clicks, 0);
}

#[test]
fn a_click_waits_for_a_target_a_plain_thread_reveals() {
    let (outcome, elapsed) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let started = pilot.handle(click(id("slow"), 1000), &mut h.cx).await;
        assert!(
            matches!(started, Outcome::Ok(Reply::Click { .. })),
            "{started:?}"
        );
        let begun = Instant::now();
        let outcome = pilot.handle(click(id("slow-done"), 5000), &mut h.cx).await;
        (outcome, begun.elapsed())
    });
    let Outcome::Ok(Reply::Click { clicked, hit, .. }) = outcome else {
        panic!("unexpected: {outcome:?}");
    };
    assert!(ends_with(&clicked.path, "slow-done"));
    assert!(hit.path.starts_with(&clicked.path), "{hit:?}");
    assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
}

#[test]
fn a_target_that_never_appears_keeps_its_code_after_the_timeout() {
    let (outcome, elapsed) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let begun = Instant::now();
        let outcome = pilot.handle(click(id("slow-done"), 100), &mut h.cx).await;
        (outcome, begun.elapsed())
    });
    let Outcome::Err(error) = outcome else {
        panic!("unexpected: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::ElementNotFound);
    assert!(
        error.message.starts_with("after 100 ms: "),
        "{}",
        error.message
    );
    assert!(!error.nearest.is_empty());
    assert!(
        elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );
}

#[test]
fn a_menu_item_click_hits_the_item_and_closes_the_menu() {
    let (outcome, roles) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let opened = pilot.handle(click(id("menu"), 1000), &mut h.cx).await;
        assert!(matches!(opened, Outcome::Ok(_)), "{opened:?}");
        let alpha = Target::Role {
            role: "menuitem".into(),
            name: "Alpha".into(),
            nth: None,
        };
        let outcome = pilot.handle(click(alpha, 1000), &mut h.cx).await;
        let tree = Request::Tree {
            within: None,
            wait_for: None,
            timeout_ms: 0,
            settle_ms: 0,
        };
        let Outcome::Ok(Reply::Tree { nodes, .. }) = pilot.handle(tree, &mut h.cx).await else {
            panic!("tree failed");
        };
        (
            outcome,
            nodes.into_iter().filter_map(|n| n.role).collect::<Vec<_>>(),
        )
    });
    let Outcome::Ok(Reply::Click { clicked, hit, .. }) = outcome else {
        panic!("unexpected: {outcome:?}");
    };
    assert_eq!(clicked.name.as_deref(), Some("Alpha"));
    assert!(hit.path.starts_with(&clicked.path), "{hit:?}");
    assert!(
        !roles.iter().any(|r| r == "Menu"),
        "the menu closed: {roles:?}"
    );
}

#[test]
fn hover_repaints_the_hovered_button_and_reports_it() {
    let (outcome, changed) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        gpui_pilot_host::settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let before = window.last_frame().unwrap().expect("a frame").hash();
        let pilot = Pilot::new(h.platform.clone());
        let hover = Request::Hover {
            target: id("count"),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let outcome = pilot.handle(hover, &mut h.cx).await;
        let after = window.last_frame().unwrap().expect("a frame").hash();
        (outcome, before != after)
    });
    let Outcome::Ok(Reply::Hover {
        hovered,
        hit,
        settled,
    }) = outcome
    else {
        panic!("unexpected: {outcome:?}");
    };
    assert!(ends_with(&hovered.path, "count"));
    assert!(hit.path.starts_with(&hovered.path));
    assert!(settled);
    assert!(changed, "the hover style repainted the button");
}

#[test]
fn wait_returns_the_node_a_thread_reveals() {
    let outcome = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        pilot.handle(click(id("slow"), 1000), &mut h.cx).await;
        let wait = Request::Wait {
            until: WaitUntil::Visible(id("slow-done")),
            timeout_ms: 2000,
        };
        pilot.handle(wait, &mut h.cx).await
    });
    let Outcome::Ok(Reply::Wait { node, waited_ms }) = outcome else {
        panic!("unexpected: {outcome:?}");
    };
    assert!(ends_with(&node.expect("the node").path, "slow-done"));
    assert!(waited_ms >= 100, "{waited_ms}");
}

#[test]
fn wait_for_gone_and_for_a_duration() {
    let (absent, present, slept, point) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let wait = |until, timeout_ms| Request::Wait { until, timeout_ms };
        (
            pilot
                .handle(wait(WaitUntil::Gone(id("nope")), 1000), &mut h.cx)
                .await,
            pilot
                .handle(wait(WaitUntil::Gone(id("count")), 100), &mut h.cx)
                .await,
            pilot
                .handle(wait(WaitUntil::Elapsed(120), 120), &mut h.cx)
                .await,
            pilot
                .handle(
                    wait(WaitUntil::Visible(Target::Point { x: 1., y: 1. }), 100),
                    &mut h.cx,
                )
                .await,
        )
    });
    let Outcome::Ok(Reply::Wait {
        node: None,
        waited_ms,
    }) = absent
    else {
        panic!("unexpected: {absent:?}");
    };
    assert!(waited_ms < 1000);
    let Outcome::Err(error) = present else {
        panic!("unexpected: {present:?}");
    };
    assert_eq!(error.code, ErrorCode::Timeout);
    assert!(error.matches.iter().any(|n| ends_with(&n.path, "count")));
    let Outcome::Ok(Reply::Wait { waited_ms, .. }) = slept else {
        panic!("unexpected: {slept:?}");
    };
    assert!(waited_ms >= 120, "{waited_ms}");
    let Outcome::Err(error) = point else {
        panic!("unexpected: {point:?}");
    };
    assert_eq!(error.code, ErrorCode::Unsupported);
}

#[test]
fn tree_settles_before_it_reads() {
    let nodes = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let debounce = Request::Click {
            target: id("debounce"),
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 0,
        };
        assert!(matches!(
            pilot.handle(debounce, &mut h.cx).await,
            Outcome::Ok(_)
        ));
        let tree = Request::Tree {
            within: None,
            wait_for: None,
            timeout_ms: 0,
            settle_ms: 2000,
        };
        match pilot.handle(tree, &mut h.cx).await {
            Outcome::Ok(Reply::Tree { nodes, .. }) => nodes,
            other => panic!("unexpected: {other:?}"),
        }
    });
    assert!(nodes.iter().any(|n| ends_with(&n.path, "late")));
}

fn drag(from: Target, to: Target, timeout_ms: u64) -> Request {
    Request::Drag {
        from,
        to,
        steps: 10,
        button: MouseButton::Left,
        modifiers: Vec::new(),
        timeout_ms,
        settle_ms: 2000,
    }
}

#[test]
fn a_right_drag_holds_the_right_button_and_drops_nothing() {
    let line = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let right = Request::Drag {
            from: id("chip"),
            to: id("dropzone"),
            steps: 10,
            button: MouseButton::Right,
            modifiers: Vec::new(),
            timeout_ms: 2000,
            settle_ms: 2000,
        };
        let reply = pilot.handle(right, &mut h.cx).await;
        assert!(
            matches!(reply, Outcome::Ok(Reply::Drag { .. })),
            "{reply:?}"
        );
        h.demo.read_with(&h.cx, |d, cx| d.state_line(cx))
    });
    // A GPUI drag starts only on the left button, so the chip is not dropped.
    assert_eq!(line, "clicks=0 name=\"\" held=right");
}

#[test]
fn drag_moves_the_chip_onto_the_dropzone_and_names_both_ends() {
    let (outcome, dropped) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let outcome = pilot
            .handle(drag(id("chip"), id("dropzone"), 2000), &mut h.cx)
            .await;
        (outcome, h.demo.read_with(&h.cx, |d, _| d.dropped))
    });
    let Outcome::Ok(Reply::Drag { from, to, settled }) = outcome else {
        panic!("unexpected: {outcome:?}");
    };
    assert!(ends_with(&from.path, "chip"), "{from:?}");
    assert!(ends_with(&to.path, "dropzone"), "{to:?}");
    assert!(settled);
    assert_eq!(dropped, 1);
}

#[test]
fn a_drag_to_a_missing_end_fails_and_one_to_empty_space_drops_nothing() {
    let (missed, outcome, dropped) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let missed = pilot
            .handle(drag(id("chip"), id("nowhere"), 0), &mut h.cx)
            .await;
        let empty = Target::Point { x: 620.0, y: 390.0 };
        let outcome = pilot.handle(drag(id("chip"), empty, 2000), &mut h.cx).await;
        (missed, outcome, h.demo.read_with(&h.cx, |d, _| d.dropped))
    });
    let Outcome::Err(error) = missed else {
        panic!("expected ElementNotFound: {missed:?}");
    };
    assert_eq!(error.code, ErrorCode::ElementNotFound);
    assert!(
        matches!(outcome, Outcome::Ok(Reply::Drag { .. })),
        "{outcome:?}"
    );
    assert_eq!(dropped, 0);
}

#[test]
fn a_disabled_drag_source_is_refused_before_any_press() {
    let (outcome, dropped) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let outcome = pilot
            .handle(drag(id("locked"), id("dropzone"), 0), &mut h.cx)
            .await;
        (outcome, h.demo.read_with(&h.cx, |d, _| d.dropped))
    });
    let Outcome::Err(error) = outcome else {
        panic!("expected Disabled: {outcome:?}");
    };
    assert_eq!(error.code, ErrorCode::Disabled);
    assert_eq!(dropped, 0);
}

#[test]
fn a_ctrl_scroll_carries_its_modifier_and_none_stays_held() {
    let (line, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        for modifiers in [vec![Modifier::Ctrl], Vec::new()] {
            let scroll = Request::Scroll {
                target: Target::Id {
                    id: vec![Segment::Str("name".into())],
                },
                dx: 0.,
                dy: 40.,
                modifiers,
                wheel: None,
                timeout_ms: 1000,
                settle_ms: 2000,
            };
            let reply = pilot.handle(scroll, &mut h.cx).await;
            assert!(
                matches!(reply, Outcome::Ok(Reply::Scroll { settled: true })),
                "{reply:?}"
            );
        }
        let after = h
            .window
            .update(&mut h.cx, |_, window, _| window.modifiers())
            .expect("the demo window is open");
        (h.demo.read_with(&h.cx, |d, cx| d.state_line(cx)), after)
    });
    assert_eq!(line, "clicks=0 name=\"\" scrolled zoom=1");
    assert_eq!(
        after,
        gpui_kit::Modifiers::default(),
        "no modifier stays held"
    );
}

#[test]
fn a_modifier_click_delivers_them_on_press_and_release_and_a_plain_one_clears_them() {
    let (lines, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let mut lines = Vec::new();
        for modifiers in [vec![Modifier::Ctrl, Modifier::Shift], Vec::new()] {
            let click = Request::Click {
                target: id("count"),
                button: MouseButton::Left,
                count: 1,
                modifiers,
                timeout_ms: 1000,
                settle_ms: 2000,
            };
            let reply = pilot.handle(click, &mut h.cx).await;
            assert!(
                matches!(reply, Outcome::Ok(Reply::Click { .. })),
                "{reply:?}"
            );
            lines.push(h.demo.read_with(&h.cx, |d, cx| d.state_line(cx)));
        }
        let after = h
            .window
            .update(&mut h.cx, |_, window, _| window.modifiers())
            .expect("the demo window is open");
        (lines, after)
    });
    assert_eq!(lines[0], "clicks=1 name=\"\" down=ctrl+shift up=ctrl+shift");
    assert_eq!(lines[1], "clicks=2 name=\"\"");
    assert_eq!(
        after,
        gpui_kit::Modifiers::default(),
        "no modifier stays held"
    );
}

#[test]
fn a_modifier_drag_holds_them_from_the_press_to_the_drop() {
    let (line, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let drag = Request::Drag {
            from: id("chip"),
            to: id("dropzone"),
            steps: 10,
            button: MouseButton::Left,
            modifiers: vec![Modifier::Shift],
            timeout_ms: 2000,
            settle_ms: 2000,
        };
        let reply = pilot.handle(drag, &mut h.cx).await;
        assert!(
            matches!(reply, Outcome::Ok(Reply::Drag { .. })),
            "{reply:?}"
        );
        let after = h
            .window
            .update(&mut h.cx, |_, window, _| window.modifiers())
            .expect("the demo window is open");
        (h.demo.read_with(&h.cx, |d, cx| d.state_line(cx)), after)
    });
    assert_eq!(line, "clicks=0 name=\"\" dropped=1 down=shift up=shift");
    assert_eq!(
        after,
        gpui_kit::Modifiers::default(),
        "no modifier stays held"
    );
}

#[test]
fn hover_waits_for_the_tooltip_it_triggers() {
    let (settled, roles) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        gpui_pilot_host::settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let pilot = Pilot::new(h.platform.clone());
        let hover = Request::Hover {
            target: id("count"),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let Outcome::Ok(Reply::Hover { settled, .. }) = pilot.handle(hover, &mut h.cx).await else {
            panic!("the hover failed");
        };
        // The kit's tooltip popup has a role but no `.test_support()`, so only GPUI's
        // accessibility tree lists it.
        let tree = common::tree_of(&h, &window).await;
        let roles: Vec<gpui_kit::accesskit::Role> =
            tree.nodes.iter().map(|(_, node)| node.role()).collect();
        (settled, roles)
    });
    assert!(settled);
    assert!(
        roles.contains(&gpui_kit::accesskit::Role::Tooltip),
        "no tooltip after hover: {roles:?}"
    );
}

#[test]
fn a_hover_with_a_small_budget_still_settles() {
    let outcome = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        gpui_pilot_host::settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let hover = Request::Hover {
            target: id("paste"),
            timeout_ms: 1000,
            settle_ms: 300,
        };
        Pilot::new(h.platform.clone())
            .handle(hover, &mut h.cx)
            .await
    });
    let Outcome::Ok(Reply::Hover { settled, .. }) = outcome else {
        panic!("{outcome:?}");
    };
    assert!(settled, "the wait must not eat the settle budget");
}

fn drop_on(target: Target, paths: &[&str], cancel: bool) -> Request {
    Request::Drop {
        target,
        paths: paths.iter().map(Into::into).collect(),
        cancel,
        timeout_ms: 2000,
        settle_ms: 2000,
    }
}

#[test]
fn a_drop_delivers_the_paths_to_the_element_under_it() {
    let (outcome, drops, line, dragging) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let request = drop_on(id("files"), &["/tmp/one.json", "/no/such/folder"], false);
        let outcome = pilot.handle(request, &mut h.cx).await;
        let (drops, line, dragging) = h.demo.read_with(&h.cx, |d, cx| {
            (d.file_drops.clone(), d.state_line(cx), cx.has_active_drag())
        });
        (outcome, drops, line, dragging)
    });
    // The drop must end GPUI's drag, or the next mouse move would still carry the paths.
    assert!(!dragging, "the drop left an active drag");
    let Outcome::Ok(Reply::Drop {
        over,
        dropped,
        settled,
        ..
    }) = outcome
    else {
        panic!("unexpected: {outcome:?}");
    };
    assert!(ends_with(&over.path, "files"), "{over:?}");
    assert!(dropped && settled);
    let expected = vec![
        std::path::PathBuf::from("/tmp/one.json"),
        std::path::PathBuf::from("/no/such/folder"),
    ];
    assert_eq!(drops, Some(expected));
    assert_eq!(
        line,
        r#"clicks=0 name="" files=["/tmp/one.json", "/no/such/folder"] file_over=1 file_exits=0"#
    );
}

#[test]
fn a_cancelled_drop_passes_over_the_element_and_drops_nothing() {
    let (outcome, line, dragging) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let outcome = pilot
            .handle(drop_on(id("files"), &["/tmp/one.json"], true), &mut h.cx)
            .await;
        let (line, dragging) = h
            .demo
            .read_with(&h.cx, |d, cx| (d.state_line(cx), cx.has_active_drag()));
        (outcome, line, dragging)
    });
    assert!(!dragging, "the cancelled drop left an active drag");
    assert!(
        matches!(outcome, Outcome::Ok(Reply::Drop { dropped: false, .. })),
        "{outcome:?}"
    );
    assert_eq!(line, r#"clicks=0 name="" file_over=1 file_exits=1"#);
}

#[test]
fn a_drop_with_a_relative_or_no_path_is_refused_before_any_input() {
    let (outcomes, line) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let mut outcomes = Vec::new();
        for paths in [&["relative.json"][..], &[]] {
            outcomes.push(
                pilot
                    .handle(drop_on(id("files"), paths, false), &mut h.cx)
                    .await,
            );
        }
        (outcomes, h.demo.read_with(&h.cx, |d, cx| d.state_line(cx)))
    });
    for outcome in outcomes {
        let Outcome::Err(error) = outcome else {
            panic!("expected InvalidArgument: {outcome:?}");
        };
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
    assert_eq!(line, r#"clicks=0 name="""#);
}

/// Hovers `tipped` until its GPUI tooltip shows, presses `keys` and reads the roles GPUI's
/// accessibility tree lists afterwards, with the modifiers the window holds then.
fn roles_after_hover_then_key(
    keys: &'static str,
) -> (Vec<gpui_kit::accesskit::Role>, gpui_kit::Modifiers) {
    with_demo(640., 400., 1.0, move |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        gpui_pilot_host::settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let pilot = Pilot::new(h.platform.clone());
        let hover = Request::Hover {
            target: id("tipped"),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let Outcome::Ok(Reply::Hover { .. }) = pilot.handle(hover, &mut h.cx).await else {
            panic!("the hover failed");
        };
        let key = Request::Key {
            keys: keys.into(),
            settle_ms: 2000,
        };
        let pressed = pilot.handle(key, &mut h.cx).await;
        assert!(
            matches!(pressed, Outcome::Ok(Reply::Key { .. })),
            "{pressed:?}"
        );
        let held = h
            .window
            .update(&mut h.cx, |_, window, _| window.modifiers())
            .expect("the demo window is open");
        let tree = common::tree_of(&h, &window).await;
        let roles = tree.nodes.iter().map(|(_, node)| node.role()).collect();
        (roles, held)
    })
}

#[test]
fn a_modifier_only_key_keeps_the_tooltip_a_hover_showed() {
    for keys in ["shift", "ctrl-shift"] {
        let (roles, held) = roles_after_hover_then_key(keys);
        assert!(
            roles.contains(&gpui_kit::accesskit::Role::Tooltip),
            "`key {keys}` hid the tooltip: {roles:?}"
        );
        assert_eq!(held, gpui_kit::Modifiers::default(), "{keys} is released");
    }
}

#[test]
fn a_named_key_still_enters_keyboard_modality_and_hides_the_tooltip() {
    let (roles, _) = roles_after_hover_then_key("f13");
    assert!(
        !roles.contains(&gpui_kit::accesskit::Role::Tooltip),
        "a key down hides hover in real GPUI too: {roles:?}"
    );
}
