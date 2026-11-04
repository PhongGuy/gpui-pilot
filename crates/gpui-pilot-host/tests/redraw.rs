//! The headless window draws a change the app makes on its own, with no input to wake it.

mod common;

use common::{first_frame, with_demo};
use futures::StreamExt as _;
use gpui_pilot_host::Pilot;
use gpui_pilot_protocol::{Node, Outcome, Reply, Request, Segment};

/// Ticks allowed between the app's change and a frame showing it: the change lands between two
/// ticks, so the next one draws it; the margin is counted in ticks, never in wall time.
const MAX_TICKS: u32 = 3;

fn has(nodes: &[Node], name: &str) -> bool {
    nodes
        .iter()
        .any(|n| n.path.last() == Some(&Segment::Str(name.into())))
}

#[test]
fn a_timer_driven_change_reaches_tree_and_screenshot_without_input() {
    let dir = tempfile::tempdir().unwrap();
    let (before, after) = (dir.path().join("before.png"), dir.path().join("after.png"));
    let (shot_before, shot_after) = (before.clone(), after.clone());
    let (booted_before, ticks) = with_demo(640., 400., 1.0, move |mut h| async move {
        first_frame(&h).await;
        let pilot = Pilot::new(h.platform.clone());
        // A zero settle reads what the frame clock last drew and draws nothing itself.
        let tree = || Request::Tree {
            within: None,
            wait_for: None,
            timeout_ms: 0,
            settle_ms: 0,
        };
        let nodes = |outcome| match outcome {
            Outcome::Ok(Reply::Tree { nodes, .. }) => nodes,
            other => panic!("unexpected: {other:?}"),
        };
        let shot = |path: &std::path::Path| Request::Screenshot {
            target: None,
            path: path.to_path_buf(),
        };
        let booted_before = has(&nodes(pilot.handle(tree(), &mut h.cx).await), "booted");
        let taken = pilot.handle(shot(&shot_before), &mut h.cx).await;
        assert!(
            matches!(taken, Outcome::Ok(Reply::Screenshot { .. })),
            "{taken:?}"
        );

        // The boot timer flips the state from a spawned task; nothing is dispatched.
        let mut frames = h.pilot_window().ticks();
        h.demo.update(&mut h.cx, |view, cx| view.boot(cx));
        let mut ticks = 0;
        loop {
            frames.next().await.expect("the window stays open");
            if !h.demo.read_with(&h.cx, |d, _| d.booted) {
                continue;
            }
            ticks += 1;
            if has(&nodes(pilot.handle(tree(), &mut h.cx).await), "booted") || ticks > MAX_TICKS {
                break;
            }
        }
        let taken = pilot.handle(shot(&shot_after), &mut h.cx).await;
        assert!(
            matches!(taken, Outcome::Ok(Reply::Screenshot { .. })),
            "{taken:?}"
        );
        (booted_before, ticks)
    });
    assert!(!booted_before, "the demo starts unbooted");
    assert!(
        ticks <= MAX_TICKS,
        "tree showed the change only after {ticks} ticks"
    );
    let read = |path: &std::path::Path| image::open(path).unwrap().to_rgba8();
    assert_ne!(
        read(&before),
        read(&after),
        "the screenshot shows the booted button"
    );
}
