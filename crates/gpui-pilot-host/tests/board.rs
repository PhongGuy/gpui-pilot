//! pilot-demo's `--board` canvas: shapes published as GPUI synthetic accessibility leaves,
//! listed and driven by id like registered elements.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{Harness, first_frame, tree_of, with_demo};
use gpui_kit::accesskit::{Node, NodeId, Rect};
use gpui_kit::base::test_support::snapshots;
use gpui_kit::{AppContext as _, Bounds, ElementId, Pixels, Point, point, px};
use gpui_pilot_host::Pilot;
use gpui_pilot_host::{click_events, dispatch_events, node_id, settle};
use gpui_pilot_protocol::{
    MouseButton, Node as WireNode, Outcome, Reply, Request, Segment, Target,
};

/// The board's leaves in push order: shapes bottom to top, then their handles.
const LEAVES: [&str; 8] = [
    "shape-1",
    "label-1",
    "shape-2",
    "shape-3",
    "shape-4",
    "handle-1-se",
    "handle-2-se",
    "handle-3-se",
];

/// Switches the demo to the board fixture and waits for it to settle.
async fn open_board(h: &mut Harness) {
    h.demo.update(&mut h.cx, |demo, cx| {
        demo.configure(&["--board".to_string()]);
        cx.notify();
    });
    first_frame(h).await;
    let window = h.pilot_window();
    settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
}

/// The board's registered path and logical bounds.
fn board_of(h: &mut Harness) -> (Vec<ElementId>, Bounds<Pixels>) {
    let wanted = ElementId::Name("board".into());
    h.window
        .update(&mut h.cx, |_, window, _| {
            let board = snapshots(window)
                .into_iter()
                .find(|s| s.path().last() == Some(&wanted))
                .expect("the board is registered");
            (board.path().to_vec(), board.bounds())
        })
        .expect("the demo window is open")
}

/// Clicks `at` (window coordinates) with raw events, bypassing the pilot's resolution.
async fn press_at(h: &mut Harness, at: Point<Pixels>) {
    let events = click_events(at, gpui_kit::MouseButton::Left, 1, Default::default());
    h.cx.update_window(h.window.into(), |_, window, cx| {
        dispatch_events(window, cx, events)
    })
    .expect("the demo window is open");
    let window = h.pilot_window();
    settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

#[test]
fn the_boards_shapes_are_leaves_of_its_node_in_push_order_at_device_scale() {
    let (origin, leaves) = with_demo(640., 400., 2.0, |mut h| async move {
        open_board(&mut h).await;
        let (path, board) = board_of(&mut h);
        let window = h.pilot_window();
        let tree = tree_of(&h, &window).await;
        let nodes: HashMap<NodeId, &Node> = tree.nodes.iter().map(|(id, n)| (*id, n)).collect();
        let board_node = nodes
            .get(&node_id(&path))
            .expect("the board has an accessibility node");
        let leaves: Vec<(String, usize, Option<Rect>, Option<bool>)> = board_node
            .children()
            .iter()
            .map(|child| {
                let n = nodes.get(child).expect("every child is in the update");
                let author = n.author_id().unwrap_or_default().to_string();
                (author, n.children().len(), n.bounds(), n.is_selected())
            })
            .collect();
        (board.origin, leaves)
    });
    let names: Vec<&str> = leaves.iter().map(|l| l.0.as_str()).collect();
    assert_eq!(names, LEAVES);
    assert!(
        leaves.iter().all(|l| l.1 == 0),
        "synthetic children are leaves"
    );
    let (ox, oy) = (
        f64::from(f32::from(origin.x)),
        f64::from(f32::from(origin.y)),
    );
    let card = leaves[2].2.expect("shape-2 has bounds");
    // Logical (260, 20) 100 x 80 inside the board, doubled at scale 2.
    assert!(
        close(card.x0, (ox + 260.) * 2.) && close(card.y0, (oy + 20.) * 2.),
        "{card:?}"
    );
    assert!(
        close(card.x1, (ox + 360.) * 2.) && close(card.y1, (oy + 100.) * 2.),
        "{card:?}"
    );
    assert_eq!(leaves[0].3, Some(false), "shapes carry their selection");
    assert_eq!(leaves[5].3, None, "handles carry none");
}

#[test]
fn a_raw_press_selects_the_topmost_shape_in_push_order() {
    let picked = with_demo(640., 400., 1.0, |mut h| async move {
        open_board(&mut h).await;
        let (_, board) = board_of(&mut h);
        let at = |x: f32, y: f32| point(board.origin.x + px(x), board.origin.y + px(y));
        // (100, 80) is inside both the frame and the label drawn over it.
        press_at(&mut h, at(100., 80.)).await;
        press_at(&mut h, at(310., 60.)).await;
        h.demo.read_with(&h.cx, |d, _| {
            let selected = |id: &str| d.shape(id).expect("a board shape").selected;
            (
                selected("label-1"),
                selected("shape-1"),
                selected("shape-2"),
            )
        })
    });
    assert_eq!(picked, (true, false, true));
}

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

async fn tree_nodes(pilot: &Pilot, h: &mut Harness) -> Vec<WireNode> {
    let whole = Request::Tree {
        within: None,
        wait_for: None,
        timeout_ms: 0,
        settle_ms: 500,
    };
    match pilot.handle(whole, &mut h.cx).await {
        Outcome::Ok(Reply::Tree { nodes, .. }) => nodes,
        other => panic!("unexpected: {other:?}"),
    }
}

fn own(node: &WireNode) -> Option<&str> {
    match node.path.last() {
        Some(Segment::Str(s)) => Some(s),
        _ => None,
    }
}

fn named<'a>(nodes: &'a [WireNode], name: &str) -> &'a WireNode {
    let found: Vec<_> = nodes.iter().filter(|n| own(n) == Some(name)).collect();
    assert_eq!(found.len(), 1, "expected one `{name}` in the tree");
    found[0]
}

fn selected(h: &Harness, id: &str) -> bool {
    h.demo
        .read_with(&h.cx, |d, _| d.shape(id).expect("a board shape").selected)
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.01
}

#[test]
fn tree_lists_each_shape_under_the_board_with_its_state_and_logical_bounds() {
    let nodes = with_demo(640., 400., 2.0, |mut h| async move {
        open_board(&mut h).await;
        let pilot = Pilot::new(h.platform.clone());
        tree_nodes(&pilot, &mut h).await
    });
    let board = named(&nodes, "board").bounds.clone();
    for leaf in LEAVES {
        let node = named(&nodes, leaf);
        assert_eq!(
            node.path.iter().rev().nth(1),
            Some(&Segment::Str("board".into())),
            "{leaf}: {:?}",
            node.path
        );
        assert!(node.visible, "{leaf}");
    }
    let card = named(&nodes, "shape-2");
    assert_eq!(card.role.as_deref(), Some("Group"));
    assert_eq!(card.name.as_deref(), Some("Card B"));
    assert_eq!(card.selected, Some(false));
    let b = &card.bounds;
    assert!(
        near(b.x, board.x + 260.) && near(b.y, board.y + 20.) && near(b.w, 100.) && near(b.h, 80.),
        "{b:?}"
    );
    let handle = named(&nodes, "handle-2-se");
    assert_eq!(handle.selected, None);
    let hb = &handle.bounds;
    assert!(
        near(hb.x, board.x + 356.) && near(hb.y, board.y + 96.) && near(hb.w, 8.),
        "{hb:?}"
    );
}

#[test]
fn clicking_a_shape_by_id_toggles_its_selection() {
    let (outcome, chosen, listed) = with_demo(640., 400., 1.0, |mut h| async move {
        open_board(&mut h).await;
        let pilot = Pilot::new(h.platform.clone());
        let outcome = pilot.handle(click(id("shape-2"), 1000), &mut h.cx).await;
        let listed = named(&tree_nodes(&pilot, &mut h).await, "shape-2").selected;
        (outcome, selected(&h, "shape-2"), listed)
    });
    let Outcome::Ok(Reply::Click { clicked, .. }) = outcome else {
        panic!("unexpected: {outcome:?}")
    };
    assert_eq!(own(&clicked), Some("shape-2"));
    assert!(chosen);
    assert_eq!(listed, Some(true));
}

#[test]
fn dragging_a_handle_by_id_grows_its_shape() {
    let (outcome, size, listed) = with_demo(640., 400., 1.0, |mut h| async move {
        open_board(&mut h).await;
        let pilot = Pilot::new(h.platform.clone());
        let board = named(&tree_nodes(&pilot, &mut h).await, "board")
            .bounds
            .clone();
        let drag = Request::Drag {
            from: id("handle-2-se"),
            to: Target::Point {
                x: board.x + 400.,
                y: board.y + 130.,
            },
            steps: 10,
            button: MouseButton::Left,
            modifiers: Vec::new(),
            timeout_ms: 1000,
            settle_ms: 2000,
        };
        let outcome = pilot.handle(drag, &mut h.cx).await;
        let size = h.demo.read_with(&h.cx, |d, _| {
            let s = d.shape("shape-2").expect("a board shape");
            (s.w, s.h)
        });
        let listed = named(&tree_nodes(&pilot, &mut h).await, "shape-2")
            .bounds
            .clone();
        (outcome, size, listed)
    });
    assert!(
        matches!(outcome, Outcome::Ok(Reply::Drag { .. })),
        "{outcome:?}"
    );
    assert_eq!(size, (140., 110.));
    assert!(near(listed.w, 140.) && near(listed.h, 110.), "{listed:?}");
}

#[test]
fn after_a_pan_a_click_by_id_lands_at_the_new_bounds() {
    let (before, after, chosen, pan) = with_demo(640., 400., 1.0, |mut h| async move {
        open_board(&mut h).await;
        let pilot = Pilot::new(h.platform.clone());
        let before = named(&tree_nodes(&pilot, &mut h).await, "shape-2")
            .bounds
            .clone();
        let panned = pilot.handle(click(id("pan"), 1000), &mut h.cx).await;
        assert!(
            matches!(panned, Outcome::Ok(Reply::Click { .. })),
            "{panned:?}"
        );
        let after = match pilot.handle(click(id("shape-2"), 1000), &mut h.cx).await {
            Outcome::Ok(Reply::Click { clicked, .. }) => clicked.bounds,
            other => panic!("unexpected: {other:?}"),
        };
        let pan = h.demo.read_with(&h.cx, |d, _| d.pan);
        (before, after, selected(&h, "shape-2"), pan)
    });
    assert_eq!(pan, (30., 20.));
    assert!(
        near(after.x, before.x + 30.) && near(after.y, before.y + 20.),
        "{before:?} -> {after:?}"
    );
    assert!(chosen, "the click reached the panned shape");
}

#[test]
fn the_default_demo_lists_each_author_id_button_once() {
    let nodes = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        tree_nodes(&pilot, &mut h).await
    });
    for name in [
        "count", "name", "chip", "dropzone", "ask", "zoom", "files", "viewport",
    ] {
        named(&nodes, name);
    }
}
