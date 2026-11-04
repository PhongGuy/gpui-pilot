//! Facts about pilot-demo's fixtures that the agent-ergonomics tests rely on.

mod common;

use std::time::{Duration, Instant};

use common::{Harness, bounds_of, click_id, first_frame, with_demo};
use gpui_kit::{ElementId, Role};
use gpui_pilot_host::{Observed, observe};

fn observed(h: &mut Harness) -> Vec<Observed> {
    h.window
        .update(&mut h.cx, |_, window, _| observe(window))
        .expect("the demo window is open")
}

fn last_is(o: &Observed, id: &str) -> bool {
    o.path.last() == Some(&ElementId::Name(id.to_string().into()))
}

#[test]
fn the_menu_popover_lists_its_items() {
    let mut items = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        assert!(click_id(&mut h, "menu").await.settled);
        observed(&mut h)
            .into_iter()
            .filter(|o| o.role == Some(Role::MenuItem) && o.visible)
            .filter_map(|o| o.label)
            .collect::<Vec<_>>()
    });
    items.sort();
    assert_eq!(items, ["Alpha", "Beta"]);
}

#[test]
fn the_cover_is_an_overlay_that_swallows_a_raw_click() {
    let (clicks, role, covers) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let centre = bounds_of(&mut h, "covered").center();
        click_id(&mut h, "covered").await;
        let seen = observed(&mut h);
        let cover = seen
            .iter()
            .find(|o| last_is(o, "cover"))
            .expect("the cover is observed")
            .clone();
        let clicks = h.demo.read_with(&h.cx, |d, _| d.covered_clicks);
        (
            clicks,
            cover.role,
            cover.visible && cover.bounds.contains(&centre),
        )
    });
    assert_eq!(clicks, 0, "the cover takes the click");
    assert_eq!(role, Some(Role::Dialog));
    assert!(covers, "the cover spans the covered button's centre");
}

#[test]
fn a_plain_thread_reveals_slow_done_after_settle_returns() {
    let (settled, early, late, elapsed) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let started = Instant::now();
        let settled = click_id(&mut h, "slow").await.settled;
        let early = h.demo.read_with(&h.cx, |d, _| d.slow_done);
        let mut late = false;
        for _ in 0..100 {
            late = h.demo.read_with(&h.cx, |d, _| d.slow_done);
            if late {
                break;
            }
            h.cx.background_executor()
                .timer(Duration::from_millis(20))
                .await;
        }
        (settled, early, late, started.elapsed())
    });
    assert!(
        settled,
        "settle cannot see the thread, so it returns at once"
    );
    assert!(!early, "slow-done is not there when settle returns");
    assert!(late, "slow-done arrives within 2 s");
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
}

#[test]
fn a_counted_boot_timer_holds_settle_until_booted() {
    let booted = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        h.demo.update(&mut h.cx, |view, cx| view.boot(cx));
        let window = h.pilot_window();
        let settled =
            gpui_pilot_host::settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        assert!(settled.settled);
        h.demo.read_with(&h.cx, |d, _| d.booted)
    });
    assert!(booted);
}

#[test]
fn the_state_line_reports_clicks_and_the_name() {
    let lines = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let before = h.demo.read_with(&h.cx, |d, cx| d.state_line(cx));
        assert!(click_id(&mut h, "count").await.settled);
        let after = h.demo.read_with(&h.cx, |d, cx| d.state_line(cx));
        (before, after)
    });
    assert_eq!(
        lines,
        (
            "clicks=0 name=\"\"".to_owned(),
            "clicks=1 name=\"\"".to_owned()
        )
    );
}

#[test]
fn select_rows_with_an_observed_render_are_listed_and_clickable() {
    let (rows, chosen, value) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        assert!(click_id(&mut h, "env").await.settled);
        let mut rows: Vec<String> = observed(&mut h)
            .into_iter()
            .filter(|o| o.visible)
            .filter_map(|o| match o.path.last() {
                Some(ElementId::Name(n)) if n.starts_with("env-option-") => Some(n.to_string()),
                _ => None,
            })
            .collect();
        rows.sort();
        click_id(&mut h, "env-option-prod").await;
        let chosen = h
            .demo
            .read_with(&h.cx, |d, cx| d.env.read(cx).selected_value().cloned());
        let value = observed(&mut h)
            .into_iter()
            .find(|o| last_is(o, "env"))
            .expect("the env select is observed")
            .value;
        (rows, chosen, value)
    });
    assert_eq!(rows, ["env-option-dev", "env-option-prod"]);
    assert_eq!(chosen.as_deref(), Some("prod"));
    assert_eq!(
        value.as_deref(),
        Some("prod"),
        "the tree reports the chosen title"
    );
}
