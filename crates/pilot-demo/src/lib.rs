//! The fixture app gpui-pilot drives in its tests: a counter, a text input, open and save
//! buttons that raise path prompts, a paste button that reads the clipboard, and the
//! timing and overlay fixtures the agent-ergonomics tests drive.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::base::Root;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{TitleBar, h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Bounds, DragMoveEvent, ElementId, Entity, ExternalPaths, GlobalElementId,
    InspectorElementId, Interactivity, LayoutId, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels, PromptLevel, Role, ScrollDelta,
    ScrollWheelEvent, TestSupportExt as _, UniformListScrollHandle, Window, WindowDecorations,
    WindowHandle, WindowOptions, accesskit, div, px, uniform_list,
};

gpui_kit::actions!(pilot_demo, [Pick]);

/// Rows in the `--list` fixture, enough that a notch never reaches its end.
pub const LIST_ROWS: usize = 10_000;

pub struct DemoView {
    pub clicks: u32,
    pub name: Entity<InputState>,
    /// A masked input, drawn only on `--secret`, as a password field.
    pub secret: Entity<InputState>,
    pub secret_form: bool,
    pub opened: Option<Vec<PathBuf>>,
    pub saved: Option<PathBuf>,
    pub pasted: Option<String>,
    pub debounced: bool,
    pub slow_done: bool,
    pub covered_clicks: u32,
    pub booted: bool,
    pub greeting: Option<String>,
    pub scrolled: bool,
    /// Wheel events seen with control held.
    pub zoom: u32,
    /// Window closes refused by `guard_close`.
    pub close_asks: u32,
    pub spinning: bool,
    pub dropped: u32,
    /// The paths of the latest external file drop on `files`.
    pub file_drops: Option<Vec<PathBuf>>,
    /// External drags that came over `files`, and those that left the window there.
    pub file_over: u32,
    pub file_exits: u32,
    /// An external drag is over `files` now.
    pub file_hovering: bool,
    /// The window's logical viewport size at the latest render, shown by `viewport`.
    pub viewport: (f32, f32),
    /// The modifiers the latest mouse down and mouse up carried, as `ctrl+shift`, empty for
    /// none; a new press clears the release, so a plain click shows nothing.
    pub down_mods: String,
    pub up_mods: String,
    /// The button held during the latest pointer move when it is not the left one, as
    /// `right`, so a right or middle drag (a pan) is observable.
    pub held: Option<String>,
    pub answer: Option<usize>,
    pub env: Entity<SelectState<Vec<EnvItem>>>,
    /// Where the count is kept across a relaunch; `None` keeps it in memory only.
    pub persist: Option<PathBuf>,
    /// The `--dir` folder that `Touch` writes into.
    pub dir: Option<PathBuf>,
    /// The `--dir` entry names, sorted and joined by ",", as read at configure.
    pub listing: Option<String>,
    /// The `--fetch` url, `http://<ip>:<port><path>`.
    pub fetch_url: Option<String>,
    /// `fetched: <body>` or `fetch failed` once a fetch has finished.
    pub fetched: Option<String>,
    /// `Some(reused)` on `--show-data`: whether the launch reused a kept data dir.
    pub data_reused: Option<bool>,
    /// On `--list`, the window shows only a `LIST_ROWS`-row `uniform_list`.
    pub list: bool,
    /// On `--big-value N`, the window shows only the `name` input and a `big` element whose
    /// accessibility value is N chars, the digit `i % 10` at char `i`.
    pub big_value: Option<gpui_kit::SharedString>,
    /// On `--popover`, the window shows only a click-opened kit popover over observed rows.
    pub popover: bool,
    /// Clicks on the popover's `pop-item`, and on any of the `under-*` rows below it.
    pub pop_clicks: u32,
    pub under_clicks: u32,
    /// On `--context-menu`, the window shows only a focused element and a right-click kit
    /// context menu target, as Apinata's sidebar tree; `ctx_clicks` counts its `Rename` item.
    pub context_menu: bool,
    pub ctx_clicks: u32,
    ctx_focus: gpui_kit::FocusHandle,
    focus_ctx_pending: bool,
    /// On `--render-delay-ms <n>`, each render sleeps this long first, a slow app's frame.
    pub render_delay: Option<Duration>,
    /// On `--late-paint-ms <n>`, a `repaint` click shows a pending label at once and counts
    /// `repaints` only `n` ms later, an app whose content lands frames after its input.
    pub late_paint: Option<Duration>,
    pub repaints: u32,
    pub repainting: bool,
    /// Line wheel events the list has seen, and the sum of their `y` in lines.
    pub wheels: u32,
    pub wheel_lines: f32,
    list_scroll: UniformListScrollHandle,
}

/// The drag payload: dropping it on the `dropzone` button counts one drop.
pub struct Chip;

/// What follows the pointer while a `Chip` is dragged.
struct ChipGhost;

impl Render for ChipGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child("Chip")
    }
}

/// One row of the `env` Select.
#[derive(Clone)]
pub struct EnvItem(pub gpui_kit::SharedString);

// Select rows are drawn by gpui-component without `.test_support()`, so the row content
// registers itself; a click at its centre bubbles to the row's click handler.
impl gpui_kit::component::searchable_list::SearchableListItem for EnvItem {
    type Value = gpui_kit::SharedString;
    fn title(&self) -> gpui_kit::SharedString {
        self.0.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.0
    }
    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .id(gpui_kit::SharedString::from(format!(
                "env-option-{}",
                self.0
            )))
            .test_support()
            .child(self.0.clone())
    }
}

pub fn init(cx: &mut App) {
    gpui_kit::init(cx);
}

/// Opens the demo window; the platform decides its size and scale. `PILOT_DEMO_CSD=1` asks
/// for client-side decorations, so `Root` pads the content by its shadow inset as in Apinata.
pub fn open(cx: &mut App) -> anyhow::Result<(WindowHandle<Root>, Entity<DemoView>)> {
    let mut demo = None;
    let options = match std::env::var("PILOT_DEMO_CSD").as_deref() {
        Ok("1") => WindowOptions {
            window_decorations: Some(WindowDecorations::Client),
            ..WindowOptions::default()
        },
        _ => WindowOptions::default(),
    };
    let window = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| DemoView::new(window, cx));
        demo = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    })?;
    let demo = demo.expect("open_window builds the root view before it returns");
    Ok((window, demo))
}

/// The held modifiers as `ctrl+alt+shift+super`, empty when none.
fn modifier_names(modifiers: Modifiers) -> String {
    [
        (modifiers.control, "ctrl"),
        (modifiers.alt, "alt"),
        (modifiers.shift, "shift"),
        (modifiers.platform, "super"),
    ]
    .into_iter()
    .filter_map(|(held, name)| held.then_some(name))
    .collect::<Vec<_>>()
    .join("+")
}

impl DemoView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            clicks: 0,
            name: cx.new(|cx| InputState::new(window, cx)),
            secret: cx.new(|cx| InputState::new(window, cx).masked(true)),
            secret_form: false,
            opened: None,
            saved: None,
            pasted: None,
            debounced: false,
            slow_done: false,
            covered_clicks: 0,
            booted: false,
            greeting: None,
            scrolled: false,
            zoom: 0,
            close_asks: 0,
            spinning: false,
            dropped: 0,
            file_drops: None,
            file_over: 0,
            file_exits: 0,
            file_hovering: false,
            viewport: (0., 0.),
            down_mods: String::new(),
            up_mods: String::new(),
            held: None,
            answer: None,
            persist: None,
            dir: None,
            listing: None,
            fetch_url: None,
            fetched: None,
            data_reused: None,
            list: false,
            big_value: None,
            popover: false,
            pop_clicks: 0,
            under_clicks: 0,
            context_menu: false,
            ctx_clicks: 0,
            ctx_focus: cx.focus_handle(),
            focus_ctx_pending: false,
            render_delay: None,
            late_paint: None,
            repaints: 0,
            repainting: false,
            wheels: 0,
            wheel_lines: 0.,
            list_scroll: UniformListScrollHandle::new(),
            env: cx.new(|cx| {
                SelectState::new(
                    vec![EnvItem("dev".into()), EnvItem("prod".into())],
                    None,
                    window,
                    cx,
                )
            }),
        }
    }

    /// Keeps the count in `path` from now on, starting from the count saved there when it
    /// parses, else from 0.
    pub fn persist_to(&mut self, path: PathBuf) {
        self.clicks = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0);
        self.persist = Some(path);
    }

    /// Applies the `--list`, `--popover`, `--context-menu` and `--secret` flags and the `--dir <path>`,
    /// `--fetch <url>`, `--render-delay-ms <n>` and `--late-paint-ms <n>` pairs from `args`;
    /// unknown args are ignored.
    pub fn configure(&mut self, args: &[String]) {
        if args.iter().any(|arg| arg == "--list") {
            self.list = true;
        }
        if args.iter().any(|arg| arg == "--popover") {
            self.popover = true;
        }
        if args.iter().any(|arg| arg == "--secret") {
            self.secret_form = true;
        }
        if args.iter().any(|arg| arg == "--context-menu") {
            self.context_menu = true;
            self.focus_ctx_pending = true;
        }
        for pair in args.windows(2) {
            match pair[0].as_str() {
                "--dir" => {
                    let dir = PathBuf::from(&pair[1]);
                    self.listing = Some(match list_dir(&dir) {
                        Ok(names) => names,
                        Err(e) => format!("unreadable ({e})"),
                    });
                    self.dir = Some(dir);
                }
                "--fetch" => self.fetch_url = Some(pair[1].clone()),
                "--big-value" => {
                    let len: usize = pair[1].parse().unwrap_or(0);
                    let text: String = (0..len)
                        .map(|i| char::from(b'0' + (i % 10) as u8))
                        .collect();
                    self.big_value = Some(text.into());
                }
                "--render-delay-ms" => {
                    self.render_delay = pair[1].parse().ok().map(Duration::from_millis);
                }
                "--late-paint-ms" => {
                    self.late_paint = pair[1].parse().ok().map(Duration::from_millis);
                }
                _ => {}
            }
        }
    }

    /// Fetches `fetch_url` off the UI thread and shows the body, or `fetch failed`.
    fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(url) = self.fetch_url.clone() else {
            return;
        };
        let reply = cx.background_spawn(async move { http_get(&url) });
        cx.spawn(async move |this, cx| {
            let fetched = match reply.await {
                Ok(body) => format!("fetched: {}", body.trim()),
                Err(e) => {
                    eprintln!("pilot-demo: fetch failed: {e}");
                    "fetch failed".to_string()
                }
            };
            this.update(cx, |view, cx| {
                view.fetched = Some(fetched);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The list's scroll offset y in logical px, negative once scrolled down.
    pub fn list_offset(&self) -> f32 {
        f32::from(self.list_scroll.0.borrow().base_handle.offset().y)
    }

    pub fn secret_text(&self, cx: &App) -> String {
        self.secret.read(cx).value().to_string()
    }

    pub fn name_text(&self, cx: &App) -> String {
        self.name.read(cx).value().to_string()
    }

    /// One line of observable state, printed by the windowed binary on every change.
    pub fn state_line(&self, cx: &App) -> String {
        let scrolled = if self.scrolled { " scrolled" } else { "" };
        let mut line = format!(
            "clicks={} name={:?}{scrolled}",
            self.clicks,
            self.name_text(cx)
        );
        // Appended only when set, so the lines older tests match exactly stay unchanged.
        if let Some(paths) = &self.opened {
            line.push_str(&format!(" opened={paths:?}"));
        }
        if let Some(path) = &self.saved {
            line.push_str(&format!(" saved={path:?}"));
        }
        if let Some(text) = &self.pasted {
            line.push_str(&format!(" pasted={text:?}"));
        }
        if self.dropped > 0 {
            line.push_str(&format!(" dropped={}", self.dropped));
        }
        if let Some(paths) = &self.file_drops {
            line.push_str(&format!(" files={paths:?}"));
        }
        if self.file_over > 0 {
            line.push_str(&format!(
                " file_over={} file_exits={}",
                self.file_over, self.file_exits
            ));
        }
        if !self.down_mods.is_empty() || !self.up_mods.is_empty() {
            line.push_str(&format!(" down={} up={}", self.down_mods, self.up_mods));
        }
        if let Some(button) = &self.held {
            line.push_str(&format!(" held={button}"));
        }
        if let Some(n) = self.answer {
            line.push_str(&format!(" answer={n}"));
        }
        if self.zoom > 0 {
            line.push_str(&format!(" zoom={}", self.zoom));
        }
        if self.close_asks > 0 {
            line.push_str(&format!(" close_asks={}", self.close_asks));
        }
        if let Some(text) = &self.fetched {
            line.push_str(&format!(" fetched={text:?}"));
        }
        if self.wheels > 0 {
            line.push_str(&format!(
                " wheels={} lines={}",
                self.wheels, self.wheel_lines
            ));
        }
        line
    }

    /// Asks for one file, or with `directories` for one folder, and stores it in `opened`.
    fn pick_open(&mut self, directories: bool, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: !directories,
            directories,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = answer.await {
                this.update(cx, |view, cx| {
                    view.opened = Some(paths);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Raises a GPUI prompt and stores the chosen button's index in `answer`.
    fn ask(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reply = window.prompt(
            PromptLevel::Warning,
            "Delete the chip?",
            Some("This cannot be undone."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if let Ok(index) = reply.await {
                this.update(cx, |view, cx| {
                    view.answer = Some(index);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Shows the `late` button 100 ms after a click, as a debounced search would.
    fn debounce(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(100))
                .await;
            this.update(cx, |view, cx| {
                view.debounced = true;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Paints the pending label now and the next count after `late_paint`, so one click
    /// produces two frames that far apart.
    fn repaint(&mut self, cx: &mut Context<Self>) {
        let Some(delay) = self.late_paint else {
            return;
        };
        self.repainting = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |view, cx| {
                view.repaints += 1;
                view.repainting = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Sets `booted` after a 150 ms executor timer, which settle counts as pending work.
    pub fn boot(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            this.update(cx, |view, cx| {
                view.booted = true;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reveals `slow-done` after a plain thread finishes, work settle cannot see; only the
    /// task awaiting the channel lives on the executor.
    fn slow(&mut self, cx: &mut Context<Self>) {
        let (done, finished) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            done.send(()).ok();
        });
        cx.spawn(async move |this, cx| {
            if finished.await.is_ok() {
                this.update(cx, |view, cx| {
                    view.slow_done = true;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn pick_save(&mut self, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_new_path(&std::env::temp_dir(), Some("demo.txt"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = answer.await {
                this.update(cx, |view, cx| {
                    view.saved = Some(path);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

/// The entry names of `dir`, sorted and joined by ",".
fn list_dir(dir: &std::path::Path) -> std::io::Result<String> {
    let mut names = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names.join(","))
}

/// A bare HTTP/1.0 GET of `http://<ip>:<port><path>`, returning the body of a 200 reply.
fn http_get(url: &str) -> anyhow::Result<String> {
    use std::io::{Read as _, Write as _};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("not an http:// url: {url}"))?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let address: std::net::SocketAddr = host.parse()?;
    let timeout = Duration::from_secs(5);
    let mut stream = std::net::TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    // One write, so a server that reads the request once and closes sees all of it; leftover
    // unread bytes would make its close a reset.
    let request = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n");
    stream.write_all(request.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    let (head, body) = reply
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("no header end in the reply"))?;
    let status = head.lines().next().unwrap_or_default();
    anyhow::ensure!(status.split(' ').nth(1) == Some("200"), "status {status:?}");
    Ok(body.to_string())
}

/// Routes the window's close through a prompt: each should-close counts one ask and returns
/// false; unless a close prompt is already open it asks "Discard unsaved changes?" with
/// buttons ["Discard", "Cancel"] at PromptLevel::Warning, and Discard removes the window.
pub fn guard_close(demo: Entity<DemoView>, window: &mut Window, cx: &mut App) {
    // Set while the prompt is up, so a repeated close counts but raises no second prompt.
    let asking = Rc::new(Cell::new(false));
    window.on_window_should_close(cx, move |window, cx| {
        demo.update(cx, |view, cx| {
            view.close_asks += 1;
            cx.notify();
        });
        if asking.replace(true) {
            return false;
        }
        let reply = window.prompt(
            PromptLevel::Warning,
            "Discard unsaved changes?",
            None,
            &["Discard", "Cancel"],
            cx,
        );
        let asking = asking.clone();
        window
            .spawn(cx, async move |cx| {
                // A prompt dismissed unanswered errs, which keeps the window like Cancel.
                let answer = reply.await;
                asking.set(false);
                if answer == Ok(0) {
                    cx.update(|window, _| window.remove_window()).ok();
                }
            })
            .detach();
        false
    });
}

/// Lends `accessibility_id` to widgets, like `Button`, that lack `StatefulInteractiveElement`.
pub struct A11y<E>(pub E);

impl<E: InteractiveElement> InteractiveElement for A11y<E> {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.0.interactivity()
    }
}

impl<E: InteractiveElement> StatefulInteractiveElement for A11y<E> {}

/// Reports its element as disabled to accessibility, which no GPUI widget does at the pinned
/// version, so tests can check how actions treat a disabled target. Everything else delegates.
pub struct AriaDisabled<E>(pub E);

impl<E: InteractiveElement> InteractiveElement for AriaDisabled<E> {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.0.interactivity()
    }
}

impl<E: Element> IntoElement for AriaDisabled<E> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl<E: Element> Element for AriaDisabled<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.0.id()
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        self.0.source_location()
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.0.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.0
            .prepaint(id, inspector_id, bounds, layout, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.0
            .paint(id, inspector_id, bounds, layout, prepaint, window, cx)
    }

    fn a11y_role(&self) -> Option<accesskit::Role> {
        self.0.a11y_role()
    }

    fn write_a11y_info(&self, node: &mut accesskit::Node) {
        self.0.write_a11y_info(node);
        node.set_disabled();
    }
}

impl Render for DemoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(delay) = self.render_delay {
            std::thread::sleep(delay);
        }
        let viewport = window.viewport_size();
        self.viewport = (viewport.width.into(), viewport.height.into());
        if self.list {
            // The list's own scroll listener never stops propagation, so this wrapper sees
            // every wheel event too and counts the line ones a real wheel notch produces.
            return div()
                .size_full()
                .on_scroll_wheel(
                    cx.listener(|view: &mut Self, event: &ScrollWheelEvent, _, cx| {
                        if let ScrollDelta::Lines(lines) = event.delta {
                            view.wheels += 1;
                            view.wheel_lines += lines.y;
                            cx.notify();
                        }
                    }),
                )
                .child(
                    uniform_list("list", LIST_ROWS, |rows, _, _| {
                        rows.map(|i| div().h(px(24.)).child(format!("row {i}")))
                            .collect()
                    })
                    .track_scroll(&self.list_scroll)
                    .size_full(),
                )
                .into_any_element();
        }
        if self.popover {
            return self.popover_fixture(cx).into_any_element();
        }
        if let Some(value) = self.big_value.clone() {
            return v_flex()
                .size_full()
                .child(Input::new(&self.name).id("name").accessibility_id("name"))
                .child(
                    div()
                        .id("big")
                        .test_support()
                        .aria_value(value)
                        .h(px(40.))
                        .w_full()
                        .child("big value"),
                )
                .into_any_element();
        }
        if self.context_menu {
            return self.context_menu_fixture(window, cx).into_any_element();
        }
        let paste = cx.listener(|view: &mut Self, _, _, cx| {
            view.pasted = cx.read_from_clipboard().and_then(|item| item.text());
            cx.notify();
        });
        v_flex()
            .size_full()
            // Capture phase, so a press or release is seen whatever element handles it; they
            // record the modifiers that arrived with the event.
            .capture_any_mouse_down(cx.listener(
                |view: &mut Self, event: &MouseDownEvent, _, cx| {
                    view.down_mods = modifier_names(event.modifiers);
                    view.up_mods.clear();
                    cx.notify();
                },
            ))
            .capture_any_mouse_up(cx.listener(|view: &mut Self, event: &MouseUpEvent, _, cx| {
                view.up_mods = modifier_names(event.modifiers);
                cx.notify();
            }))
            .on_mouse_move(
                cx.listener(|view: &mut Self, event: &MouseMoveEvent, _, cx| {
                    let held = match event.pressed_button {
                        Some(MouseButton::Right) => "right",
                        Some(MouseButton::Middle) => "middle",
                        _ => return,
                    };
                    if view.held.as_deref() != Some(held) {
                        view.held = Some(held.to_owned());
                        cx.notify();
                    }
                }),
            )
            // A control wheel counts a zoom, as an app's ctrl+scroll would; otherwise only the
            // first wheel event notifies, so the observer prints one new state line.
            .on_scroll_wheel(
                cx.listener(|view: &mut Self, event: &ScrollWheelEvent, _, cx| {
                    if event.modifiers.control {
                        view.zoom += 1;
                        cx.notify();
                    } else if !view.scrolled {
                        view.scrolled = true;
                        cx.notify();
                    }
                }),
            )
            .p(px(16.))
            .gap(px(8.))
            .child(div().child(format!("clicks: {}", self.clicks)))
            .child(
                A11y(Button::new("count"))
                    .accessibility_id("count")
                    .0
                    .primary()
                    .label("Count")
                    .tooltip("Count")
                    .on_click(cx.listener(|view: &mut Self, _, _, cx| {
                        view.clicks += 1;
                        if let Some(path) = &view.persist {
                            std::fs::write(path, view.clicks.to_string())
                                .expect("write the persisted count");
                        }
                        cx.notify();
                    })),
            )
            .child(Input::new(&self.name).id("name").accessibility_id("name"))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(Button::new("open").label("Open").on_click(
                        cx.listener(|view: &mut Self, _, _, cx| view.pick_open(false, cx)),
                    ))
                    .child(
                        Button::new("save")
                            .label("Save")
                            .on_click(cx.listener(|view: &mut Self, _, _, cx| view.pick_save(cx))),
                    )
                    .child(Button::new("paste").label("Paste").on_click(paste))
                    .child(
                        Button::new("debounce")
                            .label("Later")
                            .on_click(cx.listener(|view: &mut Self, _, _, cx| view.debounce(cx))),
                    )
                    .child(Button::new("folder").label("Folder").on_click(
                        cx.listener(|view: &mut Self, _, _, cx| view.pick_open(true, cx)),
                    ))
                    // GPUI's own tooltip, unlike the kit Button's managed one, hides in
                    // keyboard modality, as an app's plain `div().tooltip` does; last, so the
                    // others keep their positions.
                    .child(
                        div()
                            .id("tipped")
                            .role(Role::Button)
                            .aria_label("Tipped")
                            .child("Tipped")
                            .tooltip(|window, cx| Tooltip::new("Tip").build(window, cx))
                            .test_support(),
                    ),
            )
            .when(self.debounced, |this| {
                this.child(Button::new("late").label("Late"))
            })
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("menu")
                            .label("Menu")
                            .dropdown_menu(|menu, _, _| {
                                menu.menu("Alpha", Box::new(Pick))
                                    .menu("Beta", Box::new(Pick))
                            }),
                    )
                    .child(div().w(px(140.)).child(Select::new(&self.env).id("env")))
                    // A spinner animates every frame while its layout stays still.
                    .child(Button::new("spin").label("Spin").on_click(cx.listener(
                        |view: &mut Self, _, _, cx| {
                            view.spinning = !view.spinning;
                            cx.notify();
                        },
                    )))
                    .when(self.spinning, |this| this.child(Spinner::new()))
                    .child(
                        Button::new("slow")
                            .label("Slow")
                            .on_click(cx.listener(|view: &mut Self, _, _, cx| view.slow(cx))),
                    )
                    // An occluding Dialog-role cover over a live button: a raw click lands
                    // on the cover, never on `covered`.
                    .child(
                        div()
                            .relative()
                            .child(
                                Button::new("covered")
                                    .label("Covered")
                                    .on_click(cx.listener(|view: &mut Self, _, _, cx| {
                                        view.covered_clicks += 1;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id("cover")
                                    .role(Role::Dialog)
                                    .test_support()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full()
                                    .occlude(),
                            ),
                    ),
            )
            .when(self.slow_done, |this| {
                this.child(Button::new("slow-done").label("Done"))
            })
            .when(self.booted, |this| {
                this.child(Button::new("booted").label("Booted"))
            })
            .when_some(self.greeting.clone(), |this, text| {
                this.child(Button::new("greeting").label(text))
            })
            // Last, so the fixtures above keep their positions.
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        A11y(Button::new("chip"))
                            .accessibility_id("chip")
                            .on_drag(Chip, |_, _, _, cx| cx.new(|_| ChipGhost))
                            .0
                            .label("Chip"),
                    )
                    .child(
                        A11y(Button::new("dropzone"))
                            .accessibility_id("dropzone")
                            .0
                            .label(if self.dropped == 0 {
                                "Drop here".to_string()
                            } else {
                                format!("Dropped {}", self.dropped)
                            })
                            .on_drop(cx.listener(|view: &mut Self, _: &Chip, _, cx| {
                                view.dropped += 1;
                                cx.notify();
                            })),
                    )
                    .child(
                        A11y(Button::new("ask"))
                            .accessibility_id("ask")
                            .0
                            .label("Ask")
                            .on_click(
                                cx.listener(|view: &mut Self, _, window, cx| view.ask(window, cx)),
                            ),
                    )
                    // A drag source that reports itself disabled; last, so the others keep
                    // their positions.
                    .child(
                        AriaDisabled(
                            div()
                                .id("locked")
                                .role(Role::Button)
                                .on_drag(Chip, |_, _, _, cx| cx.new(|_| ChipGhost))
                                .child("Locked"),
                        )
                        .test_support(),
                    )
                    .child(
                        A11y(Button::new("zoom"))
                            .accessibility_id("zoom")
                            .0
                            .label(format!("Zoom {}", self.zoom)),
                    )
                    // An external file drop target, last so the others keep their positions;
                    // its label shows a drag over it, and the state line what it received.
                    .child(
                        A11y(Button::new("files"))
                            .accessibility_id("files")
                            .0
                            .label(match (&self.file_drops, self.file_hovering) {
                                (_, true) => "Files over".to_string(),
                                (Some(paths), false) => format!("Files: {}", paths.len()),
                                (None, false) => "Drop files".to_string(),
                            })
                            .on_drag_move(cx.listener(
                                |view: &mut Self, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                                    let over = event.bounds.contains(&event.event.position);
                                    if over && !view.file_hovering {
                                        view.file_over += 1;
                                    }
                                    if over != view.file_hovering {
                                        view.file_hovering = over;
                                        cx.notify();
                                    }
                                },
                            ))
                            .on_file_drop_exit(cx.listener(|view: &mut Self, _, _, cx| {
                                view.file_hovering = false;
                                view.file_exits += 1;
                                cx.notify();
                            }))
                            .on_drop(cx.listener(
                                |view: &mut Self, paths: &ExternalPaths, _, cx| {
                                    view.file_hovering = false;
                                    view.file_drops = Some(paths.paths().to_vec());
                                    cx.notify();
                                },
                            )),
                    )
                    // A width-dependent readout for `resize`, after the drop target so the
                    // others keep their positions.
                    .child(
                        A11y(Button::new("viewport"))
                            .accessibility_id("viewport")
                            .0
                            .label(format!("Viewport {}x{}", self.viewport.0, self.viewport.1)),
                    ),
            )
            // Only with `--secret`, so other trees and screenshots stay unchanged.
            .when(self.secret_form, |this| {
                this.child(Input::new(&self.secret).id("secret").mask_toggle())
            })
            // Only with `--dir` or `--fetch`, so other trees and screenshots stay unchanged.
            .when(self.listing.is_some() || self.fetch_url.is_some(), |this| {
                this.child(
                    h_flex()
                        .gap(px(8.))
                        .when_some(self.listing.clone(), |this, names| {
                            this.child(
                                A11y(Button::new("listing"))
                                    .accessibility_id("listing")
                                    .0
                                    .label(format!("listing: {names}")),
                            )
                            .child(
                                A11y(Button::new("touch"))
                                    .accessibility_id("touch")
                                    .0
                                    .label("Touch")
                                    .on_click(cx.listener(|view: &mut Self, _, _, _| {
                                        if let Some(dir) = &view.dir {
                                            std::fs::write(dir.join("touched"), "pilot-demo")
                                                .expect("write the touched file");
                                        }
                                    })),
                            )
                        })
                        .when(self.fetch_url.is_some(), |this| {
                            this.child(
                                A11y(Button::new("fetch"))
                                    .accessibility_id("fetch")
                                    .0
                                    .label("Fetch")
                                    .on_click(
                                        cx.listener(|view: &mut Self, _, _, cx| view.fetch(cx)),
                                    ),
                            )
                        })
                        .when_some(self.fetched.clone(), |this, text| {
                            this.child(
                                A11y(Button::new("fetched"))
                                    .accessibility_id("fetched")
                                    .0
                                    .label(text),
                            )
                        }),
                )
            })
            // Only with `--show-data`, so other trees and screenshots stay unchanged.
            .when_some(self.data_reused, |this, reused| {
                this.child(
                    A11y(Button::new("data"))
                        .accessibility_id("data")
                        .0
                        .label(if reused {
                            "data: reused"
                        } else {
                            "data: fresh"
                        }),
                )
            })
            // Only with `--late-paint-ms`, so other trees and screenshots stay unchanged.
            .when(self.late_paint.is_some(), |this| {
                this.child(
                    A11y(Button::new("repaint"))
                        .accessibility_id("repaint")
                        .0
                        .label(if self.repainting {
                            format!("Repaint {}...", self.repaints)
                        } else {
                            format!("Repaint {}", self.repaints)
                        })
                        .on_click(cx.listener(|view: &mut Self, _, _, cx| view.repaint(cx))),
                )
            })
            .into_any_element()
    }
}

impl DemoView {
    /// A click-opened kit popover whose content (a button and an observed div) is drawn over
    /// the observed `under-*` rows laid out below its trigger.
    fn popover_fixture(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let under = |ix: usize, cx: &mut Context<Self>| {
            Button::new(("under", ix))
                .label(format!("Under {ix}"))
                .w(px(240.))
                .on_click(cx.listener(|view: &mut Self, _, _, cx| {
                    view.under_clicks += 1;
                    cx.notify();
                }))
        };
        // As Apinata's collection switcher: a click-opened popover in the kit's title bar,
        // behind an `overflow_hidden` row, over rows nested deeper than its content.
        let popover = Popover::new("pop")
            .trigger(Button::new("pop-trigger").label("Pop"))
            .content(move |_, _, _| {
                let view = view.clone();
                v_flex()
                    .w(px(200.))
                    .child(
                        Button::new("pop-item")
                            .label("Item")
                            .on_click(move |_, _, cx| {
                                view.update(cx, |v, cx| {
                                    v.pop_clicks += 1;
                                    cx.notify();
                                })
                            }),
                    )
                    .child(div().id("pop-mark").test_support().h(px(40.)).w_full())
            });
        let deep = (1..=12usize).fold(
            v_flex()
                .gap(px(4.))
                .child(under(1, cx))
                .child(under(2, cx))
                .child(under(3, cx))
                .into_any_element(),
            |inner, level| div().id(("nest", level)).child(inner).into_any_element(),
        );
        v_flex()
            .size_full()
            .child(
                TitleBar::new().child(
                    h_flex()
                        .id("title-row")
                        .test_support()
                        .absolute()
                        .inset_0()
                        .child(h_flex().min_w_0().overflow_hidden().child(popover)),
                ),
            )
            .child(div().p(px(8.)).child(deep))
    }
}

impl DemoView {
    /// A focused `ctx-focus` group before a `ctx-target` with a right-click kit context menu.
    /// The group (it needs a role to own an accessibility node) reports focus in prepaint
    /// before the menu takes focus, so both report it on the frame the menu opens.
    fn context_menu_fixture(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if std::mem::take(&mut self.focus_ctx_pending) {
            self.ctx_focus.focus(window, cx);
        }
        let view = cx.entity();
        v_flex()
            .size_full()
            .p(px(8.))
            .gap(px(8.))
            .child(
                div()
                    .id("ctx-focus")
                    .test_support()
                    .role(Role::Group)
                    .track_focus(&self.ctx_focus)
                    .w(px(240.))
                    .h(px(40.))
                    .child("Focused"),
            )
            .child(
                div()
                    .id("ctx-target")
                    .test_support()
                    .w(px(240.))
                    .h(px(120.))
                    .child("Right-click me")
                    .context_menu(move |menu, _, _| {
                        let view = view.clone();
                        menu.item(PopupMenuItem::new("Rename").on_click(move |_, _, cx| {
                            view.update(cx, |v, cx| {
                                v.ctx_clicks += 1;
                                cx.notify();
                            })
                        }))
                    }),
            )
    }
}

/// pilot-demo's opt-in: the kit's assets, the kit's init, and the demo window.
pub struct Demo;

impl gpui_pilot_host::PilotApp for Demo {
    fn assets(&self) -> Option<std::sync::Arc<dyn gpui_kit::AssetSource>> {
        Some(std::sync::Arc::new(gpui_kit::assets::AllAssets))
    }

    fn init(&self, cx: &mut App) {
        init(cx);
    }

    /// Opens the demo, greets with `PILOT_DEMO_GREETING` when set, boots on `--boot`, keeps
    /// its count in the launch's data dir on `--persist`, and guards its close on
    /// `--guard-close`, applies `--dir`, `--fetch`, `--render-delay-ms` and `--late-paint-ms`,
    /// shows data reuse on `--show-data`, shows only the long list on `--list`, only the
    /// popover fixture on `--popover`, and only the context menu fixture on `--context-menu`.
    fn open(&self, launch: &gpui_pilot_host::Launch, cx: &mut App) -> anyhow::Result<()> {
        let (handle, demo) = open(cx)?;
        if launch.args.iter().any(|arg| arg == "--guard-close") {
            handle.update(cx, |_, window, cx| guard_close(demo.clone(), window, cx))?;
        }
        let greeting = std::env::var("PILOT_DEMO_GREETING").ok();
        let boot = launch.args.iter().any(|arg| arg == "--boot");
        let persist = launch.args.iter().any(|arg| arg == "--persist");
        let show_data = launch.args.iter().any(|arg| arg == "--show-data");
        demo.update(cx, |view, cx| {
            view.greeting = greeting;
            view.data_reused = show_data.then_some(launch.reused);
            view.configure(&launch.args);
            if persist {
                view.persist_to(launch.data_dir.join("clicks"));
            }
            if boot {
                view.boot(cx);
            }
            cx.notify();
        });
        Ok(())
    }
}
