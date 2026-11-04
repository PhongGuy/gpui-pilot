mod common;

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use common::{click_id, first_frame, with_demo, with_demo_at};
use gpui_kit::base::test_support::snapshots;
use gpui_kit::{AppContext as _, AsyncApp, ElementId};
use gpui_pilot_host::{
    PilotPlatform, QUIET_LAYOUT_MS, dispatch_keystrokes, settle, text_keystrokes, ticks_for,
};

const FORTY: &str = "The quick brown fox jumps over a lazy do";

#[test]
fn a_click_and_a_forty_character_type_settle_without_the_input_sustain() {
    let (click, typed, clicks, text, render) = with_demo(640., 400., 1.0, |mut h| async move {
        let render = first_frame(&h).await.render_time;
        let click = click_id(&mut h, "count").await;
        click_id(&mut h, "name").await;
        let keys = text_keystrokes(FORTY);
        // `update_window` leaves the root view unleased: a dirty window redraws it before
        // dispatching a key, and `WindowHandle::update` would hold it.
        h.cx.update_window(h.window.into(), |_, window, cx| {
            dispatch_keystrokes(window, cx, keys)
        })
        .expect("the demo window is open");
        let window = h.pilot_window();
        let typed = settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        let clicks = h.demo.read_with(&h.cx, |demo, _| demo.clicks);
        let text = h.demo.read_with(&h.cx, |demo, cx| demo.name_text(cx));
        (click, typed, clicks, text, render)
    });
    eprintln!("first render+hash {render:?}; click {click:?}; type {typed:?}");
    assert_eq!(FORTY.chars().count(), 40);
    assert!(click.settled, "click: {click:?}");
    assert_eq!(clicks, 1);
    assert!(typed.settled, "type: {typed:?}");
    assert_eq!(text, FORTY);
    assert!(
        typed.elapsed < Duration::from_millis(200),
        "the type must settle within 200 ms of the last key, not after the 1 s sustain: {typed:?}"
    );
}

#[test]
fn a_prompt_answer_settles_only_after_the_app_takes_it() {
    let opened = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        click_id(&mut h, "open").await;
        h.platform
            .answer_prompt(Some(vec!["/tmp/pilot-open.txt".into()]));
        let window = h.pilot_window();
        let done = settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        assert!(done.settled, "{done:?}");
        h.demo.read_with(&h.cx, |demo, _| demo.opened.clone())
    });
    assert_eq!(opened, Some(vec!["/tmp/pilot-open.txt".into()]));
}

#[test]
fn a_debounced_change_is_waited_for() {
    let (click, late) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let click = click_id(&mut h, "debounce").await;
        let late = ElementId::Name("late".into());
        let shown = h
            .window
            .update(&mut h.cx, |_, window, _| {
                snapshots(window)
                    .iter()
                    .any(|s| s.path().last() == Some(&late))
            })
            .expect("the demo window is open");
        (click, shown)
    });
    assert!(click.settled, "{click:?}");
    assert!(
        late,
        "settle returned before the 100 ms debounce showed `late`"
    );
}

#[test]
fn a_blinking_cursor_does_not_hold_settle_off() {
    let (focus, again) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let focus = click_id(&mut h, "name").await;
        let window = h.pilot_window();
        let again = settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await;
        (focus, again)
    });
    assert!(focus.settled && again.settled, "{focus:?} {again:?}");
    assert!(again.elapsed < Duration::from_millis(600), "{again:?}");
}

/// Waits `duration` on a plain thread, so the wait itself is not app work. A wait on the
/// counted executor is a background task whose worker still holds its count after waking us.
async fn pause(duration: Duration) {
    let (wake, woken) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        wake.send(()).ok();
    });
    woken.await.ok();
}

/// Whether the pending count reaches zero within `deadline`, polled in short pauses.
async fn drained(platform: &PilotPlatform, deadline: Duration) -> bool {
    let until = Instant::now() + deadline;
    while platform.pending_work() != 0 {
        if Instant::now() >= until {
            return false;
        }
        pause(Duration::from_millis(5)).await;
    }
    true
}

#[test]
fn pending_work_counts_queued_tasks_busy_threads_and_near_timers() {
    let seen = common::with_platform(|platform, cx| async move {
        let timers = cx.background_executor().clone();
        let mut seen = Vec::new();
        for _ in 0..3 {
            cx.spawn(async |_: &mut AsyncApp| {}).detach();
        }
        seen.push(platform.pending_work() >= 3);
        seen.push(drained(&platform, Duration::from_millis(300)).await);
        let near = timers.timer(Duration::from_millis(100));
        cx.spawn(async move |_: &mut AsyncApp| near.await).detach();
        pause(Duration::from_millis(40)).await;
        seen.push(platform.pending_work() >= 1);
        seen.push(drained(&platform, Duration::from_millis(300)).await);
        // The far timer is still outstanding when the count drains, so zero means it was
        // registered and left uncounted rather than not yet scheduled.
        let fired = Rc::new(Cell::new(false));
        let far = timers.timer(Duration::from_millis(500));
        let far_fired = fired.clone();
        cx.spawn(async move |_: &mut AsyncApp| {
            far.await;
            far_fired.set(true);
        })
        .detach();
        seen.push(drained(&platform, Duration::from_millis(300)).await && !fired.get());
        timers
            .spawn(async { std::thread::sleep(Duration::from_millis(100)) })
            .detach();
        std::thread::sleep(Duration::from_millis(30));
        seen.push(platform.pending_work() >= 1);
        seen.push(drained(&platform, Duration::from_millis(300)).await);
        seen
    });
    let names = [
        "queued tasks count",
        "nothing pending once they ran",
        "a 100 ms timer counts",
        "nothing pending once it fired",
        "a 500 ms timer does not count",
        "a busy background task counts",
        "nothing pending once it finished",
    ];
    for (ok, name) in seen.iter().zip(names) {
        assert!(ok, "{name}: {seen:?}");
    }
}

#[test]
fn a_budget_inside_the_horizon_still_settles_an_idle_window() {
    let quick = with_demo(640., 400., 1.0, |h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::from_millis(150)).await
    });
    assert!(
        quick.settled,
        "the budget's own timer held settle off: {quick:?}"
    );
}

#[test]
fn a_spinner_settles_once_the_layout_is_quiet() {
    let (spin, click, clicks) = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        let spin = click_id(&mut h, "spin").await;
        let click = click_id(&mut h, "count").await;
        (spin, click, h.demo.read_with(&h.cx, |demo, _| demo.clicks))
    });
    eprintln!("spinner: spin {spin:?}; click {click:?}");
    for (what, s) in [("spin", spin), ("click", click)] {
        assert!(s.settled && s.animating, "{what}: {s:?}");
        assert!(s.ticks >= ticks_for(QUIET_LAYOUT_MS, 60), "{what}: {s:?}");
        assert!(
            s.elapsed < Duration::from_millis(600),
            "{what} must not wait out the 2 s budget: {s:?}"
        );
    }
    assert_eq!(clicks, 1);
}

#[test]
fn a_still_app_settles_on_pixels_not_layout() {
    let click = with_demo(640., 400., 1.0, |mut h| async move {
        first_frame(&h).await;
        click_id(&mut h, "count").await
    });
    assert!(click.settled && !click.animating, "{click:?}");
    assert!(click.ticks < ticks_for(QUIET_LAYOUT_MS, 60), "{click:?}");
}

#[test]
fn a_zero_budget_returns_at_once() {
    let zero = with_demo(640., 400., 1.0, |h| async move {
        first_frame(&h).await;
        let window = h.pilot_window();
        settle(&h.platform, &window, &h.cx, Duration::ZERO).await
    });
    assert!(zero.elapsed < Duration::from_millis(50), "{zero:?}");
}

#[test]
fn a_spinner_settles_at_144_hz_after_the_same_wall_clock() {
    let spin = with_demo_at(640., 400., 1.0, 144, |mut h| async move {
        first_frame(&h).await;
        click_id(&mut h, "spin").await
    });
    eprintln!("spinner at 144 Hz: {spin:?}");
    assert!(spin.settled && spin.animating, "{spin:?}");
    assert!(spin.ticks >= ticks_for(QUIET_LAYOUT_MS, 144), "{spin:?}");
    // 24 consecutive ticks span at least 23 periods of 6.944 ms.
    assert!(spin.elapsed >= Duration::from_millis(150), "{spin:?}");
    assert!(spin.elapsed < Duration::from_millis(1000), "{spin:?}");
}
