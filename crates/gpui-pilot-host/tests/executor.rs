mod common;

use std::path::PathBuf;

use common::{first_frame, with_demo, with_demo_placed};
use gpui_kit::{ElementId, Role, point, px};
use gpui_pilot_host::{Pilot, observe};
use gpui_pilot_protocol::{
    Answered, ErrorCode, MouseButton, Outcome, PickAnswer, PromptKind, Reply, Request, Segment,
    Target,
};

fn id(text: &str) -> Target {
    Target::Id {
        id: vec![Segment::Str(text.into())],
    }
}

fn error_code(outcome: Outcome) -> ErrorCode {
    match outcome {
        Outcome::Err(error) => error.code,
        other => panic!("expected an error: {other:?}"),
    }
}

#[test]
fn click_by_id_counts_and_returns_the_node() {
    let (outcome, clicks) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let click = Request::Click {
            target: id("count"),
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let outcome = pilot.handle(click, &mut h.cx).await;
        (outcome, h.demo.read_with(&h.cx, |demo, _| demo.clicks))
    });
    let Outcome::Ok(Reply::Click {
        clicked, settled, ..
    }) = outcome
    else {
        panic!("unexpected: {outcome:?}");
    };
    assert!(settled);
    assert_eq!(clicked.path.last(), Some(&Segment::Str("count".into())));
    assert_eq!(clicks, 1);
}

#[test]
fn type_then_key_edits_the_input() {
    let text = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let typed = Request::Type {
            text: "abc".into(),
            target: Some(id("name")),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        assert!(matches!(
            pilot.handle(typed, &mut h.cx).await,
            Outcome::Ok(Reply::Type {
                settled: true,
                masked: false
            })
        ));
        let key = Request::Key {
            keys: "backspace".into(),
            settle_ms: 2000,
        };
        assert!(matches!(
            pilot.handle(key, &mut h.cx).await,
            Outcome::Ok(Reply::Key { settled: true })
        ));
        h.demo.read_with(&h.cx, |demo, cx| demo.name_text(cx))
    });
    assert_eq!(text, "ab");
}

/// A kit Input's id names a role-less frame whose text lives on its inner editor; the frame is
/// observed with the editor's role and value, as gpui-base's own `find` reports it.
#[test]
fn an_inputs_id_carries_its_editors_role_and_value() {
    let name = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let typed = Request::Type {
            text: "abc".into(),
            target: Some(id("name")),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        assert!(matches!(
            pilot.handle(typed, &mut h.cx).await,
            Outcome::Ok(Reply::Type { settled: true, .. })
        ));
        h.window
            .update(&mut h.cx, |_, window, _| {
                observe(window)
                    .into_iter()
                    .find(|o| o.path.last() == Some(&ElementId::Name("name".into())))
            })
            .expect("the demo window is open")
    });
    let name = name.expect("the name input is observed");
    assert_eq!(name.role, Some(Role::TextInput));
    assert_eq!(name.value.as_deref(), Some("abc"));
}

/// Typing into a masked input reports `masked`, so the server can keep the text out of its
/// replies; typing into a plain input does not.
#[test]
fn typing_reports_whether_the_focused_input_is_masked() {
    let (secret, plain, typed) = with_demo(640., 400., 1.0, |mut h| async move {
        h.demo.update(&mut h.cx, |demo, cx| {
            demo.configure(&["--secret".to_string()]);
            cx.notify();
        });
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let typing = |text: &str, target: &str| Request::Type {
            text: text.into(),
            target: Some(id(target)),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let secret = pilot.handle(typing("hunter2", "secret"), &mut h.cx).await;
        let plain = pilot.handle(typing("abc", "name"), &mut h.cx).await;
        let typed = h.demo.read_with(&h.cx, |demo, cx| demo.secret_text(cx));
        (secret, plain, typed)
    });
    assert_eq!(typed, "hunter2");
    assert!(
        matches!(secret, Outcome::Ok(Reply::Type { masked: true, .. })),
        "{secret:?}"
    );
    assert!(
        matches!(plain, Outcome::Ok(Reply::Type { masked: false, .. })),
        "{plain:?}"
    );
}

#[test]
fn errors_carry_their_codes() {
    let codes = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let missing = Request::Click {
            target: id("nope"),
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 100,
            settle_ms: 100,
        };
        let bad_key = Request::Key {
            keys: "ctrl-a-b".into(),
            settle_ms: 100,
        };
        let never = Request::Tree {
            within: None,
            wait_for: Some(id("nope")),
            timeout_ms: 100,
            settle_ms: 0,
        };
        vec![
            error_code(pilot.handle(missing, &mut h.cx).await),
            error_code(pilot.handle(bad_key, &mut h.cx).await),
            error_code(pilot.handle(never, &mut h.cx).await),
        ]
    });
    assert_eq!(
        codes,
        [
            ErrorCode::ElementNotFound,
            ErrorCode::InvalidKeystroke,
            ErrorCode::Timeout
        ]
    );
}

#[test]
fn tree_lists_the_demo_in_reading_order() {
    let nodes = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let tree = Request::Tree {
            within: None,
            wait_for: Some(id("paste")),
            timeout_ms: 2000,
            settle_ms: 0,
        };
        match Pilot::new(h.platform.clone()).handle(tree, &mut h.cx).await {
            Outcome::Ok(Reply::Tree { nodes, .. }) => nodes,
            other => panic!("unexpected: {other:?}"),
        }
    });
    let order: Vec<_> = nodes
        .iter()
        .filter_map(|n| match n.path.last() {
            Some(Segment::Str(s)) if ["count", "name", "open"].contains(&s.as_str()) => {
                Some(s.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(order, ["count", "name", "open"]);
}

#[test]
fn screenshot_crops_to_the_target_and_writes_a_png() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("count.png");
    let (outcome, png) = with_demo(640., 400., 2.0, move |mut h| async move {
        first_frame(&h).await;
        let shot = Request::Screenshot {
            target: Some(id("count")),
            path: path.clone(),
        };
        let outcome = Pilot::new(h.platform.clone()).handle(shot, &mut h.cx).await;
        (outcome, image::open(&path).unwrap().to_rgba8())
    });
    let Outcome::Ok(Reply::Screenshot { w, h, scale }) = outcome else {
        panic!("unexpected: {outcome:?}");
    };
    assert_eq!((png.width(), png.height()), (w, h));
    assert_eq!(scale, 2.0);
    assert!(
        w < 1280 && h < 800 && w > h,
        "cropped to the button: {w}x{h}"
    );
}

#[test]
fn pick_file_and_clipboard_go_through_the_platform() {
    let (pick, clip) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let pick = Request::PickFile {
            answer: PickAnswer::Cancel,
            settle_ms: 100,
            timeout_ms: None,
        };
        let clip = Request::Clipboard {
            set: Some("x".into()),
            settle_ms: 100,
        };
        (
            pilot.handle(pick, &mut h.cx).await,
            pilot.handle(clip, &mut h.cx).await,
        )
    });
    assert!(matches!(
        pick,
        Outcome::Ok(Reply::PickFile {
            answered: Answered::Queued,
            prompt: None,
            ..
        })
    ));
    let Outcome::Ok(Reply::Clipboard { text, .. }) = clip else {
        panic!("unexpected: {clip:?}");
    };
    assert_eq!(text.as_deref(), Some("x"));
}

#[test]
fn tree_reports_a_waiting_dialog_and_a_queued_answer() {
    let (waiting, answered, queued) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let tree = || Request::Tree {
            within: None,
            wait_for: None,
            timeout_ms: 0,
            settle_ms: 0,
        };
        let cancel = || Request::PickFile {
            answer: PickAnswer::Cancel,
            settle_ms: 100,
            timeout_ms: None,
        };
        let click = Request::Click {
            target: id("open"),
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 500,
        };
        pilot.handle(click, &mut h.cx).await;
        let waiting = pilot.handle(tree(), &mut h.cx).await;
        pilot.handle(cancel(), &mut h.cx).await;
        let answered = pilot.handle(tree(), &mut h.cx).await;
        pilot.handle(cancel(), &mut h.cx).await;
        let queued = pilot.handle(tree(), &mut h.cx).await;
        (waiting, answered, queued)
    });
    let dialog = |outcome: &Outcome| match outcome {
        Outcome::Ok(Reply::Tree {
            prompt,
            pick_queued,
            ..
        }) => (prompt.as_ref().map(|p| p.kind), *pick_queued),
        other => panic!("unexpected: {other:?}"),
    };
    assert_eq!(dialog(&waiting), (Some(PromptKind::Open), false));
    assert_eq!(dialog(&answered), (None, false));
    assert_eq!(dialog(&queued), (None, true));
}

#[test]
fn enter_and_space_press_a_focused_button() {
    let clicks = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        // gpui-component Buttons take focus only by keyboard traversal, never on a click;
        // `count` is the demo's first tab stop. The frame after focusing paints its key handler.
        h.window
            .update(&mut h.cx, |_, window, cx| window.focus_next(cx))
            .expect("the demo window is open");
        let window = h.pilot_window();
        gpui_pilot_host::settle(
            &h.platform,
            &window,
            &h.cx,
            std::time::Duration::from_secs(2),
        )
        .await;
        let pilot = Pilot::new(h.platform.clone());
        let mut seen = Vec::new();
        for keys in ["enter", "space"] {
            let key = Request::Key {
                keys: keys.into(),
                settle_ms: 2000,
            };
            let pressed = pilot.handle(key, &mut h.cx).await;
            assert!(
                matches!(pressed, Outcome::Ok(Reply::Key { .. })),
                "{keys}: {pressed:?}"
            );
            seen.push(h.demo.read_with(&h.cx, |demo, _| demo.clicks));
        }
        seen
    });
    assert_eq!(clicks, [1, 2]);
}

fn tree_nodes(outcome: Outcome) -> Vec<gpui_pilot_protocol::Node> {
    match outcome {
        Outcome::Ok(Reply::Tree { nodes, .. }) => nodes,
        other => panic!("unexpected: {other:?}"),
    }
}

fn whole_tree() -> Request {
    Request::Tree {
        within: None,
        wait_for: None,
        timeout_ms: 0,
        settle_ms: 500,
    }
}

fn str_path(node: &gpui_pilot_protocol::Node) -> Vec<String> {
    node.path
        .iter()
        .map(|s| match s {
            Segment::Str(s) => s.clone(),
            Segment::Int(i) => i.to_string(),
            other => format!("{other:?}"),
        })
        .collect()
}

/// The synthetic rows in `nodes`, grouped by their list segment.
fn synthetic_groups(
    nodes: &[gpui_pilot_protocol::Node],
) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut groups = std::collections::BTreeMap::<String, Vec<String>>::new();
    for p in nodes.iter().map(str_path) {
        if let [.., list, row] = p.as_slice()
            && row.starts_with("a11y-listitem-")
        {
            groups.entry(list.clone()).or_default().push(row.clone());
        }
    }
    groups
}

/// A plain kit Select registers no rows; the pilot lists them from GPUI's accessibility tree
/// and clicks one by its synthetic id.
#[test]
fn a_plain_selects_rows_are_listed_and_clicked_from_the_accessibility_tree() {
    let (rows, waited, chosen) = with_demo(640., 700., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let open = Request::Click {
            target: id("plain"),
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        assert!(matches!(
            pilot.handle(open, &mut h.cx).await,
            Outcome::Ok(Reply::Click { .. })
        ));
        let groups = synthetic_groups(&tree_nodes(pilot.handle(whole_tree(), &mut h.cx).await));
        let (list, rows) = groups
            .into_iter()
            .find(|(_, rows)| rows.len() == 3)
            .expect("a synthetic list with the three plain rows");
        // The popup is deferred to the window root, where it is the only unregistered list.
        assert_eq!(list, "a11y-list-1");
        let second = Target::Id {
            id: vec![
                Segment::Str(list.clone()),
                Segment::Str("a11y-listitem-2".into()),
            ],
        };
        let waited = pilot
            .handle(
                Request::Wait {
                    until: gpui_pilot_protocol::WaitUntil::Visible(second.clone()),
                    timeout_ms: 1000,
                },
                &mut h.cx,
            )
            .await;
        let click = Request::Click {
            target: second,
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        assert!(matches!(
            pilot.handle(click, &mut h.cx).await,
            Outcome::Ok(Reply::Click { .. })
        ));
        let chosen = h
            .demo
            .read_with(&h.cx, |d, cx| d.plain.read(cx).selected_value().cloned());
        (rows, waited, chosen)
    });
    assert_eq!(
        rows,
        ["a11y-listitem-1", "a11y-listitem-2", "a11y-listitem-3"]
    );
    assert!(
        matches!(waited, Outcome::Ok(Reply::Wait { node: Some(_), .. })),
        "{waited:?}"
    );
    assert_eq!(chosen.as_deref(), Some("green"));
}

/// A Select whose rows register themselves adds no synthetic rows.
#[test]
fn registered_select_rows_are_not_listed_twice() {
    let (before, after, options) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let count = |nodes: &[gpui_pilot_protocol::Node]| {
            synthetic_groups(nodes)
                .values()
                .map(Vec::len)
                .sum::<usize>()
        };
        let before = count(&tree_nodes(pilot.handle(whole_tree(), &mut h.cx).await));
        let open = Request::Click {
            target: id("env"),
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        pilot.handle(open, &mut h.cx).await;
        let nodes = tree_nodes(pilot.handle(whole_tree(), &mut h.cx).await);
        let options = nodes
            .iter()
            .filter(|n| {
                str_path(n)
                    .last()
                    .is_some_and(|s| s.starts_with("env-option-"))
            })
            .count();
        (before, count(&nodes), options)
    });
    assert_eq!(options, 2);
    assert_eq!(after, before);
}

/// A window placed away from the screen's corner still reports its shown a11y-only rows as
/// visible, since row bounds are relative to the window's content.
#[test]
fn a11y_rows_are_visible_in_a_window_placed_off_the_corner() {
    let rows = with_demo_placed(640., 700., point(px(200.), px(400.)), |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        let open = Request::Click {
            target: id("plain"),
            button: MouseButton::Left,
            count: 1,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        assert!(matches!(
            pilot.handle(open, &mut h.cx).await,
            Outcome::Ok(Reply::Click { .. })
        ));
        tree_nodes(pilot.handle(whole_tree(), &mut h.cx).await)
            .into_iter()
            .filter(|n| {
                str_path(n)
                    .last()
                    .is_some_and(|s| s.starts_with("a11y-listitem-"))
            })
            .map(|n| (str_path(&n), n.visible))
            .collect::<Vec<_>>()
    });
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.iter().all(|(_, visible)| *visible), "{rows:?}");
}
