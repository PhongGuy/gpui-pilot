//! Runs an app on a fresh `PilotPlatform` on the test thread and hands the test body an
//! async context; the app quits when the body returns.
#![allow(dead_code)]

use std::{future::Future, rc::Rc, sync::mpsc, time::Duration};

use gpui_kit::accesskit::TreeUpdate;
use gpui_kit::base::Root;
use gpui_kit::base::test_support::snapshots;
use gpui_kit::{
    App, AppContext as _, Application, AsyncApp, Bounds, ElementId, Entity, MouseButton, Pixels,
    WindowHandle, px, size,
};
use gpui_pilot_host::{Frame, PilotPlatform, PilotWindow, Settled};
use pilot_demo::DemoView;

/// Machine load for timing assertions; see `test-support/load.rs`.
#[path = "../../../../test-support/load.rs"]
pub mod load;

fn run_app<R: Send + 'static>(
    platform: Rc<PilotPlatform>,
    start: impl FnOnce(&mut App, mpsc::Sender<R>) + 'static,
) -> R {
    let (done, result) = mpsc::channel();
    Application::with_platform(platform)
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| start(cx, done));
    result
        .recv()
        .expect("the test body finished before the app quit")
}

pub fn with_platform<F, Fut, R>(body: F) -> R
where
    F: FnOnce(Rc<PilotPlatform>, AsyncApp) -> Fut + 'static,
    Fut: Future<Output = R> + 'static,
    R: Send + 'static,
{
    let platform = PilotPlatform::new(size(px(640.), px(400.)), 1.0, 60);
    run_app(platform.clone(), move |cx, done| {
        let body_cx = cx.to_async();
        cx.spawn(async move |cx| {
            done.send(body(platform, body_cx).await).ok();
            cx.update(|cx| cx.quit());
        })
        .detach();
    })
}

pub struct Harness {
    pub platform: Rc<PilotPlatform>,
    pub window: WindowHandle<Root>,
    pub demo: Entity<DemoView>,
    pub cx: AsyncApp,
}

impl Harness {
    pub fn pilot_window(&self) -> PilotWindow {
        self.platform
            .main_window()
            .expect("pilot-demo has a window")
    }
}

/// Runs pilot-demo at 60 Hz, a period every dev-build frame fits in.
pub fn with_demo<F, Fut, R>(width: f32, height: f32, scale: f32, body: F) -> R
where
    F: FnOnce(Harness) -> Fut + 'static,
    Fut: Future<Output = R> + 'static,
    R: Send + 'static,
{
    with_demo_at(width, height, scale, 60, body)
}

pub fn with_demo_at<F, Fut, R>(width: f32, height: f32, scale: f32, refresh_hz: u32, body: F) -> R
where
    F: FnOnce(Harness) -> Fut + 'static,
    Fut: Future<Output = R> + 'static,
    R: Send + 'static,
{
    let platform = PilotPlatform::new(size(px(width), px(height)), scale, refresh_hz);
    run_app(platform.clone(), move |cx, done| {
        pilot_demo::init(cx);
        let (window, demo) = pilot_demo::open(cx).expect("open pilot-demo");
        let harness = Harness {
            platform,
            window,
            demo,
            cx: cx.to_async(),
        };
        cx.spawn(async move |cx| {
            done.send(body(harness).await).ok();
            cx.update(|cx| cx.quit());
        })
        .detach();
    })
}

pub async fn first_frame(h: &Harness) -> Frame {
    for _ in 0..200 {
        if let Some(frame) = h.pilot_window().last_frame().expect("a readback") {
            return frame;
        }
        h.cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
    panic!("no frame was presented within 2 s");
}

/// Logical bounds of the one observed element whose last path segment is `id`.
pub fn bounds_of(h: &mut Harness, id: &str) -> Bounds<Pixels> {
    let wanted = ElementId::Name(id.to_string().into());
    h.window
        .update(&mut h.cx, |_, window, _| {
            let found: Vec<_> = snapshots(window)
                .into_iter()
                .filter(|s| s.path().last() == Some(&wanted))
                .collect();
            assert_eq!(found.len(), 1, "expected one element with id {id}");
            found[0].bounds()
        })
        .expect("the demo window is open")
}

/// Clicks the centre of the element `id` and waits for the app to settle.
pub async fn click_id(h: &mut Harness, id: &str) -> Settled {
    let at = bounds_of(h, id).center();
    let events = gpui_pilot_host::click_events(at, MouseButton::Left, 1, Default::default());
    h.cx.update_window(h.window.into(), |_, window, cx| {
        gpui_pilot_host::dispatch_events(window, cx, events)
    })
    .expect("the demo window is open");
    let window = h.pilot_window();
    gpui_pilot_host::settle(&h.platform, &window, &h.cx, Duration::from_secs(2)).await
}

/// Turns accessibility on, which the pilot otherwise does only inside an `act`, and waits up
/// to 2 s for the window's first tree: activation forces a redraw that sends one.
pub async fn tree_of(h: &Harness, window: &PilotWindow) -> Rc<TreeUpdate> {
    window.set_a11y_active(true);
    for _ in 0..200 {
        if let Some(tree) = window.a11y_tree() {
            return tree;
        }
        h.cx.background_executor()
            .timer(Duration::from_millis(10))
            .await;
    }
    panic!("GPUI sent no accessibility tree within 2 s");
}
