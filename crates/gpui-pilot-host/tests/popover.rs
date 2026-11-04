//! An open kit popover: GPUI's accessibility tree ranks its content above the rows it covers.

mod common;

use std::time::Duration;

use common::{Harness, bounds_of, first_frame, tree_of, with_demo};
use gpui_kit::ElementId;
use gpui_pilot_host::{Layer, Layering, Observed, Pilot, node_id, observe, segment_of, settle};
use gpui_pilot_protocol::{MouseButton, Outcome, Reply, Request, Segment, Target};

fn click(target: Target) -> Request {
    Request::Click {
        target,
        button: MouseButton::Left,
        count: 1,
        modifiers: Vec::new(),
        timeout_ms: 1000,
        settle_ms: 1000,
    }
}

/// Switches the demo to `--popover` and opens the popover with a click on its trigger.
async fn open_popover(h: &mut Harness, pilot: &Pilot) {
    first_frame(h).await;
    h.demo.update(&mut h.cx, |demo, cx| {
        demo.configure(&["--popover".to_string()]);
        cx.notify();
    });
    let window = h.pilot_window();
    assert!(
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2))
            .await
            .settled
    );
    let trigger = Target::IdSuffix {
        id_suffix: "pop-trigger".into(),
    };
    let opened = pilot.handle(click(trigger), &mut h.cx).await;
    assert!(
        matches!(opened, Outcome::Ok(Reply::Click { .. })),
        "{opened:?}"
    );
}

fn own(observed: &[Observed], id: &str) -> Vec<ElementId> {
    let wanted = Segment::Str(id.into());
    observed
        .iter()
        .find(|o| o.path.last().map(segment_of).as_ref() == Some(&wanted))
        .unwrap_or_else(|| panic!("no element {id}"))
        .path
        .clone()
}

#[test]
fn the_popover_content_ranks_above_the_rows_and_sits_in_its_dialog() {
    let (missing, mark, row) = with_demo(640., 400., 1.0, |mut h| async move {
        let pilot = Pilot::new(h.platform.clone());
        open_popover(&mut h, &pilot).await;
        let tree = tree_of(&h, &h.pilot_window()).await;
        let layering = Layering::from_update(&tree);
        h.window
            .update(&mut h.cx, |_, window, _| {
                let observed = observe(window);
                let missing: Vec<String> = observed
                    .iter()
                    .filter(|o| o.role.is_some() && !layering.contains(node_id(&o.path)))
                    .map(|o| format!("{:?}", o.path))
                    .collect();
                let layer =
                    |id: &str| -> Layer { layering.layer(&own(&observed, id)).expect("ranked") };
                (missing, layer("pop-mark"), layer("under-2"))
            })
            .expect("the demo window is open")
    });
    assert!(missing.is_empty(), "{missing:#?}");
    assert!(
        mark.overlay.is_some(),
        "pop-mark sits in the popover's Dialog"
    );
    assert_eq!(row.overlay, None);
    assert!(mark.rank > row.rank, "{mark:?} {row:?}");
}

fn at(point: gpui_kit::Point<gpui_kit::Pixels>) -> Target {
    Target::Point {
        x: f32::from(point.x),
        y: f32::from(point.y),
    }
}

fn ends_with(path: &[Segment], text: &str) -> bool {
    path.last() == Some(&Segment::Str(text.into()))
}

#[test]
fn a_point_inside_the_popover_hits_its_content() {
    let (hover, clicked, clicks) = with_demo(640., 400., 1.0, |mut h| async move {
        let pilot = Pilot::new(h.platform.clone());
        open_popover(&mut h, &pilot).await;
        let mark = bounds_of(&mut h, "pop-mark").center();
        let item = bounds_of(&mut h, "pop-item").center();
        let hover = Request::Hover {
            target: at(mark),
            timeout_ms: 0,
            settle_ms: 500,
        };
        let hover = pilot.handle(hover, &mut h.cx).await;
        let clicked = pilot.handle(click(at(item)), &mut h.cx).await;
        let clicks = h
            .demo
            .read_with(&h.cx, |demo, _| (demo.pop_clicks, demo.under_clicks));
        (hover, clicked, clicks)
    });
    let Outcome::Ok(Reply::Hover { hit, .. }) = hover else {
        panic!("{hover:?}");
    };
    assert!(ends_with(&hit.path, "pop-mark"), "{:?}", hit.path);
    let Outcome::Ok(Reply::Click { hit, .. }) = clicked else {
        panic!("{clicked:?}");
    };
    assert!(ends_with(&hit.path, "pop-item"), "{:?}", hit.path);
    assert_eq!(clicks, (1, 0));
}
