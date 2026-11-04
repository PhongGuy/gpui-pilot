//! `Pilot`: turns one protocol request into snapshot lookups, input dispatched to the main
//! window, and a settle wait, and answers it with a protocol outcome.

use std::{
    path::Path,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui_kit::{
    AppContext as _, AsyncApp, ExternalPaths, FileDropEvent, InputEvent as _, PromptLevel, Window,
};
use gpui_pilot_protocol::{
    Answered, BuildInfo, DialogInfo, DialogLevel, ErrorCode, Modifier, MouseButton, Node, Outcome,
    PickAnswer, PilotError, PromptInfo, PromptKind, Recorder, Rect, Reply, Request, Target,
    WaitUntil, Wheel, WindowInfo, hover_wait_ms,
};
use image::{ImageFormat, imageops};

use crate::{
    Actionable, Dialog, Frame, Layering, MAX_NOTCHES, NOTCH_MS_RANGE, PathPrompt, PilotPlatform,
    PilotWindow, PromptAnswered, Resolved, actionable, click_events, dispatch_events,
    dispatch_keystrokes, drag_move, drag_path, drag_press, drag_release, focused_input_masked,
    hover_events, notch_events, observe, parse_keys, resolve, scroll_events, settle, subtree,
    text_keystrokes, to_nodes,
};

/// How often `tree` re-reads the snapshots while it waits for its target.
const POLL: Duration = Duration::from_millis(16);
/// The largest side, in device pixels, a headless window may be resized to.
const RESIZE_MAX_DEVICE_PX: f32 = 8192.0;
/// How long a screenshot waits for the window's first frame.
const FRAME_WAIT: Duration = Duration::from_secs(2);

/// Executes protocol requests against the app running on one `PilotPlatform`.
pub struct Pilot {
    platform: Rc<PilotPlatform>,
}

type Handled<T> = Result<T, PilotError>;

impl Pilot {
    pub fn new(platform: Rc<PilotPlatform>) -> Self {
        Self { platform }
    }

    /// Handles every request except `Hello` and `Close`, which belong to the socket loop.
    pub async fn handle(&self, request: Request, cx: &mut AsyncApp) -> Outcome {
        match self.run(request, cx).await {
            Ok(reply) => Outcome::Ok(reply),
            Err(error) => Outcome::Err(error),
        }
    }

    async fn run(&self, request: Request, cx: &mut AsyncApp) -> Handled<Reply> {
        if matches!(request, Request::Hello { .. } | Request::Close) {
            return Err(PilotError::new(
                ErrorCode::Internal,
                "hello and close are handled by the socket loop, not the executor",
            ));
        }
        let window = self
            .platform
            .main_window()
            .ok_or_else(|| not_running("the app has no open window"))?;
        // A waiting prompt would swallow input in a real window, so refuse it before any
        // target is resolved.
        if matches!(
            request,
            Request::Click { .. }
                | Request::Type { .. }
                | Request::Key { .. }
                | Request::Scroll { .. }
                | Request::Hover { .. }
                | Request::Drag { .. }
                | Request::Drop { .. }
        ) && let Some(dialog) = window.pending_dialog()
        {
            return Err(PilotError::new(
                ErrorCode::Occluded,
                format!(
                    "a dialog is open (\"{}\"); answer it with the dialog tool",
                    dialog.message
                ),
            ));
        }
        match request {
            Request::Click {
                target,
                button,
                count,
                modifiers,
                timeout_ms,
                settle_ms,
            } => {
                let button = gpui_button(button)?;
                let modifiers = gpui_modifiers(&modifiers)?;
                let acted = self
                    .act(&window, cx, &target, timeout_ms, true, |window, cx, a| {
                        dispatch_events(
                            window,
                            cx,
                            click_events(a.at, button, count as usize, modifiers),
                        );
                    })
                    .await?;
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::Click {
                    clicked: acted.node,
                    hit: acted.hit,
                    settled,
                })
            }
            Request::Type {
                text,
                target,
                timeout_ms,
                settle_ms,
            } => {
                // Focus the target with a click in its own update, so the keys below reach
                // whatever the click focused.
                if let Some(target) = target {
                    let left = gpui_kit::MouseButton::Left;
                    self.act(&window, cx, &target, timeout_ms, true, |window, cx, a| {
                        dispatch_events(
                            window,
                            cx,
                            click_events(a.at, left, 1, Default::default()),
                        );
                    })
                    .await?;
                }
                // Read masking before typing: the text may end in a key that moves focus.
                let masked = focus_masked(&window, cx)?;
                let keys = text_keystrokes(&text);
                on_window(&window, cx, |window, cx| {
                    dispatch_keystrokes(window, cx, keys);
                    Ok(())
                })?;
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::Type { settled, masked })
            }
            Request::Key { keys, settle_ms } => {
                let keys = parse_keys(&keys)
                    .map_err(|error| PilotError::new(ErrorCode::InvalidKeystroke, error))?;
                on_window(&window, cx, |window, cx| {
                    dispatch_keystrokes(window, cx, keys);
                    Ok(())
                })?;
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::Key { settled })
            }
            Request::Scroll {
                target,
                dx,
                dy,
                modifiers,
                wheel,
                timeout_ms,
                settle_ms,
            } => {
                let modifiers = gpui_modifiers(&modifiers)?;
                if let Some(wheel) = wheel {
                    if wheel.notches_x.unsigned_abs() > MAX_NOTCHES
                        || wheel.notches_y.unsigned_abs() > MAX_NOTCHES
                    {
                        return Err(PilotError::new(
                            ErrorCode::InvalidArgument,
                            format!("at most {MAX_NOTCHES} notches per axis"),
                        ));
                    }
                    if !NOTCH_MS_RANGE.contains(&wheel.notch_ms) {
                        return Err(PilotError::new(
                            ErrorCode::InvalidArgument,
                            "`notch_ms` is between 1 and 1000",
                        ));
                    }
                    self.notches(&window, cx, &target, timeout_ms, wheel, modifiers)
                        .await?;
                    let settled = self.settle(&window, cx, settle_ms).await;
                    return Ok(Reply::Scroll { settled });
                }
                self.act(&window, cx, &target, timeout_ms, false, |window, cx, a| {
                    dispatch_events(window, cx, scroll_events(a.at, dx, dy, modifiers));
                })
                .await?;
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::Scroll { settled })
            }
            Request::Hover {
                target,
                timeout_ms,
                settle_ms,
            } => {
                let acted = self
                    .act(&window, cx, &target, timeout_ms, false, |window, cx, a| {
                        dispatch_events(window, cx, hover_events(a.at));
                    })
                    .await?;
                // A tooltip's show timer lies past settle's horizon, so wait it out first,
                // then give settle its whole budget.
                let wait = hover_wait_ms(settle_ms, self.platform.refresh_hz() * 1000);
                if wait > 0 {
                    self.platform
                        .pilot_timers()
                        .timer(Duration::from_millis(wait))
                        .await;
                }
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::Hover {
                    hovered: acted.node,
                    hit: acted.hit,
                    settled,
                })
            }
            Request::Drag {
                from,
                to,
                steps,
                button,
                modifiers,
                timeout_ms,
                settle_ms,
            } => {
                let modifiers = gpui_modifiers(&modifiers)?;
                let button = gpui_button(button)?;
                // The drop end is only observed; the source must be enabled, as a click target
                // must, and is pressed once both ends are actionable.
                let end = self
                    .act(&window, cx, &to, timeout_ms, false, |_, _, _| {})
                    .await?;
                let start = self
                    .act(&window, cx, &from, timeout_ms, true, |window, cx, a| {
                        dispatch_events(window, cx, drag_press(a.at, button, modifiers));
                    })
                    .await?;
                // One move per update with a frame between, so GPUI paints the drag and its drop targets.
                for at in drag_path(start.at, end.at, steps.clamp(1, 100)) {
                    self.platform.pilot_timers().timer(POLL).await;
                    on_window(&window, cx, |window, cx| {
                        dispatch_events(window, cx, vec![drag_move(at, button, modifiers)]);
                        Ok(())
                    })?;
                }
                self.platform.pilot_timers().timer(POLL).await;
                on_window(&window, cx, |window, cx| {
                    dispatch_events(window, cx, drag_release(end.at, button, modifiers));
                    Ok(())
                })?;
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::Drag {
                    from: start.node,
                    to: end.node,
                    settled,
                })
            }
            Request::Drop {
                target,
                paths,
                cancel,
                timeout_ms,
                settle_ms,
            } => {
                if paths.is_empty() || paths.iter().any(|path| !path.is_absolute()) {
                    return Err(PilotError::new(
                        ErrorCode::InvalidArgument,
                        "`paths` is a non-empty list of absolute paths",
                    ));
                }
                // The platform's sequence: the paths enter at the target, move there a frame
                // later, then drop or leave a frame after that, so drag-over styles get painted.
                let paths = ExternalPaths(paths.into_iter().collect());
                let acted = self
                    .act(&window, cx, &target, timeout_ms, false, |window, cx, a| {
                        let entered = FileDropEvent::Entered {
                            position: a.at,
                            paths: paths.clone(),
                        };
                        dispatch_events(window, cx, vec![entered.to_platform_input()]);
                    })
                    .await?;
                let at = acted.at;
                let end = if cancel {
                    FileDropEvent::Exited
                } else {
                    FileDropEvent::Submit { position: at }
                };
                for event in [FileDropEvent::Pending { position: at }, end] {
                    self.platform.pilot_timers().timer(POLL).await;
                    on_window(&window, cx, |window, cx| {
                        dispatch_events(window, cx, vec![event.to_platform_input()]);
                        Ok(())
                    })?;
                }
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::Drop {
                    over: acted.node,
                    hit: acted.hit,
                    dropped: !cancel,
                    settled,
                })
            }
            Request::Resize {
                width,
                height,
                settle_ms,
            } => {
                gpui_pilot_protocol::check_resize(width, height)
                    .map_err(|message| PilotError::new(ErrorCode::InvalidArgument, message))?;
                // The window renders at device pixels, and a side past the render target limit
                // would break every later frame, so the limit applies after scaling.
                let scale = on_window(&window, cx, |window, _| Ok(window.scale_factor()))?;
                if width * scale > RESIZE_MAX_DEVICE_PX || height * scale > RESIZE_MAX_DEVICE_PX {
                    return Err(PilotError::new(
                        ErrorCode::InvalidArgument,
                        format!(
                            "a {width}x{height} window at scale {scale} exceeds \
                             {RESIZE_MAX_DEVICE_PX} device pixels per side"
                        ),
                    ));
                }
                // No pointer event is sent: the pointer stays where GPUI last saw it, so hover
                // and tooltips are re-decided at the same point in the new layout.
                let pointer = on_window(&window, cx, |window, _| Ok(window.mouse_position()))?;
                let size = gpui_kit::size(gpui_kit::px(width), gpui_kit::px(height));
                window.resize_to(size, pointer);
                let settled = self.settle(&window, cx, settle_ms).await;
                let info = on_window(&window, cx, |window, _| {
                    let size = window.viewport_size();
                    Ok(WindowInfo::new(
                        size.width.into(),
                        size.height.into(),
                        window.scale_factor(),
                    ))
                })?;
                Ok(Reply::Resize {
                    window: info,
                    settled,
                })
            }
            Request::Dialog {
                button,
                timeout_ms,
                settle_ms,
            } => {
                self.dialog(&window, cx, button, timeout_ms, settle_ms)
                    .await
            }
            Request::Wait { until, timeout_ms } => self.wait(&window, cx, until, timeout_ms).await,
            Request::PickFile {
                answer, settle_ms, ..
            } => {
                let paths = match answer {
                    PickAnswer::Paths(paths) => Some(paths),
                    PickAnswer::Cancel => None,
                    other => return Err(unsupported(format!("pick answer {other:?}"))),
                };
                let (answered, prompt) = self.platform.answer_prompt(paths);
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::PickFile {
                    answered: answered_of(answered),
                    prompt: prompt.map(prompt_info),
                    settled,
                })
            }
            Request::Clipboard { set, settle_ms } => {
                if let Some(text) = set {
                    self.platform.set_clipboard_text(text);
                }
                let settled = self.settle(&window, cx, settle_ms).await;
                let text = self.platform.clipboard_text();
                Ok(Reply::Clipboard { text, settled })
            }
            Request::Tree {
                within,
                wait_for,
                timeout_ms,
                settle_ms,
            } => {
                self.tree(&window, cx, within, wait_for, timeout_ms, settle_ms)
                    .await
            }
            Request::Screenshot { target, path } => {
                self.screenshot(&window, cx, target, &path).await
            }
            // The app's should-close handler decides, as on a compositor's close request. An
            // agreeing window stays open until the reply is out; `finish_window_close` ends it.
            Request::CloseWindow { settle_ms, .. } => {
                if window.should_close() {
                    return Ok(Reply::CloseWindow {
                        closed: true,
                        settled: true,
                    });
                }
                let settled = self.settle(&window, cx, settle_ms).await;
                Ok(Reply::CloseWindow {
                    closed: false,
                    settled,
                })
            }
            Request::FramesStart { idle_ms } => {
                let refresh_mhz = self.platform.refresh_hz() * 1000;
                if !window.start_recording(Recorder::new(Instant::now(), refresh_mhz, idle_ms)) {
                    return Err(PilotError::new(
                        ErrorCode::InvalidArgument,
                        "frames is already recording; call frames stop first",
                    ));
                }
                Ok(Reply::FramesStarted { refresh_mhz })
            }
            Request::FramesStop => {
                let recorder = window.stop_recording().ok_or_else(|| {
                    PilotError::new(
                        ErrorCode::InvalidArgument,
                        "frames is not recording; call frames start first",
                    )
                })?;
                let build =
                    BuildInfo::new(env!("GPUI_PILOT_HOST_OPT_LEVEL"), cfg!(debug_assertions));
                Ok(Reply::Frames(recorder.finish(Instant::now(), Some(build))))
            }
            other => Err(unsupported(format!("request {other:?}"))),
        }
    }

    /// Runs GPUI's close path for the main window (its `on_close` callback), which removes it.
    /// Call it after a `CloseWindow` reply with `closed: true` has been written.
    pub fn finish_window_close(&self, _cx: &mut AsyncApp) {
        if let Some(window) = self.platform.main_window() {
            window.close();
        }
    }

    /// Reaches `target`, then sends one wheel event per notch on absolute deadlines on the
    /// pilot's own timers, so settle never counts them as app work and a late notch never
    /// delays the next; the modifiers are released after the last notch.
    async fn notches(
        &self,
        window: &PilotWindow,
        cx: &mut AsyncApp,
        target: &Target,
        timeout_ms: u64,
        wheel: Wheel,
        modifiers: gpui_kit::Modifiers,
    ) -> Handled<()> {
        let acted = self
            .act(window, cx, target, timeout_ms, false, |window, cx, a| {
                let press = notch_events(a.at, wheel.notches_x, wheel.notches_y, modifiers).press;
                dispatch_events(window, cx, press);
            })
            .await?;
        let events = notch_events(acted.at, wheel.notches_x, wheel.notches_y, modifiers);
        let period = Duration::from_millis(wheel.notch_ms);
        let first = Instant::now();
        for (i, notch) in events.notches.into_iter().enumerate() {
            let scheduled = first + period * i as u32;
            let now = Instant::now();
            // A notch already overdue goes out at once, so a backlog may coalesce into one draw.
            if scheduled > now {
                self.platform.pilot_timers().timer(scheduled - now).await;
            }
            window.record_notch(scheduled, Instant::now());
            on_window(window, cx, |window, cx| {
                dispatch_events(window, cx, vec![notch]);
                Ok(())
            })?;
        }
        on_window(window, cx, |window, cx| {
            dispatch_events(window, cx, events.release);
            Ok(())
        })
    }

    /// Polls every 16 ms until `actionable` holds for the same bounds twice in a row, then
    /// runs `dispatch` in that same update and returns the actionable target. A point target
    /// or `timeout_ms == 0` acts on the first success; at the deadline an unstable success
    /// is acted on, and a retryable failure returns with `after N ms: ` before its message.
    async fn act(
        &self,
        window: &PilotWindow,
        cx: &mut AsyncApp,
        target: &Target,
        timeout_ms: u64,
        need_enabled: bool,
        dispatch: impl Fn(&mut Window, &mut gpui_kit::App, &Actionable),
    ) -> Handled<Actionable> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let once = timeout_ms == 0 || matches!(target, Target::Point { .. });
        let mut stable_since: Option<Rect> = None;
        let pilot_window = window;
        loop {
            let expired = Instant::now() >= deadline;
            // Draw pending changes inactive first, so a menu an unsettled input opened never
            // opens on the forced frame.
            pilot_window.tick();
            // Draw one frame with accessibility on, then rank, turn it off and dispatch in one
            // update, so the tree matches the snapshots and the input's frame runs inactive.
            // A cached view reused without re-render still adds no nodes that frame.
            let a11y = A11yScope::activate(pilot_window);
            on_window(pilot_window, cx, |window, _| {
                window.refresh();
                Ok(())
            })?;
            pilot_window.tick();
            let (result, acted) = on_window(pilot_window, cx, |window, cx| {
                let mut observed = observe(window);
                if let Some(tree) = pilot_window.a11y_tree() {
                    Layering::from_update(&tree).assign(&mut observed);
                }
                a11y.end();
                let result = actionable(&observed, target, need_enabled);
                let ready = result.as_ref().is_ok_and(|a| {
                    once || expired || stable_since.as_ref() == Some(&a.node.bounds)
                });
                if let (true, Ok(a)) = (ready, &result) {
                    dispatch(window, cx, a);
                }
                Ok((result, ready))
            })?;
            match result {
                Ok(a) if acted => return Ok(a),
                Ok(a) => stable_since = Some(a.node.bounds),
                Err(error) if !retryable(error.code) || once => return Err(error),
                Err(mut error) if expired => {
                    error.message = format!("after {timeout_ms} ms: {}", error.message);
                    return Err(error);
                }
                Err(_) => stable_since = None,
            }
            self.platform.pilot_timers().timer(POLL).await;
        }
    }

    async fn settle(&self, window: &PilotWindow, cx: &AsyncApp, settle_ms: u64) -> bool {
        let budget = Duration::from_millis(settle_ms);
        settle(&self.platform, window, cx, budget).await.settled
    }

    /// Settles for up to `settle_ms`, polls the snapshots until `wait_for` resolves, then
    /// lists every node, or only those under `within`'s path. A target that is ambiguous or
    /// unsupported fails at once.
    async fn tree(
        &self,
        window: &PilotWindow,
        cx: &mut AsyncApp,
        within: Option<Target>,
        wait_for: Option<Target>,
        timeout_ms: u64,
        settle_ms: u64,
    ) -> Handled<Reply> {
        if let Some(Target::Point { .. }) = within {
            return Err(unsupported("`within` takes an element target, not a point"));
        }
        self.settle(window, cx, settle_ms).await;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let observed = loop {
            let observed = on_window(window, cx, |window, _| Ok(observe(window)))?;
            let Some(wanted) = &wait_for else {
                break observed;
            };
            match resolve(&observed, wanted) {
                Ok(_) => break observed,
                Err(error) if error.code != ErrorCode::ElementNotFound => return Err(error),
                Err(error) if Instant::now() >= deadline => {
                    let mut timeout = PilotError::new(
                        ErrorCode::Timeout,
                        format!("after {timeout_ms} ms: {}", error.message),
                    );
                    timeout.nearest = error.nearest;
                    return Err(timeout);
                }
                Err(_) => self.platform.pilot_timers().timer(POLL).await,
            }
        };
        let nodes = match &within {
            Some(within) => subtree(&observed, within)?,
            None => to_nodes(&observed),
        };
        Ok(Reply::Tree {
            nodes,
            prompt: self.platform.pending_prompt().map(prompt_info),
            pick_queued: self.platform.has_queued_answer(),
            dialog: window.pending_dialog().map(dialog_info),
        })
    }

    /// Without a button, settles and reports the pending dialog. With one, waits up to
    /// `timeout_ms` for a dialog, answers it by exact label, settles and reports what was answered.
    async fn dialog(
        &self,
        window: &PilotWindow,
        cx: &mut AsyncApp,
        button: Option<String>,
        timeout_ms: u64,
        settle_ms: u64,
    ) -> Handled<Reply> {
        let Some(label) = button else {
            let settled = self.settle(window, cx, settle_ms).await;
            return Ok(Reply::Dialog {
                dialog: window.pending_dialog().map(dialog_info),
                answered: None,
                settled,
            });
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let dialog = loop {
            if let Some(dialog) = window.pending_dialog() {
                break dialog;
            }
            if Instant::now() >= deadline {
                return Err(PilotError::new(
                    ErrorCode::Timeout,
                    format!("after {timeout_ms} ms: no dialog is open"),
                ));
            }
            self.platform.pilot_timers().timer(POLL).await;
        };
        let Some(index) = dialog.buttons.iter().position(|b| *b == label) else {
            return Err(PilotError::new(
                ErrorCode::ElementNotFound,
                format!(
                    "the dialog has no button \"{label}\"; its buttons are {:?}",
                    dialog.buttons
                ),
            ));
        };
        if !window.answer_dialog(index) {
            return Err(PilotError::new(
                ErrorCode::ElementNotFound,
                "the dialog was answered or replaced before the button was pressed",
            ));
        }
        let settled = self.settle(window, cx, settle_ms).await;
        Ok(Reply::Dialog {
            dialog: Some(dialog_info(dialog)),
            answered: Some(label),
            settled,
        })
    }

    /// Sleeps for `Elapsed`, or polls the snapshots until a `Visible` target has a visible
    /// match or a `Gone` target has none. Running out of time is `Timeout`; a point target,
    /// and any failure the app's loading cannot clear, fail at once.
    async fn wait(
        &self,
        window: &PilotWindow,
        cx: &mut AsyncApp,
        until: WaitUntil,
        timeout_ms: u64,
    ) -> Handled<Reply> {
        let started = Instant::now();
        let waited_ms = || started.elapsed().as_millis() as u64;
        let (target, gone) = match until {
            WaitUntil::Elapsed(ms) => {
                let pause = Duration::from_millis(ms);
                self.platform.pilot_timers().timer(pause).await;
                return Ok(Reply::Wait {
                    node: None,
                    waited_ms: waited_ms(),
                });
            }
            WaitUntil::Visible(target) => (target, false),
            WaitUntil::Gone(target) => (target, true),
            other => return Err(unsupported(format!("wait until {other:?}"))),
        };
        if let Target::Point { .. } = target {
            return Err(unsupported("`wait` takes an element target, not a point"));
        }
        let deadline = started + Duration::from_millis(timeout_ms);
        loop {
            let observed = on_window(window, cx, |window, _| Ok(observe(window)))?;
            let resolved = resolve(&observed, &target);
            let poll = if gone {
                gone_poll(resolved)
            } else {
                visible_poll(resolved)
            };
            match poll {
                Poll::Done(node) => {
                    return Ok(Reply::Wait {
                        node,
                        waited_ms: waited_ms(),
                    });
                }
                Poll::Failed(error) => return Err(error),
                Poll::Pending(mut timeout) if Instant::now() >= deadline => {
                    timeout.message = format!("after {timeout_ms} ms: {}", timeout.message);
                    return Err(timeout);
                }
                Poll::Pending(_) => self.platform.pilot_timers().timer(POLL).await,
            }
        }
    }

    /// Writes the latest frame, cropped to the target's bounds when one is given, as a PNG.
    async fn screenshot(
        &self,
        window: &PilotWindow,
        cx: &mut AsyncApp,
        target: Option<Target>,
        path: &Path,
    ) -> Handled<Reply> {
        let started = Instant::now();
        let frame = loop {
            if let Some(frame) = latest_frame(window)? {
                break frame;
            }
            if started.elapsed() >= FRAME_WAIT {
                let message = "the window presented no frame within 2 s";
                return Err(PilotError::new(ErrorCode::Timeout, message));
            }
            self.platform.pilot_timers().timer(POLL).await;
        };
        let (scale, area) = on_window(window, cx, |window, _| {
            let area = match &target {
                None => None,
                Some(Target::Point { .. }) => {
                    return Err(unsupported("a screenshot target must be an element"));
                }
                Some(target) => Some(element(resolve(&observe(window), target)?)?.bounds),
            };
            Ok((window.scale_factor(), area))
        })?;
        let image = &*frame.image;
        let shot = match area {
            None => image.clone(),
            // Device pixels are logical bounds times the scale, clamped to the frame.
            Some(area) => {
                let device = |logical: f32, limit: u32| {
                    ((logical * scale).round().max(0.0) as u32).min(limit)
                };
                let (x0, y0) = (
                    device(area.x, image.width()),
                    device(area.y, image.height()),
                );
                let x1 = device(area.x + area.w, image.width());
                let y1 = device(area.y + area.h, image.height());
                imageops::crop_imm(image, x0, y0, x1 - x0, y1 - y0).to_image()
            }
        };
        shot.save_with_format(path, ImageFormat::Png)
            .map_err(|error| internal(format!("writing {}: {error}", path.display())))?;
        Ok(Reply::Screenshot {
            w: shot.width(),
            h: shot.height(),
            scale,
        })
    }
}

/// Runs `f` on the window inside one app update; a closed window or a quitting app is
/// `NotRunning`. `update_window` leaves the root view unleased, so dispatch can redraw it.
fn on_window<T>(
    window: &PilotWindow,
    cx: &mut AsyncApp,
    f: impl FnOnce(&mut Window, &mut gpui_kit::App) -> Handled<T>,
) -> Handled<T> {
    cx.update_window(window.handle(), |_, window, cx| f(window, cx))
        .map_err(|error| not_running(format!("the window is gone: {error}")))?
}

/// One look at a `wait` target: finished, still waiting (with the error a timeout reports
/// now), or failed for good.
enum Poll {
    Done(Option<Node>),
    Pending(PilotError),
    Failed(PilotError),
}

/// A visible single match ends the wait; a missing or hidden one keeps it going.
fn visible_poll(resolved: Result<Resolved, PilotError>) -> Poll {
    match resolved {
        Ok(Resolved::Element(node)) if node.visible => Poll::Done(Some(node)),
        Ok(Resolved::Element(node)) => {
            let mut timeout = PilotError::new(ErrorCode::Timeout, "the only match is hidden");
            timeout.nearest = vec![node.path];
            Poll::Pending(timeout)
        }
        Err(error) if error.code == ErrorCode::ElementNotFound => {
            let mut timeout = PilotError::new(ErrorCode::Timeout, error.message);
            timeout.nearest = error.nearest;
            Poll::Pending(timeout)
        }
        Err(error) => Poll::Failed(error),
        Ok(Resolved::Point(_)) => Poll::Failed(unsupported("this target must be an element")),
    }
}

/// No visible match ends the wait: none at all, a hidden one, or several that are all
/// hidden. Visible matches keep it going and are listed if it times out.
fn gone_poll(resolved: Result<Resolved, PilotError>) -> Poll {
    let visible = match resolved {
        Err(error) if error.code == ErrorCode::ElementNotFound => Vec::new(),
        Err(error) if error.code == ErrorCode::Ambiguous => {
            error.matches.into_iter().filter(|n| n.visible).collect()
        }
        Err(error) => return Poll::Failed(error),
        Ok(Resolved::Element(node)) => Some(node).filter(|n| n.visible).into_iter().collect(),
        Ok(Resolved::Point(_)) => {
            return Poll::Failed(unsupported("this target must be an element"));
        }
    };
    if visible.is_empty() {
        return Poll::Done(None);
    }
    let message = format!("{} visible element(s) still match", visible.len());
    let mut timeout = PilotError::new(ErrorCode::Timeout, message);
    timeout.matches = visible;
    Poll::Pending(timeout)
}

/// Whether the focused input hides its text, from the snapshots of a frame drawn after the
/// focusing click.
fn focus_masked(window: &PilotWindow, cx: &mut AsyncApp) -> Handled<bool> {
    window.tick();
    on_window(window, cx, |window, _| Ok(focused_input_masked(window)))
}

/// Accessibility turned on for one `act` poll; `end` or a drop on any exit turns it off.
struct A11yScope<'a> {
    window: &'a PilotWindow,
    active: std::cell::Cell<bool>,
}

impl<'a> A11yScope<'a> {
    fn activate(window: &'a PilotWindow) -> Self {
        window.set_a11y(true);
        Self {
            window,
            active: std::cell::Cell::new(true),
        }
    }

    fn end(&self) {
        if self.active.replace(false) {
            self.window.set_a11y(false);
        }
    }
}

impl Drop for A11yScope<'_> {
    fn drop(&mut self) {
        self.end();
    }
}

/// Conditions that may clear while the app keeps loading or animating.
fn retryable(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::ElementNotFound | ErrorCode::Occluded | ErrorCode::Disabled
    )
}

fn element(resolved: Resolved) -> Handled<Node> {
    match resolved {
        Resolved::Element(node) => Ok(node),
        _ => Err(unsupported("this target must be an element")),
    }
}

fn gpui_button(button: MouseButton) -> Handled<gpui_kit::MouseButton> {
    match button {
        MouseButton::Left => Ok(gpui_kit::MouseButton::Left),
        MouseButton::Right => Ok(gpui_kit::MouseButton::Right),
        MouseButton::Middle => Ok(gpui_kit::MouseButton::Middle),
        other => Err(unsupported(format!("mouse button {other:?}"))),
    }
}

fn gpui_modifiers(modifiers: &[Modifier]) -> Handled<gpui_kit::Modifiers> {
    let mut held = gpui_kit::Modifiers::default();
    for modifier in modifiers {
        match modifier {
            Modifier::Ctrl => held.control = true,
            Modifier::Alt => held.alt = true,
            Modifier::Shift => held.shift = true,
            Modifier::Super => held.platform = true,
            other => return Err(unsupported(format!("modifier {other:?}"))),
        }
    }
    Ok(held)
}

fn answered_of(answered: PromptAnswered) -> Answered {
    match answered {
        PromptAnswered::Open => Answered::Open,
        PromptAnswered::Save => Answered::Save,
        PromptAnswered::Queued => Answered::Queued,
        PromptAnswered::Cancelled => Answered::Cancelled,
    }
}

fn prompt_info(prompt: PathPrompt) -> PromptInfo {
    match prompt {
        PathPrompt::Open { multiple, .. } => PromptInfo::new(PromptKind::Open, multiple),
        PathPrompt::Save {
            directory,
            suggested_name,
        } => {
            let mut info = PromptInfo::new(PromptKind::Save, false);
            info.directory = Some(directory);
            info.suggested_name = suggested_name;
            info
        }
    }
}

fn dialog_info(dialog: Dialog) -> DialogInfo {
    let level = match dialog.level {
        PromptLevel::Info => DialogLevel::Info,
        PromptLevel::Warning => DialogLevel::Warning,
        PromptLevel::Critical => DialogLevel::Critical,
    };
    let mut info = DialogInfo::new(level, dialog.message, dialog.buttons);
    info.detail = dialog.detail;
    info
}

fn not_running(message: impl Into<String>) -> PilotError {
    PilotError::new(ErrorCode::NotRunning, message)
}

fn unsupported(message: impl Into<String>) -> PilotError {
    PilotError::new(ErrorCode::Unsupported, message)
}

fn internal(message: impl Into<String>) -> PilotError {
    PilotError::new(ErrorCode::Internal, message)
}

/// The window's latest frame, or `None` before its first present; a failed render is an
/// `Internal` error, never the previous frame passed off as current.
fn latest_frame(window: &PilotWindow) -> Handled<Option<Frame>> {
    if let Some(error) = window.render_error() {
        return Err(internal(format!("render failed: {error}")));
    }
    window
        .last_frame()
        .map_err(|error| internal(format!("render failed: {error}")))
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui_kit::{PlatformWindow as _, Scene};

    use super::*;
    use crate::window::tests::flaky_readback_window;

    #[test]
    fn a_screenshot_errors_when_the_pending_readback_fails() {
        let failing = Rc::new(Cell::new(false));
        let window = flaky_readback_window(failing.clone());
        window.draw(&Scene::default());
        let first = latest_frame(&window).expect("no error").expect("a frame");
        // The next present renders on the GPU, but reading its pixels back fails.
        failing.set(true);
        window.draw(&Scene::default());
        match latest_frame(&window) {
            Err(error) => assert_eq!(error.code, ErrorCode::Internal, "{error:?}"),
            Ok(frame) => panic!(
                "a stale frame (seq {:?}, first was {}) passed off as current",
                frame.map(|frame| frame.seq),
                first.seq
            ),
        }
    }
}
