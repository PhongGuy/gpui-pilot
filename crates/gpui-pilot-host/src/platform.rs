//! `PilotPlatform`: the Linux headless platform with every method that would reach D-Bus, a
//! display server, a child process or the network replaced by in-process state.

use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui_kit::{
    Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle,
    ForegroundExecutor, GraphicalEnvironment, Keymap, Menu, MenuItem, OwnedMenu, PathPromptOptions,
    Pixels, Platform, PlatformDisplay, PlatformInfo, PlatformKeyboardLayout,
    PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Size, SystemNotification,
    SystemNotificationResponse, Task, ThermalState, WindowAppearance, WindowButtonLayout,
    WindowParams, WindowingRequest,
};

use crate::dispatch::CountingDispatcher;
use crate::window::{PilotWindow, WeakWindow};

/// A file dialog the app has opened and the agent has not yet answered.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum PathPrompt {
    Open {
        directories: bool,
        multiple: bool,
    },
    Save {
        directory: PathBuf,
        suggested_name: Option<String>,
    },
}

/// What an answer did: completed an open or save dialog, waited for the next one, or
/// cancelled the pending one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PromptAnswered {
    Open,
    Save,
    Queued,
    Cancelled,
}

/// A side effect the app asked for that the pilot logged instead of performing.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Recorded {
    OpenUrl(String),
    OpenWithSystem(PathBuf),
    RevealPath(PathBuf),
    Restart(Option<PathBuf>),
    Notification { title: String },
}

/// The app's side of a pending dialog: the open sender wants paths, the save sender one path.
enum Reply {
    Open(oneshot::Sender<Result<Option<Vec<PathBuf>>>>),
    Save(oneshot::Sender<Result<Option<PathBuf>>>),
}

impl Reply {
    /// Completes the dialog; a save answered with several paths takes the first.
    fn send(self, paths: Option<Vec<PathBuf>>) -> PromptAnswered {
        match (self, paths) {
            (Reply::Open(tx), Some(paths)) => {
                tx.send(Ok(Some(paths))).ok();
                PromptAnswered::Open
            }
            (Reply::Save(tx), Some(paths)) => {
                tx.send(Ok(paths.into_iter().next())).ok();
                PromptAnswered::Save
            }
            (Reply::Open(tx), None) => {
                tx.send(Ok(None)).ok();
                PromptAnswered::Cancelled
            }
            (Reply::Save(tx), None) => {
                tx.send(Ok(None)).ok();
                PromptAnswered::Cancelled
            }
        }
    }
}

#[derive(Default)]
struct State {
    pending: Option<(PathPrompt, Reply)>,
    queued: Option<Option<Vec<PathBuf>>>,
    clipboard: Option<ClipboardItem>,
    primary: Option<ClipboardItem>,
    credentials: HashMap<String, (String, Vec<u8>)>,
    recorded: Vec<Recorded>,
    /// A request is being answered; set by `begin_request`.
    in_request: bool,
    /// The app asked to quit while a request was in flight.
    quit_deferred: bool,
}

/// A GPUI platform for one piloted app: forwards to the Linux headless platform and keeps
/// dialogs, the clipboard, credentials and outward side effects in memory.
pub struct PilotPlatform {
    inner: Rc<dyn Platform>,
    dispatcher: Arc<CountingDispatcher>,
    foreground: ForegroundExecutor,
    background: BackgroundExecutor,
    size: Size<Pixels>,
    scale: f32,
    refresh_hz: u32,
    state: RefCell<State>,
    /// Every window opened, oldest first; a window is open while GPUI holds its box.
    windows: RefCell<Vec<WeakWindow>>,
}

impl PilotPlatform {
    /// The first window opened takes `size`; every window renders at `scale` and refreshes at
    /// `refresh_hz`.
    /// Both executors share one counting dispatcher over the Linux platform's.
    /// The platform is never dropped; see the comment below.
    pub fn new(size: Size<Pixels>, scale: f32, refresh_hz: u32) -> Rc<Self> {
        let inner = gpui_kit::platform::current_platform(true);
        let linux = inner.background_executor().dispatcher().clone();
        let dispatcher = Arc::new(CountingDispatcher::new(linux));
        let platform = Rc::new(Self {
            inner,
            foreground: ForegroundExecutor::new(dispatcher.clone()),
            background: BackgroundExecutor::new(dispatcher.clone()),
            dispatcher,
            size,
            scale,
            refresh_hz,
            state: RefCell::default(),
            windows: RefCell::default(),
        });
        // A timer firing after the app quits wakes parked foreground tasks (the frame ticker);
        // with the scheduler gone they are dropped off-thread, which aborts. Leak one handle.
        std::mem::forget(platform.clone());
        platform
    }

    /// The rate, in Hz, this platform's windows refresh at.
    pub fn refresh_hz(&self) -> u32 {
        self.refresh_hz
    }

    /// Queued main-thread runnables, unfinished background polls, timers due within the horizon.
    pub fn pending_work(&self) -> usize {
        self.dispatcher.pending()
    }

    /// Timers due further out than this are not counted (default 250 ms).
    pub fn set_settle_horizon(&self, horizon: Duration) {
        self.dispatcher.set_horizon(horizon);
    }

    /// Timers outside the pending count, for the pilot's own clocks: the frame ticker and
    /// settle's deadline must not read as app work.
    pub(crate) fn pilot_timers(&self) -> BackgroundExecutor {
        self.inner.background_executor()
    }

    /// Marks a request in flight: a `quit` until `end_request` is held back.
    pub fn begin_request(&self) {
        self.state.borrow_mut().in_request = true;
    }

    /// Ends the request; true when the app asked to quit meanwhile.
    pub fn end_request(&self) -> bool {
        let mut state = self.state.borrow_mut();
        state.in_request = false;
        std::mem::take(&mut state.quit_deferred)
    }

    /// The first window that is still open.
    pub fn main_window(&self) -> Option<PilotWindow> {
        self.open_windows().into_iter().next()
    }

    fn open_windows(&self) -> Vec<PilotWindow> {
        self.windows
            .borrow()
            .iter()
            .filter_map(WeakWindow::upgrade)
            .collect()
    }

    /// The dialog the app is waiting on, if any.
    pub fn pending_prompt(&self) -> Option<PathPrompt> {
        self.state
            .borrow()
            .pending
            .as_ref()
            .map(|(prompt, _)| prompt.clone())
    }

    /// Whether an answer is waiting for the next dialog the app opens.
    pub fn has_queued_answer(&self) -> bool {
        self.state.borrow().queued.is_some()
    }

    /// `Some(paths)` answers, `None` cancels; with nothing pending the answer is queued.
    /// Returns what the answer did and the prompt it answered.
    pub fn answer_prompt(
        &self,
        paths: Option<Vec<PathBuf>>,
    ) -> (PromptAnswered, Option<PathPrompt>) {
        let pending = self.state.borrow_mut().pending.take();
        match pending {
            Some((prompt, reply)) => (reply.send(paths), Some(prompt)),
            None => {
                self.state.borrow_mut().queued = Some(paths);
                (PromptAnswered::Queued, None)
            }
        }
    }

    pub fn clipboard_text(&self) -> Option<String> {
        self.state
            .borrow()
            .clipboard
            .as_ref()
            .and_then(|item| item.text())
    }

    pub fn set_clipboard_text(&self, text: String) {
        self.state.borrow_mut().clipboard = Some(ClipboardItem::new_string(text));
    }

    /// Side effects the app requested, oldest first.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.state.borrow().recorded.clone()
    }

    fn record(&self, effect: Recorded) {
        self.state.borrow_mut().recorded.push(effect);
    }

    /// Opens a dialog: a queued answer completes it at once; otherwise it waits, and any
    /// dialog it replaces is cancelled.
    fn open_prompt(&self, prompt: PathPrompt, reply: Reply) {
        let queued = self.state.borrow_mut().queued.take();
        if let Some(paths) = queued {
            reply.send(paths);
            return;
        }
        let replaced = self.state.borrow_mut().pending.replace((prompt, reply));
        if let Some((_, old)) = replaced {
            old.send(None);
        }
    }
}

impl Platform for PilotPlatform {
    fn platform_info(&self) -> PlatformInfo {
        self.inner.platform_info()
    }

    fn background_executor(&self) -> BackgroundExecutor {
        self.background.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.inner.text_system()
    }

    // In headless mode the Linux platform connects to no display server on `run`.
    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        self.inner.run(on_finish_launching)
    }

    // An app quitting mid-request (on its last window closing) would drop the reply, so the
    // quit waits for `end_request` and the serve loop quits once that reply is written.
    fn quit(&self) {
        {
            let mut state = self.state.borrow_mut();
            if state.in_request {
                state.quit_deferred = true;
                return;
            }
        }
        self.inner.quit()
    }

    fn set_initial_windowing(&self, _request: WindowingRequest) {}

    fn request_windowing(&self, _request: WindowingRequest) -> Task<Result<()>> {
        Task::ready(Err(anyhow!("the pilot platform stays headless")))
    }

    fn restart(&self, binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {
        self.record(Recorded::Restart(binary_path));
    }

    fn activate(&self, ignoring_other_apps: bool) {
        self.inner.activate(ignoring_other_apps)
    }

    fn hide(&self) {
        self.inner.hide()
    }

    fn hide_other_apps(&self) {
        self.inner.hide_other_apps()
    }

    fn unhide_other_apps(&self) {
        self.inner.unhide_other_apps()
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        self.inner.displays()
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        self.inner.primary_display()
    }

    // The display set is fixed for the launch, so there is never a change to report.
    fn on_displays_changed(&self, _callback: Box<dyn FnMut()>) {}

    // The newest window stands in front, so it is the active one once its first tick lands.
    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.open_windows()
            .into_iter()
            .rev()
            .find(|window| window.is_active())
            .map(|window| window.handle())
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        Some(
            self.open_windows()
                .iter()
                .rev()
                .map(PilotWindow::handle)
                .collect(),
        )
    }

    // A missing GPU fails here with the renderer's error; there is no software fallback.
    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let renderer = gpui_kit::platform::current_headless_renderer()?
            .ok_or_else(|| anyhow!("this platform has no headless renderer"))?;
        let mut windows = self.windows.borrow_mut();
        let mut bounds = options.bounds;
        if windows.is_empty() {
            bounds.size = self.size;
        }
        let display = self.inner.primary_display();
        let window = PilotWindow::new(handle, bounds, self.scale, display, renderer);
        windows.push(window.downgrade());
        window.start_ticker(&self.foreground, self.pilot_timers(), self.refresh_hz);
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        self.inner.window_appearance()
    }

    fn button_layout(&self) -> Option<WindowButtonLayout> {
        self.inner.button_layout()
    }

    fn open_url(&self, url: &str) {
        self.record(Recorded::OpenUrl(url.to_string()));
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.inner.on_open_urls(callback)
    }

    fn register_url_scheme(&self, url: &str) -> Task<Result<()>> {
        self.inner.register_url_scheme(url)
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        let prompt = PathPrompt::Open {
            directories: options.directories,
            multiple: options.multiple,
        };
        self.open_prompt(prompt, Reply::Open(tx));
        rx
    }

    fn prompt_for_new_path(
        &self,
        directory: &Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        let prompt = PathPrompt::Save {
            directory: directory.to_path_buf(),
            suggested_name: suggested_name.map(str::to_string),
        };
        self.open_prompt(prompt, Reply::Save(tx));
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        self.inner.can_select_mixed_files_and_dirs()
    }

    fn reveal_path(&self, path: &Path) {
        self.record(Recorded::RevealPath(path.to_path_buf()));
    }

    fn open_with_system(&self, path: &Path) {
        self.record(Recorded::OpenWithSystem(path.to_path_buf()));
    }

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.inner.on_quit(callback)
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_reopen(callback)
    }

    // The Linux platform starts a login1 listener on the system bus for these two.
    fn on_system_sleep(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, menus: Vec<Menu>, keymap: &Keymap) {
        self.inner.set_menus(menus, keymap)
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        self.inner.get_menus()
    }

    fn set_dock_menu(&self, menu: Vec<MenuItem>, keymap: &Keymap) {
        self.inner.set_dock_menu(menu, keymap)
    }

    fn add_recent_document(&self, path: &Path) {
        self.inner.add_recent_document(path)
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.inner.on_app_menu_action(callback)
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_will_open_app_menu(callback)
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.inner.on_validate_app_menu_command(callback)
    }

    fn thermal_state(&self) -> ThermalState {
        self.inner.thermal_state()
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_thermal_state_change(callback)
    }

    fn prevent_idle_sleep(&self, reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Err(anyhow!(
            "the pilot platform does not inhibit idle sleep ({reason})"
        )))
    }

    fn set_app_identity(&self, identifier: &str, name: &str) {
        self.inner.set_app_identity(identifier, name)
    }

    fn show_system_notification(&self, notification: SystemNotification) {
        self.record(Recorded::Notification {
            title: notification.title.to_string(),
        });
    }

    fn dismiss_system_notification(&self, _tag: &str) {}

    fn on_system_notification_response(
        &self,
        _callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
    }

    fn compositor_name(&self) -> &'static str {
        self.inner.compositor_name()
    }

    fn graphical_environment(&self) -> Option<GraphicalEnvironment> {
        self.inner.graphical_environment()
    }

    fn app_path(&self) -> Result<PathBuf> {
        self.inner.app_path()
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        self.inner.path_for_auxiliary_executable(name)
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        self.inner.set_cursor_style(style)
    }

    fn hide_cursor_until_mouse_moves(&self) {
        self.inner.hide_cursor_until_mouse_moves()
    }

    fn is_cursor_visible(&self) -> bool {
        self.inner.is_cursor_visible()
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        self.inner.should_auto_hide_scrollbars()
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.state.borrow().clipboard.clone()
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        self.state.borrow_mut().clipboard = Some(item);
    }

    fn read_from_primary(&self) -> Option<ClipboardItem> {
        self.state.borrow().primary.clone()
    }

    fn write_to_primary(&self, item: ClipboardItem) {
        self.state.borrow_mut().primary = Some(item);
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        let entry = (username.to_string(), password.to_vec());
        self.state
            .borrow_mut()
            .credentials
            .insert(url.to_string(), entry);
        Task::ready(Ok(()))
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(self.state.borrow().credentials.get(url).cloned()))
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        self.state.borrow_mut().credentials.remove(url);
        Task::ready(Ok(()))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        self.inner.keyboard_layout()
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        self.inner.keyboard_mapper()
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.inner.on_keyboard_layout_change(callback)
    }
}
