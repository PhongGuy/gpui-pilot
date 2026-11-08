//! `PilotWindow`: a platform window with no surface that renders every presented scene
//! offscreen on the GPU and keeps the latest frame, driven by a frame tick on absolute
//! deadlines at the launch refresh rate.

use std::{
    cell::RefCell,
    hash::Hasher,
    rc::{Rc, Weak},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::channel::{mpsc, oneshot};
use gpui_kit::{
    AnyWindowHandle, BackgroundExecutor, Bounds, Capslock, DevicePixels, DispatchEventResult,
    ForegroundExecutor, GpuSpecs, Modifiers, Pixels, PlatformAtlas, PlatformDisplay,
    PlatformHeadlessRenderer, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, Scene, Size, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowVisibility,
};
use gpui_pilot_protocol::Recorder;
use image::RgbaImage;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use rustc_hash::FxHasher;

/// One presented frame: its pixels at device resolution, its position in the window's
/// present order, and how long its present took to render (and read back, when it was read
/// back as it was drawn). Its pixel hash is computed on demand.
#[derive(Clone)]
#[non_exhaustive]
pub struct Frame {
    pub image: Arc<RgbaImage>,
    hash: Arc<OnceLock<u64>>,
    pub seq: u64,
    pub render_time: Duration,
}

impl Frame {
    /// A hash of the frame's pixels, computed on first use and shared by every clone.
    pub fn hash(&self) -> u64 {
        *self.hash.get_or_init(|| {
            // Whole native-endian words first, then the bytes past the last whole word.
            let bytes: &[u8] = self.image.as_raw();
            let mut hasher = FxHasher::default();
            let (words, rest) = bytes.as_chunks::<8>();
            for word in words {
                hasher.write_u64(u64::from_ne_bytes(*word));
            }
            hasher.write(rest);
            hasher.finish()
        })
    }

    #[cfg(test)]
    pub(crate) fn is_hashed(&self) -> bool {
        self.hash.get().is_some()
    }
}

type Callback<T> = Option<Box<T>>;

/// A presented frame rendered on the GPU but not yet read back: a copy of its scene, so the
/// pixels can be rendered again and read once something asks for them.
struct Unread {
    scene: Scene,
    size: Size<DevicePixels>,
    seq: u64,
    render_time: Duration,
}

/// A copy of `scene` that renders to the same pixels; GPUI's own view cache rebuilds scenes
/// the same way.
fn copy_scene(scene: &Scene) -> Scene {
    let mut copy = Scene::default();
    copy.replay(0..scene.len(), scene);
    copy.finish();
    copy
}

struct State {
    handle: AnyWindowHandle,
    bounds: Bounds<Pixels>,
    scale: f32,
    display: Option<Rc<dyn PlatformDisplay>>,
    renderer: Box<dyn PlatformHeadlessRenderer>,
    atlas: Arc<dyn PlatformAtlas>,
    last_frame: Option<Frame>,
    /// The latest present while nothing has asked for its pixels; newer than `last_frame`.
    unread: Option<Unread>,
    readbacks: u64,
    render_error: Option<String>,
    ticks: Vec<mpsc::UnboundedSender<Option<u64>>>,
    last_wake: Option<(Instant, Instant)>,
    /// When the current tick began, so a draw inside it counts the app's work as well.
    tick_started: Option<Instant>,
    recorder: Option<Recorder>,
    activated: bool,
    title: String,
    fullscreen: bool,
    input_handler: Option<PlatformInputHandler>,
    request_frame: Callback<dyn FnMut(RequestFrameOptions)>,
    active_status_change: Callback<dyn FnMut(bool)>,
    should_close: Callback<dyn FnMut() -> bool>,
    close: Callback<dyn FnOnce()>,
    /// GPUI's resize callback, fired by `resize_to` as a compositor's configure would.
    resized: Callback<dyn FnMut(Size<Pixels>, f32)>,
    /// The last pointer position, as of the latest resize; GPUI reads it when it asks after a
    /// resize.
    pointer: Point<Pixels>,
    dialog: Option<(Dialog, oneshot::Sender<usize>)>,
    /// GPUI's accessibility callbacks, called only around an `act` poll's forced frame.
    a11y: Option<gpui_kit::A11yCallbacks>,
    a11y_tree: Option<Rc<gpui_kit::accesskit::TreeUpdate>>,
    a11y_active: bool,
}

/// A prompt the app raised through `Window::prompt`, held for the agent to answer.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq)]
pub struct Dialog {
    pub level: PromptLevel,
    pub message: String,
    pub detail: Option<String>,
    /// `PromptButton::label()` of each answer, in order.
    pub buttons: Vec<String>,
}

/// A pilot-owned window: GPUI draws into it as into any window, and each draw becomes a
/// `Frame`. Clones share one window.
#[derive(Clone)]
pub struct PilotWindow(Rc<RefCell<State>>);

impl PilotWindow {
    pub(crate) fn new(
        handle: AnyWindowHandle,
        bounds: Bounds<Pixels>,
        scale: f32,
        display: Option<Rc<dyn PlatformDisplay>>,
        renderer: Box<dyn PlatformHeadlessRenderer>,
    ) -> Self {
        let atlas = renderer.sprite_atlas();
        Self(Rc::new(RefCell::new(State {
            handle,
            bounds,
            scale,
            display,
            renderer,
            atlas,
            last_frame: None,
            unread: None,
            readbacks: 0,
            render_error: None,
            ticks: Vec::new(),
            last_wake: None,
            tick_started: None,
            recorder: None,
            activated: false,
            title: String::new(),
            fullscreen: false,
            input_handler: None,
            request_frame: None,
            active_status_change: None,
            should_close: None,
            close: None,
            resized: None,
            pointer: Point::default(),
            dialog: None,
            a11y: None,
            a11y_tree: None,
            a11y_active: false,
        })))
    }

    pub(crate) fn downgrade(&self) -> WeakWindow {
        WeakWindow(Rc::downgrade(&self.0))
    }

    /// Ticks the window on deadlines `t0 + n / refresh_hz`, as a compositor's frame clock
    /// would, until GPUI drops it; a tick that overruns skips the deadlines it missed. Only a
    /// weak reference survives each wait, so the ticker never keeps a closed window alive.
    pub(crate) fn start_ticker(
        &self,
        foreground: &ForegroundExecutor,
        timers: BackgroundExecutor,
        refresh_hz: u32,
    ) {
        let weak = self.downgrade();
        let t0 = Instant::now();
        foreground
            .spawn(async move {
                let mut n = 1;
                loop {
                    let deadline = t0 + deadline_offset(n, refresh_hz);
                    let now = Instant::now();
                    if deadline > now {
                        timers.timer(deadline - now).await;
                    }
                    let Some(window) = weak.upgrade() else {
                        break;
                    };
                    window.0.borrow_mut().last_wake = Some((deadline, Instant::now()));
                    window.tick();
                    n = next_deadline(n, t0.elapsed(), refresh_hz);
                }
            })
            .detach();
    }

    /// The ticker's latest wake: the deadline it waited for and when it woke.
    pub fn last_wake(&self) -> Option<(Instant, Instant)> {
        self.0.borrow().last_wake
    }

    /// The prompt waiting for an answer, if any.
    pub fn pending_dialog(&self) -> Option<Dialog> {
        self.0
            .borrow()
            .dialog
            .as_ref()
            .map(|(dialog, _)| dialog.clone())
    }

    /// Sends `index` to the app and clears the dialog; false (dialog kept) when none waits or
    /// `index` is out of range.
    pub fn answer_dialog(&self, index: usize) -> bool {
        let mut state = self.0.borrow_mut();
        if state
            .dialog
            .as_ref()
            .is_none_or(|(dialog, _)| index >= dialog.buttons.len())
        {
            return false;
        }
        let (_, sender) = state.dialog.take().expect("checked above");
        // The app may have dropped its receiver; the answer is then moot, not an error.
        sender.send(index).ok();
        true
    }

    /// Asks the app whether the window may close, as a compositor's close request would: its
    /// should-close callback decides, and without one the window agrees. Call it outside any
    /// GPUI update, since the callback updates the window itself.
    pub(crate) fn should_close(&self) -> bool {
        // Taken out first: the app's handler may prompt, which borrows this state.
        let Some(mut callback) = self.0.borrow_mut().should_close.take() else {
            return true;
        };
        let agreed = callback();
        self.0.borrow_mut().should_close.get_or_insert(callback);
        agreed
    }

    /// Runs GPUI's close callback once, which removes the window; false when it already ran
    /// or GPUI never registered one. Call it outside any GPUI update.
    pub(crate) fn close(&self) -> bool {
        let Some(callback) = self.0.borrow_mut().close.take() else {
            return false;
        };
        callback();
        let mut state = self.0.borrow_mut();
        state.a11y = None;
        state.a11y_tree = None;
        state.a11y_active = false;
        true
    }

    /// Gives the window the content `size` and tells GPUI, which lays out and draws at the new
    /// size on the next frame. GPUI re-reads the pointer then, so `pointer` (where GPUI last
    /// saw it) keeps it in place. Call it outside any GPUI update, since the callback updates
    /// the window itself.
    pub(crate) fn resize_to(&self, size: Size<Pixels>, pointer: Point<Pixels>) {
        let scale = {
            let mut state = self.0.borrow_mut();
            state.bounds.size = size;
            state.pointer = pointer;
            state.scale
        };
        let Some(mut callback) = self.0.borrow_mut().resized.take() else {
            return;
        };
        callback(size, scale);
        self.0.borrow_mut().resized.get_or_insert(callback);
    }

    /// GPUI's latest accessibility tree for this window; `None` before the first one, after
    /// the window closed, or when the app disabled accessibility.
    pub fn a11y_tree(&self) -> Option<Rc<gpui_kit::accesskit::TreeUpdate>> {
        self.0.borrow().a11y_tree.clone()
    }

    /// Whether GPUI's accessibility is active for this window, as the pilot last set it.
    pub fn is_a11y_active(&self) -> bool {
        self.0.borrow().a11y_active
    }

    /// Turns GPUI's accessibility on or off; the flag applies from the next frame. Off also
    /// drops the stored tree, so a stale tree is never ranked against newer snapshots. The
    /// callbacks are taken out first, so no borrow is held while GPUI runs them.
    pub(crate) fn set_a11y(&self, active: bool) {
        let Some(callbacks) = self.0.borrow_mut().a11y.take() else {
            return;
        };
        if active {
            let _ = (callbacks.activation)();
        } else {
            (callbacks.deactivation)();
        }
        let mut state = self.0.borrow_mut();
        state.a11y = Some(callbacks);
        state.a11y_active = active;
        if !active {
            state.a11y_tree = None;
        }
    }

    /// Test hook: tests that read `a11y_tree` after settling activate it first.
    #[doc(hidden)]
    pub fn set_a11y_active(&self, active: bool) {
        self.set_a11y(active);
    }

    pub fn handle(&self) -> AnyWindowHandle {
        self.0.borrow().handle
    }

    /// The most recently presented frame, read back from the GPU first if nothing has read it
    /// yet; `None` before the first present. A failed readback is an error, also kept in
    /// `render_error`, because the previous frame is no longer the latest.
    pub fn last_frame(&self) -> Result<Option<Frame>, String> {
        let mut state = self.0.borrow_mut();
        if let Some(unread) = state.unread.take() {
            // The readback renders the copied scene again, assuming the atlas still holds the
            // tiles it was drawn with.
            match state
                .renderer
                .render_scene_to_image(&unread.scene, unread.size)
            {
                Ok(image) => {
                    state.readbacks += 1;
                    state.last_frame = Some(Frame {
                        image: Arc::new(image),
                        hash: Arc::default(),
                        seq: unread.seq,
                        render_time: unread.render_time,
                    });
                }
                Err(error) => {
                    let error = format!("{error:#}");
                    state.render_error = Some(error.clone());
                    return Err(error);
                }
            }
        }
        Ok(state.last_frame.clone())
    }

    /// How many frames have been read back from the GPU to the CPU.
    pub fn readbacks(&self) -> u64 {
        self.0.borrow().readbacks
    }

    /// Why the latest present failed to render, until a later present succeeds.
    pub fn render_error(&self) -> Option<String> {
        self.0.borrow().render_error.clone()
    }

    /// Starts recording into `recorder`; false (unchanged) when a recording is running.
    pub fn start_recording(&self, recorder: Recorder) -> bool {
        let mut state = self.0.borrow_mut();
        if state.recorder.is_some() {
            return false;
        }
        state.recorder = Some(recorder);
        true
    }

    /// Ends the running recording and hands it back.
    pub fn stop_recording(&self) -> Option<Recorder> {
        self.0.borrow_mut().recorder.take()
    }

    /// Stamps one notch into the running recording, if any.
    pub fn record_notch(&self, scheduled: Instant, sent: Instant) {
        if let Some(recorder) = self.0.borrow_mut().recorder.as_mut() {
            recorder.notch(scheduled, sent);
        }
    }

    /// Every tick sends `Some(hash)` if it presented a frame, else `None`.
    pub fn ticks(&self) -> mpsc::UnboundedReceiver<Option<u64>> {
        let (tx, rx) = mpsc::unbounded();
        self.0.borrow_mut().ticks.push(tx);
        rx
    }

    /// Delivers one frame request, as a compositor's frame callback would, then tells every
    /// `ticks` subscriber whether it presented. The first tick also reports the window active,
    /// since GPUI throttles inactive windows. No borrow is held across a callback, because the
    /// frame callback re-enters `draw`. A new frame is hashed only while a subscriber listens.
    pub fn tick(&self) {
        let before = self.last_seq();
        self.0.borrow_mut().tick_started = Some(Instant::now());
        self.deliver_frame_request();
        {
            let mut state = self.0.borrow_mut();
            state.tick_started = None;
            state.ticks.retain(|tx| !tx.is_closed());
            if state.ticks.is_empty() {
                return;
            }
        }
        // A failed readback counts as no new frame; its `render_error` keeps settle unsure.
        let drawn = self
            .last_frame()
            .ok()
            .flatten()
            .filter(|frame| Some(frame.seq) != before)
            .map(|frame| frame.hash());
        self.0
            .borrow_mut()
            .ticks
            .retain(|tx| tx.unbounded_send(drawn).is_ok());
    }

    fn last_seq(&self) -> Option<u64> {
        let state = self.0.borrow();
        match &state.unread {
            Some(unread) => Some(unread.seq),
            None => state.last_frame.as_ref().map(|frame| frame.seq),
        }
    }

    fn deliver_frame_request(&self) {
        let activate = {
            let mut state = self.0.borrow_mut();
            if state.activated {
                None
            } else {
                state.activated = true;
                state.active_status_change.take()
            }
        };
        if let Some(mut callback) = activate {
            callback(true);
            self.0.borrow_mut().active_status_change = Some(callback);
        }
        let Some(mut request_frame) = self.0.borrow_mut().request_frame.take() else {
            return;
        };
        request_frame(RequestFrameOptions::default());
        self.0.borrow_mut().request_frame = Some(request_frame);
    }
}

/// Offset of deadline `n` from the ticker's start, in whole nanoseconds so the grid never drifts.
fn deadline_offset(n: u64, refresh_hz: u32) -> Duration {
    Duration::from_nanos(n * 1_000_000_000 / u64::from(refresh_hz))
}

/// Index of the first deadline strictly after `elapsed`, never below `served + 1`;
/// deadline n is at t0 + n * 1e9 / refresh_hz ns.
pub(crate) fn next_deadline(served: u64, elapsed: Duration, refresh_hz: u32) -> u64 {
    let mut next = served + 1;
    while deadline_offset(next, refresh_hz) <= elapsed {
        next += 1;
    }
    next
}

/// A window reference that does not keep the window open.
pub(crate) struct WeakWindow(Weak<RefCell<State>>);

impl WeakWindow {
    pub(crate) fn upgrade(&self) -> Option<PilotWindow> {
        self.0.upgrade().map(PilotWindow)
    }
}

impl HasWindowHandle for PilotWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Err(HandleError::NotSupported)
    }
}

impl HasDisplayHandle for PilotWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Err(HandleError::NotSupported)
    }
}

impl PlatformWindow for PilotWindow {
    /// Keeps GPUI's accessibility callbacks but leaves it inactive: an active frame that
    /// opens a kit menu trips gpui-pre's debug `set_focus` assert, so only `act` turns it on.
    fn a11y_init(&self, callbacks: gpui_kit::A11yCallbacks) {
        self.0.borrow_mut().a11y = Some(callbacks);
    }

    /// Runs inside GPUI's draw, so it only swaps the stored tree and never calls back.
    fn a11y_tree_update(&self, update: gpui_kit::accesskit::TreeUpdate) {
        self.0.borrow_mut().a11y_tree = Some(Rc::new(update));
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    fn resize(&mut self, size: Size<Pixels>) {
        self.0.borrow_mut().bounds.size = size;
    }

    fn scale_factor(&self) -> f32 {
        self.0.borrow().scale
    }

    fn appearance(&self) -> WindowAppearance {
        WindowAppearance::Dark
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.0.borrow().display.clone()
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.borrow().pointer
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.borrow_mut().input_handler.take()
    }

    // The prompt is held for the agent instead of drawn; GPUI's fallback never builds.
    fn prompt(
        &self,
        level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        let (sender, receiver) = oneshot::channel();
        let dialog = Dialog {
            level,
            message: msg.to_owned(),
            detail: detail.map(str::to_owned),
            buttons: answers.iter().map(|a| a.label().to_string()).collect(),
        };
        // A newer prompt replaces an unanswered one; dropping its sender cancels the app's await.
        self.0.borrow_mut().dialog = Some((dialog, sender));
        Some(receiver)
    }

    // Activation is delivered by the first tick, outside any GPUI update.
    fn activate(&self) {}

    fn is_active(&self) -> bool {
        self.0.borrow().activated
    }

    fn visibility(&self) -> WindowVisibility {
        WindowVisibility::Visible
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        self.0.borrow_mut().title = title.to_owned();
    }

    fn get_title(&self) -> String {
        self.0.borrow().title.clone()
    }

    fn set_background_appearance(&self, _background: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {
        let mut state = self.0.borrow_mut();
        state.fullscreen = !state.fullscreen;
    }

    fn is_fullscreen(&self) -> bool {
        self.0.borrow().fullscreen
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.borrow_mut().request_frame = Some(callback);
    }

    // The pilot dispatches input through `Window` and never moves or hovers the window from
    // outside, so these callbacks are never fired; `resize_to` fires `on_resize`.
    fn on_input(&self, _callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {}

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.borrow_mut().active_status_change = Some(callback);
    }

    fn on_visibility_change(&self, _callback: Box<dyn FnMut(WindowVisibility)>) {}

    fn on_hover_status_change(&self, _callback: Box<dyn FnMut(bool)>) {}

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.borrow_mut().resized = Some(callback);
    }

    fn on_moved(&self, _callback: Box<dyn FnMut()>) {}

    // A pilot window stays on the platform's one display, which never changes.
    fn on_display_changed(&self, _callback: Box<dyn FnMut()>) {}

    // Kept for `should_close` and `close`, which the executor calls on a window close request.
    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.borrow_mut().should_close = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.borrow_mut().close = Some(callback);
    }

    fn on_hit_test_window_control(&self, _callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
    }

    fn on_appearance_changed(&self, _callback: Box<dyn FnMut()>) {}

    // A present becomes a frame, rendered on the GPU as a real present would be. Its pixels
    // are read back at once only while a tick subscriber will hash them; otherwise the scene
    // is kept and `last_frame` reads it back on demand, so a frame nobody looks at costs no
    // GPU wait. A render error keeps the previous frame, so the agent sees the last good
    // pixels rather than none, and is kept so settle does not trust them. A running recording
    // stamps each good frame with the work since its tick began.
    fn draw(&self, scene: &Scene) {
        let seq = self.last_seq().map_or(1, |seq| seq + 1);
        let mut state = self.0.borrow_mut();
        let size = state.bounds.size.to_device_pixels(state.scale);
        let watched = state.ticks.iter().any(|tx| !tx.is_closed());
        let started = Instant::now();
        let rendered = if watched {
            state.renderer.render_scene_to_image(scene, size).map(Some)
        } else {
            state.renderer.render_scene(scene, size).map(|()| None)
        };
        let image = match rendered {
            Ok(image) => image,
            Err(error) => {
                state.render_error = Some(format!("{error:#}"));
                return;
            }
        };
        state.render_error = None;
        let render_time = started.elapsed();
        match image {
            Some(image) => {
                state.readbacks += 1;
                state.unread = None;
                state.last_frame = Some(Frame {
                    image: Arc::new(image),
                    hash: Arc::default(),
                    seq,
                    render_time,
                });
            }
            None => {
                state.unread = Some(Unread {
                    scene: copy_scene(scene),
                    size,
                    seq,
                    render_time,
                });
            }
        }
        let now = Instant::now();
        let work = now - state.tick_started.unwrap_or(started);
        if let Some(recorder) = state.recorder.as_mut() {
            recorder.frame(now, Some(work));
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.borrow().atlas.clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }

    fn render_to_image(&self, scene: &Scene) -> Result<RgbaImage> {
        let mut state = self.0.borrow_mut();
        let size = state.bounds.size.to_device_pixels(state.scale);
        state.renderer.render_scene_to_image(scene, size)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{borrow::Cow, cell::Cell, rc::Rc};

    use anyhow::anyhow;
    use gpui_kit::{
        AtlasKey, AtlasTile, DevicePixels, ScaledPixels, WindowHandle, WindowId, point, px, size,
    };

    use super::*;

    /// A renderer whose GPU work always fails.
    struct Failing(Arc<dyn PlatformAtlas>);

    impl PlatformHeadlessRenderer for Failing {
        fn render_scene_to_image(&mut self, _: &Scene, _: Size<DevicePixels>) -> Result<RgbaImage> {
            Err(anyhow!("device lost"))
        }
        fn render_scene(&mut self, _: &Scene, _: Size<DevicePixels>) -> Result<()> {
            Err(anyhow!("device lost"))
        }
        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.clone()
        }
    }

    struct NoAtlas;

    impl PlatformAtlas for NoAtlas {
        fn get_or_insert_with<'a>(
            &self,
            _: AtlasKey,
            _: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
        ) -> Result<Option<AtlasTile>> {
            Ok(None)
        }
        fn remove(&self, _: &AtlasKey) {}
    }

    #[test]
    fn a_failed_render_is_reported_and_keeps_no_frame() {
        let handle = WindowHandle::<gpui_kit::ExternalPaths>::new(WindowId::from(1)).into();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(8.), px(8.)));
        let renderer = Box::new(Failing(Arc::new(NoAtlas)));
        let window = PilotWindow::new(handle, bounds, 1.0, None, renderer);
        window.draw(&Scene::default());
        assert!(window.last_frame().unwrap().is_none());
        assert_eq!(window.render_error().as_deref(), Some("device lost"));
    }

    #[test]
    fn the_next_deadline_skips_every_missed_one_and_never_queues() {
        use super::next_deadline;
        let ms = Duration::from_millis;
        // On time: the next deadline is the following one.
        assert_eq!(next_deadline(1, ms(3), 144), 2);
        assert_eq!(next_deadline(0, Duration::ZERO, 60), 1);
        // A 20 ms draw at 144 Hz (period 6.944 ms) misses deadline 2 (13.889 ms).
        assert_eq!(next_deadline(1, ms(20), 144), 3);
        // Exactly on a deadline counts as missed: 50 ms is deadline 3 at 60 Hz.
        assert_eq!(next_deadline(1, ms(50), 60), 4);
        // A stall of a whole second resumes on the grid, not 240 ticks behind it.
        assert_eq!(next_deadline(5, Duration::from_secs(1) + ms(1), 240), 241);
    }

    /// A renderer that paints every pixel the grey level in its shared cell.
    struct Solid(Arc<dyn PlatformAtlas>, Rc<Cell<u8>>);

    impl PlatformHeadlessRenderer for Solid {
        fn render_scene_to_image(
            &mut self,
            _: &Scene,
            size: Size<DevicePixels>,
        ) -> Result<RgbaImage> {
            let level = self.1.get();
            let (w, h) = (size.width.0 as u32, size.height.0 as u32);
            Ok(RgbaImage::from_pixel(
                w,
                h,
                image::Rgba([level, level, level, 255]),
            ))
        }
        fn render_scene(&mut self, _: &Scene, _: Size<DevicePixels>) -> Result<()> {
            Ok(())
        }
        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.clone()
        }
    }

    /// A renderer that paints every pixel black except the last, whose grey level is in its
    /// shared cell.
    struct LastPixel(Arc<dyn PlatformAtlas>, Rc<Cell<u8>>);

    impl PlatformHeadlessRenderer for LastPixel {
        fn render_scene_to_image(
            &mut self,
            _: &Scene,
            size: Size<DevicePixels>,
        ) -> Result<RgbaImage> {
            let level = self.1.get();
            let (w, h) = (size.width.0 as u32, size.height.0 as u32);
            let mut image = RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]));
            image.put_pixel(w - 1, h - 1, image::Rgba([level, level, level, 255]));
            Ok(image)
        }
        fn render_scene(&mut self, _: &Scene, _: Size<DevicePixels>) -> Result<()> {
            Ok(())
        }
        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.clone()
        }
    }

    /// A renderer whose GPU render always succeeds but whose readback fails while its shared
    /// cell is set.
    struct FlakyReadback(Arc<dyn PlatformAtlas>, Rc<Cell<bool>>);

    impl PlatformHeadlessRenderer for FlakyReadback {
        fn render_scene_to_image(
            &mut self,
            _: &Scene,
            size: Size<DevicePixels>,
        ) -> Result<RgbaImage> {
            if self.1.get() {
                return Err(anyhow!("readback lost"));
            }
            let (w, h) = (size.width.0 as u32, size.height.0 as u32);
            Ok(RgbaImage::from_pixel(w, h, image::Rgba([9, 9, 9, 255])))
        }
        fn render_scene(&mut self, _: &Scene, _: Size<DevicePixels>) -> Result<()> {
            Ok(())
        }
        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.clone()
        }
    }

    /// A 4 by 4 window painted by `FlakyReadback`.
    pub(crate) fn flaky_readback_window(failing: Rc<Cell<bool>>) -> PilotWindow {
        let handle = WindowHandle::<gpui_kit::ExternalPaths>::new(WindowId::from(1)).into();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(4.), px(4.)));
        let renderer = Box::new(FlakyReadback(Arc::new(NoAtlas), failing));
        PilotWindow::new(handle, bounds, 1.0, None, renderer)
    }

    /// A window of `w` by `h` logical pixels at scale 1 painted by `Solid`.
    pub(super) fn solid_window(level: Rc<Cell<u8>>, w: f32, h: f32) -> PilotWindow {
        let handle = WindowHandle::<gpui_kit::ExternalPaths>::new(WindowId::from(1)).into();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(w), px(h)));
        PilotWindow::new(
            handle,
            bounds,
            1.0,
            None,
            Box::new(Solid(Arc::new(NoAtlas), level)),
        )
    }

    #[test]
    fn a_frame_hashes_its_pixels_only_when_asked() {
        let level = Rc::new(Cell::new(10));
        // 8 by 9 pixels is 288 bytes, a whole number of u64 words.
        let window = solid_window(level.clone(), 8., 9.);
        window.draw(&Scene::default());
        let first = window.last_frame().unwrap().expect("a frame");
        assert!(!first.is_hashed(), "drawing alone hashes nothing");
        window.draw(&Scene::default());
        assert_eq!(
            window.last_frame().unwrap().unwrap().hash(),
            first.hash(),
            "equal pixels"
        );
        assert!(first.is_hashed());
        level.set(11);
        window.draw(&Scene::default());
        assert_ne!(window.last_frame().unwrap().unwrap().hash(), first.hash());
    }

    #[test]
    fn bytes_past_the_last_whole_word_still_count() {
        // 3 by 1 pixels is 12 bytes: one word and a 4-byte remainder, which holds exactly the
        // last pixel, the only one that changes.
        let level = Rc::new(Cell::new(10));
        let handle = WindowHandle::<gpui_kit::ExternalPaths>::new(WindowId::from(1)).into();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(3.), px(1.)));
        let renderer = Box::new(LastPixel(Arc::new(NoAtlas), level.clone()));
        let window = PilotWindow::new(handle, bounds, 1.0, None, renderer);
        window.draw(&Scene::default());
        let first = window.last_frame().unwrap().expect("a frame").hash();
        window.draw(&Scene::default());
        assert_eq!(window.last_frame().unwrap().unwrap().hash(), first);
        level.set(12);
        window.draw(&Scene::default());
        assert_ne!(window.last_frame().unwrap().unwrap().hash(), first);
    }

    /// A renderer that paints every pixel the grey level `scene.len()`, so the pixels show
    /// which scene was rendered, and counts what it was asked to do.
    #[derive(Default)]
    struct Counts {
        renders: Cell<u32>,
        readbacks: Cell<u32>,
    }

    struct SceneLevel(Arc<dyn PlatformAtlas>, Rc<Counts>);

    impl PlatformHeadlessRenderer for SceneLevel {
        fn render_scene_to_image(
            &mut self,
            scene: &Scene,
            size: Size<DevicePixels>,
        ) -> Result<RgbaImage> {
            self.1.readbacks.set(self.1.readbacks.get() + 1);
            let level = scene.len() as u8;
            let (w, h) = (size.width.0 as u32, size.height.0 as u32);
            Ok(RgbaImage::from_pixel(
                w,
                h,
                image::Rgba([level, level, level, 255]),
            ))
        }
        fn render_scene(&mut self, _: &Scene, _: Size<DevicePixels>) -> Result<()> {
            self.1.renders.set(self.1.renders.get() + 1);
            Ok(())
        }
        fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
            self.0.clone()
        }
    }

    fn counting_window() -> (PilotWindow, Rc<Counts>) {
        let counts = Rc::new(Counts::default());
        let handle = WindowHandle::<gpui_kit::ExternalPaths>::new(WindowId::from(1)).into();
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(4.), px(4.)));
        let renderer = Box::new(SceneLevel(Arc::new(NoAtlas), counts.clone()));
        (
            PilotWindow::new(handle, bounds, 1.0, None, renderer),
            counts,
        )
    }

    /// A scene of `layers` empty layers: `2 * layers` paint operations.
    fn scene_of(layers: usize) -> Scene {
        let mut scene = Scene::default();
        let bounds = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(0.)),
            size(ScaledPixels(1.), ScaledPixels(1.)),
        );
        for _ in 0..layers {
            scene.push_layer(bounds);
            scene.pop_layer();
        }
        scene
    }

    #[test]
    fn draws_nobody_watches_render_without_reading_back() {
        let (window, counts) = counting_window();
        for layers in 1..=3 {
            window.draw(&scene_of(layers));
        }
        assert_eq!(counts.renders.get(), 3);
        assert_eq!(counts.readbacks.get(), 0);
        assert_eq!(window.readbacks(), 0);
    }

    #[test]
    fn the_latest_unread_frame_is_read_back_once_on_demand() {
        let (window, counts) = counting_window();
        for layers in 1..=3 {
            window.draw(&scene_of(layers));
        }
        let frame = window.last_frame().unwrap().expect("a frame");
        assert_eq!(frame.seq, 3);
        assert_eq!(
            frame.image.get_pixel(0, 0).0,
            [6, 6, 6, 255],
            "the third scene"
        );
        assert_eq!(
            window.last_frame().unwrap().expect("a frame").hash(),
            frame.hash()
        );
        assert_eq!(counts.readbacks.get(), 1);
        assert_eq!(window.readbacks(), 1);
    }

    #[test]
    fn a_watched_window_reads_each_draw_back() {
        let (window, counts) = counting_window();
        let _watcher = window.ticks();
        window.draw(&scene_of(1));
        window.draw(&scene_of(2));
        assert_eq!(counts.readbacks.get(), 2);
        assert_eq!(
            window
                .last_frame()
                .unwrap()
                .expect("a frame")
                .image
                .get_pixel(0, 0)
                .0[0],
            4
        );
        assert_eq!(
            counts.readbacks.get(),
            2,
            "the drawn frame was already read"
        );
    }

    #[test]
    fn a_recording_window_stamps_each_draw_with_its_work() {
        use gpui_pilot_protocol::Recorder;
        let window = solid_window(Rc::new(Cell::new(1)), 8., 9.);
        assert!(window.start_recording(Recorder::new(Instant::now(), 60_000, 100)));
        assert!(!window.start_recording(Recorder::new(Instant::now(), 60_000, 100)));
        window.draw(&Scene::default());
        window.draw(&Scene::default());
        let report = window
            .stop_recording()
            .expect("recording")
            .finish(Instant::now(), None);
        assert_eq!(report.frames, 2);
        assert_eq!(report.work.expect("headless work").count, 2);
        assert!(window.stop_recording().is_none());
    }
}
