//! Turned on, the headless window's GPUI accessibility tree is kept, latest first.

mod common;

use std::time::Duration;

use common::{first_frame, tree_of, with_demo};
use gpui_kit::QuitMode;
use gpui_kit::accesskit::Role;
use gpui_pilot_host::{Layering, Pilot, node_id, observe, settle};

#[test]
fn the_window_keeps_gpuis_latest_accessibility_tree() {
    let (active, has_root, roles) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let tree = tree_of(&h, &window).await;
        let active = h
            .window
            .update(&mut h.cx, |_, window, _| window.is_a11y_active())
            .expect("the demo window is open");
        let root = tree
            .tree
            .as_ref()
            .expect("every update is a full tree")
            .root;
        let has_root = tree.nodes.iter().any(|(id, _)| *id == root);
        let roles: Vec<Role> = tree.nodes.iter().map(|(_, node)| node.role()).collect();
        (active, has_root, roles)
    });
    assert!(active, "the app sees accessibility as active");
    assert!(has_root);
    assert!(roles.contains(&Role::Button), "{roles:?}");
}

#[test]
fn closing_the_window_drops_its_tree() {
    let (before, after) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        // Removing the last window would otherwise quit the test's app.
        h.cx.update(|cx| cx.set_quit_mode(QuitMode::Explicit));
        let window = h.pilot_window();
        tree_of(&h, &window).await;
        let before = window.a11y_tree().is_some();
        Pilot::new(h.platform.clone()).finish_window_close(&mut h.cx);
        h.cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        (before, window.a11y_tree().is_some())
    });
    assert!(before);
    assert!(!after, "close clears the stored tree");
}

/// Binds the host's `node_id` to GPUI's private `accesskit_node_id`: a GPUI bump that changes
/// the hash or stops pushing roled elements fails here, not in an agent's hover.
#[test]
fn every_roled_element_has_its_own_accessibility_node() {
    let (roled, missing) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let tree = tree_of(&h, &window).await;
        let layering = Layering::from_update(&tree);
        h.window
            .update(&mut h.cx, |_, window, _| {
                let roled: Vec<_> = observe(window)
                    .into_iter()
                    .filter(|o| o.role.is_some())
                    .collect();
                let missing: Vec<String> = roled
                    .iter()
                    .filter(|o| !layering.contains(node_id(&o.path)))
                    .map(|o| format!("{:?}", o.path))
                    .collect();
                (roled.len(), missing)
            })
            .expect("the demo window is open")
    });
    assert!(roled >= 10, "only {roled} roled elements");
    assert!(
        missing.is_empty(),
        "GPUI's node id no longer matches the element path: {missing:#?}"
    );
}
