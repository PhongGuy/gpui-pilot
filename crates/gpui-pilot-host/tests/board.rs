//! pilot-demo's `--board` canvas: shapes published as GPUI synthetic accessibility leaves,
//! listed and driven by id like registered elements.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use common::{Harness, first_frame, tree_of, with_demo};
use gpui_kit::accesskit::{Node, NodeId, Rect};
use gpui_kit::base::test_support::snapshots;
use gpui_kit::{AppContext as _, Bounds, ElementId, Pixels, Point, point, px};
use gpui_pilot_host::{click_events, dispatch_events, node_id, settle};

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
